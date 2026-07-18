use crate::file_type::{QuickFileType, find_quick_file_type};
use crate::fs::FileSystem;
use anyhow::Result;
use chrono::DateTime;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use tracing::{debug, warn};
use unicode_normalization::UnicodeNormalization;

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all(serialize = "camelCase"))]
pub(crate) struct HashInfo {
    pub(crate) short_checksum: String,
    pub(crate) long_checksum: String,
}

/// Similar to github generate a short and long hash from the bytes
pub(crate) fn checksum_bytes<R: Read + Seek>(reader: &mut R) -> Result<HashInfo> {
    let mut hasher = Sha256::new();
    let mut buffer = [0; 64 * 1024]; // Read in 64KB chunks
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
    /// Unix Epoch time of last file modification
    pub(crate) modified_datetime: Option<i64>,
    /// Unix Epoch time file creation
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

pub(crate) fn timestamp_to_rfc3339(ts: i64) -> Option<String> {
    DateTime::from_timestamp_millis(ts).map(|d| d.to_rfc3339())
}

/// Pair up a latitude/longitude only when both are present and not the `(0, 0)`
/// "null island" sentinel. EXIF and Google Takeout both emit zeros when they
/// have no fix rather than omitting the value, so we treat that as absent.
pub(crate) fn non_zero_coords(lat: Option<f64>, long: Option<f64>) -> Option<(f64, f64)> {
    match (lat, long) {
        (Some(lat), Some(long)) if lat != 0.0 || long != 0.0 => Some((lat, long)),
        _ => None,
    }
}

/// Standard geohash base-32 alphabet (omits a, i, l, o).
const GEOHASH_BASE32: &[u8] = b"0123456789bcdefghjkmnpqrstuvwxyz";

/// Geohash length stored per item; 12 chars preserves full source precision
/// while still allowing coarser clustering via prefix matching.
pub(crate) const GEOHASH_PRECISION: usize = 12;

/// Encode a latitude/longitude as a geohash of `precision` base-32 characters.
/// Nearby points share a common prefix, so a `geohash LIKE 'gcpv%'` query
/// clusters photos by location without needing any geocoding.
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

/// SHA-256 (first 16 hex chars) of a string — a short, stable, content-derived
/// id that is the same on any machine or run.
fn stable_hash16(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    hex::encode(hasher.finalize()).chars().take(16).collect()
}

/// Stable identifier for a person from their name. The name is trimmed,
/// lowercased, and Unicode-normalized (NFC) before hashing, so the same person
/// resolves to the same id regardless of case or whether the source encoded
/// accents/scripts as precomposed or combining characters (e.g. macOS NFD vs
/// NFC). Works for any alphabet since `to_lowercase` is Unicode-aware and the
/// hash is over UTF-8 bytes.
pub(crate) fn person_id_for(name: &str) -> String {
    let lowered = name.trim().to_lowercase();
    let normalized: String = lowered.nfc().collect();
    stable_hash16(&normalized)
}

/// Stable id derived from a path: trimmed and Unicode-normalized (NFC) before
/// hashing. Paths are case-sensitive, so case is preserved. Reproducible across
/// machines, runs, and database rebuilds for the same archive layout.
fn path_id(path: &str) -> String {
    let normalized: String = path.trim().nfc().collect();
    stable_hash16(&normalized)
}

/// Stable identifier for an album from its path (see [`path_id`]).
pub(crate) fn album_id_for(album_path: &str) -> String {
    path_id(album_path)
}

/// Stable identifier for a media item from its path within the archive. Keyed on
/// the path, not the content, so duplicate files (same bytes, different paths)
/// stay as distinct rows (one row per file).
pub(crate) fn media_item_id_for(media_path: &str) -> String {
    path_id(media_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{FileMetadata, OsFileSystem, ReadSeek, ZipFileSystem};
    use anyhow::anyhow;

    /// A backend that reports a checksum from "metadata" but whose body read
    /// always fails. It proves `is_existing_file_same` decides from the recorded
    /// checksum alone and never touches the bytes - the S3 HeadObject fast path.
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
        // A matching recorded checksum resolves to "same" without ever calling
        // open() (which errors here); a mismatch resolves to "different".
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
        // Find Canon_40D.jpg
        let si = index
            .iter()
            .find(|i| i.file_path == "Canon_40D.jpg")
            .ok_or_else(|| anyhow!("Canon_40D.jpg not found in zip"))?;
        // The fixture's 0x5455 extended-timestamp field records the mtime as
        // 2025-06-14T04:09:22Z; we read that UTC instant, not the zoneless DOS
        // time. The field carries no creation time, so `created` stays absent.
        assert_eq!(si.modified_datetime, Some(1749874162000));
        assert_eq!(si.created_datetime, None);
        Ok(())
    }

    #[test]
    fn test_geohash_encode() {
        // Canonical reference value from the geohash spec.
        assert_eq!(geohash_encode(42.6, -5.6, 5), "ezs42");
        // Nearby points share a prefix; far-apart points do not.
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
        // Case and surrounding whitespace do not change the id.
        assert_eq!(a, person_id_for("tim tam"));
        assert_eq!(a, person_id_for("  TIM TAM  "));
        // Different names differ.
        assert_ne!(a, person_id_for("Ada Lovelace"));
    }

    #[test]
    fn test_person_id_non_ascii_and_normalization() {
        // Non-Latin scripts hash fine and fold case (Cyrillic, Greek).
        assert_eq!(person_id_for("Привет"), person_id_for("привет"));
        assert_eq!(person_id_for("ΑΘΗΝΑ"), person_id_for("αθηνα"));
        // Same name, different Unicode forms: precomposed "é" (U+00E9) vs
        // decomposed "e"+combining acute (U+0301) — must yield the same id.
        let precomposed = "Jos\u{00e9}";
        let decomposed = "Jose\u{0301}";
        assert_ne!(precomposed.as_bytes(), decomposed.as_bytes());
        assert_eq!(person_id_for(precomposed), person_id_for(decomposed));
    }

    #[test]
    fn test_media_item_id_stable() {
        let a = media_item_id_for("Google Photos/Holiday/IMG_0001.jpg");
        assert_eq!(a.len(), 16);
        // Deterministic and path-derived; different paths differ.
        assert_eq!(
            a,
            media_item_id_for("  Google Photos/Holiday/IMG_0001.jpg  ")
        );
        assert_ne!(a, media_item_id_for("Google Photos/Holiday/IMG_0002.jpg"));
        // Path-keyed, not content-keyed: two paths never share an id.
        assert_ne!(
            media_item_id_for("a/IMG.jpg"),
            media_item_id_for("b/IMG.jpg")
        );
    }

    #[test]
    fn test_album_id_stable() {
        let a = album_id_for("Google Photos/Holiday/metadata.json");
        assert_eq!(a.len(), 16);
        // Whitespace-insensitive and deterministic; different paths differ.
        assert_eq!(a, album_id_for("  Google Photos/Holiday/metadata.json  "));
        assert_ne!(a, album_id_for("Google Photos/Trip/metadata.json"));
        // Paths are case-sensitive (unlike people).
        assert_ne!(a, album_id_for("google photos/holiday/metadata.json"));
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
