use crate::album::{Album, build_album_md, parse_album, split_album_notes};
use crate::dedup::{DeDuplicationResult, Deduplicator};
use crate::file_type::QuickFileType;
use crate::fs::{FileSystem, OsFileSystem, ZipFileSystem};
use crate::inspect::inspect_media_files;
use crate::markdown::sync_markdown;
use crate::media::{MediaFileDerivedInfo, MediaFileInfo, media_file_derived_from_media_info};
use crate::progress::Progress;
use crate::util::{ScanInfo, scan_fs};
use anyhow::anyhow;
use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::{debug, info, warn};

pub(crate) fn main(
    dry_run: bool,
    input: &String,
    output_directory: &Option<String>,
    skip_markdown: bool,
    skip_media: bool,
    skip_albums: bool,
) -> anyhow::Result<()> {
    let path = Path::new(input);
    if !path.exists() {
        return Err(anyhow!("Input path does not exist: {}", input));
    }
    let container: Arc<dyn FileSystem> = if path.is_dir() {
        info!("Input directory: {input}");
        Arc::new(OsFileSystem::new(input))
    } else {
        info!("Input zip: {input}");
        let tz = chrono::Local::now().offset().to_owned();
        Arc::new(ZipFileSystem::new(input, tz)?)
    };

    let files = scan_fs(container.as_ref());
    info!("Found {} files in input", files.len());

    let mut output_container_o: Option<OsFileSystem> = None;
    if let Some(output) = output_directory {
        info!("Output directory: {output}");
        let output_container = OsFileSystem::new(output);
        if !output_container.root_exists() {
            warn!("Output directory does not exist {output}");
        }
        output_container_o = Some(output_container);
    }
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
        let (media_iter, unprocessed) =
            inspect_media_files(container.clone(), media_si_files, prog.clone());
        for media in media_iter {
            deduper.add(media);
        }
        drop(prog);
        let unprocessed = unprocessed.load(Ordering::Relaxed);
        if unprocessed > 0 {
            warn!("{unprocessed} files could not be processed");
        }

        if let Some(ref mut output_container) = output_container_o {
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
                                &derived,
                                &album_names,
                                output_container,
                            );
                            if let Err(e) = sync_md_r {
                                warn!(
                                    "Error writing markdown file: {:?}, error: {}",
                                    derived.desired_media_path, e
                                );
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

    if !skip_albums && let Some(ref output_container) = output_container_o {
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
            if output_container.exists(output_path) {
                debug!(
                    "  Album markdown file already exists, rewriting (notes preserved) at {output_path:?}"
                );
            }
            let bytes = md.as_bytes().to_vec();
            output_container.write(dry_run, output_path, Cursor::new(bytes));
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
fn read_album_notes(output_container: &OsFileSystem, path: &str) -> String {
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
    output_container: &OsFileSystem,
) -> anyhow::Result<String> {
    let desired_output_path_with_ext =
        match Deduplicator::resolve_output_path(media_file, derived, output_container)? {
            DeDuplicationResult::SkipWrite(path) => return Ok(path),
            DeDuplicationResult::WritePath(path) => path,
        };
    info!("Output {:?}", desired_output_path_with_ext);
    let reader = input_container.open(&media_file.original_file_this_run)?;
    output_container.write(dry_run, &desired_output_path_with_ext.clone(), reader);
    output_container.set_modified(
        dry_run,
        &desired_output_path_with_ext.clone(),
        &media_file.modified,
    );
    Ok(desired_output_path_with_ext)
}
