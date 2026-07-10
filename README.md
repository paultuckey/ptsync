# ptsync: Photo Takeout Sync

Merge your Google Takeout and iCloud photo exports into one clean, deduplicated,
date-organized archive - stored as ordinary files plus Markdown notes that you fully
own, with no database or proprietary app needed to read it later.

## The problem

Google Takeout and iCloud archives of your photos and videos:

- don't share a common standard for directory and file naming
- don't represent albums in any standard, portable way
- don't merge duplicates - so the same photo from two sources lands twice

If you've got a pile of Takeout zips and iCloud exports and want a single, tidy,
app-independent archive you can back up anywhere, this is for you.

## Features

- **Two sources, one library** - reads Google Takeout *and* iCloud archives, as either a
  zip file or an unpacked directory.
- **Deduplicates by content** - files are matched by checksum, not filename, so the same
  photo exported twice is stored once.
- **Sorted by date** - a `year/month/day` layout built for decades-long archiving, so you
  can act on whole years at a time.
- **Plain-text & future-proof** - every photo and video gets a sibling Markdown file with
  its metadata in [YAML](https://en.wikipedia.org/wiki/YAML) frontmatter, readable in any
  editor and friendly to tools like [Obsidian](https://obsidian.md/).
- **Albums travel with you** - Google (JSON) and iCloud (CSV) albums become Markdown files
  under `albums/`.
- **Non-destructive & repeatable** - additive only, and idempotent: running it again
  produces no changes.

Supported formats: images (JPG, PNG, HEIC, GIF) and video (MP4, MOV). Other file types are
skipped.

> [!NOTE]
> **Your originals are safe.** ptsync is *additive only*: it copies files **into** your
> output directory and never deletes or overwrites your photos, videos, or the notes you
> write. Re-running on the same source changes nothing. (Album files are regenerated each
> run, but any notes you add below the notes marker are preserved - see
> [How it works](#how-it-works).)

## What you get

A sync produces a directory like this:

```
archive/
├── 2024/
│   └── 07/
│       └── 15/
│           ├── 1430-22417.heic          # taken 14:30:22.417
│           ├── 1430-22417.md            # metadata + your editable notes
│           ├── 1430-22417-a1b2c3d.jpg   # same instant, different photo → checksum suffix
│           └── 1430-22417-a1b2c3d.md
├── undated/                             # no reliable date → named by checksum
│   ├── 9f8e7d6.png
│   └── 9f8e7d6.md
└── albums/
    └── summer-trip-2024.md
```

Each photo's Markdown file looks like this - edit the body freely, it's never clobbered:

```markdown
---
datetime: 2024-07-15T14:30:22.417Z
checksum: a1b2c3d4e5f6...
original-paths:
  - Takeout/Google Photos/Photos from 2024/IMG_3986.HEIC
people:
  - "[[Paul]]"
albums:
  - "[[Summer trip 2024]]"
latitude: 12.3456
longitude: -78.9012
---

![](1430-22417.heic)

Add your own notes here - they survive every later run.
```

## Quick start

1. **Export your photos.** Request a [Google Takeout](https://takeout.google.com/) of
   Google Photos, and/or export your [iCloud Photos](https://privacy.apple.com/). You can
   keep them as the downloaded zips - no need to unpack.

2. **Install ptsync.** You'll need Rust and Cargo first
   ([installation instructions](https://www.rust-lang.org/tools/install)), then:

   ```shell
   cargo install --git https://github.com/paultuckey/ptsync.git ptsync
   ```

3. **Preview the sync** with `--dry-run` - this writes nothing, it just prints what would
   happen:

   ```shell
   ptsync sync --dry-run \
     --input "takeout-20250614.zip" \
     --output ~/photo-archive
   ```

4. **Run it for real** by dropping `--dry-run`. Point `--input` at each export in turn
   (zip or directory); the same `--output` accumulates everything:

   ```shell
   ptsync sync --input "takeout-20250614.zip" --output ~/photo-archive
   ptsync sync --input "iCloud Photos"        --output ~/photo-archive
   ```

5. **Browse `~/photo-archive`.** Open it in any file manager, Markdown editor, or Obsidian.

Add `--debug` to any command for verbose logging.

## Commands

| Command | What it does                                                                                                                                          |
| --- |-------------------------------------------------------------------------------------------------------------------------------------------------------|
| `ptsync sync` | The main command - syncs photos, videos and albums into a standardized directory.                                                                     |
| `ptsync info` | Inspect the metadata ptsync would extract from a single photo, video or album.                                                                        |
| `ptsync db`   | Scan an archive into a SQLite [database](docs/db-schema.md) of file metadata (helpful for inspection). [Example queries](docs/db-example-queries.md). |

`sync` also accepts `--skip-markdown`, `--skip-media` and `--skip-albums` to process only
part of an archive. See the full [CLI reference](docs/cli.md) for every option, or run
`ptsync --help`.

## How it works

- **Dates** are read from EXIF metadata, supplemental JSON sidecars (common in Google
  Takeout), or the file's modification time as a fallback.
- **File paths** follow `yyyy/mm/dd/hhmm-ssms.ext` - for example
  `2024/07/15/1430-22417.jpg` is 15 July 2024 at 14:30:22.417. If two *different* photos
  share the same instant, the second gets a checksum suffix
  (`1430-22417-a1b2c3d.jpg`). Files with no determinable date go into `undated/`, named by
  their checksum.
- **Duplicates** are detected by a SHA256 checksum over the file's bytes, so identical
  content is stored only once no matter how it was named or where it came from.
- **Extensions** are corrected by inspecting the file's actual bytes, so a mislabeled
  `.jpg` that is really a `.png` is named correctly.
- **Per-photo Markdown** is written alongside each file. The YAML frontmatter holds
  metadata (date, checksum, original paths, people, albums, GPS); the body is yours to
  edit and is preserved verbatim on every later run.
- **Albums** become Markdown files under `albums/`. The photo list is regenerated each
  run, but anything you write below the `<!-- ptsync:notes -->` marker is kept, so albums
  can be annotated like any other note.

## FAQ

> Why date-based file and directory names? Why put the checksum in the file name sometimes?

Time is the most important thing in archiving - it lets you treat different years
differently. The naming has to stay durable for the *very* long term. Multiple photos can
be taken within the same millisecond, so the checksum makes them unique when needed.

> Why Markdown files?

Markdown is widely supported and human-readable without any special software. As with
[Obsidian](https://obsidian.md/), you can edit the files in any text editor and back the
directories up to any storage you like.

> What format is the short checksum?

The first 7 characters of a SHA256 hash over the file's bytes. Like a Git short hash, it's
a good trade-off between uniqueness and length.

> What is the YAML in the Markdown files for?

Two things: it lets you keep notes on each photo or album that won't be clobbered on later
runs, and it stores metadata in a structured form that software can easily parse.

## License

[MIT](LICENSE) © Paul Tuckey

---

This project is an independent open-source tool and is not affiliated with, sponsored by,
or endorsed by Google, Apple or Dynalist.

Google is a trademark of Google LLC. iCloud is a trademark of Apple Inc. Obsidian is a
trademark of Dynalist Inc.
