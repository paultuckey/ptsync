//! End-to-end tests for the `db` command: each drives `run_db_scan` over a
//! fixture container and asserts the resulting rows. The schema-hash tripwire
//! lives with the schema in `schema.rs`; doc-query validation in
//! `db_example_queries.rs`.

use super::db_utils::test_support::{create_zip_of_test_dir, one_row};
use super::*;
use crate::fs::{OsFileSystem, ZipFileSystem};
use crate::util::GEOHASH_PRECISION;
use std::fs;
use std::path::PathBuf;

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

async fn scan(container: Arc<dyn FileSystem>, conn: &Connection, root: &str) -> anyhow::Result<()> {
    scan_with(container, conn, root, DbScanOpts::default()).await
}

async fn scan_with(
    container: Arc<dyn FileSystem>,
    conn: &Connection,
    root: &str,
    opts: DbScanOpts,
) -> anyhow::Result<()> {
    run_db_scan(container, conn, opts, root, crate::test_util::tz()).await
}

/// A fresh directory under `target/` holding a media file and an album CSV that
/// lists it, so a full scan populates `media_item`, `album` and `album_file`.
fn album_fixture(name: &str) -> anyhow::Result<PathBuf> {
    use std::io::Write;
    let dir = Path::new("target").join(name);
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    fs::create_dir_all(&dir)?;
    fs::copy("test/Canon_40D.jpg", dir.join("Canon_40D.jpg"))?;
    let mut file = fs::File::create(dir.join("album.csv"))?;
    writeln!(file, "Images")?;
    writeln!(file, "Canon_40D.jpg")?;
    Ok(dir)
}

/// What one scan of the `test/` fixtures records: the media rows and their
/// promoted columns, plus the classification of every file it walked.
#[tokio::test]
async fn test_db_scan() -> anyhow::Result<()> {
    crate::test_util::setup_log();
    let (_db, conn) = open_conn(":memory:").await?;
    scan(Arc::new(OsFileSystem::new("test")), &conn, "test").await?;

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
    for path in ["Canon_40D.jpg", "Hello.mp4"] {
        assert!(
            results
                .iter()
                .any(|(p, ftype)| p == path && ftype == "Media"),
            "{path} should be recorded as media"
        );
    }

    // Video dimensions, duration and orientation come from track metadata; a
    // photo has no duration. `kind` tags each item as photo or video, and
    // display_mirrored/display_rotate are never NULL — Canon_40D.jpg is EXIF
    // orientation 1, and a video has no EXIF orientation at all.
    let row = one_row(
        &conn,
        "SELECT width, height, duration_ms, orientation, kind, display_mirrored, display_rotate,
                guessed_datetime
         FROM media_item WHERE media_path = ?1",
        ["Hello.mp4"],
    )
    .await?;
    assert_eq!(row.get::<Option<i64>>(0)?, Some(854));
    assert_eq!(row.get::<Option<i64>>(1)?, Some(480));
    assert_eq!(row.get::<Option<i64>>(2)?, Some(5000));
    assert_eq!(row.get::<Option<String>>(3)?.as_deref(), Some("landscape"));
    assert_eq!(row.get::<String>(4)?, "v");
    assert_eq!((row.get::<bool>(5)?, row.get::<i64>(6)?), (false, 0));
    // With no supplemental or EXIF date, the video falls back to its embedded
    // track creation time rather than the file timestamps.
    assert_eq!(
        row.get::<Option<String>>(7)?.as_deref(),
        Some("2024-04-18T11:24:26+00:00")
    );

    let row = one_row(
        &conn,
        "SELECT duration_ms, kind, display_mirrored, display_rotate
         FROM media_item WHERE media_path = ?1",
        ["Canon_40D.jpg"],
    )
    .await?;
    assert_eq!(row.get::<Option<i64>>(0)?, None, "photos have no duration");
    assert_eq!(row.get::<String>(1)?, "p");
    assert_eq!((row.get::<bool>(2)?, row.get::<i64>(3)?), (false, 0));

    // Every scanned file is recorded in classified_file, matched or not.
    assert!(
        count_of(&conn, "classified_file").await? > 0,
        "expected classified_file rows"
    );
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

/// Both halves of a Live Photo record the identifier that links them, read out
/// of two different places: the still's Apple maker note and the video's
/// QuickTime metadata. Storing it is what lets a query find the pair.
#[tokio::test]
async fn test_db_scan_records_content_identifiers() -> anyhow::Result<()> {
    crate::test_util::setup_log();
    let (_db, conn) = open_conn(":memory:").await?;
    scan(
        Arc::new(OsFileSystem::new("test/live_photo")),
        &conn,
        "test/live_photo",
    )
    .await?;

    let mut rows = conn
        .query(
            "SELECT media_path, kind, content_identifier FROM media_item ORDER BY media_path",
            (),
        )
        .await?;
    let mut found: Vec<(String, String, Option<String>)> = Vec::new();
    while let Some(row) = rows.next().await? {
        found.push((row.get(0)?, row.get(1)?, row.get(2)?));
    }
    assert_eq!(
        found,
        vec![
            (
                "clip.mov".to_string(),
                "v".to_string(),
                Some("11111111-2222-3333-4444-555555555555".to_string())
            ),
            (
                "still.jpg".to_string(),
                "p".to_string(),
                Some("11111111-2222-3333-4444-555555555555".to_string())
            ),
        ]
    );

    // A photo from a camera that writes no Apple maker note leaves the column
    // NULL rather than filling it with something derived.
    let (_db, conn) = open_conn(":memory:").await?;
    scan(Arc::new(OsFileSystem::new("test")), &conn, "test").await?;
    let row = one_row(
        &conn,
        "SELECT content_identifier FROM media_item WHERE media_path = ?1",
        ["Canon_40D.jpg"],
    )
    .await?;
    assert_eq!(row.get::<Option<String>>(0)?, None);
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
    scan(container, &conn, "test").await?;

    let mut rows = conn
        .query("SELECT media_path FROM media_item ORDER BY media_path", ())
        .await?;
    let mut paths: Vec<String> = Vec::new();
    while let Some(row) = rows.next().await? {
        paths.push(row.get::<String>(0)?);
    }
    for path in ["Canon_40D.jpg", "Hello.mp4"] {
        assert!(
            paths.iter().any(|p| p == path),
            "{path} should be recorded from the zip"
        );
    }

    let _ = fs::remove_file(zip_path);
    Ok(())
}

#[tokio::test]
async fn test_db_scan_records_albums() -> anyhow::Result<()> {
    crate::test_util::setup_log();
    let test_dir = album_fixture("test_db_album")?;
    let test_dir_str = test_dir.to_string_lossy();

    let (_db, conn) = open_conn(":memory:").await?;
    scan(
        Arc::new(OsFileSystem::new(&test_dir_str)),
        &conn,
        &test_dir_str,
    )
    .await?;

    // The album id is the stable hash of the album path.
    let row = one_row(&conn, "SELECT album_id, title, album_path FROM album", ()).await?;
    assert_eq!(row.get::<String>(1)?, "album");
    assert_eq!(row.get::<String>(2)?, "album.csv");
    assert_eq!(
        row.get::<String>(0)?,
        crate::util::album_id_for("album.csv")
    );

    // Membership is stored by media_item_id and joins back to the item's path.
    let row = one_row(
        &conn,
        "SELECT m.media_path FROM album_file af
         JOIN media_item m ON m.media_item_id = af.media_item_id",
        (),
    )
    .await?;
    assert_eq!(row.get::<String>(0)?, "Canon_40D.jpg");

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

    // Canon_40D.jpg has no EXIF GPS coords, so the location must come from the
    // supplemental json beside it.
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
    scan(
        Arc::new(OsFileSystem::new(&test_dir_str)),
        &conn,
        &test_dir_str,
    )
    .await?;

    // Location promoted into columns, and also stored as a geohash for
    // prefix-based clustering. EXIF camera and dimension details likewise.
    let row = one_row(
        &conn,
        "SELECT latitude, longitude, geohash, camera_make, camera_model, width, height
         FROM media_item WHERE media_path = ?1",
        ["Canon_40D.jpg"],
    )
    .await?;
    let lat: Option<f64> = row.get(0)?;
    let long: Option<f64> = row.get(1)?;
    assert_eq!(lat.map(|v| format!("{v:.4}")).as_deref(), Some("-21.6303"));
    assert_eq!(long.map(|v| format!("{v:.4}")).as_deref(), Some("152.2605"));
    assert_eq!(
        row.get::<Option<String>>(2)?.as_deref(),
        Some(crate::util::geohash_encode(-21.6303194, 152.2605444, GEOHASH_PRECISION).as_str())
    );
    assert_eq!(row.get::<Option<String>>(3)?.as_deref(), Some("Canon"));
    assert_eq!(
        row.get::<Option<String>>(4)?.as_deref(),
        Some("Canon EOS 40D")
    );
    assert!(row.get::<Option<i64>>(5)?.is_some_and(|w| w > 0));
    assert!(row.get::<Option<i64>>(6)?.is_some_and(|h| h > 0));

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

    // The ids stored are the stable content hashes, so a rescan reproduces them.
    let tim_id: String = one_row(
        &conn,
        "SELECT person_id FROM person WHERE name = ?1",
        ["Tim Tam"],
    )
    .await?
    .get(0)?;
    assert_eq!(tim_id, crate::util::person_id_for("Tim Tam"));
    assert_eq!(
        media_item_id_of(&conn, "Canon_40D.jpg").await?,
        crate::util::media_item_id_for("Canon_40D.jpg")
    );

    fs::remove_dir_all(test_dir)?;
    Ok(())
}

/// Resume as incremental indexing: a second run over the same input skips what
/// is already recorded and takes in only what has since appeared, and `--clear`
/// starts over.
#[tokio::test]
async fn test_db_scan_resume_and_clear() -> anyhow::Result<()> {
    crate::test_util::setup_log();
    let test_dir = album_fixture("test_db_resume")?;
    let test_dir_str = test_dir.to_string_lossy();
    let fs_at = || -> Arc<dyn FileSystem> { Arc::new(OsFileSystem::new(&test_dir_str)) };

    let (_db, conn) = open_conn(":memory:").await?;
    scan(fs_at(), &conn, &test_dir_str).await?;
    let id_first = media_item_id_of(&conn, "Canon_40D.jpg").await?;

    // Second run without --clear resumes the same input: already-recorded media
    // is skipped and the additive tables stay deduped.
    scan(fs_at(), &conn, &test_dir_str).await?;
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

    // A new file appears: it is picked up, and the recorded one is left alone.
    fs::copy("test/Hello.mp4", test_dir.join("Hello.mp4"))?;
    scan(fs_at(), &conn, &test_dir_str).await?;
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
    assert_eq!(count_of(&conn, "run").await?, 1, "the same input resumes");

    // --clear wipes everything, including the run-scoped tables, and rebuilds
    // from scratch without a "FOREIGN KEY constraint failed" on the deletes.
    scan_with(
        fs_at(),
        &conn,
        &test_dir_str,
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

    fs::remove_dir_all(test_dir)?;
    Ok(())
}

/// The resume skip is guarded by size: a file changed in place (same path, new
/// bytes) is re-inspected and its row replaced, not left stale.
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
    let fs_at = || -> Arc<dyn FileSystem> { Arc::new(OsFileSystem::new(&test_dir_str)) };
    let recorded = async |conn: &Connection| -> anyhow::Result<(String, i64)> {
        let row = one_row(
            conn,
            "SELECT long_hash, file_size FROM media_item WHERE media_path = 'photo.jpg'",
            (),
        )
        .await?;
        Ok((row.get(0)?, row.get(1)?))
    };

    scan(fs_at(), &conn, &test_dir_str).await?;
    let (hash_before, size_before) = recorded(&conn).await?;

    // Bytes appended past a JPEG's end marker leave it a valid image but change
    // its size, which is what the resume guard checks.
    let mut f = fs::OpenOptions::new().append(true).open(&media_path)?;
    f.write_all(&[0u8; 4096])?;
    drop(f);

    scan(fs_at(), &conn, &test_dir_str).await?;

    // Still one row for that path, but its recorded content is refreshed.
    let (hash_after, size_after) = recorded(&conn).await?;
    assert_eq!(
        count_of(&conn, "media_item").await?,
        1,
        "the changed file replaces its row, not adds one"
    );
    assert_ne!(hash_before, hash_after, "changed bytes are re-hashed");
    assert_eq!(
        size_after,
        size_before + 4096,
        "recorded size reflects the change"
    );

    fs::remove_dir_all(test_dir)?;
    Ok(())
}

#[tokio::test]
async fn test_db_scan_skip_flags() -> anyhow::Result<()> {
    crate::test_util::setup_log();
    let test_dir = album_fixture("test_db_skip_flags")?;
    let test_dir_str = test_dir.to_string_lossy();
    let fs_at = || -> Arc<dyn FileSystem> { Arc::new(OsFileSystem::new(&test_dir_str)) };

    // --skip-media --skip-albums: classification only.
    {
        let (_db, conn) = open_conn(":memory:").await?;
        scan_with(
            fs_at(),
            &conn,
            &test_dir_str,
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
        let (_db, conn) = open_conn(":memory:").await?;
        scan_with(
            fs_at(),
            &conn,
            &test_dir_str,
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

        scan(fs_at(), &conn, &test_dir_str).await?;

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
        let (_db, conn) = open_conn(":memory:").await?;
        scan_with(
            fs_at(),
            &conn,
            &test_dir_str,
            DbScanOpts {
                skip_albums: true,
                ..Default::default()
            },
        )
        .await?;

        assert_eq!(count_of(&conn, "media_item").await?, 1, "media inspected");
        assert_eq!(count_of(&conn, "album").await?, 0, "album empty");
    }

    fs::remove_dir_all(test_dir)?;
    Ok(())
}
