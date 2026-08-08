use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use nom_exif::{EntryValue, ExifIter, ExifIterEntry, ExifTag, MediaKind, MediaParser, MediaSource};
use serde::Serialize;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use tracing::debug;

/*

Util file to help with exif parsing.

it's not the responsibility of this module to decide if exif data is valid or not, just to
parse it best as possible.

store in db as json

 */

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all(serialize = "camelCase"))]
pub(crate) struct PsExifInfo {
    // dates as ISO 8601
    pub(crate) tags: HashMap<String, String>,
    // as iso6709
    pub(crate) gps: Option<String>,
    pub(crate) latitude: Option<f64>,
    pub(crate) longitude: Option<f64>,
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
        return None; // skip undefined values
    }
    if tag == ExifTag::GPSTimeStamp {
        // Not a string at all, and the generic rendering of it is unusable.
        // Falls back to that rendering rather than dropping a tag we cannot
        // make sense of - deciding what is valid is not this module's job.
        return gps_time_of_day(&value).or_else(|| Some(value.to_string()));
    }
    // dates are returned as a ISO 8601 string with no timezone
    Some(value.to_string())
}

/// Render `GPSTimeStamp` as `hh:mm:ss`, with a fraction only when there is one.
///
/// The tag is three rationals — hour, minute, second — not a string, and the
/// seconds one is a rational precisely so it can carry a fraction. Rendered
/// generically it comes out as `URationalArray[19/1 (19.0000), …]`, which is no
/// use to a date parser and no use to a human reading the stored metadata, so it
/// is normalised here on the way in.
///
/// Values outside the ranges a clock can hold are rejected; a receiver with
/// nothing to report writes zeros, which are a legitimate midnight and kept.
fn gps_time_of_day(value: &EntryValue) -> Option<String> {
    let [hours, minutes, seconds] = value.as_urational_slice()? else {
        return None;
    };
    let (hours, minutes, seconds) = (hours.to_f64()?, minutes.to_f64()?, seconds.to_f64()?);
    // Seconds allow 60 for a leap second; the date parser is the final judge.
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

/// Read an EXIF date/time tag. Rendering it gives RFC 3339, the form every other
/// date source in [`crate::media::best_guess_taken_dt`] returns and the only one
/// [`crate::media::get_desired_media_path`] can parse; it stays a chrono value
/// until then so a `SubSecTime*` tag can still be folded in.
///
/// EXIF has no single spelling. Depending on whether the file carries an
/// `OffsetTime` tag, the parser hands back either a full RFC 3339 instant
/// (`2023-08-05T19:59:55+12:00`) or a bare local wall-clock reading
/// (`2015-04-18 11:10:44`); the raw tag form separates the date with colons
/// (`2015:04:18 11:10:44`), and `GPSDateStamp` is a date with no time at all
/// (`2015:04:17`). None of them spell out a fraction of a second — that is
/// always a separate tag, which is what [`sub_sec_millis`] is for.
///
/// A reading with no offset is taken as UTC. The camera's real zone is unknown
/// and unknowable from the file, and this keeps the wall-clock reading — which is
/// what the photographer saw and what the `yyyy/mm/dd` bucketing is for — intact.
fn exif_datetime_parse(raw: &str) -> Option<DateTime<FixedOffset>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    // Already carries an offset: keep the recorded instant exactly as it stands.
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Some(dt);
    }
    let normalised = dashed_date_separators(raw);
    for format in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(&normalised, format) {
            return Some(naive.and_utc().fixed_offset());
        }
    }
    // `GPSDateStamp` pins down a day and nothing finer; midnight is the only
    // instant it justifies.
    if let Ok(date) = NaiveDate::parse_from_str(&normalised, "%Y-%m-%d") {
        return Some(date.and_time(NaiveTime::MIN).and_utc().fixed_offset());
    }
    None
}

/// Read an EXIF `SubSecTime*` value as whole milliseconds.
///
/// The tag holds the digits that follow the decimal point, not a count of
/// anything: `"07"` is 0.07s (70 ms) and `"9"` is 0.9s (900 ms). Reading it with
/// an integer parse — the obvious thing, and the thing that was missing here —
/// turns those into 7 ms and 9 ms, so the value is read positionally and padded
/// out to three places instead. Precision finer than a millisecond is dropped,
/// as that is all the output path carries.
///
/// Cameras pad the field with spaces or NULs, and some write nothing usable at
/// all. A non-numeric value is *no* sub-second reading rather than a zero one,
/// so it leaves whatever the date tag itself carried alone.
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
/// `DateTimeOriginal` is when the shutter fired and `CreateDate`
/// (`DateTimeDigitized`) when the image was written, the same instant on a
/// digital camera. `ModifyDate` (`DateTime`) is only the last time the file was
/// *changed*, so an edit in any photo tool moves it years past the capture — it
/// ranks below both. The GPS receiver's own reading is the last resort, below
/// every reading of the camera's clock but well above the file's mtime, which on
/// a copied archive is when the file was copied.
///
/// Each of the camera-clock tags carries its seconds whole; the fraction lives
/// in a separate tag, and each has its own. Pairing them up is the only way the
/// sub-second precision reaches the output path.
///
/// The pairing below reads wrong and is right — `nom_exif` uses ExifTool's
/// names, which are not the spec's, so only the tag IDs make it obvious:
///
/// | date tag                         | fraction tag                |
/// |----------------------------------|-----------------------------|
/// | `DateTimeOriginal`      `0x9003` | `SubSecTimeOriginal`  `0x9291` |
/// | `CreateDate`  (DateTimeDigitized, `0x9004`) | `SubSecTimeDigitized` `0x9292` |
/// | `ModifyDate`  (DateTime, `0x0132`)          | `SubSecTime`          `0x9290` |
///
/// Exif 2.32 defines `0x9290`–`0x9292` consecutively as "fractions of seconds
/// for the DateTime / DateTimeOriginal / DateTimeDigitized tag" respectively, so
/// the bare `SubSecTime` belongs to the bare `DateTime` — the tag ExifTool calls
/// `ModifyDate`. It is not a default or a catch-all for the other two.
///
/// GPS is not in that table because it is a different shape: its reading is
/// split date-and-time rather than seconds-and-fraction. See [`gps_datetime`].
pub(crate) fn best_guess_taken_exif(exif: &Option<PsExifInfo>) -> Option<String> {
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
        // Sub-second precision has exactly one source, so this replaces rather
        // than adds. No usable sub-second tag leaves the reading as it stands,
        // which keeps any fraction the date tag itself spelled out.
        let dt = field_value(exif, sub_sec_tag)
            .as_deref()
            .and_then(sub_sec_millis)
            .and_then(|millis| dt.with_nanosecond(millis * 1_000_000))
            .unwrap_or(dt);
        Some(dt.to_rfc3339())
    });
    from_camera_clock.or_else(|| gps_datetime(exif))
}

/// The GPS receiver's reading of when the shutter fired, from the
/// `GPSDateStamp` (`0x001d`) / `GPSTimeStamp` (`0x0007`) pair.
///
/// These two are one reading split across two tags, and they differ from the
/// camera's own clock in a way that matters: they are UTC by definition, so the
/// `+00:00` this ends up carrying is a fact about the file rather than the usual
/// assumption made for an offset-less reading.
///
/// Taking the date alone and calling it midnight — which is what this did before
/// the time tag was read — is the worst available answer, not a conservative
/// one. Midnight UTC sits exactly on the day boundary, so every photographer
/// west of UTC gets filed a day early, systematically. The time tag is right
/// there in the same IFD.
fn gps_datetime(exif: &PsExifInfo) -> Option<String> {
    let date = field_value(exif, ExifTag::GPSDateStamp)?;
    let date = date.trim();
    // A time that will not parse must not cost us the date it came with.
    field_value(exif, ExifTag::GPSTimeStamp)
        .and_then(|time| exif_datetime_parse(&format!("{date} {}", time.trim())))
        .or_else(|| exif_datetime_parse(date))
        .map(|dt| dt.to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::FileSystem;
    use crate::fs::OsFileSystem;
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
        // EXIF Orientation "1" decodes to the no-op transform.
        assert_eq!(exif_display_transform(&info), Some((false, 0)));
        Ok(())
    }

    /// Render a parsed EXIF date the way `best_guess_taken_exif` eventually
    /// does, so a test can assert on the string rather than a chrono value.
    fn exif_datetime_to_rfc3339(raw: &str) -> Option<String> {
        exif_datetime_parse(raw).map(|dt| dt.to_rfc3339())
    }

    /// Every spelling of an EXIF date this codebase has seen must come back as
    /// RFC 3339, because that is the only form `get_desired_media_path` parses -
    /// anything else lands the file in `undated/`.
    #[test]
    fn test_exif_datetime_to_rfc3339() {
        for (raw, expected) in [
            // Already an instant (file carried an OffsetTime tag): kept as-is.
            ("2023-08-05T19:59:55+12:00", "2023-08-05T19:59:55+12:00"),
            ("2023-08-05T07:59:55Z", "2023-08-05T07:59:55+00:00"),
            // Bare wall-clock reading, the common case: read as UTC.
            ("2015-04-18 11:10:44", "2015-04-18T11:10:44+00:00"),
            ("2015-04-18T11:10:44", "2015-04-18T11:10:44+00:00"),
            // Raw EXIF colon-separated date.
            ("2015:04:18 11:10:44", "2015-04-18T11:10:44+00:00"),
            // Sub-second precision.
            ("2015-04-18 11:10:44.939", "2015-04-18T11:10:44.939+00:00"),
            // GPSDateStamp: a day and nothing finer.
            ("2015:04:17", "2015-04-17T00:00:00+00:00"),
            ("2015-04-17", "2015-04-17T00:00:00+00:00"),
            // Surrounding whitespace, as EXIF strings are often space-padded.
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
            assert_eq!(exif_datetime_to_rfc3339(raw), None, "rejecting {raw:?}");
        }
    }

    /// Build an EXIF record from `(tag, value)` pairs, so a test can state just
    /// the tags it is about.
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

    /// An unparseable tag must not shadow a good one further down the list.
    #[test]
    fn test_best_guess_falls_through_unparseable_tag() {
        let exif = exif_with(&[
            (ExifTag::DateTimeOriginal, "0000:00:00 00:00:00"),
            (ExifTag::ModifyDate, "2015:04:18 11:10:44"),
        ]);
        assert_eq!(
            best_guess_taken_exif(&exif).as_deref(),
            Some("2015-04-18T11:10:44+00:00")
        );
    }

    /// `SubSecTime*` holds the digits after the decimal point, so it has to be
    /// read positionally. An integer parse makes `"07"` seven milliseconds when
    /// it means seventy, and `"9"` nine when it means nine hundred.
    #[test]
    fn test_sub_sec_millis_reads_a_fraction_not_an_integer() {
        for (raw, expected) in [
            ("9", Some(900)),
            ("07", Some(70)),
            ("93", Some(930)),
            ("939", Some(939)),
            ("00", Some(0)),
            ("0", Some(0)),
            // Finer than a millisecond: truncated, never rounded up into the
            // next millisecond.
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

    /// The reported bug: EXIF records the seconds and their fraction in two
    /// separate tags, and only the first was ever read, so every EXIF-dated file
    /// came out on a whole second.
    #[test]
    fn test_best_guess_applies_sub_second_tag() {
        let exif = exif_with(&[
            (ExifTag::DateTimeOriginal, "2015:04:18 11:10:44"),
            (ExifTag::SubSecTimeOriginal, "939"),
        ]);
        assert_eq!(
            best_guess_taken_exif(&exif).as_deref(),
            Some("2015-04-18T11:10:44.939+00:00")
        );
    }

    /// Each date tag has its own sub-second tag; crossing them would stamp one
    /// reading's fraction onto another's seconds.
    #[test]
    fn test_sub_second_tags_pair_with_their_own_date() {
        // SubSecTime belongs to ModifyDate, not to DateTimeOriginal.
        let exif = exif_with(&[
            (ExifTag::DateTimeOriginal, "2015:04:18 11:10:44"),
            (ExifTag::SubSecTime, "939"),
        ]);
        assert_eq!(
            best_guess_taken_exif(&exif).as_deref(),
            Some("2015-04-18T11:10:44+00:00"),
            "SubSecTime must not refine DateTimeOriginal"
        );

        // CreateDate is refined by SubSecTimeDigitized.
        let exif = exif_with(&[
            (ExifTag::CreateDate, "2015:04:18 11:10:44"),
            (ExifTag::SubSecTimeDigitized, "5"),
            (ExifTag::SubSecTimeOriginal, "939"),
        ]);
        assert_eq!(
            best_guess_taken_exif(&exif).as_deref(),
            Some("2015-04-18T11:10:44.500+00:00")
        );

        // ModifyDate is refined by SubSecTime.
        let exif = exif_with(&[
            (ExifTag::ModifyDate, "2015:04:18 11:10:44"),
            (ExifTag::SubSecTime, "07"),
        ]);
        assert_eq!(
            best_guess_taken_exif(&exif).as_deref(),
            Some("2015-04-18T11:10:44.070+00:00")
        );
    }

    /// An offset-carrying reading keeps its offset when the fraction is folded
    /// in — the sub-second tag says nothing about the zone.
    #[test]
    fn test_sub_second_keeps_recorded_offset() {
        let exif = exif_with(&[
            (ExifTag::DateTimeOriginal, "2023-08-05T19:59:55+12:00"),
            (ExifTag::SubSecTimeOriginal, "42"),
        ]);
        assert_eq!(
            best_guess_taken_exif(&exif).as_deref(),
            Some("2023-08-05T19:59:55.420+12:00")
        );
    }

    /// A dead-clock date is still rejected even when a perfectly good
    /// sub-second tag sits beside it, and the fraction does not follow the
    /// fall-through to the next tag.
    #[test]
    fn test_sub_second_does_not_rescue_or_follow_a_rejected_date() {
        let exif = exif_with(&[
            (ExifTag::DateTimeOriginal, "0000:00:00 00:00:00"),
            (ExifTag::SubSecTimeOriginal, "939"),
            (ExifTag::ModifyDate, "2015:04:18 11:10:44"),
        ]);
        assert_eq!(
            best_guess_taken_exif(&exif).as_deref(),
            Some("2015-04-18T11:10:44+00:00")
        );
    }

    /// The GPS reading has no sub-second tag of its own; nothing must leak into
    /// it from the ones that do.
    #[test]
    fn test_gps_date_stamp_takes_no_sub_second() {
        let exif = exif_with(&[
            (ExifTag::GPSDateStamp, "2015:04:17"),
            (ExifTag::GPSTimeStamp, "19:30:45"),
            (ExifTag::SubSecTimeOriginal, "939"),
            (ExifTag::SubSecTime, "939"),
            (ExifTag::SubSecTimeDigitized, "939"),
        ]);
        assert_eq!(
            best_guess_taken_exif(&exif).as_deref(),
            Some("2015-04-17T19:30:45+00:00")
        );
    }

    /// `GPSDateStamp` and `GPSTimeStamp` are one reading in two tags. Reading
    /// only the first buckets every GPS-dated file at midnight UTC — which is
    /// exactly the day boundary, so photographers west of UTC get filed a day
    /// early.
    #[test]
    fn test_gps_date_and_time_are_read_as_one_reading() {
        let exif = exif_with(&[
            (ExifTag::GPSDateStamp, "2015:04:17"),
            (ExifTag::GPSTimeStamp, "19:30:45"),
        ]);
        assert_eq!(
            best_guess_taken_exif(&exif).as_deref(),
            Some("2015-04-17T19:30:45+00:00")
        );

        // The seconds field is a rational, so it can carry a fraction.
        let exif = exif_with(&[
            (ExifTag::GPSDateStamp, "2015:04:17"),
            (ExifTag::GPSTimeStamp, "19:30:45.250"),
        ]);
        assert_eq!(
            best_guess_taken_exif(&exif).as_deref(),
            Some("2015-04-17T19:30:45.250+00:00")
        );
    }

    /// Any camera-clock reading outranks the GPS one, which is the last resort.
    #[test]
    fn test_camera_clock_outranks_gps() {
        let exif = exif_with(&[
            (ExifTag::ModifyDate, "2008:07:31 10:38:11"),
            (ExifTag::GPSDateStamp, "2015:04:17"),
            (ExifTag::GPSTimeStamp, "19:30:45"),
        ]);
        assert_eq!(
            best_guess_taken_exif(&exif).as_deref(),
            Some("2008-07-31T10:38:11+00:00")
        );
    }

    /// A time tag that will not parse must not cost us the date it came with —
    /// midnight is worse than the real time, but far better than no date at all,
    /// which would drop the file through to its mtime.
    #[test]
    fn test_unusable_gps_time_falls_back_to_the_date() {
        for time in ["", "   ", "not a time", "URationalArray[19/1 (19.0000)]"] {
            let exif = exif_with(&[
                (ExifTag::GPSDateStamp, "2015:04:17"),
                (ExifTag::GPSTimeStamp, time),
            ]);
            assert_eq!(
                best_guess_taken_exif(&exif).as_deref(),
                Some("2015-04-17T00:00:00+00:00"),
                "with GPSTimeStamp {time:?}"
            );
        }

        // And a date with no time tag at all is still a date.
        let exif = exif_with(&[(ExifTag::GPSDateStamp, "2015:04:17")]);
        assert_eq!(
            best_guess_taken_exif(&exif).as_deref(),
            Some("2015-04-17T00:00:00+00:00")
        );
    }

    /// `GPSTimeStamp` is three rationals, not a string. The generic rendering is
    /// unusable, so it is normalised on the way into the tag map.
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
        // Zero-padded, and all-zeros is a legitimate midnight rather than absence.
        assert_eq!(
            gps_time_of_day(&whole(0, 0, 0)).as_deref(),
            Some("00:00:00")
        );
        assert_eq!(
            gps_time_of_day(&whole(9, 5, 3)).as_deref(),
            Some("09:05:03")
        );
        // A fractional second, rendered only because there is one.
        assert_eq!(
            gps_time_of_day(&time((19, 1), (30, 1), (455, 10))).as_deref(),
            Some("19:30:45.500")
        );
        // Out of range for a clock.
        assert_eq!(gps_time_of_day(&whole(24, 0, 0)), None);
        assert_eq!(gps_time_of_day(&whole(0, 60, 0)), None);
        // A zero denominator has no value, and neither does the wrong shape.
        assert_eq!(gps_time_of_day(&time((19, 0), (30, 1), (45, 1))), None);
        assert_eq!(
            gps_time_of_day(&EntryValue::URationalArray(vec![URational::new(19, 1)])),
            None
        );
        assert_eq!(gps_time_of_day(&EntryValue::Text("19:30:45".into())), None);
    }

    /// End to end on a real file whose only date is the GPS pair: no
    /// `DateTimeOriginal`, no `CreateDate`, no `ModifyDate`. This is the branch
    /// that had no real-file coverage at all.
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

        let taken = best_guess_taken_exif(&Some(info));
        assert_eq!(taken.as_deref(), Some("2015-04-17T19:30:45+00:00"));
        assert_eq!(
            crate::media::get_desired_media_path("abc1234", &taken),
            "2015/04/17/1930-45000"
        );
        Ok(())
    }

    /// `ModifyDate` is the last time the file was *changed*, so an edit pushes it
    /// well past the capture. `CreateDate` records when the image was digitised
    /// and must be preferred over it.
    #[test]
    fn test_create_date_preferred_over_modify_date() {
        let exif = exif_with(&[
            (ExifTag::CreateDate, "2008:05:30 15:56:01"),
            (ExifTag::ModifyDate, "2008:07:31 10:38:11"),
        ]);
        assert_eq!(
            best_guess_taken_exif(&exif).as_deref(),
            Some("2008-05-30T15:56:01+00:00")
        );
    }

    /// The sub-second reading has to survive all the way to the name of the
    /// written file, which is the only place a user sees it.
    #[test]
    fn test_sub_second_reaches_the_output_path() {
        let exif = exif_with(&[
            (ExifTag::DateTimeOriginal, "2008:05:30 15:56:01"),
            (ExifTag::SubSecTimeOriginal, "07"),
        ]);
        let taken = best_guess_taken_exif(&exif);
        assert_eq!(
            crate::media::get_desired_media_path("abc1234", &taken),
            "2008/05/30/1556-01070"
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
        let path = crate::media::get_desired_media_path("abc1234", &Some(taken));
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
