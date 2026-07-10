//! auto-tagger: generate a caption and keyword tags for each photo in a ptsync
//! archive using a vision model, writing them into the photo's sidecar Markdown.
//!
//! This is an experimental companion to `ptsync`, kept as its own crate so the
//! heavier image-decode stack (`image` + `heic`) and the model call stay out of
//! ptsync's small core.
//!
//! The job is **resumable**: progress lives in a small `auto_tags` table (a plain
//! SQLite file inside the archive), so you can start it, quit, and resume later
//! without redoing work. The archive Markdown is the source of truth; the table
//! is only a work queue / cache.
//!
//! The model is reached over the OpenAI-compatible `/chat/completions` API, so
//! the same code path works against a local server (Ollama, LM Studio, …) or a
//! cloud provider — the user picks via `--base-url` / `--model`.

use anyhow::anyhow;
use base64::Engine;
use clap::Parser;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::runtime;
use turso::{Connection, Database, params};
use yaml_rust2::yaml::Hash;
use yaml_rust2::{Yaml, YamlEmitter, YamlLoader};

/// Bumped when the prompt changes, so a later run can tell tags produced by an
/// older prompt apart from current ones. Stored on each queue row.
const PROMPT_VERSION: i64 = 1;

/// Longest edge (px) sent to the model. Larger images are downscaled to this
/// before encoding — smaller payloads mean faster local inference and lower
/// cloud cost, and it is plenty of detail for tagging.
const MAX_DIM: u32 = 786;

/// JPEG quality for the downscaled image sent to the model.
const JPEG_QUALITY: u8 = 85;

/// How the model is asked to respond. We request strict JSON and parse it
/// tolerantly (see `parse_tag_json`), which is more robust across the many local
/// models than relying on an OpenAI `response_format` that not all servers honour.
const PROMPT: &str = "You are a photo tagging assistant. Look at this photo and \
respond with ONLY a JSON object, no other text, in exactly this form: \
{\"caption\": \"a short one-sentence description\", \"tags\": [\"tag1\", \"tag2\"]}. \
Use between 5 and 15 short lowercase keyword tags describing the main subjects, \
the setting, and any notable objects.";

const CREATE_AUTO_TAGS: &str = "
    CREATE TABLE IF NOT EXISTS auto_tags (
        auto_tags_id   INTEGER PRIMARY KEY AUTOINCREMENT,
        archive_path   TEXT NOT NULL UNIQUE, -- media path relative to the archive root
        checksum       TEXT,                 -- content hash from the .md frontmatter, if present
        status         TEXT NOT NULL,        -- 'started' | 'done' | 'error'
        caption        TEXT,                 -- one-line description, NULL until done
        tags           TEXT,                 -- JSON array of strings, NULL until done
        model          TEXT,                 -- model that produced the tags
        prompt_version INTEGER,              -- PROMPT_VERSION at the time
        attempts       INTEGER NOT NULL DEFAULT 0,
        error_message  TEXT,                 -- populated when status = 'error'
        started_at     DATETIME DEFAULT CURRENT_TIMESTAMP,
        finished_at    DATETIME              -- set on done/error
    )
";

/// Generate captions and tags for photos in a ptsync archive using a vision
/// model, writing them into each photo's Markdown. Resumable: start it, quit,
/// and run again to carry on.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// The archive directory to tag (as produced by `ptsync sync`)
    #[arg(short, long)]
    output: String,

    /// Queue/cache database [default: <output>/.ptsync/auto-tags.db]
    #[arg(long)]
    db: Option<String>,

    /// OpenAI-compatible API base URL. Defaults to a local Ollama server; point
    /// it at any local or cloud provider that speaks the same API.
    #[arg(long, default_value = "http://localhost:11434/v1")]
    base_url: String,

    /// Vision model name (must be available on the chosen server, e.g.
    /// `ollama pull qwen2.5vl`)
    #[arg(long, default_value = "qwen2.5vl")]
    model: String,

    /// Name of the environment variable holding the API key, for cloud providers
    /// (local servers usually need none)
    #[arg(long)]
    api_key_env: Option<String>,

    /// Tag at most N items this run (a cost/time guardrail)
    #[arg(long)]
    limit: Option<usize>,

    /// Re-attempt items currently marked error
    #[arg(long)]
    retry_errors: bool,

    /// Re-tag items already marked done (e.g. after switching models)
    #[arg(long)]
    retag: bool,

    /// Don't call the model or write anything; just report what would be tagged
    #[arg(short = 'n', long)]
    dry_run: bool,
}

/// Connection details for an OpenAI-compatible chat/completions endpoint.
struct ModelConfig {
    /// Base URL, e.g. `http://localhost:11434/v1`. `/chat/completions` is appended.
    base_url: String,
    model: String,
    /// Bearer token; `None` (or empty) for local servers that ignore it.
    api_key: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let root = PathBuf::from(&cli.output);
    if !root.is_dir() {
        return Err(anyhow!("Archive directory does not exist: {}", cli.output));
    }

    // Keep resume state with the archive so it travels with it and does not
    // depend on the working directory.
    let db_path = match &cli.db {
        Some(p) => p.clone(),
        None => default_db_path(&cli.output),
    };

    let api_key = match &cli.api_key_env {
        Some(name) => match std::env::var(name) {
            Ok(v) => Some(v),
            Err(_) => {
                eprintln!("warning: env var {name} is not set; sending no API key");
                None
            }
        },
        None => None,
    };
    let cfg = ModelConfig {
        base_url: cli.base_url.trim_end_matches('/').to_string(),
        model: cli.model.clone(),
        api_key,
    };

    println!("Tagging archive: {}", cli.output);
    println!("Model: {} at {}", cfg.model, cfg.base_url);

    let mut candidates: Vec<String> = walk(&root).into_iter().filter(|p| is_taggable(p)).collect();
    candidates.sort();

    let rt = runtime::Builder::new_current_thread().build()?;

    // Dry run is read-only: never create the queue db (honouring "no changes will
    // be made to disk"). Consult an existing one only if it is already there.
    if cli.dry_run {
        let statuses = if Path::new(&db_path).exists() {
            rt.block_on(async {
                let (_db, conn) = open_conn(&db_path).await?;
                load_statuses(&conn).await
            })?
        } else {
            HashMap::new()
        };
        let pending = pending_of(&candidates, &statuses, cli.retry_errors, cli.retag);
        log_summary(&candidates, &statuses, &pending);
        for p in pending.iter().take(10) {
            println!("  would tag {p}");
        }
        if pending.len() > 10 {
            println!("  … and {} more", pending.len() - 10);
        }
        return Ok(());
    }

    if let Some(parent) = Path::new(&db_path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    rt.block_on(async {
        let (_db, conn) = open_conn(&db_path).await?;
        conn.execute(CREATE_AUTO_TAGS, ()).await?;
        process(
            &conn,
            &root,
            &candidates,
            &cfg,
            cli.limit,
            cli.retry_errors,
            cli.retag,
        )
        .await
    })
}

#[allow(clippy::too_many_arguments)]
async fn process(
    conn: &Connection,
    root: &Path,
    candidates: &[String],
    cfg: &ModelConfig,
    limit: Option<usize>,
    retry_errors: bool,
    retag: bool,
) -> anyhow::Result<()> {
    // Single-process, so any 'started' row is a crashed/quit remnant, not work
    // in flight. Requeue it by clearing the claim; combined with writing the
    // Markdown before marking 'done', this makes resume correct regardless of
    // how the previous run ended.
    let reclaimed = conn
        .execute("DELETE FROM auto_tags WHERE status = 'started'", ())
        .await?;
    if reclaimed > 0 {
        println!("Requeued {reclaimed} interrupted item(s) from a previous run");
    }

    let statuses = load_statuses(conn).await?;
    let pending = pending_of(candidates, &statuses, retry_errors, retag);
    log_summary(candidates, &statuses, &pending);

    let todo: Vec<&String> = match limit {
        Some(n) => pending.iter().take(n).collect(),
        None => pending.iter().collect(),
    };

    let prog = indicatif::ProgressBar::new(todo.len() as u64);
    let mut tagged = 0u64;
    let mut errors = 0u64;
    for path in todo {
        claim(conn, path).await?;
        match tag_one(conn, root, path, cfg).await {
            Ok(()) => tagged += 1,
            Err(e) => {
                prog.suspend(|| eprintln!("warning: could not tag {path}: {e}"));
                record_error(conn, path, &e.to_string()).await?;
                errors += 1;
            }
        }
        prog.inc(1);
    }
    prog.finish_and_clear();

    // Fold the WAL back into the main file so the queue db stays a single,
    // directly-readable SQLite file.
    let mut rows = conn.query("PRAGMA wal_checkpoint(TRUNCATE)", ()).await?;
    while rows.next().await?.is_some() {}

    println!("Auto-tag finished: {tagged} tagged, {errors} error(s)");
    Ok(())
}

/// Read one media file, ask the model to describe it, write the result into the
/// sidecar `.md` first, then mark the queue row done. The write-before-done
/// order means a crash in between simply re-tags next run (an idempotent no-op),
/// never a row marked done with no tags on disk.
async fn tag_one(
    conn: &Connection,
    root: &Path,
    path: &str,
    cfg: &ModelConfig,
) -> anyhow::Result<()> {
    let md_rel = get_desired_markdown_path(path)?;
    let md_abs = root.join(&md_rel);
    if !md_abs.exists() {
        return Err(anyhow!("No sidecar {md_rel} for {path}; run `sync` first"));
    }

    // Decode → downscale → JPEG, so every image (including HEIC) reaches the model
    // as a small JPEG regardless of its source format.
    let jpeg = prepare_image(root, path)?;
    let (caption, tags) = call_model(cfg, &jpeg)?;

    let checksum = write_tags_to_md(&md_abs, &caption, &tags)?;
    let tags_json = serde_json::to_string(&tags)?;
    conn.execute(
        "UPDATE auto_tags SET status = 'done', caption = ?1, tags = ?2, model = ?3, \
         prompt_version = ?4, checksum = ?5, error_message = NULL, \
         finished_at = CURRENT_TIMESTAMP WHERE archive_path = ?6",
        params![
            caption,
            tags_json,
            cfg.model.clone(),
            PROMPT_VERSION,
            checksum,
            path
        ],
    )
    .await?;
    Ok(())
}

/// Claim an item: upsert its row to `started` and bump the attempt count. After
/// `reclaim` there are no `started` rows, so the only conflicts are with existing
/// `done`/`error` rows being re-attempted (`--retag`/`--retry-errors`).
async fn claim(conn: &Connection, path: &str) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO auto_tags (archive_path, status, attempts, started_at, finished_at) \
         VALUES (?1, 'started', 1, CURRENT_TIMESTAMP, NULL) \
         ON CONFLICT(archive_path) DO UPDATE SET \
           status = 'started', attempts = attempts + 1, \
           started_at = CURRENT_TIMESTAMP, finished_at = NULL, error_message = NULL",
        params![path],
    )
    .await?;
    Ok(())
}

async fn record_error(conn: &Connection, path: &str, message: &str) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE auto_tags SET status = 'error', error_message = ?1, \
         finished_at = CURRENT_TIMESTAMP WHERE archive_path = ?2",
        params![message, path],
    )
    .await?;
    Ok(())
}

/// Current status for every queued item, keyed by archive path.
async fn load_statuses(conn: &Connection) -> anyhow::Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    let mut rows = conn
        .query("SELECT archive_path, status FROM auto_tags", ())
        .await?;
    while let Some(row) = rows.next().await? {
        map.insert(row.get::<String>(0)?, row.get::<String>(1)?);
    }
    Ok(map)
}

/// Archive files that still need tagging, given each one's recorded status.
fn pending_of(
    candidates: &[String],
    statuses: &HashMap<String, String>,
    retry_errors: bool,
    retag: bool,
) -> Vec<String> {
    candidates
        .iter()
        .filter(|p| is_pending(statuses.get(*p).map(|s| s.as_str()), retry_errors, retag))
        .cloned()
        .collect()
}

fn log_summary(candidates: &[String], statuses: &HashMap<String, String>, pending: &[String]) {
    let count_with_status = |want: &str| {
        candidates
            .iter()
            .filter(|p| statuses.get(*p).map(|s| s == want).unwrap_or(false))
            .count()
    };
    println!(
        "{} taggable file(s): {} to tag, {} done, {} error",
        candidates.len(),
        pending.len(),
        count_with_status("done"),
        count_with_status("error"),
    );
}

/// Whether an item should be (re)processed this run given its recorded status. A
/// missing row is fresh work; `done`/`error` are only revisited when the user
/// opts in. (`started` should not survive `reclaim`, but if one does we retry it.)
fn is_pending(status: Option<&str>, retry_errors: bool, retag: bool) -> bool {
    match status {
        None => true,
        Some("done") => retag,
        Some("error") => retry_errors,
        _ => true,
    }
}

/// Still-image types we can decode and send. HEIC is included via the `heic`
/// crate; video is skipped (it needs frame extraction).
fn is_taggable(path: &str) -> bool {
    matches!(
        ext_of(path).as_str(),
        "jpg" | "jpeg" | "png" | "gif" | "heic"
    )
}

fn ext_of(path: &str) -> String {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_lowercase())
        .unwrap_or_default()
}

fn default_db_path(output: &str) -> String {
    Path::new(output)
        .join(".ptsync")
        .join("auto-tags.db")
        .to_string_lossy()
        .to_string()
}

/// Every file under `root`, as archive-relative, forward-slashed paths.
fn walk(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    walk_into(root, Path::new(""), &mut out);
    out
}

fn walk_into(root: &Path, rel: &Path, out: &mut Vec<String>) {
    let dir = root.join(rel);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let child = rel.join(entry.file_name());
        let path = entry.path();
        if path.is_dir() {
            walk_into(root, &child, out);
        } else if path.is_file() {
            out.push(child.to_string_lossy().replace('\\', "/"));
        }
    }
}

// --- image preprocessing -------------------------------------------------------

/// Decode a media file (HEIC via the pure-Rust `heic` crate, everything else via
/// `image`), downscale to `MAX_DIM`, and encode a JPEG for the model.
fn prepare_image(root: &Path, rel: &str) -> anyhow::Result<Vec<u8>> {
    let path = root.join(rel);
    let img = if ext_of(rel) == "heic" {
        let data = std::fs::read(&path)?;
        let out = heic::DecoderConfig::new()
            .decode(&data, heic::PixelLayout::Rgb8)
            .map_err(|e| anyhow!("heic decode failed: {e:?}"))?;
        let buf = image::RgbImage::from_raw(out.width, out.height, out.data)
            .ok_or_else(|| anyhow!("heic returned an unexpected buffer size"))?;
        image::DynamicImage::ImageRgb8(buf)
    } else {
        image::ImageReader::open(&path)?
            .with_guessed_format()?
            .decode()?
    };
    encode_jpeg(&downscale(img, MAX_DIM), JPEG_QUALITY)
}

/// Shrink so the longest edge is `max_dim`, preserving aspect ratio. Never
/// upscales.
fn downscale(img: image::DynamicImage, max_dim: u32) -> image::DynamicImage {
    if img.width().max(img.height()) > max_dim {
        img.resize(max_dim, max_dim, image::imageops::FilterType::Triangle)
    } else {
        img
    }
}

fn encode_jpeg(img: &image::DynamicImage, quality: u8) -> anyhow::Result<Vec<u8>> {
    let rgb = img.to_rgb8();
    let mut buf = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
    enc.encode(
        rgb.as_raw(),
        rgb.width(),
        rgb.height(),
        image::ExtendedColorType::Rgb8,
    )?;
    Ok(buf)
}

// --- model call ----------------------------------------------------------------

/// POST the JPEG to the chat/completions endpoint and return the parsed
/// `(caption, tags)`. Blocking HTTP: fine because the loop is sequential.
fn call_model(cfg: &ModelConfig, jpeg_bytes: &[u8]) -> anyhow::Result<(String, Vec<String>)> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(jpeg_bytes);
    let data_uri = format!("data:image/jpeg;base64,{b64}");
    let body = serde_json::json!({
        "model": cfg.model,
        "stream": false,
        "temperature": 0.2,
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": PROMPT },
                { "type": "image_url", "image_url": { "url": data_uri } }
            ]
        }]
    });
    let body_str = serde_json::to_string(&body)?;

    let url = format!("{}/chat/completions", cfg.base_url);
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(180))
        .build();
    let mut req = agent.post(&url).set("Content-Type", "application/json");
    if let Some(key) = &cfg.api_key
        && !key.is_empty()
    {
        req = req.set("Authorization", &format!("Bearer {key}"));
    }

    let text = match req.send_string(&body_str) {
        Ok(resp) => resp.into_string()?,
        Err(ureq::Error::Status(code, resp)) => {
            let detail = resp.into_string().unwrap_or_default();
            return Err(anyhow!("model returned HTTP {code}: {}", detail.trim()));
        }
        Err(e) => return Err(anyhow!("model request to {url} failed: {e}")),
    };
    parse_tag_response(&text)
}

/// Pull `(caption, tags)` out of a chat/completions response body.
fn parse_tag_response(response_body: &str) -> anyhow::Result<(String, Vec<String>)> {
    let v: serde_json::Value = serde_json::from_str(response_body)
        .map_err(|e| anyhow!("model response was not JSON: {e}"))?;
    let content = v["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| anyhow!("no message content in model response: {response_body}"))?;
    parse_tag_json(content)
}

/// Parse the model's message text, which should be a `{caption, tags}` object.
/// Tolerant of surrounding prose or code fences by slicing to the outermost
/// braces before parsing.
fn parse_tag_json(content: &str) -> anyhow::Result<(String, Vec<String>)> {
    let start = content
        .find('{')
        .ok_or_else(|| anyhow!("model did not return a JSON object: {content}"))?;
    let end = content
        .rfind('}')
        .ok_or_else(|| anyhow!("model did not return a JSON object: {content}"))?;
    let obj: serde_json::Value = serde_json::from_str(&content[start..=end])
        .map_err(|e| anyhow!("could not parse tags JSON ({e}): {content}"))?;

    let caption = obj["caption"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    let mut tags = Vec::new();
    if let Some(arr) = obj["tags"].as_array() {
        for t in arr {
            if let Some(s) = t.as_str() {
                let norm = s.trim().to_lowercase();
                if !norm.is_empty() && !tags.contains(&norm) {
                    tags.push(norm);
                }
            }
        }
    }
    Ok((caption, tags))
}

// --- frontmatter write-back ----------------------------------------------------
//
// Ported from ptsync's `markdown.rs` so files written here stay byte-compatible
// with what `ptsync sync` writes (same YAML emitter, same merge rules).

/// Merge auto-generated `caption` (scalar, overwritten) and `tags` (a unioned
/// set) into a sidecar `.md`, preserving every other key and the note body. Only
/// rewrites when the canonical frontmatter changes. Returns the `checksum`
/// recorded in the frontmatter, if any, for storing alongside the queue row.
fn write_tags_to_md(
    md_path: &Path,
    caption: &str,
    tags: &[String],
) -> anyhow::Result<Option<String>> {
    let existing = std::fs::read_to_string(md_path)?;
    let (e_yaml, e_body) = split_frontmatter(&existing);

    let mut root: Hash = if e_yaml.trim().is_empty() {
        Hash::default()
    } else {
        let docs = YamlLoader::load_from_str(&e_yaml)
            .map_err(|e| anyhow!("Could not parse frontmatter YAML in {md_path:?}: {e}"))?;
        match docs.into_iter().next() {
            Some(Yaml::Hash(hash)) => hash,
            Some(_) => return Err(anyhow!("Frontmatter root is not a mapping in {md_path:?}")),
            None => Hash::default(),
        }
    };

    let checksum = root
        .get(&Yaml::String("checksum".to_string()))
        .and_then(|y| y.as_str().map(|s| s.to_string()));

    let original = root.clone();
    if !caption.is_empty() {
        set_scalar(&mut root, "caption", Yaml::String(caption.to_string()));
    }
    yaml_array_merge(&mut root, "tags", tags);

    if root != original {
        let yaml = emit_yaml(&root)?;
        let mut s = String::new();
        s.push_str("---\n");
        s.push_str(&yaml);
        s.push_str("---\n");
        s.push_str(&e_body);
        std::fs::write(md_path, s)?;
    }
    Ok(checksum)
}

/// The sidecar markdown path for a media file: its own path with the extension
/// swapped for `.md`.
fn get_desired_markdown_path(media_path: &str) -> anyhow::Result<String> {
    if media_path.is_empty() {
        return Err(anyhow!("media path is empty"));
    }
    let last_slash = media_path.rfind('/').map_or(0, |i| i + 1);
    match media_path[last_slash..].rfind('.') {
        Some(dot) => Ok(format!("{}.md", &media_path[..last_slash + dot])),
        None => Ok(format!("{media_path}.md")),
    }
}

/// Grab anything between "---[\r]\n" and "---[\r]\n" into .0; the rest into .1.
/// On any malformed case, returns empty frontmatter and the original content.
fn split_frontmatter(file_contents: &str) -> (String, String) {
    let trimmed = file_contents.trim_start_matches(['\n', '\r']);
    if !trimmed.starts_with("---") {
        return (String::new(), file_contents.to_string());
    }
    let (line_ending, after_first) = if let Some(s) = trimmed.strip_prefix("---\r\n") {
        ("\r\n", s)
    } else if let Some(s) = trimmed.strip_prefix("---\n") {
        ("\n", s)
    } else {
        return (String::new(), file_contents.to_string());
    };

    let Some(end_pos) = after_first.find("---") else {
        return (String::new(), file_contents.to_string());
    };
    let fm_raw = &after_first[..end_pos];
    let after_end = &after_first[end_pos..];
    if fm_raw.trim().is_empty() {
        return (String::new(), file_contents.to_string());
    }
    let fm = fm_raw.trim_end_matches(['\n', '\r']).to_string();

    if let Some(rest) = after_end.strip_prefix("---\r\n") {
        let body = if rest.is_empty() {
            "\r\n".to_string()
        } else {
            rest.to_string()
        };
        (fm, body)
    } else if let Some(rest) = after_end.strip_prefix("---\n") {
        let body = if rest.is_empty() {
            "\n".to_string()
        } else {
            rest.to_string()
        };
        (fm, body)
    } else if let Some(after_closing) = after_end.strip_prefix("---") {
        if after_closing.is_empty() {
            (fm, String::new())
        } else {
            (fm, format!("{line_ending}{after_closing}"))
        }
    } else {
        (String::new(), file_contents.to_string())
    }
}

/// Set a scalar key in place (preserving position) rather than re-inserting it.
fn set_scalar(root: &mut Hash, key: &str, value: Yaml) {
    let k = Yaml::String(key.to_string());
    if root.get(&k).is_some() {
        root[&k] = value;
    } else {
        root.insert(k, value);
    }
}

/// Union `values` into the array at `key`, creating it if absent. Existing
/// members are kept; only genuinely new ones are appended.
fn yaml_array_merge(root: &mut Hash, key: &str, values: &[String]) {
    let k = Yaml::String(key.to_string());
    if let Some(Yaml::Array(existing)) = root.get(&k).cloned() {
        let mut merged = existing.clone();
        for v in values {
            let y = Yaml::String(v.clone());
            if !merged.contains(&y) {
                merged.push(y);
            }
        }
        if merged.len() != existing.len() {
            root[&k] = Yaml::Array(merged);
        }
        return;
    }
    if !values.is_empty() {
        let arr = values.iter().map(|v| Yaml::String(v.clone())).collect();
        root.insert(k, Yaml::Array(arr));
    }
}

/// Emit a YAML mapping as the body of a frontmatter block (no `---` fences, a
/// single trailing newline).
fn emit_yaml(root: &Hash) -> anyhow::Result<String> {
    let mut out = String::new();
    {
        let mut emitter = YamlEmitter::new(&mut out);
        emitter
            .dump(&Yaml::Hash(root.clone()))
            .map_err(|e| anyhow!("YAML dump failed: {e:?}"))?;
    }
    out = out.trim_start_matches("---").to_string();
    out = out.trim_start_matches('\n').to_string();
    out = out.trim_end_matches('\n').to_string();
    out += "\n";
    Ok(out)
}

/// Open (or create) the queue database — an ordinary local SQLite file.
async fn open_conn(path: &str) -> anyhow::Result<(Database, Connection)> {
    let db = turso::Builder::new_local(path).build().await?;
    let conn = db.connect()?;
    Ok((db, conn))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taggable_includes_heic_not_video() {
        assert!(is_taggable("2024/07/15/1430.jpg"));
        assert!(is_taggable("a/b/c.JPEG"));
        assert!(is_taggable("x.png"));
        assert!(is_taggable("x.gif"));
        assert!(is_taggable("x.heic"));
        assert!(is_taggable("x.HEIC"));
        assert!(!is_taggable("x.mp4"));
        assert!(!is_taggable("x.mov"));
        assert!(!is_taggable("x.md"));
    }

    #[test]
    fn markdown_path_swaps_extension() -> anyhow::Result<()> {
        assert_eq!(
            get_desired_markdown_path("2024/07/15/1430-22.jpg")?,
            "2024/07/15/1430-22.md"
        );
        assert_eq!(
            get_desired_markdown_path("undated/9f8e.heic")?,
            "undated/9f8e.md"
        );
        Ok(())
    }

    #[test]
    fn pending_rules() {
        assert!(is_pending(None, false, false));
        assert!(!is_pending(Some("done"), false, false));
        assert!(is_pending(Some("done"), false, true));
        assert!(!is_pending(Some("error"), false, false));
        assert!(is_pending(Some("error"), true, false));
    }

    #[test]
    fn parse_plain_json() -> anyhow::Result<()> {
        let (caption, tags) =
            parse_tag_json("{\"caption\": \"A dog on a beach\", \"tags\": [\"dog\", \"beach\"]}")?;
        assert_eq!(caption, "A dog on a beach");
        assert_eq!(tags, vec!["dog".to_string(), "beach".to_string()]);
        Ok(())
    }

    #[test]
    fn parse_json_with_fences_and_prose() -> anyhow::Result<()> {
        let content = "Sure! Here it is:\n```json\n{\"caption\":\"Sunset\",\
             \"tags\":[\"Sunset\",\"SKY\",\"sunset\"]}\n```";
        let (caption, tags) = parse_tag_json(content)?;
        assert_eq!(caption, "Sunset");
        assert_eq!(tags, vec!["sunset".to_string(), "sky".to_string()]);
        Ok(())
    }

    #[test]
    fn parse_missing_object_errors() {
        assert!(parse_tag_json("no json here").is_err());
    }

    #[test]
    fn downscale_preserves_aspect_and_caps_long_edge() {
        let big = image::DynamicImage::ImageRgb8(image::RgbImage::new(2000, 1000));
        let small = downscale(big, MAX_DIM);
        assert_eq!(small.width(), MAX_DIM);
        assert_eq!(small.height(), MAX_DIM / 2);
        // Already-small images are left untouched (no upscaling).
        let tiny = image::DynamicImage::ImageRgb8(image::RgbImage::new(100, 50));
        let same = downscale(tiny, MAX_DIM);
        assert_eq!((same.width(), same.height()), (100, 50));
    }

    #[test]
    fn encode_jpeg_roundtrips() -> anyhow::Result<()> {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::new(64, 48));
        let jpeg = encode_jpeg(&img, JPEG_QUALITY)?;
        let decoded = image::load_from_memory(&jpeg)?;
        assert_eq!((decoded.width(), decoded.height()), (64, 48));
        Ok(())
    }

    #[test]
    fn write_tags_merges_and_is_idempotent() -> anyhow::Result<()> {
        let dir = std::env::temp_dir().join(format!("auto-tagger-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let md = dir.join("note.md");
        std::fs::write(
            &md,
            "---\nchecksum: abc123\npeople:\n  - \"[[Paul]]\"\n---\n\n![](note.jpg)\n\nMy own note.\n",
        )?;

        let checksum = write_tags_to_md(&md, "A dog on a beach", &["dog".into(), "beach".into()])?;
        assert_eq!(checksum.as_deref(), Some("abc123"));

        let after = std::fs::read_to_string(&md)?;
        assert!(after.contains("caption: A dog on a beach"));
        assert!(after.contains("- dog"));
        assert!(after.contains("- beach"));
        assert!(after.contains("[[Paul]]"), "existing frontmatter preserved");
        assert!(after.contains("My own note."), "note body preserved");

        // Identical re-tag: no rewrite.
        write_tags_to_md(&md, "A dog on a beach", &["dog".into(), "beach".into()])?;
        let after2 = std::fs::read_to_string(&md)?;
        assert_eq!(after, after2, "identical re-tag must not rewrite the file");

        // New tag is unioned in; old ones kept.
        write_tags_to_md(&md, "A dog on a beach", &["sunset".into()])?;
        let after3 = std::fs::read_to_string(&md)?;
        assert!(after3.contains("- dog"));
        assert!(after3.contains("- sunset"));

        std::fs::remove_dir_all(&dir).ok();
        Ok(())
    }
}
