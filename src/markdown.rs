use crate::fs::WritableFileSystem;
use crate::metadata::MediaFileInfo;
use crate::metadata::reconcile::{
    best_guess_archived, best_guess_description, best_guess_favorite, best_guess_lat_long,
    best_guess_rating, best_guess_taken_dt, best_guess_title,
};
use crate::util::name_part;
use anyhow::anyhow;
use std::io::{Cursor, Read};
use tracing::{debug, warn};
use yaml_rust2::yaml::Hash;
use yaml_rust2::{Yaml, YamlEmitter, YamlLoader};

pub(crate) fn mfm_from_media_file_info(
    media_info: &MediaFileInfo,
    album_names: &[String],
) -> PhotoSorterFrontMatter {
    let guessed_datetime = best_guess_taken_dt(media_info);
    // The note and the database column come from the same resolver, so a photo
    // can never be at one place in its note and another in the index.
    let (latitude, longitude) = best_guess_lat_long(media_info).unzip();
    let xmp = media_info.xmp_info.as_ref();
    PhotoSorterFrontMatter {
        path_original: media_info.original_path.clone(),
        checksum: media_info.hash_info.long_checksum.clone(),
        datetime: guessed_datetime,
        latitude,
        longitude,
        people: people_links(media_info),
        // Render album membership as wikilinks so each photo note links
        // back to the album files under `albums/`
        albums: album_names.iter().map(|n| as_wikilink(n)).collect(),
        // Keywords stay bare strings rather than wikilinks: `tags` is a key
        // Obsidian reads natively, and it expects tag text, not links.
        tags: xmp.map(|x| x.tags.clone()).unwrap_or_default(),
        label: xmp.and_then(|x| x.label.clone()),
        // Everything below is pooled across sidecars by the same shared
        // resolvers the date and coordinates use, so a caption written in Google
        // Photos reaches the note even when the photo has no XMP beside it - and
        // so the note and the index can never disagree about any of them.
        rating: best_guess_rating(media_info),
        title: best_guess_title(media_info),
        description: best_guess_description(media_info),
        favorite: best_guess_favorite(media_info),
        archived: best_guess_archived(media_info),
        // Not derivable from the file itself: which clip belongs to this still
        // is a property of the archive around it, so `sync_markdown` fills it in
        // once the clip's output path is known.
        motion: None,
    }
}

/// People (face tags) rendered as wikilinks, from Google's supplemental metadata
/// and from any XMP sidecar, pooled.
///
/// The two sources name the same people in the same archive - Google's face tags
/// for photos that came from Takeout, XMP's `PersonInImage` and face regions for
/// photos that passed through digiKam or Lightroom - so a person tagged in both
/// must yield one wikilink, not two. Order is source order with repeats dropped,
/// which keeps re-runs from reshuffling a note's frontmatter.
fn people_links(media_info: &MediaFileInfo) -> Vec<String> {
    let from_supp = media_info
        .supp_info
        .iter()
        .flat_map(|supp| supp.people.iter())
        .filter_map(|p| p.name.as_ref())
        .map(String::as_str);
    let from_xmp = media_info
        .xmp_info
        .iter()
        .flat_map(|xmp| xmp.people.iter())
        .map(String::as_str);

    let mut seen = std::collections::HashSet::new();
    from_supp
        .chain(from_xmp)
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .filter(|n| seen.insert(n.to_lowercase()))
        .map(as_wikilink)
        .collect()
}

fn as_wikilink(name: &str) -> String {
    format!("[[{name}]]")
}

// `Default` so a test can name the keys it is exercising and leave the rest
// alone, rather than every construction site being touched whenever a new key
// is derived from a sidecar.
#[derive(Default)]
pub(crate) struct PhotoSorterFrontMatter {
    pub(crate) path_original: Vec<String>,
    pub(crate) checksum: String,
    pub(crate) datetime: Option<String>,
    pub(crate) latitude: Option<f64>,
    pub(crate) longitude: Option<f64>,
    /// People (face tags), as wikilinks.
    pub(crate) people: Vec<String>,
    /// Albums this photo belongs to, as wikilinks.
    pub(crate) albums: Vec<String>,
    /// Keywords from an XMP sidecar, as plain Obsidian tag text.
    pub(crate) tags: Vec<String>,
    /// `xmp:Rating`, 0-5 (or -1 for rejected).
    pub(crate) rating: Option<i64>,
    /// `xmp:Label` - a colour name in most tools.
    pub(crate) label: Option<String>,
    /// `dc:title`, or Google's `title` when it is not just the file name. Heads
    /// the note body on first write as well as being a frontmatter key, so the
    /// note reads as a titled page and not only as metadata.
    pub(crate) title: Option<String>,
    /// `dc:description`, or the caption from Google's supplemental json. Seeds
    /// the note body on first write rather than becoming a frontmatter key,
    /// since it is prose and the body is where prose belongs.
    pub(crate) description: Option<String>,
    /// Google Photos' star. Absent from a note means false, so the key is only
    /// written when it is true.
    pub(crate) favorite: bool,
    /// Archived in Google Photos - hidden from its main grid, not deleted.
    /// Written only when true, like [`Self::favorite`].
    pub(crate) archived: bool,
    /// This live photo's motion clip, for a still that has one: its file name
    /// when it sits beside the note, otherwise its path within the archive. The
    /// clip has no note of its own, so this key is the only thing tying the two
    /// halves together once they are in the vault. See [`crate::live_photo`].
    pub(crate) motion: Option<String>,
}

/// Write (or update) the note beside a media file.
///
/// `motion_path` is the archive path of this still's live-photo clip, for the
/// still half of a pair. The clip itself gets no note - see [`crate::live_photo`]
/// - so this is what records that the two belong together.
pub(crate) fn sync_markdown(
    dry_run: bool,
    media_file: &MediaFileInfo,
    resolved_media_path: &str,
    album_names: &[String],
    motion_path: Option<&str>,
    output_c: &dyn WritableFileSystem,
) -> anyhow::Result<()> {
    // The sidecar is placed beside the *resolved* media file (the path
    // `write_media` actually wrote to), not the bare date path. Same-instant
    // photos collide on the date name and all but the first carry a checksum
    // suffix (`2213-20000-ccf63c8.jpg`); deriving the sidecar from the resolved
    // path gives each its own note (`2213-20000-ccf63c8.md`) instead of having
    // them all clobber a single `2213-20000.md`.
    let Some(output_path) = resolve_markdown_path(resolved_media_path, media_file, output_c)?
    else {
        // Two same-instant files in different formats want one note name. The
        // one that got there first keeps it; say so rather than inventing a
        // second name, so the clash is the user's to see and resolve.
        warn!(
            "No note written for {resolved_media_path}: {} already holds another file's note",
            get_desired_markdown_path(resolved_media_path)?
        );
        return Ok(());
    };
    let mut mfm = mfm_from_media_file_info(media_file, album_names);
    mfm.motion = motion_path.map(|clip| relative_to_note(&output_path, clip));
    // On first creation the body embeds the photo itself, so opening the note in
    // A markdown viewer shows the image. The body is preserved
    // verbatim on later runs, so user notes and this embed are never clobbered.
    let mut e_md = new_note_body(
        resolved_media_path,
        mfm.title.as_deref(),
        mfm.description.as_deref(),
    );
    let mut e_yaml = None;

    if output_c.exists(&output_path) {
        let mut reader = output_c.open(&output_path)?;
        let mut existing_md_bytes = Vec::new();
        match reader.read_to_end(&mut existing_md_bytes) {
            Ok(_) => {
                let existing_full_md = String::from_utf8_lossy(&existing_md_bytes);
                let (e_yaml_i, e_md_i) = split_frontmatter(&existing_full_md);
                e_yaml = Some(e_yaml_i);
                e_md = e_md_i;
            }
            Err(e) => {
                warn!("Could not read existing markdown file at {output_path:?}: {e}");
                return Err(anyhow!(
                    "Could not read existing markdown file at {output_path:?}: {e}"
                ));
            }
        }
    }
    let md_res = assemble_markdown(&mfm, &e_yaml, &e_md)?;
    if let AssembledMarkdown::Modified(md_str) = md_res {
        let md_bytes = md_str.as_bytes().to_vec();
        output_c.write(dry_run, &output_path, &mut Cursor::new(&md_bytes))?;
    }
    Ok(())
}

/// How to name `target` in `note_path`'s frontmatter: its bare file name when the
/// two sit in one directory, its whole archive path otherwise.
///
/// A live photo's two halves usually land together, since the clip inherits the
/// still's capture date through Google's shared sidecar json. Where they do not -
/// an iCloud export dates each half from its own metadata, which can differ by a
/// second and cross midnight - the bare name would point at nothing, so the full
/// path is used instead.
fn relative_to_note(note_path: &str, target: &str) -> String {
    fn dir_of(p: &str) -> Option<&str> {
        p.rfind('/').map(|i| &p[..i])
    }
    if dir_of(note_path) == dir_of(target) {
        return name_part(&target.to_string());
    }
    target.to_string()
}

/// Grab anything between "---[\r]\n" and "---[\r]\n" and put into .0. Put everything else into .1.
/// If any sort of invalid case is encountered, return empty frontmatter and original content.
pub(crate) fn split_frontmatter(file_contents: &str) -> (String, String) {
    let trimmed = file_contents.trim_start_matches(['\n', '\r']);

    if !trimmed.starts_with("---") {
        return ("".to_string(), file_contents.to_string());
    }

    let (line_ending, after_first_delim) = if let Some(stripped) = trimmed.strip_prefix("---\r\n") {
        ("\r\n", stripped)
    } else if let Some(stripped) = trimmed.strip_prefix("---\n") {
        ("\n", stripped)
    } else {
        // An opening "---" with no newline after it is not a delimiter.
        return ("".to_string(), file_contents.to_string());
    };

    if let Some(end_pos) = after_first_delim.find("---") {
        let potential_frontmatter = &after_first_delim[..end_pos];
        let after_end_delim = &after_first_delim[end_pos..];

        if let Some(remaining_content) = after_end_delim.strip_prefix("---\r\n") {
            if potential_frontmatter.trim().is_empty() {
                return ("".to_string(), file_contents.to_string());
            }
            let fm = potential_frontmatter
                .trim_end_matches(['\n', '\r'])
                .to_string();
            // The newline that followed the closing "---" belongs to the body.
            if remaining_content.is_empty() {
                return (fm, "\r\n".to_string());
            } else {
                return (fm, remaining_content.to_string());
            }
        } else if let Some(remaining_content) = after_end_delim.strip_prefix("---\n") {
            if potential_frontmatter.trim().is_empty() {
                return ("".to_string(), file_contents.to_string());
            }
            let fm = potential_frontmatter
                .trim_end_matches(['\n', '\r'])
                .to_string();
            if remaining_content.is_empty() {
                return (fm, "\n".to_string());
            } else {
                return (fm, remaining_content.to_string());
            }
        } else if let Some(after_closing) = after_end_delim.strip_prefix("---") {
            if potential_frontmatter.trim().is_empty() {
                return ("".to_string(), file_contents.to_string());
            }
            let fm = potential_frontmatter
                .trim_end_matches(['\n', '\r'])
                .to_string();
            // Body content ran straight into the closing "---" with no newline
            // between them; re-insert the file's own line ending.
            if !after_closing.is_empty() {
                let remaining_with_newline = format!("{line_ending}{after_closing}");
                return (fm, remaining_with_newline);
            } else {
                return (fm, "".to_string());
            }
        }
    }

    // No closing delimiter: the whole file is body.
    ("".to_string(), file_contents.to_string())
}

pub(crate) enum AssembledMarkdown {
    Modified(String),
    Unchanged(String),
}

impl AssembledMarkdown {
    pub(crate) fn into_string(self) -> String {
        match self {
            AssembledMarkdown::Modified(s) => s,
            AssembledMarkdown::Unchanged(s) => s,
        }
    }
}

pub(crate) fn assemble_markdown(
    mfm: &PhotoSorterFrontMatter,
    existing_yaml: &Option<String>,
    markdown_content: &str,
) -> anyhow::Result<AssembledMarkdown> {
    let MergedYaml { yaml, changed } = merge_yaml(existing_yaml, mfm)?;
    if yaml.is_empty() {
        warn!("Generated YAML is empty, returning markdown content");
        return Ok(AssembledMarkdown::Unchanged(markdown_content.to_string()));
    }
    // `changed` compares the parsed/canonicalised frontmatter, not the raw bytes,
    // so re-running over a hand-formatted file that is already semantically
    // current does not rewrite (and thus does not reformat) it.
    if !changed {
        return Ok(AssembledMarkdown::Unchanged(markdown_content.to_string()));
    }
    let mut s = String::new();
    s.push_str("---\n");
    s.push_str(&yaml);
    s.push_str("---\n");
    s.push_str(markdown_content);
    Ok(AssembledMarkdown::Modified(s))
}

struct MergedYaml {
    /// The merged frontmatter, emitted as the body of a frontmatter block.
    yaml: String,
    /// Whether the merge added or altered anything versus the existing
    /// frontmatter. When false the caller should leave the file untouched.
    changed: bool,
}

/// Merge the generated metadata in `fm` into any existing frontmatter `s`.
///
/// Existing keys (including ones the user added by hand) are preserved in place;
/// array fields like `original-paths`, `people` and `albums` are unioned. Returns
/// an error if `s` is present but is not a parseable YAML mapping, so the caller
/// can surface the problem and leave the file untouched rather than silently
/// dropping the generated metadata.
fn merge_yaml(s: &Option<String>, fm: &PhotoSorterFrontMatter) -> anyhow::Result<MergedYaml> {
    let mut root: Hash = match s {
        Some(s) => {
            let yaml_docs = YamlLoader::load_from_str(s)
                .map_err(|e| anyhow!("Could not parse existing frontmatter YAML: {e}"))?;
            let Some(yaml_doc) = yaml_docs.into_iter().next() else {
                return Err(anyhow!("No YAML document found in existing frontmatter"));
            };
            let Yaml::Hash(hash) = yaml_doc else {
                return Err(anyhow!("Existing frontmatter root is not a mapping"));
            };
            hash
        }
        None => Hash::default(),
    };
    // Snapshot before merging so we can tell whether anything actually changed.
    // Updates below preserve key order (re-inserting an existing key would move
    // it to the end), so an order-sensitive comparison is both correct and avoids
    // rewriting - and thus reformatting - files that are already current.
    let original = root.clone();

    if let Some(dt) = &fm.datetime {
        set_scalar(&mut root, "datetime", Yaml::String(dt.to_string()));
    }
    set_scalar(&mut root, "checksum", Yaml::String(fm.checksum.to_string()));
    yaml_array_merge(&mut root, &"original-paths".to_string(), &fm.path_original);
    yaml_array_merge(&mut root, &"people".to_string(), &fm.people);
    yaml_array_merge(&mut root, &"albums".to_string(), &fm.albums);
    yaml_array_merge(&mut root, &"tags".to_string(), &fm.tags);

    // Rating, label and title are opinions, not derived facts, so they are
    // seeded from the sidecar once and then left alone. Re-deriving them every
    // run would mean a star added in Obsidian is silently reverted to whatever
    // darktable thought on the next sync - the archive is the master copy, and a
    // hand-edited value has to survive.
    if let Some(rating) = fm.rating {
        set_scalar_if_absent(&mut root, "rating", Yaml::Integer(rating));
    }
    if let Some(label) = &fm.label {
        set_scalar_if_absent(&mut root, "label", Yaml::String(label.to_string()));
    }
    if let Some(title) = &fm.title {
        set_scalar_if_absent(&mut root, "title", Yaml::String(title.to_string()));
    }
    // Flags are written only when set. An absent key already means false, so
    // stamping `favorite: false` onto every note in an archive would be noise -
    // and because these go through `set_scalar_if_absent` like the other
    // opinions, un-favouriting a photo in the vault survives the next run.
    // (Rendering them as `tags` instead would not: `tags` is unioned, never
    // truncated, so a tag ptsync added could never be removed.)
    for (key, set) in [("favorite", fm.favorite), ("archived", fm.archived)] {
        if set {
            set_scalar_if_absent(&mut root, key, Yaml::Boolean(true));
        }
    }

    // Derived from where the clip actually landed, so it is re-stated every run
    // rather than seeded once: an archive reorganised by a later sync would
    // otherwise leave the still pointing at a file that has moved.
    if let Some(motion) = &fm.motion {
        set_scalar(&mut root, "motion", Yaml::String(motion.to_string()));
    }

    if let Some(lat) = fm.latitude {
        set_scalar(&mut root, "latitude", Yaml::Real(lat.to_string()));
    }
    if let Some(long) = fm.longitude {
        set_scalar(&mut root, "longitude", Yaml::Real(long.to_string()));
    }

    let changed = root != original;
    let merged = emit_yaml(&root)?;
    Ok(MergedYaml {
        yaml: merged,
        changed,
    })
}

/// Set a scalar key only when the note does not already carry it, so a value the
/// user has edited by hand is never re-derived over the top. A key present but
/// empty counts as set - clearing a rating is a decision too.
fn set_scalar_if_absent(root: &mut Hash, key: &str, value: Yaml) {
    let k = Yaml::String(key.to_string());
    if root.get(&k).is_none() {
        root.insert(k, value);
    }
}

/// Set a scalar key, updating an existing entry in place (preserving its
/// position) rather than re-inserting it (which would move it to the end).
fn set_scalar(root: &mut Hash, key: &str, value: Yaml) {
    let k = Yaml::String(key.to_string());
    if root.get(&k).is_some() {
        root[&k] = value;
    } else {
        root.insert(k, value);
    }
}

/// Emit a YAML mapping as the body of a frontmatter block (no `---` fences, a
/// single trailing newline).
fn emit_yaml(root: &Hash) -> anyhow::Result<String> {
    let mut out_str = String::new();
    {
        let mut emitter = YamlEmitter::new(&mut out_str);
        let yaml_hash = Yaml::Hash(root.clone());
        emitter
            .dump(&yaml_hash)
            .map_err(|e| anyhow!("YAML dump failed: {:?}", e))?;
    }
    out_str = out_str.trim_start_matches("---").to_string();
    out_str = out_str.trim_start_matches("\n").to_string();
    out_str = out_str.trim_end_matches("\n").to_string();
    out_str += "\n";
    Ok(out_str)
}

fn yaml_array_merge(root: &mut Hash, key: &String, arr: &Vec<String>) {
    if let Some(value_o) = root.get(&Yaml::String(key.clone())) {
        match value_o.clone() {
            Yaml::Array(po) => {
                let mut additions = Vec::new();
                for v in arr {
                    if po.contains(&Yaml::String(v.clone())) {
                        debug!("Path original {v} already exists in {key}");
                    } else {
                        debug!("Adding {v} to {key}");
                        additions.push(Yaml::String(v.to_string()));
                    }
                }
                if !additions.is_empty() {
                    let mut new_po = po;
                    new_po.extend(additions);
                    root[&Yaml::String(key.to_string())] = Yaml::Array(new_po);
                }
                return;
            }
            Yaml::BadValue => {
                // fall through as current value is empty/unknown
                warn!("Expected {key} to be an array, but it was a bad value");
            }
            _ => {
                warn!("Expected {key} to be an array, found: {value_o:?}");
                return;
            }
        }
    }
    debug!("Adding {key} to YAML");
    let arr_y = arr
        .iter()
        .map(|x| Yaml::String(x.to_string()))
        .collect::<Vec<Yaml>>();
    if !arr_y.is_empty() {
        root.insert(Yaml::String(key.to_string()), Yaml::Array(arr_y));
    }
}

/// Body for a note being created for the first time: a relative markdown image
/// embed of the sibling media file, with whatever title and description its
/// sidecars carried written underneath it.
///
/// A relative link (rather than a `![[wikilink]]`) renders in plain markdown
/// viewers too and is unambiguous because the photo is in the same directory as
/// the note. The embed uses the resolved media file name (including any
/// collision-resolving checksum suffix) so each same-instant photo embeds its
/// own file, not a shared bare name.
///
/// Title and description go in the body rather than in frontmatter alone
/// because they are what a person wrote about the photo, which is exactly what
/// the body is for - and putting them here means they are theirs to edit from
/// then on, since later runs never touch the body. They sit *under* the embed so
/// the photo is still the first thing the note shows.
pub(crate) fn new_note_body(
    resolved_media_path: &str,
    title: Option<&str>,
    description: Option<&str>,
) -> String {
    fn non_blank(s: Option<&str>) -> Option<&str> {
        s.map(str::trim).filter(|v| !v.is_empty())
    }

    let file_name = name_part(&resolved_media_path.to_string());
    let mut body = format!("\n![]({file_name})\n");
    // A heading rather than a plain line: markdown viewers give the note a
    // visible name, and Obsidian's outline picks it up.
    if let Some(title) = non_blank(title) {
        body.push_str(&format!("\n# {title}\n"));
    }
    if let Some(description) = non_blank(description) {
        body.push_str(&format!("\n{description}\n"));
    }
    body
}

/// The sidecar markdown path for a media file: the media extension swapped for
/// `.md` (`2213-20000-ccf63c8.jpg` -> `2213-20000-ccf63c8.md`). A name with no
/// extension keeps its whole self and gains one (`abc` -> `abc.md`).
///
/// Photo and note differing only in extension is what makes an archive read as
/// one thing in Obsidian rather than as files with `.jpg.md` tacked on.
///
/// The name is not always free. `resolve_output_path`
/// ([`crate::dedup::Deduplicator::resolve_output_path`]) resolves media
/// collisions per *full* name, extension included, so two different files
/// captured in the same millisecond but stored in different formats - a photo
/// and a video, say - both keep the bare date name and would want one note
/// between them. That is what [`disambiguated_markdown_path`] is for. Callers
/// should go through [`resolve_markdown_path`], which picks between the two;
/// this function is the name to *prefer*, not always the name to write.
pub(crate) fn get_desired_markdown_path(resolved_media_path: &str) -> anyhow::Result<String> {
    if resolved_media_path.is_empty() {
        return Err(anyhow!("Resolved media path is empty"));
    }
    Ok(format!(
        "{}.md",
        crate::path::strip_ext(resolved_media_path)
    ))
}

/// The fallback note name for a media file whose preferred one is taken: `.md`
/// appended to the *whole* name, extension included
/// (`2213-20000.mp4` -> `2213-20000.mp4.md`).
///
/// Unique by construction, since the media names it is derived from are already
/// unique. `None` when the media name has no extension, because then it would
/// equal [`get_desired_markdown_path`] and disambiguate nothing.
///
/// This is also the convention darktable and digiKam use for `.xmp` sidecars
/// (`IMG_1234.jpg.xmp`) - see [`crate::metadata::xmp`].
fn disambiguated_markdown_path(resolved_media_path: &str) -> Option<String> {
    let last_slash = resolved_media_path.rfind('/').map_or(0, |i| i + 1);
    let dot = resolved_media_path[last_slash..].rfind('.')?;
    (dot > 0).then(|| format!("{resolved_media_path}.md"))
}

/// Where this media file's note lives, or `None` when another file's note is
/// already sitting at the name it wants.
///
/// A note is this file's when its frontmatter records this checksum. That is the
/// only proof available - a same-instant sibling in another format wants the
/// exact same name - so anything else (a note holding a different checksum, or
/// none at all) is left alone and this file goes without one. Writing there
/// anyway would pool two files' `original-paths` under a single checksum; the
/// blocked file is reported by the caller so the clash is visible rather than
/// quietly worked around.
///
/// A note already sitting at the [`disambiguated_markdown_path`] for *this* file
/// is kept there rather than re-created under the preferred name, so an archive
/// synced while that was the default keeps the prose someone wrote in it.
fn resolve_markdown_path(
    resolved_media_path: &str,
    media_file: &MediaFileInfo,
    output_c: &dyn WritableFileSystem,
) -> anyhow::Result<Option<String>> {
    let desired = get_desired_markdown_path(resolved_media_path)?;
    let checksum = &media_file.hash_info.long_checksum;

    if output_c.exists(&desired) {
        return match read_note_checksum(&desired, output_c) {
            Some(existing) if &existing == checksum => Ok(Some(desired)),
            _ => Ok(None),
        };
    }

    if let Some(disambiguated) = disambiguated_markdown_path(resolved_media_path)
        && output_c.exists(&disambiguated)
        && read_note_checksum(&disambiguated, output_c).as_deref() == Some(checksum.as_str())
    {
        debug!("Keeping existing note {disambiguated} for {resolved_media_path}");
        return Ok(Some(disambiguated));
    }
    Ok(Some(desired))
}

/// The `checksum` recorded in an existing note's frontmatter, if it has one. An
/// unreadable or unparseable note yields `None`, which the caller reads as "not
/// this file's note".
fn read_note_checksum(path: &str, output_c: &dyn WritableFileSystem) -> Option<String> {
    let mut reader = output_c.open(path).ok()?;
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).ok()?;
    let (yaml, _) = split_frontmatter(&String::from_utf8_lossy(&bytes));
    let docs = YamlLoader::load_from_str(&yaml).ok()?;
    let Yaml::Hash(hash) = docs.into_iter().next()? else {
        return None;
    };
    match hash.get(&Yaml::String("checksum".to_string()))? {
        Yaml::String(s) => Some(s.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_split(text: &str, expected_fm: &str, expected_md: &str) {
        let (fm, md) = split_frontmatter(text);
        assert_eq!(
            fm, expected_fm,
            "Frontmatter mismatch for input: {:?}",
            text
        );
        assert_eq!(md, expected_md, "Markdown mismatch for input: {:?}", text);
    }

    fn get_mfi() -> PhotoSorterFrontMatter {
        PhotoSorterFrontMatter {
            path_original: vec!["p1".to_string(), "p2".to_string()],
            checksum: "abcdefg".to_string(),
            ..PhotoSorterFrontMatter::default()
        }
    }

    #[test]
    fn test_yaml_output() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let s = "foo:
  - list1
"
        .to_string();
        let yaml = merge_yaml(&Some(s), &get_mfi())?.yaml;
        assert_eq!(
            yaml,
            "foo:
  - list1
checksum: abcdefg
original-paths:
  - p1
  - p2
"
        );
        Ok(())
    }

    #[test]
    fn test_yaml_output_with_gps() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let mut mfi = get_mfi();
        mfi.latitude = Some(12.3456);
        mfi.longitude = Some(-78.9012);

        let yaml = merge_yaml(&None, &mfi)?.yaml;
        assert!(yaml.contains("latitude: 12.3456"));
        assert!(yaml.contains("longitude: -78.9012"));
        assert!(yaml.contains("checksum: abcdefg"));
        Ok(())
    }

    #[test]
    fn test_yaml_output_existing() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let s = "foo:
  - list1
original-paths:
  - p0
people:
  - Nandor
  - Nadja
  - Laszlo
checksum: abcdefg
"
        .to_string();
        let yaml = merge_yaml(&Some(s), &get_mfi())?.yaml;
        assert_eq!(
            yaml,
            "foo:
  - list1
original-paths:
  - p0
  - p1
  - p2
people:
  - Nandor
  - Nadja
  - Laszlo
checksum: abcdefg
"
        );
        Ok(())
    }

    #[test]
    fn parse_with_missing_beginning_line() {
        assert_split("", "", "");
    }

    #[test]
    fn parse_with_missing_ending_line() {
        assert_split("---\n", "", "---\n");
        assert_split("---\r\n", "", "---\r\n");
    }

    #[test]
    fn parse_with_empty_frontmatter() {
        assert_split("---\n---\n", "", "---\n---\n");
        assert_split("---\r\n---\r\n", "", "---\r\n---\r\n");
    }

    #[test]
    fn parse_with_missing_known_field() {
        assert_split("---\ndate: 2000-01-01\n---\n", "date: 2000-01-01", "\n");
        assert_split(
            "---\r\ndate: 2000-01-01\r\n---\r\n",
            "date: 2000-01-01",
            "\r\n",
        );
    }

    #[test]
    fn parse_with_valid_frontmatter() {
        assert_split(
            "---\ntitle: dummy_title---\ndummy_body",
            "title: dummy_title",
            "dummy_body",
        );
        assert_split(
            "---\r\ntitle: dummy_title---\r\ndummy_body",
            "title: dummy_title",
            "dummy_body",
        );
    }

    #[test]
    fn parse_with_extra_whitespace() {
        assert_split(
            "\n\n\n---\ntitle: dummy_title---\ndummy_body",
            "title: dummy_title",
            "dummy_body",
        );
        assert_split(
            "\r\n\r\n\r\n---\r\ntitle: dummy_title---\r\ndummy_body",
            "title: dummy_title",
            "dummy_body",
        );
    }

    #[test]
    fn parse_md_only_with_no_frontmatter() {
        assert_split("\n\n\ndummy_body", "", "\n\n\ndummy_body");
    }

    #[test]
    fn test_desired_md_path() {
        crate::test_util::setup_log();
        assert_eq!(get_desired_markdown_path("").ok(), None);
        // The media extension is swapped for `.md`, so note and photo differ
        // only in extension...
        assert_eq!(
            get_desired_markdown_path("2025/02/09/1818-44000.jpg").ok(),
            Some("2025/02/09/1818-44000.md".to_string())
        );
        // ...including when the name carries a collision-resolving checksum suffix.
        assert_eq!(
            get_desired_markdown_path("2025/02/09/1818-44000-ccf63c8.jpg").ok(),
            Some("2025/02/09/1818-44000-ccf63c8.md".to_string())
        );
        // Two same-instant files differing only by extension prefer one name
        // between them; `resolve_markdown_path` is what keeps them apart.
        assert_eq!(
            get_desired_markdown_path("2025/02/09/1818-44000.heic").ok(),
            get_desired_markdown_path("2025/02/09/1818-44000.mp4").ok()
        );
        // No extension to swap: `.md` is appended instead.
        assert_eq!(
            get_desired_markdown_path("abc").ok(),
            Some("abc.md".to_string())
        );
        // A leading dot names a hidden file, it does not start an extension.
        assert_eq!(
            get_desired_markdown_path("2025/.hidden").ok(),
            Some("2025/.hidden.md".to_string())
        );
    }

    #[test]
    fn test_disambiguated_md_path() {
        // Appending keeps the extension, so the fallback name is unique.
        assert_eq!(
            disambiguated_markdown_path("2025/02/09/1818-44000.jpg"),
            Some("2025/02/09/1818-44000.jpg.md".to_string())
        );
        assert_ne!(
            disambiguated_markdown_path("2025/02/09/1818-44000.heic"),
            disambiguated_markdown_path("2025/02/09/1818-44000.mp4")
        );
        // No extension means it would equal the preferred name, so there is none.
        assert_eq!(disambiguated_markdown_path("abc"), None);
        assert_eq!(disambiguated_markdown_path("2025/.hidden"), None);
    }

    #[test]
    fn test_new_note_body_embeds_sibling_photo() {
        assert_eq!(
            new_note_body("2025/02/09/1818-44000.jpg", None, None),
            "\n![](1818-44000.jpg)\n"
        );
        // The embed uses the resolved (suffixed) file name, not the bare date name.
        assert_eq!(
            new_note_body("2025/02/09/1818-44000-ccf63c8.jpg", None, None),
            "\n![](1818-44000-ccf63c8.jpg)\n"
        );
        // A description seeds the body below the embed.
        assert_eq!(
            new_note_body("2025/02/09/1818-44000.jpg", None, Some("Low tide.")),
            "\n![](1818-44000.jpg)\n\nLow tide.\n"
        );
        // An empty description is not worth a blank paragraph.
        assert_eq!(
            new_note_body("2025/02/09/1818-44000.jpg", None, Some("   ")),
            "\n![](1818-44000.jpg)\n"
        );
    }

    /// The flags are frontmatter booleans, written only when set - and, like the
    /// other opinions, never re-derived over a hand edit.
    #[test]
    fn test_flags_are_written_only_when_set() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let fm = PhotoSorterFrontMatter {
            checksum: "abc".to_string(),
            favorite: true,
            ..PhotoSorterFrontMatter::default()
        };
        let md = assemble_markdown(&fm, &None, "")?.into_string();
        assert!(md.contains("favorite: true"), "got:\n{md}");
        assert!(
            !md.contains("archived"),
            "an unset flag is left out entirely, got:\n{md}"
        );

        // Un-favourited by hand in the vault. The next run merges the derived
        // keys in around it and must leave that decision alone rather than
        // putting the star back.
        let existing = Some("favorite: false".to_string());
        let md = assemble_markdown(&fm, &existing, "")?.into_string();
        assert!(md.contains("favorite: false"), "got:\n{md}");
        assert!(!md.contains("favorite: true"));
        assert!(md.contains("checksum: abc"), "got:\n{md}");

        // And a note that already says what the sidecar says is not rewritten
        // at all, so no file is touched just to restate a flag.
        let existing = Some("checksum: abc\nfavorite: true".to_string());
        assert!(matches!(
            assemble_markdown(&fm, &existing, "body")?,
            AssembledMarkdown::Unchanged(_)
        ));
        Ok(())
    }

    /// A title heads the prose, and both sit under the embed so the photo stays
    /// the first thing the note shows.
    #[test]
    fn test_new_note_body_puts_title_under_the_embed() {
        assert_eq!(
            new_note_body(
                "2025/02/09/1818-44000.jpg",
                Some("Sunset"),
                Some("Low tide.")
            ),
            "\n![](1818-44000.jpg)\n\n# Sunset\n\nLow tide.\n"
        );
        // A title with no description, and the reverse, each stand alone.
        assert_eq!(
            new_note_body("2025/02/09/1818-44000.jpg", Some("Sunset"), None),
            "\n![](1818-44000.jpg)\n\n# Sunset\n"
        );
        assert_eq!(
            new_note_body("2025/02/09/1818-44000.jpg", Some(" "), Some("Low tide.")),
            "\n![](1818-44000.jpg)\n\nLow tide.\n"
        );
    }

    fn mfi_with_supp(
        geo: Option<crate::metadata::supplemental::SupplementalInfoGeoData>,
        people: &[&str],
    ) -> MediaFileInfo {
        use crate::metadata::supplemental::{PsSupplementalInfo, SupplementalInfoPerson};
        let mut m = MediaFileInfo::new_for_test();
        m.supp_info = Some(PsSupplementalInfo {
            geo_data: geo,
            people: people
                .iter()
                .map(|n| SupplementalInfoPerson {
                    name: Some(n.to_string()),
                })
                .collect(),
            ..PsSupplementalInfo::default()
        });
        m
    }

    #[test]
    fn test_mfm_people_albums_and_supplemental_gps() {
        use crate::metadata::supplemental::SupplementalInfoGeoData;
        // People come from supplemental metadata; blank names are dropped. GPS is
        // taken from supplemental geo_data when EXIF has none.
        let geo = SupplementalInfoGeoData {
            latitude: Some(-21.6303),
            longitude: Some(152.2605),
        };
        let m = mfi_with_supp(Some(geo), &["Tim Tam", "  ", "Nandor"]);
        let mfm = mfm_from_media_file_info(&m, &["Holiday".to_string()]);
        assert_eq!(mfm.people, vec!["[[Tim Tam]]", "[[Nandor]]"]);
        assert_eq!(mfm.albums, vec!["[[Holiday]]"]);
        assert_eq!(mfm.latitude, Some(-21.6303));
        assert_eq!(mfm.longitude, Some(152.2605));
    }

    /// The note's coordinates are whatever [`best_guess_lat_long`] resolved, for
    /// every kind of source. This is the guarantee centralising bought: the two
    /// used to be separate implementations that disagreed, so a video had
    /// coordinates in the database and none in its note, and a photo with both
    /// Google geo fields set could be placed differently in each.
    #[test]
    fn test_mfm_gps_matches_the_shared_resolver() {
        use crate::metadata::exif::PsExifInfo;
        use crate::metadata::supplemental::{PsSupplementalInfo, SupplementalInfoGeoData};
        use crate::metadata::track::PsTrackInfo;

        let geo = |lat: f64, long: f64| SupplementalInfoGeoData {
            latitude: Some(lat),
            longitude: Some(long),
        };

        // A video with GPS only in its track metadata - the case the note used
        // to miss entirely.
        let mut video = MediaFileInfo::new_for_test();
        video.track_info = Some(PsTrackInfo {
            gps_iso_6709: Some("+27.5916+086.5640/".to_string()),
            latitude: Some(27.5916),
            longitude: Some(86.5640),
            ..Default::default()
        });

        // A photo carrying both of Google's geo fields, which the two
        // implementations used to rank in opposite orders.
        let mut both_geo = MediaFileInfo::new_for_test();
        both_geo.supp_info = Some(PsSupplementalInfo {
            geo_data: Some(geo(5.0, 6.0)),
            geo_data_exif: Some(geo(3.0, 4.0)),
            ..PsSupplementalInfo::default()
        });

        // EXIF present, which must still outrank everything else.
        let mut with_exif = both_geo.clone();
        with_exif.exif_info = Some(PsExifInfo {
            latitude: Some(1.0),
            longitude: Some(2.0),
            ..Default::default()
        });

        for info in [&video, &both_geo, &with_exif] {
            let mfm = mfm_from_media_file_info(info, &[]);
            assert_eq!(
                (mfm.latitude, mfm.longitude),
                best_guess_lat_long(info).unzip(),
                "note coordinates must be exactly what the shared resolver returns"
            );
        }
        // And the video really does get coordinates now, rather than silently none.
        assert_eq!(
            mfm_from_media_file_info(&video, &[]).latitude,
            Some(27.5916)
        );
    }

    #[test]
    fn test_yaml_wikilinks_emit_and_round_trip() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let mut mfm = get_mfi();
        mfm.people = vec!["[[Tim Tam]]".to_string()];
        mfm.albums = vec!["[[Holiday]]".to_string()];
        let first = merge_yaml(&None, &mfm)?;
        assert!(first.changed);
        assert!(first.yaml.contains("[[Tim Tam]]"));
        assert!(first.yaml.contains("[[Holiday]]"));
        // Re-running over the emitted frontmatter adds nothing, which proves the
        // wikilinks re-parse as valid YAML and that there is no rewrite churn.
        let second = merge_yaml(&Some(first.yaml.clone()), &mfm)?;
        assert!(
            !second.changed,
            "re-run should be a no-op, got:\n{}",
            second.yaml
        );
        Ok(())
    }

    #[test]
    fn test_no_rewrite_when_reformatted_but_current() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        // Frontmatter that already contains everything the tool would add, but
        // hand-indented and reordered. Comparison is on parsed content, so this
        // must not be flagged as changed (no reformatting churn).
        let mangled = "original-paths:\n      - p1\n      - p2\nchecksum: abcdefg\n".to_string();
        let res = merge_yaml(&Some(mangled), &get_mfi())?;
        assert!(
            !res.changed,
            "reformatted-but-equal frontmatter should not be rewritten:\n{}",
            res.yaml
        );
        Ok(())
    }

    #[test]
    fn test_assemble_markdown_unchanged_on_rerun_skips_write() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        // First try yields Modified output that sync_markdown would write to disk.
        let mfm = get_mfi();
        let first = assemble_markdown(&mfm, &None, "\n![](x.jpg)\n")?;
        let AssembledMarkdown::Modified(full) = first else {
            return Err(anyhow!("first assembly should be Modified"));
        };
        // Re-run: split the on-disk file exactly as sync_markdown does, then
        // re-assemble. With nothing changed it must report Unchanged, which is how
        // sync_markdown knows to skip the write (a true no-op, not identical bytes).
        let (yaml, body) = split_frontmatter(&full);
        let second = assemble_markdown(&mfm, &Some(yaml), &body)?;
        assert!(
            matches!(second, AssembledMarkdown::Unchanged(_)),
            "re-running over current frontmatter must not rewrite the sidecar"
        );
        Ok(())
    }
}
