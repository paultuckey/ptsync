//! Turning one inspected file into rows: `db_record` maps a [`MediaFileInfo`]
//! onto a `media_item` row (promoting EXIF/track/supplemental fields into
//! columns) and links any named people through `person`/`media_person`.

use super::schema::{DB_MEDIA_ITEM_INSERT, DB_MEDIA_PERSON_INSERT, DB_PERSON_INSERT};
use crate::media::{MediaFileInfo, best_guess_lat_long, best_guess_taken_dt};
use crate::util::{GEOHASH_PRECISION, geohash_encode, orientation};
use turso::{Connection, params};

pub(super) async fn db_record(conn: &Connection, info: &MediaFileInfo) -> anyhow::Result<()> {
    let media_info_json = serde_json::to_string(&info)?;
    let guessed_datetime = best_guess_taken_dt(info);
    let lat_long = best_guess_lat_long(info);
    let (latitude, longitude) = match lat_long {
        Some((lat, long)) => (Some(lat), Some(long)),
        None => (None, None),
    };
    let geohash = lat_long.map(|(lat, long)| geohash_encode(lat, long, GEOHASH_PRECISION));
    // Camera and dimensions come from EXIF for images; for videos they live in
    // the track metadata, so fall back to that when EXIF has nothing.
    let exif = info.exif_info.as_ref();
    let track = info.track_info.as_ref();

    let camera_make = exif
        .and_then(crate::exif_util::camera_make)
        .or_else(|| track.and_then(|t| t.make.clone()));
    let camera_model = exif
        .and_then(crate::exif_util::camera_model)
        .or_else(|| track.and_then(|t| t.model.clone()));
    let width = exif
        .and_then(crate::exif_util::image_width)
        .or_else(|| track.and_then(|t| t.width).map(|w| w as i64));
    let height = exif
        .and_then(crate::exif_util::image_height)
        .or_else(|| track.and_then(|t| t.height).map(|h| h as i64));

    let duration_ms = track.and_then(|t| t.duration_ms).map(|d| d as i64);
    let kind = crate::file_type::media_kind(&info.accurate_file_type);
    let orientation = orientation(width, height).map(str::to_string);
    let (display_mirrored, display_rotate) = exif
        .and_then(crate::exif_util::exif_display_transform)
        .unwrap_or((false, 0));
    let display_rotate = display_rotate as i64;

    let long_hash = &info.hash_info.long_checksum;
    let short_hash = &info.hash_info.short_checksum;
    let media_item_id = crate::util::media_item_id_for(&info.original_file_this_run);
    let item = DbMediaItem {
        media_item_id: media_item_id.clone(),
        media_path: info.original_file_this_run.clone(),
        long_hash: long_hash.to_string(),
        short_hash: short_hash.to_string(),
        media_info: Some(media_info_json),
        modified_at: info.modified.unwrap_or(0),
        created_at: info.created.unwrap_or(0),
        quick_file_type: info.quick_file_type.to_string(),
        accurate_file_type: info.accurate_file_type.to_string(),
        guessed_datetime,
        file_size: info.file_size as i64,
        latitude,
        longitude,
        camera_make,
        camera_model,
        width,
        height,
        duration_ms,
        orientation,
        display_mirrored,
        display_rotate,
        geohash,
        kind,
    };

    let mut stmt = conn.prepare_cached(DB_MEDIA_ITEM_INSERT).await?;
    stmt.execute(params![
        item.media_path.as_str(),
        item.long_hash.as_str(),
        item.short_hash.as_str(),
        item.quick_file_type.as_str(),
        item.accurate_file_type.as_str(),
        item.media_info.as_deref(),
        item.guessed_datetime.as_deref(),
        item.modified_at,
        item.created_at,
        item.file_size,
        item.latitude,
        item.longitude,
        item.camera_make.as_deref(),
        item.camera_model.as_deref(),
        item.width,
        item.height,
        item.duration_ms,
        item.orientation.as_deref(),
        item.display_mirrored,
        item.display_rotate,
        item.geohash.as_deref(),
        item.kind,
        item.media_item_id.as_str(),
    ])
    .await?;

    // Named people come from Google supplemental metadata. Each name resolves to
    // a stable, content-derived person id (shared across items and rebuilds), so
    // we upsert the person then link it to this media item.
    if let Some(supp) = &info.supp_info {
        let mut stmt_person = conn.prepare_cached(DB_PERSON_INSERT).await?;
        let mut stmt_media_person = conn.prepare_cached(DB_MEDIA_PERSON_INSERT).await?;
        for person in &supp.people {
            if let Some(name) = &person.name {
                let person_id = crate::util::person_id_for(name);
                stmt_person
                    .execute((person_id.as_str(), name.as_str()))
                    .await?;
                stmt_media_person
                    .execute((media_item_id.as_str(), person_id.as_str()))
                    .await?;
            }
        }
    }

    Ok(())
}

#[derive(Debug)]
struct DbMediaItem {
    // stable hash of media_path; reproducible across runs/machines/clears
    media_item_id: String,
    media_path: String,
    long_hash: String,
    short_hash: String,
    media_info: Option<String>,
    quick_file_type: String,
    accurate_file_type: String,
    // formatted as ISO 8601
    guessed_datetime: Option<String>,
    modified_at: i64,
    created_at: i64,
    // file size in bytes
    file_size: i64,
    // best-guess GPS coordinates, None if unknown
    latitude: Option<f64>,
    longitude: Option<f64>,
    // EXIF camera details, None if unknown
    camera_make: Option<String>,
    camera_model: Option<String>,
    // image/video dimensions in pixels, None if unknown
    width: Option<i64>,
    height: Option<i64>,
    // video duration in ms, None for photos
    duration_ms: Option<i64>,
    // portrait/landscape/square, None if dimensions unknown
    orientation: Option<String>,
    // whether the image must be flipped horizontally for display; false if no EXIF
    display_mirrored: bool,
    // clockwise degrees to rotate for display (-90/0/90/180); 0 if no EXIF
    display_rotate: i64,
    // geohash of the coordinates, None if no location
    geohash: Option<String>,
    // 'p' for photo, 'v' for video, None if neither
    kind: Option<&'static str>,
}
