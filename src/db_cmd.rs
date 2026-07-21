//! The `db` command: scan an archive and collect its metadata into a SQLite
//! index. This module owns the flow — open the container, prepare the schema,
//! classify paths, inspect media, and record albums. The pieces it orchestrates
//! live in submodules:
//!
//! A run is keyed on its `--input`, not on the invocation, so re-running the
//! same input resumes it: media already recorded is skipped rather than
//! re-hashed. See [`run_db_scan`].

use crate::classify::{classify_dir, classify_file};
use crate::file_type::QuickFileType;
use crate::fs::{FileSystem, open_input};
use crate::inspect::inspect_media_files;
use crate::progress::Progress;
use crate::util::{ScanInfo, media_item_id_for, scan_fs};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use tokio::runtime;
use tracing::{debug, info, warn};
use turso::Connection;

mod db_utils;
mod record;
pub(crate) mod schema;

use db_utils::{open_conn, query_one};
use record::db_record;
use schema::{
    DB_ALBUM_FILE_INSERT, DB_ALBUM_INSERT, DB_CLASSIFIED_DIR_DELETE_BY_RUN, DB_CLASSIFIED_DIR_INSERT,
    DB_CLASSIFIED_FILE_DELETE_BY_RUN, DB_CLASSIFIED_FILE_INSERT, DB_MEDIA_ITEM_ID_BY_PATH,
    DB_MEDIA_ITEM_LOAD_RECORDED, DB_RUN_FIND_BY_INPUT, DB_RUN_INSERT, db_prepare,
};

const DB_BATCH_SIZE: usize = 100;

pub(crate) fn main(input: &String, output: &str, clear: bool) -> anyhow::Result<()> {
    debug!("Inspecting: {input}");
    let container = open_input(input)?;

    info!("Writing database: {output}");
    let rt = runtime::Builder::new_current_thread().build()?;
    // `container` keeps its own handle so the one moved into the async block is
    // never the last: `S3FileSystem` owns a tokio runtime, and dropping a runtime
    // from inside another runtime's context panics. This way the S3 runtime is
    // released here, on a plain blocking thread, after `block_on` returns.
    let result = rt.block_on(async {
        let (_db, conn) = open_conn(output).await?;
        run_db_scan(container.clone(), &conn, clear, input).await
    });
    drop(container);
    result
}

async fn run_db_scan(
    container: Arc<dyn FileSystem>,
    conn: &Connection,
    clear: bool,
    run_input: &str,
) -> anyhow::Result<()> {
    db_prepare(conn, clear).await?;

    // A run is identified by its input: re-running for same input reuses run row.
    let run_id = match query_one(conn, DB_RUN_FIND_BY_INPUT, [run_input]).await? {
        Some(row) => {
            let id = row.get::<i64>(0)?;
            debug!("Resuming run {id} for input {run_input:?}");
            id
        }
        None => {
            conn.execute(DB_RUN_INSERT, [run_input]).await?;
            conn.last_insert_rowid()
        }
    };

    let files = scan_fs(container.as_ref());
    info!("Found {} files in input", files.len());

    db_classify_paths(conn, &files, run_id).await?;

    // Everything classified as media, before we set aside the ones already done.
    let all_media: Vec<&ScanInfo> = files
        .iter()
        .filter(|m| m.quick_file_type == QuickFileType::Media)
        .collect();

    // Media already recorded by an earlier run are skipped so we don't re-hash
    // them. Only re-hash if size changes.
    let recorded = load_recorded_media(conn).await?;
    let media_si_files: Vec<ScanInfo> = all_media
        .iter()
        .filter(|m| match recorded.get(&media_item_id_for(&m.file_path)) {
            Some(&size) => size != m.file_size as i64,
            None => true,
        })
        .map(|m| (*m).clone())
        .collect();

    let skipped_done = all_media.len() - media_si_files.len();
    if skipped_done > 0 {
        info!(
            "Resuming: {skipped_done} of {} photo and video files already recorded",
            all_media.len()
        );
    }
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

async fn db_classify_paths(
    conn: &Connection,
    files: &[ScanInfo],
    run_id: i64,
) -> anyhow::Result<()> {
    info!("Classifying {} files against known patterns", files.len());

    conn.execute("BEGIN", ()).await?;

    // This run may be a resume of an earlier one: clear its previous
    // classification rows so a re-scan refreshes them in place instead of
    // stacking a second full set under the same run_id.
    conn.execute(DB_CLASSIFIED_FILE_DELETE_BY_RUN, [run_id])
        .await?;
    conn.execute(DB_CLASSIFIED_DIR_DELETE_BY_RUN, [run_id])
        .await?;

    let mut matched_files = 0usize;
    let mut stmt_file = conn.prepare_cached(DB_CLASSIFIED_FILE_INSERT).await?;
    for si in files {
        let known = classify_file(&si.file_path);
        if known.is_some() {
            matched_files += 1;
        }
        stmt_file
            .execute((
                run_id,
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
                run_id,
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

async fn load_recorded_media(conn: &Connection) -> anyhow::Result<HashMap<String, i64>> {
    let mut rows = conn.query(DB_MEDIA_ITEM_LOAD_RECORDED, ()).await?;
    let mut recorded = HashMap::new();
    while let Some(row) = rows.next().await? {
        let id = row.get::<String>(0)?;
        // file_size is always written by db_record; treat a NULL as "unknown"
        // (-1) so it never matches a scanned size and the file is re-inspected.
        let size = row.get::<Option<i64>>(1)?.unwrap_or(-1);
        recorded.insert(id, size);
    }
    Ok(recorded)
}

#[cfg(test)]
mod tests;

/// Test-only: checks every SQL snippet in `docs/db-example-queries.md` runs
/// against a scanned database. (The `docs/db-schema.md` generator that reads
/// `SCHEMA_TABLE_STATEMENTS` lives in the crate's `docs_generator` module.)
#[cfg(test)]
mod db_example_queries;
