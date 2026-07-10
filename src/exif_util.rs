use nom_exif::{ExifIter, ExifIterEntry, ExifTag, MediaKind, MediaParser, MediaSource};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use tracing::debug;

/*

Util file to help with exif parsing.

it's not the responsibility of this module to decide if exif data is valid or not, just to
parse it best as possible.

store in db as json

 */

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all(deserialize = "camelCase", serialize = "camelCase"))]
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

pub(crate) fn best_guess_taken_exif(exif: &Option<PsExifInfo>) -> Option<String> {
    match exif {
        Some(exif) => {
            if let Some(dt) = field_value(exif, ExifTag::DateTimeOriginal) {
                return Some(dt);
            }
            if let Some(dt) = field_value(exif, ExifTag::ModifyDate) {
                return Some(dt);
            }
            if let Some(dt) = field_value(exif, ExifTag::GPSDateStamp) {
                return Some(dt);
            }
            None
        }
        None => None,
    }
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

        // SubSecTimeOriginal
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
