# TODO

- [x] wmv
- [x] remove PTSYNC_TZ env var
- [x] source code documentation tidy up.
  - documentation only at site of logic (not in calling functions)
  - simplify, succinct, clear and targeted, do not explain the obvious
- [ ] remove overlapping tests, prefer high level blackbox tests
  - high level test corpus that tests all file formats and edge cases in one
- [ ] info command output should be in markdown and show all info we parse
- [ ] understand timezones
  - we want preferred paths to be wall clock time
- [ ] live photo support
  - Apple maker note parsing
  - TEXT content_identifier - store in db
  - video is a sidecar to the still
- [ ] xmp import
  - INTEGER rating
  - TEXT label
  - TEXT title
  - TEXT description
  - INTEGER favorite
  - INTEGER archived
  - `--skip-xmp` support in all commands



- [ ] what can we do to make it more appealing or findable to new users?
  - target audience? - I wrote this just for me but others might find it useful

- [ ] add multi language support (see development.md) and make the CLI output localized.
  - Implemented as a tiny home-baked `t!` macro + pure-Rust `src/i18n/<code>.rs` catalogs (English/es/zh/hi). See "Localization" in Development.md.
  - Language from env: `PTSYNC_LANG` override, then POSIX `LC_ALL`/`LC_MESSAGES`/`LANG`.
  - "English in source": the English string is both the message and the lookup key.
  - `--help` descriptions are localized too (via `i18n::localize_command`); clap's own chrome ("Usage:", "Options:", "Print help") stays English.
  - Per-language READMEs: `README.es.md` / `README.zh.md` / `README.hi.md` with a switcher row.
  - `cargo test` fails if any message (console or `--help`) is missing a translation (or placeholders mismatch).
  - Note: `debug!` traces, the `info` command's detail report, and tool-internal "this is a bug" diagnostics (e.g. overlapping built-in regexes) are intentionally left in English.

