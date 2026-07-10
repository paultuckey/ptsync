
# Development

## Rust code

Let's use the most basic rust we can to make the code as approachable as possible.

- Don't use lifetimes
- Don't use `unsafe` or `expect()`
- Only use `unwrap()` in tests
- Use `.clone()` to avoid hard things
- Don't use `async`/`await` (this type of I/O heavy work may not benefit that much)
- Use `anyhow::Result` for fallible functions and propagate errors with `?`.
- Write unit tests within the module (typically a `tests` module at the bottom of the file), and call `crate::test_util::setup_log()` at the start of tests to enable logging output.

## Technical goals

- User should never lose a media file, an album or a metadata file during processing
- Processing the same source zip or directory multiple times should result in no changes
- It should run as quickly as possible
- It should work and be documented clearly for the languages of at least half to world population: 
  - English (primary language, source code comments), Mandarin, Hindi, Spanish

## Tech Stack

Keep the tech stack as small and as simple as possible. Use only the basics.

- **Language**: Rust
- **CLI Framework**: `clap`
- **Logging**: `tracing`
- **Database**: `rusqlite` (SQLite)
- **Error Handling**: `anyhow`
- **Serialization**: `serde`, `serde_json`, `yaml-rust2`
- **Regex**: `regex` (for file pattern matching)

## Project Structure

The source code is located in `src/`:

-   **`main.rs`**: Entry point. Defines the CLI structure using `clap` and orchestrates subcommands.
-   **`*_cmd.rs`**: Implementations for specific CLI commands:
  -   `sync_cmd.rs`: Core logic for syncing files, deduplication, and writing to the output directory.
  -   `db_cmd.rs`: Logic for scanning files and populating a SQLite database with metadata. Also stores each file/directory's known-pattern classification (via `classify.rs`) in the `classified_file`/`classified_dir` tables.
  -   `info_cmd.rs`: Inspects and displays details for a single file.
-   **`classify.rs`**: Classifies input directory/zip paths against known Google Takeout / iCloud file and directory patterns using regex. Consumed by `db_cmd.rs` (not a standalone command).
-   **`media.rs`**: Core data structures (`MediaFileInfo`, `MediaFileDerivedInfo`) and logic for extracting metadata, calculating checksums, and deriving target paths/dates.
-   **`album.rs`**: Logic for parsing album metadata (from CSV or JSON) and generating album Markdown files.
-   **`markdown.rs`**: Utilities for reading/writing Markdown files and managing YAML frontmatter.
-   **`util.rs`**: General utilities, including the `PsContainer` trait which abstracts file system access (supporting both directories and zip files).
-   **`test_util.rs`**: Helper functions for testing, primarily logging setup.

## Key Concepts

-   **PsContainer**: An abstraction to treat directories and zip files uniformly.
-   **ScanInfo**: Basic information about a file found during a scan.
-   **MediaFileInfo**: detailed metadata about a media file (EXIF, checksum, etc.).
-   **Supplemental Info**: JSON sidecar files (often from Google Takeout) containing metadata like creation time and GPS coordinates.

## Commands

```shell
cargo fmt
```

```shell
cargo clippy --tests
```

```shell
cargo run
```

```shell
cargo test
```

```shell
cargo update
```

```shell
cargo build
```

Verify help is working:

```shell
cargo run -- --help
```

Test a single photo file:

```shell
cargo run -- info --debug --root "test" --input "Canon_40D.jpg"
```

Test a single album file:

```shell
cargo run -- info --debug --root "test/takeout1" --input "Google Photos/album1/metadata.json"
```

Make a database for takeout:

```shell
cargo run -- db --debug --input "input/Takeout"
```

Make a database for iCloud:

```shell
cargo run -- db --debug --input "input/iCloud Photos"
```



Dry run a sync operation:

```shell
cargo run -- \
  sync --debug --dry-run \
    --input "input/takeout-20250614T030613Z-1-001.zip" \
    --output "output/archive"
```

Sync a directory:

```shell
cargo run -- sync --debug --input "input/Takeout-small" --output "output/archive-small"
```

```shell
cargo run -- sync --input "input/takeout" --output "output/archive"
```

```shell
cargo run -- sync --input "input/icloud photos" --output "output/archive"
```


## Output

Console output is based on rsync. 

```sh
rsync --dry-run -a --verbose ../input/takeout-small/ ../output/takeout-small/
```

## Zip File Debugging


```sh
zipinfo -m input/takeout-20250614T030613Z-1-001.zip > output/takeout-list.txt
```

## Exif debugging

```shell
brew install exiftool
```

```shell
exiftool "input/iCloud Photos/Photos/IMG_5071.HEIC"
```

```shell
exiftool "input/Takeout/Google Photos/Photos from 2024/IMG_3986.HEIC" > a.txt
exiftool "input/iCloud Photos/Photos/IMG_3986.HEIC" > b.txt
```

## Update docs

Update `docs/cli.md` and `docs/db-schema.md`:

```shell
UPDATE_DOCS=1 cargo test
```

## Notes

The same photo from different sources give different sizes:

```shell
ls -la "input/iCloud Photos/Photos/IMG_5071.HEIC"
ls -la "input/Takeout/Google Photos/Photos from 2025/IMG_5071.HEIC"
```
