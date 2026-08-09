#[cfg(test)]
use std::sync::Once;

#[cfg(test)]
static INIT: Once = Once::new();

#[cfg(test)]
pub(crate) fn setup_log() {
    INIT.call_once(|| {
        use tracing::Level;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;

        let filter = tracing_subscriber::filter::Targets::new()
            .with_default(Level::DEBUG)
            .with_target("nom_exif", Level::ERROR);
        let registry_layer = tracing_subscriber::fmt::layer().with_target(false);
        tracing_subscriber::registry()
            .with(registry_layer)
            .with(filter)
            .init();
    });
}

/// The zone every test states its date expectations in, so an assertion means
/// the same thing on a laptop in Auckland and on a build agent in UTC.
///
/// `+12:00` is far enough east that converting an instant crosses a day boundary
/// for most of the day, so a test asserting a full path catches a regression to
/// UTC in the *directory* rather than only in the offset spelling.
/// `tests/sync_snapshot.rs` pins its subprocess to `Pacific/Auckland`, the zone
/// this stands in for.
#[cfg(test)]
pub(crate) fn tz() -> crate::util::OutputTZ {
    const TWELVE_HOURS_EAST: i32 = 12 * 3600;
    // `east_opt` only rejects offsets beyond ±24h, but `unwrap` is denied
    // crate-wide. Every expectation written against +12:00 fails loudly if this
    // fallback is ever taken.
    use chrono::{Offset, Utc};
    let offset = chrono::FixedOffset::east_opt(TWELVE_HOURS_EAST).unwrap_or_else(|| Utc.fix());
    crate::util::OutputTZ::Fixed(offset)
}

/// The machine-checkable form of "never write outside `--output`": true for an
/// absolute or rooted path, or one that climbs above the root with `..`.
#[cfg(test)]
pub(crate) fn escapes_output(rel: &str) -> bool {
    use std::path::{Component, Path};
    let mut depth: i32 = 0;
    for comp in Path::new(rel).components() {
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

/// `../` traversal, unicode, a name well over 255 chars, Windows-reserved device
/// names, and embedded control characters.
#[cfg(test)]
pub(crate) fn hostile_names() -> Vec<String> {
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
        String::new(),
    ]
}

/// Signature-valid PNG (8-byte magic plus an IHDR chunk) — enough for content
/// sniffing, deliberately not a decodable image.
#[cfg(test)]
pub(crate) fn fake_png() -> Vec<u8> {
    let mut v = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    v.extend_from_slice(&[0, 0, 0, 0x0d]); // IHDR length
    v.extend_from_slice(b"IHDR");
    v.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
    v.extend_from_slice(&[0x1f, 0x15, 0xc4, 0x89]); // (bogus) CRC
    v
}

#[cfg(test)]
pub(crate) fn build_zip(input: &str) -> anyhow::Result<tempfile::NamedTempFile> {
    use crate::fs::FileSystem;
    use crate::fs::OsFileSystem;
    use std::io::copy;
    use zip::CompressionMethod;
    use zip::write::FileOptions;
    let dir_fs = OsFileSystem::new(input);
    let mut zip_temp = tempfile::Builder::new().suffix(".zip").tempfile()?;
    {
        let mut zip_writer = zip::ZipWriter::new(&mut zip_temp);
        let options = FileOptions::<()>::default().compression_method(CompressionMethod::Stored);
        for rel in dir_fs.walk() {
            let mut reader = dir_fs.open(&rel)?;
            zip_writer.start_file(rel.as_str(), options)?;
            copy(&mut reader, &mut zip_writer)?;
        }
        zip_writer.finish()?;
    }
    Ok(zip_temp)
}
