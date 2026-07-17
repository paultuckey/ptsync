//! Generates the demo GIF shown at the top of the README.
//!
//! Run it with:
//!
//! ```shell
//! cargo run --manifest-path demo/Cargo.toml
//! ```
//!
//! The pipeline is: build the real `ptsync` binary, run a short script of real
//! commands against a fixture Google Takeout zip built from `test/takeout_basic`,
//! capture what they actually print, write an [asciicast v2] file, then render it
//! to `tools/demo-generator/generated/demo.gif` with [agg].
//!
//! Two properties are worth knowing when editing this:
//!
//! - **The content is real.** Every line in the GIF is captured from the binary
//!   built from the current working tree, so the demo cannot drift away from what
//!   ptsync actually prints. Change the output, re-run this, and the GIF follows.
//! - **The timing is authored.** A real sync of the fixture finishes in about
//!   15ms, which would render as one unreadable frame, so this file paces the
//!   output out line by line. The `*_SECS` constants below are the only made-up
//!   numbers here.
//!
//! The fixture is the committed `test/takeout_basic` tree, never a real photo
//! library, so regenerating the GIF can't leak private photos into the README.
//!
//! [asciicast v2]: https://docs.asciinema.org/manual/asciicast/v2/
//! [agg]: https://github.com/asciinema/agg

use anyhow::{Context, Result, bail};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Terminal width. Wide enough that the longest line the demo prints — a photo
/// note's 64-character checksum — doesn't wrap.
const COLS: usize = 100;
/// Row bounds. The real height is the script's line count (so nothing scrolls
/// out of frame), clamped into this range so an unexpectedly chatty run can't
/// produce an absurdly tall GIF.
const MIN_ROWS: usize = 24;
const MAX_ROWS: usize = 46;

/// Pacing. These are the only invented numbers in the demo — see the module docs.
const TYPE_CHAR_SECS: f64 = 0.035;
const AFTER_ENTER_SECS: f64 = 0.45;
const PER_LINE_SECS: f64 = 0.13;
const BETWEEN_STEPS_SECS: f64 = 0.9;
/// How long the finished archive stays on screen before the GIF loops.
const HOLD_END_SECS: f64 = 3.0;

/// The fixture zip's name. Shaped like a real Google Takeout download so the
/// demo matches the README's quick start.
const TAKEOUT_ZIP: &str = "takeout-20240715.zip";
const ARCHIVE_DIR: &str = "photo-archive";

/// The agg release to render with, pinned so the GIF doesn't change out from
/// under us when agg does.
const AGG_TAG: &str = "v1.9.0";

fn main() -> Result<()> {
    let root = repo_root()?;
    let ptsync = build_ptsync(&root)?;
    let demo_dir = prepare_demo_dir(&root)?;

    let mut cast = Cast::new();
    cast.wait(0.6);

    // 1. The headline: a takeout zip goes in, a tidy archive comes out.
    cast.run(
        &demo_dir,
        &format!("ptsync sync --input {TAKEOUT_ZIP} --output {ARCHIVE_DIR}"),
        &[
            path_arg(&ptsync)?,
            "sync".to_string(),
            "--input".to_string(),
            TAKEOUT_ZIP.to_string(),
            "--output".to_string(),
            ARCHIVE_DIR.to_string(),
        ],
    )?;

    // 2. What that produced. Rendered from the directory ptsync just wrote, so
    //    it stays true if the layout ever changes.
    let archive = demo_dir.join(ARCHIVE_DIR);
    let tree = render_tree(&archive).context("rendering the output tree")?;
    cast.note(
        "# ...and your archive is now ordinary files, sorted by date:",
        &tree,
    );

    // 3. The payoff: a note the tool wrote, with metadata you own in plain text.
    //    Discovered rather than hard-coded, so it survives fixture changes.
    let note = richest_photo_note(&archive)?;
    let note_rel = relative_to(&note, &demo_dir)?;
    cast.run(
        &demo_dir,
        &format!("cat {note_rel}"),
        &["cat".to_string(), path_arg(&note)?],
    )?;

    let cast_path = root.join("tools/demo-generator/generated/demo.cast");
    cast.write(&cast_path)?;
    println!("wrote {}", cast_path.display());

    let gif_path = root.join("tools/demo-generator/generated/demo.gif");
    render_gif(&cast_path, &gif_path, cast.rows())?;
    println!("wrote {}", gif_path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// asciicast building
// ---------------------------------------------------------------------------

/// An asciicast v2 recording, assembled event by event.
///
/// The format is a JSON header line followed by one `[time, "o", text]` line per
/// chunk of terminal output. We synthesize the whole thing rather than recording
/// a live terminal, which keeps the result byte-identical between machines.
struct Cast {
    events: Vec<String>,
    /// Seconds since the start of the recording.
    clock: f64,
    /// Rows written so far, used to size the terminal so nothing scrolls away.
    lines: usize,
}

impl Cast {
    fn new() -> Self {
        Cast {
            events: Vec::new(),
            clock: 0.0,
            lines: 0,
        }
    }

    fn wait(&mut self, secs: f64) {
        self.clock += secs;
    }

    /// Emit a chunk of terminal output at the current time.
    ///
    /// Captured output uses bare `\n`, but a terminal needs `\r\n` to also return
    /// to column zero — without the `\r` every line would staircase to the right.
    fn out(&mut self, text: &str) {
        self.lines += text.matches('\n').count();
        let text = text.replace('\n', "\r\n");
        let event = serde_json::json!([self.clock, "o", text]);
        self.events.push(event.to_string());
    }

    /// Draw the prompt, then type `display` one character at a time.
    fn type_line(&mut self, display: &str) {
        self.out("\u{1b}[32m$\u{1b}[0m ");
        for ch in display.chars() {
            self.wait(TYPE_CHAR_SECS);
            self.out(&ch.to_string());
        }
        self.wait(AFTER_ENTER_SECS);
        self.out("\n");
    }

    /// Type a command, actually run it, and play back its real output.
    ///
    /// `display` is what the viewer sees typed; `argv` is what runs. They differ
    /// because the viewer should see `ptsync`, not an absolute path into `target/`.
    fn run(&mut self, cwd: &Path, display: &str, argv: &[String]) -> Result<String> {
        self.type_line(display);
        let output = capture(cwd, argv)?;
        self.play(&output);
        self.wait(BETWEEN_STEPS_SECS);
        self.out("\n");
        Ok(output)
    }

    /// Show a dimmed `#` comment followed by generated text.
    ///
    /// Used for the directory tree, which is derived from the real archive but
    /// isn't the output of a command — the comment marks it as narration rather
    /// than something the viewer should try to type.
    fn note(&mut self, comment: &str, body: &str) {
        self.out(&format!("\u{1b}[2m{comment}\u{1b}[0m\n"));
        self.wait(0.4);
        self.play(body);
        self.wait(BETWEEN_STEPS_SECS);
        self.out("\n");
    }

    /// Reveal text one line at a time, so the viewer can follow it.
    fn play(&mut self, text: &str) {
        for line in text.lines() {
            self.wait(PER_LINE_SECS);
            self.out(&format!("{line}\n"));
        }
    }

    /// Terminal height: tall enough to hold the whole script without scrolling.
    fn rows(&self) -> usize {
        (self.lines + 2).clamp(MIN_ROWS, MAX_ROWS)
    }

    fn write(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // `timestamp` is deliberately omitted: it would change on every run and
        // make the committed cast file churn for no reason.
        let header = serde_json::json!({
            "version": 2,
            "width": COLS,
            "height": self.rows(),
            "env": { "TERM": "xterm-256color" },
        });
        let mut file =
            File::create(path).with_context(|| format!("creating {}", path.display()))?;
        writeln!(file, "{header}")?;
        for event in &self.events {
            writeln!(file, "{event}")?;
        }
        Ok(())
    }
}

/// Run a command and return what it printed, stderr first.
///
/// ptsync logs through `tracing`, which writes to stderr, while plain tools like
/// `cat` write to stdout. No step in this demo writes to both, so concatenating
/// rather than interleaving is safe — if you add one that does, the ordering here
/// will need revisiting.
fn capture(cwd: &Path, argv: &[String]) -> Result<String> {
    let (program, args) = argv
        .split_first()
        .context("a step needs at least a program to run")?;
    let output = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} failed with {}:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let mut text = String::from_utf8_lossy(&output.stderr).to_string();
    text.push_str(&String::from_utf8_lossy(&output.stdout));
    Ok(text)
}

// ---------------------------------------------------------------------------
// fixture setup
// ---------------------------------------------------------------------------

/// The demo crate lives at `<root>/demo`, so the repo root is its parent.
fn repo_root() -> Result<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest
        .parent()
        .context("demo crate should have a parent directory")?
        .to_path_buf();
    Ok(root)
}

/// Build the real binary, so the GIF always shows the current working tree.
fn build_ptsync(root: &Path) -> Result<PathBuf> {
    println!("building ptsync (release)...");
    let status = Command::new("cargo")
        .args(["build", "--release", "--manifest-path"])
        .arg(root.join("Cargo.toml"))
        .status()
        .context("running cargo build")?;
    if !status.success() {
        bail!("cargo build failed");
    }
    let bin = root.join("target/release/ptsync");
    if !bin.is_file() {
        bail!("expected a ptsync binary at {}", bin.display());
    }
    Ok(bin)
}

/// Create a clean scratch directory holding just the fixture takeout zip.
///
/// It lives under `target/` so it's already gitignored, and the commands run
/// with this as their working directory so the paths in the GIF stay short and
/// relative rather than exposing an absolute path from the machine that built it.
fn prepare_demo_dir(root: &Path) -> Result<PathBuf> {
    let demo_dir = root.join("target/demo");
    if demo_dir.exists() {
        std::fs::remove_dir_all(&demo_dir)?;
    }
    std::fs::create_dir_all(&demo_dir)?;

    let fixture = root.join("test/takeout_basic");
    if !fixture.is_dir() {
        bail!("missing fixture at {}", fixture.display());
    }
    zip_dir(&fixture, &demo_dir.join(TAKEOUT_ZIP))?;
    Ok(demo_dir)
}

/// Zip `src` into `dest`, mirroring how a downloaded Google Takeout arrives.
fn zip_dir(src: &Path, dest: &Path) -> Result<()> {
    let file = File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    let mut writer = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    let mut entries = Vec::new();
    collect_files(src, &mut entries)?;
    // Sorted so the zip's central directory is stable between runs.
    entries.sort();

    for entry in entries {
        let name = relative_to(&entry, src)?;
        writer.start_file(name, options)?;
        let mut input =
            File::open(&entry).with_context(|| format!("opening {}", entry.display()))?;
        std::io::copy(&mut input, &mut writer)?;
    }
    writer.finish()?;
    Ok(())
}

fn collect_files(dir: &Path, found: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            collect_files(&path, found)?;
        } else if !is_ignorable(&path) {
            found.push(path);
        }
    }
    Ok(())
}

/// Skip macOS's `.DS_Store` droppings, which would otherwise land in the zip and
/// show up as a skipped file in the demo output.
fn is_ignorable(path: &Path) -> bool {
    path.file_name()
        .map(|name| name == ".DS_Store")
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// reading back what ptsync produced
// ---------------------------------------------------------------------------

/// Render a directory as a `tree`-style listing.
///
/// Done here rather than shelling out to `tree`, which isn't installed by
/// default on macOS and would be one more thing to install before regenerating.
fn render_tree(root: &Path) -> Result<String> {
    let name = root
        .file_name()
        .context("archive directory should have a name")?
        .to_string_lossy()
        .to_string();
    let mut out = format!("{name}\n");
    push_tree(root, &mut String::new(), &mut out)?;
    Ok(out)
}

fn push_tree(dir: &Path, prefix: &mut String, out: &mut String) -> Result<()> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    entries.retain(|path| !is_ignorable(path));
    // Directories first, then files, each alphabetically.
    entries.sort_by_key(|path| (path.is_file(), path.to_string_lossy().to_string()));

    for (index, path) in entries.iter().enumerate() {
        let last = index + 1 == entries.len();
        let branch = if last { "└── " } else { "├── " };
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        out.push_str(&format!("{prefix}{branch}{name}\n"));
        if path.is_dir() {
            let mut child = prefix.clone();
            child.push_str(if last { "    " } else { "│   " });
            push_tree(path, &mut child, out)?;
        }
    }
    Ok(())
}

/// Pick the per-photo note that best shows what ptsync does.
///
/// Album notes under `albums/` are skipped — the per-photo note is the one
/// carrying the metadata frontmatter that makes the point. Among the rest we
/// take the one with the most frontmatter, which is the one that demonstrates
/// the most: a photo that arrived twice has two `original-paths` and an album
/// wikilink, where a plain one-off has neither. Chosen by inspection rather
/// than hard-coded so it still picks well if the fixture changes.
fn richest_photo_note(archive: &Path) -> Result<PathBuf> {
    let mut found = Vec::new();
    collect_files(archive, &mut found)?;
    let albums = archive.join("albums");
    let mut notes: Vec<(usize, PathBuf)> = Vec::new();
    for path in found {
        if path.extension().map(|e| e != "md").unwrap_or(true) || path.starts_with(&albums) {
            continue;
        }
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        notes.push((body.lines().count(), path));
    }
    // Most lines wins; path breaks ties so the choice is stable between runs.
    notes.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let note = notes
        .into_iter()
        .next()
        .context("expected ptsync to write at least one photo note")?;
    Ok(note.1)
}

fn relative_to(path: &Path, base: &Path) -> Result<String> {
    let rel = path
        .strip_prefix(base)
        .with_context(|| format!("{} should sit under {}", path.display(), base.display()))?;
    Ok(rel.to_string_lossy().replace('\\', "/"))
}

fn path_arg(path: &Path) -> Result<String> {
    Ok(path.to_string_lossy().to_string())
}

// ---------------------------------------------------------------------------
// rendering
// ---------------------------------------------------------------------------

/// Put asciinema's `agg` on the PATH, installing it if this is the first run.
///
/// It's invoked as a subprocess rather than linked as a library on purpose: agg
/// is GPL-3.0 and ptsync is MIT, and running a program is not linking against
/// it. Shelling out keeps the GPL from reaching this crate, and keeps agg's
/// ~200 transitive dependencies (gifski, resvg, reqwest, ...) out of our tree.
fn ensure_agg() -> Result<()> {
    if Command::new("agg").arg("--version").output().is_ok() {
        return Ok(());
    }
    println!("agg not found — installing {AGG_TAG} (one-time, takes a few minutes)...");
    let status = Command::new("cargo")
        .args(["install", "--git", "https://github.com/asciinema/agg"])
        .args(["--tag", AGG_TAG])
        // Build against the lockfile agg ships, so this doesn't break on an
        // unrelated upstream dependency bump.
        .arg("--locked")
        .status()
        .context("running cargo install for agg")?;
    if !status.success() {
        bail!(
            "installing agg failed. Install it yourself and re-run:\n\n    \
             cargo install --git https://github.com/asciinema/agg --tag {AGG_TAG} --locked\n"
        );
    }
    Ok(())
}

/// Render the cast to a GIF with `agg`, asciinema's renderer.
fn render_gif(cast: &Path, gif: &Path, rows: usize) -> Result<()> {
    ensure_agg()?;
    println!("rendering {} with agg...", gif.display());
    let status = Command::new("agg")
        .args(["--cols", &COLS.to_string()])
        .args(["--rows", &rows.to_string()])
        .args(["--theme", "asciinema"])
        .args(["--font-size", "16"])
        .args(["--last-frame-duration", &HOLD_END_SECS.to_string()])
        .arg(cast)
        .arg(gif)
        .status()
        .context("running agg")?;
    if !status.success() {
        bail!("agg failed with {status}");
    }
    Ok(())
}
