# photos-ui3

A tiny, self-contained [GPUI](https://www.gpui.rs) app that lists every row in
the `media_item` table of ptsync's `db.sqlite`.

GPUI is the GPU-accelerated Rust UI framework the **Zed editor** is built on.

Originally I tried Tauri + React and Xilem but settled on GPUI due to performance and UI quality.

It has **zero dependency on the parent `ptsync` crate** — it opens the database
read-only, loads all rows once at startup, and renders them in a
virtual-scrolled list (so 15k+ rows stay smooth).

Features:

- **Filter box** — live-filters the list by path as you type (backspace
  deletes, escape clears).
- **Click a row** — resolves the original file on disk, decodes it on GPUI's
  background executor (so the click is instant), applies EXIF orientation, and
  shows the photo in an in-app preview pane (aspect-preserving, downscaled to
  1600px). Clicking another row while one is decoding just supersedes it.

## Run

```sh
cargo run
```

> **Heads up:** the first build clones and compiles GPUI from Zed's repo (it
> isn't on crates.io), which pulls a large dependency tree and takes a while.
> Requires network access and a working Rust toolchain. macOS builds against
> Metal; Linux needs the usual Vulkan/X11/Wayland dev packages.

By default it reads `../../db.sqlite` (the repo root), resolved relative to the
crate so the working directory doesn't matter. Override with a path argument:

```sh
cargo run -- /path/to/other.sqlite
```

### Where photos are loaded from

`media_path` is stored relative to a Google Takeout root, so the preview tries
`input/Takeout/`, `input/Takeout2/`, `input/Takeout-small/`, `input/`, then the
repo root. Point it elsewhere with the `PTSYNC_MEDIA_ROOT` env var.

### Headless smoke test

```sh
cargo run -- --check
```

Resolves and decodes the first few previewable images, prints their dimensions,
and exits without opening a window. Useful for verifying the preview pipeline
without building/opening a GPUI window.

## What it shows

Each row displays the media path, accurate file type, and human-readable file
size, sorted by path; the preview pane adds the guessed datetime.

## Notes

- Built against GPUI pinned to Zed tag **v0.224.10** (GPUI's API tracks Zed's
  `main` and isn't published to crates.io, so it's pulled as a git dependency at
  a fixed tag for reproducibility).
- Declares an empty `[workspace]` so it builds as its own standalone crate
  rather than being absorbed into the parent project.
- JPEG/PNG/GIF are decoded with the `image` crate; HEIC/HEIF use the pure-Rust
  [`heic`](https://crates.io/crates/heic) crate (no system libraries). Only
  MP4/MOV/AVI (video) *content* rows show a "no preview" message now. Note
  ptsync content-sniffs, so some `.HEIC`-named files whose bytes are actually
  JPEG take the JPEG path.
- Everything ends up as a tightly-packed BGRA buffer — the byte order gpui's
  `RenderImage` expects — wrapped in a single-frame image for the `img()`
  element. The `image` path swaps RGBA→BGRA; the `heic` path decodes straight
  to BGRA (`PixelLayout::Bgra8`).
- Decoding runs on GPUI's background executor, so clicking a row never blocks
  the UI. A monotonic sequence number discards results for rows you've since
  clicked away from.
- Orientation is applied so portrait phone photos display upright: EXIF
  orientation for JPEGs, and the HEIF container's own rotation/mirror transforms
  (handled inside the `heic` crate) for HEIC.
- The filter box is a minimal focus-tracked key handler (not a full text
  editor): it handles typing, backspace, and escape — enough to filter.
