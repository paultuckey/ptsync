use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime};
use nom_exif::{ExifIter, ExifIterEntry, ExifTag, MediaKind, MediaParser, MediaSource};
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

/// Normalise an EXIF date/time to RFC 3339, the form every other date source in
/// [`crate::media::best_guess_taken_dt`] returns and the only one
/// [`crate::media::get_desired_media_path`] can parse.
///
/// EXIF has no single spelling. Depending on whether the file carries an
/// `OffsetTime` tag, the parser hands back either a full RFC 3339 instant
/// (`2023-08-05T19:59:55+12:00`) or a bare local wall-clock reading
/// (`2015-04-18 11:10:44`); the raw tag form separates the date with colons
/// (`2015:04:18 11:10:44`), and `GPSDateStamp` is a date with no time at all
/// (`2015:04:17`).
///
/// A reading with no offset is taken as UTC. The camera's real zone is unknown
/// and unknowable from the file, and this keeps the wall-clock reading — which is
/// what the photographer saw and what the `yyyy/mm/dd` bucketing is for — intact.
pub(crate) fn exif_datetime_to_rfc3339(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    // Already carries an offset: keep the recorded instant exactly as it stands.
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Some(dt.to_rfc3339());
    }
    let normalised = dashed_date_separators(raw);
    for format in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(&normalised, format) {
            return Some(naive.and_utc().to_rfc3339());
        }
    }
    // `GPSDateStamp` pins down a day and nothing finer; midnight is the only
    // instant it justifies.
    if let Ok(date) = NaiveDate::parse_from_str(&normalised, "%Y-%m-%d") {
        return Some(date.and_time(NaiveTime::MIN).and_utc().to_rfc3339());
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
pub(crate) fn best_guess_taken_exif(exif: &Option<PsExifInfo>) -> Option<String> {
    let exif = exif.as_ref()?;
    [
        ExifTag::DateTimeOriginal,
        ExifTag::ModifyDate,
        ExifTag::GPSDateStamp,
    ]
    .into_iter()
    .find_map(|tag| {
        field_value(exif, tag)
            .as_deref()
            .and_then(exif_datetime_to_rfc3339)
    })
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
            gps: None,
            latitude: None,
            longitude: None,
        });
        assert_eq!(
            best_guess_taken_exif(&exif).as_deref(),
            Some("2015-04-18T11:10:44+00:00")
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
