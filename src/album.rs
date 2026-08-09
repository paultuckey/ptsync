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

        // iCloud lists members by bare filename and keeps the photos in a separate
        // directory from the album CSV, so names resolve against the whole scan
        // rather than the CSV's own directory.
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
    // Takeout drops a `metadata.json` into every `Photos from YYYY` folder. Those
    // mirror the year-based structure the archive already produces, so treating
    // them as albums would make one giant album per year.
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
            // A blank title falls back to the directory name below.
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
    // Every media file in the directory belongs to the album.
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

/// Separates the generated portion of an album file from the user's own notes.
/// Everything after it is preserved verbatim across runs.
pub(crate) fn album_notes_marker() -> String {
    format!("<!-- {}:notes -->", crate::COMMAND_NAME)
}

/// The user-authored notes from an existing album file: everything after the
/// [`album_notes_marker`], or empty when the marker is absent.
pub(crate) fn split_album_notes(existing: &str) -> String {
    let marker = album_notes_marker();
    let Some(idx) = existing.find(&marker) else {
        return String::new();
    };
    let after = &existing[idx + marker.len()..];
    // The newline ending the marker line is not part of the notes.
    after
        .strip_prefix("\r\n")
        .or_else(|| after.strip_prefix("\n"))
        .unwrap_or(after)
        .to_string()
}

/// The Markdown, and the number of photos actually rendered into it — callers use
/// the count to skip albums that resolved to no usable media.
///
/// The photo list is regenerated every run; `existing_notes` is appended back
/// unchanged so albums can be annotated like any other note.
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
            // No media index to resolve against, so the original path stands.
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

    fn scanned(paths: &[&str]) -> Vec<ScanInfo> {
        paths
            .iter()
            .map(|p| ScanInfo::new(p.to_string(), None, None, 0))
            .collect()
    }

    #[test]
    fn test_parse_csv_album() -> anyhow::Result<()> {
        use anyhow::anyhow;
        crate::test_util::setup_log();
        let c = OsFileSystem::new("test");
        let qsf = ScanInfo::new("ic-album-sample.csv".to_string(), None, None, 0);
        // The CSV lists bare filenames, so resolution must reach into `Photos/`.
        let media = scanned(&[
            "Photos/35F8739B-30E0-4620-802C-0817AD7356F6.JPG",
            "Photos/AECA2F1F-8308-4989-8149-89D45A5867FD.jpg",
            "Photos/7AB0F3A2-9235-44D4-8AC9-C9B758CF15C0.jpg",
            "Photos/6F00C466-8F35-499D-9346-554E3BC2F931.jpg",
            "Photos/399E997B-A322-449A-80B5-F2F5AE98DAD5.JPG",
        ]);
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

        // No CSV member is present in the scan, so nothing resolves.
        assert!(parse_album(&c, &qsf, &[]).is_none());
        Ok(())
    }

    #[test]
    fn test_parse_json_album() -> anyhow::Result<()> {
        use anyhow::anyhow;
        crate::test_util::setup_log();
        let c = OsFileSystem::new("test/takeout1");
        let album_at = |dir: &str, media: &[&str]| {
            let qsf = ScanInfo::new(format!("{dir}/metadata.json"), None, None, 0);
            parse_album(&c, &qsf, &scanned(media))
        };

        // Only files under the album's own directory are members.
        let a = album_at(
            "Google Photos/album1",
            &["Google Photos/album1/test1.jpg", "different/test2.jpg"],
        )
        .ok_or_else(|| anyhow!("Failed to parse album"))?;
        assert_eq!(a.title, "Some album title".to_string());
        assert_eq!(a.files, vec!["Google Photos/album1/test1.jpg".to_string()]);

        // An empty title falls back to the directory name.
        let a = album_at(
            "Google Photos/empty-title-album",
            &["Google Photos/empty-title-album/test1.jpg"],
        )
        .ok_or_else(|| anyhow!("Failed to parse album"))?;
        assert_eq!(a.title, "empty-title-album".to_string());

        // A year folder is Takeout's own bucketing, not an album a user made.
        assert!(
            album_at(
                "Google Photos/Photos from 2012",
                &["Google Photos/Photos from 2012/IMG_1234.jpg"],
            )
            .is_none()
        );
        Ok(())
    }

    /// Album files are whatever the export wrote, so the parsers must return
    /// rather than panic, and never name an output path outside the archive.
    #[test]
    fn test_parse_album_never_panics_on_malformed() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let dir = tempfile::tempdir()?;
        // A normal (non year-folder) directory so parse_json_album does not
        // early-out.
        std::fs::create_dir_all(dir.path().join("SomeAlbum"))?;
        let fs = OsFileSystem::new(&dir.path().to_string_lossy());

        let csvs: Vec<(&str, &str)> = vec![
            ("empty.csv", ""),
            ("garbage.csv", "\u{0}\u{1}binary\u{7f}payload"),
            ("wrong_header.csv", "NotImages\nfoo.jpg\n"),
            ("images_no_rows.csv", "Images\n"),
            // Unbalanced quotes: the csv reader must not choke fatally.
            ("unclosed_quote.csv", "Images\n\"unterminated,foo.jpg\n"),
            // Ragged rows with wildly varying column counts.
            ("ragged.csv", "Images\na.jpg,b,c,d,e\n\n,,,\nx.jpg\n"),
            // Unicode and embedded newlines inside a quoted field.
            (
                "unicode.csv",
                "Images\n\"Ñoño 📸\nsecond line\"\ncafé.jpg\n",
            ),
            // Members that try to traverse out of the tree.
            ("traversal.csv", "Images\n../../../etc/passwd\n..\\evil\n"),
        ];
        let jsons: Vec<&str> = vec![
            "",
            "\u{0}not json",
            "{\"title\": \"unterminated",
            "[1,2,3]",
            "42",
            "null",
            r#"{"title": 12345}"#,     // title is a number
            r#"{"title": null}"#,      // title is null
            r#"{"title": {"a":"b"}}"#, // title is an object
            r#"{"title": "   "}"#,     // whitespace-only, falls back to dir name
            r#"{"notitle": "x"}"#,     // missing title key
            r#"{"title": "Ñoño 📸 café"}"#,
        ];

        let json_rel = "SomeAlbum/metadata.json";
        let cases = csvs
            .into_iter()
            .chain(jsons.into_iter().map(|body| (json_rel, body)));
        for (name, body) in cases {
            std::fs::write(dir.path().join(name), body)?;
            let si = ScanInfo::new(name.to_string(), None, None, 0);
            // No scanned media, so nothing resolves; any album that does come
            // back must still have a safe output path.
            if let Some(album) = parse_album(&fs, &si, &[]) {
                assert!(
                    !crate::test_util::escapes_output(&album.desired_album_md_path),
                    "album path escaped output: {}",
                    album.desired_album_md_path
                );
            }
        }
        Ok(())
    }

    fn test_album(files: &[&str]) -> Album {
        Album {
            desired_album_md_path: "albums/test.md".to_string(),
            title: "Test Album".to_string(),
            files: files.iter().map(|f| f.to_string()).collect(),
        }
    }

    /// Without a media index the member paths are used as they are; with one,
    /// each member is rewritten to where the file was actually written.
    #[test]
    fn test_build_album_md_links_members() {
        let (md, rendered) = build_album_md(
            &test_album(&["file1.jpg", "file2.jpg"]),
            None,
            "../media/",
            None,
            "",
        );
        assert_eq!(rendered, 2);
        assert!(md.contains("# Test Album"));
        assert!(md.contains("![Photo](../media/file1.jpg)"));
        assert!(md.contains("![Photo](../media/file2.jpg)"));

        let mut media_info = MediaFileInfo::new_for_test();
        media_info.original_path = vec!["file1.jpg".to_string()];
        media_info.hash_info.long_checksum = "longhash1".to_string();
        let mut all_media = HashMap::new();
        all_media.insert("key1".to_string(), media_info);
        let mut final_path_by_checksum = HashMap::new();
        final_path_by_checksum.insert("longhash1".to_string(), "2023/01/file1.jpg".to_string());

        let (md, rendered) = build_album_md(
            &test_album(&["file1.jpg"]),
            Some(&all_media),
            "../media/",
            Some(&final_path_by_checksum),
            "",
        );
        assert_eq!(rendered, 1);
        assert!(md.contains("![Photo](../media/2023/01/file1.jpg)"));

        // Re-rendering the same album is byte-identical, which is what lets the
        // write be skipped on a re-run.
        let (md2, _) = build_album_md(
            &test_album(&["file1.jpg"]),
            Some(&all_media),
            "../media/",
            Some(&final_path_by_checksum),
            "",
        );
        assert_eq!(md, md2);
    }

    /// A member left out rather than rendered as a broken link, whichever hop
    /// failed: never inspected, or inspected but never written.
    #[test]
    fn test_build_album_md_unresolvable_member_is_skipped() {
        let mut media_info = MediaFileInfo::new_for_test();
        media_info.original_path = vec!["file1.jpg".to_string()];
        media_info.hash_info.long_checksum = "longhash1".to_string();
        let mut inspected = HashMap::new();
        inspected.insert("key1".to_string(), media_info);

        for all_media in [HashMap::new(), inspected] {
            let (md, rendered) = build_album_md(
                &test_album(&["file1.jpg"]),
                Some(&all_media),
                "../media/",
                Some(&HashMap::new()),
                "",
            );
            assert_eq!(rendered, 0);
            assert!(md.contains("# Test Album"));
            assert!(!md.contains("![Photo]"));
        }
    }

    /// Everything below the marker is the user's, so it round-trips verbatim
    /// however odd or large it is.
    #[test]
    fn test_split_album_notes() {
        crate::test_util::setup_log();
        let album = test_album(&["file1.jpg"]);
        let (first, _) = build_album_md(&album, None, "../media/", None, "");
        assert!(first.contains(&album_notes_marker()));

        let edited = format!("{first}## My notes\n\nGreat trip!\n");
        let notes = split_album_notes(&edited);
        assert_eq!(notes, "## My notes\n\nGreat trip!\n");
        let (second, _) = build_album_md(&album, None, "../media/", None, &notes);
        assert!(second.contains("## My notes\n\nGreat trip!\n"));
        assert_eq!(split_album_notes(&second), notes);

        // No marker at all: no notes, rather than a panic.
        assert_eq!(split_album_notes(""), "");
        assert_eq!(split_album_notes("# heading only\n"), "");

        // A very large body, and binary-ish unicode, both survive unchanged.
        let marker = album_notes_marker();
        for body in ["x".repeat(200_000), "Ñoño\u{0}\u{7f}📸".to_string()] {
            assert_eq!(
                split_album_notes(&format!("# Album\n\n{marker}\n{body}")),
                body
            );
        }
    }
}
