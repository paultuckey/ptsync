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
//! The third generator — the README demo GIF (`docs/demo.cast` / `docs/demo.gif`)
//! — lives in `tests/demo.rs` instead: it needs the *built* binary to capture
//! real output, which an integration test gets for free via `CARGO_BIN_EXE`.

mod cli;
mod db_schema;
