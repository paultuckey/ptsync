use crate::fs::FileSystem;
use crate::util::non_zero_coords;
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
    supp_json_name_cut_at(base, counter, SUPP_STEM_MAX_CHARS)
}

/// As [`supp_json_name`], but cutting the stem at an arbitrary length. Used to
/// sweep every truncation point when the measured cap turns out to be wrong.
fn supp_json_name_cut_at(base: &str, counter: &str, stem_chars: usize) -> String {
    let stem: String = format!("{base}{SUPP_TAG}")
        .chars()
        .take(stem_chars)
        .collect();
    format!("{stem}{counter}{JSON_EXT}")
}

/// Every filename the sidecar for `base` could have if Takeout's length cap is
/// not the one we measured, longest (least truncated) first.
///
/// The stem is always some prefix of `{base}{SUPP_TAG}` that keeps the whole of
/// `base` — Google only ever cuts into the tag, never into the filename it is
/// describing. Enumerating those prefixes covers a cap that moved in either
/// direction, and as a side effect covers Google shortening the tag itself to
/// any prefix of its current spelling.
fn supp_json_names_any_cap(base: &str, counter: &str) -> impl Iterator<Item = String> {
    let shortest = base.chars().count();
    let longest = shortest + SUPP_TAG.chars().count();
    (shortest..=longest)
        .rev()
        .map(move |n| supp_json_name_cut_at(base, counter, n))
}

/// Whether the cap is even involved for this name. When `{base}{SUPP_TAG}` fits
/// inside it, [`supp_json_name`] returns the untruncated name and its exact value
/// is irrelevant — so the sweep has nothing to add and is skipped, which is what
/// keeps the cost off the common case and off archives with no sidecars at all.
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

    // Rule 1: the sidecar named after this exact file.
    push(file_name.to_string(), "");
    // Rule 2: the duplicate counter relocated onto the json name.
    if !counter.is_empty() {
        push(format!("{base}{ext}"), counter);
    }
    // Rule 3: keyed on an extension-less upload title, counter either way round.
    push(stem.to_string(), "");
    if !counter.is_empty() {
        push(base.to_string(), counter);
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
                push(named.clone(), counter);
            }
            push(named, "");
        }
        if !counter.is_empty() {
            push(original.to_string(), counter);
        }
        push(original.to_string(), "");
    }

    // Rule 4b: a live-photo motion clip, whose metadata sits on the paired still.
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
/// Two passes. The first names each candidate at the length cap we have
/// measured, which is what matches in practice — for the overwhelming majority
/// of files the very first entry is the answer, one `exists` call and done.
/// The second re-tries every candidate at every other truncation point, so an
/// archive built with a different cap still resolves instead of silently
/// falling back to EXIF. It costs nothing on the common path: it only produces
/// entries for names long enough for the cap to bite, and it is only ever
/// reached for a file whose sidecar was not found at all.
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

/// Read the sidecar at `path`, which describes `media_file_path`.
///
/// The media file's own name is needed to make sense of the `title` field — see
/// [`PsSupplementalInfo::drop_file_name_title`].
pub(crate) fn load_supplemental_info(
    path: &str,
    media_file_path: &str,
    container: &dyn FileSystem,
) -> Option<PsSupplementalInfo> {
    let reader_r = container.open(path);
    let Ok(reader) = reader_r else {
        warn!("Could not read supplemental json file: {path}");
        return None;
    };
    debug!("  Loaded: {path}");
    let mut info = parse_supplemental_info(reader)?;
    info.drop_file_name_title(media_file_path);
    Some(info)
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

#[cfg(test)]
impl SupplementalInfoDateTime {
    /// Build one from the unix-seconds string Takeout writes, so tests in other
    /// modules can exercise date precedence without the `timestamp` field
    /// leaving this one.
    pub(crate) fn new_for_test(timestamp_s: &str) -> Self {
        Self {
            timestamp: Some(timestamp_s.to_string()),
            formatted: None,
        }
    }
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
// `Default` so a test can name the one or two fields it cares about and leave
// the rest alone, rather than every construction site having to be touched each
// time Takeout gains a field worth reading.
#[derive(Deserialize, Serialize, Debug, Clone, Default)]
#[serde(rename_all(deserialize = "camelCase", serialize = "camelCase"))]
pub(crate) struct PsSupplementalInfo {
    /// Google's `title`, which is the name of the file as it was uploaded and
    /// only rarely a title anyone wrote. Cleared when it is just that name — see
    /// [`PsSupplementalInfo::drop_file_name_title`] — so what survives here is
    /// something a person chose.
    pub(crate) title: Option<String>,
    /// The caption typed into Google Photos. Nothing else carries it: Takeout
    /// writes it here and not into the media file's own EXIF or XMP, so a
    /// description dropped on the floor here is one that cannot be recovered
    /// from the archive later.
    pub(crate) description: Option<String>,
    /// Google Photos' star. Written as `"favorited": true` and simply absent
    /// when the photo is not one, so absent and false mean the same thing.
    ///
    /// Nothing else in the archive carries it: XMP has no favourite property at
    /// all, so like the caption it is lost for good if it is not read here.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub(crate) favorited: bool,
    /// Whether the photo was archived - hidden from the main Google Photos grid
    /// but not deleted.
    ///
    /// Exports also express this by putting the file under an `Archive/`
    /// directory, which [`crate::classify`] recognises as
    /// [`KnownDir::GpArchive`](crate::classify::KnownDir::GpArchive); the key is
    /// read too because it costs nothing and is the only signal that survives a
    /// file being moved out of that directory.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub(crate) archived: bool,
    pub(crate) geo_data: Option<SupplementalInfoGeoData>,
    pub(crate) geo_data_exif: Option<SupplementalInfoGeoData>,
    #[serde(default)]
    pub(crate) people: Vec<SupplementalInfoPerson>,
    pub(crate) photo_taken_time: Option<SupplementalInfoDateTime>,
    pub(crate) creation_time: Option<SupplementalInfoDateTime>,
}

impl PsSupplementalInfo {
    /// Clear a `title` that merely repeats the name of the file it describes,
    /// and normalise both free-text fields.
    ///
    /// Takeout fills `title` with the original upload's filename for every photo,
    /// so taken at face value it would stamp a filename onto notes that already
    /// name the file twice over. Only a title that is *not* the filename was
    /// typed by someone, and only that is worth keeping.
    ///
    /// The names to compare against are exactly the ones a sidecar can be keyed
    /// on, which [`supplemental_keys`] already enumerates: that covers the
    /// extension-less upload of rule 3 (`PicasaSync` for `PicasaSync(6).jpg`)
    /// and the original behind a derived file in rule 4, whose title a rendition
    /// inherits along with the rest of the sidecar (`IMG_1189.JPG` for
    /// `IMG_1189-edited.JPG`).
    ///
    /// Takeout writes `""` rather than omitting a field it has nothing for, so
    /// blanks become `None` here and no consumer has to test for both.
    fn drop_file_name_title(&mut self, media_file_path: &str) {
        self.title = self.title.take().filter(|t| !t.trim().is_empty());
        self.description = self.description.take().filter(|d| !d.trim().is_empty());

        let (_, media_file_name) = split_dir(media_file_path);
        if let Some(title) = &self.title
            && title_is_file_name(title.trim(), media_file_name)
        {
            debug!("  Ignoring supplemental title {title:?}: it is the file's own name");
            self.title = None;
        }
    }
}

impl PsSupplementalInfo {
    /// Drop a `geoData`/`geoDataExif` block that locates nothing.
    ///
    /// Takeout writes `0, 0` rather than omitting the block when it has no fix,
    /// and a pair missing one half locates nothing either. Clearing the whole
    /// block (rather than leaving zeros, or a half-filled struct) means a
    /// `geo_data` that survives parsing is always a real position, so no
    /// consumer - resolver, database, or `info` report - has to know Google's
    /// convention. Same rule as [`crate::exif_util::parse_exif_info`] and
    /// [`crate::xmp::parse_xmp`].
    fn drop_zero_coords(&mut self) {
        for geo in [&mut self.geo_data, &mut self.geo_data_exif] {
            if geo
                .as_ref()
                .is_some_and(|g| non_zero_coords(g.latitude, g.longitude).is_none())
            {
                *geo = None;
            }
        }
    }
}

/// Deserialize a flag without letting an unexpected spelling cost the whole
/// sidecar.
///
/// Serde's own `bool` rejects `"true"` or `1`, and rejecting means
/// [`parse_supplemental_info`] discards the entire file — so one flag Google
/// decided to write differently would take a photo's date, location and caption
/// down with it. A flag is the least important thing in the file; anything not
/// recognisably true reads as false, which is also what an absent key means.
fn lenient_bool<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    Ok(match serde_json::Value::deserialize(deserializer) {
        Ok(serde_json::Value::Bool(b)) => b,
        Ok(serde_json::Value::String(s)) => s.eq_ignore_ascii_case("true") || s == "1",
        Ok(serde_json::Value::Number(n)) => n.as_f64().is_some_and(|n| n != 0.0),
        _ => false,
    })
}

/// Whether `title` is one of the file names this sidecar could have been keyed
/// on, rather than something written about the photo.
fn title_is_file_name(title: &str, media_file_name: &str) -> bool {
    supplemental_keys(media_file_name)
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case(title))
}

fn parse_supplemental_info<R: Read>(json_reader: R) -> Option<PsSupplementalInfo> {
    let gs_r: Result<PsSupplementalInfo, _> = serde_json::from_reader(json_reader);
    if let Ok(mut gs) = gs_r {
        gs.drop_zero_coords();
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

    /// The measured cap is a fact about Google's exporter today, not a rule they
    /// promised. If it moves, sidecars must still resolve — otherwise every
    /// long-named photo quietly loses its date and falls back to EXIF.
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

    /// The same sweep also absorbs Google shortening the tag itself, as long as
    /// the new spelling is a prefix of the current one.
    #[test]
    fn test_detect_survives_a_shortened_tag() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let media = "IMG_20140913_100655_nopm_.jpg";
        let json = "IMG_20140913_100655_nopm_.jpg.supplemental.json";
        assert_eq!(detect_in(&[json], media)?.as_deref(), Some(json));
        Ok(())
    }

    /// The sweep must stay off the common path. A name short enough that the cap
    /// never bites gets exactly one candidate per key — no fallback probing — so
    /// an archive of ordinary filenames, or one with no sidecars at all, does not
    /// pay for this robustness.
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

        // A long name does get the sweep, and it stays bounded: one entry per
        // truncation point of the tag, per key.
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

    /// Load `json` as the sidecar for `media`, the way `analyze_file` does.
    fn load_for(media: &str, json: &str) -> anyhow::Result<PsSupplementalInfo> {
        use anyhow::anyhow;
        let dir = tempfile::tempdir()?;
        let name = format!("{media}.supplemental-metadata.json");
        std::fs::write(dir.path().join(&name), json)?;
        let fs = OsFileSystem::new(&dir.path().to_string_lossy());
        load_supplemental_info(&name, media, &fs).ok_or_else(|| anyhow!("failed to parse"))
    }

    /// Takeout fills `geoData` with zeros for a photo it has no location for,
    /// so the block is dropped at the parse rather than passed on as a position.
    #[test]
    fn test_parse_supp_zero_coords_dropped() -> anyhow::Result<()> {
        use anyhow::anyhow;
        crate::test_util::setup_log();
        let info = load_for(
            "IMG_0001.jpg",
            r#"{"geoData":{"latitude":0.0,"longitude":0.0},
                "geoDataExif":{"latitude":51.5,"longitude":-0.125}}"#,
        )?;
        assert!(info.geo_data.is_none());
        // A real fix in the same sidecar is untouched.
        let exif_geo = info
            .geo_data_exif
            .ok_or_else(|| anyhow!("missing geoDataExif"))?;
        assert_eq!(exif_geo.latitude, Some(51.5));
        assert_eq!(exif_geo.longitude, Some(-0.125));

        // Half a pair locates nothing either, so it goes the same way.
        let info = load_for("IMG_0001.jpg", r#"{"geoData":{"latitude":51.5}}"#)?;
        assert!(info.geo_data.is_none());
        Ok(())
    }

    /// A caption typed into Google Photos is the one thing in a Takeout sidecar
    /// that exists nowhere else, so it has to survive the parse.
    #[test]
    fn test_parse_supp_description() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let info = load_for(
            "IMG_0001.jpg",
            r#"{"title":"Sunset","description":"Low tide at Porthcurno."}"#,
        )?;
        assert_eq!(info.description.as_deref(), Some("Low tide at Porthcurno."));
        assert_eq!(info.title.as_deref(), Some("Sunset"));

        // Takeout writes "" rather than omitting a field, and an empty string is
        // not a description.
        let info = load_for("IMG_0001.jpg", r#"{"description":"   "}"#)?;
        assert_eq!(info.description, None);
        Ok(())
    }

    /// The favourite star and the archived flag, neither of which any other
    /// source in an archive carries.
    #[test]
    fn test_parse_supp_flags() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let info = load_for("IMG_0001.jpg", r#"{"favorited":true,"archived":true}"#)?;
        assert!(info.favorited);
        assert!(info.archived);

        // Google omits both keys for the ordinary photo, which reads as false.
        let info = load_for("IMG_0001.jpg", r#"{"description":"Plain."}"#)?;
        assert!(!info.favorited);
        assert!(!info.archived);

        // A flag spelled some other way must cost only the flag. Rejecting it
        // would discard the whole sidecar, taking the date and caption with it.
        let info = load_for(
            "IMG_0001.jpg",
            r#"{"favorited":"true","archived":[],"description":"Plain."}"#,
        )?;
        assert!(info.favorited, "a stringly-typed true is still true");
        assert!(!info.archived, "an unreadable flag reads as false");
        assert_eq!(
            info.description.as_deref(),
            Some("Plain."),
            "the rest of the sidecar must survive an odd flag"
        );
        Ok(())
    }

    /// Google's `title` is the uploaded file's name for all but a handful of
    /// photos, and repeating that in a note is worse than having no title.
    #[test]
    fn test_parse_supp_ignores_a_file_name_title() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        for (media, title) in [
            // The file's own name, and the same name in another case.
            ("IMG_0001.jpg", "IMG_0001.jpg"),
            ("IMG_0001.jpg", "img_0001.JPG"),
            // Rule 3: an upload with no extension is keyed on the bare title.
            ("PicasaSync(6).jpg", "PicasaSync"),
            ("11 8_30_46 AM.jpg", "11 8_30_46 AM"),
            // Rule 4: a rendition inherits the original's sidecar, so it
            // inherits a title naming a file that is not even this one.
            ("IMG_1189-edited.JPG", "IMG_1189.JPG"),
            ("FullSizeRender-ANIMATION.gif", "FullSizeRender.jpg"),
        ] {
            let json = format!(r#"{{"title":"{title}"}}"#);
            let info = load_for(media, &json)?;
            assert_eq!(info.title, None, "{title:?} is a file name, not a title");
        }

        // A title someone actually typed stays, even one that mentions a file.
        let info = load_for("IMG_0001.jpg", r#"{"title":"IMG_0001 reshoot"}"#)?;
        assert_eq!(info.title.as_deref(), Some("IMG_0001 reshoot"));
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
