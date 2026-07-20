//! Classification of the dirs/files in a google takeout or icloud directory/zip.
//!
//! Naming is pretty loose, especially in google takeout. This module uses the strictest possible regex to identify
//! dirs/files that match known patterns. The classification is consumed by the
//! `db` command, which stores the result for every scanned path.
//!
//! Open questions:
//!  - How do we relate albums to corresponding photos/videos?
//!  - How do we relate photos/videos to separate corresponding metadata files?
//!  - Do dirs change names for other languages? eg, es:fotos zh:照片?
//!  - Fo file prefixes/suffixes change for other languages? eg, is `image_001.jpg` different in ES?
//!
//! Out of scope:
//!  - relate edits/animations/originals together
//!    - this requires too much knowledge of icloud and google takeout structure

use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;
use strum_macros::Display;
use tracing::warn;

/// Classify a single file by its path. Returns the first matching pattern, or
/// `None` if the file does not match any known pattern.
pub(crate) fn classify_file(file_path: &str) -> Option<KnownFileType> {
    find_known_files(file_path).into_iter().next()
}

/// Classify a single directory by its path. Returns the first matching pattern,
/// or `None` if the directory does not match any known pattern.
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
    /// The captured value (e.g. the year for `GpPhotosFromYear`), if the variant
    /// carries one. Stored alongside the variant name in the database.
    pub(crate) fn value(&self) -> Option<String> {
        match self {
            KnownDir::GpPhotosFromYear(v) => Some(v.clone()),
            _ => None,
        }
    }
}

#[derive(Debug, Display, PartialEq)]
pub(crate) enum KnownFileType {
    // can be in either provider
    Photo(String),
    Ignored, // any file where we know it's file pattern and we know we don't need it

    // typically in google photos
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

    // typically in icloud photos
    IcpAlbumCsv(String),
    IcpSharedAlbumsZip,
}

impl KnownFileType {
    /// The captured value (e.g. the photo id) if the variant carries one.
    /// Stored alongside the variant name in the database.
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
    //debug!("haystack: {haystack_lc} needle: {re}");
    let caps_o = re.captures(&haystack_lc);
    if let Some(caps) = caps_o {
        //debug!("Matched: {caps:?}");
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

fn make_file_patterns() -> Vec<(Vec<Regex>, MatchingFilePatternFn)> {
    let patterns: Vec<(&[&str], MatchingFilePatternFn)> = vec![
        (
            &[
                r"^img_([\d_]+)\.(heic|jpg|jpeg|mov|png)$",
                r"^([\d_]+)\.(heic|jpg|jpeg|mov|png)$",
                r"^img_([\d_]+)-edited\.(heic|jpg|jpeg|mov|png)$",
                r"^image_([\d_]+)\.(heic|jpg|jpeg|mov|png)$",
            ],
            |m| KnownFileType::Photo(m.g1),
        ),
        (
            &[
                r"^([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\.(heic|jpg|jpeg|mov|png)$",
                r"^([0-9]{11}__[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{9})\.(heic|jpg|jpeg|mov|png)$",
                r"^image_([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\.(heic|jpg|jpeg|mov|png)$",
            ],
            |m| KnownFileType::PhotoWithGuid(m.g1),
        ),
        (
            &[
                r"^image_([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\.(heic|jpg|jpeg|mov|png)\.json$",
                r"^(.+)\.(heic|jpg|jpeg|mov|png|gif)\.suppl\.json$",
                r"^(.+)\.(heic|jpg|jpeg|mov|png|gif)\.supplemental-meta\.json$",
                r"^(.+)\.(heic|jpg|jpeg|mov|png|gif)\.supplemental-metadata\([0-9]+\)\.json$",
                r"^(.+)\.(heic|jpg|jpeg|mov|png|gif)\.supplemental-metadata.json$",
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
        (&[r"^\.ds_store$"], |_| KnownFileType::Ignored),
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
}
