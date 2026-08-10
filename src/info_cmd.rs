use crate::album::{build_album_md, parse_album};
use crate::exif_util::PsExifInfo;
use crate::file_type::QuickFileType;
use crate::fs::{FileSystem, OsFileSystem};
use crate::inspect::analyze_file;
use crate::markdown::{
    assemble_markdown, get_desired_markdown_path, mfm_from_media_file_info, new_note_body,
};
use crate::media::{MediaFileInfo, media_file_derived_from_media_info};
use crate::supplemental_info::{
    SupplementalInfoDateTime, SupplementalInfoGeoData, detect_supplemental_info,
};
use crate::util::{OutputTZ, ScanInfo, scan_fs};
use std::fmt::Write;
use tracing::{debug, warn};

pub(crate) fn main(input: &String, root_s: &str, tz: OutputTZ) -> anyhow::Result<()> {
    debug!("Inspecting: {input}");
    let root: Box<dyn FileSystem> = Box::new(OsFileSystem::new(root_s));
    print!("{}", render(input, root.as_ref(), tz)?);
    Ok(())
}

/// The whole document the command prints. Empty when the file is not one ptsync
/// can read.
pub(crate) fn render(
    input: &String,
    root: &dyn FileSystem,
    tz: OutputTZ,
) -> anyhow::Result<String> {
    let len = root.metadata(input).map(|m| m.len).unwrap_or(0);
    let si = ScanInfo::new(input.clone(), None, None, len);
    match si.quick_file_type {
        QuickFileType::Unknown => {
            warn!("File type is unknown, skipping: {input}");
            Ok(String::new())
        }
        QuickFileType::AlbumCsv | QuickFileType::AlbumJson => album(&si, root),
        QuickFileType::Media => media(&si, root, tz),
    }
}

/// The markdown sidecar `sync` would write for this file, followed by everything
/// else parsed out of it.
///
/// Empty when the file isn't a supported media type.
pub(crate) fn media(si: &ScanInfo, root: &dyn FileSystem, tz: OutputTZ) -> anyhow::Result<String> {
    let Some(media_file_info) = analyze_file(root, si)? else {
        debug!("Not a valid media file: {}", si.file_path);
        return Ok(String::new());
    };

    let derived = media_file_derived_from_media_info(&media_file_info, tz)?;
    let media_path = derived
        .desired_media_path
        .as_ref()
        .map(|p| format!("{p}.{}", derived.desired_media_extension));

    let mfm = mfm_from_media_file_info(&media_file_info, &[], tz);
    let body = match &media_path {
        Some(p) => new_note_body(p),
        None => String::new(),
    };
    let mut out = assemble_markdown(&mfm, &None, &body)?.into_string();

    source_section(&mut out, si, &media_file_info, tz)?;
    output_section(&mut out, &media_path)?;
    supplemental_section(&mut out, si, root, &media_file_info, tz)?;
    track_section(&mut out, &media_file_info)?;
    exif_section(&mut out, &media_file_info)?;
    Ok(out)
}

fn source_section(
    out: &mut String,
    si: &ScanInfo,
    media_file_info: &MediaFileInfo,
    tz: OutputTZ,
) -> anyhow::Result<()> {
    let rows = vec![
        ("Path", Some(code(&si.file_path))),
        ("Size", Some(format!("{} bytes", media_file_info.file_size))),
        (
            "Type from name",
            Some(media_file_info.quick_file_type.to_string()),
        ),
        (
            "Type from content",
            Some(media_file_info.accurate_file_type.to_string()),
        ),
        (
            "Modified",
            media_file_info.modified.and_then(|m| tz.render_millis(m)),
        ),
        (
            "Created",
            media_file_info.created.and_then(|c| tz.render_millis(c)),
        ),
        (
            "Short checksum",
            Some(code(&media_file_info.hash_info.short_checksum)),
        ),
        (
            "Long checksum",
            Some(code(&media_file_info.hash_info.long_checksum)),
        ),
    ];
    section(out, "Source file", ("Field", "Value"), rows)
}

/// Where `sync` would file this photo, relative to its output directory.
fn output_section(out: &mut String, media_path: &Option<String>) -> anyhow::Result<()> {
    let Some(media_path) = media_path else {
        return Ok(());
    };
    let rows = vec![
        ("Media", Some(code(media_path))),
        (
            "Markdown",
            Some(code(&get_desired_markdown_path(media_path)?)),
        ),
    ];
    section(out, "Output paths", ("Field", "Value"), rows)
}

/// The Takeout sidecar, reported even when it failed to parse — a sidecar that is
/// found but unreadable is the interesting case.
fn supplemental_section(
    out: &mut String,
    si: &ScanInfo,
    root: &dyn FileSystem,
    media_file_info: &MediaFileInfo,
    tz: OutputTZ,
) -> anyhow::Result<()> {
    let sidecar = detect_supplemental_info(&si.file_path, root);
    let supp = media_file_info.supp_info.as_ref();
    let people: Vec<String> = supp
        .map(|s| {
            s.people
                .iter()
                .filter_map(|p| p.name.as_ref())
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let rows = vec![
        ("Sidecar", sidecar.as_deref().map(code)),
        (
            "Photo taken time",
            supp.and_then(|s| supp_datetime(&s.photo_taken_time, tz)),
        ),
        (
            "Creation time",
            supp.and_then(|s| supp_datetime(&s.creation_time, tz)),
        ),
        (
            "Location (geoDataExif)",
            supp.and_then(|s| coords(&s.geo_data_exif)),
        ),
        ("Location (geoData)", supp.and_then(|s| coords(&s.geo_data))),
        ("People", (!people.is_empty()).then(|| people.join(", "))),
    ];
    section(out, "Supplemental metadata", ("Field", "Value"), rows)
}

fn track_section(out: &mut String, media_file_info: &MediaFileInfo) -> anyhow::Result<()> {
    let Some(track) = &media_file_info.track_info else {
        return Ok(());
    };
    let (latitude, longitude) = split_lat_long(track.lat_long());
    let rows = vec![
        ("Width", track.width.map(|w| w.to_string())),
        ("Height", track.height.map(|h| h.to_string())),
        ("Created", track.creation_time.clone()),
        ("Duration", track.duration_ms.map(|d| format!("{d} ms"))),
        ("Make", track.make.clone()),
        ("Model", track.model.clone()),
        ("Software", track.software.clone()),
        ("Author", track.author.clone()),
        ("Location (ISO 6709)", track.gps_iso_6709.clone()),
        ("Latitude", latitude),
        ("Longitude", longitude),
    ];
    section(out, "Video track", ("Field", "Value"), rows)
}

/// The decoded GPS fix, then every tag read off the file.
fn exif_section(out: &mut String, media_file_info: &MediaFileInfo) -> anyhow::Result<()> {
    let Some(exif) = &media_file_info.exif_info else {
        return Ok(());
    };
    let (latitude, longitude) = split_lat_long(exif_lat_long(exif));
    let rows = vec![
        ("Location (ISO 6709)", exif.gps.clone()),
        ("Latitude", latitude),
        ("Longitude", longitude),
    ];
    let has_gps = rows.iter().any(|(_, v)| v.is_some());
    if !has_gps && exif.tags.is_empty() {
        return Ok(());
    }
    if has_gps {
        section(out, "EXIF", ("Field", "Value"), rows)?;
    } else {
        writeln!(out, "\n## EXIF")?;
    }
    if exif.tags.is_empty() {
        return Ok(());
    }

    let mut tags: Vec<(&String, &String)> = exif.tags.iter().collect();
    tags.sort();
    let tag_rows = tags
        .into_iter()
        .map(|(name, value)| (name.as_str(), Some(value.clone())))
        .collect();
    writeln!(out, "\n### Tags\n")?;
    table(out, ("Tag", "Value"), tag_rows)
}

/// Empty when the file isn't a valid album.
pub(crate) fn album(si: &ScanInfo, root: &dyn FileSystem) -> anyhow::Result<String> {
    let files = scan_fs(root);
    let album_o = parse_album(root, si, &files);
    let Some(album) = album_o else {
        warn!("Not a valid album file: {}", si.file_path);
        return Ok(String::new());
    };

    // Links to the media's original paths (`build_album_md`'s `None` branch), so
    // the referenced media needs no inspecting or hashing.
    let (mut out, _) = build_album_md(&album, None, "", None, "");

    let rows = vec![
        ("Title", Some(album.title.clone())),
        ("Source", Some(code(&si.file_path))),
        ("Output", Some(code(&album.desired_album_md_path))),
        ("Entries", Some(album.files.len().to_string())),
    ];
    section(&mut out, "Album", ("Field", "Value"), rows)?;

    if !album.files.is_empty() {
        writeln!(out, "\n### Members\n")?;
        for file in &album.files {
            writeln!(out, "- {}", code(file))?;
        }
    }
    Ok(out)
}

/// A heading and its table, both skipped when every row is absent.
fn section(
    out: &mut String,
    heading: &str,
    columns: (&str, &str),
    rows: Vec<(&str, Option<String>)>,
) -> anyhow::Result<()> {
    if rows.iter().all(|(_, value)| value.is_none()) {
        return Ok(());
    }
    writeln!(out, "\n## {heading}\n")?;
    table(out, columns, rows)
}

/// A GitHub-flavoured markdown table; rows with no value are dropped so metadata
/// the file doesn't carry leaves no empty row behind.
fn table(
    out: &mut String,
    columns: (&str, &str),
    rows: Vec<(&str, Option<String>)>,
) -> anyhow::Result<()> {
    writeln!(out, "| {} | {} |", cell(columns.0), cell(columns.1))?;
    writeln!(out, "| --- | --- |")?;
    for (name, value) in rows {
        let Some(value) = value else {
            continue;
        };
        writeln!(out, "| {} | {} |", cell(name), cell(&value))?;
    }
    Ok(())
}

/// EXIF tag values and file names are whatever the file said, so anything that
/// would break out of its cell is neutralised.
fn cell(value: &str) -> String {
    value.replace('|', "\\|").replace(['\n', '\r'], " ")
}

fn code(value: &str) -> String {
    format!("`{value}`")
}

fn split_lat_long(lat_long: Option<(f64, f64)>) -> (Option<String>, Option<String>) {
    match lat_long {
        Some((lat, long)) => (Some(lat.to_string()), Some(long.to_string())),
        None => (None, None),
    }
}

fn exif_lat_long(exif: &PsExifInfo) -> Option<(f64, f64)> {
    Some((exif.latitude?, exif.longitude?))
}

/// Both spellings Takeout gives: the instant in the output zone, and Google's own
/// rendering of it.
fn supp_datetime(dt: &Option<SupplementalInfoDateTime>, tz: OutputTZ) -> Option<String> {
    let dt = dt.as_ref()?;
    match (dt.timestamp_s_as_iso_8601(tz), dt.formatted.clone()) {
        (Some(iso), Some(formatted)) => Some(format!("{iso} ({formatted})")),
        (Some(iso), None) => Some(iso),
        (None, formatted) => formatted,
    }
}

fn coords(geo: &Option<SupplementalInfoGeoData>) -> Option<String> {
    let geo = geo.as_ref()?;
    Some(format!("{}, {}", geo.latitude?, geo.longitude?))
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

        // The frontmatter and body `sync` would write for this photo.
        assert!(out.starts_with("---\n"));
        assert!(out.contains("datetime: \"2008-05-30T15:56:01+00:00\""));
        assert!(out.contains(
            "checksum: 6bfdabd4fc33d112283c147acccc574e770bbe6fbdbc3d4da968ba7b606ecc2f"
        ));
        assert!(out.contains("![](1556-01000.jpg)"));

        assert!(out.contains("## Source file"));
        assert!(out.contains("| Short checksum | `6bfdabd` |"));
        assert!(out.contains("| Type from content | Jpg |"));
        assert!(out.contains("## Output paths"));
        assert!(out.contains("| Media | `2008/05/30/1556-01000.jpg` |"));
        assert!(out.contains("| Markdown | `2008/05/30/1556-01000.md` |"));
        assert!(out.contains("## EXIF"));
        assert!(out.contains("| Make | Canon |"));
        assert!(out.contains("| Model | Canon EOS 40D |"));
        // No sidecar, no GPS fix and no video track, so those sections are absent
        // rather than empty.
        assert!(!out.contains("## Supplemental metadata"));
        assert!(!out.contains("## Video track"));
        assert!(!out.contains("| Latitude |"));
        Ok(())
    }

    #[test]
    fn test_info_media_with_sidecar() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let root = OsFileSystem::new("test/takeout_basic");
        let si = ScanInfo::new(
            "Google Photos/Holiday/Canon_40D.jpg".to_string(),
            None,
            None,
            0,
        );
        let out = media(&si, &root, crate::test_util::tz())?;
        assert!(out.contains("## Supplemental metadata"));
        assert!(out.contains(
            "| Sidecar | `Google Photos/Holiday/Canon_40D.jpg.supplemental-metadata.json` |"
        ));
        // The sidecar's instant, rendered in the output zone, with Google's own
        // UTC spelling of it alongside.
        assert!(out.contains(
            "| Photo taken time | 2024-05-22T12:17:51+12:00 (22 May 2024, 00:17:51 UTC) |"
        ));
        // And it outranks EXIF, so it decides the output path.
        assert!(out.contains("| Media | `2024/05/22/1217-51000.jpg` |"));
        Ok(())
    }

    #[test]
    fn test_info_media_video_track() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let root = OsFileSystem::new("test");
        let si = ScanInfo::new("Hello.mp4".to_string(), None, None, 0);
        let out = media(&si, &root, crate::test_util::tz())?;
        assert!(out.contains("| Type from content | Mp4 |"));
        assert!(out.contains("## Video track"));
        assert!(out.contains("| Width | "));
        // Videos carry no EXIF.
        assert!(!out.contains("## EXIF"));
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
        assert!(out.contains("# Some album title"));
        assert!(out.contains("![Photo](Google Photos/album1/IMG_0001.jpg)"));
        assert!(out.contains("## Album"));
        assert!(out.contains("| Title | Some album title |"));
        assert!(out.contains("| Output | `albums/album1.md` |"));
        assert!(out.contains("| Entries | 1 |"));
        assert!(out.contains("- `Google Photos/album1/IMG_0001.jpg`"));
        Ok(())
    }
}
