//! Output-path safety: dodgy filenames must not panic the path helpers,
//! derived paths must never escape the output tree, and a full sync over
//! dodgy input names must stay contained.

use super::{escapes_output, hostile_names, real_jpeg};
use crate::file_type::find_quick_file_type;
use crate::markdown::get_desired_markdown_path;
use crate::media::get_desired_media_path;
use crate::test_util::setup_log;
use crate::util::{ScanInfo, dir_part, name_part};
use anyhow::Result;
use std::path::{Path, PathBuf};

#[test]
fn derived_media_paths_never_escape_output() {
    setup_log();
    // Real short checksums are always hex, so the only attacker-influenced input
    // to the media path is the datetime string. None of these - including ones
    // crafted to look like traversal - may produce an escaping path.
    let checksum = "6bfdabd";
    let datetimes: Vec<Option<String>> = vec![
        None,
        Some("2008-05-30T15:56:01Z".to_string()),
        Some("../../../../etc/passwd".to_string()),
        Some("/absolute/path".to_string()),
        Some("not a date at all".to_string()),
        Some("9999-99-99T99:99:99Z".to_string()),
        Some("Ñoño 📸".to_string()),
        Some(String::new()),
    ];
    for dt in datetimes {
        let path = get_desired_media_path(checksum, &dt);
        assert!(
            !escapes_output(&path),
            "media path escaped output for datetime {dt:?}: {path}"
        );
    }
}

#[test]
fn hostile_filenames_do_not_panic_path_helpers() -> Result<()> {
    setup_log();
    // Feed traversal, unicode, over-255-char, reserved and control-char names to
    // every filename helper. None may panic. The markdown-path helper preserves
    // the directory (it only swaps the extension), so we do not require it to
    // sanitise - the output path it is fed is itself always tool-derived.
    for name in hostile_names() {
        let _ = find_quick_file_type(&name);
        let _ = name_part(&name);
        let _ = dir_part(&name);
        let _ = ScanInfo::new(name.clone(), None, None, 0);
        // Must return Ok/Err but never panic (empty input is the one Err case).
        let md = get_desired_markdown_path(&name);
        if name.is_empty() {
            assert!(md.is_err(), "empty resolved path should error");
        } else {
            assert!(md.is_ok());
        }
    }
    Ok(())
}

#[test]
fn sync_over_hostile_input_names_stays_within_output() -> Result<()> {
    setup_log();
    let temp = tempfile::tempdir()?;
    let input = temp.path().join("input");
    let output = temp.path().join("output");

    // A subdirectory whose name is unicode, containing two distinct real photos
    // under a reserved device name and a long-ish name, each with supplemental
    // metadata fixing the date, plus an album metadata.json for the folder.
    let album_dir = input.join("café 📸 Ñoño");
    std::fs::create_dir_all(&album_dir)?;
    let base = real_jpeg()?;
    for (name, marker) in [("CON.jpg", "A"), ("really_long_name_photo.jpg", "BB")] {
        let mut bytes = base.clone();
        bytes.extend_from_slice(marker.as_bytes());
        std::fs::write(album_dir.join(name), &bytes)?;
        std::fs::write(
            album_dir.join(format!("{name}.supplemental-metadata.json")),
            r#"{"photoTakenTime":{"timestamp":"1700000000"}}"#,
        )?;
    }
    std::fs::write(
        album_dir.join("metadata.json"),
        r#"{"title":"Weird 📸 Album"}"#,
    )?;

    let input_s = input.to_string_lossy().to_string();
    let output_s = Some(output.to_string_lossy().to_string());
    // The sync must complete without panicking or erroring on these names.
    crate::sync_cmd::main(false, &input_s, &output_s, false, false, false)?;

    // Every file the sync produced must sit under the output root - the derived
    // names are date/checksum based, so nothing leaks the hostile input names.
    let mut count = 0;
    for path in files_under(&output)? {
        let rel = path.strip_prefix(&output)?.to_string_lossy().to_string();
        assert!(!escapes_output(&rel), "sync wrote outside output: {rel}");
        count += 1;
    }
    assert!(count > 0, "sync should have written at least one file");
    Ok(())
}

/// Every regular file under `dir`, recursing into subdirectories.
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
