//! End-to-end tests for the `db` command: each drives `run_db_scan` over a
//! fixture container and asserts the resulting rows. The schema-hash tripwire
//! lives with the schema in `schema.rs`; doc-query validation in
//! `db_example_queries.rs`.

use super::db_utils::test_support::{create_zip_of_test_dir, one_row};
use super::*;
use crate::fs::{OsFileSystem, ZipFileSystem};
use crate::util::GEOHASH_PRECISION;
use std::fs;

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
async fn test_db_scan() -> anyhow::Result<()> {
    crate::test_util::setup_log();
    let (_db, conn) = open_conn(":memory:").await?;
    let container: Arc<dyn FileSystem> = Arc::new(OsFileSystem::new("test"));
    run_db_scan(container, &conn, false, "test").await?;

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
    run_db_scan(container, &conn, false, "test").await?;

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

#[tokio::test]
async fn test_db_scan_zip() -> anyhow::Result<()> {
    crate::test_util::setup_log();
    let zip_path = Path::new("target/test_output.zip");
    create_zip_of_test_dir(zip_path)?;

    let (_db, conn) = open_conn(":memory:").await?;
    let container: Arc<dyn FileSystem> =
        Arc::new(ZipFileSystem::new(zip_path.to_string_lossy().as_ref())?);

    run_db_scan(container, &conn, false, "test").await?;

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

    let src_file = Path::new("test/Canon_40D.jpg");
    let dest_file = test_dir.join("Canon_40D.jpg");
    fs::copy(src_file, &dest_file)?;

    let album_path = test_dir.join("album.csv");
    let mut file = fs::File::create(&album_path)?;
    writeln!(file, "Images")?;
    writeln!(file, "Canon_40D.jpg")?;

    let (_db, conn) = open_conn(":memory:").await?;
    let test_dir_str = test_dir.to_string_lossy();
    let container: Arc<dyn FileSystem> = Arc::new(OsFileSystem::new(&test_dir_str));
    run_db_scan(container, &conn, false, &test_dir_str).await?;

    // The album id is the stable hash of the album path.
    let row = one_row(&conn, "SELECT album_id, title, album_path FROM album", ()).await?;
    let album_id: String = row.get(0)?;
    let title: String = row.get(1)?;
    let path: String = row.get(2)?;
    assert_eq!(title, "album");
    assert_eq!(path, "album.csv");
    assert_eq!(album_id, crate::util::album_id_for("album.csv"));

    // Membership is stored by media_item_id and joins back to the media
    // item's path.
    let row = one_row(
        &conn,
        "SELECT m.media_path FROM album_file af
         JOIN media_item m ON m.media_item_id = af.media_item_id",
        (),
    )
    .await?;
    let path: String = row.get(0)?;
    assert_eq!(path, "Canon_40D.jpg");

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
    run_db_scan(container, &conn, false, &test_dir_str).await?;

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
    run_db_scan(container.clone(), &conn, false, &test_dir_str).await?;
    let id_first = media_item_id_of(&conn, "Canon_40D.jpg").await?;

    // Second run without --clear resumes the same input: already-recorded
    // media is skipped and the additive tables stay deduped.
    run_db_scan(container, &conn, false, &test_dir_str).await?;

    // Additive tables hold exactly one of each despite the re-scan.
    let album_count: i64 = one_row(&conn, "SELECT COUNT(*) FROM album", ())
        .await?
        .get(0)?;
    assert_eq!(album_count, 1, "album deduped across runs");
    let album_file_count: i64 = one_row(&conn, "SELECT COUNT(*) FROM album_file", ())
        .await?
        .get(0)?;
    assert_eq!(album_file_count, 1, "album_file deduped across runs");
    let media_item_count: i64 = one_row(&conn, "SELECT COUNT(*) FROM media_item", ())
        .await?
        .get(0)?;
    assert_eq!(media_item_count, 1, "media_item deduped across runs");

    // The media_item id is reproducible: a rescan yields the same stable id.
    assert_eq!(
        id_first,
        media_item_id_of(&conn, "Canon_40D.jpg").await?,
        "media_item_id stable across runs"
    );

    // Re-running the same input resumes its run rather than logging a new
    // one, so there is still exactly one run row and one run's worth of
    // classified rows — refreshed in place, not duplicated.
    let run_count: i64 = one_row(&conn, "SELECT COUNT(*) FROM run", ())
        .await?
        .get(0)?;
    assert_eq!(run_count, 1, "re-running the same input reuses its run row");
    let classified_runs: i64 = one_row(
        &conn,
        "SELECT COUNT(DISTINCT run_id) FROM classified_file",
        (),
    )
    .await?
    .get(0)?;
    assert_eq!(classified_runs, 1, "classified rows refreshed, not duplicated");
    // The single classified_file row for our media file was refreshed, not
    // stacked: a resume must not double-insert it under the same run.
    let classified_files: i64 = one_row(
        &conn,
        "SELECT COUNT(*) FROM classified_file WHERE file_path = 'Canon_40D.jpg'",
        (),
    )
    .await?
    .get(0)?;
    assert_eq!(classified_files, 1, "classified_file refreshed in place on resume");

    // --clear wipes everything, including the run-scoped tables, and rebuilds
    // from scratch without a "FOREIGN KEY constraint failed" on the deletes.
    let container: Arc<dyn FileSystem> = Arc::new(OsFileSystem::new(&test_dir_str));
    run_db_scan(container, &conn, true, &test_dir_str).await?;
    let run_count: i64 = one_row(&conn, "SELECT COUNT(*) FROM run", ())
        .await?
        .get(0)?;
    assert_eq!(run_count, 1, "clear resets the run log to just this run");
    let classified_runs: i64 = one_row(
        &conn,
        "SELECT COUNT(DISTINCT run_id) FROM classified_file",
        (),
    )
    .await?
    .get(0)?;
    assert_eq!(
        classified_runs, 1,
        "clear leaves only the current run's rows"
    );

    fs::remove_dir_all(test_dir)?;
    Ok(())
}

/// Resume as incremental indexing: a second run over the same input keeps the
/// files already recorded and only takes in the ones that have since appeared.
#[tokio::test]
async fn test_db_scan_resumes_and_adds_new_files() -> anyhow::Result<()> {
    crate::test_util::setup_log();
    let test_dir = Path::new("target/test_db_resume_incremental");
    if test_dir.exists() {
        fs::remove_dir_all(test_dir)?;
    }
    fs::create_dir_all(test_dir)?;
    fs::copy("test/Canon_40D.jpg", test_dir.join("Canon_40D.jpg"))?;

    let (_db, conn) = open_conn(":memory:").await?;
    let test_dir_str = test_dir.to_string_lossy();

    // First run records the single file present.
    let container: Arc<dyn FileSystem> = Arc::new(OsFileSystem::new(&test_dir_str));
    run_db_scan(container, &conn, false, &test_dir_str).await?;
    let count: i64 = one_row(&conn, "SELECT COUNT(*) FROM media_item", ())
        .await?
        .get(0)?;
    assert_eq!(count, 1, "first run records the initial file");
    let first_id = media_item_id_of(&conn, "Canon_40D.jpg").await?;

    // A new file appears; re-running the same input picks it up while the
    // already-recorded file is skipped rather than duplicated.
    fs::copy("test/Hello.mp4", test_dir.join("Hello.mp4"))?;
    let container: Arc<dyn FileSystem> = Arc::new(OsFileSystem::new(&test_dir_str));
    run_db_scan(container, &conn, false, &test_dir_str).await?;

    let count: i64 = one_row(&conn, "SELECT COUNT(*) FROM media_item", ())
        .await?
        .get(0)?;
    assert_eq!(count, 2, "second run adds the new file and keeps the old one");
    assert_eq!(
        first_id,
        media_item_id_of(&conn, "Canon_40D.jpg").await?,
        "the already-recorded file keeps its id"
    );
    let run_count: i64 = one_row(&conn, "SELECT COUNT(*) FROM run", ())
        .await?
        .get(0)?;
    assert_eq!(run_count, 1, "the same input resumes its one run");

    fs::remove_dir_all(test_dir)?;
    Ok(())
}

/// The resume skip is guarded by size: a file changed in place (same path,
/// new bytes) is re-inspected and its row replaced, not left stale.
#[tokio::test]
async fn test_db_scan_reinspects_changed_file() -> anyhow::Result<()> {
    use std::io::Write;
    crate::test_util::setup_log();
    let test_dir = Path::new("target/test_db_resume_changed");
    if test_dir.exists() {
        fs::remove_dir_all(test_dir)?;
    }
    fs::create_dir_all(test_dir)?;
    let media_path = test_dir.join("photo.jpg");
    fs::copy("test/Canon_40D.jpg", &media_path)?;

    let (_db, conn) = open_conn(":memory:").await?;
    let test_dir_str = test_dir.to_string_lossy();

    let container: Arc<dyn FileSystem> = Arc::new(OsFileSystem::new(&test_dir_str));
    run_db_scan(container, &conn, false, &test_dir_str).await?;
    let hash_before: String = one_row(
        &conn,
        "SELECT long_hash FROM media_item WHERE media_path = 'photo.jpg'",
        (),
    )
    .await?
    .get(0)?;
    let size_before: i64 = one_row(
        &conn,
        "SELECT file_size FROM media_item WHERE media_path = 'photo.jpg'",
        (),
    )
    .await?
    .get(0)?;

    // Change the file in place. Bytes appended past a JPEG's end marker leave
    // it a valid image but change its size and content, so the guard re-scans.
    let mut f = fs::OpenOptions::new().append(true).open(&media_path)?;
    f.write_all(&[0u8; 4096])?;
    drop(f);

    let container: Arc<dyn FileSystem> = Arc::new(OsFileSystem::new(&test_dir_str));
    run_db_scan(container, &conn, false, &test_dir_str).await?;

    // Still one row for that path, but its recorded content is refreshed.
    let count: i64 = one_row(&conn, "SELECT COUNT(*) FROM media_item", ())
        .await?
        .get(0)?;
    assert_eq!(count, 1, "the changed file replaces its row, not adds one");
    let hash_after: String = one_row(
        &conn,
        "SELECT long_hash FROM media_item WHERE media_path = 'photo.jpg'",
        (),
    )
    .await?
    .get(0)?;
    let size_after: i64 = one_row(
        &conn,
        "SELECT file_size FROM media_item WHERE media_path = 'photo.jpg'",
        (),
    )
    .await?
    .get(0)?;
    assert_ne!(hash_before, hash_after, "changed bytes are re-hashed");
    assert_eq!(
        size_after,
        size_before + 4096,
        "recorded size reflects the change"
    );

    fs::remove_dir_all(test_dir)?;
    Ok(())
}
