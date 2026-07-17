//! Generators for the committed artifacts under `docs/`.
//!
//! Each submodule owns one generated artifact and, run under `UPDATE_DOCS=1`,
//! rewrites it from the current source of truth so it can't silently drift:
//!
//! - [`cli`] — `docs/cli.md`, from the CLI's own `--help` output.
//! - [`db_schema`] — `docs/db-schema.md`, from the `CREATE TABLE` statements.
//! - [`demo`] — `docs/demo.cast` / `docs/demo.gif`, the README demo GIF.
//!
//! All three verify their committed copy on a plain `cargo test`. `demo` checks
//! only the deterministic cast (not the GIF), since re-rendering the GIF is too
//! heavy for every run (see that module for why). The whole tree is
//! `#[cfg(test)]`, so none of it reaches the shipped binary.

mod cli;
mod db_schema;
mod demo;
