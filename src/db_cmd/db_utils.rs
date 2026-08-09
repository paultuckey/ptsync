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
}
