//! Pairing a live photo's still with its motion clip.
//!
//! A live photo reaches an archive as two ordinary files: the still someone
//! thinks of as "the photo", and a two-second clip beside it. Both exporters
//! name the halves alike - `IMG_3716.HEIC` with `IMG_3716.MP4` from Google
//! Takeout, `IMG_3716.HEIC` with `IMG_3716.MOV` from iCloud - so a shared stem
//! in a shared directory is what pairs them here.
//!
//! Apple also embeds a real identifier, and ptsync now reads it:
//! [`crate::metadata::track`] recovers `com.apple.quicktime.content.identifier`
//! from the clip and [`crate::metadata::exif`] the matching `ContentIdentifier`
//! from the still, both surfacing as `content_identifier`. (The readers beneath
//! them are private to `metadata`, which is why they are named by their parents
//! here.) Pairing still goes by the stem
//! because this runs on the directory scan, before any file has been opened -
//! and because the identifier is the half that goes missing: across a 643-file
//! iCloud export, 32 of 37 clips carried one but only 30 of their stills did,
//! the rest having been edited or re-encoded into losing the maker note. Keying
//! on it would want both, as a cross-check on the stem rather than a
//! replacement for it.
//!
//! [`crate::metadata::supplemental`] already treats the still as the metadata bearer
//! for the pair - Google writes one sidecar json and attaches it to the still -
//! and this module extends that to the archive: the still is the asset, and the
//! clip rides along without a note of its own.
//!
//! It rides along in its *name* too. [`crate::sync_cmd::derived_for_clip`] gives
//! the clip the still's resolved path and changes only the extension, so a pair
//! is one name with three of them (`1430-22417.heic`, `.mov`, `.md`). Letting
//! each half name itself pulled them apart: the still's EXIF is a local wall
//! clock and the clip's QuickTime time is UTC, which on a 690-file iCloud export
//! filed 11 of 32 live photos a day away from their own clip, and the still's
//! sub-second EXIF then separated even the ones that had agreed. The `motion`
//! key in the note stays the thing that records the pairing, because the shared
//! name is a preference the archive cannot always honour - an unrelated file may
//! already hold it - and because the clip's extension is not predictable from
//! the still's.

use crate::file_type::QuickFileType;
use crate::path::{split_dir, split_ext};
use crate::util::ScanInfo;
use std::collections::HashMap;
use tracing::debug;

/// Extensions a live photo's still is stored under, spelled the way
/// [`split_ext`] returns them - with the dot, so the comparison is
/// against exactly what it hands back.
const STILL_EXTS: &[&str] = &[".heic", ".heif", ".jpg", ".jpeg", ".png"];

/// Extensions its motion clip is stored under. Takeout renames Apple's `.MOV`
/// to `.MP4` without repacking it - the fixture in `test/livephoto` is still a
/// QuickTime container under the `.MP4` name - so both spellings turn up in the
/// same archive, and neither says anything reliable about the bytes.
const MOTION_EXTS: &[&str] = &[".mov", ".mp4", ".m4v"];

/// Which still each motion clip belongs to, keyed by the clip's scan path.
///
/// Pairs are only reported where a directory holds exactly one still and exactly
/// one clip under a given stem. Anything else - a `.jpg` and a `.heic` of the
/// same shot, or two clips - is ambiguous about which file is "the photo", and
/// guessing there would silently drop a note for whichever lost, so those are
/// left as independent media.
pub(crate) fn detect_live_photo_pairs(files: &[ScanInfo]) -> HashMap<String, String> {
    /// The media sharing one directory and stem, split into stills and clips.
    type Candidates<'a> = (Vec<&'a str>, Vec<&'a str>);

    let mut stills_and_clips: HashMap<(&str, &str), Candidates> = HashMap::new();
    for si in files
        .iter()
        .filter(|si| si.quick_file_type == QuickFileType::Media)
    {
        let (dir, name) = split_dir(&si.file_path);
        let (stem, ext) = split_ext(name);
        let entry = stills_and_clips.entry((dir, stem)).or_default();
        if has_ext(ext, STILL_EXTS) {
            entry.0.push(&si.file_path);
        } else if has_ext(ext, MOTION_EXTS) {
            entry.1.push(&si.file_path);
        }
    }

    let mut by_clip = HashMap::new();
    for ((_, stem), (stills, clips)) in stills_and_clips {
        match (stills.as_slice(), clips.as_slice()) {
            ([still], [clip]) => {
                debug!("Live photo: {clip} is the motion clip for {still}");
                by_clip.insert(clip.to_string(), still.to_string());
            }
            (stills, clips) if !stills.is_empty() && !clips.is_empty() => {
                debug!(
                    "Not pairing {stem}: {} stills and {} clips share the stem",
                    stills.len(),
                    clips.len()
                );
            }
            _ => {}
        }
    }
    by_clip
}

/// Whether `ext` is one of `known`, ignoring case: archives come off
/// case-preserving and case-sensitive filesystems alike, and Apple writes
/// `.HEIC`/`.MOV` where Google writes `.heic`/`.mp4`.
fn has_ext(ext: &str, known: &[&str]) -> bool {
    known.iter().any(|k| ext.eq_ignore_ascii_case(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(paths: &[&str]) -> Vec<ScanInfo> {
        paths
            .iter()
            .map(|p| ScanInfo::new(p.to_string(), None, None, 0))
            .collect()
    }

    /// A real Takeout live photo, scanned off disk: `test/livephoto` holds an
    /// iPhone 13 still, its clip re-wrapped as `.MP4`, and the sidecar json
    /// Google attaches to the still.
    #[test]
    fn test_pairs_a_real_takeout_live_photo() {
        crate::test_util::setup_log();
        let files = crate::util::scan_fs(&crate::fs::OsFileSystem::new("test/livephoto"));
        let pairs = detect_live_photo_pairs(&files);

        assert_eq!(
            pairs.get("IMG_3221.MP4").map(String::as_str),
            Some("IMG_3221.HEIC")
        );
        // Only the clip is a key, and the sidecar json - which shares the stem
        // but is not media - pairs nothing.
        assert_eq!(pairs.len(), 1, "only the clip is a key: {pairs:?}");
    }

    #[test]
    fn test_pairs_the_icloud_spelling() {
        crate::test_util::setup_log();
        // iCloud keeps Apple's `.MOV` where Takeout re-wraps it, and exports
        // some stills under a bare asset uuid rather than an `IMG_` name. No
        // fixture on hand is shaped that way, so the paths are named directly -
        // this is about the names alone, which is all this module reads.
        let files = scan(&[
            "icloud/35F8739B-30E0-4620-802C-0817AD7356F6.JPG",
            "icloud/35F8739B-30E0-4620-802C-0817AD7356F6.MOV",
        ]);
        let pairs = detect_live_photo_pairs(&files);
        assert_eq!(
            pairs
                .get("icloud/35F8739B-30E0-4620-802C-0817AD7356F6.MOV")
                .map(String::as_str),
            Some("icloud/35F8739B-30E0-4620-802C-0817AD7356F6.JPG")
        );
        assert_eq!(pairs.len(), 1, "only the clip is a key: {pairs:?}");
    }

    #[test]
    fn test_does_not_pair_across_directories() {
        crate::test_util::setup_log();
        // Same stem, different folders: unrelated files that happen to share a
        // camera's counter, which rolls over every 9999 shots.
        let files = scan(&["a/IMG_3716.HEIC", "b/IMG_3716.MP4"]);
        assert!(detect_live_photo_pairs(&files).is_empty());
    }

    #[test]
    fn test_does_not_pair_when_ambiguous() {
        crate::test_util::setup_log();
        // Two stills for one clip: which is "the photo" is a guess, so neither
        // is paired and both keep their own note.
        let files = scan(&["a/IMG_1.HEIC", "a/IMG_1.jpg", "a/IMG_1.MOV"]);
        assert!(detect_live_photo_pairs(&files).is_empty());
        // ...and likewise two clips for one still.
        let files = scan(&["a/IMG_1.HEIC", "a/IMG_1.MOV", "a/IMG_1.mp4"]);
        assert!(detect_live_photo_pairs(&files).is_empty());
    }

    #[test]
    fn test_ignores_unpaired_and_non_media() {
        crate::test_util::setup_log();
        let files = scan(&[
            "a/lonely.jpg",
            "a/clip-only.mp4",
            // A sidecar shares the stem but is not media, so it pairs nothing.
            "a/IMG_2.HEIC",
            "a/IMG_2.HEIC.supplemental-metadata.json",
        ]);
        assert!(detect_live_photo_pairs(&files).is_empty());
    }
}
