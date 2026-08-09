//! EXIF parsing. Parses as best it can and leaves judging validity to callers.

use crate::util::OutputTZ;
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use nom_exif::{EntryValue, ExifIter, ExifIterEntry, ExifTag, MediaKind, MediaParser, MediaSource};
use serde::Serialize;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use tracing::debug;

/// `tags` is the faithful record of what the file said; the three GPS fields
/// below are decoded from it for convenience and are free to drop a reading the
/// raw tags still carry. A null-island fix is exactly that case — it is filtered
/// out of all three at parse, while `GPSLatitude`/`GPSLongitude` stay in `tags`.
#[derive(Serialize, Debug, Clone)]
#[serde(rename_all(serialize = "camelCase"))]
pub(crate) struct PsExifInfo {
    /// Dates are ISO 8601.
    pub(crate) tags: HashMap<String, String>,
    /// ISO 6709.
    pub(crate) gps: Option<String>,
    pub(crate) latitude: Option<f64>,
    pub(crate) longitude: Option<f64>,
}

impl PsExifInfo {
    /// The GPS fix, treating the `(0, 0)` sentinel as absent. [`parse_exif_info`]
    /// already drops such a fix, so this only bites on a record built by hand.
    pub(crate) fn lat_long(&self) -> Option<(f64, f64)> {
        crate::util::non_zero_coords(self.latitude, self.longitude)
    }
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
    match exif_iter_r {
        Ok(exif_iter) => {
            for entry in exif_iter.clone() {
                let Some(tag_enum) = entry.tag().tag() else {
                    continue; // skip unrecognised tags
                };
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
fn exif_datetime_parse(raw: &str) -> Option<DateTime<FixedOffset>> {
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

/// Best guess at when an image was taken, from EXIF alone, as RFC 3339.
///
/// Tags are tried in descending order of trust, and one that is present but
/// unparseable is skipped rather than returned, so it cannot mask a good date
/// further down. `DateTimeOriginal` (shutter) and `CreateDate` are the same
/// instant on a digital camera; `ModifyDate` is only the last time the file was
/// *changed*, so an edit moves it years past the capture. GPS is the last
/// resort, still well above the file's mtime.
///
/// Each date tag carries whole seconds and has its own fraction tag. The pairing
/// reads wrong because `nom_exif` uses ExifTool's names rather than the spec's;
/// the IDs settle it, as Exif 2.32 defines `0x9290`–`0x9292` consecutively as
/// the fractions for DateTime / DateTimeOriginal / DateTimeDigitized. So the
/// bare `SubSecTime` belongs to `ModifyDate` — it is not a catch-all.
///
/// GPS is absent from the pairing because its reading is split date-and-time
/// rather than seconds-and-fraction. See [`gps_datetime`].
pub(crate) fn best_guess_taken_exif(exif: &Option<PsExifInfo>, tz: OutputTZ) -> Option<String> {
    let exif = exif.as_ref()?;
    let from_camera_clock = [
        // 0x9003 / 0x9291
        (ExifTag::DateTimeOriginal, ExifTag::SubSecTimeOriginal),
        // 0x9004 / 0x9292
        (ExifTag::CreateDate, ExifTag::SubSecTimeDigitized),
        // 0x0132 / 0x9290
        (ExifTag::ModifyDate, ExifTag::SubSecTime),
    ]
    .into_iter()
    .find_map(|(date_tag, sub_sec_tag)| {
        let dt = field_value(exif, date_tag)
            .as_deref()
            .and_then(exif_datetime_parse)?;
        // Replaces rather than adds: no usable sub-second tag keeps any fraction
        // the date tag itself spelled out.
        let dt = field_value(exif, sub_sec_tag)
            .as_deref()
            .and_then(sub_sec_millis)
            .and_then(|millis| dt.with_nanosecond(millis * 1_000_000))
            .unwrap_or(dt);
        Some(dt.to_rfc3339())
    });
    from_camera_clock.or_else(|| gps_datetime(exif, tz))
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
            ("2023-08-05T19:59:55+12:00", "2023-08-05T19:59:55+12:00"),
            ("2023-08-05T07:59:55Z", "2023-08-05T07:59:55+00:00"),
            // Bare wall-clock reading, the common case: read as UTC.
            ("2015-04-18 11:10:44", "2015-04-18T11:10:44+00:00"),
            ("2015-04-18T11:10:44", "2015-04-18T11:10:44+00:00"),
            ("2015:04:18 11:10:44", "2015-04-18T11:10:44+00:00"),
            ("2015-04-18 11:10:44.939", "2015-04-18T11:10:44.939+00:00"),
            // GPSDateStamp: a day and nothing finer.
            ("2015:04:17", "2015-04-17T00:00:00+00:00"),
            ("2015-04-17", "2015-04-17T00:00:00+00:00"),
            // EXIF strings are often space-padded.
            ("  2015:04:18 11:10:44 ", "2015-04-18T11:10:44+00:00"),
        ] {
            assert_eq!(
                exif_datetime_to_rfc3339(raw).as_deref(),
                Some(expected),
                "converting {raw:?}"
            );
        }
    }

    #[test]
    fn test_exif_datetime_rejects_unusable() {
        // Cameras with a dead clock write zeroes. Returning one of these would
        // date the photo to the year 0 rather than fall through to the next
        // source.
        for raw in [
            "",
            "   ",
            "0000:00:00 00:00:00",
            "2015:13:45 99:99:99",
            "not a date",
            "2015",
        ] {
            assert_eq!(exif_datetime_to_rfc3339(raw), None, "rejecting {raw:?}");
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
        })
    }

    /// Every tag set [`best_guess_taken_exif`] has to resolve, and the reading it
    /// must produce. The order under test is DateTimeOriginal, CreateDate,
    /// ModifyDate, then the GPS pair — each date paired with its own sub-second
    /// tag, and each unusable one falling through rather than being returned.
    #[test]
    fn test_best_guess_taken_exif_cases() {
        const DTO: ExifTag = ExifTag::DateTimeOriginal;
        const CREATE: ExifTag = ExifTag::CreateDate;
        const MODIFY: ExifTag = ExifTag::ModifyDate;
        const SUB_ORIGINAL: ExifTag = ExifTag::SubSecTimeOriginal;
        const SUB_DIGITIZED: ExifTag = ExifTag::SubSecTimeDigitized;
        const SUB_MODIFY: ExifTag = ExifTag::SubSecTime;
        const GPS_DATE: ExifTag = ExifTag::GPSDateStamp;
        const GPS_TIME: ExifTag = ExifTag::GPSTimeStamp;

        // what the row pins down, the tags, the reading they must produce
        type TakenCase = (
            &'static str,
            Vec<(ExifTag, &'static str)>,
            Option<&'static str>,
        );

        let cases: Vec<TakenCase> = vec![
            (
                "an unparseable date falls through to the next tag",
                vec![
                    (DTO, "0000:00:00 00:00:00"),
                    (MODIFY, "2015:04:18 11:10:44"),
                ],
                Some("2015-04-18T11:10:44+00:00"),
            ),
            (
                "CreateDate is preferred over ModifyDate",
                vec![
                    (CREATE, "2008:05:30 15:56:01"),
                    (MODIFY, "2008:07:31 10:38:11"),
                ],
                Some("2008-05-30T15:56:01+00:00"),
            ),
            (
                "SubSecTimeOriginal refines DateTimeOriginal",
                vec![(DTO, "2015:04:18 11:10:44"), (SUB_ORIGINAL, "939")],
                Some("2015-04-18T11:10:44.939+00:00"),
            ),
            // Crossing the pairs would stamp one reading's fraction onto
            // another's seconds.
            (
                "SubSecTime belongs to ModifyDate, not DateTimeOriginal",
                vec![(DTO, "2015:04:18 11:10:44"), (SUB_MODIFY, "939")],
                Some("2015-04-18T11:10:44+00:00"),
            ),
            (
                "SubSecTimeDigitized pairs with CreateDate",
                vec![
                    (CREATE, "2015:04:18 11:10:44"),
                    (SUB_DIGITIZED, "5"),
                    (SUB_ORIGINAL, "939"),
                ],
                Some("2015-04-18T11:10:44.500+00:00"),
            ),
            (
                "SubSecTime pairs with ModifyDate",
                vec![(MODIFY, "2015:04:18 11:10:44"), (SUB_MODIFY, "07")],
                Some("2015-04-18T11:10:44.070+00:00"),
            ),
            (
                "the sub-second tag says nothing about the zone",
                vec![(DTO, "2023-08-05T19:59:55+12:00"), (SUB_ORIGINAL, "42")],
                Some("2023-08-05T19:59:55.420+12:00"),
            ),
            (
                "a sub-second tag does not rescue or follow a rejected date",
                vec![
                    (DTO, "0000:00:00 00:00:00"),
                    (SUB_ORIGINAL, "939"),
                    (MODIFY, "2015:04:18 11:10:44"),
                ],
                Some("2015-04-18T11:10:44+00:00"),
            ),
            // Reading only GPSDateStamp buckets every GPS-dated file at midnight
            // UTC — exactly the day boundary. 19:30 on the 17th in Greenwich is
            // 07:30 on the *18th* at +12:00, so a regression to rendering at UTC
            // shows up as a different day, not merely a different offset.
            (
                "the GPS date and time are one reading, in the output zone",
                vec![(GPS_DATE, "2015:04:17"), (GPS_TIME, "19:30:45")],
                Some("2015-04-18T07:30:45+12:00"),
            ),
            (
                "the GPS seconds field is a rational, so it can carry a fraction",
                vec![(GPS_DATE, "2015:04:17"), (GPS_TIME, "19:30:45.250")],
                Some("2015-04-18T07:30:45.250+12:00"),
            ),
            (
                "the GPS reading has no sub-second tag of its own",
                vec![
                    (GPS_DATE, "2015:04:17"),
                    (GPS_TIME, "19:30:45"),
                    (SUB_ORIGINAL, "939"),
                    (SUB_MODIFY, "939"),
                    (SUB_DIGITIZED, "939"),
                ],
                Some("2015-04-18T07:30:45+12:00"),
            ),
            (
                "the camera clock outranks GPS",
                vec![
                    (MODIFY, "2008:07:31 10:38:11"),
                    (GPS_DATE, "2015:04:17"),
                    (GPS_TIME, "19:30:45"),
                ],
                Some("2008-07-31T10:38:11+00:00"),
            ),
            // Midnight is worse than the real time but far better than no date,
            // which would drop the file through to its mtime.
            (
                "an absent GPS time falls back to the date alone",
                vec![(GPS_DATE, "2015:04:17")],
                Some("2015-04-17T00:00:00+00:00"),
            ),
        ];

        for (label, tags, expected) in cases {
            assert_eq!(
                best_guess_taken_exif(&exif_with(&tags), tz()).as_deref(),
                expected,
                "{label}"
            );
        }

        // Same fallback, for every spelling of a GPS time that cannot be read.
        for time in ["", "   ", "not a time", "URationalArray[19/1 (19.0000)]"] {
            let exif = exif_with(&[(GPS_DATE, "2015:04:17"), (GPS_TIME, time)]);
            assert_eq!(
                best_guess_taken_exif(&exif, tz()).as_deref(),
                Some("2015-04-17T00:00:00+00:00"),
                "with GPSTimeStamp {time:?}"
            );
        }
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

        let taken = best_guess_taken_exif(&Some(info), tz());
        assert_eq!(taken.as_deref(), Some("2015-04-18T07:30:45+12:00"));

        // Bucketed on the converted reading: 19:30 on the 17th in Greenwich is
        // the 18th at +12:00.
        assert_eq!(
            crate::media::get_desired_media_path("abc1234", &taken),
            "2015/04/18/0730-45000"
        );
        Ok(())
    }

    /// The written filename is the only place a user sees the sub-second reading.
    #[test]
    fn test_sub_second_reaches_the_output_path() {
        let exif = exif_with(&[
            (ExifTag::DateTimeOriginal, "2008:05:30 15:56:01"),
            (ExifTag::SubSecTimeOriginal, "07"),
        ]);
        let taken = best_guess_taken_exif(&exif, tz());
        assert_eq!(
            crate::media::get_desired_media_path("abc1234", &taken),
            "2008/05/30/1556-01070"
        );
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

    #[test]
    fn test_gps_version_only_yields_no_coords() -> anyhow::Result<()> {
        use anyhow::anyhow;
        crate::test_util::setup_log();
        // Canon_40D.jpg has a GPS sub-IFD with only GPSVersionID.
        let c = OsFileSystem::new("test");
        let reader = c.open("Canon_40D.jpg")?;
        let info = parse_exif_info(reader)?.ok_or_else(|| anyhow!("Failed to parse exif"))?;
        assert_eq!(info.gps, None);
        assert_eq!(info.latitude, None);
        assert_eq!(info.longitude, None);
        Ok(())
    }
}
