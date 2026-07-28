use crate::album::{Album, build_album_md, parse_album, split_album_notes};
use crate::dedup::{DeDuplicationResult, Deduplicator};
use crate::file_type::QuickFileType;
use crate::fs::{FileSystem, WritableFileSystem, open_input, open_output};
use crate::inspect::inspect_media_files;
use crate::markdown::sync_markdown;
use crate::media::{MediaFileDerivedInfo, MediaFileInfo, media_file_derived_from_media_info};
use crate::progress::Progress;
use crate::util::{ScanInfo, scan_fs};
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
            for media in media_to_write {
                prog.inc();
                let derived = media_file_derived_from_media_info(media)?;
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
                        final_path_by_checksum.insert(long_checksum.clone(), final_path.clone());
                        if !skip_markdown {
                            let album_names =
                                album_names_for(&album_names_by_path, &media.original_path);
                            let sync_md_r = sync_markdown(
                                dry_run,
                                media,
                                &final_path,
                                &album_names,
                                output_container,
                            );
                            if let Err(e) = sync_md_r {
                                warn!("Error writing markdown file beside {final_path:?}: {e}");
                            }
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
            drop(prog);
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

    /// Two different files captured in the same millisecond but stored in
    /// different formats both keep the bare date name (collisions are resolved
    /// per full name, extension included), so they prefer the same note name.
    /// One takes it and the other falls back to the appended form; sharing it
    /// would pool a photo's and a video's `original-paths` under a single
    /// checksum and leave one of them with no note at all.
    #[test]
    fn sync_same_instant_different_formats_get_separate_notes() -> anyhow::Result<()> {
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

        let notes: Vec<PathBuf> = files_under(&output)?
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .collect();
        assert_eq!(
            notes.len(),
            2,
            "each file must have its own note: {notes:?}"
        );

        // Which of the two wins the bare name depends on inspection order, so
        // the names are checked as a set: one preferred, one disambiguated.
        let names: BTreeSet<String> = notes
            .iter()
            .filter_map(|p| Some(p.file_name()?.to_string_lossy().to_string()))
            .collect();
        assert!(
            names.contains("2213-20000.md"),
            "one note should keep the bare date name: {names:?}"
        );
        assert!(
            names.contains("2213-20000.jpg.md") || names.contains("2213-20000.mp4.md"),
            "the other should fall back to the appended name: {names:?}"
        );

        // Each note records only its own file, so neither claims the other's
        // bytes as a duplicate of itself.
        let bodies = notes
            .iter()
            .map(read_to_string)
            .collect::<Result<Vec<String>, _>>()?;
        let jpg_md = bodies
            .iter()
            .find(|b| b.contains("- Canon_40D.jpg"))
            .ok_or_else(|| anyhow!("no note recorded the photo: {bodies:?}"))?;
        let mp4_md = bodies
            .iter()
            .find(|b| b.contains("- Hello.mp4"))
            .ok_or_else(|| anyhow!("no note recorded the video: {bodies:?}"))?;
        assert!(!jpg_md.contains("- Hello.mp4"));
        assert!(!mp4_md.contains("- Canon_40D.jpg"));
        assert!(jpg_md.contains("![](2213-20000.jpg)"));
        assert!(mp4_md.contains("![](2213-20000.mp4)"));
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
        let mut inspected = inspect_media_files(input.clone(), media_si, prog.clone(), true);
        for media in inspected.by_ref() {
            deduper.add(media);
        }
        drop(prog);

        // First pass writes media + sidecars into the fake bucket.
        let out = FakeS3FileSystem::new();
        for media in deduper.sorted_media() {
            let derived = media_file_derived_from_media_info(media)?;
            let final_path = write_media(media, &derived, false, input.as_ref(), &out)?;
            sync_markdown(false, media, &final_path, &[], &out)?;
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
            let derived = media_file_derived_from_media_info(media)?;
            let final_path = write_media(media, &derived, false, input.as_ref(), &out)?;
            sync_markdown(false, media, &final_path, &[], &out)?;
        }
        assert_eq!(out.walk().len(), before, "re-run must not add new objects");
        Ok(())
    }
}
