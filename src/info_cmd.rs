use crate::album::{build_album_md, parse_album};
use crate::file_type::QuickFileType;
use crate::fs::{FileSystem, OsFileSystem};
use crate::inspect::analyze_file;
use crate::markdown::{assemble_markdown, mfm_from_media_file_info};
use crate::util::{OutputTZ, ScanInfo, scan_fs};
use std::fmt::Write;
use tracing::{debug, warn};

pub(crate) fn main(input: &String, root_s: &str, tz: OutputTZ) -> anyhow::Result<()> {
    debug!("Inspecting: {input}");
    let root: Box<dyn FileSystem> = Box::new(OsFileSystem::new(root_s));
    let len = root.metadata(input).map(|m| m.len).unwrap_or(0);
    let si = ScanInfo::new(input.clone(), None, None, len);
    let output = match si.quick_file_type {
        QuickFileType::Unknown => {
            warn!("File type is unknown, skipping: {input}");
            return Ok(());
        }
        QuickFileType::AlbumCsv | QuickFileType::AlbumJson => album(&si, root.as_ref())?,
        QuickFileType::Media => media(&si, root.as_ref(), tz)?,
    };
    print!("{output}");
    Ok(())
}

/// Render the `info` report for a single media file. Returns an empty string
/// when the file isn't a supported media type.
pub(crate) fn media(si: &ScanInfo, root: &dyn FileSystem, tz: OutputTZ) -> anyhow::Result<String> {
    let Some(media_file_info) = analyze_file(root, si)? else {
        debug!("Not a valid media file: {}", si.file_path);
        return Ok(String::new());
    };

    let mut out = String::new();
    writeln!(out, "Hash info:")?;
    writeln!(
        out,
        " short checksum: {}",
        media_file_info.hash_info.short_checksum
    )?;
    writeln!(
        out,
        " long checksum: {}",
        media_file_info.hash_info.long_checksum
    )?;

    let mfm = mfm_from_media_file_info(&media_file_info, &[], tz);
    let s = assemble_markdown(&mfm, &None, "")?.into_string();
    writeln!(out, "Markdown:")?;
    writeln!(out, "{s}")?;

    if let Some(exif_info) = &media_file_info.exif_info {
        if !exif_info.tags.is_empty() {
            writeln!(out, "EXIF:")?;
            for (tn, tv) in &exif_info.tags {
                writeln!(out, "  {tn}: {tv}")?;
            }
        }
        if let Some(gps) = &exif_info.gps {
            writeln!(out, "EXIF:")?;
            writeln!(out, "  gps: {gps}")?;
        }
    }
    Ok(out)
}

/// Render the `info` report for an album file. Returns an empty string when the
/// file isn't a valid album.
pub(crate) fn album(si: &ScanInfo, root: &dyn FileSystem) -> anyhow::Result<String> {
    let files = scan_fs(root);
    let album_o = parse_album(root, si, &files);
    let Some(album) = album_o else {
        warn!("Not a valid album file: {}", si.file_path);
        return Ok(String::new());
    };

    let mut out = String::new();
    writeln!(out, "Album:")?;
    writeln!(out, " title: {}", album.title)?;
    writeln!(out, " source: {}", si.file_path)?;
    writeln!(out, " output: {}", album.desired_album_md_path)?;
    writeln!(out, " entries: {}", album.files.len())?;
    for file in &album.files {
        writeln!(out, "   {file}")?;
    }
    writeln!(out)?;

    // The markdown links to the media's original paths (see `build_album_md`'s
    // `None` branch), so there's no need to inspect/hash the referenced media.
    writeln!(out, "Markdown:")?;
    let (md, _) = build_album_md(&album, None, "", None, "");
    writeln!(out, "{md}")?;

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_info_media() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let root = OsFileSystem::new("test");
        let si = ScanInfo::new("Canon_40D.jpg".to_string(), None, None, 0);
        let out = media(&si, &root, crate::test_util::tz())?;
        assert!(out.contains("Hash info:"));
        assert!(out.contains("short checksum: 6bfdabd"));
        assert!(out.contains("Markdown:"));
        Ok(())
    }

    #[test]
    fn test_info_album_google_takeout() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let root = OsFileSystem::new("test/takeout1");
        let si = ScanInfo::new(
            "Google Photos/album1/metadata.json".to_string(),
            None,
            None,
            0,
        );
        let out = album(&si, &root)?;
        assert!(out.contains("Album:"));
        assert!(out.contains("title: Some album title"));
        assert!(out.contains("entries: 1"));
        assert!(out.contains("Google Photos/album1/IMG_0001.jpg"));
        assert!(out.contains("# Some album title"));
        Ok(())
    }
}
