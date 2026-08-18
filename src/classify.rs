//! Classification of the dirs/files in a Google Takeout or iCloud directory/zip,
//! consumed by the `db` command, which stores the result for every scanned path.
//!
//! Naming is loose, especially in Takeout, so the patterns here are the strictest
//! regexes that still match.
//!
//! Open questions:
//!  - How do we relate albums to corresponding photos/videos?
//!  - How do we relate photos/videos to separate corresponding metadata files?
//!  - Do dir names change for other languages? eg, es:fotos zh:照片?
//!  - Do file prefixes/suffixes? eg, is `image_001.jpg` different in ES?
//!
//! Relating edits/animations/originals together is out of scope; it needs too
//! much knowledge of iCloud and Takeout structure.

use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;
use strum_macros::Display;
use tracing::warn;

/// The first matching pattern, or `None` if none is known.
pub(crate) fn classify_file(file_path: &str) -> Option<KnownFileType> {
    find_known_files(file_path).into_iter().next()
}

/// The first matching pattern, or `None` if none is known.
pub(crate) fn classify_dir(dir_path: &str) -> Option<KnownDir> {
    find_known_dirs(dir_path).into_iter().next()
}

#[derive(Debug, Display)]
pub(crate) enum KnownDir {
    GpPhotosFromYear(String),
    GpArchive,
    GpBin,

    IcpPhotos,
    IcpAlbums,
    IcpMemories,
    IcpRecentlyDeleted,
}

impl KnownDir {
    /// The captured value, stored in the database alongside the variant name.
    pub(crate) fn value(&self) -> Option<String> {
        match self {
            KnownDir::GpPhotosFromYear(v) => Some(v.clone()),
            _ => None,
        }
    }
}

#[derive(Debug, Display, PartialEq)]
pub(crate) enum KnownFileType {
    // Either provider.
    Photo(String),
    /// A recognised pattern that is deliberately not wanted.
    Ignored,

    // Typically Google Photos.
    GpMetadataJson(String),
    GpPicasaSyncMetadataJson(String),
    GpAlbumJson,
    PhotoWithGuid(String),
    GpCollage(String),
    GpAnimation(String),
    GpPrintSubscription,
    GpSharedAlbumComments,
    GpUserGeneratedMemoryTitles,
    GpArchiveBrowser,

    // Typically iCloud Photos.
    IcpAlbumCsv(String),
    IcpSharedAlbumsZip,
}

impl KnownFileType {
    /// The captured value, stored in the database alongside the variant name.
    pub(crate) fn value(&self) -> Option<String> {
        match self {
            KnownFileType::Photo(v)
            | KnownFileType::GpMetadataJson(v)
            | KnownFileType::GpPicasaSyncMetadataJson(v)
            | KnownFileType::PhotoWithGuid(v)
            | KnownFileType::GpCollage(v)
            | KnownFileType::GpAnimation(v)
            | KnownFileType::IcpAlbumCsv(v) => Some(v.clone()),
            _ => None,
        }
    }
}

fn match_re(haystack: &str, re: &Regex) -> Option<PatternMatch> {
    let haystack_lc = haystack.to_lowercase();
    let caps_o = re.captures(&haystack_lc);
    if let Some(caps) = caps_o {
        return Some(PatternMatch {
            g1: caps
                .get(1)
                .map_or("".to_string(), |m| m.as_str().to_string()),
        });
    }
    None
}

struct PatternMatch {
    g1: String,
}

/// The media extensions the filename patterns accept, as a regex alternation
/// substituted in for `{ext}`. Kept in one place because it drifted: the photo
/// patterns listed only stills and `mov`, so every `.mp4`, `.avi` and `.mpg` —
/// including Canon's `MVI_*.AVI` — came out of a scan unclassified.
const MEDIA_EXT: &str = concat!(
    "heic|heif|avif|jpg|jpeg|jfif|jpe|png|gif|webp|tif|tiff|bmp",
    "|cr2|cr3|nef|arw|orf|raf|rw2",
    "|mov|mp4|m4v|avi|mpg|mpeg|wmv|asf|mts|m2ts|3gp|3g2|mkv|webm"
);

fn make_file_patterns() -> Vec<(Vec<Regex>, MatchingFilePatternFn)> {
    let patterns: Vec<(&[&str], MatchingFilePatternFn)> = vec![
        (
            &[
                r"^img_([\d_]+)\.({ext})$",
                r"^([\d_]+)\.({ext})$",
                r"^img_([\d_]+)-edited\.({ext})$",
                r"^image_([\d_]+)\.({ext})$",
                // Canon names stills `IMG_1234.JPG` but videos `MVI_1234.AVI`.
                r"^mvi_([\d_]+)\.({ext})$",
                r"^mvi_([\d_]+)-edited\.({ext})$",
            ],
            |m| KnownFileType::Photo(m.g1),
        ),
        (
            &[
                r"^([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\.({ext})$",
                r"^([0-9]{11}__[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{9})\.({ext})$",
                r"^image_([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\.({ext})$",
            ],
            |m| KnownFileType::PhotoWithGuid(m.g1),
        ),
        (
            &[
                r"^image_([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\.({ext})\.json$",
                r"^(.+)\.({ext})\.suppl\.json$",
                r"^(.+)\.({ext})\.supplemental-meta\.json$",
                r"^(.+)\.({ext})\.supplemental-metadata\([0-9]+\)\.json$",
                r"^(.+)\.({ext})\.supplemental-metadata.json$",
            ],
            |m| KnownFileType::GpMetadataJson(m.g1),
        ),
        (
            &[r"^picasasync\.supplemental-metadata\([0-9]+\).json$"],
            |m| KnownFileType::GpPicasaSyncMetadataJson(m.g1),
        ),
        (&[r"^shared_album_comments.json$"], |_| {
            KnownFileType::GpSharedAlbumComments
        }),
        (&[r"^archive_browser.html$"], |_| {
            KnownFileType::GpArchiveBrowser
        }),
        (&[r"^user-generated-memory-titles.json$"], |_| {
            KnownFileType::GpUserGeneratedMemoryTitles
        }),
        (
            &[r"^([\d_]+)-animation.gif$", r"^img_([\d_]+)-animation.gif$"],
            |m| KnownFileType::GpAnimation(m.g1),
        ),
        (&[r"^([\d_]+)-collage.jpg$"], |m| {
            KnownFileType::GpCollage(m.g1)
        }),
        (&[r"^print-subscriptions.json$"], |_| {
            KnownFileType::GpPrintSubscription
        }),
        (&[r"^metadata.json$"], |_| KnownFileType::GpAlbumJson),
        (&[r"^(.+)\.csv$"], |m| KnownFileType::IcpAlbumCsv(m.g1)),
        (&[r"^icloud shared albums.zip$"], |_| {
            KnownFileType::IcpSharedAlbumsZip
        }),
        (&[r"^\.ds_store$", r"^.+\.thm$"], |_| KnownFileType::Ignored),
    ];
    patterns
        .iter()
        .map(|(patterns, match_fn)| {
            let mut regexes: Vec<Regex> = vec![];
            for p in patterns.iter() {
                match Regex::new(&p.replace("{ext}", MEDIA_EXT)) {
                    Ok(re) => regexes.push(re),
                    Err(re_err) => {
                        warn!("Error while parsing: {re_err}");
                    }
                }
            }
            (regexes, *match_fn)
        })
        .collect::<Vec<(Vec<Regex>, MatchingFilePatternFn)>>()
}

fn make_dir_patterns() -> Vec<(Vec<Regex>, MatchingDirPatternFn)> {
    let patterns: Vec<(&[&str], MatchingDirPatternFn)> = vec![
        (&[r"^google photos/photos from (\d{4})$"], |m| {
            KnownDir::GpPhotosFromYear(m.g1)
        }),
        (&[r"^photos$"], |_| KnownDir::IcpPhotos),
        (&[r"^albums$"], |_| KnownDir::IcpAlbums),
        (&[r"^memories$"], |_| KnownDir::IcpMemories),
        (&[r"^archive"], |_| KnownDir::GpArchive),
        (&[r"^bin"], |_| KnownDir::GpBin),
        (&[r"^memories/(.+)$"], |_| KnownDir::IcpMemories),
        (&[r"^recently deleted"], |_| KnownDir::IcpRecentlyDeleted),
    ];
    patterns
        .iter()
        .map(|(patterns, match_fn)| {
            let mut regexes: Vec<Regex> = vec![];
            for p in patterns.iter() {
                match Regex::new(p) {
                    Ok(re) => regexes.push(re),
                    Err(re_err) => {
                        warn!("Error while parsing: {re_err}");
                    }
                }
            }
            (regexes, *match_fn)
        })
        .collect::<Vec<(Vec<Regex>, MatchingDirPatternFn)>>()
}

type MatchingFilePatternFn = fn(PatternMatch) -> KnownFileType;
type MatchingDirPatternFn = fn(PatternMatch) -> KnownDir;

static FILE_PATTERNS: LazyLock<Vec<(Vec<Regex>, MatchingFilePatternFn)>> =
    LazyLock::new(make_file_patterns);
static DIR_PATTERNS: LazyLock<Vec<(Vec<Regex>, MatchingDirPatternFn)>> =
    LazyLock::new(make_dir_patterns);

fn find_known_files(file_path: &str) -> Vec<KnownFileType> {
    let p = Path::new(file_path);
    match p.file_name() {
        None => {
            vec![]
        }
        Some(file_name) => match file_name.to_str() {
            None => {
                vec![]
            }
            Some(fn2) => {
                let known_files = FILE_PATTERNS
                    .iter()
                    .flat_map(|(patterns, match_fn)| {
                        let mut matches = vec![];
                        for p in patterns.iter() {
                            if let Some(matched) = match_re(fn2, p) {
                                matches.push(match_fn(matched))
                            }
                        }
                        matches
                    })
                    .collect::<Vec<KnownFileType>>();
                if known_files.len() > 1 {
                    warn!(
                        "File {fn2} had {} matches, this indicated overlapping regexes",
                        known_files.len()
                    )
                }
                known_files
            }
        },
    }
}

fn find_known_dirs(dir_path: &str) -> Vec<KnownDir> {
    let known_dirs = DIR_PATTERNS
        .iter()
        .flat_map(|(patterns, match_fn)| {
            let mut matches = vec![];
            for p in patterns.iter() {
                if let Some(matched) = match_re(dir_path, p) {
                    matches.push(match_fn(matched))
                }
            }
            matches
        })
        .collect::<Vec<KnownDir>>();
    if known_dirs.len() > 1 {
        warn!(
            "File {dir_path} had {} matches, this indicated overlapping regexes",
            known_dirs.len()
        )
    }
    known_dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_match() {
        crate::test_util::setup_log();
        assert_eq!(find_known_files("/hello"), vec![]);
        assert_eq!(
            find_known_files("Google Photos/Photos from 2012/IMG_1234.jpg"),
            vec![KnownFileType::Photo(String::from("1234"))]
        );
        assert_eq!(
            find_known_files("Google Photos/2016-book/IMG_1316.JPG.supplemental-metadata.json"),
            vec![KnownFileType::GpMetadataJson(String::from("img_1316"))]
        );
    }

    /// The photo patterns listed only stills and `mov`, so a scan left every
    /// `.mp4`, `.avi` and `.mpg` unclassified — Canon's `MVI_*.AVI` doubly so,
    /// since its prefix was missing too. One match each, or the regexes overlap.
    #[test]
    fn test_videos_are_classified() {
        crate::test_util::setup_log();
        for (path, expected) in [
            ("Photos from 2012/IMG_1234.MP4", "1234"),
            ("Photos from 2012/IMG_1234.m4v", "1234"),
            ("Photos from 2009/IMG_0007.mpg", "0007"),
            ("Photos from 2001/MVI_0028.AVI", "0028"),
            ("Photos from 2001/MVI_0063.avi", "0063"),
            ("Photos from 2005/100_0012.mpeg", "100_0012"),
            // Added alongside the videos, and equally absent before.
            ("Photos from 2019/IMG_1234.webp", "1234"),
            ("Photos from 2019/IMG_1234.heif", "1234"),
            ("Photos from 2019/IMG_1234.avif", "1234"),
            // Raw, and the scanner format that shares its container shape.
            ("Photos from 2008/IMG_1234.CR2", "1234"),
            ("Photos from 2015/IMG_1234.cr3", "1234"),
            ("Photos from 2008/100_1234.nef", "100_1234"),
            ("Scans/IMG_1234.tif", "1234"),
            // The AVCHD and camera-phone video eras.
            ("Photos from 2010/IMG_1234.mts", "1234"),
            ("Photos from 2006/IMG_1234.3gp", "1234"),
            ("Photos from 2016/IMG_1234.mkv", "1234"),
        ] {
            assert_eq!(
                find_known_files(path),
                vec![KnownFileType::Photo(String::from(expected))],
                "classifying {path}"
            );
        }
    }

    /// Takeout writes a sidecar for a video just as it does for a still.
    #[test]
    fn test_video_supplemental_metadata() {
        crate::test_util::setup_log();
        assert_eq!(
            find_known_files("Google Photos/2016/IMG_1316.MP4.supplemental-metadata.json"),
            vec![KnownFileType::GpMetadataJson(String::from("img_1316"))]
        );
    }

    /// A `.thm` holds JPEG bytes, but it is Canon's sidecar thumbnail of the
    /// clip beside it rather than a picture of its own, so it is not wanted.
    #[test]
    fn test_thm_is_ignored() {
        crate::test_util::setup_log();
        for path in [
            "photo/2001/dover/MVI_0028.THM",
            "photo/2001/dover/MVI_0063.thm",
        ] {
            assert_eq!(
                find_known_files(path),
                vec![KnownFileType::Ignored],
                "classifying {path}"
            );
        }
    }
}
