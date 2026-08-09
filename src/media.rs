use crate::exif_util::{PsExifInfo, best_guess_taken_exif, parse_exif_info};
use crate::file_type::{
    AccurateFileType, MetadataType, QuickFileType, determine_file_type, file_ext_from_file_type,
    metadata_type,
};
use crate::supplemental_info::PsSupplementalInfo;
use crate::track_util::{PsTrackInfo, parse_track_info};
use crate::util::{HashInfo, OutputTZ, ScanInfo};
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
    /// Unix milliseconds.
    pub(crate) modified: Option<i64>,
    /// Unix milliseconds.
    pub(crate) created: Option<i64>,
    pub(crate) file_size: u64,
}

#[derive(Debug)]
pub(crate) struct MediaFileDerivedInfo {
    /// Relative to the output directory, without extension (eg `2025/09/10/1234-56-789`).
    pub(crate) desired_media_path: Option<String>,
    /// Without the dot (eg `jpg`).
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
    tz: OutputTZ,
) -> anyhow::Result<MediaFileDerivedInfo> {
    let ext = file_ext_from_file_type(&media_info.accurate_file_type);
    let guessed_datetime = best_guess_taken_dt(media_info, tz);
    let short_checksum = &media_info.hash_info.short_checksum;
    let desired_media_path_o = Some(get_desired_media_path(short_checksum, &guessed_datetime));
    let media_file_info = MediaFileDerivedInfo {
        desired_media_path: desired_media_path_o.clone(),
        desired_media_extension: ext,
    };
    Ok(media_file_info)
}

/// Best guess at when the photo was taken, from messy optional data, in order of
/// preference:
/// 1. SupplementalInfo `photo_taken_time`
/// 2. EXIF, ranked within [`best_guess_taken_exif`]
/// 3. Track `creation_time` — the embedded capture time for videos
/// 4. SupplementalInfo `creation_time`
/// 5. File modified time, then created time — no zone, unreliable in zips and
///    not preserved by copying, so both are last resorts
///
/// Returned as RFC 3339, since [`get_desired_media_path`] parses it back and
/// files anything it cannot read under `undated/`.
///
/// The offset on that string decides the output directory, and the sources reach
/// it by opposite routes: wall-clock readings (2, 3) are the numbers the camera
/// showed with the zone unrecorded, so `+00:00` is a placeholder and the digits
/// pass through unshifted; instants (1, 4, 5) are converted to `tz` first — see
/// [`crate::util::OutputTZ`]. Nothing downstream can tell the two apart once they
/// are strings, so the conversion belongs here rather than at the bucketing.
pub(crate) fn best_guess_taken_dt(info: &MediaFileInfo, tz: OutputTZ) -> Option<String> {
    if let Some(dt) = info
        .supp_info
        .as_ref()
        .and_then(|si| si.photo_taken_time.as_ref())
        .and_then(|si_dt| si_dt.timestamp_s_as_iso_8601(tz))
    {
        return Some(dt);
    }
    let time_taken_from_exif = best_guess_taken_exif(&info.exif_info, tz);
    if let Some(dt) = time_taken_from_exif {
        return Some(dt);
    }
    // Videos have no EXIF; their capture time lives in the track metadata.
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
        .and_then(|si_dt| si_dt.timestamp_s_as_iso_8601(tz))
    {
        return Some(dt);
    }
    if let Some(dt) = info.modified {
        let o = tz.render_millis(dt);
        if let Some(dt) = o {
            return Some(dt);
        }
    }
    if let Some(dt) = info.created {
        let o = tz.render_millis(dt);
        if let Some(dt) = o {
            return Some(dt);
        }
    }
    None
}

/// Best guess at `(latitude, longitude)`. Embedded metadata — EXIF for images,
/// ISO 6709 track data for videos — is preferred over Google's supplemental
/// copies, and `geo_data` over `geo_data_exif`.
///
/// That last order only matters when both are present and disagree, since a
/// missing or `(0, 0)` pair falls through either way. When they do disagree,
/// `geo_data` is Google Photos' current location — including any the user
/// corrected by hand — while `geo_data_exif` is a copy of the file's own EXIF,
/// the source already tried and preferred above.
///
/// Each source states the `(0, 0)`-is-absent rule in its own `lat_long`, so this
/// is only the precedence between them.
pub(crate) fn best_guess_lat_long(info: &MediaFileInfo) -> Option<(f64, f64)> {
    let supp = info.supp_info.as_ref();
    info.exif_info
        .as_ref()
        .and_then(PsExifInfo::lat_long)
        .or_else(|| info.track_info.as_ref().and_then(PsTrackInfo::lat_long))
        .or_else(|| supp.and_then(|s| s.geo_data.as_ref()?.lat_long()))
        .or_else(|| supp.and_then(|s| s.geo_data_exif.as_ref()?.lat_long()))
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
                // A leap second reads as 1000-1999 ms and would widen the
                // fixed-width name by a digit.
                name = format!(
                    "{:0>2}{:0>2}-{:0>2}{:0>3}",
                    dt.hour(),
                    dt.minute(),
                    dt.second(),
                    dt.timestamp_subsec_millis().min(999)
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
    use crate::test_util::tz;

    /// Rendered at UTC — what `DateTime::from_timestamp_millis` gives unaided —
    /// this instant would be filed under the 9th at 01:46 instead, so the
    /// bucketed path is asserted alongside the reading.
    #[test]
    fn test_filesystem_times_are_read_in_the_output_tz() -> anyhow::Result<()> {
        use anyhow::anyhow;
        // 1000000000000 ms = 2001-09-09T01:46:40Z, which the output zone reads as
        // a quarter to two in the afternoon.
        let ts = 1_000_000_000_000;
        const AT_ZONE: &str = "2001-09-09T13:46:40+12:00";

        // created, modified — modified wins, since zips carry no creation time
        let cases: [(Option<i64>, Option<i64>); 3] = [
            (Some(ts), None),
            (None, Some(ts)),
            (Some(1_600_000_000_000), Some(ts)), // created is 2020-09-13T12:26:40Z
        ];

        for (created, modified) in cases {
            let mut info = MediaFileInfo::new_for_test();
            info.created = created;
            info.modified = modified;
            let taken = best_guess_taken_dt(&info, tz())
                .ok_or_else(|| anyhow!("no date from {created:?}/{modified:?}"))?;
            assert_eq!(taken, AT_ZONE, "created {created:?}, modified {modified:?}");
            assert_eq!(
                get_desired_media_path("abc1234", &Some(taken)),
                "2001/09/09/1346-40000"
            );
        }
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

        let mut info = MediaFileInfo::new_for_test();
        info.track_info = Some(track("2024-04-18T11:24:26+00:00"));
        assert_eq!(
            best_guess_taken_dt(&info, tz()).as_deref(),
            Some("2024-04-18T11:24:26+00:00")
        );

        // Preferred over the file created/modified fallbacks.
        info.created = Some(1_000_000_000_000);
        info.modified = Some(1_000_000_000_000);
        assert_eq!(
            best_guess_taken_dt(&info, tz()).as_deref(),
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

        // geo_data over geo_data_exif when the two disagree.
        info.exif_info = None;
        assert_eq!(best_guess_lat_long(&info), Some((5.0, 6.0)));

        // Either one alone is used, so the order above is unobservable here.
        info.supp_info = Some(supp(None, Some(geo(5.0, 6.0))));
        assert_eq!(best_guess_lat_long(&info), Some((5.0, 6.0)));
        info.supp_info = Some(supp(Some(geo(3.0, 4.0)), None));
        assert_eq!(best_guess_lat_long(&info), Some((3.0, 4.0)));

        // A (0, 0) geo_data is absent, so geo_data_exif still answers.
        info.supp_info = Some(supp(Some(geo(3.0, 4.0)), Some(geo(0.0, 0.0))));
        assert_eq!(best_guess_lat_long(&info), Some((3.0, 4.0)));

        // (0, 0) is absent, so this falls through to supplemental.
        info.exif_info = Some(exif(Some(0.0), Some(0.0)));
        info.supp_info = Some(supp(Some(geo(7.0, 8.0)), None));
        assert_eq!(best_guess_lat_long(&info), Some((7.0, 8.0)));

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

        let mut info = MediaFileInfo::new_for_test();
        info.exif_info = None;
        info.track_info = Some(track("+27.5916+086.5640/"));
        assert_eq!(best_guess_lat_long(&info), Some((27.5916, 86.5640)));

        // EXIF still wins over the track string when both exist.
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
}
