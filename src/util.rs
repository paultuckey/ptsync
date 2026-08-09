use crate::file_type::{QuickFileType, find_quick_file_type};
use crate::fs::FileSystem;
use anyhow::Result;
use chrono::{DateTime, FixedOffset, Local, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::str::FromStr;
use tracing::{debug, warn};
use unicode_normalization::UnicodeNormalization;

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all(serialize = "camelCase"))]
pub(crate) struct HashInfo {
    pub(crate) short_checksum: String,
    pub(crate) long_checksum: String,
}

/// A short and long sha256 of the bytes, in the style of git.
pub(crate) fn checksum_bytes<R: Read + Seek>(reader: &mut R) -> Result<HashInfo> {
    let mut hasher = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    reader.seek(SeekFrom::Start(0))?;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let digest = hasher.finalize();
    let hex = hex::encode(digest);
    let chars = hex.chars();
    Ok(HashInfo {
        short_checksum: chars.clone().take(7).collect(),
        long_checksum: chars.take(64).collect(),
    })
}

#[derive(Debug, Clone)]
pub(crate) struct ScanInfo {
    pub(crate) file_path: String,
    /// Unix milliseconds.
    pub(crate) modified_datetime: Option<i64>,
    /// Unix milliseconds.
    pub(crate) created_datetime: Option<i64>,
    pub(crate) file_size: u64,
    pub(crate) quick_file_type: QuickFileType,
}

impl ScanInfo {
    pub(crate) fn new(
        file_path: String,
        modified_datetime: Option<i64>,
        created_datetime: Option<i64>,
        file_size: u64,
    ) -> Self {
        let quick_file_type = find_quick_file_type(&file_path);
        ScanInfo {
            file_path,
            modified_datetime,
            created_datetime,
            file_size,
            quick_file_type,
        }
    }
}

pub(crate) fn scan_fs(fs: &dyn FileSystem) -> Vec<ScanInfo> {
    let paths = fs.walk();
    let mut scan_infos = Vec::new();
    for path in paths {
        let meta = fs.metadata(&path).ok();
        let (mod_dt, create_dt, len) = match meta {
            Some(m) => (m.modified, m.created, m.len),
            None => (None, None, 0),
        };
        scan_infos.push(ScanInfo::new(path, mod_dt, create_dt, len));
    }
    scan_infos
}

pub(crate) fn is_existing_file_same(
    fs: &dyn FileSystem,
    long_checksum: &str,
    output_path: &String,
) -> Option<bool> {
    // Fast path: a backend that can report the stored checksum without reading
    // the object body (e.g. S3's native checksum via HeadObject) answers the
    // "same content?" question directly - no download, no re-hash.
    if let Some(recorded) = fs.recorded_checksum(output_path) {
        return Some(recorded == long_checksum);
    }
    let Ok(mut reader) = fs.open(output_path) else {
        debug!("Could not read file bytes for checksum: {output_path:?}");
        return None;
    };
    let existing_file_hash_info_r = checksum_bytes(&mut reader);
    let Ok(existing_file_hash_info) = existing_file_hash_info_r else {
        debug!("Could not read file for checksum: {output_path:?}");
        return None;
    };
    Some(existing_file_hash_info.long_checksum.eq(long_checksum))
}

pub(crate) fn dir_part(file_path_s: &String) -> String {
    let file_path = Path::new(&file_path_s);
    let Some(parent_path) = file_path.parent() else {
        warn!("No parent directory for file path: {file_path_s:?}");
        return "@@broken".to_string();
    };
    parent_path.to_string_lossy().to_string()
}

pub(crate) fn name_part(file_path_s: &String) -> String {
    let file_path = Path::new(&file_path_s);

    let Some(file_name_str) = file_path.file_name() else {
        warn!("No file name for file path: {file_path_s:?}");
        return "@@broken".to_string();
    };
    file_name_str.to_string_lossy().to_string()
}

/// Name of the environment variable consulted when `--timezone` is not given.
pub(crate) const TZ_ENV: &str = "TZ";

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum OutputTZ {
    Machine,
    Fixed(FixedOffset),
}

impl OutputTZ {
    /// The zone for this run: `--timezone` if given, else a UTC offset in
    /// [`TZ_ENV`], else the machine's own.
    ///
    /// A flag exists at all because `TZ` is a Unix convention: on Windows chrono's
    /// [`Local`] reads the zone from the Win32 API and never looks at `TZ`, so
    /// there would otherwise be no way to ask for a particular zone.
    pub(crate) fn resolve(arg: Option<&str>) -> Result<Self> {
        Self::resolve_with(arg, std::env::var(TZ_ENV).ok().as_deref())
    }

    /// The environment is passed in rather than read here so the precedence rules
    /// can be tested without touching the real one — tests share a process, so a
    /// test that set `TZ` would leak into whichever ran alongside it.
    fn resolve_with(arg: Option<&str>, tz_env: Option<&str>) -> Result<Self> {
        // A malformed flag is an error rather than a warning-and-carry-on: falling
        // back to the machine's zone would file the whole archive somewhere other
        // than asked for.
        //
        // chrono does the parsing, so the forms are `±HH:MM` and `±HHMM`, `UTC` is
        // spelled `+00:00`, and `+12:00junk` is twelve hours east because it reads
        // an offset off the front and ignores the rest.
        if let Some(raw) = arg.map(str::trim).filter(|raw| !raw.is_empty()) {
            let offset = FixedOffset::from_str(raw).map_err(|_| {
                anyhow::anyhow!(
                    "argument {raw:?} is not a UTC offset. Expected forms like \
                     \"+12:00\", \"-04:00\", \"+0545\" or \"+00:00\"."
                )
            })?;
            return Ok(OutputTZ::Fixed(offset));
        }
        // `TZ` usually holds an IANA name or a POSIX rule, neither of which is an
        // offset. Not an error: on Unix [`Local`] resolves those itself, DST and
        // all, so the machine zone already *is* what `TZ` asked for.
        match tz_env.map(str::trim).filter(|raw| !raw.is_empty()) {
            Some(raw) => match FixedOffset::from_str(raw) {
                Ok(offset) => Ok(OutputTZ::Fixed(offset)),
                Err(_) => {
                    debug!("{TZ_ENV}={raw:?} is not a UTC offset, using the machine's zone");
                    Ok(OutputTZ::Machine)
                }
            },
            None => Ok(OutputTZ::Machine),
        }
    }

    /// Render an instant given as epoch milliseconds. `None` when it is out of
    /// chrono's range, so an absurd timestamp becomes no date at all rather than a
    /// plausible-looking wrong one.
    pub(crate) fn render_millis(&self, ts_millis: i64) -> Option<String> {
        Some(self.render(DateTime::from_timestamp_millis(ts_millis)?))
    }

    pub(crate) fn render(&self, instant: DateTime<Utc>) -> String {
        match self {
            OutputTZ::Machine => instant.with_timezone(&Local).to_rfc3339(),
            OutputTZ::Fixed(offset) => instant.with_timezone(offset).to_rfc3339(),
        }
    }
}

/// Pair up a latitude/longitude, treating the `(0, 0)` "null island" sentinel as
/// absent — EXIF and Takeout both emit zeros when they have no fix.
pub(crate) fn non_zero_coords(lat: Option<f64>, long: Option<f64>) -> Option<(f64, f64)> {
    match (lat, long) {
        (Some(lat), Some(long)) if lat != 0.0 || long != 0.0 => Some((lat, long)),
        _ => None,
    }
}

/// Standard geohash base-32 alphabet (omits a, i, l, o).
const GEOHASH_BASE32: &[u8] = b"0123456789bcdefghjkmnpqrstuvwxyz";

/// Preserves full source precision while still allowing coarser clustering by
/// prefix.
pub(crate) const GEOHASH_PRECISION: usize = 12;

/// Encode a latitude/longitude as `precision` base-32 characters. Nearby points
/// share a prefix, so `geohash LIKE 'gcpv%'` clusters photos by location without
/// any geocoding.
pub(crate) fn geohash_encode(lat: f64, lon: f64, precision: usize) -> String {
    let mut lat_range = (-90.0f64, 90.0f64);
    let mut lon_range = (-180.0f64, 180.0f64);
    let mut hash = String::with_capacity(precision);
    let mut even = true; // longitude is encoded first
    let mut bit = 0u8;
    let mut idx = 0usize;
    while hash.len() < precision {
        let (range, value) = if even {
            (&mut lon_range, lon)
        } else {
            (&mut lat_range, lat)
        };
        let mid = (range.0 + range.1) / 2.0;
        if value >= mid {
            idx |= 1 << (4 - bit);
            range.0 = mid;
        } else {
            range.1 = mid;
        }
        even = !even;
        if bit < 4 {
            bit += 1;
        } else {
            hash.push(GEOHASH_BASE32[idx] as char);
            bit = 0;
            idx = 0;
        }
    }
    hash
}

/// `portrait` / `landscape` / `square` from pixel dimensions, when both known.
pub(crate) fn orientation(width: Option<i64>, height: Option<i64>) -> Option<&'static str> {
    match (width, height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => Some(if w > h {
            "landscape"
        } else if h > w {
            "portrait"
        } else {
            "square"
        }),
        _ => None,
    }
}

/// First 16 hex chars of the SHA-256 — the same on any machine or run.
fn stable_hash16(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    hex::encode(hasher.finalize()).chars().take(16).collect()
}

/// Stable identifier for a person. Lowercasing and NFC normalisation mean the
/// same person resolves to the same id whatever the case, and whether the source
/// encoded accents precomposed or combining (macOS writes NFD).
pub(crate) fn person_id_for(name: &str) -> String {
    let lowered = name.trim().to_lowercase();
    let normalized: String = lowered.nfc().collect();
    stable_hash16(&normalized)
}

/// Stable id derived from a path. NFC-normalised but not lowercased, since paths
/// are case-sensitive.
fn path_id(path: &str) -> String {
    let normalized: String = path.trim().nfc().collect();
    stable_hash16(&normalized)
}

pub(crate) fn album_id_for(album_path: &str) -> String {
    path_id(album_path)
}

/// Keyed on the path rather than the content, so duplicate files (same bytes,
/// different paths) stay as distinct rows.
pub(crate) fn media_item_id_for(media_path: &str) -> String {
    path_id(media_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{FileMetadata, OsFileSystem, ReadSeek, ZipFileSystem};
    use anyhow::anyhow;

    /// Covers the precedence rules and the spellings the README and `--help`
    /// promise.
    #[test]
    fn test_output_tz_prefers_the_flag_over_the_environment() -> Result<()> {
        let fixed = |secs: i32| -> Result<OutputTZ> {
            Ok(OutputTZ::Fixed(
                FixedOffset::east_opt(secs).ok_or_else(|| anyhow!("bad offset"))?,
            ))
        };
        let hours = |h: i32| fixed(h * 3600);
        let resolve = OutputTZ::resolve_with;

        // Both documented forms, including the three-quarter-hour zone that makes
        // "just read the hours" wrong.
        assert_eq!(resolve(Some("+12:00"), None)?, hours(12)?);
        assert_eq!(resolve(Some("-04:00"), None)?, hours(-4)?);
        assert_eq!(resolve(Some("+0545"), None)?, fixed(5 * 3600 + 45 * 60)?);
        assert_eq!(resolve(Some("+00:00"), None)?, hours(0)?);
        assert_eq!(resolve(None, Some("+12:00"))?, hours(12)?);
        // The flag wins outright; it is not merely a default for an unset `TZ`.
        assert_eq!(resolve(Some("-04:00"), Some("+12:00"))?, hours(-4)?);

        assert_eq!(resolve(None, None)?, OutputTZ::Machine);
        assert_eq!(resolve(Some("  "), Some(""))?, OutputTZ::Machine);
        // An unparseable `TZ` is the normal case on Unix, where `Local` resolves
        // the name itself, so it must not be an error — nor shadow a good flag.
        assert_eq!(resolve(None, Some("Pacific/Auckland"))?, OutputTZ::Machine);
        assert_eq!(resolve(None, Some("EST5EDT"))?, OutputTZ::Machine);
        assert_eq!(resolve(Some("+12:00"), Some("EST5EDT"))?, hours(12)?);

        // The flag is a direct instruction, so a name it cannot read stops the run
        // rather than quietly filing the archive somewhere else.
        assert!(resolve(Some("Pacific/Auckland"), None).is_err());
        assert!(resolve(Some("UTC"), None).is_err());
        assert!(resolve(Some("Z"), None).is_err());

        // chrono's warts, inherited deliberately: an offset read off the front
        // with the rest ignored, and both fields required to be padded.
        assert_eq!(resolve(Some("+12:00junk"), None)?, hours(12)?);
        assert!(resolve(Some("+12"), None).is_err());
        Ok(())
    }

    /// The offset an instant renders at decides which `yyyy/mm/dd` directory the
    /// file lands in. Asserted against fixed offsets rather than `Local`, which
    /// would only prove the machine agrees with itself.
    #[test]
    fn test_instant_renders_at_the_offset_it_is_given() -> Result<()> {
        use chrono::FixedOffset;
        // 2024-05-22T00:17:51Z — the instant Google records for the test fixture.
        let ts = 1_716_337_071_000;
        let at = |hours: i32| -> Option<String> {
            OutputTZ::Fixed(FixedOffset::east_opt(hours * 3600)?).render_millis(ts)
        };

        // Auckland reads that instant as lunchtime on the 22nd, New York as the
        // evening of the 21st — a different directory.
        assert_eq!(at(12).as_deref(), Some("2024-05-22T12:17:51+12:00"));
        assert_eq!(at(-4).as_deref(), Some("2024-05-21T20:17:51-04:00"));
        assert_eq!(at(0).as_deref(), Some("2024-05-22T00:17:51+00:00"));

        // Whatever the offset, the instant underneath is untouched.
        for hours in [-11, -4, 0, 12, 13] {
            let rendered = at(hours).ok_or_else(|| anyhow!("no reading at {hours}h"))?;
            assert_eq!(
                chrono::DateTime::parse_from_rfc3339(&rendered)?.timestamp_millis(),
                ts,
                "rendering at {hours}h moved the instant"
            );
        }

        // Out-of-range milliseconds are no date at all, not a wrapped one.
        assert_eq!(
            OutputTZ::Fixed(FixedOffset::east_opt(0).ok_or_else(|| anyhow!("bad offset"))?)
                .render_millis(i64::MAX),
            None
        );
        Ok(())
    }

    /// Reports a checksum from "metadata" but fails every body read, so
    /// `is_existing_file_same` can only pass by using the recorded checksum.
    struct MetaOnlyFs {
        checksum: String,
    }

    impl FileSystem for MetaOnlyFs {
        fn open(&self, _path: &str) -> Result<Box<dyn ReadSeek>> {
            Err(anyhow!(
                "open() must not be called on the recorded-checksum fast path"
            ))
        }
        fn exists(&self, _path: &str) -> bool {
            true
        }
        fn walk(&self) -> Vec<String> {
            Vec::new()
        }
        fn metadata(&self, _path: &str) -> Result<FileMetadata> {
            Err(anyhow!("metadata not supported"))
        }
        fn recorded_checksum(&self, _path: &str) -> Option<String> {
            Some(self.checksum.clone())
        }
    }

    #[test]
    fn is_existing_file_same_uses_recorded_checksum_without_reading_body() {
        let fs = MetaOnlyFs {
            checksum: "abc123".to_string(),
        };
        assert_eq!(
            is_existing_file_same(&fs, "abc123", &"any/path".to_string()),
            Some(true)
        );
        assert_eq!(
            is_existing_file_same(&fs, "def456", &"any/path".to_string()),
            Some(false)
        );
    }

    #[test]
    fn test_zip() -> Result<()> {
        use anyhow::anyhow;
        crate::test_util::setup_log();
        let c = ZipFileSystem::new("test/Canon_40D.jpg.zip")?;
        let index = scan_fs(&c);
        assert_eq!(index.len(), 2);
        let si = index
            .iter()
            .find(|i| i.file_path == "Canon_40D.jpg")
            .ok_or_else(|| anyhow!("Canon_40D.jpg not found in zip"))?;
        // The fixture's 0x5455 extended-timestamp field records the mtime as
        // 2025-06-14T04:09:22Z, and that UTC instant is what is read rather than
        // the zoneless DOS time. It carries no creation time.
        assert_eq!(si.modified_datetime, Some(1749874162000));
        assert_eq!(si.created_datetime, None);
        Ok(())
    }

    #[test]
    fn test_geohash_encode() {
        // Canonical reference value from the geohash spec.
        assert_eq!(geohash_encode(42.6, -5.6, 5), "ezs42");
        let sydney = geohash_encode(-33.8688, 151.2093, 9);
        let sydney_near = geohash_encode(-33.8689, 151.2094, 9);
        let london = geohash_encode(51.5074, -0.1278, 9);
        assert_eq!(&sydney[..5], &sydney_near[..5]);
        assert_ne!(&sydney[..1], &london[..1]);
    }

    #[test]
    fn test_orientation() {
        assert_eq!(orientation(Some(854), Some(480)), Some("landscape"));
        assert_eq!(orientation(Some(480), Some(854)), Some("portrait"));
        assert_eq!(orientation(Some(500), Some(500)), Some("square"));
        assert_eq!(orientation(None, Some(480)), None);
        assert_eq!(orientation(Some(0), Some(480)), None);
    }

    #[test]
    fn test_person_id_stable_and_case_insensitive() {
        let a = person_id_for("Tim Tam");
        assert_eq!(a.len(), 16);
        assert_eq!(a, person_id_for("tim tam"));
        assert_eq!(a, person_id_for("  TIM TAM  "));
        assert_ne!(a, person_id_for("Ada Lovelace"));
    }

    #[test]
    fn test_person_id_non_ascii_and_normalization() {
        // Case folds in non-Latin scripts too.
        assert_eq!(person_id_for("Привет"), person_id_for("привет"));
        assert_eq!(person_id_for("ΑΘΗΝΑ"), person_id_for("αθηνα"));
        // Precomposed "é" (U+00E9) vs decomposed "e" + combining acute (U+0301).
        let precomposed = "Jos\u{00e9}";
        let decomposed = "Jose\u{0301}";
        assert_ne!(precomposed.as_bytes(), decomposed.as_bytes());
        assert_eq!(person_id_for(precomposed), person_id_for(decomposed));
    }

    /// Album and media ids share the one path-keyed derivation.
    #[test]
    fn test_path_ids_stable() {
        for id_for in [
            media_item_id_for as fn(&str) -> String,
            album_id_for as fn(&str) -> String,
        ] {
            let a = id_for("Google Photos/Holiday/IMG_0001.jpg");
            assert_eq!(a, id_for("  Google Photos/Holiday/IMG_0001.jpg  "));
            assert_ne!(a, id_for("Google Photos/Holiday/IMG_0002.jpg"));
            assert_ne!(id_for("a/IMG.jpg"), id_for("b/IMG.jpg"));
            // Paths are case-sensitive, unlike people.
            assert_ne!(a, id_for("google photos/holiday/img_0001.jpg"));
        }
    }

    #[test]
    fn test_files_checksum() -> Result<()> {
        let c = OsFileSystem::new("test");
        let mut b = c.open("Canon_40D.jpg")?;
        let csm = checksum_bytes(&mut b)?;
        assert_eq!(csm.short_checksum, "6bfdabd".to_string());
        assert_eq!(
            csm.long_checksum,
            "6bfdabd4fc33d112283c147acccc574e770bbe6fbdbc3d4da968ba7b606ecc2f".to_string()
        );
        Ok(())
    }
}
