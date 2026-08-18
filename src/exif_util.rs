//! EXIF parsing. Parses as best it can and leaves judging validity to callers.

use crate::util::OutputTZ;
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use nom_exif::{EntryValue, ExifIter, ExifIterEntry, ExifTag, MediaKind, MediaParser, MediaSource};
use serde::Serialize;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use tracing::debug;

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all(serialize = "camelCase"))]
pub(crate) struct PsExifInfo {
    /// Dates are ISO 8601.
    pub(crate) tags: HashMap<String, String>,
    /// ISO 6709.
    pub(crate) gps: Option<String>,
    pub(crate) latitude: Option<f64>,
    pub(crate) longitude: Option<f64>,
    /// From an Apple maker note: the UUID this still shares with its Live Photo
    /// video. See [`crate::apple_maker_note`].
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
                // A vendor blob rather than a value, so it is read on its own
                // terms rather than rendered into `tags`.
                if tag_enum == ExifTag::MakerNote {
                    content_identifier = entry
                        .clone()
                        .into_result()
                        .ok()
                        .as_ref()
                        .and_then(EntryValue::as_undefined)
                        .and_then(crate::apple_maker_note::content_identifier);
                    continue;
                }
                let tag_name = tag_enum.to_string();
                let s_o = field_to_opt_string(tag_enum, &entry);
                let Some(s) = s_o else {
                    continue;
                };
                if s.len() > 1024 {
                    continue;
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
        Err(e) => {
            debug!("Could not read EXIF data: {e}");
        }
    }
    Ok(Some(PsExifInfo {
        tags: m,
        gps: ps_gps_info,
        latitude: lat,
        longitude: long,
        content_identifier,
    }))
}

fn field_to_opt_string(tag: ExifTag, field: &ExifIterEntry) -> Option<String> {
    let value = field.clone().into_result().ok()?;
    if matches!(value, EntryValue::Undefined(_)) {
        return None;
    }
    if tag == ExifTag::GPSTimeStamp {
        return gps_time_of_day(&value).or_else(|| Some(value.to_string()));
    }
    Some(value.to_string())
}

/// Render `GPSTimeStamp` as `hh:mm:ss`, with a fraction only when there is one.
///
/// The tag is three rationals, not a string; rendered generically it comes out
/// as `URationalArray[19/1 (19.0000), …]`, useless to a date parser. All-zeros
/// is a legitimate midnight and is kept.
fn gps_time_of_day(value: &EntryValue) -> Option<String> {
    let [hours, minutes, seconds] = value.as_urational_slice()? else {
        return None;
    };
    let (hours, minutes, seconds) = (hours.to_f64()?, minutes.to_f64()?, seconds.to_f64()?);
    // Seconds allow 60 for a leap second.
    if !(0.0..24.0).contains(&hours)
        || !(0.0..60.0).contains(&minutes)
        || !(0.0..61.0).contains(&seconds)
    {
        return None;
    }
    let (hours, minutes) = (hours as u32, minutes as u32);
    let whole = seconds.trunc() as u32;
    let millis = ((seconds.fract() * 1000.0).round() as u32).min(999);
    if millis == 0 {
        Some(format!("{hours:02}:{minutes:02}:{whole:02}"))
    } else {
        Some(format!("{hours:02}:{minutes:02}:{whole:02}.{millis:03}"))
    }
}

fn field_value(exif: &PsExifInfo, code: ExifTag) -> Option<String> {
    exif.tags.get(code.name()).cloned()
}

pub(crate) fn camera_make(exif: &PsExifInfo) -> Option<String> {
    field_value(exif, ExifTag::Make)
}

pub(crate) fn camera_model(exif: &PsExifInfo) -> Option<String> {
    field_value(exif, ExifTag::Model)
}

pub(crate) fn image_width(exif: &PsExifInfo) -> Option<i64> {
    field_value(exif, ExifTag::ExifImageWidth)
        .or_else(|| field_value(exif, ExifTag::ImageWidth))
        .and_then(|s| s.trim().parse::<i64>().ok())
}

pub(crate) fn image_height(exif: &PsExifInfo) -> Option<i64> {
    field_value(exif, ExifTag::ExifImageHeight)
        .or_else(|| field_value(exif, ExifTag::ImageHeight))
        .and_then(|s| s.trim().parse::<i64>().ok())
}

/// The display transform implied by EXIF `Orientation`. `None` when the tag is
/// absent (e.g. videos). Distinct from the derived aspect orientation.
pub(crate) fn exif_display_transform(exif: &PsExifInfo) -> Option<(bool, i32)> {
    let raw = field_value(exif, ExifTag::Orientation)?;
    Some(orientation_transform(raw.trim()))
}

/// `Orientation` (`1`–`8`) as `(mirrored, rotate)`: a horizontal mirror applied
/// before a clockwise rotation of `rotate` degrees. Unrecognised means no
/// transform.
fn orientation_transform(raw: &str) -> (bool, i32) {
    match raw {
        "2" => (true, 0),
        "3" => (false, 180),
        "4" => (true, 180),
        "5" => (true, -90),
        "6" => (false, 90),
        "7" => (true, 90),
        "8" => (false, -90),
        _ => (false, 0),
    }
}

/// Parse any of EXIF's date spellings: a full RFC 3339 instant (only when the
/// file carries an `OffsetTime` tag), a bare wall-clock reading with either `-`
/// or EXIF's `:` date separators, or a `GPSDateStamp` date with no time.
///
/// A reading with no offset is taken as UTC. The camera's real zone is unknowable
/// from the file, and this keeps the wall-clock reading — what the photographer
/// saw, and what the `yyyy/mm/dd` bucketing is for — intact.
///
/// Stays a chrono value rather than a string so [`sub_sec_millis`] can still be
/// folded in; no EXIF date tag spells out its own fraction of a second.
pub(crate) fn exif_datetime_parse(raw: &str) -> Option<DateTime<FixedOffset>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Some(dt);
    }
    let normalised = dashed_date_separators(raw);
    for format in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(&normalised, format) {
            return Some(naive.and_utc().fixed_offset());
        }
    }
    // A date alone pins down a day and nothing finer; midnight is the only
    // instant it justifies.
    if let Ok(date) = NaiveDate::parse_from_str(&normalised, "%Y-%m-%d") {
        return Some(date.and_time(NaiveTime::MIN).and_utc().fixed_offset());
    }
    None
}

/// Read an EXIF `SubSecTime*` value as whole milliseconds.
///
/// The tag holds the digits that follow the decimal point, not a count: `"07"`
/// is 70 ms and `"9"` is 900 ms, so it is read positionally rather than parsed
/// as an integer. Anything finer than a millisecond is dropped, as that is all
/// the output path carries.
///
/// Cameras pad the field with spaces or NULs. A non-numeric value is *no*
/// reading rather than a zero one, leaving whatever the date tag itself carried.
fn sub_sec_millis(raw: &str) -> Option<u32> {
    let digits = raw.trim_matches(|c: char| c.is_whitespace() || c == '\0');
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let digits = digits.as_bytes();
    Some((0..3).fold(0u32, |millis, i| {
        millis * 10 + digits.get(i).map_or(0, |b| u32::from(b - b'0'))
    }))
}

/// Rewrite EXIF's `YYYY:MM:DD` date separators as dashes so one set of chrono
/// patterns covers both spellings. Only touches a leading date that is all ASCII
/// digits and colons, so the byte offsets below are safe.
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

/// One date tag with its own fraction folded in.
///
/// The fraction pairing reads wrong because `nom_exif` uses ExifTool's names
/// rather than the spec's; the IDs settle it, as Exif 2.32 defines
/// `0x9290`–`0x9292` consecutively as the fractions for DateTime /
/// DateTimeOriginal / DateTimeDigitized. So the bare `SubSecTime` belongs to
/// `ModifyDate` — it is not a catch-all.
fn dated_tag(
    exif: &PsExifInfo,
    date_tag: ExifTag,
    sub_sec_tag: ExifTag,
) -> Option<DateTime<FixedOffset>> {
    let dt = field_value(exif, date_tag)
        .as_deref()
        .and_then(exif_datetime_parse)?;
    // Replaces rather than adds: no usable sub-second tag keeps any fraction
    // the date tag itself spelled out.
    Some(
        field_value(exif, sub_sec_tag)
            .as_deref()
            .and_then(sub_sec_millis)
            .and_then(|millis| dt.with_nanosecond(millis * 1_000_000))
            .unwrap_or(dt),
    )
}

/// The camera's own reading of when the shutter fired: `DateTimeOriginal`, else
/// `CreateDate`, with its fraction of a second folded in.
///
/// This is the archive's primary date. It is the photographer's wall clock as
/// the camera displayed it — the exact thing `yyyy/mm/dd/hhmm-ssms` is built
/// from — so it needs no zone conversion and no guess about where the
/// photographer was standing.
///
/// `ModifyDate` and GPS are deliberately excluded. Neither means "taken at":
/// `ModifyDate` is the last time the file was *changed*, so an edit moves it
/// years past the capture, and GPS is a separate receiver's reading. Both stay
/// in [`fallback_taken_exif`], well down the order.
pub(crate) fn capture_reading(exif: &Option<PsExifInfo>) -> Option<DateTime<FixedOffset>> {
    let exif = exif.as_ref()?;
    [
        // 0x9003 / 0x9291
        (ExifTag::DateTimeOriginal, ExifTag::SubSecTimeOriginal),
        // 0x9004 / 0x9292
        (ExifTag::CreateDate, ExifTag::SubSecTimeDigitized),
    ]
    .into_iter()
    .find_map(|(date_tag, sub_sec_tag)| dated_tag(exif, date_tag, sub_sec_tag))
}

/// The EXIF dates that are *not* the shutter, for files that carry no capture
/// reading at all: `ModifyDate`, then GPS.
///
/// A tag that is present but unparseable is skipped rather than returned, so it
/// cannot mask a good date further down. GPS is absent from the fraction pairing
/// because its reading is split date-and-time rather than seconds-and-fraction —
/// see [`gps_datetime`].
pub(crate) fn fallback_taken_exif(exif: &Option<PsExifInfo>, tz: OutputTZ) -> Option<String> {
    let exif = exif.as_ref()?;
    // 0x0132 / 0x9290
    dated_tag(exif, ExifTag::ModifyDate, ExifTag::SubSecTime)
        .map(|dt| dt.to_rfc3339())
        .or_else(|| gps_datetime(exif, tz))
}

/// The GPS receiver's reading of when the shutter fired, from the
/// `GPSDateStamp` / `GPSTimeStamp` pair — one reading split across two tags.
///
/// Unlike the camera's clock these are UTC by definition, so a full reading is a
/// true instant and gets converted to the output zone like any other. See
/// [`crate::util::OutputTZ`].
///
/// The date-only fallback is deliberately *not* converted. `GPSDateStamp` alone
/// pins down a UTC day and nothing finer; shifting an assumed midnight by the
/// local offset recovers no information and just files everyone west of UTC on
/// the less likely of the two days it could be.
fn gps_datetime(exif: &PsExifInfo, tz: OutputTZ) -> Option<String> {
    let date = field_value(exif, ExifTag::GPSDateStamp)?;
    let date = date.trim();
    // The empty time tag needs excluding by hand rather than by letting the parse
    // fail: `"2015-04-17 "` still parses — as a bare date — so it would reach the
    // conversion below and be shifted onto midnight's neighbouring day.
    let full_reading = field_value(exif, ExifTag::GPSTimeStamp)
        .map(|time| time.trim().to_string())
        .filter(|time| !time.is_empty())
        .and_then(|time| exif_datetime_parse(&format!("{date} {time}")));
    if let Some(dt) = full_reading {
        return Some(tz.render(dt.to_utc()));
    }
    exif_datetime_parse(date).map(|dt| dt.to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::FileSystem;
    use crate::fs::OsFileSystem;
    use crate::test_util::tz;
    use nom_exif::URational;

    #[test]
    fn test_parse_exif_mp4() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let c = OsFileSystem::new("test");
        let reader = c.open("Hello.mp4")?;
        let t = parse_exif_info(reader)?;
        assert!(t.is_none());
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

    /// The Apple maker note is a vendor blob, so it never lands in `tags` — the
    /// identifier is read out of it separately.
    #[test]
    fn test_content_identifier_from_an_apple_maker_note() -> anyhow::Result<()> {
        use anyhow::anyhow;
        crate::test_util::setup_log();
        let c = OsFileSystem::new("test/live_photo");
        let info = parse_exif_info(c.open("still.jpg")?)?
            .ok_or_else(|| anyhow!("Failed to parse exif"))?;
        assert_eq!(
            info.content_identifier.as_deref(),
            Some("11111111-2222-3333-4444-555555555555")
        );
        assert!(!info.tags.contains_key(&ExifTag::MakerNote.to_string()));

        // A photo from a camera that writes no Apple maker note.
        let c = OsFileSystem::new("test");
        let info = parse_exif_info(c.open("Canon_40D.jpg")?)?
            .ok_or_else(|| anyhow!("Failed to parse exif"))?;
        assert_eq!(info.content_identifier, None);
        Ok(())
    }

    /// The accessors the db columns are filled from, over the one real file
    /// that has every tag they read.
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
        // Orientation "1" is the no-op transform.
        assert_eq!(exif_display_transform(&info), Some((false, 0)));
        // The file's GPS sub-IFD holds only GPSVersionID, which is not a fix.
        assert_eq!(info.gps, None);
        assert_eq!(info.latitude, None);
        assert_eq!(info.longitude, None);
        Ok(())
    }

    fn exif_datetime_to_rfc3339(raw: &str) -> Option<String> {
        exif_datetime_parse(raw).map(|dt| dt.to_rfc3339())
    }

    /// RFC 3339 is the only form `get_desired_media_path` parses — anything else
    /// lands the file in `undated/`.
    #[test]
    fn test_exif_datetime_to_rfc3339() {
        for (raw, expected) in [
            // Already an instant (file carried an OffsetTime tag): kept as-is.
            (
                "2023-08-05T19:59:55+12:00",
                Some("2023-08-05T19:59:55+12:00"),
            ),
            ("2023-08-05T07:59:55Z", Some("2023-08-05T07:59:55+00:00")),
            // Bare wall-clock reading, the common case: read as UTC.
            ("2015-04-18 11:10:44", Some("2015-04-18T11:10:44+00:00")),
            ("2015-04-18T11:10:44", Some("2015-04-18T11:10:44+00:00")),
            ("2015:04:18 11:10:44", Some("2015-04-18T11:10:44+00:00")),
            (
                "2015-04-18 11:10:44.939",
                Some("2015-04-18T11:10:44.939+00:00"),
            ),
            // GPSDateStamp: a day and nothing finer.
            ("2015:04:17", Some("2015-04-17T00:00:00+00:00")),
            ("2015-04-17", Some("2015-04-17T00:00:00+00:00")),
            // EXIF strings are often space-padded.
            ("  2015:04:18 11:10:44 ", Some("2015-04-18T11:10:44+00:00")),
            // Cameras with a dead clock write zeroes. Returning one of these
            // would date the photo to the year 0 rather than fall through to
            // the next source.
            ("0000:00:00 00:00:00", None),
            ("2015:13:45 99:99:99", None),
            ("", None),
            ("   ", None),
            ("not a date", None),
            ("2015", None),
        ] {
            assert_eq!(
                exif_datetime_to_rfc3339(raw).as_deref(),
                expected,
                "converting {raw:?}"
            );
        }
    }

    fn exif_with(pairs: &[(ExifTag, &str)]) -> Option<PsExifInfo> {
        let mut tags = HashMap::new();
        for (tag, value) in pairs {
            tags.insert(tag.to_string(), (*value).to_string());
        }
        Some(PsExifInfo {
            tags,
            gps: None,
            latitude: None,
            longitude: None,
            content_identifier: None,
        })
    }

    #[test]
    fn test_sub_sec_millis_reads_a_fraction_not_an_integer() {
        for (raw, expected) in [
            ("9", Some(900)),
            ("07", Some(70)),
            ("93", Some(930)),
            ("939", Some(939)),
            ("00", Some(0)),
            ("0", Some(0)),
            // Truncated, never rounded up into the next millisecond.
            ("9399", Some(939)),
            ("000123", Some(0)),
            // Cameras pad the fixed-width field with spaces or NULs.
            ("  07  ", Some(70)),
            ("07\0", Some(70)),
            // Nothing usable: leaves the reading alone rather than zeroing it.
            ("", None),
            ("   ", None),
            ("abc", None),
            ("1.5", None),
            ("-1", None),
        ] {
            assert_eq!(sub_sec_millis(raw), expected, "reading {raw:?}");
        }
    }

    /// Which shutter tag wins, and what the winning one is refined by.
    #[test]
    fn test_capture_reading() {
        // (why it matters, the tags present, the reading they should produce)
        let cases = vec![
            (
                "DateTimeOriginal is preferred over CreateDate",
                vec![
                    (ExifTag::DateTimeOriginal, "2008:05:30 15:56:01"),
                    (ExifTag::CreateDate, "2008:06:30 15:56:01"),
                ],
                Some("2008-05-30T15:56:01+00:00"),
            ),
            (
                "an unparseable tag falls through to the next shutter tag",
                vec![
                    (ExifTag::DateTimeOriginal, "0000:00:00 00:00:00"),
                    (ExifTag::CreateDate, "2015:04:18 11:10:44"),
                ],
                Some("2015-04-18T11:10:44+00:00"),
            ),
            (
                "a sub-second tag neither rescues a rejected date nor follows it",
                vec![
                    (ExifTag::DateTimeOriginal, "0000:00:00 00:00:00"),
                    (ExifTag::SubSecTimeOriginal, "939"),
                    (ExifTag::CreateDate, "2015:04:18 11:10:44"),
                ],
                Some("2015-04-18T11:10:44+00:00"),
            ),
            (
                "the sub-second tag refines its own date",
                vec![
                    (ExifTag::DateTimeOriginal, "2015:04:18 11:10:44"),
                    (ExifTag::SubSecTimeOriginal, "939"),
                ],
                Some("2015-04-18T11:10:44.939+00:00"),
            ),
            // Crossing the pairs would stamp one reading's fraction onto
            // another's seconds.
            (
                "SubSecTime belongs to ModifyDate, so it must not refine DateTimeOriginal",
                vec![
                    (ExifTag::DateTimeOriginal, "2015:04:18 11:10:44"),
                    (ExifTag::SubSecTime, "939"),
                ],
                Some("2015-04-18T11:10:44+00:00"),
            ),
            (
                "CreateDate takes SubSecTimeDigitized, not SubSecTimeOriginal",
                vec![
                    (ExifTag::CreateDate, "2015:04:18 11:10:44"),
                    (ExifTag::SubSecTimeDigitized, "5"),
                    (ExifTag::SubSecTimeOriginal, "939"),
                ],
                Some("2015-04-18T11:10:44.500+00:00"),
            ),
            (
                "the sub-second tag says nothing about the zone",
                vec![
                    (ExifTag::DateTimeOriginal, "2023-08-05T19:59:55+12:00"),
                    (ExifTag::SubSecTimeOriginal, "42"),
                ],
                Some("2023-08-05T19:59:55.420+12:00"),
            ),
            // The shutter is the archive's primary date, so nothing that merely
            // resembles one may stand in for it here.
            (
                "ModifyDate is not a shutter reading",
                vec![(ExifTag::ModifyDate, "2008:07:31 10:38:11")],
                None,
            ),
            (
                "nor is a GPS reading",
                vec![
                    (ExifTag::GPSDateStamp, "2015:04:17"),
                    (ExifTag::GPSTimeStamp, "19:30:45"),
                ],
                None,
            ),
        ];
        for (why, tags, expected) in cases {
            assert_eq!(
                capture_reading(&exif_with(&tags))
                    .map(|dt| dt.to_rfc3339())
                    .as_deref(),
                expected,
                "{why}"
            );
        }
        assert!(capture_reading(&None).is_none());
    }

    /// What a file with no shutter reading falls back on. The GPS rows are
    /// stated at `+12:00`: 19:30 on the 17th in Greenwich is 07:30 on the
    /// *18th* there, so a regression to rendering at UTC shows up as a different
    /// day rather than merely a different offset.
    #[test]
    fn test_fallback_taken_exif() {
        // (why it matters, the tags present, the reading they should produce)
        let cases = vec![
            (
                "ModifyDate takes SubSecTime",
                vec![
                    (ExifTag::ModifyDate, "2015:04:18 11:10:44"),
                    (ExifTag::SubSecTime, "07"),
                ],
                "2015-04-18T11:10:44.070+00:00",
            ),
            // Reading only `GPSDateStamp` buckets every GPS-dated file at
            // midnight UTC — exactly the day boundary, so photographers west of
            // UTC file a day early.
            (
                "the GPS date and time are one reading, converted to the output zone",
                vec![
                    (ExifTag::GPSDateStamp, "2015:04:17"),
                    (ExifTag::GPSTimeStamp, "19:30:45"),
                ],
                "2015-04-18T07:30:45+12:00",
            ),
            (
                "the GPS seconds field is a rational, so it can carry a fraction",
                vec![
                    (ExifTag::GPSDateStamp, "2015:04:17"),
                    (ExifTag::GPSTimeStamp, "19:30:45.250"),
                ],
                "2015-04-18T07:30:45.250+12:00",
            ),
            (
                "the GPS reading has no sub-second tag of its own",
                vec![
                    (ExifTag::GPSDateStamp, "2015:04:17"),
                    (ExifTag::GPSTimeStamp, "19:30:45"),
                    (ExifTag::SubSecTimeOriginal, "939"),
                    (ExifTag::SubSecTime, "939"),
                    (ExifTag::SubSecTimeDigitized, "939"),
                ],
                "2015-04-18T07:30:45+12:00",
            ),
            (
                "ModifyDate outranks GPS",
                vec![
                    (ExifTag::ModifyDate, "2008:07:31 10:38:11"),
                    (ExifTag::GPSDateStamp, "2015:04:17"),
                    (ExifTag::GPSTimeStamp, "19:30:45"),
                ],
                "2008-07-31T10:38:11+00:00",
            ),
        ];
        for (why, tags, expected) in cases {
            assert_eq!(
                fallback_taken_exif(&exif_with(&tags), tz()).as_deref(),
                Some(expected),
                "{why}"
            );
        }
    }

    /// Midnight is worse than the real time but far better than no date, which
    /// would drop the file through to its mtime.
    #[test]
    fn test_unusable_gps_time_falls_back_to_the_date() {
        for time in [
            Some(""),
            Some("   "),
            Some("not a time"),
            Some("URationalArray[19/1 (19.0000)]"),
            None,
        ] {
            let mut tags = vec![(ExifTag::GPSDateStamp, "2015:04:17")];
            if let Some(time) = time {
                tags.push((ExifTag::GPSTimeStamp, time));
            }
            assert_eq!(
                fallback_taken_exif(&exif_with(&tags), tz()).as_deref(),
                Some("2015-04-17T00:00:00+00:00"),
                "with GPSTimeStamp {time:?}"
            );
        }
    }

    #[test]
    fn test_gps_time_of_day_renders_rationals() {
        let time = |h: (u32, u32), m: (u32, u32), s: (u32, u32)| {
            EntryValue::URationalArray(vec![
                URational::new(h.0, h.1),
                URational::new(m.0, m.1),
                URational::new(s.0, s.1),
            ])
        };
        let whole = |h, m, s| time((h, 1), (m, 1), (s, 1));

        assert_eq!(
            gps_time_of_day(&whole(19, 30, 45)).as_deref(),
            Some("19:30:45")
        );
        // All-zeros is a legitimate midnight rather than absence.
        assert_eq!(
            gps_time_of_day(&whole(0, 0, 0)).as_deref(),
            Some("00:00:00")
        );
        assert_eq!(
            gps_time_of_day(&whole(9, 5, 3)).as_deref(),
            Some("09:05:03")
        );
        assert_eq!(
            gps_time_of_day(&time((19, 1), (30, 1), (455, 10))).as_deref(),
            Some("19:30:45.500")
        );
        // Out of range for a clock.
        assert_eq!(gps_time_of_day(&whole(24, 0, 0)), None);
        assert_eq!(gps_time_of_day(&whole(0, 60, 0)), None);
        // Zero denominator, then the wrong shape entirely.
        assert_eq!(gps_time_of_day(&time((19, 0), (30, 1), (45, 1))), None);
        assert_eq!(
            gps_time_of_day(&EntryValue::URationalArray(vec![URational::new(19, 1)])),
            None
        );
        assert_eq!(gps_time_of_day(&EntryValue::Text("19:30:45".into())), None);
    }

    /// End to end on a real file whose only date is the GPS pair.
    #[test]
    fn test_gps_only_file_is_dated_from_the_gps_pair() -> anyhow::Result<()> {
        use anyhow::anyhow;
        crate::test_util::setup_log();
        let c = OsFileSystem::new("test");
        let reader = c.open("gps_date_only.jpg")?;
        let info = parse_exif_info(reader)?.ok_or_else(|| anyhow!("Failed to parse exif"))?;

        // The rationals are normalised on the way in, not left as a debug string.
        assert_eq!(
            info.tags
                .get(&ExifTag::GPSTimeStamp.to_string())
                .map(|s| s.as_str()),
            Some("19:30:45")
        );
        for absent in [
            ExifTag::DateTimeOriginal,
            ExifTag::CreateDate,
            ExifTag::ModifyDate,
        ] {
            assert!(
                !info.tags.contains_key(&absent.to_string()),
                "{absent:?} should be stripped from this fixture"
            );
        }

        let info = Some(info);
        assert!(
            capture_reading(&info).is_none(),
            "a GPS reading is not a shutter reading"
        );
        assert_eq!(
            fallback_taken_exif(&info, tz()).as_deref(),
            Some("2015-04-18T07:30:45+12:00")
        );
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
        assert_eq!(orientation_transform("9"), (false, 0));
    }

    /// Malformed bytes must be swallowed rather than panicking or erroring: any
    /// tags read out of them are unusable, so no date comes back.
    #[test]
    fn test_parse_exif_never_panics_on_malformed_bytes() -> anyhow::Result<()> {
        use std::io::Cursor;
        crate::test_util::setup_log();
        let real_jpeg = std::fs::read("test/Canon_40D.jpg")?;
        let cases: Vec<Vec<u8>> = vec![
            vec![],           // empty
            vec![0x00],       // single byte
            vec![0xff, 0xd8], // SOI marker only
            b"this is plainly not an image".to_vec(),
            (0u8..=255).cycle().take(4096).collect(), // structured garbage
            real_jpeg.into_iter().take(64).collect(), // valid header, cut short
            crate::test_util::fake_png(),             // wrong container for the bytes
            b"GIF89a\x01\x00\x01\x00".to_vec(),       // a stubby GIF
        ];
        for bytes in cases {
            let len = bytes.len();
            let info = parse_exif_info(Cursor::new(bytes))?;
            assert!(
                capture_reading(&info).is_none() && fallback_taken_exif(&info, tz()).is_none(),
                "malformed input of {len} bytes should date nothing"
            );
        }
        Ok(())
    }
}
