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
