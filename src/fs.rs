use crate::s3_uri::{S3Uri, is_s3_uri};
use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tracing::{debug, error, info};
use zip::{ExtraField, ZipArchive};

#[cfg(not(test))]
const MAX_MEM_THRESHOLD: u64 = 100 * 1024 * 1024; // 100MB
#[cfg(test)]
const MAX_MEM_THRESHOLD: u64 = 100; // 100 bytes for testing

pub trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

#[derive(Debug, Clone)]
pub struct FileMetadata {
    pub len: u64,
    pub modified: Option<i64>,
    pub created: Option<i64>,
}

pub trait FileSystem: Send + Sync {
    fn open(&self, path: &str) -> Result<Box<dyn ReadSeek>>;
    fn exists(&self, path: &str) -> bool;
    // Walk returns all files recursively as relative paths
    fn walk(&self) -> Vec<String>;
    fn metadata(&self, path: &str) -> Result<FileMetadata>;

    /// A hex SHA-256 (see [`crate::util::HashInfo::long_checksum`])
    /// already known for `path`, obtainable *without* reading the object body -
    /// e.g. S3's native `x-amz-checksum-sha256` fetched via HeadObject. Returns
    /// `None` when the backend has no such side-channel (local files, zips,
    /// in-memory), in which case callers fall back to reading the bytes and
    /// hashing.
    fn recorded_checksum(&self, _path: &str) -> Option<String> {
        None
    }
}

/// A [`FileSystem`] that can also be written to - kept separate because some
/// backends are read-only (a zip source implements only `FileSystem`).
pub trait WritableFileSystem: FileSystem {
    /// Write everything from `reader` to `path`, creating any parent directories.
    /// Under `dry_run`, logs what it would do and writes nothing.
    fn write(&self, dry_run: bool, path: &str, reader: &mut dyn Read) -> Result<()>;

    /// Write `bytes` to `path`, but only when they differ from what is already
    /// stored there. Returns whether a write was performed - under `dry_run`,
    /// whether one would have been.
    fn write_if_changed(&self, dry_run: bool, path: &str, bytes: &[u8]) -> Result<bool>;

    /// Set the modified time on an already-written file when the backend supports
    /// it. Backends without settable timestamps (e.g. object stores) may treat
    /// this as a no-op.
    fn set_modified(&self, dry_run: bool, path: &str, modified_datetime: &Option<i64>);
}

#[derive(Debug)]
pub struct OsFileSystem {
    root: PathBuf,
}

impl OsFileSystem {
    pub fn new(root: &str) -> Self {
        Self {
            root: PathBuf::from(root),
        }
    }

    /// True when `path` exists and its contents are exactly `bytes`. The length
    /// is checked so an obviously different file is rejected without reading it all into memory.
    fn file_has_contents(&self, path: &str, bytes: &[u8]) -> bool {
        let p = self.root.join(path);
        let Ok(mut f) = File::open(&p) else {
            return false;
        };
        match f.metadata() {
            Ok(m) if m.len() != bytes.len() as u64 => return false,
            Ok(_) => {}
            Err(_) => return false,
        }
        let mut existing = Vec::with_capacity(bytes.len());
        if f.read_to_end(&mut existing).is_err() {
            return false;
        }
        existing == bytes
    }
}

impl FileSystem for OsFileSystem {
    fn open(&self, path: &str) -> Result<Box<dyn ReadSeek>> {
        let p = self.root.join(path);
        let f = File::open(&p).map_err(|e| anyhow!("Unable to open file {:?}: {}", p, e))?;
        Ok(Box::new(f))
    }

    fn exists(&self, path: &str) -> bool {
        self.root.join(path).exists()
    }

    fn walk(&self) -> Vec<String> {
        let mut files = Vec::new();
        if !self.root.exists() || !self.root.is_dir() {
            return files;
        }
        scan_dir_recursively(&mut files, &self.root, &self.root);
        files
    }

    fn metadata(&self, path: &str) -> Result<FileMetadata> {
        let p = self.root.join(path);
        let m = fs::metadata(&p)?;
        Ok(FileMetadata {
            len: m.len(),
            modified: m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64),
            created: m
                .created()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64),
        })
    }
}

impl WritableFileSystem for OsFileSystem {
    fn write(&self, dry_run: bool, path: &str, reader: &mut dyn Read) -> Result<()> {
        let p = self.root.join(path);
        if dry_run {
            debug!("Dry run: would write file {:?}", p);
            return Ok(());
        }
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| anyhow!("Unable to create directory {:?}: {}", parent, e))?;
        }
        let mut file =
            File::create(&p).map_err(|e| anyhow!("Unable to create file {:?}: {}", p, e))?;
        std::io::copy(reader, &mut file)
            .map_err(|e| anyhow!("Unable to write file {:?}: {}", p, e))?;
        debug!("Wrote file {p:?}");
        Ok(())
    }

    fn write_if_changed(&self, dry_run: bool, path: &str, bytes: &[u8]) -> Result<bool> {
        if self.file_has_contents(path, bytes) {
            debug!("Unchanged, skipping write of {:?}", self.root.join(path));
            return Ok(false);
        }
        self.write(dry_run, path, &mut Cursor::new(bytes))?;
        Ok(true)
    }

    fn set_modified(&self, dry_run: bool, path: &str, modified_datetime: &Option<i64>) {
        let p = self.root.join(path);
        let Some(dt) = modified_datetime else {
            return;
        };
        let st = SystemTime::UNIX_EPOCH
            .checked_add(Duration::from_millis(*dt as u64))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if dry_run {
            debug!("  Dry run: would set modified datetime for file {p:?} to {dt}");
            return;
        }
        let f_r = File::open(&p);
        let Ok(f) = f_r else {
            error!("Unable to open file {p:?} for setting modified datetime ");
            return;
        };
        if let Err(e) = f.set_modified(st) {
            error!("Unable to set modified datetime for file {p:?}: {e}");
        } else {
            debug!("Set modified datetime for file {p:?} to {dt}");
        }
    }
}

fn scan_dir_recursively(files: &mut Vec<String>, dir_path: &Path, root_path: &Path) {
    if !dir_path.exists() || !dir_path.is_dir() {
        return;
    }
    let Ok(dir_reader) = fs::read_dir(dir_path) else {
        debug!("Unable to read directory: {dir_path:?}");
        return;
    };
    for dir_entry in dir_reader {
        let Ok(dir_entry) = dir_entry else {
            continue;
        };
        let path = dir_entry.path();
        if path.is_file() {
            // trim root path from the file path
            let relative_path = path.strip_prefix(root_path).unwrap_or(&path);
            // Always record paths with `/` separators so a directory scan matches
            // the zip scan (which uses `/`) and the output stays portable across Windows and Unix.
            let relative = relative_path
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            files.push(relative);
        } else if path.is_dir() {
            scan_dir_recursively(files, &path, root_path);
        }
    }
}

pub struct ZipFileSystem {
    zip: Mutex<ZipArchive<File>>,
    file_names: Vec<String>,
    metadata_cache: HashMap<String, FileMetadata>,
}

impl ZipFileSystem {
    pub fn new(zip_file: &str) -> Result<Self> {
        let f = File::open(zip_file)?;
        let mut zip = ZipArchive::new(f)?;
        let mut file_names = Vec::new();
        let mut metadata_cache = HashMap::new();

        for i in 0..zip.len() {
            let Ok(file) = zip.by_index(i) else {
                continue;
            };
            if file.is_dir() {
                continue;
            }
            let Some(enclosed_name) = file.enclosed_name() else {
                continue;
            };
            let Some(name) = enclosed_name.to_str() else {
                continue;
            };
            // `enclosed_name` on Windows comes back with `\` separators. Normalize to `/`
            // keeping output identical across platforms. On Unix this is a no-op.
            let name_s = name.replace(std::path::MAIN_SEPARATOR, "/");
            file_names.push(name_s.clone());

            // We only trust timestamps from the 0x5455 "extended timestamp" extra
            // field, which records real UTC epoch seconds. The bare MS-DOS
            // timestamp (`file.last_modified()`) carries no timezone, so we'd have
            // to guess an offset to turn it into an instant - deliberately not
            // done. A zip entry without the extra field therefore reports no
            // times, and the date logic falls through to `undated/`.
            let (modified, created) = zip_extra_field_times(&file);

            metadata_cache.insert(
                name_s,
                FileMetadata {
                    len: file.size(),
                    modified,
                    created,
                },
            );
        }
        Ok(Self {
            zip: Mutex::new(zip),
            file_names,
            metadata_cache,
        })
    }
}

/// Modified and created times for a zip entry, in epoch milliseconds, taken only
/// from the 0x5455 extended-timestamp extra field (UTC epoch seconds). Returns
/// `(None, None)` when the entry carries no such field. Most archives only store
/// the modification time there, so `created` is usually `None`.
fn zip_extra_field_times<R: std::io::Read>(
    file: &zip::read::ZipFile<'_, R>,
) -> (Option<i64>, Option<i64>) {
    for ef in file.extra_data_fields() {
        if let ExtraField::ExtendedTimestamp(ts) = ef {
            let modified = ts.mod_time().map(|s| s as i64 * 1000);
            let created = ts.cr_time().map(|s| s as i64 * 1000);
            return (modified, created);
        }
    }
    (None, None)
}

impl FileSystem for ZipFileSystem {
    fn open(&self, path: &str) -> Result<Box<dyn ReadSeek>> {
        let mut zip = self
            .zip
            .lock()
            .map_err(|e| anyhow!("Zip lock failed: {}", e))?;
        let mut file = zip
            .by_name(path)
            .map_err(|_| anyhow!("File not found in zip: {}", path))?;

        if file.size() > MAX_MEM_THRESHOLD {
            debug!(
                "Streaming large file {} ({} bytes) to temp storage",
                path,
                file.size()
            );
            let mut temp = tempfile::tempfile()?;
            std::io::copy(&mut file, &mut temp)?;
            temp.seek(SeekFrom::Start(0))?;
            Ok(Box::new(temp))
        } else {
            let mut buffer = Vec::new();
            file.read_to_end(&mut buffer)?;
            Ok(Box::new(Cursor::new(buffer)))
        }
    }

    fn exists(&self, path: &str) -> bool {
        self.metadata_cache.contains_key(path)
    }

    fn walk(&self) -> Vec<String> {
        self.file_names.clone()
    }

    fn metadata(&self, path: &str) -> Result<FileMetadata> {
        self.metadata_cache
            .get(path)
            .cloned()
            .ok_or_else(|| anyhow!("File not found in zip metadata cache: {}", path))
    }
}

/// Build a read-only input container from a path or an `s3://` URI: a local
/// directory (`OsFileSystem`), a local zip (`ZipFileSystem`), or an S3 location.
pub fn open_input(input: &str) -> Result<Arc<dyn FileSystem>> {
    if is_s3_uri(input) {
        let uri = S3Uri::parse(input)
            .ok_or_else(|| anyhow!("Malformed S3 URI: {input} (expected s3://bucket/prefix)"))?;
        info!("Input S3: s3://{}/{}", uri.bucket, uri.prefix);
        return Ok(Arc::new(crate::s3_fs::S3FileSystem::new(uri)?));
    }
    let path = Path::new(input);
    if !path.exists() {
        return Err(anyhow!("Input path does not exist: {input}"));
    }
    if path.is_dir() {
        info!("Input directory: {input}");
        Ok(Arc::new(OsFileSystem::new(input)))
    } else {
        info!("Input zip: {input}");
        Ok(Arc::new(ZipFileSystem::new(input)?))
    }
}

/// Build a writable output container from a path or an `s3://` URI: a local
/// directory (`OsFileSystem`) or an S3 location.
pub fn open_output(output: &str) -> Result<Arc<dyn WritableFileSystem>> {
    if is_s3_uri(output) {
        let uri = S3Uri::parse(output)
            .ok_or_else(|| anyhow!("Malformed S3 URI: {output} (expected s3://bucket/prefix)"))?;
        info!("Output S3: s3://{}/{}", uri.bucket, uri.prefix);
        return Ok(Arc::new(crate::s3_fs::S3FileSystem::new(uri)?));
    }
    info!("Output directory: {output}");
    Ok(Arc::new(OsFileSystem::new(output)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::FileOptions;

    #[test]
    fn test_zip_open_streaming() -> Result<()> {
        let mut temp_file = tempfile::NamedTempFile::new()?;

        {
            let mut zip_writer = zip::ZipWriter::new(&mut temp_file);
            let options =
                FileOptions::<()>::default().compression_method(zip::CompressionMethod::Stored);

            let large_content = vec![b'a'; 200];
            zip_writer.start_file("large.txt", options)?;
            zip_writer.write_all(&large_content)?;

            let small_content = vec![b'b'; 50];
            zip_writer.start_file("small.txt", options)?;
            zip_writer.write_all(&small_content)?;

            zip_writer.finish()?;
        }

        let fs = ZipFileSystem::new(&temp_file.path().to_string_lossy())?;

        // Test large file (should stream)
        let mut reader = fs.open("large.txt")?;
        let mut content = Vec::new();
        reader.read_to_end(&mut content)?;
        assert_eq!(content.len(), 200);
        assert_eq!(content, vec![b'a'; 200]);

        // Test small file (should buffer)
        let mut reader = fs.open("small.txt")?;
        let mut content = Vec::new();
        reader.read_to_end(&mut content)?;
        assert_eq!(content.len(), 50);
        assert_eq!(content, vec![b'b'; 50]);

        Ok(())
    }

    #[test]
    fn test_zip_nested_entries_walk_with_slashes_and_open() -> Result<()> {
        let mut temp_file = tempfile::NamedTempFile::new()?;
        {
            let mut zip_writer = zip::ZipWriter::new(&mut temp_file);
            let options =
                FileOptions::<()>::default().compression_method(zip::CompressionMethod::Stored);
            zip_writer.start_file("Photos/Holiday/img.txt", options)?;
            zip_writer.write_all(b"hello")?;
            zip_writer.finish()?;
        }
        let fs = ZipFileSystem::new(&temp_file.path().to_string_lossy())?;
        // walk() must report `/`-separated names on every platform: `enclosed_name`
        let names = fs.walk();
        assert!(names.contains(&"Photos/Holiday/img.txt".to_string()));
        // Every walked name must round-trip back through open().
        for name in &names {
            let mut reader = fs.open(name)?;
            let mut content = Vec::new();
            reader.read_to_end(&mut content)?;
            assert_eq!(content, b"hello");
        }
        Ok(())
    }

    #[test]
    fn test_write_if_changed_skips_identical_bytes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let fs = OsFileSystem::new(&dir.path().to_string_lossy());
        // Nested path also exercises parent-directory creation.
        let path = "albums/trip.md";
        let on_disk = dir.path().join(path);

        // First write creates the file and reports that it wrote.
        assert!(fs.write_if_changed(false, path, b"hello")?);
        let mtime_after_create = fs::metadata(&on_disk)?.modified()?;

        // Re-writing identical bytes is a no-op: nothing is written and the
        // file's modified time is untouched.
        assert!(!fs.write_if_changed(false, path, b"hello")?);
        assert_eq!(mtime_after_create, fs::metadata(&on_disk)?.modified()?);

        // Changed content is written through.
        assert!(fs.write_if_changed(false, path, b"hello world")?);
        assert_eq!(fs::read(&on_disk)?, b"hello world");
        Ok(())
    }

    #[test]
    fn test_write_if_changed_dry_run_writes_nothing() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let fs = OsFileSystem::new(&dir.path().to_string_lossy());

        // A dry run reports it would write (content differs from the absent file)
        // but must not actually create it.
        assert!(fs.write_if_changed(true, "albums/trip.md", b"hello")?);
        assert!(!dir.path().join("albums/trip.md").exists());
        Ok(())
    }

    #[test]
    fn open_factories_route_scheme_and_reject_malformed_s3() {
        // A malformed `s3://` URI is a hard error, never silently treated as a
        // local path/zip.
        assert!(open_input("s3://").is_err());
        assert!(open_output("s3://").is_err());
        assert!(open_input("test").is_ok());
        assert!(open_input("does-not-exist-xyz").is_err());
    }
}
