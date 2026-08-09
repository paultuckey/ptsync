//! One input tree covering every file format and dating edge case the tool
//! handles, synced by running the real binary, with the whole output tree
//! snapshotted. Most of what used to be asserted a slice at a time — dates,
//! deduplication, sidecars, album membership, extension correction, unreadable
//! files — is visible here as a path in `tests/snapshots/corpus.txt`.
//!
//! Kept separate from `test/takeout_basic`, which stays small because
//! `docs/demo.tape` records it into the README's GIF.
//!
//! The tree is built at run time rather than committed. Nothing here is
//! platform-specific, but building it keeps the shape of the corpus readable in
//! one place instead of spread over a directory listing. Hostile filenames are
//! deliberately *not* included: `CON.jpg` and a 300-character name cannot be
//! created on every platform, so the tree would differ per OS and the snapshot
//! could not be shared. `src/boundary_tests/paths.rs` covers those with
//! invariants instead of a snapshot.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

const ARCHIVE_DIR: &str = "photo-archive";
const SNAPSHOT_PATH: &str = "tests/snapshots/corpus.txt";

/// Fixed so the `yyyy/mm/dd` buckets do not depend on the machine. Far enough
/// east that an instant crosses a day boundary, so a regression to UTC shows up
/// as a different directory rather than only a different offset.
const TZ: &str = "+12:00";

/// 2024-05-22T00:17:51Z, which at `+12:00` is lunchtime on the 22nd.
const TAKEN: i64 = 1716337071;
/// 2023-11-14T22:13:20Z. Two distinct photos share it, so they compete for one
/// name.
const SHARED: i64 = 1700000000;

/// An hour later than [`TAKEN`], per step. Files that are not deliberately
/// competing for a name get one each, so the snapshot reads as a list of
/// distinct decisions rather than a wall of checksum suffixes.
fn hour(n: i64) -> String {
    (TAKEN + n * 3600).to_string()
}

#[test]
fn corpus_sync_output_matches_snapshot() -> Result<()> {
    let root = repo_root()?;
    let work = prepare(&root, "target/corpus")?;
    sync(&work, &work.join("input"))?;

    let generated = render_tree(&work.join(ARCHIVE_DIR))?;
    let snapshot = root.join(SNAPSHOT_PATH);

    if std::env::var_os("UPDATE_DOCS").is_some() {
        if let Some(dir) = snapshot.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&snapshot, &generated)
            .with_context(|| format!("writing {}", snapshot.display()))?;
        println!("wrote {}", snapshot.display());
        return Ok(());
    }

    let existing = std::fs::read_to_string(&snapshot).unwrap_or_default();
    assert_eq!(
        generated, existing,
        "{SNAPSHOT_PATH} is out of date. If the change is intended, regenerate with:\
         \n\n    UPDATE_DOCS=1 cargo test\n"
    );
    Ok(())
}

/// "Processing the same source twice results in no changes": asserted on
/// modified times, so a file rewritten with identical bytes still fails.
#[test]
fn corpus_rerun_rewrites_nothing() -> Result<()> {
    let root = repo_root()?;
    let work = prepare(&root, "target/corpus_rerun")?;
    let input = work.join("input");
    let archive = work.join(ARCHIVE_DIR);

    sync(&work, &input)?;
    let first = mtimes_under(&archive)?;
    assert!(!first.is_empty(), "the first run should have written files");

    sync(&work, &input)?;
    assert_eq!(
        first,
        mtimes_under(&archive)?,
        "re-running over unchanged input must not rewrite any output file"
    );
    Ok(())
}

/// "A user should never lose a media file": the input is read-only as far as
/// the tool is concerned.
#[test]
fn corpus_sync_never_modifies_input() -> Result<()> {
    let root = repo_root()?;
    let work = prepare(&root, "target/corpus_input_untouched")?;
    let input = work.join("input");

    let before = render_tree(&input)?;
    sync(&work, &input)?;
    assert_eq!(
        before,
        render_tree(&input)?,
        "sync must not add, remove, or modify any file in the input tree"
    );
    Ok(())
}

/// A downloaded Takeout arrives zipped, and must sync to byte-identical output.
#[test]
fn corpus_zip_and_directory_produce_identical_output() -> Result<()> {
    let root = repo_root()?;
    let work = prepare(&root, "target/corpus_zip")?;
    let input = work.join("input");

    let zipped = work.join("takeout.zip");
    zip_dir(&input, &zipped)?;

    sync(&work, &input)?;
    let from_dir = render_tree(&work.join(ARCHIVE_DIR))?;
    std::fs::remove_dir_all(work.join(ARCHIVE_DIR))?;

    sync(&work, &zipped)?;
    assert_eq!(from_dir, render_tree(&work.join(ARCHIVE_DIR))?);
    Ok(())
}

/// KNOWN BUG, currently failing — remove the `ignore` when it is fixed.
///
/// The name a media file resolves to is checked against other *media*, and the
/// sidecar path is then derived by swapping the extension. Two files taken at
/// one instant but in different formats do not collide as media — `1217-51000.jpg`
/// and `1217-51000.mp4` are different names — yet both want `1217-51000.md`. The
/// second overwrites the first, so one of the two photos loses its recorded
/// original path, people and location, and the survivor is rewritten on every
/// run rather than the run being a no-op.
///
/// This is the "never lose a metadata file" goal, so a fix probably belongs in
/// the deduplicator: resolve the sidecar name alongside the media name rather
/// than deriving it afterwards.
#[test]
#[ignore = "known bug: same-instant files of different formats share one sidecar"]
fn same_instant_different_formats_each_keep_a_sidecar() -> Result<()> {
    let root = repo_root()?;
    let work = root.join("target/corpus_sidecar_clash");
    if work.exists() {
        std::fs::remove_dir_all(&work)?;
    }
    let input = work.join("input");
    std::fs::create_dir_all(&input)?;

    // One photo and one video, told they were taken at the same instant.
    copy_fixture("Canon_40D.jpg", &input.join("photo.jpg"))?;
    write_sidecar(&input.join("photo.jpg"), &hour(0), None, &[])?;
    copy_fixture("Hello.mp4", &input.join("video.mp4"))?;
    write_sidecar(&input.join("video.mp4"), &hour(0), None, &[])?;

    sync(&work, &input)?;

    let archive = work.join(ARCHIVE_DIR);
    let written = files_under(&archive)?;
    let media: Vec<&PathBuf> = written
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e != "md"))
        .collect();
    assert_eq!(media.len(), 2, "both files are written");

    for path in media {
        let sidecar = path.with_extension("md");
        assert!(
            sidecar.is_file(),
            "{} has no sidecar of its own",
            path.display()
        );
        let text = std::fs::read_to_string(&sidecar)?;
        let name = path
            .file_name()
            .context("media path has no file name")?
            .to_string_lossy()
            .to_string();
        assert!(
            text.contains(&format!("![]({name})")),
            "{} embeds another file:\n{text}",
            sidecar.display()
        );
    }
    Ok(())
}

// ---- the corpus ---------------------------------------------------------

/// Build the input tree under `dir/input`.
///
/// Every media file needs a date that does not come from the filesystem, or its
/// output path would depend on when the repo was checked out. Only
/// `video_track_dated.mp4` carries a usable one of its own (a track creation
/// time); `.mov` and `.m4v` have theirs zeroed, and AVI/MPG/WMV hold no metadata
/// at all, so each is given the supplemental sidecar Takeout would have written.
///
/// The Google half sits under `Takeout/`, which is what a downloaded zip
/// actually contains — see the note on the year folder below.
fn build_corpus(dir: &Path) -> Result<()> {
    let gp = dir.join("Takeout/Google Photos");

    // A dated album. `Holiday/Canon_40D.jpg` is byte-identical to the copy under
    // `Photos from 2024`, so the two collapse to one output file listing both
    // source paths.
    let holiday = gp.join("Holiday");
    std::fs::create_dir_all(&holiday)?;
    copy_fixture("Canon_40D.jpg", &holiday.join("Canon_40D.jpg"))?;
    write_sidecar(
        &holiday.join("Canon_40D.jpg"),
        &hour(0),
        Some(GEO),
        &["Tim Tam", "  ", "Nandor"],
    )?;
    write(
        &holiday.join("metadata.json"),
        br#"{"title": "Holiday Snaps"}"#,
    )?;

    let photos = gp.join("Photos from 2024");
    std::fs::create_dir_all(&photos)?;

    // The same bytes as the album copy above.
    copy_fixture("Canon_40D.jpg", &photos.join("Canon_40D.jpg"))?;
    write_sidecar(
        &photos.join("Canon_40D.jpg"),
        &hour(0),
        Some(GEO),
        &["Tim Tam", "  ", "Nandor"],
    )?;

    // No sidecar, so this is dated from its own EXIF instead: 2008, not 2024.
    // Marked so it is a distinct file from the two above.
    copy_marked("Canon_40D.jpg", &photos.join("exif_only.jpg"), b"E")?;

    // The only date is a GPS date/time pair, read as one reading and converted:
    // 19:30 on the 17th in Greenwich is 07:30 on the 18th at +12:00.
    copy_fixture("gps_date_only.jpg", &photos.join("gps_only.jpg"))?;

    // Two distinct photos claiming one instant: the second cannot have the bare
    // date name, so it gains a checksum suffix and its sidecar follows it.
    for (name, marker) in [("clash_a.jpg", &b"A"[..]), ("clash_b.jpg", &b"BB"[..])] {
        copy_marked("Canon_40D.jpg", &photos.join(name), marker)?;
        write_sidecar(&photos.join(name), &SHARED.to_string(), None, &[])?;
    }

    // Google writes 0,0 to mean "no location"; it must not reach the sidecar.
    copy_marked("Canon_40D.jpg", &photos.join("null_island.jpg"), b"N")?;
    write_sidecar(
        &photos.join("null_island.jpg"),
        &hour(1),
        Some(r#"{"latitude": 0.0, "longitude": 0.0}"#),
        &[],
    )?;

    // Dates itself from its track metadata. Its stem is unique in the directory
    // because a file with no sidecar of its own will inherit a same-stem one —
    // that is how a live photo's clip picks up the still's metadata — and an
    // inherited sidecar would outrank the track date being tested here.
    copy_fixture("Hello.mp4", &photos.join("video_track_dated.mp4"))?;

    // Every other supported container, each dated by its own sidecar.
    for (n, (fixture, name)) in [
        ("Hello.mov", "clip.mov"),
        ("Hello.m4v", "clip.m4v"),
        ("Hello.avi", "clip.avi"),
        ("Hello.mpg", "clip.mpg"),
        ("Hello.wmv", "clip.wmv"),
        ("Hello.png", "still.png"),
        ("Hello.gif", "still.gif"),
    ]
    .into_iter()
    .enumerate()
    {
        copy_fixture(fixture, &photos.join(name))?;
        write_sidecar(&photos.join(name), &hour(2 + n as i64), None, &[])?;
    }

    // Content decides the type, not the name: PNG bytes under a .jpg name are
    // written into the archive as a .png.
    copy_marked("Hello.png", &photos.join("mislabelled.jpg"), b"M")?;
    write_sidecar(&photos.join("mislabelled.jpg"), &hour(9), None, &[])?;

    // An ASF container carrying audio: it sniffs as unsupported and must not be
    // filed as a video with no picture, even under a video extension.
    copy_fixture("Hello.wma", &photos.join("music.asf"))?;
    write_sidecar(&photos.join("music.asf"), &hour(10), None, &[])?;

    // Classifies as media on its extension but is not an image, so it is counted
    // as unprocessable and skipped rather than copied.
    write(&photos.join("broken.jpg"), b"this is not an image")?;
    write_sidecar(&photos.join("broken.jpg"), &hour(11), None, &[])?;

    // Not media at all.
    write(&photos.join("README.txt"), b"not a photo\n")?;

    // Takeout truncates a sidecar name to fit a length cap, so it no longer
    // spells out `supplemental-metadata`.
    let long = "9C19C4BF-E0C8-4D74-8DC4-4BB2338FB029.JPG";
    copy_marked("Canon_40D.jpg", &photos.join(long), b"L")?;
    write_json(
        &photos.join(format!("{long}.suppl.json")),
        &hour(12),
        None,
        &[],
    )?;

    // An edited rendition has no sidecar of its own and inherits the original's,
    // so both are dated even though only one was described — and, sharing that
    // instant and extension, the second gains a checksum suffix.
    copy_marked("Canon_40D.jpg", &photos.join("IMG_1189.JPG"), b"O")?;
    write_sidecar(&photos.join("IMG_1189.JPG"), &hour(13), None, &[])?;
    copy_marked("Canon_40D.jpg", &photos.join("IMG_1189-edited.JPG"), b"D")?;

    // A `Photos from <year>` folder is Takeout's own bucketing, not an album.
    //
    // KNOWN BUG, recorded by the snapshot: the year-folder pattern is anchored at
    // `google photos/`, so it only matches when the container root is the
    // `Takeout` directory. A downloaded zip has `Takeout/` inside it, as here, so
    // the folder is not recognised and `albums/Photos from 2012.md` is written.
    // When that is fixed the snapshot loses that line.
    let year = gp.join("Photos from 2012");
    std::fs::create_dir_all(&year)?;
    write(
        &year.join("metadata.json"),
        br#"{"title": "Photos from 2012"}"#,
    )?;
    copy_marked("Canon_40D.jpg", &year.join("IMG_1234.jpg"), b"Y")?;
    write_sidecar(&year.join("IMG_1234.jpg"), &hour(14), None, &[])?;

    // An album whose title is blank falls back to its directory name.
    let untitled = gp.join("empty-title-album");
    std::fs::create_dir_all(&untitled)?;
    write(&untitled.join("metadata.json"), br#"{"title": ""}"#)?;
    copy_marked("Canon_40D.jpg", &untitled.join("IMG_9999.jpg"), b"U")?;
    write_sidecar(&untitled.join("IMG_9999.jpg"), &hour(15), None, &[])?;

    // iCloud names its members in a csv rather than a per-directory json, and
    // lists bare filenames that have to be resolved against `Photos/`. The last
    // member is not in this export, so it must be left out rather than rendered
    // as a broken link.
    let icloud = dir.join("iCloud Photos");
    std::fs::create_dir_all(icloud.join("Photos"))?;
    for (n, (name, marker)) in [("IMG_5071.jpg", &b"I"[..]), ("IMG_5072.jpg", &b"J"[..])]
        .into_iter()
        .enumerate()
    {
        copy_marked("Canon_40D.jpg", &icloud.join("Photos").join(name), marker)?;
        write_sidecar(
            &icloud.join("Photos").join(name),
            &hour(16 + n as i64),
            None,
            &[],
        )?;
    }
    write(
        &icloud.join("Shared Trip.csv"),
        b"Images\nIMG_5071.jpg\nIMG_5072.jpg\nnot-in-this-export.jpg\n",
    )?;

    Ok(())
}

const GEO: &str = r#"{"latitude": -21.6303194, "longitude": 152.2605444}"#;

fn write_sidecar(media: &Path, taken: &str, geo: Option<&str>, people: &[&str]) -> Result<()> {
    let name = media
        .file_name()
        .context("sidecar target has no file name")?
        .to_string_lossy()
        .to_string();
    let json = media.with_file_name(format!("{name}.supplemental-metadata.json"));
    write_json(&json, taken, geo, people)
}

fn write_json(path: &Path, taken: &str, geo: Option<&str>, people: &[&str]) -> Result<()> {
    let mut body = format!(r#"{{"photoTakenTime": {{"timestamp": "{taken}"}}"#);
    if let Some(geo) = geo {
        body.push_str(&format!(r#", "geoData": {geo}"#));
    }
    if !people.is_empty() {
        let names: Vec<String> = people
            .iter()
            .map(|n| format!(r#"{{"name": "{n}"}}"#))
            .collect();
        body.push_str(&format!(r#", "people": [{}]"#, names.join(", ")));
    }
    body.push('}');
    write(path, body.as_bytes())
}

fn write(path: &Path, content: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))
}

fn copy_fixture(name: &str, to: &Path) -> Result<()> {
    let from = repo_root()?.join("test").join(name);
    let bytes =
        std::fs::read(&from).with_context(|| format!("reading fixture {}", from.display()))?;
    write(to, &bytes)
}

/// A fixture with bytes appended, so it is a distinct file that still parses —
/// both JPEG and PNG readers stop at their own end markers.
fn copy_marked(name: &str, to: &Path, marker: &[u8]) -> Result<()> {
    let from = repo_root()?.join("test").join(name);
    let mut bytes =
        std::fs::read(&from).with_context(|| format!("reading fixture {}", from.display()))?;
    bytes.extend_from_slice(marker);
    write(to, &bytes)
}

// ---- harness ------------------------------------------------------------

fn prepare(root: &Path, dir: &str) -> Result<PathBuf> {
    let work = root.join(dir);
    if work.exists() {
        std::fs::remove_dir_all(&work)?;
    }
    std::fs::create_dir_all(&work)?;
    build_corpus(&work.join("input"))?;
    Ok(work)
}

fn sync(cwd: &Path, input: &Path) -> Result<()> {
    let output = Command::new(PathBuf::from(env!("CARGO_BIN_EXE_ptsync")))
        .args(["sync", "--timezone", TZ, "--input"])
        .arg(input)
        .args(["--output", ARCHIVE_DIR])
        .current_dir(cwd)
        .output()
        .context("running ptsync")?;
    if !output.status.success() {
        bail!(
            "ptsync sync failed with {}:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// Every file under `dir`, sorted. The path carries the dating, deduplication
/// and extension decisions. Markdown is quoted in full — it is the part a
/// reviewer needs to read, and a digest would only say that something moved —
/// while media is reduced to a digest.
fn render_tree(dir: &Path) -> Result<String> {
    let mut lines = vec![format!(
        "# Generated by tests/corpus.rs. Do not edit by hand.\n\
         # `ptsync sync` over the corpus built in that file, at TZ {TZ}.\n\
         # Media files show a digest; markdown is quoted in full.\n"
    )];
    let mut tree = BTreeMap::new();
    for path in files_under(dir)? {
        let rel = path
            .strip_prefix(dir)?
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let bytes = std::fs::read(&path)?;
        let body = if rel.ends_with(".md") {
            let text = String::from_utf8_lossy(&bytes);
            let quoted: Vec<String> = text.lines().map(|l| format!("    {l}")).collect();
            format!("\n{}", quoted.join("\n"))
        } else {
            format!("  {:x}", md5::compute(&bytes))
        };
        tree.insert(rel, body);
    }
    for (rel, body) in tree {
        lines.push(format!("{rel}{body}"));
    }
    lines.push(String::new());
    Ok(lines.join("\n"))
}

fn mtimes_under(dir: &Path) -> Result<BTreeMap<String, std::time::SystemTime>> {
    let mut tree = BTreeMap::new();
    for path in files_under(dir)? {
        let rel = path.strip_prefix(dir)?.to_string_lossy().to_string();
        tree.insert(rel, std::fs::metadata(&path)?.modified()?);
    }
    Ok(tree)
}

fn files_under(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            out.extend(files_under(&path)?);
        } else {
            out.push(path);
        }
    }
    Ok(out)
}

fn zip_dir(src: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::create(dest)?;
    let mut writer = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    let mut entries = files_under(src)?;
    // Sorted so the archive is stable between runs.
    entries.sort();
    for entry in entries {
        let name = entry
            .strip_prefix(src)?
            .to_string_lossy()
            .replace('\\', "/");
        writer.start_file(name, options)?;
        let mut input = std::fs::File::open(&entry)?;
        std::io::copy(&mut input, &mut writer)?;
    }
    writer.finish()?;
    Ok(())
}

/// Cargo runs integration tests with `CARGO_MANIFEST_DIR` set to the package
/// root, which is where `test/` lives.
fn repo_root() -> Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}
