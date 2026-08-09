mod album;
mod exif;
mod file_type;
mod frontmatter;
mod paths;
mod supplemental;
mod zips;

use anyhow::Result;
use std::io::Write;
use std::path::{Component, Path};
use zip::CompressionMethod;
use zip::write::FileOptions;

/// The machine-checkable form of "never write outside `--output`": true for an
/// absolute or rooted path, or one that climbs above the root with `..`.
fn escapes_output(rel: &str) -> bool {
    let p = Path::new(rel);
    let mut depth: i32 = 0;
    for comp in p.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => return true,
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return true;
                }
            }
            Component::CurDir => {}
            Component::Normal(_) => depth += 1,
        }
    }
    false
}

/// The real fixture JPEG, used as a base for "valid header, corrupt tail" cases.
fn real_jpeg() -> Result<Vec<u8>> {
    Ok(std::fs::read("test/Canon_40D.jpg")?)
}

/// Signature-valid PNG (8-byte magic plus an IHDR chunk) — enough for content
/// sniffing, deliberately not a decodable image.
fn fake_png() -> Vec<u8> {
    let mut v = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    v.extend_from_slice(&[0, 0, 0, 0x0d]); // IHDR length
    v.extend_from_slice(b"IHDR");
    v.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
    v.extend_from_slice(&[0x1f, 0x15, 0xc4, 0x89]); // (bogus) CRC
    v
}

/// `../` traversal, unicode, a name well over 255 chars, Windows-reserved device
/// names, and embedded control characters.
fn hostile_names() -> Vec<String> {
    vec![
        "../../../../etc/passwd.jpg".to_string(),
        "..\\..\\windows\\system32\\config.jpg".to_string(),
        "café/📸/Ñoño.JPG".to_string(),
        format!("{}.jpg", "a".repeat(300)),
        "CON".to_string(),
        "CON.jpg".to_string(),
        "PRN.jpeg".to_string(),
        "AUX.png".to_string(),
        "NUL.mp4".to_string(),
        "COM1.csv".to_string(),
        "LPT1.metadata.json".to_string(),
        "with\ttab\nand\rnewline.jpg".to_string(),
        "trailing.dot.".to_string(),
        ".".to_string(),
        "..".to_string(),
        "".to_string(),
    ]
}

/// Entry names are used verbatim so traversal names can be tested. Names the zip
/// writer refuses are skipped: the point is the read side.
fn zip_with_raw_names(entries: &[(&str, &[u8])]) -> Result<tempfile::NamedTempFile> {
    let mut temp = tempfile::Builder::new().suffix(".zip").tempfile()?;
    {
        let mut w = zip::ZipWriter::new(&mut temp);
        let options = FileOptions::<()>::default().compression_method(CompressionMethod::Stored);
        for (name, content) in entries {
            if w.start_file(*name, options).is_ok() {
                w.write_all(content)?;
            }
        }
        w.finish()?;
    }
    Ok(temp)
}
