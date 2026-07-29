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
    /// Apple's iTunes video container: an MP4 with an `M4V ` major brand.
    M4v,
    /// RIFF container, typically Motion JPEG from a compact camera of the
    /// `MVI_1234.AVI` era.
    Avi,
    /// MPEG-1/2 elementary or program stream, as ripped from a DVD or camcorder.
    Mpg,
    /// ASF container carrying a video stream, typically a Windows Media
    /// transcode of camera footage. Audio-only ASF (WMA) is not media we keep.
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

/// Coarse photo/video classification for the `media_item.kind` column:
/// `Some("p")` for images, `Some("v")` for videos, and `None` for anything that
/// is neither (e.g. an unidentifiable file that slipped through the media filter).
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
    // take json files at face value
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
    use crate::fs::FileSystem;

    /// An extension missing from this table is never even opened, so every one
    /// the sniffer supports needs a row here. Matching is case-insensitive and
    /// only the last extension counts; one row each proves both.
    #[test]
    fn test_quick_file_type() {
        crate::test_util::setup_log();
        for (path, expected) in [
            ("test/test1.jpg", QuickFileType::Media),
            ("test/test1.mp4", QuickFileType::Media),
            ("test/Hello.m4v", QuickFileType::Media),
            ("test/Hello.mov", QuickFileType::Media),
            ("test/Hello.avi", QuickFileType::Media),
            ("test/Hello.mpg", QuickFileType::Media),
            ("test/Hello.mpeg", QuickFileType::Media),
            ("test/Hello.wmv", QuickFileType::Media),
            ("test/Hello.asf", QuickFileType::Media),
            ("test/test1.csv", QuickFileType::AlbumCsv),
            ("test/metadata.json", QuickFileType::AlbumJson),
            // Case folds, for a media and a non-media extension alike.
            ("test/MVI_0028.AVI", QuickFileType::Media),
            ("test/MeTaDaTa.JsOn", QuickFileType::AlbumJson),
            // Only the last extension decides.
            ("test/te.s.jpg", QuickFileType::Media),
            ("test/MVI_1943.avi.wmv", QuickFileType::Media),
            // Unrecognised, and no extension at all.
            ("test/test1.abc", QuickFileType::Unknown),
            ("test/tes", QuickFileType::Unknown),
        ] {
            assert_eq!(find_quick_file_type(path), expected, "classifying {path}");
        }
    }

    /// Every container the sniffer supports, checked the whole way through:
    /// sniffed from its bytes, kept under the right extension on the way out,
    /// counted as a video, and read (or not) for track metadata.
    ///
    /// The metadata column is the load-bearing one. MOV and M4V are ISO base
    /// media files whose capture time the track parser can reach; AVI is RIFF
    /// and MPEG-1/2 is a start-code stream, so neither has track metadata and
    /// the date logic must fall back to supplemental info or the filesystem
    /// rather than warn on every file. ASF is one container shared by Windows
    /// Media video and audio, so the bytes alone do not settle it — it takes the
    /// stream GUIDs, which is why the `reader-asf` feature has to be enabled.
    /// Without it every `.wmv` reports the generic ASF type and is skipped.
    #[test]
    fn test_supported_video_containers() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        use crate::fs::OsFileSystem;
        let root = OsFileSystem::new("test");

        for (name, expected, ext, has_track_metadata) in [
            ("Hello.mov", AccurateFileType::Mov, "mov", true),
            ("Hello.m4v", AccurateFileType::M4v, "m4v", true),
            ("Hello.avi", AccurateFileType::Avi, "avi", false),
            ("Hello.mpg", AccurateFileType::Mpg, "mpg", false),
            ("Hello.wmv", AccurateFileType::Wmv, "wmv", false),
        ] {
            let name = name.to_string();
            let ft = determine_file_type(root.open(&name)?, &name)?;
            assert_eq!(ft, expected, "sniffing {name}");
            assert_eq!(file_ext_from_file_type(&ft), ext, "extension for {name}");
            assert_eq!(media_kind(&ft), Some("v"), "media kind for {name}");
            assert_eq!(
                matches!(metadata_type(&ft), MetadataType::Track),
                has_track_metadata,
                "track metadata for {name}"
            );
        }
        Ok(())
    }

    /// The brand decides, not the filename: a QuickTime file Google named `.mp4`
    /// is still a QuickTime file, and an MP4 named `.mov` is still an MP4.
    #[test]
    fn test_video_type_follows_content_not_extension() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        use crate::fs::OsFileSystem;
        let root = OsFileSystem::new("test");

        // QuickTime bytes under a .mp4 name.
        let misnamed = "Hello.mov".to_string();
        assert_eq!(
            determine_file_type(root.open(&misnamed)?, &"Hello.mp4".to_string())?,
            AccurateFileType::Mov
        );
        // MP4 bytes under a .mov name.
        let mp4 = "Hello.mp4".to_string();
        assert_eq!(
            determine_file_type(root.open(&mp4)?, &"Hello.mov".to_string())?,
            AccurateFileType::Mp4
        );
        Ok(())
    }

    /// `.mpeg` is accepted on the way in but normalises to `.mpg` on the way out,
    /// so one format does not produce two extensions in the archive.
    #[test]
    fn test_mpeg_extension_normalises_to_mpg() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        use crate::fs::OsFileSystem;
        let root = OsFileSystem::new("test");
        let ft = determine_file_type(root.open("Hello.mpg")?, &"clip.mpeg".to_string())?;

        assert_eq!(ft, AccurateFileType::Mpg);
        assert_eq!(file_ext_from_file_type(&ft), "mpg");
        Ok(())
    }

    #[test]
    fn test_video_content_types() {
        assert_eq!(
            file_type_from_content_type("video/x-m4v"),
            AccurateFileType::M4v
        );
        assert_eq!(
            file_type_from_content_type("video/quicktime"),
            AccurateFileType::Mov
        );
        assert_eq!(
            file_type_from_content_type("video/mp4"),
            AccurateFileType::Mp4
        );
        assert_eq!(
            file_type_from_content_type("video/x-msvideo"),
            AccurateFileType::Avi
        );
        assert_eq!(
            file_type_from_content_type("video/mpeg"),
            AccurateFileType::Mpg
        );
        assert_eq!(
            file_type_from_content_type("video/x-ms-wmv"),
            AccurateFileType::Wmv
        );
        // Audio-only MPEG must not be mistaken for the video container.
        assert_eq!(
            file_type_from_content_type("audio/mpeg"),
            AccurateFileType::Unsupported
        );
        // Likewise for the ASF family: audio-only WMA is not video, and a bare
        // ASF media type means the reader could not find a video stream.
        assert_eq!(
            file_type_from_content_type("audio/x-ms-wma"),
            AccurateFileType::Unsupported
        );
        assert_eq!(
            file_type_from_content_type("application/vnd.ms-asf"),
            AccurateFileType::Unsupported
        );
    }
}
