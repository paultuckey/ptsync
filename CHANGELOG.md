# Changelog

All notable changes to this project are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Releases are cut with `cargo release`, which turns the `[Unreleased]` heading below into the
version being released. See [docs/Release.md](docs/Release.md).

## [Unreleased]

### Added

- **`ptsync sync`** merges Google Takeout and iCloud photo exports — as zip files or unpacked
  directories — into one archive laid out as `yyyy/mm/dd/hhmm-ssms.ext`. The run is additive and
  idempotent: nothing is deleted or overwritten, and a second run over the same source produces no
  changes.
- **Deduplication by content.** Files are matched on a SHA-256 of their bytes, so the same photo
  exported from two sources is stored once regardless of how it was named. Distinct photos sharing
  a capture instant are separated by a short-checksum suffix.
- **Dates read from the file itself** — EXIF for photos, embedded creation time for videos —
  falling back to Google Takeout's JSON sidecars and then the file's modification time. The archive
  is laid out on the photographer's wall clock; `--timezone` / `-z` pins the offset used for the
  values that are instants rather than readings.
- **A Markdown note beside every photo and video**, carrying metadata as YAML frontmatter with a
  body that is preserved verbatim on later runs.
- **Albums as Markdown** under `albums/`, read from Google's JSON and iCloud's CSV. The photo list
  is regenerated each run; anything written below the `<!-- ptsync:notes -->` marker is kept.
- **Live Photos are kept together** — an iPhone's still and its paired video are recognized as one
  item through the Apple content identifier and filed under one name.
- **Extension correction** by content sniffing, so a `.jpg` that is really a PNG is named for what
  it is.
- **S3 output**, with each object's SHA-256 recorded as its native checksum so re-runs skip what is
  already uploaded without re-downloading it.
- **`ptsync info`** prints the metadata ptsync would extract from a single file as Markdown, and
  **`ptsync db`** scans an archive into a SQLite database for inspection.

[Unreleased]: https://github.com/paultuckey/ptsync/compare/v0.1.0...HEAD
