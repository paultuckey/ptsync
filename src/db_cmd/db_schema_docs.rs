//! Generates `docs/db-schema.md` as a mermaid ER diagram from the `CREATE TABLE`
//! statements in the parent module, and verifies the committed copy is current.
//!
//! Run `UPDATE_DOCS=1 cargo test` to regenerate the doc after changing the
//! schema; a plain `cargo test` (locally and in CI) fails if it is stale.

const DOC_PATH: &str = "docs/db-schema.md";

/// Every `CREATE TABLE` statement, in the order the tables are created.
fn create_statements() -> Vec<&'static str> {
    vec![
        super::DB_MEDIA_ITEM_CREATE,
        super::DB_MEDIA_PERSON_CREATE,
        super::DB_ALBUM_CREATE,
        super::DB_ALBUM_FILE_CREATE,
        super::DB_CLASSIFIED_FILE_CREATE,
        super::DB_CLASSIFIED_DIR_CREATE,
    ]
}

struct Column {
    name: String,
    col_type: String,
    keys: Vec<&'static str>,
}

struct ForeignKey {
    column: String,
    ref_table: String,
}

struct Table {
    name: String,
    columns: Vec<Column>,
    foreign_keys: Vec<ForeignKey>,
}

/// Drop `-- ...` line comments so they don't interfere with parsing.
fn strip_comments(sql: &str) -> String {
    sql.lines()
        .map(|line| match line.find("--") {
            Some(i) => &line[..i],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Split a `CREATE TABLE (...)` body on commas that are not nested inside
/// parentheses (e.g. a `FOREIGN KEY(col) REFERENCES t(col)` clause).
fn split_top_level(body: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for ch in body.chars() {
        match ch {
            '(' => {
                depth += 1;
                cur.push(ch);
            }
            ')' => {
                depth -= 1;
                cur.push(ch);
            }
            ',' if depth == 0 => {
                parts.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    let last = cur.trim();
    if !last.is_empty() {
        parts.push(last.to_string());
    }
    parts
}

/// The substring between the first `open` and the next following `close`.
fn between(s: &str, open: char, close: char) -> &str {
    let start = s.find(open).map(|i| i + 1).unwrap_or(0);
    let end = s[start..].find(close).map(|i| start + i).unwrap_or(s.len());
    &s[start..end]
}

fn parse_table(create_sql: &str) -> anyhow::Result<Table> {
    use anyhow::anyhow;
    let sql = strip_comments(create_sql);
    let open = sql
        .find('(')
        .ok_or_else(|| anyhow!("CREATE TABLE has an opening paren"))?;
    let close = sql
        .rfind(')')
        .ok_or_else(|| anyhow!("CREATE TABLE has a closing paren"))?;

    // The table name is the last whitespace-delimited token before `(`.
    let name = sql[..open]
        .split_whitespace()
        .last()
        .ok_or_else(|| anyhow!("table name"))?
        .to_string();

    let mut columns = Vec::new();
    let mut foreign_keys = Vec::new();

    for part in split_top_level(&sql[open + 1..close]) {
        let upper = part.to_uppercase();
        if upper.starts_with("FOREIGN KEY") {
            // FOREIGN KEY(col) REFERENCES ref_table(ref_col)
            let column = between(&part, '(', ')').trim().to_string();
            let references = upper
                .find("REFERENCES")
                .ok_or_else(|| anyhow!("FOREIGN KEY clause missing REFERENCES"))?;
            let after = &part[references + "REFERENCES".len()..];
            let ref_table = after
                .trim_start()
                .split(|c: char| c == '(' || c.is_whitespace())
                .next()
                .unwrap_or("")
                .to_string();
            foreign_keys.push(ForeignKey { column, ref_table });
            continue;
        }
        // Table-level constraints are not columns.
        if upper.starts_with("PRIMARY KEY")
            || upper.starts_with("UNIQUE")
            || upper.starts_with("CHECK")
            || upper.starts_with("CONSTRAINT")
        {
            continue;
        }

        let mut tokens = part.split_whitespace();
        let Some(col_name) = tokens.next() else {
            continue;
        };
        let col_type = tokens.next().unwrap_or("").to_string();
        let mut keys = Vec::new();
        if upper.contains("PRIMARY KEY") {
            keys.push("PK");
        }
        if upper.contains("UNIQUE") {
            keys.push("UK");
        }
        columns.push(Column {
            name: col_name.to_string(),
            col_type,
            keys,
        });
    }

    // Flag columns backing a foreign key.
    for fk in &foreign_keys {
        if let Some(col) = columns.iter_mut().find(|c| c.name == fk.column)
            && !col.keys.contains(&"FK")
        {
            col.keys.push("FK");
        }
    }

    Ok(Table {
        name,
        columns,
        foreign_keys,
    })
}

fn generate() -> anyhow::Result<String> {
    let tables: Vec<Table> = create_statements()
        .iter()
        .map(|s| parse_table(s))
        .collect::<anyhow::Result<_>>()?;

    let mut out = String::new();
    out.push_str("# Database schema\n\n");
    out.push_str(
        "<!-- Generated by the `db_schema_docs` test from the `CREATE TABLE` statements in `db_cmd.rs`. -->\n",
    );
    out.push_str("<!-- Do not edit by hand. Run `UPDATE_DOCS=1 cargo test` to regenerate. -->\n\n");
    out.push_str("```mermaid\nerDiagram\n");

    // Relationships first, then an entity block per table.
    for table in &tables {
        for fk in &table.foreign_keys {
            out.push_str(&format!(
                "    {} ||--o{{ {} : \"{}\"\n",
                fk.ref_table, table.name, fk.column
            ));
        }
    }
    for table in &tables {
        out.push_str(&format!("    {} {{\n", table.name));
        for col in &table.columns {
            let keys = if col.keys.is_empty() {
                String::new()
            } else {
                format!(" {}", col.keys.join(","))
            };
            out.push_str(&format!("        {} {}{}\n", col.col_type, col.name, keys));
        }
        out.push_str("    }\n");
    }
    out.push_str("```\n");
    Ok(out)
}

#[test]
fn db_schema_docs_up_to_date() -> anyhow::Result<()> {
    let generated = generate()?;

    if std::env::var_os("UPDATE_DOCS").is_some() {
        if let Some(dir) = std::path::Path::new(DOC_PATH).parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(DOC_PATH, &generated)?;
        return Ok(());
    }

    let existing = std::fs::read_to_string(DOC_PATH).unwrap_or_default();
    assert_eq!(
        existing, generated,
        "{DOC_PATH} is out of date. Regenerate with:\n\n    UPDATE_DOCS=1 cargo test\n"
    );
    Ok(())
}
