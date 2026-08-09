use crate::fs::FileSystem;
use crate::util::OutputTZ;
use serde::{Deserialize, Serialize};
use std::io::Read;
use tracing::{debug, warn};

/// Longest filename Takeout emits, in characters. A property of Google's
/// exporter, measured across a real export rather than documented. Names are cut
/// *before* the extension, so the extension always survives; duplicate counters
/// are appended afterwards and can push the result past the cap.
const TAKEOUT_FILENAME_MAX_CHARS: usize = 51;
const SUPP_TAG: &str = ".supplemental-metadata";
const JSON_EXT: &str = ".json";

/// Where a sidecar's stem is cut, derived from the cap above.
const SUPP_STEM_MAX_CHARS: usize = TAKEOUT_FILENAME_MAX_CHARS - JSON_EXT.len();

/// Extensions a still image can carry when it is the metadata-bearing original
/// behind a derived file or a live-photo motion clip. Both cases are listed
/// because archives come off case-sensitive filesystems too.
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

/// Find the Takeout sidecar json describing `path`, if one exists. Candidates
/// are tried most specific first, so an exact sidecar wins over an inherited one.
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

/// Directory prefix (with trailing `/`, empty at the root) and file name. Paths
/// from [`FileSystem::walk`] always use `/`.
fn split_dir(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..=i], &path[i + 1..]),
        None => ("", path),
    }
}

/// Stem and extension, the extension keeping its dot. A leading dot belongs to
/// the stem, so a `.hidden` file is all stem.
fn split_ext(file_name: &str) -> (&str, &str) {
    match file_name.rfind('.') {
        Some(i) if i > 0 => (&file_name[..i], &file_name[i..]),
        _ => (file_name, ""),
    }
}

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
    supp_json_name_cut_at(base, counter, SUPP_STEM_MAX_CHARS)
}

fn supp_json_name_cut_at(base: &str, counter: &str, stem_chars: usize) -> String {
    let stem: String = format!("{base}{SUPP_TAG}")
        .chars()
        .take(stem_chars)
        .collect();
    format!("{stem}{counter}{JSON_EXT}")
}

/// Every filename the sidecar for `base` could have if the measured cap is
/// wrong, longest (least truncated) first.
///
/// Google only ever cuts into the tag, never into the filename it describes, so
/// the stem is some prefix of `{base}{SUPP_TAG}` keeping all of `base`.
/// Enumerating those covers a cap that moved either way, and as a side effect
/// covers Google shortening the tag itself.
fn supp_json_names_any_cap(base: &str, counter: &str) -> impl Iterator<Item = String> {
    let shortest = base.chars().count();
    let longest = shortest + SUPP_TAG.chars().count();
    (shortest..=longest)
        .rev()
        .map(move |n| supp_json_name_cut_at(base, counter, n))
}

/// Whether the cap is involved at all. When it is not, [`supp_json_name`]
/// already returns the untruncated name and the sweep is skipped — which is what
/// keeps its cost off ordinary filenames.
fn cap_applies(base: &str) -> bool {
    base.chars().count() + SUPP_TAG.chars().count() > SUPP_STEM_MAX_CHARS
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

/// The `(name, counter)` pairs a sidecar for `file_name` could be keyed on, most
/// specific first. These encode *which file* the metadata belongs to; turning a
/// pair into a filename is [`supp_json_name`]'s job.
///
/// Google names a sidecar `{media file}.supplemental-metadata.json`, but only
/// when that fits and only for files the user actually uploaded. Three rules
/// reshape the key, and they compose (truncation is [`supp_json_name`]'s job):
///
/// 1. **Duplicate counters** move off the media name onto the end of the json
///    name, *after* truncation — which is why the result can exceed the cap:
///    `FullSizeRender(42).jpg` -> `FullSizeRender.jpg.supplemental-metadata(42).json`.
/// 2. **Extension-less titles.** An upload with no extension keys its json on
///    that bare title even though the exported media file gained one:
///    `PicasaSync(6).jpg` -> `PicasaSync.supplemental-metadata(6).json`.
/// 3. **Derived files.** Renditions Google made itself carry no sidecar; the
///    metadata stays on the original, which may have a different extension (an
///    `-ANIMATION.gif` comes from a `.jpg`). Live-photo motion clips work the
///    same way: `IMG_3716.MP4`'s metadata lives on `IMG_3716.HEIC`.
fn supplemental_keys(file_name: &str) -> Vec<(String, String)> {
    let (stem, ext) = split_ext(file_name);
    let (base, counter) = split_counter(stem);

    let mut keys: Vec<(String, String)> = Vec::new();
    let mut push = |name: String, counter: &str| {
        let key = (name, counter.to_string());
        if !keys.contains(&key) {
            keys.push(key);
        }
    };

    // The sidecar named after this exact file.
    push(file_name.to_string(), "");
    // Rule 1.
    if !counter.is_empty() {
        push(format!("{base}{ext}"), counter);
    }
    // Rule 2, counter either way round.
    push(stem.to_string(), "");
    if !counter.is_empty() {
        push(base.to_string(), counter);
    }

    // Rule 3, peeled repeatedly since renditions stack (`-ERASER-edited`).
    let mut original = base;
    for _ in 0..MAX_DERIVED_DEPTH {
        let Some(stripped) = strip_derived_suffix(original) else {
            break;
        };
        original = stripped;
        for candidate_ext in std::iter::once(ext).chain(STILL_EXTS.iter().copied()) {
            let named = format!("{original}{candidate_ext}");
            if !counter.is_empty() {
                push(named.clone(), counter);
            }
            push(named, "");
        }
        if !counter.is_empty() {
            push(original.to_string(), counter);
        }
        push(original.to_string(), "");
    }

    // Rule 3, the live-photo case: metadata sits on the paired still.
    if is_motion_clip(ext) {
        for still_ext in STILL_EXTS {
            let named = format!("{base}{still_ext}");
            if !counter.is_empty() {
                push(named.clone(), counter);
            }
            push(named, "");
        }
    }

    keys
}

/// Every json filename that could describe `file_name`, most specific first.
///
/// Two passes. The first names each key at the measured cap, which is what
/// matches in practice — usually the first entry is the answer, one `exists`
/// call and done. The second re-tries at every other truncation point so an
/// archive built with a different cap still resolves rather than silently
/// falling back to EXIF; it is only reached when the first pass found nothing.
fn supplemental_candidates(file_name: &str) -> Vec<String> {
    let keys = supplemental_keys(file_name);
    let mut out: Vec<String> = Vec::with_capacity(keys.len());
    let push = |name: String, out: &mut Vec<String>| {
        if !out.contains(&name) {
            out.push(name);
        }
    };

    for (base, counter) in &keys {
        push(supp_json_name(base, counter), &mut out);
    }
    for (base, counter) in &keys {
        if !cap_applies(base) {
            continue;
        }
        for name in supp_json_names_any_cap(base, counter) {
            push(name, &mut out);
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
    /// Unix timestamp in seconds, as a string, e.g. `"1716539968"`.
    timestamp: Option<String>,
    pub(crate) formatted: Option<String>,
}

impl SupplementalInfoDateTime {
    /// Read unconditionally as seconds. Guessing the unit from the digit count
    /// instead breaks on photos predating 2001-09-09, whose timestamps are nine
    /// digits and would read as milliseconds — filing every older scan in 1970.
    ///
    /// The value is an absolute instant, so it renders in `tz` rather than UTC.
    /// See [`OutputTZ`].
    pub(crate) fn timestamp_s_as_iso_8601(&self, tz: OutputTZ) -> Option<String> {
        let seconds = self.timestamp.as_ref()?.trim().parse::<i64>().ok()?;
        tz.render_millis(seconds.checked_mul(1000)?)
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
    use crate::test_util::tz;
    use std::fs::File;

    /// Lay out `files` as empty files in a temp dir, then resolve the sidecar for
    /// `media`.
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

    /// Which sidecar a media file resolves to. Compared case-insensitively:
    /// both spellings of an original's extension are tried, and on a
    /// case-insensitive filesystem whichever comes first matches — either
    /// resolves to the same file.
    #[test]
    fn test_detect_supplemental_info() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        // (media file, the json files sitting beside it, the one it resolves to)
        let cases: Vec<(&str, Vec<&str>, Option<&str>)> = vec![
            (
                "IMG_0001.jpg",
                vec!["IMG_0001.jpg.supplemental-metadata.json"],
                Some("IMG_0001.jpg.supplemental-metadata.json"),
            ),
            ("IMG_0001.jpg", vec!["unrelated.json"], None),
            // Takeout caps the filename length, so the tag is cut at a different
            // point for each of these.
            (
                "IMG_20140913_100655_nopm_.jpg",
                vec!["IMG_20140913_100655_nopm_.jpg.supplemental-met.json"],
                Some("IMG_20140913_100655_nopm_.jpg.supplemental-met.json"),
            ),
            (
                "9C19C4BF-E0C8-4D74-8DC4-4BB2338FB029.JPG",
                vec!["9C19C4BF-E0C8-4D74-8DC4-4BB2338FB029.JPG.suppl.json"],
                Some("9C19C4BF-E0C8-4D74-8DC4-4BB2338FB029.JPG.suppl.json"),
            ),
            (
                "IMAGE_184EC254-639C-450E-9B14-7EE4377AE094.MOV",
                vec!["IMAGE_184EC254-639C-450E-9B14-7EE4377AE094.MOV.json"],
                Some("IMAGE_184EC254-639C-450E-9B14-7EE4377AE094.MOV.json"),
            ),
            (
                "2019 school photo wgc soraya.JPG",
                vec!["2019 school photo wgc soraya.JPG.supplemental-.json"],
                Some("2019 school photo wgc soraya.JPG.supplemental-.json"),
            ),
            // Truncation counts characters, not bytes: the narrow no-break space
            // here is three bytes but one character.
            (
                "Screenshot 2025-03-03 at 6.01.26\u{202f}PM.png",
                vec!["Screenshot 2025-03-03 at 6.01.26\u{202f}PM.png.supple.json"],
                Some("Screenshot 2025-03-03 at 6.01.26\u{202f}PM.png.supple.json"),
            ),
            // The counter is appended after truncation, so it moves onto the
            // json name and the result can exceed the cap.
            (
                "FullSizeRender(42).jpg",
                vec!["FullSizeRender.jpg.supplemental-metadata(42).json"],
                Some("FullSizeRender.jpg.supplemental-metadata(42).json"),
            ),
            (
                "IMAGE_D0209A83-AEF5-4D12-A17A-6981B7EDD66C.MOV(1).jpg",
                vec!["IMAGE_D0209A83-AEF5-4D12-A17A-6981B7EDD66C.MOV(1).json"],
                Some("IMAGE_D0209A83-AEF5-4D12-A17A-6981B7EDD66C.MOV(1).json"),
            ),
            // A title that carried no extension of its own.
            (
                "PicasaSync(6).jpg",
                vec!["PicasaSync.supplemental-metadata(6).json"],
                Some("PicasaSync.supplemental-metadata(6).json"),
            ),
            (
                "11 8_30_46 AM.jpg",
                vec!["11 8_30_46 AM.supplemental-metadata.json"],
                Some("11 8_30_46 AM.supplemental-metadata.json"),
            ),
            // A rendition inherits the original's sidecar, including when the
            // extension changes or it is two renditions deep.
            (
                "IMG_1189-edited.JPG",
                vec!["IMG_1189.JPG.supplemental-metadata.json"],
                Some("IMG_1189.JPG.supplemental-metadata.json"),
            ),
            (
                "IMG_20141017_180800-edited(1).jpg",
                vec!["IMG_20141017_180800.jpg.supplemental-metadata(1).json"],
                Some("IMG_20141017_180800.jpg.supplemental-metadata(1).json"),
            ),
            (
                "IMG_20140905_093118-SMILE.jpg",
                vec!["IMG_20140905_093118.jpg.supplemental-metadata.json"],
                Some("IMG_20140905_093118.jpg.supplemental-metadata.json"),
            ),
            (
                "FullSizeRender-ANIMATION.gif",
                vec!["FullSizeRender.jpg.supplemental-metadata.json"],
                Some("FullSizeRender.jpg.supplemental-metadata.json"),
            ),
            (
                "IMG_20140712_105906-ERASER-edited.jpg",
                vec!["IMG_20140712_105906.jpg.supplemental-metadata.json"],
                Some("IMG_20140712_105906.jpg.supplemental-metadata.json"),
            ),
            // A live photo's motion clip inherits the still's sidecar.
            (
                "IMG_3716.MP4",
                vec!["IMG_3716.HEIC.supplemental-metadata.json"],
                Some("IMG_3716.HEIC.supplemental-metadata.json"),
            ),
            // With both present, the rendition's own sidecar wins.
            (
                "IMG_1189-edited.JPG",
                vec![
                    "IMG_1189-edited.JPG.supplemental-metadata.json",
                    "IMG_1189.JPG.supplemental-metadata.json",
                ],
                Some("IMG_1189-edited.JPG.supplemental-metadata.json"),
            ),
        ];

        for (media, files, expected) in cases {
            let found = detect_in(&files, media)?;
            assert_eq!(
                found.as_deref().map(|f| f.to_ascii_lowercase()).as_deref(),
                expected.map(|e| e.to_ascii_lowercase()).as_deref(),
                "sidecar for {media}, got {found:?}"
            );
        }
        Ok(())
    }

    /// If the cap moves, sidecars must still resolve — otherwise every long-named
    /// photo quietly loses its date and falls back to EXIF.
    #[test]
    fn test_detect_survives_a_changed_length_cap() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let media = "9C19C4BF-E0C8-4D74-8DC4-4BB2338FB029.JPG";
        // The cap we measured cuts this one at `.suppl`.
        assert_eq!(
            supp_json_name(media, ""),
            "9C19C4BF-E0C8-4D74-8DC4-4BB2338FB029.JPG.suppl.json"
        );

        // A longer cap (less truncation), a shorter one (more), and no cap at all.
        for json in [
            "9C19C4BF-E0C8-4D74-8DC4-4BB2338FB029.JPG.supplemental-me.json",
            "9C19C4BF-E0C8-4D74-8DC4-4BB2338FB029.JPG.sup.json",
            "9C19C4BF-E0C8-4D74-8DC4-4BB2338FB029.JPG.supplemental-metadata.json",
            "9C19C4BF-E0C8-4D74-8DC4-4BB2338FB029.JPG.json",
        ] {
            assert_eq!(
                detect_in(&[json], media)?.as_deref(),
                Some(json),
                "sidecar at a different cap: {json}"
            );
        }
        Ok(())
    }

    /// An archive of ordinary filenames must not pay for the sweep.
    #[test]
    fn test_short_names_generate_no_fallback_candidates() {
        let short = supplemental_candidates("IMG_0001.jpg");
        assert_eq!(
            short,
            supplemental_keys("IMG_0001.jpg")
                .iter()
                .map(|(b, c)| supp_json_name(b, c))
                .collect::<Vec<_>>(),
            "a short name yields only the one-per-key first pass"
        );
        assert!(
            !cap_applies("IMG_0001.jpg"),
            "the cap is not in play for this name"
        );

        // A long name does get the sweep, bounded at one entry per truncation
        // point of the tag, per key.
        let long = supplemental_candidates("9C19C4BF-E0C8-4D74-8DC4-4BB2338FB029.JPG");
        assert!(long.len() > short.len());
        assert!(
            long.len()
                <= supplemental_keys("9C19C4BF-E0C8-4D74-8DC4-4BB2338FB029.JPG").len()
                    * (SUPP_TAG.chars().count() + 1),
            "fallback stays bounded by tag length, got {}",
            long.len()
        );
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

    #[test]
    fn test_parse_supp() -> anyhow::Result<()> {
        use anyhow::anyhow;
        crate::test_util::setup_log();
        use std::path::Path;
        let file = Path::new("test/test1.jpeg.supplemental-metadata.json");
        let json_reader = File::open(file)?;
        let r = parse_supplemental_info(json_reader)
            .ok_or_else(|| anyhow!("Failed to parse supplemental info"))?;
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
        // The `formatted` sibling spells this instant in UTC; the reading comes
        // out in the output zone, a quarter past noon rather than midnight.
        assert_eq!(
            taken
                .timestamp_s_as_iso_8601(tz())
                .ok_or_else(|| anyhow!("Missing iso 8601"))?,
            "2024-05-22T12:17:51+12:00"
        );
        Ok(())
    }

    /// Sidecars are whatever the export wrote. Malformed json returns None and
    /// valid-but-odd json may return Some; the invariant is that neither panics.
    #[test]
    fn test_load_supplemental_info_never_panics_on_malformed() -> anyhow::Result<()> {
        use std::io::Write;
        crate::test_util::setup_log();
        let dir = tempfile::tempdir()?;
        let fs = OsFileSystem::new(&dir.path().to_string_lossy());

        // Deep enough to catch a naive recursive parser, but shaped so serde
        // skips it as an unknown field rather than accepting it.
        let depth = 500;
        let nested = format!("{{\"unknown\":{}{}}}", "[".repeat(depth), "]".repeat(depth));
        let cases: Vec<(&str, &str)> = vec![
            ("empty.json", ""),
            ("garbage.json", "\u{0}\u{1}\u{2}not json at all"),
            ("truncated.json", "{\"geoData\": {\"latitude\": 1.0"),
            ("array.json", "[1, 2, 3]"),
            ("scalar.json", "42"),
            ("null.json", "null"),
            // Right shape, wrong value types for every field.
            (
                "wrong_types.json",
                r#"{"geoData":"nope","people":"nobody","photoTakenTime":5}"#,
            ),
            // geoData present but lat/long are strings, not numbers.
            (
                "bad_geo.json",
                r#"{"geoData":{"latitude":"north","longitude":"west"}}"#,
            ),
            ("nested.json", &nested),
            // Unicode everywhere, plus a non-numeric timestamp.
            (
                "unicode.json",
                r#"{"people":[{"name":"Ñoño 📸"}],"photoTakenTime":{"timestamp":"not-a-number"}}"#,
            ),
        ];
        for (name, body) in cases {
            std::fs::write(dir.path().join(name), body)?;
            let _ = load_supplemental_info(&name.to_string(), &fs);
        }

        // Enormous but structurally valid: a huge person list.
        let mut f = std::fs::File::create(dir.path().join("huge.json"))?;
        f.write_all(br#"{"people":["#)?;
        for i in 0..5000 {
            if i > 0 {
                f.write_all(b",")?;
            }
            f.write_all(br#"{"name":"p"}"#)?;
        }
        f.write_all(b"]}")?;
        drop(f);
        let _ = load_supplemental_info(&"huge.json".to_string(), &fs);
        Ok(())
    }

    /// Takeout timestamps are seconds whatever their digit count.
    #[test]
    fn test_timestamp_read_as_seconds_at_every_width() {
        let at = |ts: &str| {
            SupplementalInfoDateTime {
                timestamp: Some(ts.to_string()),
                formatted: None,
            }
            .timestamp_s_as_iso_8601(tz())
        };
        // Ten digits, the common modern case.
        assert_eq!(
            at("1716337071").as_deref(),
            Some("2024-05-22T12:17:51+12:00")
        );
        // Nine digits: a scanned photo from 1990, not 1970.
        assert_eq!(
            at("631152000").as_deref(),
            Some("1990-01-01T12:00:00+12:00")
        );
        // Older than the epoch.
        assert_eq!(
            at("-31536000").as_deref(),
            Some("1969-01-01T12:00:00+12:00")
        );
        assert_eq!(
            at(" 631152000 ").as_deref(),
            Some("1990-01-01T12:00:00+12:00")
        );
        assert_eq!(at(""), None);
        assert_eq!(at("not a timestamp"), None);
        // Overflows rather than wrapping into a plausible-looking date.
        assert_eq!(at("9223372036854775807"), None);
    }
}
