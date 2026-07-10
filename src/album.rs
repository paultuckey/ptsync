use crate::classify::{KnownDir, classify_dir};
use crate::file_type::{AccurateFileType, QuickFileType};
use crate::fs::FileSystem;
use crate::media::MediaFileInfo;
use crate::util::{ScanInfo, dir_part, name_part};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use tracing::{debug, warn};

pub(crate) fn parse_album(
    container: &dyn FileSystem,
    si: &ScanInfo,
    si_files: &[ScanInfo],
) -> Option<Album> {
    match si.quick_file_type {
        QuickFileType::AlbumCsv => parse_csv_album(container, si, si_files),
        QuickFileType::AlbumJson => parse_json_album(container, si, si_files),
        _ => None,
    }
}

fn parse_csv_album(
    container: &dyn FileSystem,
    si: &ScanInfo,
    all_scanned_files: &[ScanInfo],
) -> Option<Album> {
    debug!("Parse CSV album: {:?}", &si.file_path);
    let reader_r = container.open(&si.file_path);
    let Ok(reader) = reader_r else {
        warn!("No bytes for album: {:?}", &si.file_path);
        return None;
    };
    let name = &si.file_path;
    let mut rdr = csv::Reader::from_reader(reader);
    let Ok(s) = rdr.headers() else {
        debug!("  No headers");
        return None;
    };
    if s.is_empty() {
        debug!("  Headers empty");
        return None;
    }
    let Some(col0) = s.get(0) else {
        debug!("  No first header");
        return None;
    };
    if col0.trim().to_lowercase() != "Images".to_lowercase() {
        debug!("  Not an iCloud album (column 0 should be 'Images', was {col0})");
        return None;
    }
    let mut files: Vec<String> = vec![];

    for result in rdr.records() {
        let Ok(record) = result else {
            debug!("Error reading record");
            continue;
        };
        debug!("{record:?}");
        if record.is_empty() {
            continue;
        }
        let Some(file_name) = record.get(0) else {
            continue;
        };

        // iCloud lists album members by bare filename, and the photos live in a
        // separate directory (e.g. `Photos/`) from the album CSV (`Albums/`), so
        // resolve each name against the scanned media rather than assuming it
        // sits beside the CSV.
        let resolved = all_scanned_files.iter().find(|f| {
            f.quick_file_type == QuickFileType::Media
                && name_part(&f.file_path).eq_ignore_ascii_case(file_name)
        });
        match resolved {
            Some(f) => files.push(f.file_path.clone()),
            None => warn!("Album member not found in scan, skipping: {file_name}"),
        }
    }
    if files.is_empty() {
        debug!("Not an album: {name:?}");
        return None;
    }
    // find index of last dot and get all chars before that
    let name_without_ext = Path::new(name)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| name.clone());

    if name_without_ext.is_empty() {
        debug!("Album file has no name: {name:?}");
        return None;
    }
    debug!(
        "Found album: {:?} with {:?} entries at {:?}",
        name_without_ext,
        files.len(),
        name
    );
    Some(Album {
        desired_album_md_path: format!("albums/{name_without_ext}.md"),
        title: name_without_ext.clone(),
        files,
    })
}

fn parse_json_album(
    container: &dyn FileSystem,
    si: &ScanInfo,
    all_scanned_files: &[ScanInfo],
) -> Option<Album> {
    let directory_path_str = dir_part(&si.file_path);
    // Google Takeout drops a `metadata.json` into every `Photos from YYYY`
    // folder, but those are not real albums - they mirror the year-based
    // directory structure we already produce. Treating them as albums would
    // make one giant album per year, so skip them.
    if let Some(KnownDir::GpPhotosFromYear(_)) = classify_dir(&directory_path_str) {
        debug!(
            "Skipping year-folder metadata.json, not a real album: {:?}",
            &si.file_path
        );
        return None;
    }
    let reader_r = container.open(&si.file_path);
    let Ok(reader) = reader_r else {
        warn!("No bytes for album: {:?}", &si.file_path);
        return None;
    };
    let j: Result<Value, _> = serde_json::from_reader(reader);
    let title;
    if let Ok(j) = j {
        let title_res = j.get("title");
        if let Some(title_value) = title_res {
            debug!("  Found album title: {title_value}");
            // An empty/whitespace title falls back to the directory name below.
            let t = title_value.as_str().unwrap_or("").trim().to_string();
            title = if t.is_empty() { None } else { Some(t) };
        } else {
            debug!("Title not found in JSON, skipping {:?}", &si.file_path);
            return None;
        }
    } else {
        warn!("Unable to decode album JSON: {:?}", &si.file_path);
        return None;
    }
    // all files in this directory are in the album
    let same_dir_files = all_scanned_files
        .iter()
        .filter(|si| {
            let q_dir_part = &dir_part(&si.file_path);
            si.quick_file_type == QuickFileType::Media && directory_path_str.eq(q_dir_part)
        })
        .map(|si| si.file_path.clone())
        .collect::<Vec<String>>();

    let directory_path_name_str = name_part(&directory_path_str);
    let desired_album_md_path = format!("albums/{directory_path_name_str}.md");
    // todo: how check for existing album?
    Some(Album {
        desired_album_md_path,
        title: title.unwrap_or(directory_path_name_str),
        files: same_dir_files,
    })
}

pub(crate) struct Album {
    pub(crate) desired_album_md_path: String,
    pub(crate) title: String,
    pub(crate) files: Vec<String>,
}

/// Marker separating the generated portion of an album file from the user's own
/// notes. Everything after it is preserved verbatim across runs. The command name
/// is sourced from [`crate::COMMAND_NAME`] so it stays consistent tool-wide.
pub(crate) fn album_notes_marker() -> String {
    format!("<!-- {}:notes -->", crate::COMMAND_NAME)
}

/// Return the user-authored notes from an existing album file: everything after
/// the [`album_notes_marker`]. Returns an empty string if the marker is absent
/// (e.g. a brand new file or one written by an older version).
pub(crate) fn split_album_notes(existing: &str) -> String {
    let marker = album_notes_marker();
    let Some(idx) = existing.find(&marker) else {
        return String::new();
    };
    let after = &existing[idx + marker.len()..];
    // Drop the single newline that immediately follows the marker line; the
    // user's notes begin after it.
    after
        .strip_prefix("\r\n")
        .or_else(|| after.strip_prefix("\n"))
        .unwrap_or(after)
        .to_string()
}

/// Get the Markdown and the number of photos actually rendered into it. Callers use the count to
/// skip writing albums that resolved to no usable media.
///
/// The photo list is regenerated on every run, but `existing_notes` (the text a
/// user wrote after the [`album_notes_marker`]) is appended back unchanged so
/// albums can be annotated like any other note.
pub(crate) fn build_album_md(
    album: &Album,
    all_media_o: Option<&HashMap<String, MediaFileInfo>>,
    media_relative_path: &str,
    final_path_by_checksum: Option<&HashMap<String, String>>,
    existing_notes: &str,
) -> (String, usize) {
    let mut md = String::new();
    let mut resolved_count = 0;
    let generated_note = format!(
        "[ The photo list below is generated by {} and rebuilt on every run. Write notes beneath \
        the marker near the end of the file; that section is preserved. ]: #\n\n",
        crate::COMMAND_NAME
    );
    md.push_str(&generated_note);
    md.push_str(&format!("# {}", &album.title));
    md.push_str("\n\n");
    for f in &album.files {
        let target_path_o: Option<String>;
        if let Some(all_media) = all_media_o {
            target_path_o = all_media
                .values()
                .find(|m| {
                    m.accurate_file_type != AccurateFileType::Unsupported
                        && m.quick_file_type == QuickFileType::Media
                        && m.original_path.iter().any(|p| p.eq(f))
                })
                .and_then(|m| {
                    let long_checksum = &m.hash_info.long_checksum;
                    final_path_by_checksum.and_then(|fp_map| fp_map.get(long_checksum).cloned())
                });
            if target_path_o.is_none() {
                warn!("No media file desired path found for: {f}");
                continue;
            }
        } else {
            // intentionally use the original path
            target_path_o = Some(f.clone());
        }
        if let Some(target_path) = target_path_o {
            let alt_text = "Photo";
            let path = format!("{media_relative_path}{target_path}");
            md.push_str(&format!("\n![{alt_text}]({path})"));
            resolved_count += 1;
        } else {
            warn!("Target path empty: {f}");
        }
    }
    md.push_str("\n\n");
    md.push_str(&album_notes_marker());
    md.push('\n');
    md.push_str(existing_notes);
    (md, resolved_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::OsFileSystem;

    #[test]
    fn test_ic_sample() -> anyhow::Result<()> {
        use anyhow::anyhow;
        crate::test_util::setup_log();
        let c = OsFileSystem::new("test");
        let qsf = ScanInfo::new("ic-album-sample.csv".to_string(), None, None, 0);
        // The CSV lists members by bare filename; the photos live in a separate
        // `Photos/` directory, so resolution must match across directories.
        let media: Vec<ScanInfo> = [
            "Photos/35F8739B-30E0-4620-802C-0817AD7356F6.JPG",
            "Photos/AECA2F1F-8308-4989-8149-89D45A5867FD.jpg",
            "Photos/7AB0F3A2-9235-44D4-8AC9-C9B758CF15C0.jpg",
            "Photos/6F00C466-8F35-499D-9346-554E3BC2F931.jpg",
            "Photos/399E997B-A322-449A-80B5-F2F5AE98DAD5.JPG",
        ]
        .iter()
        .map(|p| ScanInfo::new(p.to_string(), None, None, 0))
        .collect();
        let a = parse_album(&c, &qsf, &media).ok_or_else(|| anyhow!("Failed to parse album"))?;
        assert_eq!(a.title, "ic-album-sample".to_string());
        assert_eq!(
            a.desired_album_md_path,
            "albums/ic-album-sample.md".to_string()
        );
        assert_eq!(a.files.len(), 5);
        assert_eq!(
            a.files.first().ok_or_else(|| anyhow!("Album empty"))?,
            "Photos/35F8739B-30E0-4620-802C-0817AD7356F6.JPG"
        );
        Ok(())
    }

    #[test]
    fn test_ic_sample_unresolved_members_skipped() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        // None of the CSV members are present in the scan, so the album has no
        // resolvable files and is not treated as an album.
        let c = OsFileSystem::new("test");
        let qsf = ScanInfo::new("ic-album-sample.csv".to_string(), None, None, 0);
        assert!(parse_album(&c, &qsf, &[]).is_none());
        Ok(())
    }

    #[test]
    fn test_g_year_folder_not_album() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        // A `metadata.json` inside a `Photos from YYYY` folder is the year's own
        // marker, not a real album, so it must not be parsed as one.
        let c = OsFileSystem::new("test/takeout1");
        let qsf = ScanInfo::new(
            "Google Photos/Photos from 2012/metadata.json".to_string(),
            None,
            None,
            0,
        );
        let photo = ScanInfo::new(
            "Google Photos/Photos from 2012/IMG_1234.jpg".to_string(),
            None,
            None,
            0,
        );
        assert!(parse_album(&c, &qsf, &[photo]).is_none());
        Ok(())
    }

    #[test]
    fn test_g_sample() -> anyhow::Result<()> {
        use anyhow::anyhow;
        crate::test_util::setup_log();
        let c = OsFileSystem::new("test/takeout1");
        let qsf = ScanInfo::new(
            "Google Photos/album1/metadata.json".to_string(),
            None,
            None,
            0,
        );
        let si1 = ScanInfo::new("Google Photos/album1/test1.jpg".to_string(), None, None, 0);
        let si2 = ScanInfo::new("different/test2.jpg".to_string(), None, None, 0);
        let a =
            parse_album(&c, &qsf, &[si1, si2]).ok_or_else(|| anyhow!("Failed to parse album"))?;
        assert_eq!(a.title, "Some album title".to_string());
        assert_eq!(a.files.len(), 1);
        assert_eq!(
            a.files
                .first()
                .ok_or_else(|| anyhow!("Album empty"))?
                .to_string(),
            "Google Photos/album1/test1.jpg".to_string()
        );
        Ok(())
    }

    #[test]
    fn test_g_empty_title_falls_back_to_dir_name() -> anyhow::Result<()> {
        use anyhow::anyhow;
        crate::test_util::setup_log();
        // The album's metadata.json has an empty title, so the album should
        // be named after its containing directory rather than left blank.
        let c = OsFileSystem::new("test/takeout1");
        let qsf = ScanInfo::new(
            "Google Photos/empty-title-album/metadata.json".to_string(),
            None,
            None,
            0,
        );
        let photo = ScanInfo::new(
            "Google Photos/empty-title-album/test1.jpg".to_string(),
            None,
            None,
            0,
        );
        let a = parse_album(&c, &qsf, &[photo]).ok_or_else(|| anyhow!("Failed to parse album"))?;
        assert_eq!(a.title, "empty-title-album".to_string());
        Ok(())
    }

    #[test]
    fn test_build_album_md_no_media_info() {
        let album = Album {
            desired_album_md_path: "albums/test.md".to_string(),
            title: "Test Album".to_string(),
            files: vec!["file1.jpg".to_string(), "file2.jpg".to_string()],
        };
        let (md, rendered) = build_album_md(&album, None, "../media/", None, "");
        assert_eq!(rendered, 2);
        assert!(md.contains("# Test Album"));
        assert!(md.contains("![Photo](../media/file1.jpg)"));
        assert!(md.contains("![Photo](../media/file2.jpg)"));
    }

    #[test]
    fn test_build_album_md_with_mappings() {
        let album = Album {
            desired_album_md_path: "albums/test.md".to_string(),
            title: "Test Album".to_string(),
            files: vec!["file1.jpg".to_string()],
        };
        let mut media_info = MediaFileInfo::new_for_test();
        media_info.original_path = vec!["file1.jpg".to_string()];
        media_info.hash_info.long_checksum = "longhash1".to_string();

        let mut all_media = HashMap::new();
        all_media.insert("key1".to_string(), media_info);

        let mut final_path_by_checksum = HashMap::new();
        final_path_by_checksum.insert("longhash1".to_string(), "2023/01/file1.jpg".to_string());

        let (md, rendered) = build_album_md(
            &album,
            Some(&all_media),
            "../media/",
            Some(&final_path_by_checksum),
            "",
        );
        assert_eq!(rendered, 1);
        assert!(md.contains("# Test Album"));
        assert!(md.contains("![Photo](../media/2023/01/file1.jpg)"));
    }

    #[test]
    fn test_build_album_md_missing_mapping() {
        let album = Album {
            desired_album_md_path: "albums/test.md".to_string(),
            title: "Test Album".to_string(),
            files: vec!["file1.jpg".to_string()],
        };
        let all_media = HashMap::new(); // Empty
        let final_path_by_checksum = HashMap::new();

        let (md, rendered) = build_album_md(
            &album,
            Some(&all_media),
            "../media/",
            Some(&final_path_by_checksum),
            "",
        );
        assert_eq!(rendered, 0);
        assert!(md.contains("# Test Album"));
        assert!(!md.contains("![Photo]")); // Should be skipped
    }

    #[test]
    fn test_build_album_md_missing_final_path() {
        let album = Album {
            desired_album_md_path: "albums/test.md".to_string(),
            title: "Test Album".to_string(),
            files: vec!["file1.jpg".to_string()],
        };
        let mut media_info = MediaFileInfo::new_for_test();
        media_info.original_path = vec!["file1.jpg".to_string()];
        media_info.hash_info.long_checksum = "longhash1".to_string();

        let mut all_media = HashMap::new();
        all_media.insert("key1".to_string(), media_info);

        let final_path_by_checksum = HashMap::new(); // Empty, so lookup fails

        let (md, rendered) = build_album_md(
            &album,
            Some(&all_media),
            "../media/",
            Some(&final_path_by_checksum),
            "",
        );
        assert_eq!(rendered, 0);
        assert!(md.contains("# Test Album"));
        assert!(!md.contains("![Photo]")); // Should be skipped
    }

    #[test]
    fn test_build_album_md_preserves_notes() {
        let album = Album {
            desired_album_md_path: "albums/test.md".to_string(),
            title: "Test Album".to_string(),
            files: vec!["file1.jpg".to_string()],
        };
        // First render with no existing notes; the notes marker is present.
        let (first, _) = build_album_md(&album, None, "../media/", None, "");
        assert!(first.contains(&album_notes_marker()));

        // A user writes notes below the marker, then we re-render and feed the
        // extracted notes back in - they survive verbatim.
        let edited = format!("{first}## My notes\n\nGreat trip!\n");
        let notes = split_album_notes(&edited);
        assert_eq!(notes, "## My notes\n\nGreat trip!\n");
        let (second, _) = build_album_md(&album, None, "../media/", None, &notes);
        assert!(second.contains("## My notes\n\nGreat trip!\n"));

        // Re-extracting from the re-rendered file yields identical notes (round trip).
        assert_eq!(split_album_notes(&second), notes);
    }

    #[test]
    fn test_split_album_notes_no_marker() {
        assert_eq!(split_album_notes("# Just a heading\n"), "");
    }

    #[test]
    fn test_album_rerun_is_a_no_op_write() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let dir = tempfile::tempdir()?;
        let out = OsFileSystem::new(&dir.path().to_string_lossy());
        let album = Album {
            desired_album_md_path: "albums/test.md".to_string(),
            title: "Test Album".to_string(),
            files: vec!["file1.jpg".to_string()],
        };
        let (md, _) = build_album_md(&album, None, "../", None, "");
        assert!(out.write_if_changed(false, &album.desired_album_md_path, md.as_bytes()));

        // Re-run: identical content regenerated from the same inputs.
        let (md2, _) = build_album_md(&album, None, "../", None, "");
        assert_eq!(md, md2);
        assert!(!out.write_if_changed(false, &album.desired_album_md_path, md2.as_bytes()));
        Ok(())
    }
}
