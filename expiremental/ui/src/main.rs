//! Minimal, self-contained [GPUI](https://www.gpui.rs) app that browses the
//! `media_item` table of ptsync's SQLite database.
//!
//! GPUI is the GPU-accelerated Rust UI framework that the Zed editor is built
//! on — this is the same app as `expiremental/ui` (which uses Xilem), rewritten
//! against Zed's toolkit. It depends on *nothing* from the parent `ptsync`
//! crate: it opens `db.sqlite` read-only, loads all rows once at startup, and
//! shows:
//!   * a filter box that live-filters the list by path,
//!   * a virtual-scrolled list (so 15k+ rows stay smooth), and
//!   * a clickable row → decodes the actual photo from disk and previews it.
//!
//! Decoding runs on GPUI's background executor, so clicking a row is instant;
//! the decoded result is sent back and shown when ready. JPEG/PNG/GIF go through
//! the `image` crate (with EXIF orientation applied); HEIC/HEIF go through the
//! pure-Rust `heic` crate. A monotonic sequence number discards results for rows
//! you've since clicked away from.
//!
//! Run from this directory with `cargo run`. The database path defaults to
//! `../../db.sqlite`; pass another path as the first argument to override.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    App, Application, Bounds, Context, FocusHandle, KeyDownEvent, RenderImage, SharedString,
    TitlebarOptions, Window, WindowBounds, WindowOptions, div, img, prelude::*, px, rgb, size,
    uniform_list,
};
use heic::{DecoderConfig, PixelLayout};
use image::{DynamicImage, Frame, ImageDecoder, ImageReader, RgbaImage};
use rusqlite::{Connection, OpenFlags};

/// One photo / media row — just the fields this viewer needs.
struct MediaItem {
    media_path: String,
    /// Pre-lowercased path, so filtering doesn't re-allocate on every keystroke.
    path_lower: String,
    accurate_file_type: String,
    file_size: i64,
    guessed_datetime: Option<String>,
}

/// State of the preview pane for the currently selected row.
enum PreviewState {
    /// Nothing selected yet.
    Empty,
    /// A decode is in flight on the background executor.
    Loading,
    /// A decoded, ready-to-render bitmap (already in gpui's BGRA byte order).
    Ready(Arc<RenderImage>),
    /// A status / error message (not found, unsupported format, decode error).
    Message(String),
}

/// A decode request handed to the background executor.
struct DecodeRequest {
    /// Pre-resolved absolute path, or `None` if the file wasn't found on disk.
    path: Option<PathBuf>,
    /// Original `accurate_file_type` (for messages + the unsupported check).
    file_type: String,
    media_path: String,
}

/// Whole-app state — a single GPUI entity that also implements [`Render`].
struct PhotosApp {
    /// The full list, loaded once at startup.
    all: Vec<MediaItem>,
    /// Indices into `all` that match the current filter (recomputed on edit).
    filtered: Vec<usize>,
    /// Current filter-box text.
    filter: String,
    /// Index into `all` of the selected row, if any.
    selected: Option<usize>,
    /// What the preview pane is currently showing.
    preview: PreviewState,
    /// Incremented on every selection; lets us ignore stale decode responses.
    decode_seq: u64,
    /// Base directories to try when resolving a `media_path` to a real file.
    media_roots: Vec<PathBuf>,
    /// Focus target for the filter box (so it receives keystrokes).
    focus_handle: FocusHandle,
    /// Set once we've grabbed focus for the filter box on first render.
    focused_once: bool,
}

impl PhotosApp {
    fn new(cx: &mut Context<Self>, all: Vec<MediaItem>, media_roots: Vec<PathBuf>) -> Self {
        let filtered = (0..all.len()).collect();
        Self {
            all,
            filtered,
            filter: String::new(),
            selected: None,
            preview: PreviewState::Empty,
            decode_seq: 0,
            media_roots,
            focus_handle: cx.focus_handle(),
            focused_once: false,
        }
    }

    /// Recompute `filtered` from `filter` — cheap even over 15k rows.
    fn recompute(&mut self) {
        let needle = self.filter.to_lowercase();
        self.filtered = self
            .all
            .iter()
            .enumerate()
            .filter(|(_, m)| needle.is_empty() || m.path_lower.contains(&needle))
            .map(|(i, _)| i)
            .collect();
    }

    /// Handle a keystroke while the filter box is focused. Plain characters are
    /// appended, backspace deletes, escape clears — everything else is ignored.
    fn on_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        // Leave keyboard shortcuts (cmd/ctrl/fn combos) alone.
        if ks.modifiers.platform || ks.modifiers.control || ks.modifiers.function {
            return;
        }
        let mut changed = false;
        match ks.key.as_str() {
            "backspace" => changed = self.filter.pop().is_some(),
            "escape" => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    changed = true;
                }
            }
            _ => {
                // `key_char` is the actual text the keystroke would insert;
                // skip control characters (arrows, enter, etc.).
                if let Some(ch) = ks.key_char.as_ref() {
                    if !ch.is_empty() && !ch.chars().any(char::is_control) {
                        self.filter.push_str(ch);
                        changed = true;
                    }
                }
            }
        }
        if changed {
            self.recompute();
            cx.notify();
        }
    }

    /// Select a row: resolve its file (cheap), mark the preview loading, and
    /// hand the decode off to the background executor. Returns immediately.
    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        self.selected = Some(index);
        self.decode_seq += 1;
        let seq = self.decode_seq;

        let (media_path, file_type) = match self.all.get(index) {
            Some(item) => (item.media_path.clone(), item.accurate_file_type.clone()),
            None => {
                self.preview = PreviewState::Message("Item not found.".to_string());
                cx.notify();
                return;
            }
        };
        // Resolving is a handful of cheap stat() calls — fine on the UI thread.
        let path = resolve_path(&media_path, &self.media_roots);
        self.preview = PreviewState::Loading;
        cx.notify();

        let bg = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let req = DecodeRequest {
                path,
                file_type,
                media_path,
            };
            // Decode off the UI thread so clicking a row never blocks.
            let result = bg.spawn(async move { run_decode(&req) }).await;
            let _ = this.update(cx, |this, cx| {
                // Ignore results for a row we've since clicked away from.
                if this.decode_seq == seq {
                    this.preview = match result {
                        Ok(bgra) => PreviewState::Ready(to_render_image(bgra)),
                        Err(msg) => PreviewState::Message(msg),
                    };
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// One clickable list row. `ix` indexes `filtered`; the real index into
    /// `all` comes from there (or `None` for an out-of-range request).
    fn row(&self, ix: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let real = self.filtered.get(ix).copied();
        let (path, meta, selected) = match real.and_then(|r| self.all.get(r).map(|m| (r, m))) {
            Some((r, m)) => (
                m.media_path.clone(),
                format!("{}  ·  {}", m.accurate_file_type, human_size(m.file_size)),
                self.selected == Some(r),
            ),
            None => (String::new(), String::new(), false),
        };
        let bg = if selected { rgb(0x094771) } else { rgb(0x1e1e1e) };
        div()
            .id(ix)
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_2()
            .px_3()
            .py_1()
            .cursor_pointer()
            .bg(bg)
            .hover(|s| s.bg(rgb(0x2a2d2e)))
            .on_click(cx.listener(move |this, _event, window, cx| {
                if let Some(r) = real {
                    this.select(r, cx);
                    // Clicking a row shouldn't steal focus from the filter box.
                    this.focus_handle.focus(window, cx);
                }
            }))
            .child(div().flex_1().min_w(px(0.)).truncate().child(path))
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(rgb(0x858585))
                    .child(meta),
            )
    }

    /// The preview pane: the selected photo, a "loading" note, or a message.
    fn preview_view(&self) -> impl IntoElement + use<> {
        match &self.preview {
            PreviewState::Ready(data) => {
                let (line1, line2) = match self.selected.and_then(|i| self.all.get(i)) {
                    Some(m) => (
                        m.media_path.clone(),
                        format!(
                            "{}  ·  {}  ·  {}",
                            m.accurate_file_type,
                            human_size(m.file_size),
                            m.guessed_datetime.as_deref().unwrap_or("—")
                        ),
                    ),
                    None => (String::new(), String::new()),
                };
                div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .p_3()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().truncate().text_color(rgb(0xd4d4d4)).child(line1))
                            .child(div().text_xs().text_color(rgb(0x858585)).child(line2)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(0.))
                            .child(img(data.clone()).size_full()),
                    )
            }
            PreviewState::Loading => centered("Decoding…".to_string()),
            PreviewState::Message(msg) => centered(msg.clone()),
            PreviewState::Empty => {
                centered("Select a photo from the list to preview it.".to_string())
            }
        }
    }
}

impl Render for PhotosApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Grab focus for the filter box once so typing works from startup.
        if !self.focused_once {
            self.focus_handle.focus(window, cx);
            self.focused_once = true;
        }

        let shown = self.filtered.len();
        let total = self.all.len();

        let filter_display = if self.filter.is_empty() {
            div().text_color(rgb(0x6a6a6a)).child("Filter by path…")
        } else {
            div().child(self.filter.clone())
        };

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(rgb(0x2a2a2a))
            .child(
                div()
                    .min_w(px(96.))
                    .text_color(rgb(0x858585))
                    .child(format!("{shown} / {total}")),
            )
            .child(
                div()
                    .flex_1()
                    .track_focus(&self.focus_handle)
                    .on_key_down(cx.listener(Self::on_key))
                    .px_2()
                    .py_1()
                    .border_1()
                    .border_color(rgb(0x3c3c3c))
                    .rounded_md()
                    .child(filter_display),
            );

        let list = uniform_list(
            "rows",
            shown,
            cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                let mut rows = Vec::with_capacity(range.len());
                for ix in range {
                    rows.push(this.row(ix, cx));
                }
                rows
            }),
        )
        .size_full();

        let list_col = div()
            .w(px(480.))
            .h_full()
            .border_r_1()
            .border_color(rgb(0x2a2a2a))
            .child(list);

        let preview_col = div().flex_1().h_full().child(self.preview_view());

        let body = div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h(px(0.))
            .child(list_col)
            .child(preview_col);

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x1e1e1e))
            .text_color(rgb(0xd4d4d4))
            .text_sm()
            .child(header)
            .child(body)
    }
}

// ---------------------------------------------------------------------------
// Data loading
// ---------------------------------------------------------------------------

/// Open the database read-only and pull every `media_item` row.
fn load_media_items(db_path: &str) -> Result<Vec<MediaItem>, rusqlite::Error> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare(
        "SELECT media_path, accurate_file_type, file_size, guessed_datetime \
         FROM media_item \
         ORDER BY media_path",
    )?;
    let items = stmt
        .query_map([], |row| {
            let media_path: String = row.get(0)?;
            Ok(MediaItem {
                path_lower: media_path.to_lowercase(),
                media_path,
                accurate_file_type: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                file_size: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                guessed_datetime: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(items)
}

// ---------------------------------------------------------------------------
// Preview: resolve a media_path to a file on disk and decode it
// ---------------------------------------------------------------------------

/// Try each base directory joined with `media_path`; return the first that
/// names an existing file. Falls back to treating `media_path` as-is.
fn resolve_path(media_path: &str, roots: &[PathBuf]) -> Option<PathBuf> {
    for root in roots {
        let candidate = root.join(media_path);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let direct = PathBuf::from(media_path);
    direct.is_file().then_some(direct)
}

/// Decode an image file into an RGBA buffer, applying EXIF orientation and
/// downscaling very large photos. The result is swapped into BGRA byte order,
/// which is what gpui's [`RenderImage`] expects.
fn decode_image(path: &Path) -> Result<RgbaImage, String> {
    let mut decoder = ImageReader::open(path)
        .map_err(|e| e.to_string())?
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .into_decoder()
        .map_err(|e| e.to_string())?;
    // Read EXIF orientation before consuming the decoder, then apply it so
    // phone photos aren't rotated.
    let orientation = decoder.orientation().map_err(|e| e.to_string())?;
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);
    let mut rgba = downscale(img.into_rgba8(), 1600);
    for pixel in rgba.pixels_mut() {
        pixel.0.swap(0, 2); // RGBA -> BGRA
    }
    Ok(rgba)
}

/// Decode a HEIC/HEIF file with the pure-Rust `heic` crate. It outputs BGRA
/// directly (gpui's byte order) and applies the container's rotation/mirror
/// transforms, so — unlike [`decode_image`] — no channel swap or EXIF step is
/// needed here. The result is downscaled to keep the preview cheap to upload.
fn decode_heic(path: &Path) -> Result<RgbaImage, String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let output = DecoderConfig::new()
        .decode(&data, PixelLayout::Bgra8)
        .map_err(|e| e.to_string())?;
    let buf = RgbaImage::from_raw(output.width, output.height, output.data)
        .ok_or_else(|| "heic: decoded buffer size didn't match dimensions".to_string())?;
    Ok(downscale(buf, 1600))
}

/// Wrap a decoded BGRA buffer in a single-frame [`RenderImage`] for `img()`.
fn to_render_image(bgra: RgbaImage) -> Arc<RenderImage> {
    Arc::new(RenderImage::new(vec![Frame::new(bgra)]))
}

/// Shrink an image so its longest side is at most `max` pixels.
fn downscale(img: RgbaImage, max: u32) -> RgbaImage {
    let (w, h) = img.dimensions();
    if w <= max && h <= max {
        return img;
    }
    let scale = max as f32 / w.max(h) as f32;
    let nw = ((w as f32 * scale).round() as u32).max(1);
    let nh = ((h as f32 * scale).round() as u32).max(1);
    image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle)
}

/// The blocking half of a decode. Returns the decoded (BGRA) bitmap, or a
/// message explaining why it couldn't be previewed.
fn run_decode(req: &DecodeRequest) -> Result<RgbaImage, String> {
    let Some(path) = &req.path else {
        return Err(format!(
            "File not found on disk for:\n{}\n\n(searched under input/Takeout*)",
            req.media_path
        ));
    };
    match req.file_type.to_ascii_lowercase().as_str() {
        "heic" | "heif" => decode_heic(path),
        // Video containers — still no in-app preview.
        "mp4" | "mov" | "avi" => Err(format!(
            "No in-app preview for {} files.\n\n{}",
            req.file_type,
            path.display()
        )),
        _ => decode_image(path),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A centered, muted status message filling the preview pane.
fn centered(text: String) -> gpui::Div {
    div()
        .flex()
        .size_full()
        .items_center()
        .justify_center()
        .p_4()
        .text_color(rgb(0x858585))
        .child(text)
}

/// Human-readable byte size, e.g. `1.4 MB`.
fn human_size(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes <= 0 {
        return "0 B".to_string();
    }
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `--check` runs a headless smoke test of the preview pipeline and exits.
    let check_mode = args.iter().any(|a| a == "--check");
    let db_path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| concat!(env!("CARGO_MANIFEST_DIR"), "/../../db.sqlite").to_string());

    let all = load_media_items(&db_path)?;
    eprintln!("loaded {} media_item rows from {db_path}", all.len());

    // Where the actual photo files live. `media_path` is stored relative to a
    // Google Takeout root, so try the known extraction dirs (override the first
    // candidate with the PTSYNC_MEDIA_ROOT env var).
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut media_roots = Vec::new();
    if let Ok(custom) = std::env::var("PTSYNC_MEDIA_ROOT") {
        media_roots.push(PathBuf::from(custom));
    }
    media_roots.extend([
        repo.join("input/Takeout"),
        repo.join("input/Takeout2"),
        repo.join("input/Takeout-small"),
        repo.join("input"),
        repo.clone(),
    ]);

    // Headless smoke test: resolve + decode one sample of each previewable file
    // type (so every decode path — including HEIC — is exercised) and exit
    // without opening a window. Run with `cargo run -- --check`.
    if check_mode {
        let mut decoded = 0;
        let mut seen: Vec<String> = Vec::new();
        for item in &all {
            let ext = item.accurate_file_type.to_ascii_lowercase();
            if !matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "gif" | "heic" | "heif") {
                continue;
            }
            // One sample per distinct type keeps the smoke test quick while
            // still covering both the `image` and `heic` decode paths.
            if seen.contains(&ext) {
                continue;
            }
            seen.push(ext.clone());
            let req = DecodeRequest {
                path: resolve_path(&item.media_path, &media_roots),
                file_type: item.accurate_file_type.clone(),
                media_path: item.media_path.clone(),
            };
            match run_decode(&req) {
                Ok(img) => {
                    eprintln!(
                        "OK  {:>4}  {:>5}x{:<5}  {}",
                        ext,
                        img.width(),
                        img.height(),
                        item.media_path
                    );
                    decoded += 1;
                }
                Err(e) => eprintln!(
                    "ERR {:>4}  {}  ({})",
                    ext,
                    e.lines().next().unwrap_or_default(),
                    item.media_path
                ),
            }
        }
        eprintln!(
            "--check: successfully decoded {decoded} image(s) across {} type(s)",
            seen.len()
        );
        return Ok(());
    }

    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(SharedString::from("ptsync · Photos")),
                ..Default::default()
            }),
            ..Default::default()
        };
        cx.open_window(options, |_window, cx| {
            cx.new(|cx| PhotosApp::new(cx, all, media_roots))
        })
        .unwrap();
        cx.activate(true);
    });
    Ok(())
}
