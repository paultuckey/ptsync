use crate::exif_util::{
    PsExifInfo, capture_reading, exif_datetime_parse, fallback_taken_exif, parse_exif_info,
};
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
            track_o = parse_track_info(&mut *reader, name)?;
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

/// Best guess at when the photo was taken: the camera's own reading of when the
/// shutter fired, and failing that the best substitute the file offers.
///
/// 1. EXIF capture clock — see [`capture_reading`]
/// 2. Track `creation_time` — a video's own capture time, which is an instant
///    rather than a reading; see [`track_instant`]
/// 3. SupplementalInfo `photo_taken_time` — Google's record of the capture
/// 4. The remaining EXIF dates, ranked within [`fallback_taken_exif`]
/// 5. SupplementalInfo `creation_time` — when Google received the upload
/// 6. File modified time, then created time — no zone, unreliable in zips and
///    not preserved by copying, so both are last resorts
///
/// Returned as RFC 3339, since [`get_desired_media_path`] parses it back and
/// files anything it cannot read under `undated/`.
///
/// The archive is laid out on the photographer's wall clock, and source 1 is the
/// only one that *is* one: the numbers the camera showed. It is filed unshifted,
/// which is what keeps a photo's path the same wherever the sync is run. Every
/// other source is an instant with no zone of its own and can only be converted
/// into `tz` — see [`crate::util::OutputTZ`] — filing the file where the archive
/// was built rather than where it was taken. That is the compromise this order
/// exists to minimise, and it is why a reading outranks an instant however
/// authoritative the instant's source.
///
/// Video is the exception that proves the rule: no container records the offset
/// the camera was set to, so even a video's own capture time has to be converted
/// like any other instant.
///
/// The cost is that a date corrected in the Google Photos UI does not move the
/// file, because such a correction is written only to `photo_taken_time` and
/// never back into the image. It stays visible in the note and the database. The
/// name is meant to be durable for decades, so it is built from the file's own
/// bytes rather than from a database that can be re-exported differently
/// tomorrow — and that also makes the path independent of the machine the sync
/// runs on.
pub(crate) fn best_guess_taken_dt(info: &MediaFileInfo, tz: OutputTZ) -> Option<String> {
    if let Some(reading) = capture_reading(&info.exif_info) {
        return Some(reading.to_rfc3339());
    }
    if let Some(dt) = info
        .track_info
        .as_ref()
        .and_then(|ti| ti.creation_time.as_deref())
        .and_then(|raw| track_instant(raw, tz))
    {
        return Some(dt);
    }
    if let Some(dt) = info
        .supp_info
        .as_ref()
        .and_then(|si| si.photo_taken_time.as_ref())
        .and_then(|si_dt| si_dt.timestamp_s_as_iso_8601(tz))
    {
        return Some(dt);
    }
    if let Some(dt) = fallback_taken_exif(&info.exif_info, tz) {
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

/// A video's embedded capture time, converted into the output zone.
///
/// Unlike EXIF, MP4 and QuickTime define `creation_time` as seconds from an
/// epoch in **UTC**, so it is an instant and not the reading the videographer
/// saw. Filing it unshifted puts an evening video 12 or 13 hours out and often
/// on the wrong day — `VID_20190101_124352.mp4` records `2018:12:31 23:44:02`,
/// which is a quarter to one on New Year's Day at +13:00, one year later than
/// the digits alone suggest.
///
/// `nom_exif` returns the value either way round: naive for MP4, and for some
/// QuickTime files already localized to *this machine*. Both are put back on a
/// common footing here — read as an instant, rendered in `tz` — so the answer
/// depends on the output zone rather than on which spelling the container used.
///
/// The zone dependence is unavoidable: no video container records the offset the
/// camera was set to, so unlike a shutter reading there is nothing to file it by
/// except a zone chosen from outside the file.
fn track_instant(raw: &str, tz: OutputTZ) -> Option<String> {
    Some(tz.render(exif_datetime_parse(raw)?.to_utc()))
}

/// Best guess at `(latitude, longitude)`. Embedded metadata — EXIF for images,
/// ISO 6709 track data for videos — is preferred over Google's supplemental
/// copies, of which `geo_data_exif` is the more trustworthy.
///
/// `(0, 0)` is treated as absent everywhere: EXIF and Takeout both write zeros
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

/// The Apple *content identifier* this file carries, if any: a UUID an iPhone
/// writes into both halves of a Live Photo, so the still and its video can be
/// recognised as one thing. It lives in the still's maker note and in the
/// video's QuickTime metadata, and never in both places on one file.
///
/// See [`crate::live_photo`] for what is done with it.
pub(crate) fn content_identifier(info: &MediaFileInfo) -> Option<String> {
    if let Some(exif) = &info.exif_info
        && let Some(id) = &exif.content_identifier
    {
        return Some(id.clone());
    }
    info.track_info
        .as_ref()
        .and_then(|track| track.content_identifier.clone())
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

    #[test]
    fn test_best_guess_taken_dt_timestamps() -> anyhow::Result<()> {
        use anyhow::anyhow;
        let mut info = MediaFileInfo::new_for_test();
        // 1000000000000 ms = 2001-09-09T01:46:40Z, which the output zone reads as
        // a quarter to two in the afternoon.
        let ts = 1000000000000;
        const AT_ZONE: &str = "2001-09-09T13:46:40+12:00";

        info.created = Some(ts);
        info.modified = None;
        let dt = best_guess_taken_dt(&info, tz()).ok_or_else(|| anyhow!("no date from created"))?;
        assert_eq!(dt, AT_ZONE);

        info.created = None;
        info.modified = Some(ts);
        let dt =
            best_guess_taken_dt(&info, tz()).ok_or_else(|| anyhow!("no date from modified"))?;
        assert_eq!(dt, AT_ZONE);

        // Modified wins over created, which is unavailable in zips.
        info.modified = Some(ts);
        info.created = Some(1_600_000_000_000); // 2020-09-13T12:26:40Z
        let dt =
            best_guess_taken_dt(&info, tz()).ok_or_else(|| anyhow!("no date when both present"))?;
        assert_eq!(dt, AT_ZONE);

        // Bucketed on that reading. Rendered at UTC — what
        // `DateTime::from_timestamp_millis` gives unaided — this instant would
        // be filed under the 9th at 01:46 instead.
        assert_eq!(
            get_desired_media_path("abc1234", &Some(dt)),
            "2001/09/09/1346-40000"
        );
        Ok(())
    }

    /// Fixtures for the precedence cases below.
    ///
    /// `1739078221` is 2025-02-09T05:17:01Z: 18:17:01 where the photo was taken
    /// (+13:00), and 17:17:01 at the tests' own +12:00. Those three readings
    /// being different is the point — each assertion below says which one the
    /// output path should be built from.
    fn info_with(
        tags: &[(nom_exif::ExifTag, &str)],
        track_creation_time: Option<&str>,
        photo_taken_time: Option<i64>,
    ) -> MediaFileInfo {
        use crate::exif_util::PsExifInfo;
        use crate::supplemental_info::PsSupplementalInfo;
        use crate::track_util::PsTrackInfo;
        use std::collections::HashMap;

        let mut info = MediaFileInfo::new_for_test();
        if !tags.is_empty() {
            let mut map = HashMap::new();
            for (tag, value) in tags {
                map.insert(tag.to_string(), (*value).to_string());
            }
            info.exif_info = Some(PsExifInfo {
                tags: map,
                gps: None,
                latitude: None,
                longitude: None,
                content_identifier: None,
            });
        }
        info.track_info = track_creation_time.map(|ct| PsTrackInfo {
            width: None,
            height: None,
            creation_time: Some(ct.to_string()),
            duration_ms: None,
            make: None,
            model: None,
            software: None,
            author: None,
            gps_iso_6709: None,
            content_identifier: None,
        });
        info.supp_info = photo_taken_time.map(|ts| {
            let json = format!(r#"{{"photoTakenTime":{{"timestamp":"{ts}"}}}}"#);
            serde_json::from_str::<PsSupplementalInfo>(&json)
                .unwrap_or_else(|e| panic!("fixture sidecar should parse: {e}"))
        });
        info
    }

    /// The whole order in one table. The archive is laid out on the
    /// photographer's wall clock, so the shutter reading wins wherever there is
    /// one and is filed exactly as the camera wrote it — never shifted into the
    /// zone the sync happens to run in.
    #[test]
    fn test_best_guess_taken_dt_precedence() {
        use nom_exif::ExifTag::{
            CreateDate, DateTimeOriginal, ModifyDate, OffsetTimeOriginal, SubSecTimeDigitized,
            SubSecTimeOriginal,
        };
        const TAKEN: i64 = 1739078221;
        const TRACK: &str = "2024-04-18T23:24:26+12:00";

        // (why it matters, EXIF tags, track time, sidecar timestamp, reading, path)
        let cases = vec![
            (
                "the shutter outranks the sidecar and keeps its own zone, so the \
                 photo is filed at the hour the photographer saw",
                vec![
                    (DateTimeOriginal, "2025-02-09T18:17:01+13:00"),
                    (OffsetTimeOriginal, "+13:00"),
                    (SubSecTimeOriginal, "183"),
                ],
                None,
                Some(TAKEN),
                "2025-02-09T18:17:01.183+13:00",
                "2025/02/09/1817-01183",
            ),
            // The reading is the numbers the camera showed. Without an offset tag
            // there is no zone to record, but the digits are no less the
            // photographer's wall clock — and filing them unshifted is what keeps
            // the path independent of the machine the sync runs on.
            (
                "a shutter reading with no recorded zone is still a wall clock and \
                 still wins, unshifted",
                vec![
                    (DateTimeOriginal, "2025:02:09 18:17:01"),
                    (SubSecTimeOriginal, "183"),
                ],
                None,
                Some(TAKEN),
                "2025-02-09T18:17:01.183+00:00",
                "2025/02/09/1817-01183",
            ),
            // A date fixed in the Google Photos UI lives only in the sidecar, and
            // no longer moves the file: the name is built from the image's own
            // bytes so it stays put across re-exports. The corrected time is still
            // recorded in the note and the database.
            (
                "the shutter wins even when the sidecar disagrees wholesale",
                vec![
                    (DateTimeOriginal, "2016:01:09 10:51:31"),
                    (SubSecTimeOriginal, "500"),
                ],
                None,
                Some(1_589_687_970),
                "2016-01-09T10:51:31.500+00:00",
                "2016/01/09/1051-31500",
            ),
            (
                "CreateDate carries the same authority as DateTimeOriginal",
                vec![
                    (CreateDate, "2025:02:09 18:17:01"),
                    (SubSecTimeDigitized, "5"),
                ],
                None,
                Some(TAKEN),
                "2025-02-09T18:17:01.500+00:00",
                "2025/02/09/1817-01500",
            ),
            (
                "a video has no EXIF, and its track time is the same kind of \
                 reading, so it outranks the sidecar too",
                vec![],
                Some(TRACK),
                Some(TAKEN),
                TRACK,
                "2024/04/18/2324-26000",
            ),
            (
                "the shutter still beats the track when a file somehow has both",
                vec![(DateTimeOriginal, "2025:02:09 18:17:01")],
                Some(TRACK),
                Some(TAKEN),
                "2025-02-09T18:17:01+00:00",
                "2025/02/09/1817-01000",
            ),
            (
                "with neither reading, the sidecar is the best available and is \
                 converted into the output zone",
                vec![],
                None,
                Some(TAKEN),
                "2025-02-09T17:17:01+12:00",
                "2025/02/09/1717-01000",
            ),
            (
                "ModifyDate is an edit, not a capture, so it stays below the sidecar",
                vec![(ModifyDate, "2008:07:31 10:38:11")],
                None,
                Some(TAKEN),
                "2025-02-09T17:17:01+12:00",
                "2025/02/09/1717-01000",
            ),
            (
                "but it is still far better than nothing once the sidecar is gone",
                vec![(ModifyDate, "2008:07:31 10:38:11")],
                None,
                None,
                "2008-07-31T10:38:11+00:00",
                "2008/07/31/1038-11000",
            ),
        ];

        for (why, tags, track, taken, expected_dt, expected_path) in cases {
            let info = info_with(&tags, track, taken);
            let dt = best_guess_taken_dt(&info, tz());
            assert_eq!(dt.as_deref(), Some(expected_dt), "{why}");
            assert_eq!(
                get_desired_media_path("abc1234", &dt),
                expected_path,
                "{why}"
            );
        }
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
            content_identifier: None,
        };

        let mut info = MediaFileInfo::new_for_test();
        info.track_info = Some(track("2024-04-18T23:24:26+12:00"));
        assert_eq!(
            best_guess_taken_dt(&info, tz()).as_deref(),
            Some("2024-04-18T23:24:26+12:00")
        );

        // Preferred over the file created/modified fallbacks.
        info.created = Some(1_000_000_000_000);
        info.modified = Some(1_000_000_000_000);
        assert_eq!(
            best_guess_taken_dt(&info, tz()).as_deref(),
            Some("2024-04-18T23:24:26+12:00")
        );
    }

    /// The whole precedence chain: EXIF, then a video's track string, then the
    /// two supplemental fields — with `(0, 0)` treated as absence throughout.
    #[test]
    fn test_best_guess_lat_long() {
        use crate::exif_util::PsExifInfo;
        use crate::supplemental_info::{PsSupplementalInfo, SupplementalInfoGeoData};
        use crate::track_util::PsTrackInfo;
        use std::collections::HashMap;

        let exif = |lat, long| PsExifInfo {
            tags: HashMap::new(),
            gps: None,
            latitude: lat,
            longitude: long,
            content_identifier: None,
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
            content_identifier: None,
        };

        // EXIF wins over both the track string and supplemental data.
        let mut info = MediaFileInfo::new_for_test();
        info.exif_info = Some(exif(Some(1.0), Some(2.0)));
        info.track_info = Some(track("+27.5916+086.5640/"));
        info.supp_info = Some(supp(Some(geo(3.0, 4.0)), Some(geo(5.0, 6.0))));
        assert_eq!(best_guess_lat_long(&info), Some((1.0, 2.0)));

        // Then a video's own track string.
        info.exif_info = None;
        assert_eq!(best_guess_lat_long(&info), Some((27.5916, 86.5640)));

        // Then geo_data_exif, then geo_data.
        info.track_info = None;
        assert_eq!(best_guess_lat_long(&info), Some((3.0, 4.0)));
        info.supp_info = Some(supp(None, Some(geo(5.0, 6.0))));
        assert_eq!(best_guess_lat_long(&info), Some((5.0, 6.0)));

        // (0, 0) is absent, so this falls through to supplemental.
        info.exif_info = Some(exif(Some(0.0), Some(0.0)));
        info.supp_info = Some(supp(Some(geo(7.0, 8.0)), None));
        assert_eq!(best_guess_lat_long(&info), Some((7.0, 8.0)));

        info.exif_info = None;
        info.supp_info = Some(supp(Some(geo(0.0, 0.0)), None));
        assert_eq!(best_guess_lat_long(&info), None);
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

        // Real short checksums are always hex, so the datetime string is the
        // only attacker-influenced input — and the path it produces must stay
        // inside the output tree whatever it says.
        for dt in [
            None,
            Some("../../../../etc/passwd".to_string()),
            Some("/absolute/path".to_string()),
            Some("not a date at all".to_string()),
            Some("9999-99-99T99:99:99Z".to_string()),
            Some("Ñoño 📸".to_string()),
            Some(String::new()),
        ] {
            let path = get_desired_media_path(&short_checksum, &dt);
            assert!(
                !crate::test_util::escapes_output(&path),
                "media path escaped output for datetime {dt:?}: {path}"
            );
        }
        Ok(())
    }
}
