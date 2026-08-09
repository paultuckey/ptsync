//! End-to-end tests for the `db` command: each drives `run_db_scan` over a
//! fixture container and asserts the resulting rows. The schema-hash tripwire
//! lives with the schema in `schema.rs`; doc-query validation in
//! `db_example_queries.rs`.

use super::db_utils::test_support::one_row;
use super::*;
use crate::fs::{OsFileSystem, ZipFileSystem};
use crate::util::GEOHASH_PRECISION;
use std::fs;
use turso::Database;

async fn media_item_id_of(conn: &Connection, media_path: &str) -> anyhow::Result<String> {
    let row = one_row(
        conn,
        "SELECT media_item_id FROM media_item WHERE media_path = ?1",
        [media_path],
    )
    .await?;
    Ok(row.get::<String>(0)?)
}

async fn count_of(conn: &Connection, table: &str) -> anyhow::Result<i64> {
    Ok(one_row(conn, &format!("SELECT COUNT(*) FROM {table}"), ())
        .await?
        .get(0)?)
}

/// Scan a directory into a fresh in-memory database. The `Database` is returned
/// alongside the connection because dropping it closes the connection.
async fn scan_dir(dir: &str, opts: DbScanOpts) -> anyhow::Result<(Database, Connection)> {
    let (db, conn) = open_conn(":memory:").await?;
    let container: Arc<dyn FileSystem> = Arc::new(OsFileSystem::new(dir));
    run_db_scan(container, &conn, opts, dir, crate::test_util::tz()).await?;
    Ok((db, conn))
}

/// A temp input directory holding one photo and an album CSV listing it: enough
/// for a full scan to populate `media_item`, `album` and `album_file`.
fn photo_and_album_dir() -> anyhow::Result<tempfile::TempDir> {
    use std::io::Write;
    let dir = tempfile::tempdir()?;
    fs::copy("test/Canon_40D.jpg", dir.path().join("Canon_40D.jpg"))?;
    let mut file = fs::File::create(dir.path().join("album.csv"))?;
    writeln!(file, "Images")?;
    writeln!(file, "Canon_40D.jpg")?;
    Ok(dir)
}

#[tokio::test]
async fn test_db_scan() -> anyhow::Result<()> {
    crate::test_util::setup_log();
    let (_db, conn) = scan_dir("test", DbScanOpts::default()).await?;

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

    // Video dimensions, duration and orientation come from track metadata.
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

    // display_mirrored/display_rotate are never NULL. Canon_40D.jpg is
    // orientation 1, the no-op transform.
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

    // With no supplemental or EXIF date, the video falls back to its embedded
    // track creation time rather than the file timestamps.
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
    let (_db, conn) = scan_dir("test", DbScanOpts::default()).await?;

    // Every scanned file is recorded, matched or not.
    assert!(
        count_of(&conn, "classified_file").await? > 0,
        "expected classified_file rows"
    );

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

/// A zip is meant to be interchangeable with the directory it was made from, so
/// every content-derived column has to come out the same. The date columns are
/// left out on purpose: a zip entry carries no creation time, so a file with no
/// embedded date legitimately reads differently from the two containers.
#[tokio::test]
async fn test_db_scan_dir_and_zip_agree() -> anyhow::Result<()> {
    crate::test_util::setup_log();
    const CONTENT_COLUMNS: &str = "SELECT media_path, quick_file_type, accurate_file_type, kind,
            long_hash, short_hash, file_size, width, height, duration_ms, orientation
         FROM media_item ORDER BY media_path";

    async fn content_rows(conn: &Connection) -> anyhow::Result<Vec<String>> {
        let mut rows = conn.query(CONTENT_COLUMNS, ()).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            let mut cells = Vec::new();
            for i in 0..11 {
                cells.push(format!("{:?}", row.get_value(i)?));
            }
            out.push(cells.join("|"));
        }
        Ok(out)
    }

    let (_db, dir_conn) = scan_dir("test", DbScanOpts::default()).await?;
    let dir_rows = content_rows(&dir_conn).await?;
    assert!(!dir_rows.is_empty(), "the fixture directory has media");

    let zip = crate::test_util::build_zip("test")?;
    let zip_path = zip.path().to_string_lossy().to_string();
    let (_zip_db, zip_conn) = open_conn(":memory:").await?;
    let container: Arc<dyn FileSystem> = Arc::new(ZipFileSystem::new(&zip_path)?);
    run_db_scan(
        container,
        &zip_conn,
        DbScanOpts::default(),
        &zip_path,
        crate::test_util::tz(),
    )
    .await?;

    assert_eq!(dir_rows, content_rows(&zip_conn).await?);
    Ok(())
}

#[tokio::test]
async fn test_db_scan_with_album() -> anyhow::Result<()> {
    crate::test_util::setup_log();
    let dir = photo_and_album_dir()?;
    let (_db, conn) = scan_dir(&dir.path().to_string_lossy(), DbScanOpts::default()).await?;

    // The album id is the stable hash of the album path.
    let row = one_row(&conn, "SELECT album_id, title, album_path FROM album", ()).await?;
    let album_id: String = row.get(0)?;
    let title: String = row.get(1)?;
    let path: String = row.get(2)?;
    assert_eq!(title, "album");
    assert_eq!(path, "album.csv");
    assert_eq!(album_id, crate::util::album_id_for("album.csv"));

    // Membership is stored by media_item_id and joins back to the item's path.
    let row = one_row(
        &conn,
        "SELECT m.media_path FROM album_file af
         JOIN media_item m ON m.media_item_id = af.media_item_id",
        (),
    )
    .await?;
    let path: String = row.get(0)?;
    assert_eq!(path, "Canon_40D.jpg");

    Ok(())
}

#[tokio::test]
async fn test_db_scan_records_people_and_location() -> anyhow::Result<()> {
    use std::io::Write;
    crate::test_util::setup_log();
    let dir = tempfile::tempdir()?;

    // Canon_40D.jpg has no EXIF GPS coords, so the location must come from the
    // supplemental json beside it.
    fs::copy("test/Canon_40D.jpg", dir.path().join("Canon_40D.jpg"))?;
    let mut supp = fs::File::create(dir.path().join("Canon_40D.jpg.supplemental-metadata.json"))?;
    write!(
        supp,
        r#"{{
            "geoData": {{ "latitude": -21.6303194, "longitude": 152.2605444 }},
            "people": [{{ "name": "Tim Tam" }}, {{ "name": "Ada Lovelace" }}]
        }}"#
    )?;

    let (_db, conn) = scan_dir(&dir.path().to_string_lossy(), DbScanOpts::default()).await?;

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

    Ok(())
}

/// The three ways a second scan of the same input can go: nothing changed, a
/// file has appeared, and `--clear`. A run is keyed on its `--input`, so all of
/// them resume the one run rather than starting another.
#[tokio::test]
async fn test_db_scan_resume_and_clear() -> anyhow::Result<()> {
    crate::test_util::setup_log();
    let dir = photo_and_album_dir()?;
    let dir_str = dir.path().to_string_lossy().to_string();

    let (_db, conn) = open_conn(":memory:").await?;
    let rescan = async |conn: &Connection, opts: DbScanOpts| -> anyhow::Result<()> {
        let container: Arc<dyn FileSystem> = Arc::new(OsFileSystem::new(&dir_str));
        run_db_scan(container, conn, opts, &dir_str, crate::test_util::tz()).await
    };

    rescan(&conn, DbScanOpts::default()).await?;
    let id_first = media_item_id_of(&conn, "Canon_40D.jpg").await?;

    // Nothing changed: already-recorded media is skipped and the additive tables
    // stay deduped.
    rescan(&conn, DbScanOpts::default()).await?;
    assert_eq!(count_of(&conn, "album").await?, 1, "album deduped");
    assert_eq!(
        count_of(&conn, "album_file").await?,
        1,
        "album_file deduped"
    );
    assert_eq!(
        count_of(&conn, "media_item").await?,
        1,
        "media_item deduped"
    );
    assert_eq!(
        id_first,
        media_item_id_of(&conn, "Canon_40D.jpg").await?,
        "media_item_id stable across runs"
    );
    assert_eq!(
        count_of(&conn, "run").await?,
        1,
        "re-running the same input reuses its run row"
    );
    let classified_runs: i64 = one_row(
        &conn,
        "SELECT COUNT(DISTINCT run_id) FROM classified_file",
        (),
    )
    .await?
    .get(0)?;
    assert_eq!(
        classified_runs, 1,
        "classified rows refreshed, not duplicated"
    );
    let classified_files: i64 = one_row(
        &conn,
        "SELECT COUNT(*) FROM classified_file WHERE file_path = 'Canon_40D.jpg'",
        (),
    )
    .await?
    .get(0)?;
    assert_eq!(
        classified_files, 1,
        "classified_file refreshed in place on resume"
    );

    // Resume as incremental indexing: a new file is taken in, and the recorded
    // one keeps its id rather than being duplicated or re-hashed.
    fs::copy("test/Hello.mp4", dir.path().join("Hello.mp4"))?;
    rescan(&conn, DbScanOpts::default()).await?;
    assert_eq!(
        count_of(&conn, "media_item").await?,
        2,
        "the new file is added and the old one kept"
    );
    assert_eq!(
        id_first,
        media_item_id_of(&conn, "Canon_40D.jpg").await?,
        "the already-recorded file keeps its id"
    );
    assert_eq!(count_of(&conn, "run").await?, 1, "still the one run");

    // --clear wipes everything, including the run-scoped tables, and rebuilds
    // from scratch without a "FOREIGN KEY constraint failed" on the deletes.
    rescan(
        &conn,
        DbScanOpts {
            clear: true,
            ..Default::default()
        },
    )
    .await?;
    assert_eq!(
        count_of(&conn, "run").await?,
        1,
        "clear resets the run log to just this run"
    );
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

    Ok(())
}

/// The resume skip is guarded by size: a file changed in place (same path,
/// new bytes) is re-inspected and its row replaced, not left stale.
#[tokio::test]
async fn test_db_scan_reinspects_changed_file() -> anyhow::Result<()> {
    use std::io::Write;
    crate::test_util::setup_log();
    let dir = tempfile::tempdir()?;
    let media_path = dir.path().join("photo.jpg");
    fs::copy("test/Canon_40D.jpg", &media_path)?;
    let dir_str = dir.path().to_string_lossy().to_string();

    let (_db, conn) = open_conn(":memory:").await?;
    let rescan = async |conn: &Connection| -> anyhow::Result<()> {
        let container: Arc<dyn FileSystem> = Arc::new(OsFileSystem::new(&dir_str));
        run_db_scan(
            container,
            conn,
            DbScanOpts::default(),
            &dir_str,
            crate::test_util::tz(),
        )
        .await
    };

    rescan(&conn).await?;
    let row = one_row(
        &conn,
        "SELECT long_hash, file_size FROM media_item WHERE media_path = 'photo.jpg'",
        (),
    )
    .await?;
    let hash_before: String = row.get(0)?;
    let size_before: i64 = row.get(1)?;

    // Bytes appended past a JPEG's end marker leave it a valid image but change
    // its size, which is what the resume guard checks.
    let mut f = fs::OpenOptions::new().append(true).open(&media_path)?;
    f.write_all(&[0u8; 4096])?;
    drop(f);

    rescan(&conn).await?;

    // Still one row for that path, but its recorded content is refreshed.
    assert_eq!(
        count_of(&conn, "media_item").await?,
        1,
        "the changed file replaces its row, not adds one"
    );
    let row = one_row(
        &conn,
        "SELECT long_hash, file_size FROM media_item WHERE media_path = 'photo.jpg'",
        (),
    )
    .await?;
    let hash_after: String = row.get(0)?;
    let size_after: i64 = row.get(1)?;
    assert_ne!(hash_before, hash_after, "changed bytes are re-hashed");
    assert_eq!(
        size_after,
        size_before + 4096,
        "recorded size reflects the change"
    );

    Ok(())
}

#[tokio::test]
async fn test_db_scan_skip_flags() -> anyhow::Result<()> {
    crate::test_util::setup_log();
    let dir = photo_and_album_dir()?;
    let dir_str = dir.path().to_string_lossy().to_string();

    // --skip-media --skip-albums: classification only.
    {
        let (_db, conn) = scan_dir(
            &dir_str,
            DbScanOpts {
                skip_media: true,
                skip_albums: true,
                ..Default::default()
            },
        )
        .await?;

        assert!(
            count_of(&conn, "classified_file").await? > 0,
            "classified_file is still written"
        );
        assert_eq!(count_of(&conn, "media_item").await?, 0, "media_item empty");
        assert_eq!(count_of(&conn, "album").await?, 0, "album empty");
        assert_eq!(count_of(&conn, "album_file").await?, 0, "album_file empty");
    }

    // --skip-media alone still records albums, but with no links, since each
    // member lookup finds no media_item row. A later full run fills them in,
    // because a resumed run re-visits the album pass.
    {
        let (_db, conn) = scan_dir(
            &dir_str,
            DbScanOpts {
                skip_media: true,
                ..Default::default()
            },
        )
        .await?;

        assert_eq!(count_of(&conn, "media_item").await?, 0, "media_item empty");
        assert_eq!(count_of(&conn, "album").await?, 1, "album row recorded");
        assert_eq!(
            count_of(&conn, "album_file").await?,
            0,
            "no links without media_item rows"
        );

        let container: Arc<dyn FileSystem> = Arc::new(OsFileSystem::new(&dir_str));
        run_db_scan(
            container,
            &conn,
            DbScanOpts::default(),
            &dir_str,
            crate::test_util::tz(),
        )
        .await?;

        assert_eq!(
            count_of(&conn, "media_item").await?,
            1,
            "media now inspected"
        );
        assert_eq!(count_of(&conn, "album").await?, 1, "album not duplicated");
        assert_eq!(
            count_of(&conn, "album_file").await?,
            1,
            "the follow-up run backfills the link"
        );
    }

    // --skip-albums alone leaves media untouched.
    {
        let (_db, conn) = scan_dir(
            &dir_str,
            DbScanOpts {
                skip_albums: true,
                ..Default::default()
            },
        )
        .await?;

        assert_eq!(count_of(&conn, "media_item").await?, 1, "media inspected");
        assert_eq!(count_of(&conn, "album").await?, 0, "album empty");
    }

    Ok(())
}
