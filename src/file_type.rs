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
    use crate::fs::FileSystem;

    #[test]
    fn test_quick_file_type() {
        crate::test_util::setup_log();
        assert_eq!(find_quick_file_type("test/test1.jpg"), QuickFileType::Media);
        assert_eq!(find_quick_file_type("test/test1.mp4"), QuickFileType::Media);
        assert_eq!(
            find_quick_file_type("test/test1.abc"),
            QuickFileType::Unknown
        );
        assert_eq!(
            find_quick_file_type("test/test1.csv"),
            QuickFileType::AlbumCsv
        );
        assert_eq!(
            find_quick_file_type("test/test1.CsV"),
            QuickFileType::AlbumCsv
        );
        assert_eq!(
            find_quick_file_type("test/metadata.json"),
            QuickFileType::AlbumJson
        );
        assert_eq!(
            find_quick_file_type("test/MeTaDaTa.JsOn"),
            QuickFileType::AlbumJson
        );
        assert_eq!(find_quick_file_type("test/tes"), QuickFileType::Unknown);
        assert_eq!(find_quick_file_type("test/te.s.jpg"), QuickFileType::Media);
        assert_eq!(find_quick_file_type("test/Hello.m4v"), QuickFileType::Media);
        assert_eq!(find_quick_file_type("test/Hello.M4V"), QuickFileType::Media);
        assert_eq!(find_quick_file_type("test/Hello.mov"), QuickFileType::Media);
        assert_eq!(find_quick_file_type("test/Hello.MOV"), QuickFileType::Media);
        assert_eq!(find_quick_file_type("test/Hello.avi"), QuickFileType::Media);
        assert_eq!(
            find_quick_file_type("test/MVI_0028.AVI"),
            QuickFileType::Media
        );
        assert_eq!(find_quick_file_type("test/Hello.mpg"), QuickFileType::Media);
        assert_eq!(find_quick_file_type("test/Hello.MPG"), QuickFileType::Media);
        assert_eq!(
            find_quick_file_type("test/Hello.mpeg"),
            QuickFileType::Media
        );
        assert_eq!(find_quick_file_type("test/Hello.wmv"), QuickFileType::Media);
        assert_eq!(find_quick_file_type("test/Hello.WMV"), QuickFileType::Media);
        assert_eq!(find_quick_file_type("test/Hello.asf"), QuickFileType::Media);
    }

    /// Takeout exports plenty of QuickTime files, often under a `.mp4` name, so
    /// the type comes from the `qt  ` brand in the bytes.
    #[test]
    fn test_mov_is_a_supported_video() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        use crate::fs::OsFileSystem;
        let name = "Hello.mov".to_string();
        let root = OsFileSystem::new("test");
        let ft = determine_file_type(root.open(&name)?, &name)?;

        assert_eq!(ft, AccurateFileType::Mov);
        assert_eq!(file_ext_from_file_type(&ft), "mov");
        assert_eq!(media_kind(&ft), Some("v"));
        assert!(matches!(metadata_type(&ft), MetadataType::Track));
        Ok(())
    }

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

    /// Falling through to `Unsupported` would keep an `.m4v`'s capture time out
    /// of the date logic entirely.
    #[test]
    fn test_m4v_is_a_supported_video() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        use crate::fs::OsFileSystem;
        let name = "Hello.m4v".to_string();
        let root = OsFileSystem::new("test");
        let ft = determine_file_type(root.open(&name)?, &name)?;

        assert_eq!(ft, AccurateFileType::M4v);
        assert_eq!(file_ext_from_file_type(&ft), "m4v");
        assert_eq!(media_kind(&ft), Some("v"));
        assert!(matches!(metadata_type(&ft), MetadataType::Track));
        Ok(())
    }

    /// A video, but RIFF rather than ISO base media, so it must report no track
    /// metadata rather than have the parser warn on every file.
    #[test]
    fn test_avi_is_a_supported_video() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        use crate::fs::OsFileSystem;
        let name = "Hello.avi".to_string();
        let root = OsFileSystem::new("test");
        let ft = determine_file_type(root.open(&name)?, &name)?;

        assert_eq!(ft, AccurateFileType::Avi);
        assert_eq!(file_ext_from_file_type(&ft), "avi");
        assert_eq!(media_kind(&ft), Some("v"));
        assert!(matches!(metadata_type(&ft), MetadataType::NoMetadata));
        Ok(())
    }

    /// Identified by a start-code prefix rather than a container brand, and like
    /// AVI carrying no track metadata.
    #[test]
    fn test_mpg_is_a_supported_video() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        use crate::fs::OsFileSystem;
        let name = "Hello.mpg".to_string();
        let root = OsFileSystem::new("test");
        let ft = determine_file_type(root.open(&name)?, &name)?;

        assert_eq!(ft, AccurateFileType::Mpg);
        assert_eq!(file_ext_from_file_type(&ft), "mpg");
        assert_eq!(media_kind(&ft), Some("v"));
        assert!(matches!(metadata_type(&ft), MetadataType::NoMetadata));
        Ok(())
    }

    /// So one format does not produce two extensions in the archive.
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

    /// Sniffing has to reach past the ASF container to the stream list, since the
    /// same bytes hold `.wma` audio.
    #[test]
    fn test_wmv_is_a_supported_video() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        use crate::fs::OsFileSystem;
        let name = "Hello.wmv".to_string();
        let root = OsFileSystem::new("test");
        let ft = determine_file_type(root.open(&name)?, &name)?;

        assert_eq!(ft, AccurateFileType::Wmv);
        assert_eq!(file_ext_from_file_type(&ft), "wmv");
        assert_eq!(media_kind(&ft), Some("v"));
        assert!(matches!(metadata_type(&ft), MetadataType::NoMetadata));
        Ok(())
    }

    /// The other side of that coin: rejected, or a music file lands in the archive
    /// as a video with nothing to show.
    #[test]
    fn test_wma_audio_is_not_a_video() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        use crate::fs::OsFileSystem;
        let root = OsFileSystem::new("test");
        let name = "Hello.wma".to_string();
        assert_eq!(
            determine_file_type(root.open(&name)?, &name)?,
            AccurateFileType::Unsupported
        );
        // Even renamed to a video extension: the bytes decide, not the name.
        assert_eq!(
            determine_file_type(root.open(&name)?, &"Hello.wmv".to_string())?,
            AccurateFileType::Unsupported
        );
        Ok(())
    }

    /// `.asf` is accepted on the way in, since plenty of Windows Media videos are
    /// named that way, then normalised like `.mpeg`.
    #[test]
    fn test_asf_extension_normalises_to_wmv() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        use crate::fs::OsFileSystem;
        let root = OsFileSystem::new("test");
        let ft = determine_file_type(root.open("Hello.wmv")?, &"clip.asf".to_string())?;

        assert_eq!(ft, AccurateFileType::Wmv);
        assert_eq!(file_ext_from_file_type(&ft), "wmv");
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
        // Nor the audio and stream-less halves of ASF, which share the container
        // with WMV but hold no picture.
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
