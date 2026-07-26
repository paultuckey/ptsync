use crate::fs::FileSystem;
use serde::{Deserialize, Serialize};
use std::io::Read;
use tracing::{debug, warn};

/// Longest filename Takeout emits, in characters. Not derivable from anything
/// else here — it is a property of Google's exporter, measured across a real
/// export, where both media files and sidecars pile up at exactly this length
/// and none exceeds it. A name too long is cut *before* its extension, so the
/// extension always survives: a 47-character stem plus `.jpg`, or a
/// 46-character stem plus `.json`. Duplicate counters are appended afterwards
/// and can push the result past the cap.
const TAKEOUT_FILENAME_MAX_CHARS: usize = 51;
const SUPP_TAG: &str = ".supplemental-metadata";
const JSON_EXT: &str = ".json";

/// Where a sidecar's stem is cut, derived from the cap above.
const SUPP_STEM_MAX_CHARS: usize = TAKEOUT_FILENAME_MAX_CHARS - JSON_EXT.len();

/// Extensions a still image can carry when it is the metadata-bearing original
/// behind a derived file or a live-photo motion clip. Each case is listed twice
/// because archives come off case-preserving and case-sensitive filesystems alike.
const STILL_EXTS: &[&str] = &[
    ".HEIC", ".heic", ".JPG", ".jpg", ".JPEG", ".jpeg", ".PNG", ".png",
];

/// Suffixes Google appends to a rendition it generated itself. Ordered longest
/// first so `-edited` is peeled before `-edit` and `-ed`, which are what is left
/// of it when the media filename hit its own length cap.
const DERIVED_SUFFIXES: &[&str] = &[
    "-ANIMATION",
    "-COLLAGE",
    "-EFFECTS",
    "-ERASER",
    "-MOTION",
    "-MOVIE",
    "-SMILE",
    "-edited",
    "-PANO",
    "-SNOW",
    "-edit",
    "-MIX",
    "-ed",
];

/// How deep to peel derived suffixes: `-ERASER-edited` is two renditions deep.
const MAX_DERIVED_DEPTH: usize = 3;

/// Find the Takeout sidecar json describing `path`, if one exists.
///
/// Google names it `{media file}.supplemental-metadata.json`, but only when that
/// name fits and only for files the user actually uploaded. Four rules reshape
/// it, and they compose:
///
/// 1. **Length.** The json filename is capped at [`TAKEOUT_FILENAME_MAX_CHARS`]
///    characters, so the tag is cut mid-word: a 29-character media name yields
///    `….jpg.supplemental-met.json`, a 40-character one `….JPG.suppl.json`.
/// 2. **Duplicate counters.** `FullSizeRender(42).jpg` is described by
///    `FullSizeRender.jpg.supplemental-metadata(42).json` — the `(42)` moves off
///    the media name onto the end of the json name, *after* truncation, which is
///    why the result can exceed the cap.
/// 3. **Extension-less titles.** When the original upload had no extension the
///    json is keyed on that bare title, even though the exported media file
///    gained one: `PicasaSync(6).jpg` -> `PicasaSync.supplemental-metadata(6).json`.
/// 4. **Derived files.** Renditions Google made itself — `-edited`, `-EFFECTS`,
///    `-ANIMATION` and friends — carry no sidecar; the metadata stays on the
///    original, which may have a different extension (an `-ANIMATION.gif` comes
///    from a `.jpg`). Live-photo motion clips work the same way:
///    `IMG_3716.MP4`'s metadata lives on `IMG_3716.HEIC`.
///
/// Candidates are tried most specific first, so an exact sidecar always wins over
/// one inherited from an original.
pub(crate) fn detect_supplemental_info(path: &str, container: &dyn FileSystem) -> Option<String> {
    let (dir, file_name) = split_dir(path);
    for candidate in supplemental_candidates(file_name) {
        let supp_info_path = format!("{dir}{candidate}");
        if container.exists(&supp_info_path) {
            return Some(supp_info_path);
        }
    }
    None
}

/// Split a relative path into its directory prefix (with trailing `/`, empty at
/// the root) and its file name. Paths from [`FileSystem::walk`] always use `/`.
fn split_dir(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..=i], &path[i + 1..]),
        None => ("", path),
    }
}

/// Split a file name into stem and extension, where the extension keeps its dot
/// and is empty when there is none. Leading dots belong to the stem, so a
/// `.hidden` file is all stem.
fn split_ext(file_name: &str) -> (&str, &str) {
    match file_name.rfind('.') {
        Some(i) if i > 0 => (&file_name[..i], &file_name[i..]),
        _ => (file_name, ""),
    }
}

/// Split a trailing Takeout duplicate counter off a stem:
/// `FullSizeRender(42)` -> `("FullSizeRender", "(42)")`.
fn split_counter(stem: &str) -> (&str, &str) {
    let Some(open) = stem.rfind('(') else {
        return (stem, "");
    };
    if !stem.ends_with(')') {
        return (stem, "");
    }
    let digits = &stem[open + 1..stem.len() - 1];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return (stem, "");
    }
    (&stem[..open], &stem[open..])
}

/// The json filename Takeout writes for a media file named `base`: the tag is
/// appended and the result truncated to fit the cap, then `counter` is added.
fn supp_json_name(base: &str, counter: &str) -> String {
    let stem: String = format!("{base}{SUPP_TAG}")
        .chars()
        .take(SUPP_STEM_MAX_CHARS)
        .collect();
    format!("{stem}{counter}{JSON_EXT}")
}

/// Strip one derived-rendition suffix, case-insensitively.
fn strip_derived_suffix(stem: &str) -> Option<&str> {
    DERIVED_SUFFIXES.iter().find_map(|suffix| {
        let start = stem.len().checked_sub(suffix.len())?;
        stem.is_char_boundary(start)
            .then(|| &stem[start..])
            .filter(|tail| tail.eq_ignore_ascii_case(suffix))
            .map(|_| &stem[..start])
    })
}

/// Every json filename that could describe `file_name`, most specific first.
fn supplemental_candidates(file_name: &str) -> Vec<String> {
    let (stem, ext) = split_ext(file_name);
    let (base, counter) = split_counter(stem);

    let mut out: Vec<String> = Vec::new();
    let mut push = |name: String| {
        if !out.contains(&name) {
            out.push(name);
        }
    };

    // Rule 1: the sidecar named after this exact file (truncated if long).
    push(supp_json_name(file_name, ""));
    // Rule 2: the duplicate counter relocated onto the json name.
    if !counter.is_empty() {
        push(supp_json_name(&format!("{base}{ext}"), counter));
    }
    // Rule 3: keyed on an extension-less upload title, counter either way round.
    push(supp_json_name(stem, ""));
    if !counter.is_empty() {
        push(supp_json_name(base, counter));
    }

    // Rule 4a: a rendition Google derived from an original that kept the sidecar.
    // Peeled repeatedly, since renditions stack (`-ERASER-edited`).
    let mut original = base;
    for _ in 0..MAX_DERIVED_DEPTH {
        let Some(stripped) = strip_derived_suffix(original) else {
            break;
        };
        original = stripped;
        for candidate_ext in std::iter::once(ext).chain(STILL_EXTS.iter().copied()) {
            let named = format!("{original}{candidate_ext}");
            if !counter.is_empty() {
                push(supp_json_name(&named, counter));
            }
            push(supp_json_name(&named, ""));
        }
        if !counter.is_empty() {
            push(supp_json_name(original, counter));
        }
        push(supp_json_name(original, ""));
    }

    // Rule 4b: a live-photo motion clip, whose metadata sits on the paired still.
    if is_motion_clip(ext) {
        for still_ext in STILL_EXTS {
            let named = format!("{base}{still_ext}");
            if !counter.is_empty() {
                push(supp_json_name(&named, counter));
            }
            push(supp_json_name(&named, ""));
        }
    }

    out
}

fn is_motion_clip(ext: &str) -> bool {
    ext.eq_ignore_ascii_case(".mp4") || ext.eq_ignore_ascii_case(".mov")
}

pub(crate) fn load_supplemental_info(
    path: &String,
    container: &dyn FileSystem,
) -> Option<PsSupplementalInfo> {
    let reader_r = container.open(path);
    let Ok(reader) = reader_r else {
        warn!("Could not read supplemental json file: {path}");
        return None;
    };
    debug!("  Loaded: {path}");
    parse_supplemental_info(reader)
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all(deserialize = "camelCase", serialize = "camelCase"))]
pub(crate) struct SupplementalInfoGeoData {
    pub(crate) latitude: Option<f64>,
    pub(crate) longitude: Option<f64>,
}
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all(deserialize = "camelCase", serialize = "camelCase"))]
pub(crate) struct SupplementalInfoPerson {
    pub(crate) name: Option<String>,
}
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all(deserialize = "camelCase", serialize = "camelCase"))]
pub(crate) struct SupplementalInfoDateTime {
    timestamp: Option<String>, // actually a unix timestamp in seconds eg, 1716539968
    pub(crate) formatted: Option<String>,
}

impl SupplementalInfoDateTime {
    pub(crate) fn timestamp_s_as_iso_8601(&self) -> Option<String> {
        if let Some(ts) = &self.timestamp
            && let Ok(ts_i64) = ts.parse::<i64>()
        {
            if ts.len() == 10 {
                // seconds to milliseconds
                return crate::util::timestamp_to_rfc3339(ts_i64 * 1000);
            }
            return crate::util::timestamp_to_rfc3339(ts_i64);
        }
        None
    }
}
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all(deserialize = "camelCase", serialize = "camelCase"))]
pub(crate) struct PsSupplementalInfo {
    pub(crate) geo_data: Option<SupplementalInfoGeoData>,
    pub(crate) geo_data_exif: Option<SupplementalInfoGeoData>,
    #[serde(default)]
    pub(crate) people: Vec<SupplementalInfoPerson>,
    pub(crate) photo_taken_time: Option<SupplementalInfoDateTime>,
    pub(crate) creation_time: Option<SupplementalInfoDateTime>,
}

fn parse_supplemental_info<R: Read>(json_reader: R) -> Option<PsSupplementalInfo> {
    let gs_r: Result<PsSupplementalInfo, _> = serde_json::from_reader(json_reader);
    if let Ok(gs) = gs_r {
        return Some(gs);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::OsFileSystem;
    use std::fs::File;

    /// Lay out `files` as empty files in a temp dir, then resolve the sidecar for
    /// `media`. Returns the json filename found, or `None`.
    fn detect_in(files: &[&str], media: &str) -> anyhow::Result<Option<String>> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir_all(dir.path().join("album"))?;
        for f in files {
            std::fs::write(dir.path().join("album").join(f), b"")?;
        }
        let fs = OsFileSystem::new(&dir.path().to_string_lossy());
        Ok(detect_supplemental_info(&format!("album/{media}"), &fs)
            .map(|p| p.trim_start_matches("album/").to_string()))
    }

    #[test]
    fn test_detect_plain_sidecar() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let json = "IMG_0001.jpg.supplemental-metadata.json";
        assert_eq!(detect_in(&[json], "IMG_0001.jpg")?.as_deref(), Some(json));
        Ok(())
    }

    #[test]
    fn test_detect_no_sidecar_at_all() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        assert_eq!(detect_in(&["unrelated.json"], "IMG_0001.jpg")?, None);
        Ok(())
    }

    /// The json filename is capped at 51 characters, cutting the tag mid-word.
    /// Each of these names is a different length, so each is cut at a different
    /// point — the two spellings that used to be hardcoded are just two of many.
    #[test]
    fn test_detect_truncated_json_name() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        for (media, json) in [
            (
                "IMG_20140913_100655_nopm_.jpg",
                "IMG_20140913_100655_nopm_.jpg.supplemental-met.json",
            ),
            (
                "9C19C4BF-E0C8-4D74-8DC4-4BB2338FB029.JPG",
                "9C19C4BF-E0C8-4D74-8DC4-4BB2338FB029.JPG.suppl.json",
            ),
            (
                "IMAGE_184EC254-639C-450E-9B14-7EE4377AE094.MOV",
                "IMAGE_184EC254-639C-450E-9B14-7EE4377AE094.MOV.json",
            ),
            (
                "2019 school photo wgc soraya.JPG",
                "2019 school photo wgc soraya.JPG.supplemental-.json",
            ),
        ] {
            assert_eq!(
                detect_in(&[json], media)?.as_deref(),
                Some(json),
                "truncated sidecar for {media}"
            );
            assert!(json.chars().count() <= TAKEOUT_FILENAME_MAX_CHARS);
        }
        Ok(())
    }

    /// A duplicate counter moves off the media name and onto the json name,
    /// *after* truncation - which is how the result can exceed the 51-char cap.
    #[test]
    fn test_detect_counter_moves_onto_json_name() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        for (media, json) in [
            (
                "FullSizeRender(42).jpg",
                "FullSizeRender.jpg.supplemental-metadata(42).json",
            ),
            (
                "IMAGE_D0209A83-AEF5-4D12-A17A-6981B7EDD66C.MOV(1).jpg",
                "IMAGE_D0209A83-AEF5-4D12-A17A-6981B7EDD66C.MOV(1).json",
            ),
        ] {
            assert_eq!(detect_in(&[json], media)?.as_deref(), Some(json));
        }
        Ok(())
    }

    /// An upload with no extension keys its json on the bare title even though
    /// the exported media file gained one.
    #[test]
    fn test_detect_extension_less_title() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let json = "PicasaSync.supplemental-metadata(6).json";
        assert_eq!(
            detect_in(&[json], "PicasaSync(6).jpg")?.as_deref(),
            Some(json)
        );

        let json = "11 8_30_46 AM.supplemental-metadata.json";
        assert_eq!(
            detect_in(&[json], "11 8_30_46 AM.jpg")?.as_deref(),
            Some(json)
        );
        Ok(())
    }

    /// Renditions Google generated carry no sidecar; the metadata stays on the
    /// original, which may have a different extension and may itself be a
    /// rendition.
    #[test]
    fn test_detect_derived_file_inherits_original() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        for (media, json) in [
            (
                "IMG_1189-edited.JPG",
                "IMG_1189.JPG.supplemental-metadata.json",
            ),
            (
                "IMG_20141017_180800-edited(1).jpg",
                "IMG_20141017_180800.jpg.supplemental-metadata(1).json",
            ),
            (
                "IMG_20140905_093118-SMILE.jpg",
                "IMG_20140905_093118.jpg.supplemental-metadata.json",
            ),
            // gif rendition of a jpg original: the extension changes too
            (
                "FullSizeRender-ANIMATION.gif",
                "FullSizeRender.jpg.supplemental-metadata.json",
            ),
            // two renditions deep
            (
                "IMG_20140712_105906-ERASER-edited.jpg",
                "IMG_20140712_105906.jpg.supplemental-metadata.json",
            ),
        ] {
            // Compared case-insensitively: an original's extension is tried in
            // both cases, and on a case-insensitive filesystem (macOS) whichever
            // spelling is tried first is the one that matches. Either resolves to
            // the same file.
            let found = detect_in(&[json], media)?;
            assert!(
                found
                    .as_deref()
                    .is_some_and(|f| f.eq_ignore_ascii_case(json)),
                "derived sidecar for {media}: expected {json:?}, got {found:?}"
            );
        }
        Ok(())
    }

    /// A live photo's motion clip is described by the still it was shot with.
    #[test]
    fn test_detect_live_photo_motion_clip() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let json = "IMG_3716.HEIC.supplemental-metadata.json";
        assert_eq!(detect_in(&[json], "IMG_3716.MP4")?.as_deref(), Some(json));
        Ok(())
    }

    /// A file's own sidecar always beats one it could inherit from an original.
    #[test]
    fn test_detect_prefers_exact_over_inherited() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let own = "IMG_1189-edited.JPG.supplemental-metadata.json";
        let original = "IMG_1189.JPG.supplemental-metadata.json";
        assert_eq!(
            detect_in(&[own, original], "IMG_1189-edited.JPG")?.as_deref(),
            Some(own)
        );
        Ok(())
    }

    #[test]
    fn test_split_counter() {
        assert_eq!(
            split_counter("FullSizeRender(42)"),
            ("FullSizeRender", "(42)")
        );
        assert_eq!(split_counter("FullSizeRender"), ("FullSizeRender", ""));
        // Not a counter: empty, non-numeric, or unterminated.
        assert_eq!(split_counter("Foo()"), ("Foo()", ""));
        assert_eq!(split_counter("Foo(bar)"), ("Foo(bar)", ""));
        assert_eq!(split_counter("Foo(1"), ("Foo(1", ""));
    }

    /// Truncation counts characters, not bytes: this name's narrow no-break space
    /// is three bytes but one character, and Takeout cuts after `.supple`.
    #[test]
    fn test_truncation_counts_characters() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let media = "Screenshot 2025-03-03 at 6.01.26\u{202f}PM.png";
        let json = "Screenshot 2025-03-03 at 6.01.26\u{202f}PM.png.supple.json";
        assert_eq!(detect_in(&[json], media)?.as_deref(), Some(json));
        Ok(())
    }

    #[test]
    fn test_parse_supp() -> anyhow::Result<()> {
        use anyhow::anyhow;
        crate::test_util::setup_log();
        use std::path::Path;
        let file = Path::new("test/test1.jpeg.supplemental-metadata.json");
        let json_reader = File::open(file)?;
        let r = parse_supplemental_info(json_reader)
            .ok_or_else(|| anyhow!("Failed to parse supplemental info"))?;
        // long lat limited to 6 decimal places
        let latitude = r
            .geo_data
            .as_ref()
            .ok_or_else(|| anyhow!("Missing geo_data"))?
            .latitude
            .ok_or_else(|| anyhow!("Missing latitude"))?;
        let longitude = r
            .geo_data
            .as_ref()
            .ok_or_else(|| anyhow!("Missing geo_data"))?
            .longitude
            .ok_or_else(|| anyhow!("Missing longitude"))?;
        assert_eq!(format!("{latitude:.4}"), "-21.6303".to_string());
        assert_eq!(format!("{longitude:.4}"), "152.2605".to_string());
        let p = r
            .people
            .first()
            .ok_or_else(|| anyhow!("Missing person"))?
            .clone();
        assert_eq!(p.name.ok_or_else(|| anyhow!("Missing name"))?, "Tim Tam");
        let ct = r
            .creation_time
            .ok_or_else(|| anyhow!("Missing creation_time"))?;
        assert_eq!(
            ct.formatted
                .ok_or_else(|| anyhow!("Missing formatted date"))?,
            "24 May 2024, 08:39:28 UTC"
        );
        assert_eq!(
            ct.timestamp.ok_or_else(|| anyhow!("Missing timestamp"))?,
            "1716539968"
        );
        Ok(())
    }

    #[test]
    fn test_parse_supp_without_people() -> anyhow::Result<()> {
        use anyhow::anyhow;
        crate::test_util::setup_log();
        let json = r#"{
            "title": "IMG_0001.jpg",
            "description": "",
            "photoTakenTime": {
                "timestamp": "1716337071",
                "formatted": "22 May 2024, 00:17:51 UTC"
            }
        }"#;
        let r = parse_supplemental_info(json.as_bytes())
            .ok_or_else(|| anyhow!("supplemental json without `people` failed to parse"))?;
        assert!(r.people.is_empty());
        let taken = r
            .photo_taken_time
            .ok_or_else(|| anyhow!("Missing photo_taken_time"))?;
        assert_eq!(
            taken
                .timestamp_s_as_iso_8601()
                .ok_or_else(|| anyhow!("Missing iso 8601"))?,
            "2024-05-22T00:17:51+00:00"
        );
        Ok(())
    }
}
