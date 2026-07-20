//! Generators for the committed artifacts under `docs/`.
//!
//! Each submodule owns one generated artifact and, run under `UPDATE_DOCS=1`,
//! rewrites it from the current source of truth so it can't silently drift:
//!
//! - [`cli`] — `docs/cli.md`, from the CLI's own `--help` output.
//! - [`db_schema`] — `docs/db-schema.md`, from the `CREATE TABLE` statements.
//!
//! Both are pure in-process generators and verify their committed copy on a
//! plain `cargo test`. The whole tree is `#[cfg(test)]`, so none of it reaches
//! the shipped binary.
//!
//! The README demo GIF (`docs/demo.gif`) is recorded by
//! VHS from `docs/demo.tape`. `tests/sync_snapshot.rs` stands in — it snapshots the same sync's console
//! output and fails when that drifts. See `Development.md`.

mod cli;
mod db_schema;
