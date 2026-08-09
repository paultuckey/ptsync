use serde::Serialize;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use strum_macros::Display;
use tracing::{debug, warn};

#[derive(Serialize, Debug, Clone, PartialEq, Display)]
#[serde(rename_all(serialize = "camelCase"))]
pub(crate) enum QuickFileType {
    Media,
    AlbumCsv,
    AlbumJson,
    Unknown,
}

pub(crate) fn find_quick_file_type(file_path: &str) -> QuickFileType {
    let p = Path::new(file_path);
    let lowercase_file_name_str = p
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_lowercase())
        .unwrap_or_default();
    if lowercase_file_name_str.eq("metadata.json") {
        return QuickFileType::AlbumJson;
    }
    let lowercase_file_ext = p
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|s| s.to_lowercase())
        .unwrap_or_default();
    match lowercase_file_ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "heic" | "mp4" | "m4v" | "mov" | "avi" | "mpg"
        | "mpeg" | "wmv" | "asf" => QuickFileType::Media,
        "csv" => QuickFileType::AlbumCsv,
        _ => QuickFileType::Unknown,
    }
}

#[derive(Serialize, Clone, Debug, PartialEq, Display)]
#[serde(rename_all(serialize = "camelCase"))]
pub(crate) enum AccurateFileType {
    Jpg,
    Png,
    Heic,
    Gif,
    Mp4,
    Mov,
    /// An MP4 with an `M4V ` major brand.
    M4v,
    /// RIFF container, typically Motion JPEG from a compact camera.
    Avi,
    /// MPEG-1/2 elementary or program stream, as ripped from a DVD or camcorder.
    Mpg,
    /// An ASF container carrying a video stream. ASF holds `.wma` audio just as
    /// happily, so only the ones with video reach this type — see
    /// [`file_type_from_content_type`].
    Wmv,
    Json,
    Csv,
    Unsupported,
}

pub(crate) fn file_ext_from_file_type(ff: &AccurateFileType) -> String {
    match ff {
        AccurateFileType::Jpg => "jpg".to_string(),
        AccurateFileType::Gif => "gif".to_string(),
        AccurateFileType::Png => "png".to_string(),
        AccurateFileType::Heic => "heic".to_string(),
        AccurateFileType::Mp4 => "mp4".to_string(),
        AccurateFileType::Mov => "mov".to_string(),
        AccurateFileType::M4v => "m4v".to_string(),
        AccurateFileType::Avi => "avi".to_string(),
        AccurateFileType::Mpg => "mpg".to_string(),
        AccurateFileType::Wmv => "wmv".to_string(),
        AccurateFileType::Unsupported => "bin".to_string(),
        AccurateFileType::Json => "json".to_string(),
        AccurateFileType::Csv => "csv".to_string(),
    }
}

/// The `media_item.kind` column: `"p"` for images, `"v"` for videos, `None` for
/// anything that is neither.
pub(crate) fn media_kind(ff: &AccurateFileType) -> Option<&'static str> {
    match ff {
        AccurateFileType::Jpg
        | AccurateFileType::Png
        | AccurateFileType::Heic
        | AccurateFileType::Gif => Some("p"),
        AccurateFileType::Mp4
        | AccurateFileType::Mov
        | AccurateFileType::M4v
        | AccurateFileType::Avi
        | AccurateFileType::Mpg
        | AccurateFileType::Wmv => Some("v"),
        AccurateFileType::Json | AccurateFileType::Csv | AccurateFileType::Unsupported => None,
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum MetadataType {
    ExifTags,
    Track,
    NoMetadata,
}

pub(crate) fn metadata_type(ff: &AccurateFileType) -> MetadataType {
    match ff {
        AccurateFileType::Jpg
        | AccurateFileType::Png
        | AccurateFileType::Heic
        | AccurateFileType::Gif => MetadataType::ExifTags,
        AccurateFileType::Mp4 | AccurateFileType::Mov | AccurateFileType::M4v => {
            MetadataType::Track
        }
        // Videos, but not ISO base media files, so the track parser cannot read
        // them — asking it to try only produces a warning per file. Their capture
        // time comes from supplemental metadata or the filesystem instead.
        AccurateFileType::Avi | AccurateFileType::Mpg | AccurateFileType::Wmv => {
            MetadataType::NoMetadata
        }
        AccurateFileType::Json | AccurateFileType::Csv | AccurateFileType::Unsupported => {
            MetadataType::NoMetadata
        }
    }
}

pub(crate) fn file_type_from_content_type(ct: &str) -> AccurateFileType {
    match ct {
        "image/jpeg" => AccurateFileType::Jpg,
        "image/gif" => AccurateFileType::Gif,
        "image/png" => AccurateFileType::Png,
        "image/heic" => AccurateFileType::Heic,
        "video/mp4" => AccurateFileType::Mp4,
        "application/mp4" => AccurateFileType::Mp4,
        "video/quicktime" => AccurateFileType::Mov,
        "video/x-m4v" => AccurateFileType::M4v,
        "video/x-msvideo" => AccurateFileType::Avi,
        "video/mpeg" => AccurateFileType::Mpg,
        "video/x-ms-wmv" => AccurateFileType::Wmv,
        // The other two ASF outcomes — `.wma` audio, and the bare container when
        // the header declares neither stream — would otherwise be filed as videos
        // with no picture.
        "audio/x-ms-wma" => AccurateFileType::Unsupported,
        "application/vnd.ms-asf" => AccurateFileType::Unsupported,
        "application/octet-stream" => AccurateFileType::Unsupported,
        "application/json" => AccurateFileType::Unsupported,
        "text/csv" => AccurateFileType::Csv,
        _ => AccurateFileType::Unsupported,
    }
}

pub(crate) fn determine_file_type<R: Read + Seek>(
    mut reader: R,
    name: &String,
) -> anyhow::Result<AccurateFileType> {
    // JSON is taken at face value rather than sniffed.
    if name.to_lowercase().ends_with(".json") {
        return Ok(AccurateFileType::Json);
    }
    reader.seek(SeekFrom::Start(0))?;
    let fmt = match file_format::FileFormat::from_reader(reader) {
        Err(e) => {
            warn!("  could not determine file format for file:{name:?}, error:{e:?}");
            return Ok(AccurateFileType::Unsupported);
        }
        Ok(fmt) => fmt,
    };
    let mt = fmt.media_type();
    if mt == "application/octet-stream" {
        debug!("  can not calculate mime type file:{name:?}");
        return Ok(AccurateFileType::Unsupported);
    }
    if mt == "application/x-empty" {
        debug!("  file appears to be empty file:{name:?}");
        return Ok(AccurateFileType::Unsupported);
    }
    let ft = file_type_from_content_type(mt);
    debug!("  file:{name:?}: mime type {mt:?}, file type {ft:?}");
    Ok(ft)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{FileSystem, OsFileSystem};
    use crate::test_util::{fake_png, setup_log};
    use std::io::Cursor;

    #[test]
    fn test_quick_file_type() {
        setup_log();
        for (path, expected) in [
            ("test/test1.jpg", QuickFileType::Media),
            ("test/te.s.jpg", QuickFileType::Media),
            ("test/test1.mp4", QuickFileType::Media),
            ("test/Hello.m4v", QuickFileType::Media),
            ("test/Hello.mov", QuickFileType::Media),
            ("test/Hello.avi", QuickFileType::Media),
            ("test/Hello.mpg", QuickFileType::Media),
            ("test/Hello.mpeg", QuickFileType::Media),
            ("test/Hello.wmv", QuickFileType::Media),
            ("test/Hello.asf", QuickFileType::Media),
            // The extension is matched case-insensitively, as Takeout and iCloud
            // both shout it on some files and not others.
            ("test/MVI_0028.AVI", QuickFileType::Media),
            ("test/Hello.MOV", QuickFileType::Media),
            ("test/test1.CsV", QuickFileType::AlbumCsv),
            ("test/test1.csv", QuickFileType::AlbumCsv),
            // Only `metadata.json` is an album; any other json is not.
            ("test/metadata.json", QuickFileType::AlbumJson),
            ("test/MeTaDaTa.JsOn", QuickFileType::AlbumJson),
            ("test/other.json", QuickFileType::Unknown),
            ("test/test1.abc", QuickFileType::Unknown),
            ("test/tes", QuickFileType::Unknown),
        ] {
            assert_eq!(find_quick_file_type(path), expected, "classifying {path}");
        }
    }

    /// Every supported container, read from its bytes. `metadata_type` decides
    /// where a capture time may come from: an ISO base media file carries a
    /// track, a RIFF or ASF one carries nothing and would only make the track
    /// parser warn once per file.
    #[test]
    fn test_accurate_file_type() -> anyhow::Result<()> {
        setup_log();
        let root = OsFileSystem::new("test");
        // (fixture read, name it is read under, type, extension, kind, metadata)
        for (fixture, name, ft, ext, kind, meta) in [
            (
                "Canon_40D.jpg",
                "Canon_40D.jpg",
                AccurateFileType::Jpg,
                "jpg",
                Some("p"),
                MetadataType::ExifTags,
            ),
            // Takeout exports plenty of QuickTime, so the type comes from the
            // `qt  ` brand in the bytes rather than the name.
            (
                "Hello.mov",
                "Hello.mov",
                AccurateFileType::Mov,
                "mov",
                Some("v"),
                MetadataType::Track,
            ),
            (
                "Hello.mp4",
                "Hello.mp4",
                AccurateFileType::Mp4,
                "mp4",
                Some("v"),
                MetadataType::Track,
            ),
            // Falling through to `Unsupported` would keep an `.m4v`'s capture
            // time out of the date logic entirely.
            (
                "Hello.m4v",
                "Hello.m4v",
                AccurateFileType::M4v,
                "m4v",
                Some("v"),
                MetadataType::Track,
            ),
            (
                "Hello.avi",
                "Hello.avi",
                AccurateFileType::Avi,
                "avi",
                Some("v"),
                MetadataType::NoMetadata,
            ),
            // Identified by a start-code prefix rather than a container brand.
            (
                "Hello.mpg",
                "Hello.mpg",
                AccurateFileType::Mpg,
                "mpg",
                Some("v"),
                MetadataType::NoMetadata,
            ),
            // Sniffing reaches past the ASF container to the stream list, since
            // the same bytes hold `.wma` audio.
            (
                "Hello.wmv",
                "Hello.wmv",
                AccurateFileType::Wmv,
                "wmv",
                Some("v"),
                MetadataType::NoMetadata,
            ),
            // `.mpeg` and `.asf` are accepted on the way in, since plenty of
            // files are named that way, then normalised so one format does not
            // produce two extensions in the archive.
            (
                "Hello.mpg",
                "clip.mpeg",
                AccurateFileType::Mpg,
                "mpg",
                Some("v"),
                MetadataType::NoMetadata,
            ),
            (
                "Hello.wmv",
                "clip.asf",
                AccurateFileType::Wmv,
                "wmv",
                Some("v"),
                MetadataType::NoMetadata,
            ),
            // The other side of that coin: rejected, or a music file lands in
            // the archive as a video with nothing to show.
            (
                "Hello.wma",
                "Hello.wma",
                AccurateFileType::Unsupported,
                "bin",
                None,
                MetadataType::NoMetadata,
            ),
        ] {
            let name = name.to_string();
            let got = determine_file_type(root.open(fixture)?, &name)?;
            assert_eq!(got, ft, "{fixture} read as {name}");
            assert_eq!(file_ext_from_file_type(&got), ext);
            assert_eq!(media_kind(&got), kind);
            assert_eq!(metadata_type(&got), meta);
        }
        Ok(())
    }

    /// A file is classified by its bytes, so a re-extension cannot smuggle
    /// unsupported content through as media, and a misnamed photo is still
    /// filed as what it is.
    #[test]
    fn test_type_follows_content_not_extension() -> anyhow::Result<()> {
        setup_log();
        let root = OsFileSystem::new("test");
        for (fixture, name, expected) in [
            ("Hello.mov", "Hello.mp4", AccurateFileType::Mov),
            ("Hello.mp4", "Hello.mov", AccurateFileType::Mp4),
            ("Canon_40D.jpg", "photo.png", AccurateFileType::Jpg),
            // ASF audio renamed to a video extension stays unsupported.
            ("Hello.wma", "Hello.wmv", AccurateFileType::Unsupported),
        ] {
            assert_eq!(
                determine_file_type(root.open(fixture)?, &name.to_string())?,
                expected,
                "{fixture} read as {name}"
            );
        }

        // A PNG named .jpg, then empty and garbage content — all from bytes
        // alone, and never a panic.
        for (bytes, name, expected) in [
            (fake_png(), "photo.jpg", AccurateFileType::Png),
            (Vec::new(), "x.jpg", AccurateFileType::Unsupported),
            (
                vec![0xde, 0xad, 0xbe, 0xef],
                "x.mp4",
                AccurateFileType::Unsupported,
            ),
        ] {
            assert_eq!(
                determine_file_type(Cursor::new(bytes), &name.to_string())?,
                expected,
                "sniffing {name}"
            );
        }
        Ok(())
    }

    #[test]
    fn test_video_content_types() {
        for (content_type, expected) in [
            ("video/x-m4v", AccurateFileType::M4v),
            ("video/quicktime", AccurateFileType::Mov),
            ("video/mp4", AccurateFileType::Mp4),
            ("video/x-msvideo", AccurateFileType::Avi),
            ("video/mpeg", AccurateFileType::Mpg),
            ("video/x-ms-wmv", AccurateFileType::Wmv),
            // Audio-only MPEG must not be mistaken for the video container,
            // nor the audio and stream-less halves of ASF, which share the
            // container with WMV but hold no picture.
            ("audio/mpeg", AccurateFileType::Unsupported),
            ("audio/x-ms-wma", AccurateFileType::Unsupported),
            ("application/vnd.ms-asf", AccurateFileType::Unsupported),
        ] {
            assert_eq!(
                file_type_from_content_type(content_type),
                expected,
                "content type {content_type}"
            );
        }
    }
}
