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

    /// A label for [`MetadataType`], which is not comparable, so the table below
    /// can state which parser a format is handed to.
    fn metadata_label(ff: &AccurateFileType) -> &'static str {
        match metadata_type(ff) {
            MetadataType::ExifTags => "exif",
            MetadataType::Track => "track",
            MetadataType::NoMetadata => "none",
        }
    }

    /// Fixture, the name it is read under, expected type, archive extension,
    /// `kind` column, metadata parser.
    type FormatCase = (
        &'static str,
        &'static str,
        AccurateFileType,
        &'static str,
        Option<&'static str>,
        &'static str,
    );

    /// Every container the archive supports, sniffed from a real fixture. Each
    /// row also names the file it is read under, because a re-extension must not
    /// change the answer — Takeout is full of QuickTime under `.mp4`.
    #[test]
    fn test_media_formats_are_detected_from_content() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        use crate::fs::OsFileSystem;
        let root = OsFileSystem::new("test");

        let cases: Vec<FormatCase> = vec![
            (
                "Canon_40D.jpg",
                "Canon_40D.jpg",
                AccurateFileType::Jpg,
                "jpg",
                Some("p"),
                "exif",
            ),
            (
                "Hello.mp4",
                "Hello.mp4",
                AccurateFileType::Mp4,
                "mp4",
                Some("v"),
                "track",
            ),
            (
                "Hello.mov",
                "Hello.mov",
                AccurateFileType::Mov,
                "mov",
                Some("v"),
                "track",
            ),
            // Falling through to Unsupported would keep an .m4v's capture time
            // out of the date logic entirely.
            (
                "Hello.m4v",
                "Hello.m4v",
                AccurateFileType::M4v,
                "m4v",
                Some("v"),
                "track",
            ),
            // A video, but RIFF rather than ISO base media, so it must report no
            // track metadata rather than have the parser warn on every file.
            (
                "Hello.avi",
                "Hello.avi",
                AccurateFileType::Avi,
                "avi",
                Some("v"),
                "none",
            ),
            // Identified by a start-code prefix rather than a container brand.
            (
                "Hello.mpg",
                "Hello.mpg",
                AccurateFileType::Mpg,
                "mpg",
                Some("v"),
                "none",
            ),
            // Sniffing has to reach past the ASF container to the stream list,
            // since the same bytes hold `.wma` audio.
            (
                "Hello.wmv",
                "Hello.wmv",
                AccurateFileType::Wmv,
                "wmv",
                Some("v"),
                "none",
            ),
            // The other side of that coin: rejected, or a music file lands in the
            // archive as a video with nothing to show.
            (
                "Hello.wma",
                "Hello.wma",
                AccurateFileType::Unsupported,
                "bin",
                None,
                "none",
            ),
            // The name lies, in both directions.
            (
                "Hello.mov",
                "Hello.mp4",
                AccurateFileType::Mov,
                "mov",
                Some("v"),
                "track",
            ),
            (
                "Hello.mp4",
                "Hello.mov",
                AccurateFileType::Mp4,
                "mp4",
                Some("v"),
                "track",
            ),
            (
                "Hello.wma",
                "Hello.wmv",
                AccurateFileType::Unsupported,
                "bin",
                None,
                "none",
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
                "none",
            ),
            (
                "Hello.wmv",
                "clip.asf",
                AccurateFileType::Wmv,
                "wmv",
                Some("v"),
                "none",
            ),
        ];

        for (fixture, read_as, expected, ext, kind, metadata) in cases {
            let name = read_as.to_string();
            let ft = determine_file_type(root.open(fixture)?, &name)?;
            assert_eq!(ft, expected, "{fixture} read as {read_as}");
            assert_eq!(file_ext_from_file_type(&ft), ext, "{fixture} extension");
            assert_eq!(media_kind(&ft), kind, "{fixture} kind");
            assert_eq!(metadata_label(&ft), metadata, "{fixture} metadata");
        }
        Ok(())
    }

    /// The audio content types that share a container with a supported video and
    /// so have no fixture of their own — every other mapping is covered by
    /// sniffing a real file above.
    #[test]
    fn test_audio_content_types_are_not_videos() {
        assert_eq!(
            file_type_from_content_type("audio/mpeg"),
            AccurateFileType::Unsupported
        );
        // The stream-less half of ASF: the header declares neither audio nor
        // video, so it holds no picture either.
        assert_eq!(
            file_type_from_content_type("application/vnd.ms-asf"),
            AccurateFileType::Unsupported
        );
    }
}
