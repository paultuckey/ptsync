//! Output-path safety: dodgy filenames must not panic the path helpers,
//! derived paths must never escape the output tree, and a full sync over
//! dodgy input names must stay contained.

use super::{escapes_output, hostile_names, real_jpeg};
use crate::file_type::find_quick_file_type;
use crate::markdown::get_desired_markdown_path;
use crate::metadata::taken::Taken;
use crate::output_path::get_desired_media_path;
use crate::test_util::setup_log;
use crate::util::{ScanInfo, dir_part, name_part};
use anyhow::Result;
use chrono::{NaiveDate, NaiveDateTime};
use std::path::{Path, PathBuf};

/// The media path is built from a checksum (always hex) and a [`Taken`], whose
/// only path-visible field is a `NaiveDateTime`. A datetime used to reach this
/// as a *string*, so a value crafted to look like traversal was a real concern
/// and this test fed it several; taking the parsed value instead means no such
/// string exists to pass. What is left to check is that no date chrono can hold,
/// the extremes of the calendar included, formats into anything but digits and
/// separators.
#[test]
fn derived_media_paths_never_escape_output() {
    setup_log();
    let checksum = "6bfdabd";
    let dates = [
        NaiveDate::from_ymd_opt(2008, 5, 30).and_then(|d| d.and_hms_opt(15, 56, 1)),
        // Year 1, and a year past four digits: neither may produce a component
        // that reads as a path segment of its own.
        NaiveDate::from_ymd_opt(1, 1, 1).and_then(|d| d.and_hms_opt(0, 0, 0)),
        NaiveDate::from_ymd_opt(262142, 12, 31).and_then(|d| d.and_hms_opt(23, 59, 59)),
        // Before the epoch, where the year is negative and prints with a sign.
        NaiveDate::from_ymd_opt(-4, 2, 29).and_then(|d| d.and_hms_opt(12, 0, 0)),
        NaiveDateTime::MIN.into(),
        NaiveDateTime::MAX.into(),
    ];
    for date in dates.into_iter().flatten() {
        let taken = Taken::wall(date);
        let path = get_desired_media_path(checksum, Some(&taken));
        assert!(
            !escapes_output(&path),
            "media path escaped output for {taken}: {path}"
        );
    }
    // No date at all is the `undated/` case, and the checksum is all that names
    // it.
    let path = get_desired_media_path(checksum, None);
    assert_eq!(path, "undated/6bfdabd");
    assert!(!escapes_output(&path));
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
    crate::sync_cmd::main(false, &input_s, &output_s, false, false, false, true)?;

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
