//! Album CSV/JSON parser (`src/album.rs`) under malformed input, plus the notes
//! splitter that must preserve arbitrary user text verbatim.

use super::escapes_output;
use crate::album::{parse_album, split_album_notes};
use crate::file_type::QuickFileType;
use crate::fs::OsFileSystem;
use crate::test_util::setup_log;
use crate::util::ScanInfo;
use anyhow::Result;

#[test]
fn album_csv_never_panics_on_malformed() -> Result<()> {
    setup_log();
    let dir = tempfile::tempdir()?;
    let fs = OsFileSystem::new(&dir.path().to_string_lossy());

    let cases: Vec<(&str, &str)> = vec![
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

    for (name, body) in cases {
        std::fs::write(dir.path().join(name), body)?;
        let si = ScanInfo::new(name.to_string(), None, None, 0);
        assert_eq!(si.quick_file_type, QuickFileType::AlbumCsv);
        // No scanned media, so nothing resolves; the parser must return without
        // panic. Any album it does produce must have a safe output path.
        if let Some(album) = parse_album(&fs, &si, &[]) {
            assert!(
                !escapes_output(&album.desired_album_md_path),
                "album path escaped output: {}",
                album.desired_album_md_path
            );
        }
    }
    Ok(())
}

#[test]
fn album_json_never_panics_on_malformed() -> Result<()> {
    setup_log();
    let dir = tempfile::tempdir()?;
    // A normal (non year-folder) directory so parse_json_album does not early-out.
    let album_dir = dir.path().join("SomeAlbum");
    std::fs::create_dir_all(&album_dir)?;
    let fs = OsFileSystem::new(&dir.path().to_string_lossy());

    let bodies: Vec<&str> = vec![
        "",
        "\u{0}not json",
        "{\"title\": \"unterminated",
        "[1,2,3]",
        "42",
        "null",
        r#"{"title": 12345}"#,     // title is a number
        r#"{"title": null}"#,      // title is null
        r#"{"title": {"a":"b"}}"#, // title is an object
        r#"{"title": "   "}"#,     // whitespace-only title (falls back to dir name)
        r#"{"notitle": "x"}"#,     // missing title key
        r#"{"title": "Ñoño 📸 café"}"#,
    ];

    let rel = "SomeAlbum/metadata.json".to_string();
    for body in bodies {
        std::fs::write(dir.path().join(&rel), body)?;
        let si = ScanInfo::new(rel.clone(), None, None, 0);
        assert_eq!(si.quick_file_type, QuickFileType::AlbumJson);
        if let Some(album) = parse_album(&fs, &si, &[]) {
            assert!(
                !escapes_output(&album.desired_album_md_path),
                "album path escaped output: {}",
                album.desired_album_md_path
            );
        }
    }
    Ok(())
}

#[test]
fn split_album_notes_survives_adversarial_bodies() {
    setup_log();
    // No marker at all - notes are empty, not a panic.
    assert_eq!(split_album_notes(""), "");
    assert_eq!(split_album_notes("# heading only\n"), "");
    // A very large body after the marker round-trips verbatim.
    let marker = format!("<!-- {}:notes -->", crate::COMMAND_NAME);
    let big = "x".repeat(200_000);
    let doc = format!("# Album\n\n{marker}\n{big}");
    assert_eq!(split_album_notes(&doc), big);
    // Binary-ish and unicode content after the marker is preserved unchanged.
    let weird = "Ñoño\u{0}\u{7f}📸";
    let doc2 = format!("{marker}\n{weird}");
    assert_eq!(split_album_notes(&doc2), weird);
}
