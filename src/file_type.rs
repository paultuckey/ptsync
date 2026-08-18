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
        // Stills. `jfif` and `jpe` are JPEG under another name, which Windows
        // and Outlook both still hand out.
        "jpg" | "jpeg" | "jfif" | "jpe" | "png" | "gif" | "webp" | "heic" | "heif" | "avif"
        | "tif" | "tiff" | "bmp"
        // Camera raw. Every one of these is a container the sniffer knows by
        // signature, so the extension only decides whether to look.
        | "cr2" | "cr3" | "nef" | "arw" | "orf" | "raf" | "rw2"
        // Video.
        | "mp4" | "m4v" | "mov" | "avi" | "mpg" | "mpeg" | "wmv" | "asf" | "mts" | "m2ts"
        | "3gp" | "3g2" | "mkv" | "webm" => QuickFileType::Media,
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
    /// Google exports plenty of these, and a phone screenshot saved from the web
    /// is often one too.
    Webp,
    Avif,
    /// Also `.tiff`. What a flatbed scanner produces, so it is most of what a
    /// digitised pre-digital archive is made of.
    Tif,
    Bmp,
    /// Camera raw. Each vendor gets its own variant because the extension has
    /// to survive the round trip — a `.cr2` written back out as `.raw` would
    /// open in nothing.
    Cr2,
    Cr3,
    Nef,
    Arw,
    Orf,
    Raf,
    Rw2,
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
    /// MPEG-2 transport stream: AVCHD camcorder footage, named `.mts` or
    /// `.m2ts`. Both spellings share one media type, so both land here.
    Mts,
    /// Also `.3g2`. What a camera phone recorded before phones shot MP4.
    ThreeGpp,
    /// A Matroska container carrying video. Matroska holds `.mka` audio and
    /// `.mks` subtitles just as happily, so only the ones with a picture reach
    /// this type — the same split as ASF above.
    Mkv,
    Webm,
    Json,
    Csv,
    Unsupported,
}

pub(crate) fn file_ext_from_file_type(ff: &AccurateFileType) -> String {
    match ff {
        AccurateFileType::Jpg => "jpg".to_string(),
        AccurateFileType::Gif => "gif".to_string(),
        AccurateFileType::Webp => "webp".to_string(),
        AccurateFileType::Avif => "avif".to_string(),
        AccurateFileType::Tif => "tif".to_string(),
        AccurateFileType::Bmp => "bmp".to_string(),
        AccurateFileType::Cr2 => "cr2".to_string(),
        AccurateFileType::Cr3 => "cr3".to_string(),
        AccurateFileType::Nef => "nef".to_string(),
        AccurateFileType::Arw => "arw".to_string(),
        AccurateFileType::Orf => "orf".to_string(),
        AccurateFileType::Raf => "raf".to_string(),
        AccurateFileType::Rw2 => "rw2".to_string(),
        AccurateFileType::Mts => "mts".to_string(),
        AccurateFileType::ThreeGpp => "3gp".to_string(),
        AccurateFileType::Mkv => "mkv".to_string(),
        AccurateFileType::Webm => "webm".to_string(),
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
        | AccurateFileType::Gif
        | AccurateFileType::Webp
        | AccurateFileType::Avif
        | AccurateFileType::Tif
        | AccurateFileType::Bmp
        | AccurateFileType::Cr2
        | AccurateFileType::Cr3
        | AccurateFileType::Nef
        | AccurateFileType::Arw
        | AccurateFileType::Orf
        | AccurateFileType::Raf
        | AccurateFileType::Rw2 => Some("p"),
        AccurateFileType::Mp4
        | AccurateFileType::Mov
        | AccurateFileType::M4v
        | AccurateFileType::Avi
        | AccurateFileType::Mpg
        | AccurateFileType::Wmv
        | AccurateFileType::Mts
        | AccurateFileType::ThreeGpp
        | AccurateFileType::Mkv
        | AccurateFileType::Webm => Some("v"),
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
        | AccurateFileType::Gif
        | AccurateFileType::Avif
        // Raw is TIFF underneath, apart from RAF and CR3, and `nom-exif` reads
        // all three shapes — so raw arrives with the richest EXIF of anything
        // here: capture clock, lens, and usually GPS.
        | AccurateFileType::Tif
        | AccurateFileType::Cr2
        | AccurateFileType::Cr3
        | AccurateFileType::Nef
        | AccurateFileType::Arw
        | AccurateFileType::Orf
        | AccurateFileType::Raf
        | AccurateFileType::Rw2 => MetadataType::ExifTags,
        // A WebP does carry EXIF, in an `EXIF` RIFF chunk, but `nom-exif` reads
        // only JPEG/PNG/HEIF/TIFF/RAF/CR3 stills and returns nothing for this
        // container — so a WebP's capture time comes from supplemental metadata
        // or the filesystem until that support lands upstream.
        AccurateFileType::Webp => MetadataType::NoMetadata,
        AccurateFileType::Mp4
        | AccurateFileType::Mov
        | AccurateFileType::M4v
        | AccurateFileType::ThreeGpp
        | AccurateFileType::Mkv
        | AccurateFileType::Webm => MetadataType::Track,
        // Videos, but not ISO base media files, so the track parser cannot read
        // them — asking it to try only produces a warning per file. Their capture
        // time comes from supplemental metadata or the filesystem instead.
        // BMP has nowhere to put EXIF, and a transport stream is a muxed
        // broadcast format rather than a container with a header to read.
        AccurateFileType::Avi
        | AccurateFileType::Mpg
        | AccurateFileType::Wmv
        | AccurateFileType::Bmp
        | AccurateFileType::Mts => MetadataType::NoMetadata,
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
        // The same still image, branded `mif1`/`msf1` rather than `heic`. Left
        // unmapped these fall through to `Unsupported` and the photo is dropped.
        "image/heif" => AccurateFileType::Heic,
        "image/heic-sequence" => AccurateFileType::Heic,
        "image/heif-sequence" => AccurateFileType::Heic,
        "image/webp" => AccurateFileType::Webp,
        "image/avif" => AccurateFileType::Avif,
        "image/avif-sequence" => AccurateFileType::Avif,
        "image/tiff" => AccurateFileType::Tif,
        "image/bmp" => AccurateFileType::Bmp,
        "image/x-canon-cr2" => AccurateFileType::Cr2,
        "image/x-canon-cr3" => AccurateFileType::Cr3,
        "image/x-nikon-nef" => AccurateFileType::Nef,
        "image/x-sony-arw" => AccurateFileType::Arw,
        "image/x-olympus-orf" => AccurateFileType::Orf,
        "image/x-fuji-raf" => AccurateFileType::Raf,
        "image/x-panasonic-rw2" => AccurateFileType::Rw2,
        "video/mp4" => AccurateFileType::Mp4,
        "application/mp4" => AccurateFileType::Mp4,
        "video/quicktime" => AccurateFileType::Mov,
        "video/x-m4v" => AccurateFileType::M4v,
        "video/x-msvideo" => AccurateFileType::Avi,
        "video/mpeg" => AccurateFileType::Mpg,
        "video/x-ms-wmv" => AccurateFileType::Wmv,
        // `.mts` and `.m2ts` share this one media type, so the distinction
        // between AVCHD and its Blu-ray variant does not survive the string.
        "video/mp2t" => AccurateFileType::Mts,
        "video/3gpp" => AccurateFileType::ThreeGpp,
        "video/3gpp2" => AccurateFileType::ThreeGpp,
        "video/matroska" => AccurateFileType::Mkv,
        "video/webm" => AccurateFileType::Webm,
        // The other two ASF outcomes — `.wma` audio, and the bare container when
        // the header declares neither stream — would otherwise be filed as videos
        // with no picture.
        "audio/x-ms-wma" => AccurateFileType::Unsupported,
        // Matroska's audio and subtitle halves, which share the container with
        // a real `.mkv` exactly as `.wma` shares ASF with `.wmv`.
        "audio/matroska" => AccurateFileType::Unsupported,
        "application/x-matroska" => AccurateFileType::Unsupported,
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
            ("test/Hello.webp", QuickFileType::Media),
            ("test/Hello.WEBP", QuickFileType::Media),
            // The HEIF spelling of the same still image.
            ("test/Hello.heif", QuickFileType::Media),
            ("test/Hello.avif", QuickFileType::Media),
            ("test/Hello.tif", QuickFileType::Media),
            ("test/Hello.tiff", QuickFileType::Media),
            ("test/Hello.bmp", QuickFileType::Media),
            // JPEG under the names Windows and Outlook hand out.
            ("test/photo.jfif", QuickFileType::Media),
            ("test/photo.jpe", QuickFileType::Media),
            // Raw.
            ("test/IMG_0001.CR2", QuickFileType::Media),
            ("test/IMG_0001.cr3", QuickFileType::Media),
            ("test/DSC_0001.nef", QuickFileType::Media),
            ("test/DSC00001.arw", QuickFileType::Media),
            ("test/P1000001.orf", QuickFileType::Media),
            ("test/DSCF0001.raf", QuickFileType::Media),
            ("test/P1000001.rw2", QuickFileType::Media),
            // Video.
            ("test/Hello.mts", QuickFileType::Media),
            ("test/00000.m2ts", QuickFileType::Media),
            ("test/Hello.3gp", QuickFileType::Media),
            ("test/Hello.3g2", QuickFileType::Media),
            ("test/Hello.mkv", QuickFileType::Media),
            ("test/Hello.webm", QuickFileType::Media),
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
            // No metadata: the EXIF chunk is there in the bytes, but out of
            // reach of the parser — see `metadata_type`.
            (
                "Hello.webp",
                "Hello.webp",
                AccurateFileType::Webp,
                "webp",
                Some("p"),
                MetadataType::NoMetadata,
            ),
            // A scanner TIFF carries full EXIF, which is what makes it worth
            // more than the raw formats it shares a container shape with.
            (
                "Hello.tif",
                "Hello.tif",
                AccurateFileType::Tif,
                "tif",
                Some("p"),
                MetadataType::ExifTags,
            ),
            (
                "Hello.avif",
                "Hello.avif",
                AccurateFileType::Avif,
                "avif",
                Some("p"),
                MetadataType::ExifTags,
            ),
            // BMP has nowhere to put EXIF.
            (
                "Hello.bmp",
                "Hello.bmp",
                AccurateFileType::Bmp,
                "bmp",
                Some("p"),
                MetadataType::NoMetadata,
            ),
            (
                "Hello.3gp",
                "Hello.3gp",
                AccurateFileType::ThreeGpp,
                "3gp",
                Some("v"),
                MetadataType::Track,
            ),
            // Muxed broadcast format, so there is no container header to read.
            (
                "Hello.mts",
                "Hello.mts",
                AccurateFileType::Mts,
                "mts",
                Some("v"),
                MetadataType::NoMetadata,
            ),
            (
                "Hello.mkv",
                "Hello.mkv",
                AccurateFileType::Mkv,
                "mkv",
                Some("v"),
                MetadataType::Track,
            ),
            (
                "Hello.webm",
                "Hello.webm",
                AccurateFileType::Webm,
                "webm",
                Some("v"),
                MetadataType::Track,
            ),
            // The Matroska equivalent of `Hello.wma`: same container, audio
            // only, so it must not arrive as a video with nothing to show.
            (
                "Hello.mka",
                "Hello.mka",
                AccurateFileType::Unsupported,
                "bin",
                None,
                MetadataType::NoMetadata,
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

    /// A HEIF-branded still is the same picture as a `heic`-branded one, and a
    /// phone or Takeout will hand over either under a `.heic` name. Mapping only
    /// `image/heic` dropped the rest on the floor.
    #[test]
    fn test_heif_family_and_webp_content_types() {
        for (content_type, expected) in [
            ("image/heic", AccurateFileType::Heic),
            ("image/heif", AccurateFileType::Heic),
            ("image/heic-sequence", AccurateFileType::Heic),
            ("image/heif-sequence", AccurateFileType::Heic),
            ("image/webp", AccurateFileType::Webp),
        ] {
            assert_eq!(
                file_type_from_content_type(content_type),
                expected,
                "content type {content_type}"
            );
        }
    }

    /// Every media type this module matches on, pinned to the `FileFormat`
    /// variant that actually emits it. A typo in one of those strings is
    /// otherwise invisible — the arm simply never fires and the file is dropped
    /// as unsupported, which is exactly how `image/heif` went missing.
    #[test]
    fn test_content_type_strings_match_real_file_formats() {
        use file_format::FileFormat;
        for (fmt, expected) in [
            (
                FileFormat::JointPhotographicExpertsGroup,
                AccurateFileType::Jpg,
            ),
            (FileFormat::PortableNetworkGraphics, AccurateFileType::Png),
            (FileFormat::GraphicsInterchangeFormat, AccurateFileType::Gif),
            (FileFormat::Webp, AccurateFileType::Webp),
            (
                FileFormat::HighEfficiencyImageCoding,
                AccurateFileType::Heic,
            ),
            (
                FileFormat::HighEfficiencyImageFileFormat,
                AccurateFileType::Heic,
            ),
            (
                FileFormat::HighEfficiencyImageCodingSequence,
                AccurateFileType::Heic,
            ),
            (
                FileFormat::HighEfficiencyImageFileFormatSequence,
                AccurateFileType::Heic,
            ),
            (FileFormat::Av1ImageFileFormat, AccurateFileType::Avif),
            (
                FileFormat::Av1ImageFileFormatSequence,
                AccurateFileType::Avif,
            ),
            (FileFormat::TagImageFileFormat, AccurateFileType::Tif),
            (FileFormat::WindowsBitmap, AccurateFileType::Bmp),
            (FileFormat::CanonRaw2, AccurateFileType::Cr2),
            (FileFormat::CanonRaw3, AccurateFileType::Cr3),
            (FileFormat::NikonElectronicFile, AccurateFileType::Nef),
            (FileFormat::SonyAlphaRaw, AccurateFileType::Arw),
            (FileFormat::OlympusRawFormat, AccurateFileType::Orf),
            (FileFormat::FujifilmRaw, AccurateFileType::Raf),
            (FileFormat::PanasonicRaw, AccurateFileType::Rw2),
            (FileFormat::AppleQuicktime, AccurateFileType::Mov),
            (FileFormat::Mpeg4Part14Video, AccurateFileType::Mp4),
            (FileFormat::AppleItunesVideo, AccurateFileType::M4v),
            (FileFormat::AudioVideoInterleave, AccurateFileType::Avi),
            (FileFormat::Mpeg12Video, AccurateFileType::Mpg),
            (FileFormat::WindowsMediaVideo, AccurateFileType::Wmv),
            (FileFormat::Mpeg2TransportStream, AccurateFileType::Mts),
            (FileFormat::BdavMpeg2TransportStream, AccurateFileType::Mts),
            (
                FileFormat::ThirdGenerationPartnershipProject,
                AccurateFileType::ThreeGpp,
            ),
            (
                FileFormat::ThirdGenerationPartnershipProject2,
                AccurateFileType::ThreeGpp,
            ),
            (FileFormat::MatroskaVideo, AccurateFileType::Mkv),
            (FileFormat::Matroska3dVideo, AccurateFileType::Mkv),
            (FileFormat::Webm, AccurateFileType::Webm),
            // Each of these shares a container with a format that does hold a
            // picture, so they have to be turned away by media type — the
            // container alone cannot tell them apart.
            (FileFormat::MatroskaAudio, AccurateFileType::Unsupported),
            (FileFormat::MatroskaSubtitles, AccurateFileType::Unsupported),
            (FileFormat::WindowsMediaAudio, AccurateFileType::Unsupported),
            (
                FileFormat::AdvancedSystemsFormat,
                AccurateFileType::Unsupported,
            ),
        ] {
            assert_eq!(
                file_type_from_content_type(fmt.media_type()),
                expected,
                "{fmt:?} emits {}",
                fmt.media_type()
            );
        }
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
