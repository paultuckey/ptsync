//! Verifies that every SQL snippet in `docs/db-example-queries.md` is valid by
//! running it against a freshly scanned database.

use super::run_db_scan;
use crate::fs::{FileSystem, OsFileSystem};
use rusqlite::Connection;
use std::sync::Arc;

const DOC_PATH: &str = "docs/db-example-queries.md";

/// Pull out the contents of every fenced ` ```sqlite ` block in the markdown.
fn extract_sql_blocks(markdown: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    for line in markdown.lines() {
        match &mut current {
            None => {
                if line.trim() == "```sqlite" {
                    current = Some(String::new());
                }
            }
            Some(block) => {
                if line.trim() == "```" {
                    blocks.push(block.trim().to_string());
                    current = None;
                } else {
                    block.push_str(line);
                    block.push('\n');
                }
            }
        }
    }
    blocks
}

#[test]
fn db_example_queries_are_valid() -> anyhow::Result<()> {
    crate::test_util::setup_log();

    // Build a database with the current schema from the test fixtures.
    let conn = Connection::open_in_memory()?;
    let container: Arc<dyn FileSystem> = Arc::new(OsFileSystem::new("test"));
    run_db_scan(container, &conn)?;

    let markdown = std::fs::read_to_string(DOC_PATH)?;
    let queries = extract_sql_blocks(&markdown);
    assert!(
        queries.len() >= 10,
        "expected around 10 example queries in {DOC_PATH}, found {}",
        queries.len()
    );

    for (i, sql) in queries.iter().enumerate() {
        // prepare() compiles the SQL; iterating the rows executes it fully.
        let run = || -> rusqlite::Result<()> {
            let mut stmt = conn.prepare(sql)?;
            let mut rows = stmt.query([])?;
            while rows.next()?.is_some() {}
            Ok(())
        };
        run().map_err(|e| {
            anyhow::anyhow!(
                "example query #{} in {DOC_PATH} failed: {e}\n\n{sql}",
                i + 1
            )
        })?;
    }

    Ok(())
}
