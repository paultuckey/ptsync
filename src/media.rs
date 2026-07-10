use crate::db_cmd::HashInfo;
use crate::exif_util::{PsExifInfo, best_guess_taken_exif, parse_exif_info};
use crate::file_type::{
    AccurateFileType, MetadataType, QuickFileType, determine_file_type, file_ext_from_file_type,
    metadata_type,
};
use crate::supplemental_info::PsSupplementalInfo;
use crate::track_util::{PsTrackInfo, parse_track_info};
use crate::util::ScanInfo;
use anyhow::anyhow;
use chrono::{DateTime, Datelike, Timelike};
use serde::Serialize;
use std::io::{Read, Seek};
use tracing::warn;

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all(serialize = "camelCase"))]
pub(crate) struct MediaFileInfo {
    pub(crate) original_file_this_run: String,
    pub(crate) original_path: Vec<String>,
    pub(crate) quick_file_type: QuickFileType,
    pub(crate) exif_info: Option<PsExifInfo>,
    pub(crate) track_info: Option<PsTrackInfo>,
    pub(crate) accurate_file_type: AccurateFileType,
    pub(crate) hash_info: HashInfo,
    pub(crate) supp_info: Option<PsSupplementalInfo>,
    // Modified time of the file
    pub(crate) modified: Option<i64>,
    pub(crate) created: Option<i64>,
    // Size of the file in bytes
    pub(crate) file_size: u64,
}

#[derive(Debug)]
pub(crate) struct MediaFileDerivedInfo {
    /// Desired path relative to output directory, minus the dot and file extension (eg, 2025/09/10/1234-56-789)
    pub(crate) desired_media_path: Option<String>,
    /// Desired file extension (eg, jpg, mp4)
    pub(crate) desired_media_extension: String,
}

pub(crate) fn media_file_info_from_readable<R: Read + Seek>(
    si: &ScanInfo,
    reader: &mut R,
    supp_info: &Option<PsSupplementalInfo>,
    hash_info: &HashInfo,
) -> anyhow::Result<MediaFileInfo> {
    let name = &si.file_path;
    let guessed_ff = determine_file_type(&mut *reader, name)?;
    if guessed_ff == AccurateFileType::Unsupported {
        warn!("Not a valid media file {name:?}");
        return Err(anyhow!("File is not a valid media file"));
    }

    let mut exif_o = None;
    let mut track_o = None;
    match metadata_type(&guessed_ff) {
        MetadataType::ExifTags => {
            exif_o = parse_exif_info(&mut *reader)?;
        }
        MetadataType::Track => {
            track_o = parse_track_info(&mut *reader)?;
        }
        MetadataType::NoMetadata => {}
    }
    let hash_info = hash_info.clone();

    let media_file_info = MediaFileInfo {
        original_file_this_run: name.clone(),
        original_path: vec![name.clone()],
        accurate_file_type: guessed_ff.clone(),
        quick_file_type: si.quick_file_type.clone(),
        exif_info: exif_o.clone(),
        track_info: track_o.clone(),
        hash_info,
        supp_info: supp_info.clone(),
        modified: si.modified_datetime,
        created: si.created_datetime,
        file_size: si.file_size,
    };
    Ok(media_file_info)
}

pub(crate) fn media_file_derived_from_media_info(
    media_info: &MediaFileInfo,
) -> anyhow::Result<MediaFileDerivedInfo> {
    let ext = file_ext_from_file_type(&media_info.accurate_file_type);
    let guessed_datetime = best_guess_taken_dt(media_info);
    let short_checksum = &media_info.hash_info.short_checksum;
    let desired_media_path_o = Some(get_desired_media_path(short_checksum, &guessed_datetime));
    let media_file_info = MediaFileDerivedInfo {
        desired_media_path: desired_media_path_o.clone(),
        desired_media_extension: ext,
    };
    Ok(media_file_info)
}

/// Best guess at the date the photo was taken from messy optional data, in the order of preference:
/// 1. SupplementalInfo photo_taken_time
/// 2. EXIF DateTimeOriginal
/// 3. EXIF DateTime
/// 4. EXIF GPSDateStamp - only accurate up to minute
/// 5. Track creation_time - the embedded capture time for videos (already rfc3339)
/// 6. SupplementalInfo creation_time
/// 7. File modified time
///   - no timezone info, unreliable in zips, somewhat unreliable in directories due to file
///     copying / syncing not preserving, only use as second to last resort
/// 8. File creation time
///   - no timezone info, unavailable in zips, somewhat unreliable in directories due to file
///     copying / syncing not preserving, only use as a last resort
///
/// Result returned as ISO 8601 string
pub(crate) fn best_guess_taken_dt(info: &MediaFileInfo) -> Option<String> {
    if let Some(dt) = info
        .supp_info
        .as_ref()
        .and_then(|si| si.photo_taken_time.as_ref())
        .and_then(|si_dt| si_dt.timestamp_s_as_iso_8601())
    {
        return Some(dt);
    }
    let time_taken_from_exif = best_guess_taken_exif(&info.exif_info);
    if let Some(dt) = time_taken_from_exif {
        return Some(dt);
    }
    // Videos have no EXIF; their capture time lives in the track metadata, which
    // is the embedded-metadata equivalent of EXIF DateTimeOriginal for images.
    if let Some(dt) = info
        .track_info
        .as_ref()
        .and_then(|ti| ti.creation_time.clone())
    {
        return Some(dt);
    }
    if let Some(dt) = info
        .supp_info
        .as_ref()
        .and_then(|si| si.creation_time.as_ref())
        .and_then(|si_dt| si_dt.timestamp_s_as_iso_8601())
    {
        return Some(dt);
    }
    if let Some(dt) = info.modified {
        let o = crate::util::timestamp_to_rfc3339(dt);
        if let Some(dt) = o {
            return Some(dt);
        }
    }
    if let Some(dt) = info.created {
        let o = crate::util::timestamp_to_rfc3339(dt);
        if let Some(dt) = o {
            return Some(dt);
        }
    }
    None
}

/// Best guess at where the media was taken, as `(latitude, longitude)`, from the
/// messy optional data, in order of preference:
/// 1. EXIF GPS (embedded in images)
/// 2. Track ISO 6709 GPS (embedded in videos)
/// 3. SupplementalInfo `geo_data_exif` (Google's copy of the EXIF coordinates)
/// 4. SupplementalInfo `geo_data` (Google's own, often absent)
///
/// Embedded metadata is preferred over Google's supplemental copies. A `(0, 0)`
/// pair is treated as absent at every source: EXIF and Takeout both write zeros
/// when they have no fix rather than omitting the value.
pub(crate) fn best_guess_lat_long(info: &MediaFileInfo) -> Option<(f64, f64)> {
    use crate::util::non_zero_coords;
    if let Some(exif) = &info.exif_info
        && let Some(coords) = non_zero_coords(exif.latitude, exif.longitude)
    {
        return Some(coords);
    }
    if let Some(track) = &info.track_info
        && let Some(coords) = track.lat_long()
    {
        return Some(coords);
    }
    if let Some(supp) = &info.supp_info {
        for geo in [supp.geo_data_exif.as_ref(), supp.geo_data.as_ref()] {
            if let Some(geo) = geo
                && let Some(coords) = non_zero_coords(geo.latitude, geo.longitude)
            {
                return Some(coords);
            }
        }
    }
    None
}

/// `yyyy/mm/dd/hhmm-ssms`
/// OR `undated/checksum`
pub(crate) fn get_desired_media_path(
    short_checksum: &str,
    media_datetime: &Option<String>,
) -> String {
    let date_dir;
    let name;
    if let Some(dt_s) = media_datetime {
        let dt_r = DateTime::parse_from_rfc3339(dt_s);
        match dt_r {
            Ok(dt) => {
                date_dir = format!("{}/{:0>2}/{:0>2}", dt.year(), dt.month(), dt.day());
                name = format!(
                    "{:0>2}{:0>2}-{:0>2}{:0>3}",
                    dt.hour(),
                    dt.minute(),
                    dt.second(),
                    dt.timestamp_subsec_millis()
                );
            }
            Err(_) => {
                warn!("Could not parse datetime: {dt_s:?}");
                date_dir = "undated".to_string();
                name = short_checksum.to_string();
            }
        }
    } else {
        date_dir = "undated".to_string();
        name = short_checksum.to_string();
    }
    format!("{date_dir}/{name}")
}

#[cfg(test)]
impl MediaFileInfo {
    pub(crate) fn new_for_test() -> Self {
        MediaFileInfo {
            original_file_this_run: "".to_string(),
            original_path: vec![],
            quick_file_type: QuickFileType::Media,
            exif_info: None,
            track_info: None,
            accurate_file_type: AccurateFileType::Jpg,
            hash_info: HashInfo {
                short_checksum: "tsc".to_string(),
                long_checksum: "tlc".to_string(),
            },
            supp_info: None,
            modified: None,
            created: None,
            file_size: 0,
        }
    }
}

#[cfg(test)]
impl MediaFileDerivedInfo {
    pub(crate) fn new_for_test(
        desired_media_path: Option<String>,
        desired_media_extension: &str,
    ) -> Self {
        MediaFileDerivedInfo {
            desired_media_path,
            desired_media_extension: desired_media_extension.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{FileSystem, OsFileSystem};

    #[test]
    fn test_best_guess_taken_dt_timestamps() -> anyhow::Result<()> {
        use anyhow::anyhow;
        let mut info = MediaFileInfo::new_for_test();
        // 1000000000000 ms = 2001-09-09T01:46:40Z
        let ts = 1000000000000;

        // Test created timestamp
        info.created = Some(ts);
        info.modified = None;
        let dt =
            best_guess_taken_dt(&info).ok_or_else(|| anyhow!("Should have a date from created"))?;
        assert_eq!(dt, "2001-09-09T01:46:40+00:00");

        // Test modified timestamp
        info.created = None;
        info.modified = Some(ts);
        let dt = best_guess_taken_dt(&info)
            .ok_or_else(|| anyhow!("Should have a date from modified"))?;
        assert_eq!(dt, "2001-09-09T01:46:40+00:00");

        // When both are present, modified wins over created (created is the very
        // last resort as it is unavailable in zips).
        info.modified = Some(ts);
        info.created = Some(1_600_000_000_000); // 2020-09-13T12:26:40Z
        let dt = best_guess_taken_dt(&info)
            .ok_or_else(|| anyhow!("Should have a date when both present"))?;
        assert_eq!(dt, "2001-09-09T01:46:40+00:00");
        Ok(())
    }

    #[test]
    fn test_best_guess_taken_dt_video_track() {
        use crate::track_util::PsTrackInfo;

        let track = |ct: &str| PsTrackInfo {
            width: None,
            height: None,
            creation_time: Some(ct.to_string()),
            duration_ms: None,
            make: None,
            model: None,
            software: None,
            author: None,
            gps_iso_6709: None,
        };

        // A video's embedded track creation time is used when present...
        let mut info = MediaFileInfo::new_for_test();
        info.track_info = Some(track("2024-04-18T11:24:26+00:00"));
        assert_eq!(
            best_guess_taken_dt(&info).as_deref(),
            Some("2024-04-18T11:24:26+00:00")
        );

        // ...and is preferred over the file created/modified fallbacks.
        info.created = Some(1_000_000_000_000);
        info.modified = Some(1_000_000_000_000);
        assert_eq!(
            best_guess_taken_dt(&info).as_deref(),
            Some("2024-04-18T11:24:26+00:00")
        );
    }

    #[test]
    fn test_best_guess_lat_long_precedence() {
        use crate::exif_util::PsExifInfo;
        use crate::supplemental_info::{PsSupplementalInfo, SupplementalInfoGeoData};
        use std::collections::HashMap;

        let exif = |lat, long| PsExifInfo {
            tags: HashMap::new(),
            gps: None,
            latitude: lat,
            longitude: long,
        };
        let geo = |lat: f64, long: f64| SupplementalInfoGeoData {
            latitude: Some(lat),
            longitude: Some(long),
        };
        let supp = |geo_data_exif, geo_data| PsSupplementalInfo {
            geo_data,
            geo_data_exif,
            people: vec![],
            photo_taken_time: None,
            creation_time: None,
        };

        // EXIF wins over supplemental data.
        let mut info = MediaFileInfo::new_for_test();
        info.exif_info = Some(exif(Some(1.0), Some(2.0)));
        info.supp_info = Some(supp(Some(geo(3.0, 4.0)), Some(geo(5.0, 6.0))));
        assert_eq!(best_guess_lat_long(&info), Some((1.0, 2.0)));

        // No EXIF coords: geo_data_exif is preferred over geo_data.
        info.exif_info = None;
        assert_eq!(best_guess_lat_long(&info), Some((3.0, 4.0)));

        // Only geo_data present.
        info.supp_info = Some(supp(None, Some(geo(5.0, 6.0))));
        assert_eq!(best_guess_lat_long(&info), Some((5.0, 6.0)));

        // (0, 0) EXIF is treated as absent, so we fall through to supplemental.
        info.exif_info = Some(exif(Some(0.0), Some(0.0)));
        info.supp_info = Some(supp(Some(geo(7.0, 8.0)), None));
        assert_eq!(best_guess_lat_long(&info), Some((7.0, 8.0)));

        // Nothing usable anywhere.
        info.exif_info = None;
        info.supp_info = Some(supp(Some(geo(0.0, 0.0)), None));
        assert_eq!(best_guess_lat_long(&info), None);
    }

    #[test]
    fn test_best_guess_lat_long_video_track() {
        use crate::exif_util::PsExifInfo;
        use crate::track_util::PsTrackInfo;
        use std::collections::HashMap;

        let track = |gps: &str| PsTrackInfo {
            width: None,
            height: None,
            creation_time: None,
            duration_ms: None,
            make: None,
            model: None,
            software: None,
            author: None,
            gps_iso_6709: Some(gps.to_string()),
        };

        // A video with only embedded track GPS: coordinates come from ISO 6709.
        let mut info = MediaFileInfo::new_for_test();
        info.exif_info = None;
        info.track_info = Some(track("+27.5916+086.5640/"));
        assert_eq!(best_guess_lat_long(&info), Some((27.5916, 86.5640)));

        // Embedded EXIF still wins over the track string when both exist.
        info.exif_info = Some(PsExifInfo {
            tags: HashMap::new(),
            gps: None,
            latitude: Some(1.0),
            longitude: Some(2.0),
        });
        assert_eq!(best_guess_lat_long(&info), Some((1.0, 2.0)));
    }

    #[test]
    fn test_desired_media_path() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        use crate::util::checksum_bytes;

        let c = OsFileSystem::new("test");
        let mut reader = c.open("Canon_40D.jpg")?;
        let short_checksum = checksum_bytes(&mut reader)?.short_checksum;

        assert_eq!(
            get_desired_media_path(&short_checksum, &None),
            "undated/6bfdabd".to_string()
        );
        assert_eq!(
            get_desired_media_path(&short_checksum, &Some("2008-05-30T15:56:01Z".to_string())),
            "2008/05/30/1556-01000".to_string()
        );
        assert_eq!(
            get_desired_media_path(
                &short_checksum,
                &Some("2008-05-30T15:56:01.009Z".to_string())
            ),
            "2008/05/30/1556-01009".to_string()
        );
        Ok(())
    }

    #[test]
    #[ignore]
    fn test_perf_benchmark_zip_read() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        // Ensure test file exists
        let zip_path = "test/Canon_40D.jpg.zip";
        let fs = crate::fs::ZipFileSystem::new(zip_path)?;

        let file_path = "Canon_40D.jpg";
        let si = ScanInfo::new(file_path.to_string(), None, None, 0);
        let hash_info = HashInfo {
            short_checksum: "dummy".to_string(),
            long_checksum: "dummy".to_string(),
        };

        let start = std::time::Instant::now();
        for _ in 0..100 {
            let mut reader = fs.open(file_path)?;
            let _ = media_file_info_from_readable(&si, &mut reader, &None, &hash_info);
        }
        let duration = start.elapsed();
        println!("Time taken for 100 iterations: {:?}", duration);
        Ok(())
    }
}
