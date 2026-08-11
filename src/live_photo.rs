//! Pairing the two halves of a Live Photo.
//!
//! An iPhone stores a Live Photo as two ordinary files — a still and a short
//! video — linked only by an Apple *content identifier*, a UUID written into
//! both. Neither export format says anything about the pairing, and the file
//! names need not match, so the identifier is the only way to recognise it.
//!
//! ptsync treats the still as the item and the video as its sidecar: the video
//! takes the still's name, gets no note of its own, and is referenced from the
//! still's. See [`crate::sync_cmd`].
//!
//! Anything ambiguous is left alone rather than guessed at. A group with no
//! still, or no video, is not a pair; extra files sharing one identifier — an
//! edited copy, say — stay ordinary media with notes of their own. The rule is
//! that a file is only ever demoted to a sidecar when there is exactly one
//! obvious thing for it to hang off.

use crate::file_type::media_kind;
use crate::media::{MediaFileInfo, content_identifier};
use std::collections::HashMap;
use tracing::debug;

/// Which files are halves of the same Live Photo, keyed by content checksum
/// because that is what identifies a file once duplicates have been collapsed.
#[derive(Default)]
pub(crate) struct LivePhotos {
    /// Video checksum to the checksum of the still it belongs to.
    still_of_video: HashMap<String, String>,
    /// Still checksum to the checksum of its video.
    video_of_still: HashMap<String, String>,
}

impl LivePhotos {
    /// Group `media` by content identifier and pair up the halves.
    ///
    /// Each identifier's outcome is settled as the files stream past — the
    /// first still and the first video in `media` win — so the pairing follows
    /// the order given rather than the order files happened to be inspected in.
    pub(crate) fn build(media: &[&MediaFileInfo]) -> Self {
        let mut by_identifier: HashMap<String, Halves> = HashMap::new();
        for file in media {
            if let Some(identifier) = content_identifier(file) {
                by_identifier.entry(identifier).or_default().add(file);
            }
        }

        let mut pairs = LivePhotos::default();
        for (identifier, halves) in by_identifier {
            let (Some(still), Some(video)) = (halves.still, halves.video) else {
                debug!("Content identifier {identifier} has only one half, so it is not a pair");
                continue;
            };
            pairs.still_of_video.insert(video.clone(), still.clone());
            pairs.video_of_still.insert(still, video);
        }
        pairs
    }

    /// True for a video that will be written as its still's sidecar, which is
    /// what excludes it from being filed and noted in its own right.
    pub(crate) fn is_sidecar_video(&self, checksum: &str) -> bool {
        self.still_of_video.contains_key(checksum)
    }

    pub(crate) fn still_for_video(&self, video_checksum: &str) -> Option<&str> {
        self.still_of_video.get(video_checksum).map(String::as_str)
    }

    pub(crate) fn video_for_still(&self, still_checksum: &str) -> Option<&str> {
        self.video_of_still.get(still_checksum).map(String::as_str)
    }
}

/// The checksums of the first still and the first video seen for one content
/// identifier.
///
/// Only the first of each is kept: a second file of the same kind — an edited
/// copy, say — finds the slot taken and so stays ordinary media, with a name
/// and a note of its own.
#[derive(Default)]
struct Halves {
    still: Option<String>,
    video: Option<String>,
}

impl Halves {
    fn add(&mut self, file: &MediaFileInfo) {
        let half = match media_kind(&file.accurate_file_type) {
            Some("p") => &mut self.still,
            Some("v") => &mut self.video,
            _ => return,
        };
        half.get_or_insert_with(|| file.hash_info.long_checksum.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exif_util::PsExifInfo;
    use crate::file_type::AccurateFileType;
    use crate::track_util::PsTrackInfo;
    use crate::util::HashInfo;

    fn still(checksum: &str, identifier: Option<&str>) -> MediaFileInfo {
        let mut m = MediaFileInfo::new_for_test();
        m.accurate_file_type = AccurateFileType::Heic;
        m.hash_info = HashInfo {
            short_checksum: checksum.chars().take(7).collect(),
            long_checksum: checksum.to_string(),
        };
        m.exif_info = Some(PsExifInfo {
            tags: std::collections::HashMap::new(),
            gps: None,
            latitude: None,
            longitude: None,
            content_identifier: identifier.map(str::to_string),
        });
        m
    }

    fn video(checksum: &str, identifier: Option<&str>) -> MediaFileInfo {
        let mut m = still(checksum, None);
        m.accurate_file_type = AccurateFileType::Mov;
        m.exif_info = None;
        m.track_info = Some(PsTrackInfo {
            width: None,
            height: None,
            creation_time: None,
            duration_ms: None,
            make: None,
            model: None,
            software: None,
            author: None,
            gps_iso_6709: None,
            content_identifier: identifier.map(str::to_string),
        });
        m
    }

    fn build(media: &[MediaFileInfo]) -> LivePhotos {
        let refs: Vec<&MediaFileInfo> = media.iter().collect();
        LivePhotos::build(&refs)
    }

    #[test]
    fn test_pairs_a_still_with_its_video() {
        let media = vec![
            still("aaaa", Some("uuid-1")),
            video("bbbb", Some("uuid-1")),
            // A second Live Photo, and a plain photo that is not part of one.
            still("cccc", Some("uuid-2")),
            video("dddd", Some("uuid-2")),
            still("eeee", None),
        ];
        let pairs = build(&media);

        assert_eq!(pairs.still_for_video("bbbb"), Some("aaaa"));
        assert_eq!(pairs.video_for_still("aaaa"), Some("bbbb"));
        assert_eq!(pairs.still_for_video("dddd"), Some("cccc"));
        assert!(pairs.is_sidecar_video("bbbb"));
        // The still is never the sidecar, and neither is an unpaired file.
        assert!(!pairs.is_sidecar_video("aaaa"));
        assert!(!pairs.is_sidecar_video("eeee"));
        assert_eq!(pairs.video_for_still("eeee"), None);
    }

    /// A file is only demoted to a sidecar when there is one obvious still for
    /// it to hang off, so everything short of that stays ordinary media.
    #[test]
    fn test_half_a_pair_is_not_a_pair() {
        // A video whose still never made it into the export, and a still whose
        // video did not.
        let media = vec![
            video("bbbb", Some("uuid-1")),
            still("cccc", Some("uuid-2")),
            // Two files that carry no identifier at all must not be grouped
            // together by their shared absence of one.
            still("eeee", None),
            video("ffff", None),
        ];
        let pairs = build(&media);
        for checksum in ["bbbb", "cccc", "eeee", "ffff"] {
            assert!(!pairs.is_sidecar_video(checksum), "{checksum}");
            assert_eq!(pairs.video_for_still(checksum), None, "{checksum}");
        }
    }

    /// One identifier over three files: the first still and the first video
    /// pair up, and the leftover keeps its own name and note rather than being
    /// dropped.
    #[test]
    fn test_extra_files_sharing_an_identifier_stay_ordinary_media() {
        let media = vec![
            still("aaaa", Some("uuid-1")),
            still("bbbb", Some("uuid-1")),
            video("cccc", Some("uuid-1")),
            video("dddd", Some("uuid-1")),
        ];
        let pairs = build(&media);

        assert_eq!(pairs.video_for_still("aaaa"), Some("cccc"));
        assert!(pairs.is_sidecar_video("cccc"));
        assert!(!pairs.is_sidecar_video("dddd"));
        assert_eq!(pairs.video_for_still("bbbb"), None);
    }

    /// Which file wins must come from the order given, not from the hash map's
    /// iteration order, or a re-run could rename both halves.
    #[test]
    fn test_pairing_does_not_depend_on_map_iteration_order() {
        let media = vec![
            still("aaaa", Some("uuid-1")),
            still("bbbb", Some("uuid-1")),
            video("cccc", Some("uuid-1")),
        ];
        for _ in 0..8 {
            let pairs = build(&media);
            assert_eq!(pairs.still_for_video("cccc"), Some("aaaa"));
        }
    }
}
