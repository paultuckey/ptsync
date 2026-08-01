//! Embedded metadata from a video's track, the counterpart to
//! [`crate::metadata::exif`] for files that have no EXIF.
//!
//! `nom_exif` supplies nine well-known fields and nothing else: it reads the
//! whole of `moov/meta` and then discards every key it doesn't recognise, and
//! it parses `tkhd` without keeping the display matrix. Both of those matter
//! here - the discarded keys include a live photo's pairing id, and the matrix
//! is the only thing saying a clip plays rotated - so [`super::isobmff`] reads
//! them back off the same file and this module merges the two.

use super::isobmff::parse_mov_extras;
use super::taken::TakenAt;
use chrono::DateTime;
use nom_exif::{MediaKind, MediaParser, MediaSource, TrackInfo, TrackInfoTag};
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use tracing::{debug, warn};

/// Apple's key for the uuid shared by both halves of a live photo.
const CONTENT_IDENTIFIER_KEY: &str = "com.apple.quicktime.content.identifier";

/// Apple's capture-time key, and the only date in a QuickTime file that records
/// the zone it was read in. See [`PsTrackInfo::taken_at`].
const CREATION_DATE_KEY: &str = "com.apple.quicktime.creationdate";

#[derive(Serialize, Debug, Clone, Default)]
#[serde(rename_all(serialize = "camelCase"))]
pub(crate) struct PsTrackInfo {
    pub width: Option<u64>,
    pub height: Option<u64>,
    // rfc3339
    pub creation_time: Option<String>,
    pub duration_ms: Option<u64>,
    pub make: Option<String>,
    pub model: Option<String>,
    pub software: Option<String>,
    pub author: Option<String>,
    /// The raw ISO 6709 string as the file stores it; `latitude` and
    /// `longitude` are the same fix in decimal degrees.
    pub gps_iso_6709: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    /// Apple's `ContentIdentifier` - the uuid that also appears in the still's
    /// maker note, and the only real link between the two halves of a live
    /// photo. See [`crate::live_photo`].
    pub content_identifier: Option<String>,
    /// Every `moov/meta` key the file carries, under the full name Apple writes
    /// them under. The equivalent of [`crate::metadata::exif::PsExifInfo::tags`]:
    /// open-ended, so a key nothing yet reads is still recorded.
    pub tags: BTreeMap<String, String>,
    /// Clockwise rotation the track plays at, in degrees, and whether it is
    /// mirrored first. See [`PsTrackInfo::display_transform`].
    pub rotation: Option<i32>,
    pub mirrored: Option<bool>,
}

impl PsTrackInfo {
    /// The display transform from the track's `tkhd` matrix, as
    /// `(mirrored, rotate)` - a horizontal flip before a clockwise rotation of
    /// `rotate` degrees. The video counterpart of
    /// [`crate::metadata::exif::exif_display_transform`], and returned in the same
    /// shape so callers can treat images and videos alike.
    ///
    /// `width` and `height` stay as the file stores them, exactly as the EXIF
    /// side leaves an image's dimensions untransformed: a portrait clip is
    /// stored 1920x1440 and rotated on playback, and flattening that here would
    /// lose the distinction rather than record it.
    pub(crate) fn display_transform(&self) -> Option<(bool, i32)> {
        Some((self.mirrored?, self.rotation?))
    }

    /// The clip's capture time, and what the container knew about its zone.
    ///
    /// QuickTime holds a date in two places and they are not the same kind of
    /// thing:
    ///
    /// - `moov/meta`'s `com.apple.quicktime.creationdate`, which Apple devices
    ///   write with a real offset (`2019-02-12T15:27:12+08:00`). Both halves
    ///   known, so [`TakenAt::Zoned`].
    /// - `mvhd`'s creation time, seconds since 1904 and nothing else. The
    ///   ISO-BMFF spec says that is UTC, and phones honour it - but cameras
    ///   predating GPS overwhelmingly wrote *local* time into it, and many still
    ///   do. That is why ExifTool leaves the value alone unless you ask for
    ///   `QuickTimeUTC`.
    ///
    /// So `mvhd` is reported as a [`TakenAt::WallClock`]: a reading whose zone
    /// nothing recorded. Calling it an `Instant` would be asserting UTC, which
    /// would license a later pass to shift it - correct for an Android clip and
    /// an hours-long error for a Canon. `WallClock` claims only what is true,
    /// that these digits are the best reading we have and no offset came with
    /// them, and it files exactly where the value files today.
    ///
    /// The Keys atom is read here rather than taken from `creation_time`
    /// because that field is whichever of the two `nom_exif` preferred, and by
    /// the time it is a string the two are indistinguishable - which is the
    /// whole problem [`TakenAt`] exists to fix.
    ///
    /// It is parsed with `%+` rather than as RFC 3339 because Apple writes the
    /// offset without its colon - `2023-01-18T21:05:38+1300` - which the strict
    /// grammar rejects. `nom_exif` reads the same key the same lenient way.
    pub(crate) fn taken_at(&self) -> Option<TakenAt> {
        if let Some(raw) = self.tags.get(CREATION_DATE_KEY)
            && let Ok(dt) = DateTime::parse_from_str(raw, "%+")
        {
            return Some(TakenAt::Zoned(dt));
        }
        // `creation_time` is already normalised by `nom_exif`, so RFC 3339 is
        // the only spelling that reaches here.
        let dt = DateTime::parse_from_rfc3339(self.creation_time.as_deref()?).ok()?;
        Some(TakenAt::WallClock(dt.naive_local()))
    }
}

/// Parse a video's embedded metadata, or `None` if the file isn't a track.
///
/// The reader is borrowed rather than consumed because the file is read twice:
/// once by `nom_exif` for the fields it knows, and once by [`super::isobmff`]
/// for the ones it drops.
pub fn parse_track_info<R: Read + Seek>(reader: &mut R) -> anyhow::Result<Option<PsTrackInfo>> {
    reader.seek(SeekFrom::Start(0))?;
    let parsed = {
        let Ok(ms) = MediaSource::seekable(&mut *reader) else {
            // Every non-media file in an archive reaches this, so it is no more
            // notable than the same case in `parse_exif_info`.
            debug!("Could not create MediaSource");
            return Ok(None);
        };
        if ms.kind() != MediaKind::Track {
            return Ok(None);
        }
        let mut parser = MediaParser::new();
        let info: nom_exif::Result<TrackInfo> = parser.parse_track(ms);
        info
    };
    let info = match parsed {
        Ok(info) => info,
        // As in `parse_exif_info`: a file that simply carries no track metadata
        // is ordinary, and only reported as an error because that is how
        // `nom_exif` spells absence. A parse that failed on metadata which is
        // there is a real loss.
        Err(nom_exif::Error::TrackNotFound) => {
            debug!("No track metadata in this file");
            return Ok(None);
        }
        Err(e) => {
            warn!("Failed to parse track metadata: {e}");
            return Ok(None);
        }
    };

    let (lat, long) = info
        .gps_info()
        .and_then(|gps| {
            crate::util::non_zero_coords(gps.latitude_decimal(), gps.longitude_decimal())
        })
        .unzip();

    let mut ti = PsTrackInfo {
        width: parse_to_o_u64(&info.get(TrackInfoTag::Width)),
        height: parse_to_o_u64(&info.get(TrackInfoTag::Height)),
        creation_time: parse_to_o_s(&info.get(TrackInfoTag::CreateDate)),
        duration_ms: parse_to_o_u64(&info.get(TrackInfoTag::DurationMs)),
        make: parse_to_o_s(&info.get(TrackInfoTag::Make)),
        model: parse_to_o_s(&info.get(TrackInfoTag::Model)),
        software: parse_to_o_s(&info.get(TrackInfoTag::Software)),
        author: parse_to_o_s(&info.get(TrackInfoTag::Author)),
        gps_iso_6709: parse_to_o_s(&info.get(TrackInfoTag::GpsIso6709)),
        latitude: lat,
        longitude: long,
        ..Default::default()
    };

    if let Some(extras) = parse_mov_extras(reader) {
        ti.content_identifier = extras.keys.get(CONTENT_IDENTIFIER_KEY).cloned();
        ti.tags = extras.keys;
        if let Some((mirrored, rotation)) = extras.display_transform {
            ti.mirrored = Some(mirrored);
            ti.rotation = Some(rotation);
        }
    }
    Ok(Some(ti))
}

fn parse_to_o_u64(opt: &Option<&nom_exif::EntryValue>) -> Option<u64> {
    if let Some(v) = opt
        && let Ok(s) = v.to_string().parse::<u64>()
    {
        return Some(s);
    }
    None
}

fn parse_to_o_s(opt: &Option<&nom_exif::EntryValue>) -> Option<String> {
    let Some(v) = opt else {
        return None;
    };
    Some(v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{FileSystem, OsFileSystem};

    #[test]
    fn test_parse_track() -> anyhow::Result<()> {
        use anyhow::anyhow;
        crate::test_util::setup_log();
        let c = OsFileSystem::new("test");
        let mut reader = c.open("Hello.mp4")?;
        let meta =
            parse_track_info(&mut reader)?.ok_or_else(|| anyhow!("Failed to parse track info"))?;
        assert_eq!(meta.width, Some(854));
        assert_eq!(meta.height, Some(480));
        assert_eq!(meta.duration_ms, Some(5000));
        assert_eq!(
            meta.creation_time,
            Some("2024-04-18T11:24:26+00:00".to_string())
        );
        Ok(())
    }

    /// A plain video has no live photo id and no rotation, and must say so
    /// rather than inventing either.
    #[test]
    fn test_plain_video_has_no_identifier() -> anyhow::Result<()> {
        use anyhow::anyhow;
        crate::test_util::setup_log();
        let c = OsFileSystem::new("test");
        let mut reader = c.open("Hello.mov")?;
        let meta =
            parse_track_info(&mut reader)?.ok_or_else(|| anyhow!("Failed to parse track info"))?;
        assert_eq!(meta.content_identifier, None);
        assert!(
            !meta.tags.contains_key(CONTENT_IDENTIFIER_KEY),
            "got {:?}",
            meta.tags
        );
        Ok(())
    }

    /// The motion clip of the live photo in `test/livephoto`, which exercises
    /// the join this module exists to make: the nine fields `nom_exif` returns,
    /// merged with the keys and display matrix it discards.
    #[test]
    fn test_parse_live_photo_clip() -> anyhow::Result<()> {
        use anyhow::anyhow;
        crate::test_util::setup_log();
        let c = OsFileSystem::new("test/livephoto");
        let mut reader = c.open("IMG_3221.MP4")?;
        let meta =
            parse_track_info(&mut reader)?.ok_or_else(|| anyhow!("Failed to parse track info"))?;

        // From nom_exif.
        assert_eq!(meta.make.as_deref(), Some("Apple"));
        assert_eq!(meta.model.as_deref(), Some("iPhone 13"));
        assert_eq!(meta.width, Some(1920));
        assert_eq!(meta.height, Some(1440));
        assert_eq!(
            meta.creation_time.as_deref(),
            Some("2023-01-18T21:05:38+13:00")
        );

        // From `gps_info()`, which replaced a hand-rolled ISO 6709 parser: the
        // decimal pair and the raw string are two readings of one fix.
        assert_eq!(
            meta.gps_iso_6709.as_deref(),
            Some("-41.2818+174.7650+064.520/")
        );
        assert_eq!(meta.latitude, Some(-41.2818));
        assert_eq!(meta.longitude, Some(174.7650));

        // From `isobmff`, which is everything nom_exif drops on the floor.
        assert_eq!(
            meta.content_identifier.as_deref(),
            Some("E1F3ADCB-67D9-48E5-A716-25F90BB2B50B")
        );
        assert_eq!(meta.display_transform(), Some((false, 90)));
        assert_eq!(
            meta.tags.get("com.apple.quicktime.live-photo.auto"),
            Some(&"1".to_string())
        );
        Ok(())
    }

    /// The distinction `taken_at` exists to draw, on the two real fixtures.
    ///
    /// Both clips report a `creation_time` and the strings look alike, but one
    /// is Apple's Keys atom with a genuine `+13:00` and the other is `mvhd`,
    /// which recorded no zone at all. Same shape, different claim.
    #[test]
    fn test_taken_at_separates_apples_zoned_date_from_a_bare_mvhd() -> anyhow::Result<()> {
        use anyhow::anyhow;
        use chrono::DateTime;
        crate::test_util::setup_log();

        let c = OsFileSystem::new("test/livephoto");
        let apple = parse_track_info(&mut c.open("IMG_3221.MP4")?)?
            .ok_or_else(|| anyhow!("no track info"))?;
        assert_eq!(
            apple.taken_at(),
            Some(TakenAt::Zoned(DateTime::parse_from_rfc3339(
                "2023-01-18T21:05:38+13:00"
            )?))
        );

        let c = OsFileSystem::new("test");
        let plain =
            parse_track_info(&mut c.open("Hello.mp4")?)?.ok_or_else(|| anyhow!("no track info"))?;
        assert!(
            !plain.tags.contains_key(CREATION_DATE_KEY),
            "fixture must have no Keys date, else it proves nothing"
        );
        assert_eq!(
            plain.taken_at().map(|t| t.to_string()).as_deref(),
            Some("2024-04-18T11:24:26"),
            "an mvhd date claims no offset, so it must not print one"
        );
        // ...but it still files exactly where it filed before.
        assert_eq!(
            plain.taken_at().map(|t| t.to_rfc3339()).as_deref(),
            Some("2024-04-18T11:24:26+00:00")
        );
        Ok(())
    }

    /// A clip with no date anywhere has none to report.
    #[test]
    fn test_taken_at_needs_a_date() {
        assert_eq!(PsTrackInfo::default().taken_at(), None);
    }

    #[test]
    fn test_display_transform_needs_both_halves() {
        // Absent unless the matrix actually decoded, so "no rotation recorded"
        // is never reported as "rotation of zero".
        assert_eq!(PsTrackInfo::default().display_transform(), None);
        let rotated = PsTrackInfo {
            mirrored: Some(false),
            rotation: Some(90),
            ..Default::default()
        };
        assert_eq!(rotated.display_transform(), Some((false, 90)));
    }
}
