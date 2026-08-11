use crate::album::{Album, build_album_md, parse_album, split_album_notes};
use crate::dedup::{DeDuplicationResult, Deduplicator};
use crate::file_type::{QuickFileType, file_ext_from_file_type};
use crate::fs::{FileSystem, WritableFileSystem, open_input, open_output};
use crate::inspect::inspect_media_files;
use crate::live_photo::LivePhotos;
use crate::markdown::{NoteLinks, sync_markdown};
use crate::media::{MediaFileDerivedInfo, MediaFileInfo, media_file_derived_from_media_info};
use crate::progress::Progress;
use crate::util::{OutputTZ, ScanInfo, name_part, scan_fs, strip_extension};
use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;
use tracing::{info, warn};

pub(crate) fn main(
    dry_run: bool,
    input: &str,
    output_directory: &Option<String>,
    skip_markdown: bool,
    skip_media: bool,
    skip_albums: bool,
    tz: OutputTZ,
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

    // Parsed up front so each photo's sidecar can record its albums; the album
    // files themselves are written later, once the output paths are known.
    let albums = if skip_albums {
        Vec::new()
    } else {
        parse_albums(container.as_ref(), &files)
    };
    let album_names_by_path = build_album_membership(&albums);

    if !skip_media {
        let media_si_files: Vec<ScanInfo> = files
            .iter()
            .filter(|m| m.quick_file_type == QuickFileType::Media)
            .cloned()
            .collect();
        info!("Inspecting {} photo and video files", media_si_files.len());
        let prog = Arc::new(Progress::new(media_si_files.len() as u64));
        // Inspection is parallel, but dedup stays on this thread since it mutates
        // the shared collection.
        let mut inspected = inspect_media_files(container.clone(), media_si_files, prog.clone());
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
            let live_photos = LivePhotos::build(&media_to_write);
            info!("Outputting {} photo and video files", media_to_write.len());
            let prog = Progress::new(media_to_write.len() as u64);

            // A Live Photo's video is named after wherever its still landed, so
            // the stills have to be written and resolved first.
            let (primaries, sidecar_videos): (Vec<&MediaFileInfo>, Vec<&MediaFileInfo>) =
                media_to_write
                    .iter()
                    .partition(|m| !live_photos.is_sidecar_video(&m.hash_info.long_checksum));

            for media in primaries.iter().chain(sidecar_videos.iter()) {
                prog.inc();
                let derived = desired_output(media, &live_photos, &final_path_by_checksum, tz)?;
                let write_r = write_media(
                    media,
                    &derived,
                    dry_run,
                    container.as_ref(),
                    output_container,
                );
                match write_r {
                    Ok(final_path) => {
                        let long_checksum = &media.hash_info.long_checksum;
                        final_path_by_checksum.insert(long_checksum.clone(), final_path);
                    }
                    Err(e) => {
                        warn!(
                            "Error writing media file: {:?}, error: {}",
                            derived.desired_media_path, e
                        );
                    }
                }
            }
            drop(prog);

            // Written last so a still's note can name the video that ended up
            // beside it.
            if !skip_markdown {
                for media in &media_to_write {
                    let Some(final_path) =
                        final_path_by_checksum.get(&media.hash_info.long_checksum)
                    else {
                        continue; // the media file itself failed to write
                    };
                    // A video that took its still's name is covered by that
                    // still's note. One whose still never got written was filed
                    // under its own date, so it still needs a note of its own.
                    if still_path_of(media, &live_photos, &final_path_by_checksum).is_some() {
                        continue;
                    }
                    let links = NoteLinks {
                        albums: album_names_for(&album_names_by_path, &media.original_path),
                        live_photo_video: live_photo_video_name(
                            media,
                            &live_photos,
                            &final_path_by_checksum,
                        ),
                    };
                    let sync_md_r =
                        sync_markdown(dry_run, media, final_path, &links, output_container, tz);
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
            // The photo list is regenerated every run, so writing only on a real
            // difference is what leaves a re-run's files and mtimes untouched.
            if let Err(e) = output_container.write_if_changed(dry_run, output_path, md.as_bytes()) {
                warn!("Error writing album file {output_path:?}: {e}");
            }
        }
    }

    Ok(())
}

/// Where this file's Live Photo still was written, if it is a video that has
/// one and that still made it to disk.
///
/// Being able to answer is what makes a file a sidecar: it takes that name, and
/// is covered by that still's note. A video whose still failed to write is not
/// one, and is filed and noted in its own right instead — better a video under
/// its own date than a video nobody wrote down.
fn still_path_of(
    media: &MediaFileInfo,
    live_photos: &LivePhotos,
    final_path_by_checksum: &HashMap<String, String>,
) -> Option<String> {
    let still = live_photos.still_for_video(&media.hash_info.long_checksum)?;
    final_path_by_checksum.get(still).cloned()
}

/// Where a media file wants to go: its own date-derived path, or — for a Live
/// Photo's video — the name its still was written under, so the pair sits side
/// by side under one name.
fn desired_output(
    media: &MediaFileInfo,
    live_photos: &LivePhotos,
    final_path_by_checksum: &HashMap<String, String>,
    tz: OutputTZ,
) -> anyhow::Result<MediaFileDerivedInfo> {
    match still_path_of(media, live_photos, final_path_by_checksum) {
        Some(still_path) => Ok(MediaFileDerivedInfo {
            desired_media_path: Some(strip_extension(&still_path)),
            desired_media_extension: file_ext_from_file_type(&media.accurate_file_type),
        }),
        None => media_file_derived_from_media_info(media, tz),
    }
}

/// The file name of the Live Photo video written beside this still, for the
/// still's note to point at.
fn live_photo_video_name(
    still: &MediaFileInfo,
    live_photos: &LivePhotos,
    final_path_by_checksum: &HashMap<String, String>,
) -> Option<String> {
    let video = live_photos.video_for_still(&still.hash_info.long_checksum)?;
    let video_path = final_path_by_checksum.get(video)?;
    Some(name_part(video_path))
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

/// Each original media path to the album link names it belongs to.
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

/// The album's vault link name: `albums/Trip.md` -> `Trip`.
fn album_link_name(desired_album_md_path: &str) -> String {
    let name = desired_album_md_path
        .strip_prefix("albums/")
        .unwrap_or(desired_album_md_path);
    name.strip_suffix(".md").unwrap_or(name).to_string()
}

/// Deduplicated, order preserved.
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
    use crate::test_util::tz;
    use anyhow::anyhow;
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::fs::read_to_string;
    use std::path::{Path, PathBuf};

    /// Tiny Google Takeout. Every media file has a `.supplemental-metadata.json`
    /// with a fixed `photoTakenTime`, so dates come from the sidecar rather than
    /// from whatever mtime the checkout happens to have.
    const TAKEOUT_BASIC: &str = "test/takeout_basic";

    /// Where the fixture's two media files land, minus the extension.
    ///
    /// These can be literals because every test here runs at
    /// [`crate::test_util::tz`]'s fixed `+12:00`. The jpg's instant is a quarter
    /// past midnight in Greenwich and a quarter past noon at +12:00, so a
    /// regression to rendering at UTC changes the name rather than quietly
    /// agreeing with itself.
    const FIXTURE_JPG_STEM: &str = "2024/05/22/1217-51000";
    const FIXTURE_MP4_STEM: &str = "2023/11/02/2130-00000";

    /// The instant behind [`FIXTURE_JPG_STEM`].
    const FIXTURE_JPG_EPOCH: i64 = 1716337071;

    fn run_sync(input: &str) -> anyhow::Result<(tempfile::TempDir, PathBuf)> {
        crate::test_util::setup_log();
        let temp = tempfile::tempdir()?;
        let archive = temp.path().join("archive");
        let output = Some(archive.to_string_lossy().to_string());
        main(false, input, &output, false, false, false, tz())?;
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

    /// An untouched file keeps its mtime, which is how a re-run is shown to have
    /// rewritten nothing.
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

    /// What one sync of the fixture produces: both media files under their
    /// sidecar dates, the photo that arrived twice written once, and the album
    /// linked from both sides.
    #[test]
    fn sync_writes_a_dated_deduplicated_archive() -> anyhow::Result<()> {
        let (_temp, archive) = run_sync(TAKEOUT_BASIC)?;
        let stem = FIXTURE_JPG_STEM;
        assert!(archive.join(format!("{stem}.jpg")).exists());
        assert!(archive.join(format!("{FIXTURE_MP4_STEM}.mp4")).exists());
        assert!(!archive.join("undated").exists());

        let md = read_to_string(archive.join(format!("{stem}.md")))?;
        // Compared as an instant rather than a string: the sidecar's offset is
        // this machine's, so the two only spell the same on a UTC machine.
        let recorded = md
            .lines()
            .find_map(|l| l.strip_prefix("datetime: "))
            .ok_or_else(|| anyhow!("sidecar has no datetime line:\n{md}"))?
            .trim_matches('"');
        assert_eq!(
            chrono::DateTime::parse_from_rfc3339(recorded)?.timestamp(),
            FIXTURE_JPG_EPOCH,
            "sidecar datetime {recorded:?} is not the instant the metadata recorded"
        );

        // The same photo sits in two source directories, so it is written once
        // with both original paths recorded against it.
        let jpgs: Vec<_> = files_under(&archive)?
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "jpg"))
            .collect();
        assert_eq!(jpgs.len(), 1);
        assert!(md.contains("- Google Photos/Holiday/Canon_40D.jpg"));
        assert!(md.contains("- Google Photos/Photos from 2024/Canon_40D.jpg"));

        // The album links to where the photo was written, and the photo's own
        // sidecar links back to the album.
        let album = read_to_string(archive.join("albums/Holiday.md"))?;
        assert!(album.contains("# Holiday Snaps"));
        assert!(album.contains(&format!("](../{stem}.jpg)")));
        assert!(md.contains("[[Holiday]]"));
        Ok(())
    }

    #[test]
    fn sync_rerun_rewrites_nothing() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let temp = tempfile::tempdir()?;
        let archive = temp.path().join("archive");
        let output = Some(archive.to_string_lossy().to_string());
        let input = TAKEOUT_BASIC.to_string();

        main(false, &input, &output, false, false, false, tz())?;
        let first = mtimes_under(&archive)?;
        let stem = FIXTURE_JPG_STEM;
        assert!(
            first.contains_key("albums/Holiday.md")
                && first.contains_key(&format!("{stem}.md"))
                && first.contains_key(&format!("{stem}.jpg")),
            "first run should have written media, sidecar and album files"
        );

        main(false, &input, &output, false, false, false, tz())?;
        let second = mtimes_under(&archive)?;
        assert_eq!(
            first, second,
            "re-running over unchanged input must not rewrite any output file"
        );
        Ok(())
    }

    /// A sync must never modify, delete, or add anything in the input tree.
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
            tz(),
        )?;

        let after = output_tree(&input)?;
        assert_eq!(
            before, after,
            "sync must not add, remove, or modify any file in the input tree"
        );
        Ok(())
    }

    /// The date-based name collides, so the second photo gains a checksum suffix
    /// and its sidecar must follow.
    #[test]
    fn sync_same_instant_photos_each_get_a_sidecar() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let temp = tempfile::tempdir()?;
        let input = temp.path().join("input");
        let output = temp.path().join("output");
        fs::create_dir_all(&input)?;

        // Two distinct photos sharing a photoTakenTime, so both want one name.
        const SAME_INSTANT_STEM: &str = "2023/11/15/1013-20000";
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
        main(false, &input_s, &output_s, false, false, false, tz())?;

        let media: Vec<PathBuf> = files_under(&output)?
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "jpg"))
            .collect();
        assert_eq!(media.len(), 2, "both same-instant photos must be written");
        assert!(
            output.join(format!("{SAME_INSTANT_STEM}.jpg")).exists(),
            "one photo should keep the bare date name"
        );

        let sidecars: BTreeSet<PathBuf> = files_under(&output)?
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .collect();
        let expected: BTreeSet<PathBuf> = media.iter().map(|p| p.with_extension("md")).collect();
        assert_eq!(
            sidecars, expected,
            "each media file must have exactly one matching sibling sidecar"
        );

        // Each sidecar embeds its *own* photo and records that photo's details.
        let mut sources = BTreeSet::new();
        for photo in &media {
            let file_name = photo
                .file_name()
                .ok_or_else(|| anyhow!("media path has no file name: {photo:?}"))?
                .to_string_lossy()
                .to_string();
            let md = read_to_string(photo.with_extension("md"))?;
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
        Ok(())
    }

    /// A still and video sharing a content identifier are written as one item:
    /// the video takes the still's name and gets no note, and the still's note
    /// is the only thing that records the pairing.
    ///
    /// The fixture's video has no usable capture time of its own — left to
    /// itself it lands under 1904 — so the date here can only have come from
    /// the still.
    #[test]
    fn sync_writes_a_live_photo_video_beside_its_still() -> anyhow::Result<()> {
        const LIVE_PHOTO: &str = "test/live_photo";
        const STEM: &str = "2008/05/30/1556-01000";
        let (_temp, archive) = run_sync(LIVE_PHOTO)?;

        assert!(archive.join(format!("{STEM}.jpg")).exists());
        assert!(
            archive.join(format!("{STEM}.mov")).exists(),
            "the video should be filed under the still's name"
        );
        assert!(!archive.join("1904").exists());

        let notes: Vec<PathBuf> = files_under(&archive)?
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .collect();
        assert_eq!(
            notes,
            vec![archive.join(format!("{STEM}.md"))],
            "only the still should have a note"
        );

        // Both spellings of the link: the frontmatter key, which is rewritten
        // every run, and the body line, which is only written on creation.
        let md = read_to_string(archive.join(format!("{STEM}.md")))?;
        assert!(md.contains("live-photo-video: 1556-01000.mov"), "{md}");
        assert!(md.contains("[Live Photo video](1556-01000.mov)"), "{md}");
        // The note is the still's, so it records the still's checksum and path.
        assert!(md.contains("- still.jpg"), "{md}");
        assert!(!md.contains("clip.mov"), "{md}");
        Ok(())
    }

    /// A Live Photo whose still collides with another photo takes a suffixed
    /// name, and the video has to follow it there rather than to the bare one.
    #[test]
    fn sync_live_photo_video_follows_a_suffixed_still() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let temp = tempfile::tempdir()?;
        let input = temp.path().join("input");
        let output = temp.path().join("output");
        fs::create_dir_all(&input)?;

        // A second photo already occupying the name the Live Photo's still
        // wants, with the same date fixed by a sidecar.
        fs::copy("test/live_photo/still.jpg", input.join("still.jpg"))?;
        fs::copy("test/live_photo/clip.mov", input.join("clip.mov"))?;
        let mut other = fs::read("test/Canon_40D.jpg")?;
        other.extend_from_slice(b"other");
        fs::write(input.join("other.jpg"), &other)?;

        main(
            false,
            &input.to_string_lossy(),
            &Some(output.to_string_lossy().to_string()),
            false,
            false,
            false,
            tz(),
        )?;

        // Whichever of the two photos took a suffix, the video sits beside the
        // one carrying the Live Photo's identifier and its note points at it.
        let videos: Vec<PathBuf> = files_under(&output)?
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "mov"))
            .collect();
        assert_eq!(videos.len(), 1);
        let video = videos
            .first()
            .ok_or_else(|| anyhow!("no video written"))?
            .clone();
        let note = video.with_extension("md");
        assert!(
            note.is_file(),
            "the video should sit beside its still's note, got {video:?}"
        );
        let file_name = video
            .file_name()
            .ok_or_else(|| anyhow!("video path has no file name"))?
            .to_string_lossy()
            .to_string();
        let md = read_to_string(&note)?;
        assert!(
            md.contains(&format!("live-photo-video: {file_name}")),
            "{md}"
        );
        assert!(md.contains("- still.jpg"), "{md}");
        Ok(())
    }

    /// Derived names are date and checksum based, so nothing an input name says
    /// can reach the output tree — and nothing may panic on the way.
    #[test]
    fn sync_over_hostile_input_names_stays_within_output() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let temp = tempfile::tempdir()?;
        let input = temp.path().join("input");
        let output = temp.path().join("output");

        // A unicode subdirectory holding two real photos — one under a reserved
        // device name, one long — each with supplemental metadata fixing the
        // date, plus an album metadata.json.
        let album_dir = input.join("café 📸 Ñoño");
        fs::create_dir_all(&album_dir)?;
        let base = fs::read("test/Canon_40D.jpg")?;
        for (name, marker) in [("CON.jpg", "A"), ("really_long_name_photo.jpg", "BB")] {
            let mut bytes = base.clone();
            bytes.extend_from_slice(marker.as_bytes());
            fs::write(album_dir.join(name), &bytes)?;
            fs::write(
                album_dir.join(format!("{name}.supplemental-metadata.json")),
                r#"{"photoTakenTime":{"timestamp":"1700000000"}}"#,
            )?;
        }
        fs::write(
            album_dir.join("metadata.json"),
            r#"{"title":"Weird 📸 Album"}"#,
        )?;

        main(
            false,
            &input.to_string_lossy(),
            &Some(output.to_string_lossy().to_string()),
            false,
            false,
            false,
            tz(),
        )?;

        let written = output_tree(&output)?;
        assert!(
            !written.is_empty(),
            "sync should have written at least one file"
        );
        for rel in written.keys() {
            assert!(
                !crate::test_util::escapes_output(rel),
                "sync wrote outside output: {rel}"
            );
        }
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
        let stem = FIXTURE_JPG_STEM;
        assert!(
            dir_tree.contains_key(&format!("{stem}.jpg"))
                && dir_tree.contains_key(&format!("{stem}.md"))
                && dir_tree.contains_key("albums/Holiday.md")
        );
        assert_eq!(dir_tree, zip_tree);
        Ok(())
    }

    /// The write path is generic over `WritableFileSystem`, and this is the seam
    /// real S3 output reuses.
    #[test]
    fn sync_writes_through_writable_trait_to_fake_s3() -> anyhow::Result<()> {
        use crate::media::media_file_derived_from_media_info;
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
        let mut inspected = inspect_media_files(input.clone(), media_si, prog.clone());
        for media in inspected.by_ref() {
            deduper.add(media);
        }
        drop(prog);

        let out = FakeS3FileSystem::new();
        for media in deduper.sorted_media() {
            let derived = media_file_derived_from_media_info(media, tz())?;
            let final_path = write_media(media, &derived, false, input.as_ref(), &out)?;
            sync_markdown(false, media, &final_path, &NoteLinks::default(), &out, tz())?;
        }
        let stem = FIXTURE_JPG_STEM;
        assert!(out.exists(&format!("{stem}.jpg")));
        assert!(out.exists(&format!("{stem}.md")));
        // The value a metadata-only HeadObject would compare against, which is
        // what lets a re-run skip the upload without reading the body back.
        assert_eq!(
            out.recorded_checksum(&format!("{stem}.jpg")).as_deref(),
            Some("6bfdabd4fc33d112283c147acccc574e770bbe6fbdbc3d4da968ba7b606ecc2f")
        );
        Ok(())
    }
}
