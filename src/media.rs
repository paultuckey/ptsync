use crate::exif_util::{PsExifInfo, best_guess_taken_exif, parse_exif_info};
use crate::file_type::{
    AccurateFileType, MetadataType, QuickFileType, determine_file_type, file_ext_from_file_type,
    metadata_type,
};
use crate::supplemental_info::PsSupplementalInfo;
use crate::track_util::{PsTrackInfo, parse_track_info};
use crate::util::{HashInfo, ScanInfo};
use crate::xmp::PsXmpInfo;
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
    pub(crate) xmp_info: Option<PsXmpInfo>,
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
    xmp_info: &Option<PsXmpInfo>,
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
        xmp_info: xmp_info.clone(),
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

/// Best guess at the date the photo was taken from messy optional data.
///
/// Sources are ranked **human before camera**: a reading someone deliberately
/// corrected is worth more than the one the camera happened to record, because
/// correcting it is precisely the act of saying the camera was wrong. See
/// [`best_guess_lat_long`], which ranks locations on the same principle.
///
/// *Human tier* — someone chose this value:
/// 1. XMP sidecar `photoshop:DateCreated` - a correction made in Lightroom,
///    darktable or digiKam
/// 2. SupplementalInfo photo_taken_time - the date Google Photos displays, which
///    the user can edit in its UI
///
/// *Camera tier* — the device's own reading:
/// 3. EXIF DateTimeOriginal
/// 4. EXIF DateTime
/// 5. EXIF GPSDateStamp - only accurate up to minute
/// 6. Track creation_time - the embedded capture time for videos (already
///    rfc3339); videos have no EXIF, so this is their equivalent of 3
///
/// *Fallback tier* — not a capture time at all, only better than nothing:
/// 7. SupplementalInfo creation_time - when the file was *uploaded* to Google,
///    which is why it ranks below the camera despite sharing a file with 2
/// 8. File modified time
///   - no timezone info, unreliable in zips, somewhat unreliable in directories due to file
///     copying / syncing not preserving, only use as second to last resort
/// 9. File creation time
///   - no timezone info, unavailable in zips, somewhat unreliable in directories due to file
///     copying / syncing not preserving, only use as a last resort
///
/// Result returned as an RFC 3339 string. Every source above normalizes to that
/// one form, because [`get_desired_media_path`] parses it back and files anything
/// it cannot read under `undated/` — a source that returns its own native
/// spelling silently loses the date it just found.
pub(crate) fn best_guess_taken_dt(info: &MediaFileInfo) -> Option<String> {
    // -- human tier --
    if let Some(dt) = info.xmp_info.as_ref().and_then(|x| x.datetime.clone()) {
        return Some(dt);
    }
    if let Some(dt) = info
        .supp_info
        .as_ref()
        .and_then(|si| si.photo_taken_time.as_ref())
        .and_then(|si_dt| si_dt.timestamp_s_as_iso_8601())
    {
        return Some(dt);
    }

    // -- camera tier --
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

    // -- fallback tier --
    // Google's `creationTime` is the upload timestamp, not a capture time, so it
    // sits below the camera's reading even though `photoTakenTime` - from the
    // same JSON file - sits above it.
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

/// Best guess at where the media was taken, as `(latitude, longitude)`.
///
/// Ranked **human before camera**, the same principle as
/// [`best_guess_taken_dt`]: a location someone set or corrected outranks the fix
/// the device recorded.
///
/// *Human tier* — someone chose this value:
/// 1. XMP sidecar `exif:GPSLatitude`/`GPSLongitude` - a location set in
///    Lightroom, darktable or digiKam
/// 2. SupplementalInfo `geo_data` - the location Google Photos displays, which
///    the user can edit in its UI
///
/// *Camera tier* — the device's own fix:
/// 3. EXIF GPS (embedded in images)
/// 4. Track ISO 6709 GPS (embedded in videos)
/// 5. SupplementalInfo `geo_data_exif` - Google's *copy* of the EXIF fix, so it
///    belongs with the camera despite sharing a file with 2. It ranks last
///    because it only adds something when the file's own EXIF is gone, which
///    Takeout often does.
///
/// A `(0, 0)` pair is treated as absent at every source: EXIF and Takeout both
/// write zeros when they have no fix rather than omitting the value. Each parser
/// now drops those before they reach a struct ([`crate::exif_util::parse_exif_info`],
/// [`crate::xmp::parse_xmp`], [`crate::supplemental_info::parse_supplemental_info`]),
/// so nothing here or downstream ever sees "null island" - the checks below stay
/// as the backstop for `track_info`, whose ISO 6709 string is parsed here, and
/// for values built in code rather than read from a file.
///
/// This is the *only* place coordinates are resolved. Both consumers - the
/// `latitude`/`longitude` frontmatter keys in a photo's note
/// ([`crate::markdown::mfm_from_media_file_info`]) and the `media_item` columns
/// the `db` command writes ([`crate::db_cmd`]) - call it, so the note and the
/// index cannot disagree about where a photo was taken. They once had separate
/// copies of this logic that had drifted: the note's ordered Google's two geo
/// fields the other way round and never consulted `track_info` at all, so
/// videos got coordinates in the database and none in their notes.
pub(crate) fn best_guess_lat_long(info: &MediaFileInfo) -> Option<(f64, f64)> {
    use crate::util::non_zero_coords;

    // -- human tier --
    if let Some(xmp) = &info.xmp_info
        && let Some(coords) = non_zero_coords(xmp.latitude, xmp.longitude)
    {
        return Some(coords);
    }
    if let Some(supp) = &info.supp_info
        && let Some(geo) = supp.geo_data.as_ref()
        && let Some(coords) = non_zero_coords(geo.latitude, geo.longitude)
    {
        return Some(coords);
    }

    // -- camera tier --
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
    if let Some(supp) = &info.supp_info
        && let Some(geo) = supp.geo_data_exif.as_ref()
        && let Some(coords) = non_zero_coords(geo.latitude, geo.longitude)
    {
        return Some(coords);
    }
    None
}

/// Best guess at a title someone gave the media, or `None` when nobody did.
///
/// Both sources are the *human tier* of [`best_guess_taken_dt`] — a title is
/// nothing but a human opinion, so there is no camera tier to fall back to:
///
/// 1. XMP sidecar `dc:title` — set in Lightroom, darktable or digiKam
/// 2. SupplementalInfo `title` — set in Google Photos, and only present when it
///    is not the file's own name (see
///    [`PsSupplementalInfo::drop_file_name_title`](crate::supplemental_info::PsSupplementalInfo))
///
/// XMP leads for the same reason it leads everywhere else here: a value written
/// by a tool the user drove deliberately outranks one from a service that fills
/// the field in by default.
pub(crate) fn best_guess_title(info: &MediaFileInfo) -> Option<String> {
    let xmp = info.xmp_info.as_ref().and_then(|x| x.title.as_deref());
    let supp = info.supp_info.as_ref().and_then(|s| s.title.as_deref());
    non_blank(xmp)
        .or_else(|| non_blank(supp))
        .map(str::to_string)
}

/// Best guess at a description someone wrote for the media, ranked exactly as
/// [`best_guess_title`]: XMP `dc:description`, then Google Photos' caption.
pub(crate) fn best_guess_description(info: &MediaFileInfo) -> Option<String> {
    let xmp = info
        .xmp_info
        .as_ref()
        .and_then(|x| x.description.as_deref());
    let supp = info
        .supp_info
        .as_ref()
        .and_then(|s| s.description.as_deref());
    non_blank(xmp)
        .or_else(|| non_blank(supp))
        .map(str::to_string)
}

/// The rating someone gave the media: 0–5, or -1 for "rejected".
///
/// XMP `xmp:Rating` is the only source — no export format ptsync reads carries a
/// star rating of its own. It is resolved here anyway so the note, the index and
/// the `info` report all read it the same way, and so this doc comment has one
/// place to say what does *not* feed it: a Google favourite is not a five-star
/// rating. Tools like PhotoSync and Synology Photos do write favourites out as
/// `xmp:Rating` 5, but reading that convention backwards would invent a rating
/// nobody gave, and since a rating is seeded into a note once and then left
/// alone, the invented value could never afterwards be told from a real one.
pub(crate) fn best_guess_rating(info: &MediaFileInfo) -> Option<i64> {
    info.xmp_info.as_ref().and_then(|x| x.rating)
}

/// Whether the media is a favourite.
///
/// Google's `favorited` is the only source. XMP has no favourite property at
/// all: the spec defines `xmp:Rating` (-1, or 0–5) and nothing else, and
/// Lightroom's pick flags never leave its catalog. `xmp:Rating == 5` is
/// deliberately *not* read as a favourite — someone rating a shoot in darktable
/// means "best of these", not "starred in Google Photos", and collapsing the two
/// would silently promote every top-rated photo. See [`best_guess_rating`] for
/// the same argument in the other direction.
pub(crate) fn best_guess_favorite(info: &MediaFileInfo) -> bool {
    info.supp_info.as_ref().is_some_and(|s| s.favorited)
}

/// Whether the media was archived — hidden from the main Google Photos grid but
/// not deleted.
///
/// Google's `archived` is the only source. `xmp:Rating == -1` ("rejected") is
/// deliberately not read as archived: rejecting a frame is a judgement about the
/// photograph, archiving it is a decision about which grid it shows up in, and
/// neither implies the other.
///
/// Takeout also expresses this structurally, by exporting the file under an
/// `Archive/` directory. That signal is *not* used here, because
/// [`classify_dir`](crate::classify::classify_dir)'s `^archive` pattern is
/// anchored at the scan root and so matches any ordinary folder of that name —
/// on a real archive it fires on things like `archive/2009/timesheets`. Reading
/// it would mark a whole backup tree as archived. Tightening the pattern first
/// is what would make the directory usable.
pub(crate) fn best_guess_archived(info: &MediaFileInfo) -> bool {
    info.supp_info.as_ref().is_some_and(|s| s.archived)
}

/// Trim a value and treat one that is all whitespace as absent — sidecar writers
/// emit empty strings for fields they have nothing for.
fn non_blank(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
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
            xmp_info: None,
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

        info.created = Some(ts);
        info.modified = None;
        let dt =
            best_guess_taken_dt(&info).ok_or_else(|| anyhow!("Should have a date from created"))?;
        assert_eq!(dt, "2001-09-09T01:46:40+00:00");

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

    /// Dates rank human before camera: a date someone corrected outranks the one
    /// the device recorded. Google's `creationTime` is the exception that proves
    /// the rule - it shares a file with `photoTakenTime` but is an *upload*
    /// timestamp, so it falls below the camera rather than above it.
    #[test]
    fn test_best_guess_taken_dt_human_beats_camera() {
        use crate::exif_util::PsExifInfo;
        use crate::supplemental_info::{
            PsSupplementalInfo, SupplementalInfoDateTime, SupplementalInfoPerson,
        };
        use std::collections::HashMap;

        // 2001-09-09T01:46:40Z, as seconds - the spelling Takeout uses.
        let supp = |photo_taken: Option<&str>, creation: Option<&str>| PsSupplementalInfo {
            people: Vec::<SupplementalInfoPerson>::new(),
            photo_taken_time: photo_taken.map(SupplementalInfoDateTime::new_for_test),
            creation_time: creation.map(SupplementalInfoDateTime::new_for_test),
            ..PsSupplementalInfo::default()
        };
        let exif_dated = |raw: &str| {
            let mut tags = HashMap::new();
            tags.insert("DateTimeOriginal".to_string(), raw.to_string());
            PsExifInfo {
                tags,
                gps: None,
                latitude: None,
                longitude: None,
            }
        };

        let mut info = MediaFileInfo::new_for_test();
        info.xmp_info = Some(crate::xmp::PsXmpInfo {
            datetime: Some("2015-01-01T00:00:00+00:00".to_string()),
            ..Default::default()
        });
        info.supp_info = Some(supp(Some("1000000000"), Some("1100000000")));
        info.exif_info = Some(exif_dated("2008:05:30 15:56:01"));

        // XMP is the most deliberate correction, so it wins outright.
        assert_eq!(
            best_guess_taken_dt(&info).as_deref(),
            Some("2015-01-01T00:00:00+00:00")
        );

        // Without XMP, Google's `photoTakenTime` - editable in the Photos UI -
        // is the remaining human source and still beats the camera's EXIF.
        info.xmp_info = None;
        assert_eq!(
            best_guess_taken_dt(&info).as_deref(),
            Some("2001-09-09T01:46:40+00:00")
        );

        // With no human source, EXIF - the camera's own reading - is used.
        info.supp_info = Some(supp(None, Some("1100000000")));
        assert_eq!(
            best_guess_taken_dt(&info).as_deref(),
            Some("2008-05-30T15:56:01+00:00")
        );

        // `creationTime` is an upload timestamp, so it only applies once the
        // camera has nothing to say - unlike `photoTakenTime` above it.
        info.exif_info = None;
        assert_eq!(
            best_guess_taken_dt(&info).as_deref(),
            Some("2004-11-09T11:33:20+00:00")
        );
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
            ..PsSupplementalInfo::default()
        };

        let xmp = |lat: f64, long: f64| crate::xmp::PsXmpInfo {
            latitude: Some(lat),
            longitude: Some(long),
            ..Default::default()
        };

        // Human beats camera: an XMP location outranks everything, including the
        // file's own EXIF fix.
        let mut info = MediaFileInfo::new_for_test();
        info.xmp_info = Some(xmp(9.0, 10.0));
        info.exif_info = Some(exif(Some(1.0), Some(2.0)));
        info.supp_info = Some(supp(Some(geo(3.0, 4.0)), Some(geo(5.0, 6.0))));
        assert_eq!(best_guess_lat_long(&info), Some((9.0, 10.0)));

        // No XMP: Google's `geo_data` - what Photos displays, and editable by the
        // user - is the next human source, and it too beats EXIF.
        info.xmp_info = None;
        assert_eq!(best_guess_lat_long(&info), Some((5.0, 6.0)));

        // With no human source left, the camera's own EXIF fix wins.
        info.supp_info = Some(supp(Some(geo(3.0, 4.0)), None));
        assert_eq!(best_guess_lat_long(&info), Some((1.0, 2.0)));

        // `geo_data_exif` is Google's copy of the EXIF fix, so it ranks last -
        // it only contributes when the file's own EXIF has been stripped.
        info.exif_info = None;
        assert_eq!(best_guess_lat_long(&info), Some((3.0, 4.0)));

        // (0, 0) is treated as absent at every source, human tier included.
        info.xmp_info = Some(xmp(0.0, 0.0));
        info.exif_info = Some(exif(Some(0.0), Some(0.0)));
        info.supp_info = Some(supp(Some(geo(7.0, 8.0)), Some(geo(0.0, 0.0))));
        assert_eq!(best_guess_lat_long(&info), Some((7.0, 8.0)));

        // Nothing usable anywhere.
        info.xmp_info = None;
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

    /// Title and description are pooled across both sidecars, XMP first: a value
    /// set in a photo tool the user drove outranks one from Google Photos, and
    /// either beats nothing at all.
    #[test]
    fn test_best_guess_title_and_description() {
        use crate::supplemental_info::PsSupplementalInfo;
        use crate::xmp::PsXmpInfo;

        let supp = |title: Option<&str>, description: Option<&str>| PsSupplementalInfo {
            title: title.map(str::to_string),
            description: description.map(str::to_string),
            ..PsSupplementalInfo::default()
        };
        let xmp = |title: Option<&str>, description: Option<&str>| PsXmpInfo {
            title: title.map(str::to_string),
            description: description.map(str::to_string),
            ..PsXmpInfo::default()
        };

        // Neither sidecar: nothing to show.
        let mut info = MediaFileInfo::new_for_test();
        assert_eq!(best_guess_title(&info), None);
        assert_eq!(best_guess_description(&info), None);

        // Google's caption alone reaches the note - the common Takeout case,
        // where there is no XMP anywhere in the archive.
        info.supp_info = Some(supp(Some("Beach day"), Some("Low tide.")));
        assert_eq!(best_guess_title(&info), Some("Beach day".to_string()));
        assert_eq!(best_guess_description(&info), Some("Low tide.".to_string()));

        // With both, XMP wins.
        info.xmp_info = Some(xmp(Some("Porthcurno"), Some("Shot on the 40D.")));
        assert_eq!(best_guess_title(&info), Some("Porthcurno".to_string()));
        assert_eq!(
            best_guess_description(&info),
            Some("Shot on the 40D.".to_string())
        );

        // A blank in the higher-ranked source is absent, not an override: the
        // lower one still gets its turn.
        info.xmp_info = Some(xmp(Some("  "), None));
        assert_eq!(best_guess_title(&info), Some("Beach day".to_string()));
        assert_eq!(best_guess_description(&info), Some("Low tide.".to_string()));
    }

    /// Rating, favourite and archived are three separate opinions that happen to
    /// live near each other, and no source of one is allowed to invent another.
    #[test]
    fn test_rating_favorite_and_archived_stay_independent() {
        use crate::supplemental_info::PsSupplementalInfo;
        use crate::xmp::PsXmpInfo;

        // A five-star XMP rating and no Google sidecar: rated, not favourited.
        let mut info = MediaFileInfo::new_for_test();
        info.xmp_info = Some(PsXmpInfo {
            rating: Some(5),
            ..PsXmpInfo::default()
        });
        assert_eq!(best_guess_rating(&info), Some(5));
        assert!(
            !best_guess_favorite(&info),
            "five stars is a rating, not a Google favourite"
        );

        // Rejected. That is a judgement about the photograph and says nothing
        // about which grid it appears in.
        info.xmp_info = Some(PsXmpInfo {
            rating: Some(-1),
            ..PsXmpInfo::default()
        });
        assert_eq!(best_guess_rating(&info), Some(-1));
        assert!(!best_guess_archived(&info), "rejected is not archived");
        assert!(!best_guess_favorite(&info));

        // A Google favourite with no XMP: favourited, and still unrated.
        let mut info = MediaFileInfo::new_for_test();
        info.supp_info = Some(PsSupplementalInfo {
            favorited: true,
            ..PsSupplementalInfo::default()
        });
        assert!(best_guess_favorite(&info));
        assert_eq!(
            best_guess_rating(&info),
            None,
            "a favourite must not be written back as a rating"
        );
        assert!(!best_guess_archived(&info));

        // Archived, and the two flags are independent of each other too.
        info.supp_info = Some(PsSupplementalInfo {
            archived: true,
            ..PsSupplementalInfo::default()
        });
        assert!(best_guess_archived(&info));
        assert!(!best_guess_favorite(&info));

        // No sidecars at all: nothing is asserted about the photo.
        let info = MediaFileInfo::new_for_test();
        assert_eq!(best_guess_rating(&info), None);
        assert!(!best_guess_favorite(&info));
        assert!(!best_guess_archived(&info));
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
