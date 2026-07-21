
use super::db_utils::query_one;
use anyhow::anyhow;
use tracing::{error, info};
use turso::Connection;

// ---- media_item ----------------------------------------------------------

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

// OR REPLACE, keyed on the path-derived media_item_id: a fresh scan of the same
// path overwrites the old row. This matters on resume — a file whose bytes
// changed in place (so `run_db_scan` re-inspects it) gets its new metadata,
// rather than the stale first row winning as it would with OR IGNORE. Files that
// are unchanged are filtered out before inspection and so never reach here.
pub(super) const DB_MEDIA_ITEM_INSERT: &str = "
    INSERT OR REPLACE INTO media_item (media_path, long_hash, short_hash, quick_file_type,
        accurate_file_type, media_info, guessed_datetime, modified_at, created_at, file_size,
        latitude, longitude, camera_make, camera_model, width, height,
        duration_ms, orientation, display_mirrored, display_rotate, geohash, kind, media_item_id)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)
";
pub(super) const DB_MEDIA_ITEM_ID_BY_PATH: &str =
    "SELECT media_item_id FROM media_item WHERE media_path = ?1";
// Every recorded media row's id and size, read at the start of a run so already
// recorded files can be skipped instead of re-hashed (see `load_recorded_media`).
pub(super) const DB_MEDIA_ITEM_LOAD_RECORDED: &str =
    "SELECT media_item_id, file_size FROM media_item";
// Album building and many info queries look media rows up by `media_path`
const DB_MEDIA_ITEM_PATH_INDEX: &str =
    "CREATE INDEX IF NOT EXISTS idx_media_item_media_path ON media_item (media_path)";
const DB_MEDIA_ITEM_DELETE_ALL: &str = "
    DELETE FROM media_item
";

// ---- person / media_person ----------------------------------------------

const DB_PERSON_CREATE: &str = "
    CREATE TABLE IF NOT EXISTS person (
        person_id TEXT PRIMARY KEY, -- stable hash of the lowercased name
        name TEXT NOT NULL
    )
";
pub(super) const DB_PERSON_INSERT: &str = "
    INSERT OR IGNORE INTO person (person_id, name) VALUES (?1, ?2)
";
const DB_PERSON_DELETE_ALL: &str = "DELETE FROM person";

const DB_MEDIA_PERSON_CREATE: &str = "
    CREATE TABLE IF NOT EXISTS media_person (
        media_item_id TEXT,
        person_id TEXT,
        UNIQUE(media_item_id, person_id), -- one link per item/person, so re-scans stay additive
        FOREIGN KEY(media_item_id) REFERENCES media_item(media_item_id),
        FOREIGN KEY(person_id) REFERENCES person(person_id)
    )
";
pub(super) const DB_MEDIA_PERSON_INSERT: &str = "
    INSERT OR IGNORE INTO media_person (media_item_id, person_id) VALUES (?1, ?2)
";
const DB_MEDIA_PERSON_DELETE_ALL: &str = "DELETE FROM media_person";

// ---- album / album_file --------------------------------------------------

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
        UNIQUE(album_id, media_item_id), -- one link per album/item, so re-scans stay additive
        FOREIGN KEY(album_id) REFERENCES album(album_id),
        FOREIGN KEY(media_item_id) REFERENCES media_item(media_item_id)
    )
";

pub(super) const DB_ALBUM_INSERT: &str = "
    INSERT OR IGNORE INTO album (album_id, title, album_path) VALUES (?1, ?2, ?3)
";

pub(super) const DB_ALBUM_FILE_INSERT: &str = "
    INSERT OR IGNORE INTO album_file (album_id, media_item_id) VALUES (?1, ?2)
";

const DB_ALBUM_DELETE_ALL: &str = "DELETE FROM album";
const DB_ALBUM_FILE_DELETE_ALL: &str = "DELETE FROM album_file";

// ---- run -----------------------------------------------------------------

// One row per distinct --input, not per invocation: re-running the same input
// resumes its run (see `run_db_scan`), reusing this row so classified_file /
// classified_dir stay a single, refreshed set rather than one per attempt.
const DB_RUN_CREATE: &str = "
    CREATE TABLE IF NOT EXISTS run (
        run_id INTEGER PRIMARY KEY AUTOINCREMENT,
        run_input TEXT NOT NULL, -- the --input value passed on the CLI
        run_date DATETIME DEFAULT CURRENT_TIMESTAMP -- when this input was first scanned
    )
";
pub(super) const DB_RUN_INSERT: &str = "INSERT INTO run (run_input) VALUES (?1)";
// Find an input's existing run so a re-run resumes it rather than starting a new
// one. Newest first, though in practice there is at most one row per input.
pub(super) const DB_RUN_FIND_BY_INPUT: &str =
    "SELECT run_id FROM run WHERE run_input = ?1 ORDER BY run_id DESC LIMIT 1";
const DB_RUN_DELETE_ALL: &str = "DELETE FROM run";

// ---- classified_file / classified_dir ------------------------------------

const DB_CLASSIFIED_FILE_CREATE: &str = "
    CREATE TABLE IF NOT EXISTS classified_file (
        classified_file_id INTEGER PRIMARY KEY AUTOINCREMENT,
        run_id INTEGER, -- the run that produced this row
        file_path TEXT NOT NULL,
        quick_file_type TEXT,
        known_file_type TEXT, -- matched pattern variant, NULL if unmatched
        known_file_type_value TEXT, -- captured value (e.g. photo id), if any
        file_size INTEGER, -- size of the file in bytes
        FOREIGN KEY(run_id) REFERENCES run(run_id)
    )
";
pub(super) const DB_CLASSIFIED_FILE_INSERT: &str = "
    INSERT INTO classified_file (run_id, file_path, quick_file_type, known_file_type, known_file_type_value, file_size)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6)
";
const DB_CLASSIFIED_FILE_DELETE_ALL: &str = "DELETE FROM classified_file";
pub(super) const DB_CLASSIFIED_FILE_DELETE_BY_RUN: &str =
    "DELETE FROM classified_file WHERE run_id = ?1";

const DB_CLASSIFIED_DIR_CREATE: &str = "
    CREATE TABLE IF NOT EXISTS classified_dir (
        classified_dir_id INTEGER PRIMARY KEY AUTOINCREMENT,
        run_id INTEGER, -- the run that produced this row
        dir_path TEXT NOT NULL,
        known_dir_type TEXT, -- matched pattern variant, NULL if unmatched
        known_dir_value TEXT, -- captured value (e.g. year), if any
        FOREIGN KEY(run_id) REFERENCES run(run_id)
    )
";
pub(super) const DB_CLASSIFIED_DIR_INSERT: &str = "
    INSERT INTO classified_dir (run_id, dir_path, known_dir_type, known_dir_value)
    VALUES (?1, ?2, ?3, ?4)
";
const DB_CLASSIFIED_DIR_DELETE_ALL: &str = "DELETE FROM classified_dir";
pub(super) const DB_CLASSIFIED_DIR_DELETE_BY_RUN: &str =
    "DELETE FROM classified_dir WHERE run_id = ?1";

// ---- schema version & build order ----------------------------------------

// Bump whenever a CREATE TABLE statement changes. `user_version` defaults to 0.
// Consider migrating users existing DBs on incrementing. The `schema_hash_is_current`
// test fails on any schema change to force this bump; see it before editing.
const DB_SCHEMA_VERSION: i64 = 3;

// The whole schema, as the ordered statements `db_prepare` runs to build it:
// tables first (parents before children so foreign keys resolve), then indexes.
// Single source of truth — `db_prepare` builds from these, the docs generator
// reads them, and `schema_hash_is_current` hashes them.
pub(crate) const SCHEMA_TABLE_STATEMENTS: [&str; 8] = [
    DB_MEDIA_ITEM_CREATE,
    DB_PERSON_CREATE,
    DB_MEDIA_PERSON_CREATE,
    DB_ALBUM_CREATE,
    DB_ALBUM_FILE_CREATE,
    DB_RUN_CREATE,
    DB_CLASSIFIED_FILE_CREATE,
    DB_CLASSIFIED_DIR_CREATE,
];
const SCHEMA_INDEX_STATEMENTS: [&str; 1] = [DB_MEDIA_ITEM_PATH_INDEX];

pub(super) async fn db_prepare(conn: &Connection, clear: bool) -> anyhow::Result<()> {
    let version = query_one(conn, "PRAGMA user_version", ())
        .await?
        .map(|row| row.get::<i64>(0))
        .transpose()?
        .unwrap_or(0);

    // Version 0 is a brand-new database with no schema yet, so there is nothing
    // to reconcile. Any other version that doesn't match is a schema mismatch we
    // can only resolve by rebuilding, which throws away data — so we only do it
    // when the user opted into --clear, and otherwise refuse.
    if version != 0 && version != DB_SCHEMA_VERSION {
        if clear {
            info!("DB schema version {version} != {DB_SCHEMA_VERSION}, rebuilding from scratch");
            db_drop_all(conn).await?;
        } else {
            error!(
                "DB schema version {version} does not match expected {DB_SCHEMA_VERSION}. \
                 Re-run with --clear=true to rebuild the database from scratch."
            );
            return Err(anyhow!("DB schema version mismatch"));
        }
    }

    // All create statements are `IF NOT EXISTS`: a no-op when the schema already
    // matches, and a full build on a fresh or just-dropped database. Tables first,
    // then indexes.
    for stmt in SCHEMA_TABLE_STATEMENTS
        .iter()
        .chain(&SCHEMA_INDEX_STATEMENTS)
    {
        conn.execute(*stmt, ()).await?;
    }

    // Clear existing rows only when asked. Delete children before parents so
    // foreign keys hold (media_person and album_file reference media_item;
    // classified_file and classified_dir reference run).
    if clear {
        conn.execute(DB_MEDIA_PERSON_DELETE_ALL, ()).await?;
        conn.execute(DB_ALBUM_FILE_DELETE_ALL, ()).await?;
        conn.execute(DB_MEDIA_ITEM_DELETE_ALL, ()).await?;
        conn.execute(DB_PERSON_DELETE_ALL, ()).await?;
        conn.execute(DB_ALBUM_DELETE_ALL, ()).await?;
        conn.execute(DB_CLASSIFIED_FILE_DELETE_ALL, ()).await?;
        conn.execute(DB_CLASSIFIED_DIR_DELETE_ALL, ()).await?;
        conn.execute(DB_RUN_DELETE_ALL, ()).await?;
    }

    conn.execute(&format!("PRAGMA user_version = {DB_SCHEMA_VERSION}"), ())
        .await?;
    Ok(())
}

// Drop every table so the following CREATE statements rebuild them at the current
// schema. Children before parents to satisfy foreign keys.
async fn db_drop_all(conn: &Connection) -> anyhow::Result<()> {
    for table in [
        "media_person",
        "album_file",
        "media_item",
        "person",
        "album",
        "classified_file",
        "classified_dir",
        "run",
    ] {
        conn.execute(&format!("DROP TABLE IF EXISTS {table}"), ())
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Canonical text of the whole schema: line comments dropped and whitespace
    /// collapsed, so only a meaningful SQL change moves the hash — reindenting a
    /// statement or rewording a `-- comment` does not.
    fn canonical_schema() -> String {
        SCHEMA_TABLE_STATEMENTS
            .iter()
            .chain(&SCHEMA_INDEX_STATEMENTS)
            .map(|stmt| {
                stmt.lines()
                    .map(|line| line.split("--").next().unwrap_or(line))
                    .collect::<Vec<_>>()
                    .join(" ")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn schema_hash() -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(canonical_schema().as_bytes());
        hex::encode(hasher.finalize())
    }

    /// Tripwire so a schema change can't ship without a deliberate version bump.
    /// Any edit to a `CREATE TABLE`/`INDEX` statement moves `schema_hash()`, which
    /// makes existing databases on the old `DB_SCHEMA_VERSION` incompatible. When
    /// this fails and the change was intended:
    ///   1. Bump `DB_SCHEMA_VERSION`.
    ///   2. Decide how existing databases move from the previous version to the new
    ///      one. Today `db_prepare` only offers a `--clear` rebuild (which discards
    ///      data); add a real migration there if the data is worth keeping.
    ///   3. Update `EXPECTED_SCHEMA_HASH` below to the value the failure prints.
    #[test]
    fn schema_hash_is_current() {
        const EXPECTED_SCHEMA_HASH: &str =
            "9fd9349c863b762b7926e2f205c1f47b7686f9f674c369b1e0726b416299f030";
        let actual = schema_hash();
        assert_eq!(DB_SCHEMA_VERSION, 3);
        assert_eq!(
            actual, EXPECTED_SCHEMA_HASH,
            "\n\nDatabase schema changed (hash is now {actual}).\n\
             If intentional:\n  \
             1. Bump DB_SCHEMA_VERSION (currently {DB_SCHEMA_VERSION}).\n  \
             2. Consider migrating existing databases from version {DB_SCHEMA_VERSION} to the new \
             version (db_prepare currently only rebuilds on --clear).\n  \
             3. Set EXPECTED_SCHEMA_HASH in this test to {actual}.\n"
        );
    }
}
