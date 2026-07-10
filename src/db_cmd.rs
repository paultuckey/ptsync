use crate::classify::{classify_dir, classify_file};
use crate::file_type::QuickFileType;
use crate::fs::{FileSystem, OsFileSystem, ZipFileSystem};
use crate::inspect::inspect_media_files;
use crate::media::{MediaFileInfo, best_guess_lat_long, best_guess_taken_dt};
use crate::progress::Progress;
use crate::util::{GEOHASH_PRECISION, ScanInfo, geohash_encode, orientation, scan_fs};
use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use tokio::runtime;
use tracing::{debug, info, warn};
use turso::{Builder, Connection, Database, IntoParams, Row, params};

const DB_BATCH_SIZE: usize = 100;

pub(crate) fn main(input: &String, output: &str) -> anyhow::Result<()> {
    debug!("Inspecting: {input}");
    let path = Path::new(input);
    if !path.exists() {
        return Err(anyhow!("Input path does not exist: {}", input));
    }
    let container: Arc<dyn FileSystem> = if path.is_dir() {
        info!("Input directory: {input}");
        Arc::new(OsFileSystem::new(input))
    } else {
        info!("Input zip: {input}");
        Arc::new(ZipFileSystem::new(input)?)
    };

    info!("Writing database: {output}");
    let rt = runtime::Builder::new_current_thread().build()?;
    rt.block_on(async {
        let (_db, conn) = open_conn(output).await?;
        run_db_scan(container, &conn).await
    })
}

async fn run_db_scan(container: Arc<dyn FileSystem>, conn: &Connection) -> anyhow::Result<()> {
    db_prepare(conn).await?;

    let files = scan_fs(container.as_ref());
    info!("Found {} files in input", files.len());

    db_classify_paths(conn, &files).await?;

    let media_si_files: Vec<ScanInfo> = files
        .iter()
        .filter(|m| m.quick_file_type == QuickFileType::Media)
        .cloned()
        .collect();
    info!("Inspecting {} photo and video files", media_si_files.len());
    let prog = Arc::new(Progress::new(media_si_files.len() as u64));

    // Inspect in parallel and stream the results straight into the db, committing
    // in batches to avoid the per-row fsync of autocommit. A single writer drains
    // the channel; the parallelism lives in `inspect_media_files`.
    conn.execute("BEGIN", ()).await?;
    let mut batch_count = 0;
    let mut inspected = inspect_media_files(container.clone(), media_si_files, prog.clone());
    for info in inspected.by_ref() {
        db_record(conn, &info).await?;
        batch_count += 1;
        if batch_count >= DB_BATCH_SIZE {
            conn.execute("COMMIT", ()).await?;
            conn.execute("BEGIN", ()).await?;
            batch_count = 0;
        }
    }
    conn.execute("COMMIT", ()).await?;

    let skipped = inspected.skipped_count();
    if skipped > 0 {
        warn!("{skipped} files could not be processed");
    }

    drop(prog);

    let album_si_files = files
        .iter()
        .filter(|m| {
            matches!(
                m.quick_file_type,
                QuickFileType::AlbumCsv | QuickFileType::AlbumJson
            )
        })
        .collect::<Vec<&ScanInfo>>();

    info!("Inspecting {} album files", album_si_files.len());
    let prog_albums = Progress::new(album_si_files.len() as u64);

    conn.execute("BEGIN", ()).await?;
    for album_si in album_si_files {
        if let Some(album) = crate::album::parse_album(container.as_ref(), album_si, &files) {
            let album_id = crate::util::album_id_for(&album_si.file_path);
            conn.execute(
                DB_ALBUM_INSERT,
                (
                    album_id.as_str(),
                    album.title.as_str(),
                    album_si.file_path.as_str(),
                ),
            )
            .await?;
            for file in &album.files {
                // Album members reference scanned file paths; link them to the
                // media_item row for that path. Skip any that weren't indexed as
                // media (e.g. an unsupported type).
                let media_item_id: Option<String> =
                    match query_one(conn, DB_MEDIA_ITEM_ID_BY_PATH, [file.as_str()]).await? {
                        Some(row) => Some(row.get::<String>(0)?),
                        None => None,
                    };
                match media_item_id {
                    Some(id) => {
                        conn.execute(DB_ALBUM_FILE_INSERT, (album_id.as_str(), id.as_str()))
                            .await?;
                    }
                    None => debug!("Album {:?} references unindexed file {file:?}", album.title),
                }
            }
        }
        prog_albums.inc();
    }
    conn.execute("COMMIT", ()).await?;
    drop(prog_albums);

    // Fold WAL back into the main file so the output is a single, file.
    let _ = query_one(conn, "PRAGMA wal_checkpoint(TRUNCATE)", ()).await?;

    info!("Done {} files", files.len());
    Ok(())
}

async fn query_one(
    conn: &Connection,
    sql: &str,
    params: impl IntoParams,
) -> anyhow::Result<Option<Row>> {
    let mut rows = conn.query(sql, params).await?;
    let first = rows.next().await?;
    while rows.next().await?.is_some() {}
    Ok(first)
}

async fn db_classify_paths(conn: &Connection, files: &[ScanInfo]) -> anyhow::Result<()> {
    info!("Classifying {} files against known patterns", files.len());

    conn.execute("BEGIN", ()).await?;

    let mut matched_files = 0usize;
    let mut stmt_file = conn.prepare_cached(DB_CLASSIFIED_FILE_INSERT).await?;
    for si in files {
        let known = classify_file(&si.file_path);
        if known.is_some() {
            matched_files += 1;
        }
        stmt_file
            .execute((
                si.file_path.as_str(),
                si.quick_file_type.to_string(),
                known.as_ref().map(|k| k.to_string()),
                known.as_ref().and_then(|k| k.value()),
                si.file_size as i64,
            ))
            .await?;
    }

    let mut matched_dirs = 0usize;
    let mut seen_dirs: HashSet<&str> = HashSet::new();
    let mut stmt_dir = conn.prepare_cached(DB_CLASSIFIED_DIR_INSERT).await?;
    for si in files {
        let Some(parent) = Path::new(&si.file_path).parent().and_then(|p| p.to_str()) else {
            continue;
        };
        if parent.is_empty() || !seen_dirs.insert(parent) {
            continue;
        }
        let known = classify_dir(parent);
        if known.is_some() {
            matched_dirs += 1;
        }
        stmt_dir
            .execute((
                parent,
                known.as_ref().map(|k| k.to_string()),
                known.as_ref().and_then(|k| k.value()),
            ))
            .await?;
    }
    info!(
        "Matched {}/{} files, {}/{} dirs against known patterns",
        matched_files,
        files.len(),
        matched_dirs,
        seen_dirs.len()
    );

    conn.execute("COMMIT", ()).await?;
    Ok(())
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all(deserialize = "camelCase", serialize = "camelCase"))]
pub(crate) struct HashInfo {
    pub(crate) short_checksum: String,
    pub(crate) long_checksum: String,
}

async fn db_record(conn: &Connection, info: &MediaFileInfo) -> anyhow::Result<()> {
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
const DB_MEDIA_ITEM_CREATE: &str = "
    CREATE TABLE IF NOT EXISTS media_item  (
        media_item_id TEXT PRIMARY KEY, -- stable hash of the media path
        media_path TEXT NOT NULL,
        long_hash TEXT,
        short_hash TEXT,
        quick_file_type TEXT,
        accurate_file_type TEXT,
        media_info TEXT,
        guessed_datetime DATETIME,
        modified_at DATETIME DEFAULT CURRENT_TIMESTAMP, -- file last modified
        created_at DATETIME DEFAULT CURRENT_TIMESTAMP, -- file created
        file_size INTEGER, -- size of the file in bytes
        latitude REAL, -- best-guess GPS latitude, NULL if unknown
        longitude REAL, -- best-guess GPS longitude, NULL if unknown
        camera_make TEXT, -- EXIF camera manufacturer, NULL if unknown
        camera_model TEXT, -- EXIF camera model, NULL if unknown
        width INTEGER, -- image/video width in pixels, NULL if unknown
        height INTEGER, -- image/video height in pixels, NULL if unknown
        duration_ms INTEGER, -- video duration in ms, NULL for photos
        orientation TEXT, -- portrait/landscape/square, NULL if dimensions unknown
        display_mirrored INTEGER NOT NULL DEFAULT 0, -- 1 if the image must be flipped horizontally for display
        display_rotate INTEGER NOT NULL DEFAULT 0, -- clockwise degrees to rotate for display (-90/0/90/180)
        geohash TEXT, -- geohash of the coordinates, NULL if no location
        kind TEXT -- 'p' for photo, 'v' for video, NULL if neither
    )
";
const DB_MEDIA_ITEM_INSERT: &str = "
    INSERT INTO media_item (media_path, long_hash, short_hash, quick_file_type,
        accurate_file_type, media_info, guessed_datetime, modified_at, created_at, file_size,
        latitude, longitude, camera_make, camera_model, width, height,
        duration_ms, orientation, display_mirrored, display_rotate, geohash, kind, media_item_id)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)
";
const DB_MEDIA_ITEM_ID_BY_PATH: &str = "SELECT media_item_id FROM media_item WHERE media_path = ?1";
const DB_MEDIA_ITEM_DELETE_ALL: &str = "
    DELETE FROM media_item
";

const DB_PERSON_CREATE: &str = "
    CREATE TABLE IF NOT EXISTS person (
        person_id TEXT PRIMARY KEY, -- stable hash of the lowercased name
        name TEXT NOT NULL
    )
";
const DB_PERSON_INSERT: &str = "
    INSERT OR IGNORE INTO person (person_id, name) VALUES (?1, ?2)
";
const DB_PERSON_DELETE_ALL: &str = "DELETE FROM person";

const DB_MEDIA_PERSON_CREATE: &str = "
    CREATE TABLE IF NOT EXISTS media_person (
        media_item_id TEXT,
        person_id TEXT,
        FOREIGN KEY(media_item_id) REFERENCES media_item(media_item_id),
        FOREIGN KEY(person_id) REFERENCES person(person_id)
    )
";
const DB_MEDIA_PERSON_INSERT: &str = "
    INSERT INTO media_person (media_item_id, person_id) VALUES (?1, ?2)
";
const DB_MEDIA_PERSON_DELETE_ALL: &str = "DELETE FROM media_person";

const DB_ALBUM_CREATE: &str = "
    CREATE TABLE IF NOT EXISTS album (
        album_id TEXT PRIMARY KEY, -- stable hash of the album path
        title TEXT,
        album_path TEXT NOT NULL
    )
";

const DB_ALBUM_FILE_CREATE: &str = "
    CREATE TABLE IF NOT EXISTS album_file (
        album_id TEXT,
        media_item_id TEXT,
        FOREIGN KEY(album_id) REFERENCES album(album_id),
        FOREIGN KEY(media_item_id) REFERENCES media_item(media_item_id)
    )
";

const DB_ALBUM_INSERT: &str = "
    INSERT OR IGNORE INTO album (album_id, title, album_path) VALUES (?1, ?2, ?3)
";

const DB_ALBUM_FILE_INSERT: &str = "
    INSERT INTO album_file (album_id, media_item_id) VALUES (?1, ?2)
";

const DB_ALBUM_DELETE_ALL: &str = "DELETE FROM album";
const DB_ALBUM_FILE_DELETE_ALL: &str = "DELETE FROM album_file";

const DB_CLASSIFIED_FILE_CREATE: &str = "
    CREATE TABLE IF NOT EXISTS classified_file (
        classified_file_id INTEGER PRIMARY KEY AUTOINCREMENT,
        file_path TEXT NOT NULL,
        quick_file_type TEXT,
        known_file_type TEXT, -- matched pattern variant, NULL if unmatched
        known_file_type_value TEXT, -- captured value (e.g. photo id), if any
        file_size INTEGER -- size of the file in bytes
    )
";
const DB_CLASSIFIED_FILE_INSERT: &str = "
    INSERT INTO classified_file (file_path, quick_file_type, known_file_type, known_file_type_value, file_size)
    VALUES (?1, ?2, ?3, ?4, ?5)
";
const DB_CLASSIFIED_FILE_DELETE_ALL: &str = "DELETE FROM classified_file";

const DB_CLASSIFIED_DIR_CREATE: &str = "
    CREATE TABLE IF NOT EXISTS classified_dir (
        classified_dir_id INTEGER PRIMARY KEY AUTOINCREMENT,
        dir_path TEXT NOT NULL,
        known_dir_type TEXT, -- matched pattern variant, NULL if unmatched
        known_dir_value TEXT -- captured value (e.g. year), if any
    )
";
const DB_CLASSIFIED_DIR_INSERT: &str = "
    INSERT INTO classified_dir (dir_path, known_dir_type, known_dir_value)
    VALUES (?1, ?2, ?3)
";
const DB_CLASSIFIED_DIR_DELETE_ALL: &str = "DELETE FROM classified_dir";

async fn open_conn(path: &str) -> anyhow::Result<(Database, Connection)> {
    // No encryption: the on-disk file stays a standard SQLite file that users can
    // open directly with sqlite3
    let db = Builder::new_local(path).build().await?;
    let conn = db.connect()?;
    Ok((db, conn))
}

async fn db_prepare(conn: &Connection) -> anyhow::Result<()> {
    conn.execute(DB_MEDIA_ITEM_CREATE, ()).await?;
    conn.execute(DB_PERSON_CREATE, ()).await?;
    conn.execute(DB_MEDIA_PERSON_CREATE, ()).await?;
    conn.execute(DB_ALBUM_CREATE, ()).await?;
    conn.execute(DB_ALBUM_FILE_CREATE, ()).await?;
    conn.execute(DB_CLASSIFIED_FILE_CREATE, ()).await?;
    conn.execute(DB_CLASSIFIED_DIR_CREATE, ()).await?;

    // Clear existing rows before re-scanning. Delete children before parents so
    // foreign keys hold (media_person and album_file both reference media_item).
    conn.execute(DB_MEDIA_PERSON_DELETE_ALL, ()).await?;
    conn.execute(DB_ALBUM_FILE_DELETE_ALL, ()).await?;
    conn.execute(DB_MEDIA_ITEM_DELETE_ALL, ()).await?;
    conn.execute(DB_PERSON_DELETE_ALL, ()).await?;
    conn.execute(DB_ALBUM_DELETE_ALL, ()).await?;
    conn.execute(DB_CLASSIFIED_FILE_DELETE_ALL, ()).await?;
    conn.execute(DB_CLASSIFIED_DIR_DELETE_ALL, ()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DB_MEDIA_ITEM_SELECT_ALL: &str = "
        SELECT media_path, long_hash, short_hash, quick_file_type,
            accurate_file_type, media_info, guessed_datetime, modified_at, created_at
        FROM media_item
    ";

    /// Fetch the single row a query is expected to return, erroring if there is none.
    async fn one_row(conn: &Connection, sql: &str, params: impl IntoParams) -> anyhow::Result<Row> {
        query_one(conn, sql, params)
            .await?
            .ok_or_else(|| anyhow!("query returned no rows: {sql}"))
    }

    async fn media_item_id_of(conn: &Connection, media_path: &str) -> anyhow::Result<String> {
        let row = one_row(
            conn,
            "SELECT media_item_id FROM media_item WHERE media_path = ?1",
            [media_path],
        )
        .await?;
        Ok(row.get::<String>(0)?)
    }

    #[tokio::test]
    #[ignore]
    async fn test_select_all() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let (_db, conn) = open_conn("db.sqlite").await?;
        let mut rows = conn.query(DB_MEDIA_ITEM_SELECT_ALL, ()).await?;
        while let Some(row) = rows.next().await? {
            let media_path: String = row.get(0)?;
            println!("media_path: {}", media_path);
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_db_scan() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let (_db, conn) = open_conn(":memory:").await?;
        let container: Arc<dyn FileSystem> = Arc::new(OsFileSystem::new("test"));
        run_db_scan(container, &conn).await?;

        let mut rows = conn
            .query(
                "SELECT media_path, quick_file_type FROM media_item ORDER BY media_path",
                (),
            )
            .await?;
        let mut results: Vec<(String, String)> = Vec::new();
        while let Some(row) = rows.next().await? {
            results.push((row.get::<String>(0)?, row.get::<String>(1)?));
        }

        assert!(
            results
                .iter()
                .any(|(path, ftype)| path == "Canon_40D.jpg" && ftype == "Media")
        );
        assert!(
            results
                .iter()
                .any(|(path, ftype)| path == "Hello.mp4" && ftype == "Media")
        );

        // Video dimensions, duration and orientation are derived from track
        // metadata; duration is null for photos.
        let row = one_row(
            &conn,
            "SELECT width, height, duration_ms, orientation FROM media_item WHERE media_path = ?1",
            ["Hello.mp4"],
        )
        .await?;
        let w: Option<i64> = row.get(0)?;
        let h: Option<i64> = row.get(1)?;
        let dur: Option<i64> = row.get(2)?;
        let orient: Option<String> = row.get(3)?;
        assert_eq!(w, Some(854));
        assert_eq!(h, Some(480));
        assert_eq!(dur, Some(5000));
        assert_eq!(orient.as_deref(), Some("landscape"));

        let photo_dur: Option<i64> = one_row(
            &conn,
            "SELECT duration_ms FROM media_item WHERE media_path = ?1",
            ["Canon_40D.jpg"],
        )
        .await?
        .get(0)?;
        assert_eq!(photo_dur, None, "photos have no duration");

        // `kind` tags each item as photo ('p') or video ('v').
        let video_kind: String = one_row(
            &conn,
            "SELECT kind FROM media_item WHERE media_path = ?1",
            ["Hello.mp4"],
        )
        .await?
        .get(0)?;
        assert_eq!(video_kind, "v");
        let photo_kind: String = one_row(
            &conn,
            "SELECT kind FROM media_item WHERE media_path = ?1",
            ["Canon_40D.jpg"],
        )
        .await?
        .get(0)?;
        assert_eq!(photo_kind, "p");

        // EXIF Orientation is split into display_mirrored/display_rotate, which
        // are never NULL. Canon_40D.jpg is orientation 1, the no-op transform.
        let row = one_row(
            &conn,
            "SELECT display_mirrored, display_rotate FROM media_item WHERE media_path = ?1",
            ["Canon_40D.jpg"],
        )
        .await?;
        let photo_display: (bool, i64) = (row.get(0)?, row.get(1)?);
        assert_eq!(photo_display, (false, 0));
        // Videos have no EXIF orientation, so they default to no transform.
        let row = one_row(
            &conn,
            "SELECT display_mirrored, display_rotate FROM media_item WHERE media_path = ?1",
            ["Hello.mp4"],
        )
        .await?;
        let video_display: (bool, i64) = (row.get(0)?, row.get(1)?);
        assert_eq!(
            video_display,
            (false, 0),
            "no EXIF defaults to no transform"
        );

        // With no supplemental or EXIF date, the video's guessed date comes from
        // its embedded track creation time rather than the file timestamps.
        let guessed: Option<String> = one_row(
            &conn,
            "SELECT guessed_datetime FROM media_item WHERE media_path = ?1",
            ["Hello.mp4"],
        )
        .await?
        .get(0)?;
        assert_eq!(guessed.as_deref(), Some("2024-04-18T11:24:26+00:00"));

        Ok(())
    }

    #[tokio::test]
    async fn test_db_scan_classifies_paths() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let (_db, conn) = open_conn(":memory:").await?;
        let container: Arc<dyn FileSystem> = Arc::new(OsFileSystem::new("test"));
        run_db_scan(container, &conn).await?;

        // Every scanned file is recorded, matched or not.
        let file_count: i64 = one_row(&conn, "SELECT COUNT(*) FROM classified_file", ())
            .await?
            .get(0)?;
        assert!(file_count > 0, "expected classified_file rows");

        // A csv is classified as an iCloud album csv.
        let known: Option<String> = one_row(
            &conn,
            "SELECT known_file_type FROM classified_file WHERE file_path = ?1",
            ["ic-album-sample.csv"],
        )
        .await?
        .get(0)?;
        assert_eq!(known.as_deref(), Some("IcpAlbumCsv"));

        // Canon_40D.jpg matches no known pattern, so it is stored unmatched.
        let unmatched: Option<String> = one_row(
            &conn,
            "SELECT known_file_type FROM classified_file WHERE file_path = ?1",
            ["Canon_40D.jpg"],
        )
        .await?
        .get(0)?;
        assert_eq!(unmatched, None);

        Ok(())
    }

    use std::fs;
    use zip::ZipWriter;
    use zip::write::FileOptions;

    fn create_zip_of_test_dir(output_path: &Path) -> anyhow::Result<()> {
        let file = fs::File::create(output_path)?;
        let mut zip = ZipWriter::new(file);
        let options = FileOptions::<()>::default();

        let root = Path::new("test");
        let walker = fs::read_dir(root)?;
        for entry in walker {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                let name = path
                    .file_name()
                    .ok_or_else(|| anyhow!("No file name"))?
                    .to_str()
                    .ok_or_else(|| anyhow!("Invalid UTF-8"))?;
                zip.start_file(name, options)?;
                let mut f = fs::File::open(&path)?;
                std::io::copy(&mut f, &mut zip)?;
            }
        }
        zip.finish()?;
        Ok(())
    }

    #[tokio::test]
    async fn test_db_scan_zip() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let zip_path = Path::new("target/test_output.zip");
        create_zip_of_test_dir(zip_path)?;

        let (_db, conn) = open_conn(":memory:").await?;
        let container: Arc<dyn FileSystem> =
            Arc::new(ZipFileSystem::new(zip_path.to_string_lossy().as_ref())?);

        run_db_scan(container, &conn).await?;

        let mut rows = conn
            .query(
                "SELECT media_path, quick_file_type FROM media_item ORDER BY media_path",
                (),
            )
            .await?;
        let mut results: Vec<(String, String)> = Vec::new();
        while let Some(row) = rows.next().await? {
            results.push((row.get::<String>(0)?, row.get::<String>(1)?));
        }

        assert!(
            results
                .iter()
                .any(|(path, ftype)| path == "Canon_40D.jpg" && ftype == "Media")
        );
        assert!(
            results
                .iter()
                .any(|(path, ftype)| path == "Hello.mp4" && ftype == "Media")
        );

        // Cleanup
        let _ = fs::remove_file(zip_path);
        Ok(())
    }

    #[tokio::test]
    async fn test_db_scan_with_album() -> anyhow::Result<()> {
        use std::io::Write;
        crate::test_util::setup_log();
        let test_dir = Path::new("target/test_db_album");
        if test_dir.exists() {
            fs::remove_dir_all(test_dir)?;
        }
        fs::create_dir_all(test_dir)?;

        // Copy a file
        let src_file = Path::new("test/Canon_40D.jpg");
        let dest_file = test_dir.join("Canon_40D.jpg");
        fs::copy(src_file, &dest_file)?;

        // Create album CSV
        let album_path = test_dir.join("album.csv");
        let mut file = fs::File::create(&album_path)?;
        writeln!(file, "Images")?;
        writeln!(file, "Canon_40D.jpg")?;

        let (_db, conn) = open_conn(":memory:").await?;
        let test_dir_str = test_dir.to_string_lossy();
        let container: Arc<dyn FileSystem> = Arc::new(OsFileSystem::new(&test_dir_str));
        run_db_scan(container, &conn).await?;

        // Verify Album: the id is the stable hash of the album path.
        let row = one_row(&conn, "SELECT album_id, title, album_path FROM album", ()).await?;
        let album_id: String = row.get(0)?;
        let title: String = row.get(1)?;
        let path: String = row.get(2)?;
        assert_eq!(title, "album");
        assert_eq!(path, "album.csv");
        assert_eq!(album_id, crate::util::album_id_for("album.csv"));

        // Verify Album Files: membership is stored by media_item_id and joins
        // back to the media item's path.
        let row = one_row(
            &conn,
            "SELECT m.media_path FROM album_file af
             JOIN media_item m ON m.media_item_id = af.media_item_id",
            (),
        )
        .await?;
        let path: String = row.get(0)?;
        assert_eq!(path, "Canon_40D.jpg");

        // Cleanup
        fs::remove_dir_all(test_dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn test_db_scan_records_people_and_location() -> anyhow::Result<()> {
        use std::io::Write;
        crate::test_util::setup_log();
        let test_dir = Path::new("target/test_db_people_location");
        if test_dir.exists() {
            fs::remove_dir_all(test_dir)?;
        }
        fs::create_dir_all(test_dir)?;

        // A media file with an adjacent Google supplemental json carrying named
        // people and geo coordinates. Canon_40D.jpg has no EXIF GPS coords, so
        // the location must come from the supplemental data.
        fs::copy("test/Canon_40D.jpg", test_dir.join("Canon_40D.jpg"))?;
        let mut supp = fs::File::create(test_dir.join("Canon_40D.jpg.supplemental-metadata.json"))?;
        write!(
            supp,
            r#"{{
                "geoData": {{ "latitude": -21.6303194, "longitude": 152.2605444 }},
                "people": [{{ "name": "Tim Tam" }}, {{ "name": "Ada Lovelace" }}]
            }}"#
        )?;

        let (_db, conn) = open_conn(":memory:").await?;
        let test_dir_str = test_dir.to_string_lossy();
        let container: Arc<dyn FileSystem> = Arc::new(OsFileSystem::new(&test_dir_str));
        run_db_scan(container, &conn).await?;

        // Location promoted into columns.
        let row = one_row(
            &conn,
            "SELECT latitude, longitude FROM media_item WHERE media_path = ?1",
            ["Canon_40D.jpg"],
        )
        .await?;
        let lat: Option<f64> = row.get(0)?;
        let long: Option<f64> = row.get(1)?;
        assert_eq!(lat.map(|v| format!("{v:.4}")).as_deref(), Some("-21.6303"));
        assert_eq!(long.map(|v| format!("{v:.4}")).as_deref(), Some("152.2605"));

        // Location also stored as a geohash for prefix-based clustering.
        let geohash: Option<String> = one_row(
            &conn,
            "SELECT geohash FROM media_item WHERE media_path = ?1",
            ["Canon_40D.jpg"],
        )
        .await?
        .get(0)?;
        assert_eq!(
            geohash.as_deref(),
            Some(crate::util::geohash_encode(-21.6303194, 152.2605444, GEOHASH_PRECISION).as_str())
        );

        // EXIF camera and dimension details promoted into columns.
        let row = one_row(
            &conn,
            "SELECT camera_make, camera_model, width, height
             FROM media_item WHERE media_path = ?1",
            ["Canon_40D.jpg"],
        )
        .await?;
        let make: Option<String> = row.get(0)?;
        let model: Option<String> = row.get(1)?;
        let width: Option<i64> = row.get(2)?;
        let height: Option<i64> = row.get(3)?;
        assert_eq!(make.as_deref(), Some("Canon"));
        assert_eq!(model.as_deref(), Some("Canon EOS 40D"));
        assert!(width.is_some_and(|w| w > 0), "width recorded");
        assert!(height.is_some_and(|h| h > 0), "height recorded");

        // People normalized into a `person` table (stable ids) linked via
        // media_person; joinable back to the media item.
        let mut rows = conn
            .query(
                "SELECT p.name FROM person p
             JOIN media_person mp ON mp.person_id = p.person_id
             JOIN media_item m ON m.media_item_id = mp.media_item_id
             WHERE m.media_path = ?1 ORDER BY p.name",
                ["Canon_40D.jpg"],
            )
            .await?;
        let mut names: Vec<String> = Vec::new();
        while let Some(row) = rows.next().await? {
            names.push(row.get::<String>(0)?);
        }
        assert_eq!(names, vec!["Ada Lovelace", "Tim Tam"]);

        // The person id is the stable content hash of the lowercased name.
        let tim_id: String = one_row(
            &conn,
            "SELECT person_id FROM person WHERE name = ?1",
            ["Tim Tam"],
        )
        .await?
        .get(0)?;
        assert_eq!(tim_id, crate::util::person_id_for("TIM TAM"));

        // media_item_id is the stable hash of the media path.
        let mid = media_item_id_of(&conn, "Canon_40D.jpg").await?;
        assert_eq!(mid, crate::util::media_item_id_for("Canon_40D.jpg"));

        fs::remove_dir_all(test_dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn test_db_scan_rerun() -> anyhow::Result<()> {
        use std::io::Write;
        crate::test_util::setup_log();
        let test_dir = Path::new("target/test_db_album_rerun");
        if test_dir.exists() {
            fs::remove_dir_all(test_dir)?;
        }
        fs::create_dir_all(test_dir)?;

        // A media file plus an album CSV referencing it, so the first scan
        // populates both album and album_file.
        fs::copy("test/Canon_40D.jpg", test_dir.join("Canon_40D.jpg"))?;
        let album_path = test_dir.join("album.csv");
        let mut file = fs::File::create(&album_path)?;
        writeln!(file, "Images")?;
        writeln!(file, "Canon_40D.jpg")?;

        let (_db, conn) = open_conn(":memory:").await?;
        let test_dir_str = test_dir.to_string_lossy();
        let container: Arc<dyn FileSystem> = Arc::new(OsFileSystem::new(&test_dir_str));

        // First run populates album (1 row) and album_file (1 row).
        run_db_scan(container.clone(), &conn).await?;
        let id_first = media_item_id_of(&conn, "Canon_40D.jpg").await?;

        // Second run must not hit "FOREIGN KEY constraint failed" while clearing
        // the previous run's rows.
        run_db_scan(container, &conn).await?;

        // And the rebuild leaves exactly one of each, not duplicates.
        let album_count: i64 = one_row(&conn, "SELECT COUNT(*) FROM album", ())
            .await?
            .get(0)?;
        assert_eq!(album_count, 1, "album rows after re-run");
        let album_file_count: i64 = one_row(&conn, "SELECT COUNT(*) FROM album_file", ())
            .await?
            .get(0)?;
        assert_eq!(album_file_count, 1, "album_file rows after re-run");

        // The media_item id is reproducible: a clear + rescan yields the same id,
        // unlike the previous autoincrement rowid.
        assert_eq!(
            id_first,
            media_item_id_of(&conn, "Canon_40D.jpg").await?,
            "media_item_id stable across runs"
        );

        fs::remove_dir_all(test_dir)?;
        Ok(())
    }
}

/// Test-only: generates `docs/db-schema.md` from the `CREATE TABLE` statements
/// above and verifies the committed copy is current.
#[cfg(test)]
mod db_schema_docs;

/// Test-only: checks every SQL snippet in `docs/db-example-queries.md` runs
/// against a scanned database.
#[cfg(test)]
mod db_example_queries;
