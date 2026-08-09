//! Shared database plumbing for the `db_cmd` submodules.

use turso::{Builder, Connection, Database, IntoParams, Row};

/// Open (or create) a local SQLite database. Unencrypted, so the file stays one
/// users can open directly with sqlite3.
pub(super) async fn open_conn(path: &str) -> anyhow::Result<(Database, Connection)> {
    let db = Builder::new_local(path).build().await?;
    let conn = db.connect()?;
    Ok((db, conn))
}

/// The first row, with the rest drained so the statement runs to completion.
pub(super) async fn query_one(
    conn: &Connection,
    sql: &str,
    params: impl IntoParams,
) -> anyhow::Result<Option<Row>> {
    let mut rows = conn.query(sql, params).await?;
    let first = rows.next().await?;
    while rows.next().await?.is_some() {}
    Ok(first)
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::query_one;
    use anyhow::anyhow;
    use std::path::Path;
    use turso::{Connection, IntoParams, Row};

    /// Errors when the query returns no row at all.
    pub(crate) async fn one_row(
        conn: &Connection,
        sql: &str,
        params: impl IntoParams,
    ) -> anyhow::Result<Row> {
        query_one(conn, sql, params)
            .await?
            .ok_or_else(|| anyhow!("query returned no rows: {sql}"))
    }

    /// Zip the `test/` directory flat, base names only, to exercise the
    /// zip-container scan path.
    pub(crate) fn create_zip_of_test_dir(output_path: &Path) -> anyhow::Result<()> {
        use std::fs;
        use zip::ZipWriter;
        use zip::write::FileOptions;

        let file = fs::File::create(output_path)?;
        let mut zip = ZipWriter::new(file);
        let options = FileOptions::<()>::default();

        let root = Path::new("test");
        for entry in fs::read_dir(root)? {
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
}
