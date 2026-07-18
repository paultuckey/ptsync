use crate::fs::WritableFileSystem;
use crate::media::{MediaFileInfo, best_guess_taken_dt};
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
    let (latitude, longitude) = best_guess_coords(media_info);
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
    }
}

/// Best guess at GPS coordinates, preferring the EXIF embedded in the file, then
/// the supplemental metadata Google ships alongside it (Takeout often strips EXIF
/// GPS but keeps it in the metadata JSON). Google writes `0,0` to mean "no
/// location", so it's treated as absent.
fn best_guess_coords(media_info: &MediaFileInfo) -> (Option<f64>, Option<f64>) {
    if let Some(exif) = &media_info.exif_info
        && let Some(coords) = non_null_island(exif.latitude, exif.longitude)
    {
        return (Some(coords.0), Some(coords.1));
    }
    if let Some(supp) = &media_info.supp_info {
        for geo in [&supp.geo_data, &supp.geo_data_exif].into_iter().flatten() {
            if let Some(coords) = non_null_island(geo.latitude, geo.longitude) {
                return (Some(coords.0), Some(coords.1));
            }
        }
    }
    (None, None)
}

fn non_null_island(lat: Option<f64>, long: Option<f64>) -> Option<(f64, f64)> {
    match (lat, long) {
        (Some(lat), Some(long)) if lat != 0.0 || long != 0.0 => Some((lat, long)),
        _ => None,
    }
}

/// People (face tags) from Google supplemental metadata, rendered as wikilinks
fn people_links(media_info: &MediaFileInfo) -> Vec<String> {
    let Some(supp) = &media_info.supp_info else {
        return vec![];
    };
    supp.people
        .iter()
        .filter_map(|p| p.name.as_ref())
        .map(|n| n.trim())
        .filter(|n| !n.is_empty())
        .map(as_wikilink)
        .collect()
}

fn as_wikilink(name: &str) -> String {
    format!("[[{name}]]")
}

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
}

pub(crate) fn sync_markdown(
    dry_run: bool,
    media_file: &MediaFileInfo,
    resolved_media_path: &str,
    album_names: &[String],
    output_c: &dyn WritableFileSystem,
) -> anyhow::Result<()> {
    // The sidecar is placed beside the *resolved* media file (the path
    // `write_media` actually wrote to), not the bare date path. Same-instant
    // photos collide on the date name and all but the first carry a checksum
    // suffix (`2213-20000-ccf63c8.jpg`); deriving the sidecar from the resolved
    // path gives each its own note (`2213-20000-ccf63c8.md`) instead of having
    // them all clobber a single `2213-20000.md`.
    let output_path = get_desired_markdown_path(resolved_media_path)?;
    let mfm = mfm_from_media_file_info(media_file, album_names);
    // On first creation the body embeds the photo itself, so opening the note in
    // A markdown viewer shows the image. The body is preserved
    // verbatim on later runs, so user notes and this embed are never clobbered.
    let mut e_md = new_note_body(resolved_media_path);
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

/// Grab anything between "---[\r]\n" and "---[\r]\n" and put into .0. Put everything else into .1.
/// If any sort of invalid case is encountered, return empty frontmatter and original content.
pub(crate) fn split_frontmatter(file_contents: &str) -> (String, String) {
    // Handle leading whitespace - trim leading newlines and carriage returns
    let trimmed = file_contents.trim_start_matches(['\n', '\r']);

    // Check if the file starts with "---"
    if !trimmed.starts_with("---") {
        return ("".to_string(), file_contents.to_string());
    }

    // Find the first newline after the opening "---"
    let (line_ending, after_first_delim) = if let Some(stripped) = trimmed.strip_prefix("---\r\n") {
        ("\r\n", stripped) // Skip "---\r\n"
    } else if let Some(stripped) = trimmed.strip_prefix("---\n") {
        ("\n", stripped) // Skip "---\n"
    } else {
        // No newline after opening "---", treat as invalid
        return ("".to_string(), file_contents.to_string());
    };

    // Find the closing "---" delimiter
    if let Some(end_pos) = after_first_delim.find("---") {
        let potential_frontmatter = &after_first_delim[..end_pos];
        let after_end_delim = &after_first_delim[end_pos..];

        // Check if the closing "---" is followed by a newline or is at the end
        if let Some(remaining_content) = after_end_delim.strip_prefix("---\r\n") {
            // Special case: if frontmatter is empty, return original content
            if potential_frontmatter.trim().is_empty() {
                return ("".to_string(), file_contents.to_string());
            }

            // Remove trailing newline from frontmatter if present
            let fm = potential_frontmatter
                .trim_end_matches(['\n', '\r'])
                .to_string();
            // If remaining content is empty, but we had a newline after ---, include it
            if remaining_content.is_empty() {
                return (fm, "\r\n".to_string());
            } else {
                return (fm, remaining_content.to_string());
            }
        } else if let Some(remaining_content) = after_end_delim.strip_prefix("---\n") {
            // Special case: if frontmatter is empty, return original content
            if potential_frontmatter.trim().is_empty() {
                return ("".to_string(), file_contents.to_string());
            }

            // Remove trailing newline from frontmatter if present
            let fm = potential_frontmatter
                .trim_end_matches(['\n', '\r'])
                .to_string();
            // If remaining content is empty, but we had a newline after ---, include it
            if remaining_content.is_empty() {
                return (fm, "\n".to_string());
            } else {
                return (fm, remaining_content.to_string());
            }
        } else if let Some(after_closing) = after_end_delim.strip_prefix("---") {
            // Special case: if frontmatter is empty, return original content
            if potential_frontmatter.trim().is_empty() {
                return ("".to_string(), file_contents.to_string());
            }

            // Remove trailing newline from frontmatter if present
            let fm = potential_frontmatter
                .trim_end_matches(['\n', '\r'])
                .to_string();

            // If there's content after the closing ---, it should be the remaining content
            // If the original had CRLF line endings, preserve that in the remaining content
            if !after_closing.is_empty() {
                let remaining_with_newline = format!("{line_ending}{after_closing}");
                return (fm, remaining_with_newline);
            } else {
                // File ends with "---"
                return (fm, "".to_string());
            }
        }
    }

    // No valid closing delimiter found, treat as invalid
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

/// Body is a relative markdown image embed of the sibling media file.
/// A relative link (rather than a
/// `![[wikilink]]`) renders in plain markdown viewers too and is unambiguous
/// because the photo is in the same directory as the note. The embed uses the
/// resolved media file name (including any collision-resolving checksum suffix)
/// so each same-instant photo embeds its own file, not a shared bare name.
fn new_note_body(resolved_media_path: &str) -> String {
    let file_name = name_part(&resolved_media_path.to_string());
    format!("\n![]({file_name})\n")
}

/// The sidecar markdown path for a media file: the media file's own path with
/// its extension swapped for `.md`, so the note is a sibling of the photo even
/// when the photo's name carries a collision-resolving checksum suffix
/// (`2213-20000-ccf63c8.jpg` -> `2213-20000-ccf63c8.md`).
pub(crate) fn get_desired_markdown_path(resolved_media_path: &str) -> anyhow::Result<String> {
    if resolved_media_path.is_empty() {
        return Err(anyhow!("Resolved media path is empty"));
    }
    // Swap the trailing extension for `.md`. The file name carries exactly one
    // dot (the extension); date-based names, checksums and the `undated` folder
    // contain none, so the final dot - when it sits in the file name - is the
    // extension separator.
    let last_slash = resolved_media_path.rfind('/').map_or(0, |i| i + 1);
    match resolved_media_path[last_slash..].rfind('.') {
        Some(dot) => Ok(format!("{}.md", &resolved_media_path[..last_slash + dot])),
        None => Ok(format!("{resolved_media_path}.md")),
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
            datetime: None,
            checksum: "abcdefg".to_string(),
            latitude: None,
            longitude: None,
            people: vec![],
            albums: vec![],
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
        // The media extension is swapped for `.md`, so the sidecar is a sibling
        // of the photo it describes...
        assert_eq!(
            get_desired_markdown_path("2025/02/09/1818-44000.jpg").ok(),
            Some("2025/02/09/1818-44000.md".to_string())
        );
        // ...including when the name carries a collision-resolving checksum suffix.
        assert_eq!(
            get_desired_markdown_path("2025/02/09/1818-44000-ccf63c8.jpg").ok(),
            Some("2025/02/09/1818-44000-ccf63c8.md".to_string())
        );
        // A name without an extension just gains `.md` (dots only ever appear in
        // the file name, never in the date directories).
        assert_eq!(
            get_desired_markdown_path("abc").ok(),
            Some("abc.md".to_string())
        );
    }

    #[test]
    fn test_new_note_body_embeds_sibling_photo() {
        assert_eq!(
            new_note_body("2025/02/09/1818-44000.jpg"),
            "\n![](1818-44000.jpg)\n"
        );
        // The embed uses the resolved (suffixed) file name, not the bare date name.
        assert_eq!(
            new_note_body("2025/02/09/1818-44000-ccf63c8.jpg"),
            "\n![](1818-44000-ccf63c8.jpg)\n"
        );
    }

    fn mfi_with_supp(
        geo: Option<crate::supplemental_info::SupplementalInfoGeoData>,
        people: &[&str],
    ) -> MediaFileInfo {
        use crate::supplemental_info::{PsSupplementalInfo, SupplementalInfoPerson};
        let mut m = MediaFileInfo::new_for_test();
        m.supp_info = Some(PsSupplementalInfo {
            geo_data: geo,
            geo_data_exif: None,
            people: people
                .iter()
                .map(|n| SupplementalInfoPerson {
                    name: Some(n.to_string()),
                })
                .collect(),
            photo_taken_time: None,
            creation_time: None,
        });
        m
    }

    #[test]
    fn test_mfm_people_albums_and_supplemental_gps() {
        use crate::supplemental_info::SupplementalInfoGeoData;
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

    #[test]
    fn test_mfm_null_island_gps_is_dropped() {
        use crate::supplemental_info::SupplementalInfoGeoData;
        // Google writes 0,0 to mean "no location"; it must not be recorded.
        let geo = SupplementalInfoGeoData {
            latitude: Some(0.0),
            longitude: Some(0.0),
        };
        let m = mfi_with_supp(Some(geo), &[]);
        let mfm = mfm_from_media_file_info(&m, &[]);
        assert_eq!(mfm.latitude, None);
        assert_eq!(mfm.longitude, None);
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

    #[test]
    fn test_malformed_frontmatter_errors_rather_than_dropping_metadata() {
        crate::test_util::setup_log();
        // Unparseable YAML must surface as an error so the caller leaves the file
        // untouched, instead of silently discarding the generated metadata.
        assert!(merge_yaml(&Some("foo: [unclosed".to_string()), &get_mfi()).is_err());
        // A non-mapping root is equally unusable.
        assert!(merge_yaml(&Some("- a\n- b\n".to_string()), &get_mfi()).is_err());
    }
}
