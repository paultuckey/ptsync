use super::taken::TakenAt;
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime};
use nom_exif::{ExifIter, ExifIterEntry, ExifTag, MediaKind, MediaParser, MediaSource};
use serde::Serialize;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use tracing::{debug, warn};

/*

Util file to help with exif parsing.

it's not the responsibility of this module to decide if exif data is valid or not, just to
parse it best as possible.

store in db as json

 */

#[derive(Serialize, Debug, Clone, Default)]
#[serde(rename_all(serialize = "camelCase"))]
pub(crate) struct PsExifInfo {
    // dates as ISO 8601
    pub(crate) tags: HashMap<String, String>,
    // as iso6709
    pub(crate) gps: Option<String>,
    pub(crate) latitude: Option<f64>,
    pub(crate) longitude: Option<f64>,
    /// Apple's `ContentIdentifier`, read out of the maker note - the uuid that
    /// also appears in the motion clip's
    /// `com.apple.quicktime.content.identifier`. Not an EXIF tag, so it is kept
    /// beside `tags` rather than in it. See [`super::apple_makernote`].
    pub(crate) content_identifier: Option<String>,
}

pub(crate) fn parse_exif_info<R: Read + Seek>(mut reader: R) -> anyhow::Result<Option<PsExifInfo>> {
    reader.seek(SeekFrom::Start(0))?;
    let ms = MediaSource::seekable(reader);
    let Ok(ms) = ms else {
        debug!("Could not create MediaSource");
        return Ok(None);
    };
    if ms.kind() != MediaKind::Image {
        debug!("File does not mave exif metadata");
        return Ok(None);
    }
    let mut m = HashMap::new();
    let mut parser = MediaParser::new();
    let exif_iter_r: nom_exif::Result<ExifIter> = parser.parse_exif(ms);
    let mut ps_gps_info = None;
    let mut lat = None;
    let mut long = None;
    let mut content_identifier = None;
    match exif_iter_r {
        Ok(exif_iter) => {
            for entry in exif_iter.clone() {
                let Some(tag_enum) = entry.tag().tag() else {
                    continue; // skip unrecognised tags
                };
                if tag_enum == ExifTag::MakerNote {
                    // An opaque vendor blob rather than a value - `nom_exif`
                    // decodes no sub-tags - but Apple's holds the live photo
                    // pairing id, so it is read here and then left out of
                    // `tags`, where a hex dump would only be noise.
                    if let Ok(nom_exif::EntryValue::Undefined(blob)) = entry.clone().into_result() {
                        content_identifier = super::apple_makernote::content_identifier(&blob);
                    }
                    continue;
                }
                let tag_name = tag_enum.to_string();
                let s_o = field_to_opt_string(&entry);
                let Some(s) = s_o else {
                    continue; // only support tags with value
                };
                if s.len() > 1024 {
                    continue; // skip large values
                }
                m.insert(tag_name, s);
            }
            if let Some(gps_info) = exif_iter.parse_gps().ok().flatten()
                && let Some((la, lo)) = crate::util::non_zero_coords(
                    gps_info.latitude_decimal(),
                    gps_info.longitude_decimal(),
                )
            {
                lat = Some(la);
                long = Some(lo);
                ps_gps_info = Some(gps_info.to_iso6709());
            }
        }
        // An image with no EXIF at all - a screenshot, an export that stripped
        // it - is ordinary and reported as an error only because that is how
        // `nom_exif` spells absence. Anything else means the file *has* EXIF
        // that could not be read, which is a real loss worth hearing about.
        // `parse_track_info` draws the same line.
        Err(nom_exif::Error::ExifNotFound) => debug!("No EXIF data in this file"),
        Err(e) => warn!("Could not read EXIF data: {e}"),
    }
    Ok(Some(PsExifInfo {
        tags: m,
        gps: ps_gps_info,
        latitude: lat,
        longitude: long,
        content_identifier,
    }))
}

fn field_to_opt_string(field: &ExifIterEntry) -> Option<String> {
    if let Ok(value) = field.clone().into_result() {
        match value {
            nom_exif::EntryValue::Undefined(_) => {
                // skip undefined values
                return None;
            }
            _ => {
                // dates are returned as a ISO 8601 string with no timezone
                return Some(value.to_string());
            }
        }
    }
    None
}

fn field_value(exif: &PsExifInfo, code: ExifTag) -> Option<String> {
    exif.tags.get(&code.to_string()).cloned()
}

/// Camera manufacturer (EXIF `Make`), e.g. `Canon`.
pub(crate) fn camera_make(exif: &PsExifInfo) -> Option<String> {
    field_value(exif, ExifTag::Make)
}

/// Camera model (EXIF `Model`), e.g. `Canon EOS 40D`.
pub(crate) fn camera_model(exif: &PsExifInfo) -> Option<String> {
    field_value(exif, ExifTag::Model)
}

/// Image width in pixels. Prefers the Exif-IFD pixel dimension, falling back to
/// the IFD0 `ImageWidth`. `None` when neither is present or numeric.
pub(crate) fn image_width(exif: &PsExifInfo) -> Option<i64> {
    field_value(exif, ExifTag::ExifImageWidth)
        .or_else(|| field_value(exif, ExifTag::ImageWidth))
        .and_then(|s| s.trim().parse::<i64>().ok())
}

/// Image height in pixels. Prefers the Exif-IFD pixel dimension, falling back to
/// the IFD0 `ImageHeight`. `None` when neither is present or numeric.
pub(crate) fn image_height(exif: &PsExifInfo) -> Option<i64> {
    field_value(exif, ExifTag::ExifImageHeight)
        .or_else(|| field_value(exif, ExifTag::ImageHeight))
        .and_then(|s| s.trim().parse::<i64>().ok())
}

/// The display transform implied by the EXIF `Orientation` tag, decomposed into
/// `(mirrored, rotate)`: a horizontal flip applied *before* a clockwise rotation
/// of `rotate` degrees (one of `-90`, `0`, `90`, `180`). `None` when the tag is
/// absent (e.g. videos) — nothing to apply. Orientation `1` yields
/// `(false, 0)`. Distinct from the derived aspect orientation. The raw numeric
/// tag is still available in `media_info`.
pub(crate) fn exif_display_transform(exif: &PsExifInfo) -> Option<(bool, i32)> {
    let raw = field_value(exif, ExifTag::Orientation)?;
    Some(orientation_transform(raw.trim()))
}

/// Decompose the raw EXIF `Orientation` value (`1`–`8`) into `(mirrored, rotate)`,
/// where a horizontal mirror is applied before a clockwise `rotate` (degrees).
/// `1` and any unrecognised value mean "no transform".
fn orientation_transform(raw: &str) -> (bool, i32) {
    match raw {
        "2" => (true, 0),
        "3" => (false, 180),
        "4" => (true, 180),
        "5" => (true, -90), // mirror horizontal + rotate 270 CW
        "6" => (false, 90),
        "7" => (true, 90),   // mirror horizontal + rotate 90 CW
        "8" => (false, -90), // rotate 270 CW
        _ => (false, 0),
    }
}

/// Read an EXIF date/time, keeping the one thing the string itself tells us:
/// whether the camera recorded the zone it was read in.
///
/// EXIF has no single spelling. Depending on whether the file carries an
/// `OffsetTime` tag, `nom_exif` hands back either a full RFC 3339 instant
/// (`2023-08-05T19:59:55+12:00`) or a bare local wall-clock reading
/// (`2015-04-18 11:10:44`); the raw tag form separates the date with colons
/// (`2015:04:18 11:10:44`), and `GPSDateStamp` is a date with no time at all
/// (`2015:04:17`).
///
/// That fork is exactly [`TakenAt`]'s first two cases, and it is why this
/// returns one rather than a string: a bare reading normalised to `+00:00`
/// cannot afterwards be told apart from a camera that really was set to UTC, and
/// the two want opposite treatment when a later pass tries to place the photo.
///
/// The classification here is purely a matter of *spelling* - what the string
/// claims about itself. Which tag it came from is a separate question, and one
/// only [`best_guess_taken_exif`] is in a position to answer.
pub(crate) fn parse_exif_datetime(raw: &str) -> Option<TakenAt> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    // Already carries an offset: keep the recorded instant exactly as it stands.
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Some(TakenAt::Zoned(dt));
    }
    let normalised = dashed_date_separators(raw);
    for format in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(&normalised, format) {
            return Some(TakenAt::WallClock(naive));
        }
    }
    // `GPSDateStamp` pins down a day and nothing finer; midnight is the only
    // time it justifies.
    if let Ok(date) = NaiveDate::parse_from_str(&normalised, "%Y-%m-%d") {
        return Some(TakenAt::WallClock(date.and_time(NaiveTime::MIN)));
    }
    None
}

/// Rewrite EXIF's `YYYY:MM:DD` date separators as dashes so one set of chrono
/// patterns covers both spellings. Only touches a leading date that is already
/// all ASCII digits and colons, so byte offsets are safe here.
fn dashed_date_separators(s: &str) -> String {
    let b = s.as_bytes();
    let is_exif_date = b.len() >= 10
        && b[4] == b':'
        && b[7] == b':'
        && b[..10]
            .iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit());
    if !is_exif_date {
        return s.to_string();
    }
    let mut out = s.to_string();
    out.replace_range(4..5, "-");
    out.replace_range(7..8, "-");
    out
}

/// Best guess at when an image was taken, from EXIF alone, as RFC 3339.
///
/// Tags are tried in descending order of trust, and one that is present but
/// unparseable is skipped rather than returned — an unusable string here would
/// otherwise mask a perfectly good date in a later tag.
///
/// EXIF splits an instant across two tags: the date tag is second-resolution by
/// spec (`YYYY:MM:DD HH:MM:SS`, 20 ASCII bytes) and the fraction lives in a
/// separate `SubSec*` tag, paired here with the date tag it qualifies.
/// `GPSDateStamp` has no such partner — it is a date and nothing finer.
/// Dropping the fraction is what made every output name end in `000`, and it
/// is precisely the digits that tell one frame of a burst from the next.
///
/// `GPSDateStamp` is also the one EXIF date that is *not* a wall clock: it comes
/// off the GPS receiver, and GPS time is UTC by definition. It reads as a bare
/// date like any other, so [`parse_exif_datetime`] cannot know that from the
/// spelling and this function says so on its behalf.
pub(crate) fn best_guess_taken_exif(exif: &Option<PsExifInfo>) -> Option<TakenAt> {
    let exif = exif.as_ref()?;
    [
        (ExifTag::DateTimeOriginal, Some(ExifTag::SubSecTimeOriginal)),
        (ExifTag::ModifyDate, Some(ExifTag::SubSecTime)),
        (ExifTag::GPSDateStamp, None),
    ]
    .into_iter()
    .find_map(|(date_tag, subsec_tag)| {
        let dt = field_value(exif, date_tag)
            .as_deref()
            .and_then(parse_exif_datetime)?;
        let dt = if date_tag == ExifTag::GPSDateStamp {
            TakenAt::Instant(dt.instant())
        } else {
            dt
        };
        let millis = subsec_tag
            .and_then(|tag| field_value(exif, tag))
            .and_then(|raw| subsec_millis(&raw));
        // A date that parsed but will not take a fraction is still a good date:
        // keep the whole second rather than dropping to the next tag.
        Some(millis.and_then(|ms| dt.with_millis(ms)).unwrap_or(dt))
    })
}

/// Read a `SubSec*` tag as whole milliseconds.
///
/// The value is the digits *after* the decimal point, not a count — EXIF's `"7"`
/// means .7 s (700 ms), `"097"` means 97 ms. Anything past three digits is finer
/// than the output name records, so it is truncated rather than rounded: a
/// truncated fraction still orders the frames of a burst correctly, whereas
/// rounding can carry into the next second and reorder them.
///
/// Cameras with nothing to report write `"00"`, and some pad the field with
/// spaces or NULs; a value that is not plain digits is treated as absent.
fn subsec_millis(raw: &str) -> Option<u32> {
    let digits = raw.trim().trim_end_matches('\0').trim();
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut millis: u32 = 0;
    for i in 0..3 {
        let d = digits.as_bytes().get(i).map_or(0, |b| u32::from(b - b'0'));
        millis = millis * 10 + d;
    }
    Some(millis)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::FileSystem;
    use crate::fs::OsFileSystem;

    #[test]
    fn test_parse_exif_mp4() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let c = OsFileSystem::new("test");
        let reader = c.open("Hello.mp4")?;
        let t = parse_exif_info(reader)?;
        assert!(t.is_none());
        Ok(())
    }

    /// The still half of the live photo in `test/livephoto`, and the only
    /// end-to-end cover for [`super::apple_makernote`]: `nom_exif` surfaces the
    /// Apple block as an opaque `Undefined` blob, which this module hands over
    /// to be decoded and which nothing else in the crate can produce.
    ///
    /// Despite the `.HEIC` name the file is a JPEG - Takeout transcoded it and
    /// kept the original name - which is why the maker note is reachable here at
    /// all, and a reminder that the extension in an archive is a claim, not a
    /// fact.
    #[test]
    fn test_reads_apple_content_identifier_from_a_real_still() -> anyhow::Result<()> {
        use anyhow::anyhow;
        crate::test_util::setup_log();
        let c = OsFileSystem::new("test/livephoto");
        let info =
            parse_exif_info(c.open("IMG_3221.HEIC")?)?.ok_or_else(|| anyhow!("no exif parsed"))?;

        // The same uuid the clip beside it carries as
        // `com.apple.quicktime.content.identifier` - see `isobmff::tests`. This
        // pair is the whole reason either reader exists.
        assert_eq!(
            info.content_identifier.as_deref(),
            Some("E1F3ADCB-67D9-48E5-A716-25F90BB2B50B")
        );
        // The maker note itself stays out of `tags`: it is a vendor blob, not an
        // EXIF value, and a hex dump of it would only be noise.
        assert!(
            !info.tags.contains_key(&ExifTag::MakerNote.to_string()),
            "the raw maker note must not be reported as a tag"
        );
        // Shot in portrait, matching the 90 degrees the clip's tkhd matrix says.
        assert_eq!(exif_display_transform(&info), Some((false, 90)));
        Ok(())
    }

    /// A still with a maker note from another vendor must not yield an
    /// identifier - Canon's tag 0x0011 means something else entirely.
    #[test]
    fn test_non_apple_still_has_no_content_identifier() -> anyhow::Result<()> {
        use anyhow::anyhow;
        crate::test_util::setup_log();
        let c = OsFileSystem::new("test");
        let info =
            parse_exif_info(c.open("Canon_40D.jpg")?)?.ok_or_else(|| anyhow!("no exif parsed"))?;
        assert_eq!(info.content_identifier, None);
        Ok(())
    }

    #[test]
    fn test_parse_exif_all_tags() -> anyhow::Result<()> {
        use anyhow::anyhow;
        crate::test_util::setup_log();
        let c = OsFileSystem::new("test");
        let reader = c.open("Canon_40D.jpg")?;
        let t = parse_exif_info(reader)?
            .ok_or_else(|| anyhow!("Failed to parse exif"))?
            .tags;
        assert_eq!(t.len(), 41);
        let mut tag_names: Vec<String> = t.keys().map(|t| t.to_string()).collect();
        tag_names.sort();

        let mut expected_tags = vec![
            "ApertureValue",
            "ColorSpace",
            "Compression",
            "CreateDate",
            "CustomRendered",
            "DateTimeOriginal",
            "ExifImageHeight",
            "ExifImageWidth",
            "ExifOffset",
            "ExposureBiasValue",
            "ExposureMode",
            "ExposureProgram",
            "ExposureTime",
            "FNumber",
            "Flash",
            "FocalLength",
            "FocalPlaneResolutionUnit",
            "FocalPlaneXResolution",
            "FocalPlaneYResolution",
            "GPSInfo",
            "GPSVersionID",
            "ISOSpeedRatings",
            "InteropOffset",
            "Make",
            "MeteringMode",
            "Model",
            "ModifyDate",
            "Orientation",
            "ResolutionUnit",
            "SceneCaptureType",
            "ShutterSpeedValue",
            "Software",
            "SubSecTime",
            "SubSecTimeDigitized",
            "SubSecTimeOriginal",
            "ThumbnailLength",
            "ThumbnailOffset",
            "WhiteBalanceMode",
            "XResolution",
            "YCbCrPositioning",
            "YResolution",
        ];
        expected_tags.sort();

        assert_eq!(tag_names, expected_tags);

        let make_tag_value = t
            .get(&ExifTag::Make.to_string())
            .ok_or_else(|| anyhow!("Make tag not found"))?;
        assert_eq!(make_tag_value, &"Canon".to_string());

        let sub_sec_time_original = t
            .get(&ExifTag::SubSecTimeOriginal.to_string())
            .ok_or_else(|| anyhow!("SubSecTimeOriginal tag not found"))?;
        assert_eq!(sub_sec_time_original.clone(), "00".to_string());
        Ok(())
    }

    #[test]
    fn test_camera_and_dimensions_accessors() -> anyhow::Result<()> {
        use anyhow::anyhow;
        crate::test_util::setup_log();
        let c = OsFileSystem::new("test");
        let reader = c.open("Canon_40D.jpg")?;
        let info = parse_exif_info(reader)?.ok_or_else(|| anyhow!("Failed to parse exif"))?;

        assert_eq!(camera_make(&info).as_deref(), Some("Canon"));
        assert_eq!(camera_model(&info).as_deref(), Some("Canon EOS 40D"));
        assert!(image_width(&info).is_some_and(|w| w > 0), "width parsed");
        assert!(image_height(&info).is_some_and(|h| h > 0), "height parsed");
        // EXIF Orientation "1" decodes to the no-op transform.
        assert_eq!(exif_display_transform(&info), Some((false, 0)));
        Ok(())
    }

    /// Every spelling of an EXIF date this codebase has seen must be read, and
    /// read into the right kind: a tag that carried an offset keeps it, and one
    /// that did not must not be handed a `+00:00` it never claimed. The two
    /// print differently, which is what the expectations below are checking.
    #[test]
    fn test_parse_exif_datetime_keeps_only_the_offsets_that_were_there() {
        for (raw, expected) in [
            // An `OffsetTime` tag was present: the offset is real, and kept.
            ("2023-08-05T19:59:55+12:00", "2023-08-05T19:59:55+12:00"),
            ("2023-08-05T07:59:55Z", "2023-08-05T07:59:55+00:00"),
            // Bare wall-clock reading, the common case: no offset invented.
            ("2015-04-18 11:10:44", "2015-04-18T11:10:44"),
            ("2015-04-18T11:10:44", "2015-04-18T11:10:44"),
            // Raw EXIF colon-separated date.
            ("2015:04:18 11:10:44", "2015-04-18T11:10:44"),
            // Sub-second precision.
            ("2015-04-18 11:10:44.939", "2015-04-18T11:10:44.939"),
            // GPSDateStamp: a day and nothing finer. Reads as a bare wall clock
            // here; only `best_guess_taken_exif` knows the tag is UTC.
            ("2015:04:17", "2015-04-17T00:00:00"),
            ("2015-04-17", "2015-04-17T00:00:00"),
            // Surrounding whitespace, as EXIF strings are often space-padded.
            ("  2015:04:18 11:10:44 ", "2015-04-18T11:10:44"),
        ] {
            assert_eq!(
                parse_exif_datetime(raw).map(|t| t.to_string()).as_deref(),
                Some(expected),
                "reading {raw:?}"
            );
        }
    }

    /// Whatever kind it turns out to be, it has to survive the RFC 3339 round
    /// trip `get_desired_media_path` puts it through - anything it cannot parse
    /// lands the file in `undated/`.
    #[test]
    fn test_parse_exif_datetime_always_yields_a_filable_date() {
        for (raw, expected) in [
            ("2023-08-05T19:59:55+12:00", "2023-08-05T19:59:55+12:00"),
            ("2015:04:18 11:10:44", "2015-04-18T11:10:44+00:00"),
            ("2015-04-18 11:10:44.939", "2015-04-18T11:10:44.939+00:00"),
            ("2015:04:17", "2015-04-17T00:00:00+00:00"),
        ] {
            assert_eq!(
                parse_exif_datetime(raw).map(|t| t.to_rfc3339()).as_deref(),
                Some(expected),
                "converting {raw:?}"
            );
        }
    }

    #[test]
    fn test_exif_datetime_rejects_unusable() {
        // Cameras with a dead clock write zeroes; a blank or nonsense tag is no
        // better. None of these is a date, and returning one would date the photo
        // to the year 0 rather than falling through to the next source.
        for raw in [
            "",
            "   ",
            "0000:00:00 00:00:00",
            "2015:13:45 99:99:99",
            "not a date",
            "2015",
        ] {
            assert_eq!(parse_exif_datetime(raw), None, "rejecting {raw:?}");
        }
    }

    /// `SubSec*` holds the digits after the decimal point, so its length is
    /// significant: `"7"` is 700 ms, not 7.
    #[test]
    fn test_subsec_millis() {
        for (raw, expected) in [
            ("489", Some(489)),
            ("097", Some(97)),
            ("7", Some(700)),
            ("75", Some(750)),
            // A camera with nothing to report; 0 ms is a real answer, and
            // grafting it on is the no-op it should be.
            ("00", Some(0)),
            ("0", Some(0)),
            // Finer than the name records: truncated, never rounded, so a burst
            // cannot be reordered by a fraction carrying into the next second.
            ("4999", Some(499)),
            ("999999", Some(999)),
            // Space- and NUL-padded fields, as EXIF strings often are.
            (" 489 ", Some(489)),
            ("489\0", Some(489)),
            // Not a fraction at all.
            ("", None),
            ("   ", None),
            ("abc", None),
            ("4.9", None),
            ("-1", None),
        ] {
            assert_eq!(subsec_millis(raw), expected, "reading {raw:?}");
        }
    }

    /// The whole point: `DateTimeOriginal` is second-resolution by spec, and the
    /// digits that separate one frame of a burst from the next live next door in
    /// `SubSecTimeOriginal`.
    #[test]
    fn test_best_guess_taken_exif_uses_subsec() {
        let exif = |date_tag: ExifTag, date: &str, subsec_tag: ExifTag, subsec: &str| {
            let mut tags = HashMap::new();
            tags.insert(date_tag.to_string(), date.to_string());
            tags.insert(subsec_tag.to_string(), subsec.to_string());
            Some(PsExifInfo {
                tags,
                ..Default::default()
            })
        };

        // A bare wall clock, with its fraction and no offset claimed.
        assert_eq!(
            best_guess_taken_exif(&exif(
                ExifTag::DateTimeOriginal,
                "2014:12:25 14:51:26",
                ExifTag::SubSecTimeOriginal,
                "674",
            ))
            .map(|t| t.to_string())
            .as_deref(),
            Some("2014-12-25T14:51:26.674")
        );

        // A reading that already carries an offset keeps it.
        assert_eq!(
            best_guess_taken_exif(&exif(
                ExifTag::DateTimeOriginal,
                "2023-01-18T21:05:38+13:00",
                ExifTag::SubSecTimeOriginal,
                "489",
            ))
            .map(|t| t.to_string())
            .as_deref(),
            Some("2023-01-18T21:05:38.489+13:00")
        );

        // Each date tag takes its own partner: `SubSecTime` qualifies
        // `ModifyDate`, and `SubSecTimeOriginal` must not be borrowed for it.
        let mut tags = HashMap::new();
        tags.insert(
            ExifTag::ModifyDate.to_string(),
            "2015:04:18 11:10:44".to_string(),
        );
        tags.insert(ExifTag::SubSecTime.to_string(), "939".to_string());
        tags.insert(ExifTag::SubSecTimeOriginal.to_string(), "111".to_string());
        assert_eq!(
            best_guess_taken_exif(&Some(PsExifInfo {
                tags,
                ..Default::default()
            }))
            .map(|t| t.to_string())
            .as_deref(),
            Some("2015-04-18T11:10:44.939")
        );

        // `GPSDateStamp` has no fraction to take, and no `SubSec*` tag is
        // allowed to invent one for a value that is only accurate to the day.
        // It is also the one EXIF date that really is UTC - it comes off the
        // GPS receiver - so unlike the readings above it prints an offset.
        let mut tags = HashMap::new();
        tags.insert(ExifTag::GPSDateStamp.to_string(), "2015:04:17".to_string());
        tags.insert(ExifTag::SubSecTimeOriginal.to_string(), "489".to_string());
        assert_eq!(
            best_guess_taken_exif(&Some(PsExifInfo {
                tags,
                ..Default::default()
            }))
            .map(|t| t.to_string())
            .as_deref(),
            Some("2015-04-17T00:00:00+00:00")
        );

        // An absent or unusable `SubSec*` leaves the whole second standing
        // rather than discarding a perfectly good date.
        assert_eq!(
            best_guess_taken_exif(&exif(
                ExifTag::DateTimeOriginal,
                "2014:12:25 14:51:26",
                ExifTag::SubSecTimeOriginal,
                "  ",
            ))
            .map(|t| t.to_string())
            .as_deref(),
            Some("2014-12-25T14:51:26")
        );
    }

    /// A real burst, straight off an iPhone: six frames, one second, six
    /// distinct names. Before the fraction was read these collapsed onto one
    /// name and five of them were pushed onto checksum suffixes.
    #[test]
    fn test_burst_frames_get_distinct_names() {
        let frame = |subsec: &str| {
            let mut tags = HashMap::new();
            tags.insert(
                ExifTag::DateTimeOriginal.to_string(),
                "2014:12:25 14:51:26".to_string(),
            );
            tags.insert(ExifTag::SubSecTimeOriginal.to_string(), subsec.to_string());
            let taken = best_guess_taken_exif(&Some(PsExifInfo {
                tags,
                ..Default::default()
            }));
            crate::output_path::get_desired_media_path("abc1234", &taken.map(|t| t.to_rfc3339()))
        };

        let names: Vec<String> = ["097", "186", "475", "575", "674", "774"]
            .into_iter()
            .map(frame)
            .collect();
        assert_eq!(
            names,
            vec![
                "2014/12/25/1451-26097",
                "2014/12/25/1451-26186",
                "2014/12/25/1451-26475",
                "2014/12/25/1451-26575",
                "2014/12/25/1451-26674",
                "2014/12/25/1451-26774",
            ]
        );
    }

    /// An unparseable tag must not shadow a good one further down the list.
    #[test]
    fn test_best_guess_falls_through_unparseable_tag() {
        let mut tags = HashMap::new();
        tags.insert(
            ExifTag::DateTimeOriginal.to_string(),
            "0000:00:00 00:00:00".to_string(),
        );
        tags.insert(
            ExifTag::ModifyDate.to_string(),
            "2015:04:18 11:10:44".to_string(),
        );
        let exif = Some(PsExifInfo {
            tags,
            ..Default::default()
        });
        assert_eq!(
            best_guess_taken_exif(&exif)
                .map(|t| t.to_string())
                .as_deref(),
            Some("2015-04-18T11:10:44")
        );
    }

    /// The whole point of the conversion: what comes out must survive the parse
    /// that decides the output path.
    #[test]
    fn test_converted_dates_produce_a_dated_path() -> anyhow::Result<()> {
        use anyhow::anyhow;
        crate::test_util::setup_log();
        let c = OsFileSystem::new("test");
        let reader = c.open("Canon_40D.jpg")?;
        let info = parse_exif_info(reader)?.ok_or_else(|| anyhow!("Failed to parse exif"))?;
        let taken = best_guess_taken_exif(&Some(info)).ok_or_else(|| anyhow!("no exif date"))?;
        let path = crate::output_path::get_desired_media_path("abc1234", &Some(taken.to_rfc3339()));
        assert!(!path.starts_with("undated/"), "got {path}");
        Ok(())
    }

    #[test]
    fn test_orientation_transform() {
        assert_eq!(orientation_transform("1"), (false, 0));
        assert_eq!(orientation_transform("2"), (true, 0));
        assert_eq!(orientation_transform("3"), (false, 180));
        assert_eq!(orientation_transform("4"), (true, 180));
        assert_eq!(orientation_transform("5"), (true, -90));
        assert_eq!(orientation_transform("6"), (false, 90));
        assert_eq!(orientation_transform("7"), (true, 90));
        assert_eq!(orientation_transform("8"), (false, -90));
        // Unknown/out-of-range values are treated as no transform.
        assert_eq!(orientation_transform("9"), (false, 0));
    }

    #[test]
    fn test_gps_version_only_yields_no_coords() -> anyhow::Result<()> {
        use anyhow::anyhow;
        crate::test_util::setup_log();
        // Canon_40D.jpg has a GPS sub-IFD with only GPSVersionID (no
        // GPSLatitude/GPSLongitude)
        let c = OsFileSystem::new("test");
        let reader = c.open("Canon_40D.jpg")?;
        let info = parse_exif_info(reader)?.ok_or_else(|| anyhow!("Failed to parse exif"))?;
        assert_eq!(info.gps, None);
        assert_eq!(info.latitude, None);
        assert_eq!(info.longitude, None);
        Ok(())
    }
}
