use crate::album::{build_album_md, parse_album};
use crate::file_type::QuickFileType;
use crate::fs::{FileSystem, OsFileSystem};
use crate::inspect::analyze_file;
use crate::markdown::{assemble_markdown, mfm_from_media_file_info, new_note_body};
use crate::util::{ScanInfo, scan_fs};
use std::fmt::Write;
use tracing::{debug, warn};

pub(crate) fn main(input: &String, root_s: &str, skip_xmp: bool) -> anyhow::Result<()> {
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
        QuickFileType::Media => media(&si, root.as_ref(), skip_xmp)?,
    };
    print!("{output}");
    Ok(())
}

/// Render the `info` report for a single media file. Returns an empty string
/// when the file isn't a supported media type.
///
/// The report *is* the note ptsync would write, with the sources behind it
/// appended as sections — so the whole thing is one markdown document, and the
/// generated frontmatter sits where frontmatter belongs, at the top.
pub(crate) fn media(
    si: &ScanInfo,
    root: &dyn FileSystem,
    skip_xmp: bool,
) -> anyhow::Result<String> {
    let Some(media_file_info) = analyze_file(root, si, skip_xmp)? else {
        debug!("Not a valid media file: {}", si.file_path);
        return Ok(String::new());
    };

    let mut out = String::new();
    let mfm = mfm_from_media_file_info(&media_file_info, &[]);
    // Body included, so what's shown is the note `sync` would write on first
    // creation rather than its frontmatter alone. The embed is relative to the
    // note, so it names the file the same way sync's does.
    let body = new_note_body(
        &si.file_path,
        mfm.title.as_deref(),
        mfm.description.as_deref(),
    );
    let generated = assemble_markdown(&mfm, &None, &body)?.into_string();
    writeln!(out, "{}\n", generated.trim_end_matches('\n'))?;

    writeln!(out, "# {}\n", si.file_path)?;

    writeln!(out, "## Hashes\n")?;
    writeln!(
        out,
        "- **short checksum**: `{}`",
        media_file_info.hash_info.short_checksum
    )?;
    writeln!(
        out,
        "- **long checksum**: `{}`\n",
        media_file_info.hash_info.long_checksum
    )?;

    if let Some(xmp) = &media_file_info.xmp_info {
        writeln!(out, "## XMP sidecar\n")?;
        for (name, value) in [
            ("datetime", xmp.datetime.clone()),
            ("title", xmp.title.clone()),
            ("label", xmp.label.clone()),
            ("rating", xmp.rating.map(|r| r.to_string())),
            ("latitude", xmp.latitude.map(|v| v.to_string())),
            ("longitude", xmp.longitude.map(|v| v.to_string())),
            ("description", xmp.description.clone()),
        ] {
            if let Some(value) = value {
                writeln!(out, "- **{name}**: {value}")?;
            }
        }
        if !xmp.tags.is_empty() {
            writeln!(out, "- **tags**: {}", xmp.tags.join(", "))?;
        }
        if !xmp.people.is_empty() {
            writeln!(out, "- **people**: {}", xmp.people.join(", "))?;
        }
        writeln!(out)?;
    }

    if let Some(supp) = &media_file_info.supp_info {
        // Google's own JSON key names, so a value here can be found in the
        // sidecar it came from. Sits between XMP and EXIF because that is where
        // Takeout's metadata ranks in `best_guess_taken_dt`/`best_guess_lat_long`.
        let mut fields = String::new();
        // A `title` that was only the file's own name has already been dropped
        // on load, so anything shown here is a title someone actually set.
        for (name, value) in [
            ("title", supp.title.as_deref()),
            ("description", supp.description.as_deref()),
        ] {
            if let Some(value) = value {
                writeln!(fields, "- **{name}**: {value}")?;
            }
        }
        // Flags are reported only when set: Google omits them when false, and
        // listing `false` against every photo in an archive says nothing.
        for (name, set) in [("favorited", supp.favorited), ("archived", supp.archived)] {
            if set {
                writeln!(fields, "- **{name}**: true")?;
            }
        }
        for (name, dt) in [
            ("photoTakenTime", supp.photo_taken_time.as_ref()),
            ("creationTime", supp.creation_time.as_ref()),
        ] {
            let Some(dt) = dt else { continue };
            match (dt.timestamp_s_as_iso_8601(), dt.formatted.as_deref()) {
                (Some(iso), Some(formatted)) => {
                    writeln!(fields, "- **{name}**: {iso} ({formatted})")?
                }
                (Some(iso), None) => writeln!(fields, "- **{name}**: {iso}")?,
                (None, Some(formatted)) => writeln!(fields, "- **{name}**: {formatted}")?,
                (None, None) => {}
            }
        }
        // Parsing already dropped Google's `0, 0` "no fix" blocks, so anything
        // still here is a real position with both halves present.
        for (name, geo) in [
            ("geoData", supp.geo_data.as_ref()),
            ("geoDataExif", supp.geo_data_exif.as_ref()),
        ] {
            if let Some(geo) = geo
                && let (Some(lat), Some(long)) = (geo.latitude, geo.longitude)
            {
                writeln!(fields, "- **{name}**: {lat}, {long}")?;
            }
        }
        let people: Vec<&str> = supp
            .people
            .iter()
            .filter_map(|p| p.name.as_deref())
            .collect();
        if !people.is_empty() {
            writeln!(fields, "- **people**: {}", people.join(", "))?;
        }
        if !fields.is_empty() {
            writeln!(out, "## Supplemental JSON sidecar\n")?;
            write!(out, "{fields}")?;
            writeln!(out)?;
        }
    }

    if let Some(exif_info) = &media_file_info.exif_info
        && (!exif_info.tags.is_empty() || exif_info.gps.is_some())
    {
        writeln!(out, "## EXIF\n")?;
        // Tag values are unescaped camera output, so quote them as code
        // rather than let stray `*` or `[` render as markup.
        for (tn, tv) in &exif_info.tags {
            writeln!(out, "- **{tn}**: `{tv}`")?;
        }
        if let Some(gps) = &exif_info.gps {
            writeln!(out, "- **gps**: `{gps}`")?;
        }
        // Not an EXIF tag but a maker note one, so it is named for where it
        // came from rather than passed off as a sibling of the tags above.
        if let Some(id) = &exif_info.content_identifier {
            writeln!(out, "- **contentIdentifier** (Apple MakerNote): `{id}`")?;
        }
        writeln!(out)?;
    }

    // Videos have no EXIF, so without this a `ptsync info` on a clip printed
    // its hashes and nothing else - not even the date and position that its
    // own frontmatter had just been built from.
    if let Some(track) = &media_file_info.track_info {
        writeln!(out, "## Track Info\n")?;
        for (name, value) in [
            ("width", track.width.map(|v| v.to_string())),
            ("height", track.height.map(|v| v.to_string())),
            ("creationTime", track.creation_time.clone()),
            ("durationMs", track.duration_ms.map(|v| v.to_string())),
            ("make", track.make.clone()),
            ("model", track.model.clone()),
            ("software", track.software.clone()),
            ("author", track.author.clone()),
            ("gps", track.gps_iso_6709.clone()),
            ("latitude", track.latitude.map(|v| v.to_string())),
            ("longitude", track.longitude.map(|v| v.to_string())),
            ("contentIdentifier", track.content_identifier.clone()),
        ] {
            if let Some(value) = value {
                writeln!(out, "- **{name}**: `{value}`")?;
            }
        }
        // Reported as the transform rather than folded into the dimensions
        // above, which stay as the file stores them. See
        // `PsTrackInfo::display_transform`.
        if let Some((mirrored, rotate)) = track.display_transform() {
            let mirrored = if mirrored { ", mirrored" } else { "" };
            writeln!(out, "- **rotation**: `{rotate}°{mirrored}`")?;
        }
        // The `moov/meta` keys behind the fields above, plus the ones nothing
        // reads yet - the live photo scores, the location accuracy.
        for (name, value) in &track.tags {
            writeln!(out, "- **{name}**: `{value}`")?;
        }
        writeln!(out)?;
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
    // The album note leads the report and supplies its `# Title` heading; the
    // markdown links to the media's original paths (see `build_album_md`'s
    // `None` branch), so there's no need to inspect/hash the referenced media.
    let (generated, _) = build_album_md(&album, None, "", None, "");
    writeln!(out, "{}\n", generated.trim_end_matches('\n'))?;

    writeln!(out, "## Album\n")?;
    writeln!(out, "- **source**: `{}`", si.file_path)?;
    writeln!(out, "- **output**: `{}`", album.desired_album_md_path)?;
    writeln!(out, "- **entries**: {}", album.files.len())?;
    for file in &album.files {
        writeln!(out, "  - `{file}`")?;
    }
    writeln!(out)?;

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
        let out = media(&si, &root, false)?;
        // The generated note leads, so its frontmatter opens the document.
        assert!(out.starts_with("---\ndatetime:"));
        // Body and all, matching what `sync` writes on first creation: the
        // photo embed, then the sidecar's title and description under it.
        assert!(out.contains("\n![](Canon_40D.jpg)\n\n# A Canon 40D test frame\n"));
        assert!(out.contains("Sample image used by the ptsync test suite."));
        assert!(out.contains("# Canon_40D.jpg"));
        assert!(out.contains("## Hashes"));
        assert!(out.contains("**short checksum**: `6bfdabd`"));
        // The `.xmp` sidecar beside the fixture is read and reported.
        assert!(out.contains("## XMP sidecar"));
        assert!(out.contains("**rating**: 5"));
        assert!(out.contains("**people**: Ada Lovelace"));
        // The Takeout sidecar beside it is too, and its `title` - the file's own
        // name - is dropped rather than reported as a title anyone gave it.
        assert!(out.contains("## Supplemental JSON sidecar"));
        assert!(
            out.contains("- **description**: First frame off the 40D, straight out of the camera.")
        );
        assert!(
            !out.contains("- **title**: Canon_40D.jpg"),
            "a title that is only the file name must not be reported, got:\n{out}"
        );
        // Flags show only when set: the fixture is favourited but not archived.
        assert!(out.contains("- **favorited**: true"));
        assert!(!out.contains("archived"));
        Ok(())
    }

    #[test]
    fn test_info_media_supplemental_sidecar() -> anyhow::Result<()> {
        use std::fs;
        crate::test_util::setup_log();

        // A media file with a Takeout sidecar beside it, richer than any
        // checked-in fixture: both times, both geo fields (one of them Google's
        // "no fix" zeros), and a face tag.
        let test_dir = std::path::Path::new("target/test_info_supplemental");
        if test_dir.exists() {
            fs::remove_dir_all(test_dir)?;
        }
        fs::create_dir_all(test_dir)?;
        fs::copy("test/Canon_40D.jpg", test_dir.join("Canon_40D.jpg"))?;
        fs::write(
            test_dir.join("Canon_40D.jpg.supplemental-metadata.json"),
            r#"{
              "photoTakenTime": { "timestamp": "1716337071", "formatted": "22 May 2024, 00:17:51 UTC" },
              "creationTime": { "timestamp": "1716539968", "formatted": "24 May 2024, 08:39:28 UTC" },
              "geoData": { "latitude": 51.5, "longitude": -0.125 },
              "geoDataExif": { "latitude": 0.0, "longitude": 0.0 },
              "people": [{ "name": "Tim Tam" }]
            }"#,
        )?;

        let root = OsFileSystem::new(&test_dir.to_string_lossy());
        let si = ScanInfo::new("Canon_40D.jpg".to_string(), None, None, 0);
        let out = media(&si, &root, true)?;

        assert!(out.contains("## Supplemental JSON sidecar"));
        assert!(out.contains(
            "- **photoTakenTime**: 2024-05-22T00:17:51+00:00 (22 May 2024, 00:17:51 UTC)"
        ));
        assert!(
            out.contains(
                "- **creationTime**: 2024-05-24T08:39:28+00:00 (24 May 2024, 08:39:28 UTC)"
            )
        );
        assert!(out.contains("- **geoData**: 51.5, -0.125"));
        // Takeout's `0, 0` "no fix" block never survives parsing, so the field
        // is absent rather than reported as a location off the coast of Africa.
        assert!(!out.contains("**geoDataExif**"));
        assert!(!out.contains(": 0, 0"));
        assert!(out.contains("- **people**: Tim Tam"));
        // photoTakenTime is what the note dates by, there being no XMP here.
        assert!(out.starts_with("---\ndatetime: \"2024-05-22T00:17:51+00:00\""));

        fs::remove_dir_all(test_dir)?;
        Ok(())
    }

    #[test]
    fn test_info_media_no_supplemental_sidecar() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let root = OsFileSystem::new("test");
        // No `.json` beside this fixture, so the section is left out entirely.
        let si = ScanInfo::new("Hello.mp4".to_string(), None, None, 0);
        let out = media(&si, &root, true)?;
        assert!(!out.contains("## Supplemental JSON sidecar"));
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
        // The generated album note leads and supplies the `# Title` heading.
        assert!(out.contains("# Some album title"));
        assert!(out.contains("![Photo](Google Photos/album1/IMG_0001.jpg)"));
        assert!(out.contains("## Album"));
        assert!(out.contains("**source**: `Google Photos/album1/metadata.json`"));
        assert!(out.contains("**entries**: 1"));
        assert!(out.contains("  - `Google Photos/album1/IMG_0001.jpg`"));
        Ok(())
    }
}
