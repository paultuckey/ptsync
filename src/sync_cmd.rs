use crate::album::{Album, build_album_md, parse_album, split_album_notes};
use crate::dedup::{DeDuplicationResult, Deduplicator};
use crate::file_type::QuickFileType;
use crate::fs::{FileSystem, WritableFileSystem, open_input, open_output};
use crate::inspect::inspect_media_files;
use crate::markdown::sync_markdown;
use crate::media::{MediaFileDerivedInfo, MediaFileInfo, media_file_derived_from_media_info};
use crate::progress::Progress;
use crate::util::{OutputTZ, ScanInfo, scan_fs};
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
            info!("Outputting {} photo and video files", media_to_write.len());
            let prog = Progress::new(media_to_write.len() as u64);
            for media in media_to_write {
                prog.inc();
                let derived = media_file_derived_from_media_info(media, tz)?;
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
                                tz,
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
            // The photo list is regenerated every run, so writing only on a real
            // difference is what leaves a re-run's files and mtimes untouched.
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
    use crate::test_util::tz;

    /// The write path is generic over `WritableFileSystem`, and this is the seam
    /// real S3 output reuses. Because the fake reports a native checksum, the
    /// second pass also exercises the skip-via-`recorded_checksum` path.
    ///
    /// Everything else about a sync is covered end to end over the corpus in
    /// `tests/corpus.rs`; this stays here because a fake filesystem cannot be
    /// reached by running the binary.
    #[test]
    fn sync_writes_through_writable_trait_to_fake_s3() -> anyhow::Result<()> {
        use crate::fs::OsFileSystem;
        use crate::media::media_file_derived_from_media_info;
        use crate::s3_fs::FakeS3FileSystem;
        crate::test_util::setup_log();

        const TAKEOUT_BASIC: &str = "test/takeout_basic";
        /// Where the fixture jpg lands at [`crate::test_util::tz`]'s `+12:00`.
        /// Its instant is a quarter past midnight in Greenwich and a quarter past
        /// noon at +12:00, so a regression to rendering at UTC changes the name
        /// rather than quietly agreeing with itself.
        const FIXTURE_JPG_STEM: &str = "2024/05/22/1217-51000";

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
            sync_markdown(false, media, &final_path, &[], &out, tz())?;
        }
        let stem = FIXTURE_JPG_STEM;
        assert!(out.exists(&format!("{stem}.jpg")));
        assert!(out.exists(&format!("{stem}.md")));
        // The value a metadata-only HeadObject would compare against.
        assert_eq!(
            out.recorded_checksum(&format!("{stem}.jpg")).as_deref(),
            Some("6bfdabd4fc33d112283c147acccc574e770bbe6fbdbc3d4da968ba7b606ecc2f")
        );

        // The media dedups to SkipWrite via the recorded checksum, and the sidecar
        // is unchanged.
        let before = out.walk().len();
        for media in deduper.sorted_media() {
            let derived = media_file_derived_from_media_info(media, tz())?;
            let final_path = write_media(media, &derived, false, input.as_ref(), &out)?;
            sync_markdown(false, media, &final_path, &[], &out, tz())?;
        }
        assert_eq!(out.walk().len(), before, "re-run must not add new objects");
        Ok(())
    }
}
