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
    /// All files recursively, as `/`-separated relative paths.
    fn walk(&self) -> Vec<String>;
    fn metadata(&self, path: &str) -> Result<FileMetadata>;

    /// A hex SHA-256 (see [`crate::util::HashInfo::long_checksum`]) already known
    /// for `path` and obtainable *without* reading the object body — e.g. S3's
    /// `x-amz-checksum-sha256` via HeadObject. `None` when the backend has no
    /// such side-channel, leaving callers to read and hash the bytes.
    fn recorded_checksum(&self, _path: &str) -> Option<String> {
        None
    }
}

/// Separate from [`FileSystem`] because some backends are read-only — a zip
/// source implements only `FileSystem`.
pub trait WritableFileSystem: FileSystem {
    /// Creates any parent directories. Under `dry_run`, logs and writes nothing.
    fn write(&self, dry_run: bool, path: &str, reader: &mut dyn Read) -> Result<()>;

    /// Write only when `bytes` differ from what is already stored. Returns whether
    /// a write happened — under `dry_run`, whether one would have.
    fn write_if_changed(&self, dry_run: bool, path: &str, bytes: &[u8]) -> Result<bool>;

    /// A no-op on backends without settable timestamps, such as object stores.
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

    /// The length is checked first so an obviously different file is rejected
    /// without reading it all into memory.
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
            let relative_path = path.strip_prefix(root_path).unwrap_or(&path);
            // `/` separators so a directory scan matches the zip scan and the
            // output stays identical across Windows and Unix.
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
            // `enclosed_name` comes back with `\` separators on Windows.
            let name_s = name.replace(std::path::MAIN_SEPARATOR, "/");
            file_names.push(name_s.clone());

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
/// from the 0x5455 extended-timestamp extra field. The bare MS-DOS timestamp
/// carries no timezone, so turning it into an instant would mean guessing an
/// offset; an entry without the extra field reports no times at all and its date
/// logic falls through to `undated/`. Most archives store only the modification
/// time there, so `created` is usually `None`.
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

/// A local directory, a local zip, or an `s3://` location.
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

/// A local directory or an `s3://` location.
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

        // Over the threshold, so the reader streams from the archive.
        let mut reader = fs.open("large.txt")?;
        let mut content = Vec::new();
        reader.read_to_end(&mut content)?;
        assert_eq!(content.len(), 200);
        assert_eq!(content, vec![b'a'; 200]);

        // Under it, so the whole entry is buffered in memory.
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
        // walk() must report `/`-separated names on every platform, and every
        // name it reports must round-trip back through open().
        let names = fs.walk();
        assert!(names.contains(&"Photos/Holiday/img.txt".to_string()));
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

        assert!(fs.write_if_changed(false, path, b"hello")?);
        let mtime_after_create = fs::metadata(&on_disk)?.modified()?;

        // Identical bytes leave even the modified time untouched.
        assert!(!fs.write_if_changed(false, path, b"hello")?);
        assert_eq!(mtime_after_create, fs::metadata(&on_disk)?.modified()?);

        assert!(fs.write_if_changed(false, path, b"hello world")?);
        assert_eq!(fs::read(&on_disk)?, b"hello world");
        Ok(())
    }

    #[test]
    fn test_write_if_changed_dry_run_writes_nothing() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let fs = OsFileSystem::new(&dir.path().to_string_lossy());

        // Reports it would write, but must not create the file.
        assert!(fs.write_if_changed(true, "albums/trip.md", b"hello")?);
        assert!(!dir.path().join("albums/trip.md").exists());
        Ok(())
    }

    #[test]
    fn open_factories_route_scheme_and_reject_malformed_s3() {
        // Never silently treated as a local path or zip.
        assert!(open_input("s3://").is_err());
        assert!(open_output("s3://").is_err());
        assert!(open_input("test").is_ok());
        assert!(open_input("does-not-exist-xyz").is_err());
    }
}
