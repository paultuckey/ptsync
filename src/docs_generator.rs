//! Generators for the committed artifacts under `docs/`. Each submodule rewrites
//! its artifact from the current source of truth under `UPDATE_DOCS=1`, and
//! verifies the committed copy on a plain `cargo test`, so neither can silently
//! drift:
//!
//! - [`cli`] — `docs/cli.md`, from the CLI's own `--help` output.
//! - [`db_schema`] — `docs/db-schema.md`, from the `CREATE TABLE` statements.
//!
//! The whole tree is `#[cfg(test)]`, so none of it reaches the shipped binary.
//!
//! The README demo GIF is recorded by VHS from `docs/demo.tape`;
//! `tests/sync_snapshot.rs` stands in for it by snapshotting the same sync's
//! console output. See `Development.md`.

mod cli;
mod db_schema;
