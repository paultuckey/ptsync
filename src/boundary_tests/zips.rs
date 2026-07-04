//! Zip truncated/garbage archives, a zip-bomb-style
//! highly compressible entry, and `../` traversal entry names.

use super::{escapes_output, zip_with_raw_names};
use crate::fs::{FileSystem, ZipFileSystem};
use crate::test_util::setup_log;
use anyhow::Result;
use std::io::{Read, Write};
use zip::CompressionMethod;
use zip::write::FileOptions;

#[test]
fn truncated_or_garbage_zip_errors_not_panics() -> Result<()> {
    setup_log();
    let dir = tempfile::tempdir()?;

    // Empty file, random bytes, and the first bytes of a real zip (central
    // directory missing) must all be rejected with an error, never a panic.
    let empty = dir.path().join("empty.zip");
    std::fs::write(&empty, b"")?;
    assert!(ZipFileSystem::new(&empty.to_string_lossy()).is_err());

    let garbage = dir.path().join("garbage.zip");
    std::fs::write(
        &garbage,
        b"PK\x03\x04 then total nonsense that is not a zip",
    )?;
    assert!(ZipFileSystem::new(&garbage.to_string_lossy()).is_err());

    let full = std::fs::read("test/Canon_40D.jpg.zip")?;
    let truncated = dir.path().join("truncated.zip");
    std::fs::write(&truncated, &full[..full.len() / 2])?;
    assert!(ZipFileSystem::new(&truncated.to_string_lossy()).is_err());

    Ok(())
}

#[test]
fn zip_entry_decompresses_without_panic() -> Result<()> {
    setup_log();
    // A highly compressible entry: a few MB of zeros stored tiny on disk. The
    // reader streams it to a temp file (the test-build threshold is 100 bytes),
    // so this exercises the decompress-to-disk path and proves it is bounded by
    // the entry's declared size rather than exploding. NOTE: there is no
    // explicit expansion cap, so this fixture is deliberately kept modest.
    let size = 4 * 1024 * 1024;
    let mut temp = tempfile::Builder::new().suffix(".zip").tempfile()?;
    {
        let mut w = zip::ZipWriter::new(&mut temp);
        let options = FileOptions::<()>::default().compression_method(CompressionMethod::Deflated);
        w.start_file("zeros.bin", options)?;
        w.write_all(&vec![0u8; size])?;
        w.finish()?;
    }
    let fs = ZipFileSystem::new(&temp.path().to_string_lossy())?;
    let mut reader = fs.open("zeros.bin")?;
    let mut content = Vec::new();
    reader.read_to_end(&mut content)?;
    assert_eq!(content.len(), size, "decompressed size must match declared");
    assert!(content.iter().all(|b| *b == 0));
    Ok(())
}

#[test]
fn zip_entries_with_parent_dir_names_are_dropped() -> Result<()> {
    setup_log();
    // Craft a zip carrying traversal entry names. On read, `enclosed_name`
    // rejects anything that would escape, so those entries never appear in the
    // walk - the tool can never be tricked into opening or writing them outside
    // the output tree.
    let temp = zip_with_raw_names(&[
        ("good/photo.jpg", b"ok"),
        ("../evil.jpg", b"nope"),
        ("a/../../evil2.jpg", b"nope"),
        ("nested/../../../evil3.jpg", b"nope"),
    ])?;
    let fs = ZipFileSystem::new(&temp.path().to_string_lossy())?;
    let names = fs.walk();
    for name in &names {
        assert!(
            !escapes_output(name),
            "zip walk surfaced an escaping entry name: {name}"
        );
    }
    // Every surfaced name must also open cleanly.
    for name in &names {
        let mut r = fs.open(name)?;
        let mut buf = Vec::new();
        r.read_to_end(&mut buf)?;
    }
    Ok(())
}
