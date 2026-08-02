//! Turning one inspected file into rows: `db_record` maps a [`MediaFileInfo`]
//! onto a `media_item` row (promoting EXIF/track/supplemental fields into
//! columns) and links any named people through `person`/`media_person`.

use super::schema::{DB_MEDIA_ITEM_INSERT, DB_MEDIA_PERSON_INSERT, DB_PERSON_INSERT};
use crate::metadata::MediaFileInfo;
use crate::metadata::reconcile::{
    best_guess_archived, best_guess_description, best_guess_favorite, best_guess_lat_long,
    best_guess_rating, best_guess_taken, best_guess_title,
};
use crate::util::{GEOHASH_PRECISION, geohash_encode, orientation};
use turso::{Connection, params};

pub(super) async fn db_record(conn: &Connection, info: &MediaFileInfo) -> anyhow::Result<()> {
    let media_info_json = serde_json::to_string(&info)?;
    // One resolution, read two ways: the RFC 3339 spelling for the column, and
    // the offset beside it so a reader can tell a recorded zone from the
    // placeholder `+00:00` a bare camera reading is written with.
    let taken = best_guess_taken(info);
    let guessed_datetime = taken
        .as_ref()
        .map(crate::metadata::taken::Taken::to_rfc3339);
    let guessed_utc_offset_s = taken
        .as_ref()
        .and_then(|t| t.offset)
        .map(|o| i64::from(o.local_minus_utc()));
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
        .and_then(crate::metadata::exif::camera_make)
        .or_else(|| track.and_then(|t| t.make.clone()));
    let camera_model = exif
        .and_then(crate::metadata::exif::camera_model)
        .or_else(|| track.and_then(|t| t.model.clone()));
    let width = exif
        .and_then(crate::metadata::exif::image_width)
        .or_else(|| track.and_then(|t| t.width).map(|w| w as i64));
    let height = exif
        .and_then(crate::metadata::exif::image_height)
        .or_else(|| track.and_then(|t| t.height).map(|h| h as i64));

    let duration_ms = track.and_then(|t| t.duration_ms).map(|d| d as i64);
    // Apple stamps one uuid on both halves of a live photo, reaching us from the
    // still's maker note or the clip's `moov` keys - never both on one file, so
    // whichever is present is this file's.
    let content_identifier = exif
        .and_then(|e| e.content_identifier.clone())
        .or_else(|| track.and_then(|t| t.content_identifier.clone()));
    let kind = crate::file_type::media_kind(&info.accurate_file_type);
    let orientation = orientation(width, height).map(str::to_string);
    let (display_mirrored, display_rotate) = exif
        .and_then(crate::metadata::exif::exif_display_transform)
        .unwrap_or((false, 0));
    let display_rotate = display_rotate as i64;

    let xmp = info.xmp_info.as_ref();
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
        guessed_utc_offset_s,
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
        label: xmp.and_then(|x| x.label.clone()),
        // Resolved rather than read straight off a sidecar, so the index and the
        // note cannot disagree - the same guarantee `best_guess_lat_long` buys
        // for coordinates. `rating` and `favorite` stay strictly separate: see
        // `best_guess_rating`.
        rating: best_guess_rating(info),
        title: best_guess_title(info),
        description: best_guess_description(info),
        favorite: best_guess_favorite(info),
        archived: best_guess_archived(info),
        content_identifier,
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
        item.guessed_utc_offset_s,
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
        item.rating,
        item.label.as_deref(),
        item.title.as_deref(),
        item.description.as_deref(),
        item.favorite,
        item.archived,
        item.content_identifier.as_deref(),
        item.media_item_id.as_str(),
    ])
    .await?;

    // Named people come from Google supplemental metadata and from any XMP
    // sidecar (`PersonInImage` plus MWG face regions). Each name resolves to a
    // stable, content-derived person id (shared across items and rebuilds), so
    // we upsert the person then link it to this media item. `media_person` is
    // UNIQUE on the pair, so a person named by both sources links once.
    let people_from_supp = info
        .supp_info
        .iter()
        .flat_map(|supp| supp.people.iter())
        .filter_map(|p| p.name.as_deref());
    let people_from_xmp = xmp
        .into_iter()
        .flat_map(|x| x.people.iter())
        .map(String::as_str);
    let mut stmt_person = conn.prepare_cached(DB_PERSON_INSERT).await?;
    let mut stmt_media_person = conn.prepare_cached(DB_MEDIA_PERSON_INSERT).await?;
    for name in people_from_supp.chain(people_from_xmp) {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let person_id = crate::util::person_id_for(name);
        stmt_person.execute((person_id.as_str(), name)).await?;
        stmt_media_person
            .execute((media_item_id.as_str(), person_id.as_str()))
            .await?;
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
    // seconds east of UTC when a source recorded the zone; None means the offset
    // on `guessed_datetime` is a placeholder rather than a reading
    guessed_utc_offset_s: Option<i64>,
    modified_at: i64,
    created_at: i64,
    // file size in bytes
    file_size: i64,
    // xmp:Rating / xmp:Label from a sidecar, None when there is none
    rating: Option<i64>,
    label: Option<String>,
    // title and description pooled across both sidecars, None when neither set one
    title: Option<String>,
    description: Option<String>,
    // Google Photos flags; false when there is no supplemental sidecar
    favorite: bool,
    archived: bool,
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
    // Apple's per-asset uuid, shared by a live photo's still and clip. None for
    // anything that never passed through an Apple device, or whose maker note
    // was stripped on the way here.
    content_identifier: Option<String>,
}
