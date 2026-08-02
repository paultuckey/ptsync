use crate::album::{Album, build_album_md, parse_album, split_album_notes};
use crate::dedup::{DeDuplicationResult, Deduplicator};
use crate::file_type::{QuickFileType, file_ext_from_file_type};
use crate::fs::{FileSystem, WritableFileSystem, open_input, open_output};
use crate::inspect::inspect_media_files;
use crate::live_photo::detect_live_photo_pairs;
use crate::markdown::sync_markdown;
use crate::metadata::MediaFileInfo;
use crate::output_path::{MediaFileDerivedInfo, media_file_derived_from_media_info};
use crate::path::strip_ext;
use crate::progress::Progress;
use crate::util::{ScanInfo, scan_fs};
use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;
use tracing::{debug, info, warn};

pub(crate) fn main(
    dry_run: bool,
    input: &str,
    output_directory: &Option<String>,
    skip_markdown: bool,
    skip_media: bool,
    skip_albums: bool,
    skip_xmp: bool,
) -> anyhow::Result<()> {
    let container = open_input(input)?;

    let files = scan_fs(container.as_ref());
    info!("Found {} files in input", files.len());

    let output_container_o: Option<Arc<dyn WritableFileSystem>> = match output_directory {
        Some(output) => Some(open_output(output)?),
        None => None,
    };
    let mut deduper = Deduplicator::new();
    let mut final_path_by_checksum = HashMap::<String, String>::new();

    // Albums are parsed up front so each photo's sidecar can record the albums it
    // belongs to. The album markdown files themselves are written later, once the
    // final media output paths are known.
    let albums = if skip_albums {
        Vec::new()
    } else {
        parse_albums(container.as_ref(), &files)
    };
    let album_names_by_path = build_album_membership(&albums);

    // A live photo's clip is a sidecar of its still: both are written, but only
    // the still gets a note, which names the clip. See `crate::live_photo`.
    let live_photo_clips = detect_live_photo_pairs(&files);
    let clips_by_still: HashMap<&str, &str> = live_photo_clips
        .iter()
        .map(|(clip, still)| (still.as_str(), clip.as_str()))
        .collect();

    if !skip_media {
        let media_si_files: Vec<ScanInfo> = files
            .iter()
            .filter(|m| m.quick_file_type == QuickFileType::Media)
            .cloned()
            .collect();
        info!("Inspecting {} photo and video files", media_si_files.len());
        let prog = Arc::new(Progress::new(media_si_files.len() as u64));
        // Inspection (hashing + metadata) runs in parallel; dedup must stay on
        // this thread since it mutates the shared collection. Files with the
        // same content hash collapse into one entry, recording each original
        // path (see `Deduplicator`).
        let mut inspected =
            inspect_media_files(container.clone(), media_si_files, prog.clone(), skip_xmp);
        for media in inspected.by_ref() {
            deduper.add(media);
        }
        let skipped = inspected.skipped_count();
        if skipped > 0 {
            warn!("{skipped} files could not be processed");
        }
        drop(prog);

        if let Some(output_container) = &output_container_o {
            let output_container: &dyn WritableFileSystem = output_container.as_ref();
            let media_to_write = deduper.sorted_media();
            info!("Outputting {} photo and video files", media_to_write.len());
            let prog = Progress::new(media_to_write.len() as u64);
            let mut checksum_by_original_path = HashMap::<&str, &str>::new();

            // Two passes, stills before clips. A live photo's clip is named from
            // its still (see `derived_for_clip`), so the still's resolved path
            // has to exist by the time the clip is written - and `sorted_media`
            // is in checksum order, which says nothing about which half of a
            // pair comes first. The note loop below splits out for the same
            // reason.
            for media in &media_to_write {
                if live_photo_still_for(&live_photo_clips, media).is_some() {
                    continue;
                }
                prog.inc();
                let derived = media_file_derived_from_media_info(media);
                write_and_record(
                    media,
                    &derived,
                    dry_run,
                    container.as_ref(),
                    output_container,
                    &mut final_path_by_checksum,
                    &mut checksum_by_original_path,
                );
            }
            for media in &media_to_write {
                let Some(still) = live_photo_still_for(&live_photo_clips, media) else {
                    continue;
                };
                prog.inc();
                let derived = derived_for_clip(
                    media,
                    still,
                    &checksum_by_original_path,
                    &final_path_by_checksum,
                );
                write_and_record(
                    media,
                    &derived,
                    dry_run,
                    container.as_ref(),
                    output_container,
                    &mut final_path_by_checksum,
                    &mut checksum_by_original_path,
                );
            }
            drop(prog);

            // Notes are written only once every file has landed, because a live
            // photo's still names its clip and the two are written in checksum
            // order - the clip may well come after the still.
            if !skip_markdown {
                for media in &media_to_write {
                    let Some(final_path) =
                        final_path_by_checksum.get(&media.hash_info.long_checksum)
                    else {
                        continue; // it failed to write; already warned about above
                    };
                    if let Some(still) = live_photo_still_for(&live_photo_clips, media) {
                        debug!(
                            "No note for motion clip {final_path}: covered by its still {still}"
                        );
                        continue;
                    }
                    let motion_path = live_photo_clip_output_for(
                        &clips_by_still,
                        &checksum_by_original_path,
                        &final_path_by_checksum,
                        media,
                    );
                    let album_names = album_names_for(&album_names_by_path, &media.original_path);
                    let sync_md_r = sync_markdown(
                        dry_run,
                        media,
                        final_path,
                        &album_names,
                        motion_path.as_deref(),
                        output_container,
                    );
                    if let Err(e) = sync_md_r {
                        warn!("Error writing markdown file beside {final_path:?}: {e}");
                    }
                }
            }
        }
    }

    if !skip_albums && let Some(output_container) = &output_container_o {
        let output_container: &dyn WritableFileSystem = output_container.as_ref();
        info!("Outputting {} albums", albums.len());
        for album in &albums {
            let output_path = &album.desired_album_md_path;
            // Preserve any notes the user wrote below the marker before rebuilding.
            let existing_notes = read_album_notes(output_container, output_path);
            let (md, resolved_count) = build_album_md(
                album,
                Some(deduper.by_checksum()),
                "../",
                Some(&final_path_by_checksum),
                &existing_notes,
            );
            if resolved_count == 0 {
                warn!("Skipping album with no resolvable photos: {output_path:?}");
                continue;
            }
            // The photo list is regenerated every run. An unchanged album
            // yields identical content; only write when it actually differs
            // so a re-run leaves the file (and its mtime) untouched.
            if let Err(e) = output_container.write_if_changed(dry_run, output_path, md.as_bytes()) {
                warn!("Error writing album file {output_path:?}: {e}");
            }
        }
    }

    Ok(())
}

/// Write one media file and record where it landed, for the maps the note and
/// album passes read afterwards.
///
/// A file that fails to write is warned about and left out of both maps, which
/// is what makes the later passes skip it rather than name a file that is not
/// there.
#[allow(clippy::too_many_arguments)]
fn write_and_record<'a>(
    media: &'a MediaFileInfo,
    derived: &MediaFileDerivedInfo,
    dry_run: bool,
    input_container: &dyn FileSystem,
    output_container: &dyn WritableFileSystem,
    final_path_by_checksum: &mut HashMap<String, String>,
    checksum_by_original_path: &mut HashMap<&'a str, &'a str>,
) {
    match write_media(media, derived, dry_run, input_container, output_container) {
        Ok(final_path) => {
            let long_checksum = &media.hash_info.long_checksum;
            final_path_by_checksum.insert(long_checksum.clone(), final_path);
            for original in &media.original_path {
                checksum_by_original_path.insert(original, long_checksum);
            }
        }
        Err(e) => {
            warn!(
                "Error writing media file: {:?}, error: {}",
                derived.desired_media_path, e
            );
        }
    }
}

/// Where a live photo's motion clip is written: beside its still, under the
/// still's name.
///
/// The clip is a sidecar of the still, the same way the note is. Its own capture
/// time is not wrong, but it is not *its* to spend on a name: the two halves are
/// one shutter press, and left to name themselves they drift apart - the still's
/// EXIF is a local wall clock while the clip's QuickTime time is UTC, which on a
/// real iCloud export filed a third of live photos a day away from their own
/// clip, and sub-second EXIF then splits even the ones that agreed.
///
/// Only the path is inherited. The extension still comes from the clip's own
/// bytes, so a Takeout `.MP4` that is really QuickTime is written `.mov` beside
/// an `.heic` still. Inheriting the *resolved* path means a still that took a
/// collision suffix hands it on, keeping the pair together under
/// `1430-22417-a1b2c3d.{heic,mov}`.
///
/// Falls back to the clip's own name when the still is not in the archive - it
/// failed to write, and was warned about then. A clip named after a still that
/// is not there would be a sidecar of nothing.
pub(crate) fn derived_for_clip(
    clip: &MediaFileInfo,
    still: &str,
    checksum_by_original_path: &HashMap<&str, &str>,
    final_path_by_checksum: &HashMap<String, String>,
) -> MediaFileDerivedInfo {
    match checksum_by_original_path
        .get(still)
        .and_then(|checksum| final_path_by_checksum.get(*checksum))
    {
        // The clip's own capture time is never consulted on this path, which is
        // the point: it is the still's name the pair is kept together under.
        Some(still_path) => MediaFileDerivedInfo {
            desired_media_path: strip_ext(still_path).to_string(),
            desired_media_extension: file_ext_from_file_type(&clip.accurate_file_type),
        },
        None => {
            debug!(
                "Motion clip {:?} keeps its own name: its still {still} is not in the archive",
                clip.original_file_this_run
            );
            media_file_derived_from_media_info(clip)
        }
    }
}

/// The still this media file is the motion clip of, if it is one.
///
/// Dedup pools identical bytes under one entry, so a file can carry several
/// original paths; being a clip at any one of them is enough, since the note
/// would describe the same bytes either way.
fn live_photo_still_for<'a>(
    stills_by_clip: &'a HashMap<String, String>,
    media: &MediaFileInfo,
) -> Option<&'a str> {
    media
        .original_path
        .iter()
        .find_map(|path| stills_by_clip.get(path.as_str()))
        .map(String::as_str)
}

/// Where this still's motion clip was written, for a still that has one.
///
/// `None` when the file is not a paired still, or when its clip never made it
/// into the archive - a clip that failed to write must not be named by a note
/// that would then point at nothing.
fn live_photo_clip_output_for(
    clips_by_still: &HashMap<&str, &str>,
    checksum_by_original_path: &HashMap<&str, &str>,
    final_path_by_checksum: &HashMap<String, String>,
    media: &MediaFileInfo,
) -> Option<String> {
    let clip = media
        .original_path
        .iter()
        .find_map(|path| clips_by_still.get(path.as_str()))?;
    let checksum = checksum_by_original_path.get(clip)?;
    final_path_by_checksum.get(*checksum).cloned()
}

/// Parse all album files in the scan into `Album`s, logging progress.
fn parse_albums(container: &dyn FileSystem, files: &[ScanInfo]) -> Vec<Album> {
    let scan_info_albums = files
        .iter()
        .filter(|m| {
            m.quick_file_type == QuickFileType::AlbumCsv
                || m.quick_file_type == QuickFileType::AlbumJson
        })
        .collect::<Vec<&ScanInfo>>();
    info!("Inspecting {} album files", scan_info_albums.len());
    let prog = Progress::new(scan_info_albums.len() as u64);
    let mut albums = Vec::new();
    for si in scan_info_albums {
        prog.inc();
        if let Some(album) = parse_album(container, si, files) {
            albums.push(album);
        }
    }
    drop(prog);
    albums
}

/// Map each original (source) media path to the album link names it belongs to,
/// so a photo's sidecar can list the albums it is part of.
fn build_album_membership(albums: &[Album]) -> HashMap<String, Vec<String>> {
    let mut by_path: HashMap<String, Vec<String>> = HashMap::new();
    for album in albums {
        let name = album_link_name(&album.desired_album_md_path);
        for file in &album.files {
            by_path.entry(file.clone()).or_default().push(name.clone());
        }
    }
    by_path
}

/// The album's vault link name: its file basename without the `albums/` folder or
/// `.md` extension (e.g. `albums/Trip.md` -> `Trip`).
fn album_link_name(desired_album_md_path: &str) -> String {
    let name = desired_album_md_path
        .strip_prefix("albums/")
        .unwrap_or(desired_album_md_path);
    name.strip_suffix(".md").unwrap_or(name).to_string()
}

/// Album names (deduplicated, order preserved) for a media file given all of its
/// original paths.
fn album_names_for(
    album_names_by_path: &HashMap<String, Vec<String>>,
    original_paths: &[String],
) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for path in original_paths {
        if let Some(album_names) = album_names_by_path.get(path) {
            for name in album_names {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
        }
    }
    names
}

/// Read the user-authored notes section from an existing album file, if any.
fn read_album_notes(output_container: &dyn FileSystem, path: &str) -> String {
    if !output_container.exists(path) {
        return String::new();
    }
    let Ok(mut reader) = output_container.open(path) else {
        return String::new();
    };
    let mut bytes = Vec::new();
    if reader.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    split_album_notes(&String::from_utf8_lossy(&bytes))
}

pub(crate) fn write_media(
    media_file: &MediaFileInfo,
    derived: &MediaFileDerivedInfo,
    dry_run: bool,
    input_container: &dyn FileSystem,
    output_container: &dyn WritableFileSystem,
) -> anyhow::Result<String> {
    let desired_output_path_with_ext =
        match Deduplicator::resolve_output_path(media_file, derived, output_container)? {
            DeDuplicationResult::SkipWrite(path) => return Ok(path),
            DeDuplicationResult::WritePath(path) => path,
        };
    info!("Output {:?}", desired_output_path_with_ext);
    let mut reader = input_container.open(&media_file.original_file_this_run)?;
    output_container.write(dry_run, &desired_output_path_with_ext, &mut reader)?;
    output_container.set_modified(dry_run, &desired_output_path_with_ext, &media_file.modified);
    Ok(desired_output_path_with_ext)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::OsFileSystem;
    use crate::test_util::build_zip;
    use anyhow::anyhow;
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::fs::read_to_string;
    use std::path::{Path, PathBuf};

    /// Tiny Google Takeout. Every media file has a `.supplemental-metadata.json`
    /// with a fixed `photoTakenTime`, so dates are derived in UTC and identical on every machine.
    const TAKEOUT_BASIC: &str = "test/takeout_basic";

    fn run_sync(input: &str) -> anyhow::Result<(tempfile::TempDir, PathBuf)> {
        crate::test_util::setup_log();
        let temp = tempfile::tempdir()?;
        let archive = temp.path().join("archive");
        let output = Some(archive.to_string_lossy().to_string());
        main(false, input, &output, false, false, false, true)?;
        Ok((temp, archive))
    }

    fn output_tree(archive: &Path) -> anyhow::Result<BTreeMap<String, String>> {
        let mut tree = BTreeMap::new();
        for path in files_under(archive)? {
            let rel = path
                .strip_prefix(archive)?
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            let cs = format!("{:x}", md5::compute(&fs::read(&path)?));
            tree.insert(rel, cs);
        }
        Ok(tree)
    }

    /// Relative path -> modified time for every file under `archive`. Used to
    /// prove a re-run rewrites nothing: an untouched file keeps its mtime.
    fn mtimes_under(archive: &Path) -> anyhow::Result<BTreeMap<String, std::time::SystemTime>> {
        let mut tree = BTreeMap::new();
        for path in files_under(archive)? {
            let rel = path
                .strip_prefix(archive)?
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            tree.insert(rel, fs::metadata(&path)?.modified()?);
        }
        Ok(tree)
    }

    /// Every regular file under `dir`, recursing into subdirectories.
    fn files_under(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                out.extend(files_under(&path)?);
            } else {
                out.push(path);
            }
        }
        Ok(out)
    }

    /// Recursively copy a directory tree so a test can run against a copy
    fn copy_dir_all(src: &Path, dst: &Path) -> anyhow::Result<()> {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let from = entry.path();
            let to = dst.join(entry.file_name());
            if from.is_dir() {
                copy_dir_all(&from, &to)?;
            } else {
                fs::copy(&from, &to)?;
            }
        }
        Ok(())
    }

    #[test]
    fn sync_dates_media_from_supplemental_metadata() -> anyhow::Result<()> {
        let (_temp, archive) = run_sync(TAKEOUT_BASIC)?;
        assert!(archive.join("2024/05/22/0017-51000.jpg").exists());
        assert!(archive.join("2023/11/02/0930-00000.mp4").exists());
        assert!(!archive.join("undated").exists());
        let md = read_to_string(archive.join("2024/05/22/0017-51000.md"))?;
        assert!(md.contains("datetime: \"2024-05-22T00:17:51+00:00\""));
        Ok(())
    }

    #[test]
    fn sync_deduplicates_identical_photo() -> anyhow::Result<()> {
        let (_temp, archive) = run_sync(TAKEOUT_BASIC)?;
        let jpgs: Vec<_> = files_under(&archive)?
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "jpg"))
            .collect();
        assert_eq!(jpgs.len(), 1);
        let md = read_to_string(archive.join("2024/05/22/0017-51000.md"))?;
        assert!(md.contains(
            "checksum: 6bfdabd4fc33d112283c147acccc574e770bbe6fbdbc3d4da968ba7b606ecc2f"
        ));
        assert!(md.contains("- Google Photos/Holiday/Canon_40D.jpg"));
        assert!(md.contains("- Google Photos/Photos from 2024/Canon_40D.jpg"));
        Ok(())
    }

    #[test]
    fn sync_writes_album_and_membership() -> anyhow::Result<()> {
        let (_temp, archive) = run_sync(TAKEOUT_BASIC)?;
        let album = read_to_string(archive.join("albums/Holiday.md"))?;
        assert!(album.contains("# Holiday Snaps"));
        assert!(album.contains("](../2024/05/22/0017-51000.jpg)"));
        let photo_md = read_to_string(archive.join("2024/05/22/0017-51000.md"))?;
        assert!(photo_md.contains("[[Holiday]]"));
        Ok(())
    }

    #[test]
    fn sync_rerun_rewrites_nothing() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let temp = tempfile::tempdir()?;
        let archive = temp.path().join("archive");
        let output = Some(archive.to_string_lossy().to_string());
        let input = TAKEOUT_BASIC.to_string();

        // First run populates the archive
        main(false, &input, &output, false, false, false, true)?;
        let first = mtimes_under(&archive)?;
        assert!(
            first.contains_key("albums/Holiday.md")
                && first.contains_key("2024/05/22/0017-51000.md")
                && first.contains_key("2024/05/22/0017-51000.jpg"),
            "first run should have written media, sidecar and album files"
        );

        // Re-running over identical input must be a no-op in writes
        main(false, &input, &output, false, false, false, true)?;
        let second = mtimes_under(&archive)?;
        assert_eq!(
            first, second,
            "re-running over unchanged input must not rewrite any output file"
        );
        Ok(())
    }

    /// A sync must never modify, delete, or add anything in the input tree. Snapshot the
    /// input before the run and assert it is byte-for-byte identical afterward.
    #[test]
    fn sync_never_modifies_input() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let temp = tempfile::tempdir()?;
        let input = temp.path().join("input");
        copy_dir_all(Path::new(TAKEOUT_BASIC), &input)?;

        let before = output_tree(&input)?;
        assert!(!before.is_empty(), "copy should contain files");

        let output = temp.path().join("archive");
        let output_s = Some(output.to_string_lossy().to_string());
        main(
            false,
            &input.to_string_lossy(),
            &output_s,
            false,
            false,
            false,
            true,
        )?;

        let after = output_tree(&input)?;
        assert_eq!(
            before, after,
            "sync must not add, remove, or modify any file in the input tree"
        );
        Ok(())
    }

    /// The note beside a media file: its extension swapped for `.md` - see
    /// [`crate::markdown::get_desired_markdown_path`]. Only valid where no
    /// same-instant sibling in another format is competing for the name.
    fn note_path(media: &Path) -> PathBuf {
        media.with_extension("md")
    }

    /// A live photo is one asset in two files: the still owns the note and the
    /// clip is written beside it without one, named by the still's `motion` key.
    ///
    /// Run against the real Takeout export in `test/livephoto` - an iPhone 13
    /// pair with the sidecar json Google attaches to the still, which the clip
    /// inherits, so both are dated alike and land in one folder.
    #[test]
    fn sync_live_photo_gives_the_still_the_note() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let temp = tempfile::tempdir()?;
        let output = temp.path().join("output");

        let output_s = Some(output.to_string_lossy().to_string());
        main(
            false,
            "test/livephoto",
            &output_s,
            false,
            false,
            false,
            true,
        )?;

        // Dated from the sidecar's `photoTakenTime` (1674029138), which outranks
        // the camera's own reading - but named `2105` rather than `0805`, the
        // hour Google's bare timestamp reads in UTC. Takeout exports no offset,
        // so the still's EXIF is asked for the wall clock and says +13:00: the
        // photo was taken at five past nine in the evening in Wellington, which
        // is where the same json's `geoData` puts it.
        //
        // Without that repair this test and its neighbour below - the same two
        // files, synced without the json beside them - disagreed by thirteen
        // hours about when one shutter press happened.
        let dir = output.join("2023/01/18");
        // Named for what the bytes are, not what the archive called them:
        // Takeout handed over a JPEG named `.HEIC` and a QuickTime named `.MP4`.
        //
        // One asset, one name, three extensions. The stem is the still's - the
        // clip is a sidecar of it, like the note - so the fraction is the
        // still's `SubSecTimeOriginal` of 489 even though the clip's own
        // QuickTime time has no fraction to offer.
        assert!(dir.join("2105-38489.jpg").is_file(), "the still is written");
        assert!(dir.join("2105-38489.mov").is_file(), "the clip is written");

        // ...and exactly one note describes the pair, the still's.
        let notes: Vec<PathBuf> = files_under(&output)?
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .collect();
        assert_eq!(
            notes,
            vec![dir.join("2105-38489.md")],
            "only the still should have a note"
        );
        let md = read_to_string(&notes[0])?;
        assert!(
            md.contains("- IMG_3221.HEIC") && !md.contains("- IMG_3221.MP4"),
            "the note describes the still, not the clip:\n{md}"
        );
        // The name is now predictable, but the key still earns its place: the
        // extension is whatever the clip's bytes turned out to be (`.mov` here,
        // from a file Takeout called `.MP4`), and its absence is how a reader
        // knows a photo has no clip at all.
        assert!(
            md.contains("motion: 2105-38489.mov"),
            "the note should name its clip:\n{md}"
        );
        assert!(
            md.contains("![](2105-38489.jpg)"),
            "embeds the still:\n{md}"
        );
        // The pair's own coordinates, resolved from the sidecar Google wrote.
        assert!(
            md.contains("latitude: -41.2818"),
            "the note carries the location:\n{md}"
        );
        Ok(())
    }

    /// Without Google's sidecar the two halves date themselves, and disagree:
    /// the still's EXIF carries `SubSecTimeOriginal` 489 while the clip's
    /// QuickTime time is a whole second. This is the iCloud shape - that export
    /// has no sidecars at all - and left to itself the clip would be written
    /// `2105-38000.mov` beside a `2105-38489.jpg` still. It takes the still's
    /// name instead.
    #[test]
    fn sync_live_photo_clip_takes_the_stills_name() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let temp = tempfile::tempdir()?;
        let input = temp.path().join("input");
        let output = temp.path().join("output");
        fs::create_dir_all(&input)?;
        // The pair without the `.supplemental-metadata.json` beside it.
        for name in ["IMG_3221.HEIC", "IMG_3221.MP4"] {
            fs::copy(Path::new("test/livephoto").join(name), input.join(name))?;
        }

        let input_s = input.to_string_lossy().to_string();
        let output_s = Some(output.to_string_lossy().to_string());
        main(false, &input_s, &output_s, false, false, false, true)?;

        // The still's own reading: 2023-01-18T21:05:38.489+13:00.
        let dir = output.join("2023/01/18");
        assert!(dir.join("2105-38489.jpg").is_file(), "the still is written");
        assert!(
            dir.join("2105-38489.mov").is_file(),
            "the clip takes the still's name, fraction included"
        );
        assert!(
            !dir.join("2105-38000.mov").exists(),
            "the clip must not name itself from its own whole-second time"
        );
        let md = read_to_string(dir.join("2105-38489.md"))?;
        assert!(
            md.contains("motion: 2105-38489.mov"),
            "the note names the clip beside it:\n{md}"
        );
        Ok(())
    }

    /// A clip is a sidecar of its still, so it follows the still wherever the
    /// still went - including into a different day's folder, and including onto
    /// a collision-suffixed name - and keeps only its own extension.
    #[test]
    fn test_derived_for_clip_follows_the_still() -> anyhow::Result<()> {
        let mut clip = MediaFileInfo::new_for_test();
        clip.original_file_this_run = "IMG_1.MP4".to_string();
        clip.accurate_file_type = crate::file_type::AccurateFileType::Mov;

        let mut checksum_by_original_path = HashMap::new();
        checksum_by_original_path.insert("IMG_1.HEIC", "still-checksum");
        let mut final_path_by_checksum = HashMap::new();

        // The still landed on a different day to anything the clip would have
        // chosen for itself - the still's EXIF is a local wall clock, the clip's
        // QuickTime time is UTC - and the clip goes with it.
        final_path_by_checksum.insert(
            "still-checksum".to_string(),
            "2023/01/18/2105-38489.jpg".to_string(),
        );
        let derived = derived_for_clip(
            &clip,
            "IMG_1.HEIC",
            &checksum_by_original_path,
            &final_path_by_checksum,
        );
        assert_eq!(derived.desired_media_path, "2023/01/18/2105-38489");
        assert_eq!(
            derived.desired_media_extension, "mov",
            "the extension stays the clip's own, read from its bytes"
        );

        // A still pushed onto a checksum suffix hands it on, so the pair stays
        // together rather than the clip claiming the bare name.
        final_path_by_checksum.insert(
            "still-checksum".to_string(),
            "2023/01/18/2105-38489-a1b2c3d.jpg".to_string(),
        );
        let derived = derived_for_clip(
            &clip,
            "IMG_1.HEIC",
            &checksum_by_original_path,
            &final_path_by_checksum,
        );
        assert_eq!(derived.desired_media_path, "2023/01/18/2105-38489-a1b2c3d");

        // The still never made it into the archive. Naming the clip after it
        // would point at nothing, so the clip keeps its own name - here the
        // `undated/` one it gets with no date of its own.
        let derived = derived_for_clip(
            &clip,
            "IMG_1.HEIC",
            &checksum_by_original_path,
            &HashMap::new(),
        );
        assert_eq!(derived.desired_media_path, "undated/tsc");
        Ok(())
    }

    /// Two *unrelated* files captured in the same millisecond and stored in
    /// different formats both keep the bare date name (collisions are resolved
    /// per full name, extension included), so they want one note between them.
    /// Sharing a stem is what makes a live photo; these do not, so there is no
    /// pair to collapse. The first there keeps the note and the second is warned
    /// about rather than given an invented name; what must never happen is the
    /// two sharing a note, pooling their `original-paths` under one checksum.
    #[test]
    fn sync_same_instant_different_formats_do_not_share_a_note() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let temp = tempfile::tempdir()?;
        let input = temp.path().join("input");
        let output = temp.path().join("output");
        fs::create_dir_all(&input)?;

        // A photo and a video sharing one photoTakenTime, as a live photo would.
        for src in ["test/Canon_40D.jpg", "test/Hello.mp4"] {
            let name = Path::new(src)
                .file_name()
                .ok_or_else(|| anyhow!("fixture has no file name: {src}"))?;
            fs::copy(src, input.join(name))?;
            fs::write(
                input.join(format!(
                    "{}.supplemental-metadata.json",
                    name.to_string_lossy()
                )),
                r#"{"photoTakenTime":{"timestamp":"1700000000"}}"#,
            )?;
        }

        let input_s = input.to_string_lossy().to_string();
        let output_s = Some(output.to_string_lossy().to_string());
        main(false, &input_s, &output_s, false, false, false, true)?;

        // Both files are written; only the name they share carries a note.
        assert!(output.join("2023/11/14/2213-20000.jpg").is_file());
        assert!(output.join("2023/11/14/2213-20000.mp4").is_file());
        let notes: Vec<PathBuf> = files_under(&output)?
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .collect();
        assert_eq!(
            notes,
            vec![output.join("2023/11/14/2213-20000.md")],
            "one note, under the preferred name, and no invented second name"
        );

        // It records exactly one of the two - whichever was inspected first -
        // never both, which is the pooling this guards against.
        let md = read_to_string(&notes[0])?;
        let has_jpg = md.contains("- Canon_40D.jpg");
        let has_mp4 = md.contains("- Hello.mp4");
        assert!(
            has_jpg ^ has_mp4,
            "the note must record one file, not both:\n{md}"
        );
        let embedded = if has_jpg {
            "![](2213-20000.jpg)"
        } else {
            "![](2213-20000.mp4)"
        };
        assert!(
            md.contains(embedded),
            "note should embed its own file:\n{md}"
        );
        Ok(())
    }

    /// A note sitting at the appended name is kept there rather than stranded,
    /// so an archive synced while that was the default keeps the prose in it.
    #[test]
    fn sync_keeps_an_existing_appended_note_for_the_same_file() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let temp = tempfile::tempdir()?;
        let input = temp.path().join("input");
        let output = temp.path().join("output");
        fs::create_dir_all(&input)?;
        fs::copy("test/Canon_40D.jpg", input.join("Canon_40D.jpg"))?;

        let input_s = input.to_string_lossy().to_string();
        let output_s = Some(output.to_string_lossy().to_string());
        main(false, &input_s, &output_s, false, false, false, true)?;

        // Rename the note to the appended form and add prose, as an archive
        // synced by the version that appended `.md` would look.
        let dir = output.join("2008/05/30");
        let appended = dir.join("1556-01000.jpg.md");
        fs::rename(dir.join("1556-01000.md"), &appended)?;
        let mut body = read_to_string(&appended)?;
        body.push_str("\nProse written years ago.\n");
        fs::write(&appended, &body)?;

        main(false, &input_s, &output_s, false, false, false, true)?;

        assert!(
            appended.is_file(),
            "the existing note must be kept, not replaced"
        );
        assert!(
            !dir.join("1556-01000.md").exists(),
            "keeping the existing note must not also write one under the preferred name"
        );
        assert!(
            read_to_string(&appended)?.contains("Prose written years ago."),
            "the prose in the existing note must survive"
        );
        Ok(())
    }

    /// Metadata from an `.xmp` sidecar reaches the note, and `--skip-xmp` keeps
    /// it out.
    #[test]
    fn sync_reads_xmp_sidecars() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let temp = tempfile::tempdir()?;
        let input = temp.path().join("input");
        fs::create_dir_all(&input)?;
        fs::copy("test/Canon_40D.jpg", input.join("Canon_40D.jpg"))?;
        fs::copy("test/Canon_40D.jpg.xmp", input.join("Canon_40D.jpg.xmp"))?;
        let input_s = input.to_string_lossy().to_string();

        let with_xmp = temp.path().join("with");
        let with_xmp_s = Some(with_xmp.to_string_lossy().to_string());
        main(false, &input_s, &with_xmp_s, false, false, false, false)?;
        let md = read_to_string(with_xmp.join("2008/05/30/1556-01000.md"))?;
        assert!(md.contains("rating: 5"), "xmp rating should reach the note");
        assert!(md.contains("label: Green"));
        assert!(md.contains("title: A Canon 40D test frame"));
        // Hierarchical keywords arrive re-spelled for Obsidian, flat ones as-is.
        assert!(md.contains("- cameras/canon"));
        assert!(md.contains("- test-fixture"));
        assert!(md.contains("[[Ada Lovelace]]"));
        // The title heads the body and the description follows it, both below
        // the image embed.
        assert!(md.contains(
            "![](1556-01000.jpg)\n\n# A Canon 40D test frame\n\nSample image used by the ptsync test suite.\n"
        ));

        let without = temp.path().join("without");
        let without_s = Some(without.to_string_lossy().to_string());
        main(false, &input_s, &without_s, false, false, false, true)?;
        let md = read_to_string(without.join("2008/05/30/1556-01000.md"))?;
        assert!(!md.contains("rating:"), "--skip-xmp should read no sidecar");
        assert!(!md.contains("Ada Lovelace"));
        Ok(())
    }

    /// A title and caption set in Google Photos reach the note body, under the
    /// image embed. Nothing else in the archive carries them: Takeout writes
    /// them only into its json, so if they don't land here they are lost.
    #[test]
    fn sync_reads_supplemental_title_and_description() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let temp = tempfile::tempdir()?;
        let input = temp.path().join("input");
        let output = temp.path().join("output");
        fs::create_dir_all(&input)?;
        // No `.xmp` beside it, so the json is the only source of either field.
        fs::copy("test/Canon_40D.jpg", input.join("Canon_40D.jpg"))?;
        fs::write(
            input.join("Canon_40D.jpg.supplemental-metadata.json"),
            r#"{
              "title": "First light",
              "description": "Straight out of the camera.",
              "photoTakenTime": { "timestamp": "1212162961", "formatted": "30 May 2008, 15:56:01 UTC" }
            }"#,
        )?;

        let input_s = input.to_string_lossy().to_string();
        let output_s = Some(output.to_string_lossy().to_string());
        main(false, &input_s, &output_s, false, false, false, true)?;

        let md = read_to_string(output.join("2008/05/30/1556-01000.md"))?;
        assert!(
            md.contains("![](1556-01000.jpg)\n\n# First light\n\nStraight out of the camera.\n"),
            "title and description belong under the embed, got:\n{md}"
        );
        assert!(md.contains("title: First light"));
        Ok(())
    }

    /// Google fills `title` with the uploaded file's name, which is not a title
    /// and must not become one.
    #[test]
    fn sync_ignores_a_supplemental_title_that_is_the_file_name() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let temp = tempfile::tempdir()?;
        let input = temp.path().join("input");
        let output = temp.path().join("output");
        fs::create_dir_all(&input)?;
        fs::copy("test/Canon_40D.jpg", input.join("Canon_40D.jpg"))?;
        fs::write(
            input.join("Canon_40D.jpg.supplemental-metadata.json"),
            r#"{ "title": "Canon_40D.jpg", "description": "Straight out of the camera." }"#,
        )?;

        let input_s = input.to_string_lossy().to_string();
        let output_s = Some(output.to_string_lossy().to_string());
        main(false, &input_s, &output_s, false, false, false, true)?;

        let md = read_to_string(output.join("2008/05/30/1556-01000.md"))?;
        assert!(!md.contains("title:"), "no title should be written:\n{md}");
        assert!(!md.contains("# Canon_40D.jpg"));
        // The description is unaffected by any of that.
        assert!(md.contains("![](1556-01000.jpg)\n\nStraight out of the camera.\n"));
        Ok(())
    }

    /// Google's star and archive flags reach the note as frontmatter booleans,
    /// and only when they are set.
    #[test]
    fn sync_reads_supplemental_flags() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let temp = tempfile::tempdir()?;
        let input = temp.path().join("input");
        let output = temp.path().join("output");
        fs::create_dir_all(&input)?;
        fs::copy("test/Canon_40D.jpg", input.join("Canon_40D.jpg"))?;
        fs::write(
            input.join("Canon_40D.jpg.supplemental-metadata.json"),
            r#"{ "favorited": true }"#,
        )?;

        let input_s = input.to_string_lossy().to_string();
        let output_s = Some(output.to_string_lossy().to_string());
        main(false, &input_s, &output_s, false, false, false, true)?;

        let md = read_to_string(output.join("2008/05/30/1556-01000.md"))?;
        assert!(md.contains("favorite: true"), "got:\n{md}");
        assert!(
            !md.contains("archived"),
            "an unset flag writes no key at all, got:\n{md}"
        );
        // A favourite is not a rating, so the note gains no stars from one.
        assert!(!md.contains("rating:"), "got:\n{md}");
        Ok(())
    }

    /// A rating edited by hand in the archive must not be reverted to whatever
    /// the sidecar says on the next run - the note is the master copy.
    #[test]
    fn sync_does_not_revert_hand_edited_xmp_fields() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let temp = tempfile::tempdir()?;
        let input = temp.path().join("input");
        let output = temp.path().join("output");
        fs::create_dir_all(&input)?;
        fs::copy("test/Canon_40D.jpg", input.join("Canon_40D.jpg"))?;
        fs::copy("test/Canon_40D.jpg.xmp", input.join("Canon_40D.jpg.xmp"))?;

        let input_s = input.to_string_lossy().to_string();
        let output_s = Some(output.to_string_lossy().to_string());
        main(false, &input_s, &output_s, false, false, false, false)?;

        let note = output.join("2008/05/30/1556-01000.md");
        let edited = read_to_string(&note)?.replace("rating: 5", "rating: 2");
        fs::write(&note, &edited)?;

        main(false, &input_s, &output_s, false, false, false, false)?;
        let after = read_to_string(&note)?;
        assert!(
            after.contains("rating: 2") && !after.contains("rating: 5"),
            "a hand-edited rating must survive a re-sync, got:\n{after}"
        );
        Ok(())
    }

    /// Different photos taken at the same time must each get their own
    /// sidecar. The date-based name collides, so the second gains a
    /// checksum suffix, the md should match.
    #[test]
    fn sync_same_instant_photos_each_get_a_sidecar() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let temp = tempfile::tempdir()?;
        let input = temp.path().join("input");
        let output = temp.path().join("output");
        fs::create_dir_all(&input)?;

        // Two distinct photos at the same photoTakenTime, so both want 2023/11/14/2213-20000.
        let base = fs::read("test/Canon_40D.jpg")?;
        for (name, marker) in [("a.jpg", "X"), ("b.jpg", "YY")] {
            let mut bytes = base.clone();
            bytes.extend_from_slice(marker.as_bytes());
            fs::write(input.join(name), &bytes)?;
            fs::write(
                input.join(format!("{name}.supplemental-metadata.json")),
                r#"{"photoTakenTime":{"timestamp":"1700000000"}}"#,
            )?;
        }

        let input_s = input.to_string_lossy().to_string();
        let output_s = Some(output.to_string_lossy().to_string());
        main(false, &input_s, &output_s, false, false, false, true)?;

        // Both photos written, one keeps bare date name, the other is suffixed.
        let media: Vec<PathBuf> = files_under(&output)?
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "jpg"))
            .collect();
        assert_eq!(media.len(), 2, "both same-instant photos must be written");
        assert!(
            output.join("2023/11/14/2213-20000.jpg").exists(),
            "one photo should keep the bare date name"
        );

        // Exactly one sidecar per media file
        let sidecars: BTreeSet<PathBuf> = files_under(&output)?
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .collect();
        let expected: BTreeSet<PathBuf> = media.iter().map(|p| note_path(p)).collect();
        assert_eq!(
            sidecars, expected,
            "each media file must have exactly one matching sibling sidecar"
        );

        // Each sidecar embeds its *own* photo by name and records that photo's details
        let mut sources = BTreeSet::new();
        for photo in &media {
            let file_name = photo
                .file_name()
                .ok_or_else(|| anyhow!("media path has no file name: {photo:?}"))?
                .to_string_lossy()
                .to_string();
            let md = read_to_string(note_path(photo))?;
            assert!(
                md.contains(&format!("![]({file_name})")),
                "sidecar for {file_name} should embed its own photo, got:\n{md}"
            );
            for src in ["a.jpg", "b.jpg"] {
                if md.contains(&format!("- {src}")) {
                    sources.insert(src);
                }
            }
        }
        assert_eq!(
            sources,
            BTreeSet::from(["a.jpg", "b.jpg"]),
            "each source photo should be recorded in its own sidecar"
        );

        // re-running over the same input rewrites nothing.
        let first = mtimes_under(&output)?;
        main(false, &input_s, &output_s, false, false, false, true)?;
        let second = mtimes_under(&output)?;
        assert_eq!(
            first, second,
            "re-running over same-instant input must not rewrite any output file"
        );
        Ok(())
    }

    #[test]
    fn sync_zip_and_directory_produce_identical_output() -> anyhow::Result<()> {
        let (_dir_temp, dir_archive) = run_sync(TAKEOUT_BASIC)?;
        let _zip = build_zip(TAKEOUT_BASIC)?;
        let zip_path = _zip.path().to_string_lossy().to_string();
        let (_zip_temp, zip_archive) = run_sync(&zip_path)?;

        let dir_tree = output_tree(&dir_archive)?;
        let zip_tree = output_tree(&zip_archive)?;
        assert!(
            dir_tree.contains_key("2024/05/22/0017-51000.jpg")
                && dir_tree.contains_key("2024/05/22/0017-51000.md")
                && dir_tree.contains_key("albums/Holiday.md")
        );
        assert_eq!(dir_tree, zip_tree);
        Ok(())
    }

    /// The write path is generic over `WritableFileSystem`. Driving it against
    /// the S3 fake (not `OsFileSystem`) must still produce each media file and
    /// its sidecar, and a second pass must add nothing - proving the dedup checks
    /// work against a non-OS backend. Because the fake reports a native checksum,
    /// the second pass also exercises the Option A fast path (skip via
    /// `recorded_checksum`, no re-read). This is the seam real S3 output reuses.
    #[test]
    fn sync_writes_through_writable_trait_to_fake_s3() -> anyhow::Result<()> {
        use crate::output_path::media_file_derived_from_media_info;
        use crate::s3_fs::FakeS3FileSystem;
        crate::test_util::setup_log();

        let input: Arc<dyn FileSystem> = Arc::new(OsFileSystem::new(TAKEOUT_BASIC));
        let files = scan_fs(input.as_ref());
        let media_si: Vec<ScanInfo> = files
            .iter()
            .filter(|m| m.quick_file_type == QuickFileType::Media)
            .cloned()
            .collect();

        let mut deduper = Deduplicator::new();
        let prog = Arc::new(Progress::new(media_si.len() as u64));
        let mut inspected = inspect_media_files(input.clone(), media_si, prog.clone(), true);
        for media in inspected.by_ref() {
            deduper.add(media);
        }
        drop(prog);

        // First pass writes media + sidecars into the fake bucket.
        let out = FakeS3FileSystem::new();
        for media in deduper.sorted_media() {
            let derived = media_file_derived_from_media_info(media);
            let final_path = write_media(media, &derived, false, input.as_ref(), &out)?;
            sync_markdown(false, media, &final_path, &[], None, &out)?;
        }
        assert!(out.exists("2024/05/22/0017-51000.jpg"));
        assert!(out.exists("2024/05/22/0017-51000.md"));
        // The fake surfaces the object's SHA-256 the way S3's native checksum
        // does - this is the value the Option A fast path compares against, so a
        // metadata-only HeadObject can answer "already here?" without a GET.
        assert_eq!(
            out.recorded_checksum("2024/05/22/0017-51000.jpg")
                .as_deref(),
            Some("6bfdabd4fc33d112283c147acccc574e770bbe6fbdbc3d4da968ba7b606ecc2f")
        );

        // Second pass over identical input must add nothing: the media dedups to
        // SkipWrite (via the fake's recorded checksum) and the sidecar is unchanged.
        let before = out.walk().len();
        for media in deduper.sorted_media() {
            let derived = media_file_derived_from_media_info(media);
            let final_path = write_media(media, &derived, false, input.as_ref(), &out)?;
            sync_markdown(false, media, &final_path, &[], None, &out)?;
        }
        assert_eq!(out.walk().len(), before, "re-run must not add new objects");
        Ok(())
    }
}
