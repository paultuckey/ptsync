use nom_exif::{MediaKind, MediaParser, MediaSource, TrackInfo, TrackInfoTag};
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom};
use tracing::{info, warn};

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all(deserialize = "camelCase", serialize = "camelCase"))]
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
    pub gps_iso_6709: Option<String>,
}

impl PsTrackInfo {
    /// Decode the embedded ISO 6709 location string into decimal
    /// `(latitude, longitude)`, treating the `(0, 0)` sentinel as absent.
    pub(crate) fn lat_long(&self) -> Option<(f64, f64)> {
        let (lat, long) = parse_iso6709(self.gps_iso_6709.as_deref()?)?;
        crate::util::non_zero_coords(Some(lat), Some(long))
    }
}

/// Parse an ISO 6709 point string in decimal-degree form, e.g.
/// `+27.5916+086.5640/` or `-21.6303-152.2605+077.000CRSWGS_84/`, into
/// `(latitude, longitude)`. The `+`/`-` sign prefixes delimit the fields;
/// latitude comes first, longitude second, and any altitude or CRS suffix is
/// ignored. Returns `None` if fewer than two numeric fields parse.
fn parse_iso6709(s: &str) -> Option<(f64, f64)> {
    let mut fields: Vec<String> = Vec::new();
    let mut cur = String::new();
    for ch in s.trim().chars() {
        match ch {
            '+' | '-' => {
                if !cur.is_empty() {
                    fields.push(std::mem::take(&mut cur));
                }
                cur.push(ch);
            }
            '0'..='9' | '.' => cur.push(ch),
            // A solidus or CRS designator (e.g. "CRSWGS_84") ends the coordinates.
            _ => break,
        }
    }
    if !cur.is_empty() {
        fields.push(cur);
    }
    let lat = fields.first()?.parse::<f64>().ok()?;
    let long = fields.get(1)?.parse::<f64>().ok()?;
    Some((lat, long))
}

pub fn parse_track_info<R: Read + Seek>(mut reader: R) -> anyhow::Result<Option<PsTrackInfo>> {
    reader.seek(SeekFrom::Start(0))?;
    let ms_r = MediaSource::seekable(reader);
    let Ok(ms) = ms_r else {
        warn!("Failed to read track media source");
        return Ok(None);
    };
    if ms.kind() != MediaKind::Track {
        return Ok(None);
    }
    let mut parser = MediaParser::new();
    let info: nom_exif::Result<TrackInfo> = parser.parse_track(ms);

    match info {
        Err(e) => {
            warn!("Failed to parse track metadata: {:?}", e);
            Ok(None)
        }
        Ok(info) => {
            let ti = PsTrackInfo {
                width: parse_to_o_u64(&info.get(TrackInfoTag::Width)),
                height: parse_to_o_u64(&info.get(TrackInfoTag::Height)),
                creation_time: parse_to_o_s(&info.get(TrackInfoTag::CreateDate)),
                duration_ms: parse_to_o_u64(&info.get(TrackInfoTag::DurationMs)),
                make: parse_to_o_s(&info.get(TrackInfoTag::Make)),
                model: parse_to_o_s(&info.get(TrackInfoTag::Model)),
                software: parse_to_o_s(&info.get(TrackInfoTag::Software)),
                author: parse_to_o_s(&info.get(TrackInfoTag::Author)),
                gps_iso_6709: parse_to_o_s(&info.get(TrackInfoTag::GpsIso6709)),
            };
            info.iter()
                // filter out known tags from above
                .filter(|(tag, _)| {
                    !matches!(
                        tag,
                        TrackInfoTag::Width
                            | TrackInfoTag::Height
                            | TrackInfoTag::CreateDate
                            | TrackInfoTag::DurationMs
                            | TrackInfoTag::Make
                            | TrackInfoTag::Model
                            | TrackInfoTag::Software
                            | TrackInfoTag::Author
                            | TrackInfoTag::GpsIso6709
                    )
                })
                .for_each(|info| {
                    info!("Track Additional Metadata: {} = {}", info.0, info.1);
                });
            Ok(Some(ti))
        }
    }
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
    use crate::util::scan_fs;
    use std::path::Path;

    #[test]
    fn test_parse_track() -> anyhow::Result<()> {
        use anyhow::anyhow;
        crate::test_util::setup_log();
        let c = OsFileSystem::new("test");
        let reader = c.open("Hello.mp4")?;
        let meta =
            parse_track_info(reader)?.ok_or_else(|| anyhow!("Failed to parse track info"))?;
        assert_eq!(meta.width, Some(854));
        assert_eq!(meta.height, Some(480));
        assert_eq!(meta.duration_ms, Some(5000));
        assert_eq!(
            meta.creation_time,
            Some("2024-04-18T11:24:26+00:00".to_string())
        );
        Ok(())
    }

    fn track_with_gps(gps: Option<&str>) -> PsTrackInfo {
        PsTrackInfo {
            width: None,
            height: None,
            creation_time: None,
            duration_ms: None,
            make: None,
            model: None,
            software: None,
            author: None,
            gps_iso_6709: gps.map(str::to_string),
        }
    }

    #[test]
    fn test_iso6709_lat_long() {
        // Latitude + longitude only.
        assert_eq!(
            parse_iso6709("+27.5916+086.5640/"),
            Some((27.5916, 86.5640))
        );
        // Negative longitude, trailing altitude and CRS suffix are ignored.
        assert_eq!(
            parse_iso6709("-21.6303-152.2605+077.000CRSWGS_84/"),
            Some((-21.6303, -152.2605))
        );
        // Not enough fields.
        assert_eq!(parse_iso6709("+27.5916/"), None);

        // lat_long() applies the (0, 0) null-island rule and handles absence.
        assert_eq!(
            track_with_gps(Some("+40.7128-074.0060/")).lat_long(),
            Some((40.7128, -74.0060))
        );
        assert_eq!(track_with_gps(Some("+0.0+0.0/")).lat_long(), None);
        assert_eq!(track_with_gps(None).lat_long(), None);
    }

    /// For research scal all MP4 files in input/ directory and look for unknown tags
    #[test]
    #[ignore]
    fn test_all_mp4s() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let c = OsFileSystem::new("input");
        for si in scan_fs(&c) {
            let path = Path::new(&si.file_path);
            if path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("mp4"))
            {
                let reader = c.open(path.to_string_lossy().as_ref())?;
                let _ = parse_track_info(reader);
            }
        }
        Ok(())
    }
}
