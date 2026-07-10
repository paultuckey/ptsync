//! Verifies that every SQL snippet in `docs/db-example-queries.md` is valid by
//! running it against a freshly scanned database.

use super::{open_conn, run_db_scan};
use crate::fs::{FileSystem, OsFileSystem};
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

#[tokio::test]
async fn db_example_queries_are_valid() -> anyhow::Result<()> {
    crate::test_util::setup_log();

    // Build a database with the current schema from the test fixtures.
    let (_db, conn) = open_conn(":memory:").await?;
    let container: Arc<dyn FileSystem> = Arc::new(OsFileSystem::new("test"));
    run_db_scan(container, &conn, false, "test").await?;

    let markdown = std::fs::read_to_string(DOC_PATH)?;
    let queries = extract_sql_blocks(&markdown);
    assert!(
        queries.len() >= 10,
        "expected around 10 example queries in {DOC_PATH}, found {}",
        queries.len()
    );

    for (i, sql) in queries.iter().enumerate() {
        // query() compiles the SQL; draining the rows executes it fully.
        let result: turso::Result<()> = async {
            let mut rows = conn.query(sql, ()).await?;
            while rows.next().await?.is_some() {}
            Ok(())
        }
        .await;
        result.map_err(|e| {
            anyhow::anyhow!(
                "example query #{} in {DOC_PATH} failed: {e}\n\n{sql}",
                i + 1
            )
        })?;
    }

    Ok(())
}
