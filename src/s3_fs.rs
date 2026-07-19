//! S3-backed `FileSystem` / `WritableFileSystem`, bridging the async AWS SDK to
//! the synchronous traits with a `block_on` runtime (as `db_cmd` does for Turso).
//! `FakeS3FileSystem` is the in-memory test double the sync-level tests use.

use crate::fs::{FileMetadata, FileSystem, ReadSeek, WritableFileSystem};
use crate::s3_uri::S3Uri;
use anyhow::{Result, anyhow};
use aws_sdk_s3::Client;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{ChecksumAlgorithm, ChecksumMode};
use base64::prelude::{BASE64_STANDARD, Engine as _};
use std::collections::BTreeMap;
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::sync::{Mutex, OnceLock};
use tracing::debug;

/// Objects larger than this are streamed to a temp file on `open` rather than
/// buffered in memory, so a large video doesn't have to fit in RAM (mirrors
/// `ZipFileSystem`).
const MAX_MEM_THRESHOLD: u64 = 100 * 1024 * 1024; // 100MB

/// Region / endpoint / profile overrides for `s3://` access, from the CLI. A
/// `None` field defers to the default AWS chain (env vars, `~/.aws`, SSO, IMDS),
/// so these layer *over* it rather than replacing it.
#[derive(Clone, Debug, Default)]
pub struct S3Config {
    pub region: Option<String>,
    pub endpoint_url: Option<String>,
    pub profile: Option<String>,
}

static S3_CONFIG: OnceLock<S3Config> = OnceLock::new();

/// Set once at startup; later calls are ignored.
pub fn set_s3_config(config: S3Config) {
    let _ = S3_CONFIG.set(config);
}

fn s3_config() -> S3Config {
    S3_CONFIG.get().cloned().unwrap_or_default()
}

async fn build_client(config: &S3Config) -> Client {
    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
    if let Some(region) = &config.region {
        loader = loader.region(aws_sdk_s3::config::Region::new(region.clone()));
    }
    if let Some(profile) = &config.profile {
        loader = loader.profile_name(profile.clone());
    }
    if let Some(endpoint) = &config.endpoint_url {
        loader = loader.endpoint_url(endpoint.clone());
    }
    let sdk_config = loader.load().await;
    if config.endpoint_url.is_some() {
        // Custom endpoints (MinIO, localstack, …) generally need path-style
        // addressing - bucket-as-subdomain won't resolve there.
        let conf = aws_sdk_s3::config::Builder::from(&sdk_config)
            .force_path_style(true)
            .build();
        Client::from_conf(conf)
    } else {
        Client::new(&sdk_config)
    }
}

/// The region a bucket actually lives in, when it differs from the one the
/// client is pointed at - else `None`.
///
/// S3 signs per-region and a region-specific endpoint won't redirect: asking
/// `ap-southeast-2` about a bucket in `ap-southeast-6` fails with
/// `IllegalLocationConstraintException` rather than being routed. AWS does put
/// the bucket's real region in `x-amz-bucket-region` on those failures, so a
/// cheap `HeadBucket` probe is enough to learn it and rebuild the client - no
/// need for the caller to know each bucket's region up front.
async fn bucket_region_override(
    client: &Client,
    bucket: &str,
    config: &S3Config,
) -> Option<String> {
    // S3-compatible stores (MinIO, localstack) serve every bucket from the one
    // endpoint and don't set the header; skip the probe entirely.
    if config.endpoint_url.is_some() {
        return None;
    }
    // Success means the region is already right. A failure for any *other*
    // reason (missing bucket, no credentials) carries no region header, so this
    // falls through to `None` and the real error surfaces from the listing.
    let err = client.head_bucket().bucket(bucket).send().await.err()?;
    let actual = err.raw_response()?.headers().get("x-amz-bucket-region")?;
    let current = client.config().region().map(|r| r.as_ref());
    (current != Some(actual)).then(|| actual.to_string())
}

/// An S3 bucket/prefix as a [`FileSystem`] + [`WritableFileSystem`]. Object keys
/// map to `/`-separated paths relative to the prefix, matching the zip and
/// directory scanners.
pub struct S3FileSystem {
    client: Client,
    /// Owns the async runtime the blocking `FileSystem` methods drive. A
    /// multi-thread runtime so concurrent `open` calls from the parallel
    /// inspection stage can each `block_on` without contending.
    rt: tokio::runtime::Runtime,
    uri: S3Uri,
    /// The listing, filled at construction: the source of truth for
    /// `walk`/`exists`/`metadata`, so no per-call network. Behind a `Mutex`
    /// because writes insert into it - so `exists` sees objects written *earlier
    /// in this same run* and same-instant collisions get suffixed instead of
    /// overwriting. A `BTreeMap` keeps `walk` order stable (reproducibility).
    objects: Mutex<BTreeMap<String, FileMetadata>>,
}

impl S3FileSystem {
    pub fn new(uri: S3Uri) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let config = s3_config();
        let client = rt.block_on(async {
            let client = build_client(&config).await;
            match bucket_region_override(&client, &uri.bucket, &config).await {
                Some(region) => {
                    debug!("Bucket {} is in {region}; rebuilding client", uri.bucket);
                    build_client(&S3Config {
                        region: Some(region),
                        ..config.clone()
                    })
                    .await
                }
                None => client,
            }
        });
        Self::from_client(client, uri, rt)
    }

    /// Construct from an already-built client and runtime, running the initial
    /// listing. `new` builds the default AWS client from the environment; tests
    /// inject a mocked client so the real listing/read code runs offline.
    fn from_client(client: Client, uri: S3Uri, rt: tokio::runtime::Runtime) -> Result<Self> {
        let objects = rt.block_on(list_objects(&client, &uri))?;
        debug!(
            "Listed {} objects under s3://{}/{}",
            objects.len(),
            uri.bucket,
            uri.prefix
        );
        Ok(Self {
            client,
            rt,
            uri,
            objects: Mutex::new(objects),
        })
    }

    /// Record an object we just uploaded in the cache, so `exists`/`metadata`
    /// reflect this run's writes.
    fn note_written(&self, path: &str, len: u64) {
        if let Ok(mut objects) = self.objects.lock() {
            objects.insert(
                path.to_string(),
                FileMetadata {
                    len,
                    modified: None,
                    created: None,
                },
            );
        }
    }

    fn object_bytes(&self, path: &str) -> Result<Vec<u8>> {
        let mut reader = self.open(path)?;
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf)?;
        Ok(buf)
    }
}

/// List every object under the prefix as a map keyed by prefix-relative path.
/// Paginated so buckets with more than 1000 keys list fully.
async fn list_objects(client: &Client, uri: &S3Uri) -> Result<BTreeMap<String, FileMetadata>> {
    let mut objects = BTreeMap::new();
    // A non-empty prefix lists with a trailing slash so only keys *under* it
    // match (not a sibling like `photos-old/`).
    let list_prefix = if uri.prefix.is_empty() {
        String::new()
    } else {
        format!("{}/", uri.prefix)
    };
    let mut pages = client
        .list_objects_v2()
        .bucket(&uri.bucket)
        .prefix(list_prefix)
        .into_paginator()
        .send();
    while let Some(page) = pages.next().await {
        let page = page.map_err(|e| anyhow!("S3 ListObjectsV2 on {} failed: {e:?}", uri.bucket))?;
        for obj in page.contents() {
            let Some(key) = obj.key() else {
                continue;
            };
            // Skip "directory" placeholder keys.
            if key.ends_with('/') {
                continue;
            }
            let Some(rel) = uri.relative_of(key) else {
                continue;
            };
            if rel.is_empty() {
                continue;
            }
            // S3 LastModified is a wall-clock instant (second precision); there
            // is no creation time.
            let modified = obj.last_modified().map(|dt| dt.secs() * 1000);
            let len = obj.size().unwrap_or(0).max(0) as u64;
            objects.insert(
                rel,
                FileMetadata {
                    len,
                    modified,
                    created: None,
                },
            );
        }
    }
    Ok(objects)
}

impl FileSystem for S3FileSystem {
    fn open(&self, path: &str) -> Result<Box<dyn ReadSeek>> {
        let key = self.uri.key_for(path);
        let size = self
            .objects
            .lock()
            .ok()
            .and_then(|o| o.get(path).map(|m| m.len))
            .unwrap_or(0);
        self.rt.block_on(async {
            let resp = self
                .client
                .get_object()
                .bucket(&self.uri.bucket)
                .key(&key)
                .send()
                .await
                .map_err(|e| anyhow!("S3 GetObject {key} failed: {e:?}"))?;
            let mut body = resp.body;
            if size > MAX_MEM_THRESHOLD {
                let mut temp = tempfile::tempfile()?;
                while let Some(chunk) = body.next().await {
                    let chunk = chunk.map_err(|e| anyhow!("S3 stream error for {key}: {e:?}"))?;
                    temp.write_all(&chunk)?;
                }
                temp.seek(SeekFrom::Start(0))?;
                Ok(Box::new(temp) as Box<dyn ReadSeek>)
            } else {
                let data = body
                    .collect()
                    .await
                    .map_err(|e| anyhow!("S3 read of {key} failed: {e:?}"))?;
                Ok(Box::new(Cursor::new(data.into_bytes().to_vec())) as Box<dyn ReadSeek>)
            }
        })
    }

    fn exists(&self, path: &str) -> bool {
        self.objects
            .lock()
            .map(|o| o.contains_key(path))
            .unwrap_or(false)
    }

    fn walk(&self) -> Vec<String> {
        self.objects
            .lock()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default()
    }

    fn metadata(&self, path: &str) -> Result<FileMetadata> {
        let objects = self
            .objects
            .lock()
            .map_err(|e| anyhow!("S3 cache lock poisoned: {e}"))?;
        objects
            .get(path)
            .cloned()
            .ok_or_else(|| anyhow!("Object not found in S3 listing: {path}"))
    }

    fn recorded_checksum(&self, path: &str) -> Option<String> {
        // Option A: read the object's native SHA-256 (x-amz-checksum-sha256) via
        // a metadata-only HeadObject, so the dedup check can skip re-uploads
        // without downloading the body.
        let key = self.uri.key_for(path);
        self.rt.block_on(async {
            let head = self
                .client
                .head_object()
                .bucket(&self.uri.bucket)
                .key(&key)
                .checksum_mode(ChecksumMode::Enabled)
                .send()
                .await
                .ok()?;
            let b64 = head.checksum_sha256()?;
            // A multipart object's SHA-256 is composite ("base64-N"), not the
            // whole-file hash, so it can't be compared to ptsync's checksum.
            if b64.contains('-') {
                return None;
            }
            let raw = BASE64_STANDARD.decode(b64).ok()?;
            Some(hex::encode(raw))
        })
    }
}

impl WritableFileSystem for S3FileSystem {
    fn write(&self, dry_run: bool, path: &str, reader: &mut dyn Read) -> Result<()> {
        let key = self.uri.key_for(path);
        if dry_run {
            debug!("Dry run: would upload s3://{}/{}", self.uri.bucket, key);
            return Ok(());
        }
        // Stage to a temp file: PutObject needs a known length and the SDK reads
        // the body to compute the SHA-256, so a large object stays out of memory.
        let mut temp = tempfile::NamedTempFile::new()?;
        std::io::copy(reader, temp.as_file_mut())?;
        temp.as_file_mut().flush()?;
        let len = temp.as_file().metadata()?.len();
        let temp_path = temp.path().to_path_buf();
        self.rt.block_on(async {
            let body = ByteStream::from_path(&temp_path)
                .await
                .map_err(|e| anyhow!("staging S3 upload for {key} failed: {e:?}"))?;
            self.client
                .put_object()
                .bucket(&self.uri.bucket)
                .key(&key)
                .body(body)
                // Store the native SHA-256 so a later run can dedup via HeadObject
                // (Option A) instead of re-downloading.
                .checksum_algorithm(ChecksumAlgorithm::Sha256)
                .send()
                .await
                .map_err(|e| anyhow!("S3 PutObject {key} failed: {e:?}"))?;
            Ok::<(), anyhow::Error>(())
        })?;
        debug!("Uploaded s3://{}/{}", self.uri.bucket, key);
        self.note_written(path, len);
        Ok(())
    }

    fn write_if_changed(&self, dry_run: bool, path: &str, bytes: &[u8]) -> Result<bool> {
        if self.exists(path) {
            let new_hash = crate::util::checksum_bytes(&mut Cursor::new(bytes))
                .ok()
                .map(|h| h.long_checksum);
            // Prefer the native checksum (no download); fall back to fetching and
            // comparing bytes when the object has no comparable checksum.
            let same = match (self.recorded_checksum(path), new_hash) {
                (Some(existing), Some(new)) => existing == new,
                _ => self.object_bytes(path).map(|b| b == bytes).unwrap_or(false),
            };
            if same {
                debug!(
                    "Unchanged, skipping S3 upload of s3://{}/{}",
                    self.uri.bucket,
                    self.uri.key_for(path)
                );
                return Ok(false);
            }
        }
        if dry_run {
            return Ok(true);
        }
        let key = self.uri.key_for(path);
        self.rt.block_on(async {
            self.client
                .put_object()
                .bucket(&self.uri.bucket)
                .key(&key)
                .body(ByteStream::from(bytes.to_vec()))
                .checksum_algorithm(ChecksumAlgorithm::Sha256)
                .send()
                .await
                .map_err(|e| anyhow!("S3 PutObject {key} failed: {e:?}"))?;
            Ok::<(), anyhow::Error>(())
        })?;
        self.note_written(path, bytes.len() as u64);
        Ok(true)
    }

    fn set_modified(&self, _dry_run: bool, _path: &str, _modified_datetime: &Option<i64>) {
        // S3 objects carry no settable mtime; the authoritative time lives in the
        // sidecar's frontmatter (consistent with FakeS3FileSystem).
    }
}

/// An in-memory fake of an S3 bucket for tests. The real `S3FileSystem` needs
/// exhaustive per-operation SDK mocks, so the sync-level test drives the write
/// path against this lighter double instead. It models what matters: a flat
/// key→bytes namespace, no mtime, and a native checksum via `recorded_checksum`.
#[cfg(test)]
pub(crate) struct FakeS3FileSystem {
    objects: std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
}

#[cfg(test)]
impl FakeS3FileSystem {
    pub(crate) fn new() -> Self {
        Self {
            objects: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
}

#[cfg(test)]
impl crate::fs::FileSystem for FakeS3FileSystem {
    fn open(&self, path: &str) -> anyhow::Result<Box<dyn crate::fs::ReadSeek>> {
        use anyhow::anyhow;
        let objects = self
            .objects
            .lock()
            .map_err(|e| anyhow!("FakeS3FileSystem lock poisoned: {e}"))?;
        let bytes = objects
            .get(path)
            .ok_or_else(|| anyhow!("No such object in FakeS3FileSystem: {path}"))?;
        Ok(Box::new(std::io::Cursor::new(bytes.clone())))
    }

    fn exists(&self, path: &str) -> bool {
        self.objects
            .lock()
            .map(|o| o.contains_key(path))
            .unwrap_or(false)
    }

    fn walk(&self) -> Vec<String> {
        self.objects
            .lock()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default()
    }

    fn metadata(&self, path: &str) -> anyhow::Result<crate::fs::FileMetadata> {
        use anyhow::anyhow;
        let objects = self
            .objects
            .lock()
            .map_err(|e| anyhow!("FakeS3FileSystem lock poisoned: {e}"))?;
        let bytes = objects
            .get(path)
            .ok_or_else(|| anyhow!("No such object in FakeS3FileSystem: {path}"))?;
        Ok(crate::fs::FileMetadata {
            len: bytes.len() as u64,
            modified: None,
            created: None,
        })
    }

    fn recorded_checksum(&self, path: &str) -> Option<String> {
        // Mirrors S3's native x-amz-checksum-sha256: the object's SHA-256 is
        // available without downloading the body, so `is_existing_file_same`
        // takes its fast path (Option A) instead of GET+rehash.
        let objects = self.objects.lock().ok()?;
        let bytes = objects.get(path)?;
        let mut cursor = std::io::Cursor::new(bytes.as_slice());
        crate::util::checksum_bytes(&mut cursor)
            .ok()
            .map(|h| h.long_checksum)
    }
}

#[cfg(test)]
impl crate::fs::WritableFileSystem for FakeS3FileSystem {
    fn write(
        &self,
        dry_run: bool,
        path: &str,
        reader: &mut dyn std::io::Read,
    ) -> anyhow::Result<()> {
        use anyhow::anyhow;
        if dry_run {
            return Ok(());
        }
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        self.objects
            .lock()
            .map_err(|e| anyhow!("FakeS3FileSystem lock poisoned: {e}"))?
            .insert(path.to_string(), bytes);
        Ok(())
    }

    fn write_if_changed(&self, dry_run: bool, path: &str, bytes: &[u8]) -> anyhow::Result<bool> {
        use anyhow::anyhow;
        {
            let objects = self
                .objects
                .lock()
                .map_err(|e| anyhow!("FakeS3FileSystem lock poisoned: {e}"))?;
            if let Some(existing) = objects.get(path)
                && existing.as_slice() == bytes
            {
                return Ok(false);
            }
        }
        if !dry_run {
            self.objects
                .lock()
                .map_err(|e| anyhow!("FakeS3FileSystem lock poisoned: {e}"))?
                .insert(path.to_string(), bytes.to_vec());
        }
        Ok(true)
    }

    fn set_modified(&self, _dry_run: bool, _path: &str, _modified_datetime: &Option<i64>) {
        // S3 objects carry no settable mtime; a faithful fake preserves none.
    }
}

// Offline unit tests that drive the *real* `S3FileSystem` (listing, pagination,
// prefix mapping, GetObject) against a mocked AWS SDK - no network, no
// credentials. The mock intercepts at the SDK operation layer, so the actual
// `list_objects`/`open` code runs.
#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_s3::operation::get_object::GetObjectOutput;
    use aws_sdk_s3::operation::head_object::HeadObjectOutput;
    use aws_sdk_s3::operation::list_objects_v2::ListObjectsV2Output;
    use aws_sdk_s3::operation::put_object::PutObjectOutput;
    use aws_sdk_s3::primitives::{ByteStream, DateTime};
    use aws_sdk_s3::types::Object;
    use aws_smithy_mocks::{RuleMode, mock, mock_client};
    use base64::prelude::BASE64_STANDARD;
    use std::io::Read;

    fn obj(key: &str, size: i64) -> Object {
        Object::builder()
            .key(key)
            .size(size)
            .last_modified(DateTime::from_secs(1_700_000_000))
            .build()
    }

    /// Build an `S3FileSystem` over a mock client, running the real listing.
    fn mock_fs(uri: &str, client: aws_sdk_s3::Client) -> anyhow::Result<S3FileSystem> {
        let uri = S3Uri::parse(uri).ok_or_else(|| anyhow!("bad test uri {uri}"))?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        S3FileSystem::from_client(client, uri, rt)
    }

    #[test]
    fn lists_maps_prefix_and_skips_placeholders() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let list = mock!(aws_sdk_s3::Client::list_objects_v2).then_output(|| {
            ListObjectsV2Output::builder()
                .contents(obj("photos/2024/", 0)) // directory placeholder -> skipped
                .contents(obj("photos/2024/a.jpg", 10))
                .contents(obj("photos/2024/sub/b.jpg", 20))
                .is_truncated(false)
                .build()
        });
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&list]);
        let fs = mock_fs("s3://bucket/photos/2024", client)?;

        assert_eq!(
            fs.walk(),
            vec!["a.jpg".to_string(), "sub/b.jpg".to_string()]
        );
        assert!(fs.exists("a.jpg"));
        assert!(!fs.exists("photos/2024/a.jpg")); // stored relative, not absolute
        assert_eq!(fs.metadata("sub/b.jpg")?.len, 20);
        Ok(())
    }

    #[test]
    fn open_reads_object_body() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let list = mock!(aws_sdk_s3::Client::list_objects_v2).then_output(|| {
            ListObjectsV2Output::builder()
                .contents(obj("p/hello.txt", 11))
                .is_truncated(false)
                .build()
        });
        // open("hello.txt") must GET the full key "p/hello.txt" (prefix rejoined).
        let get = mock!(aws_sdk_s3::Client::get_object)
            .match_requests(|req| req.key() == Some("p/hello.txt"))
            .then_output(|| {
                GetObjectOutput::builder()
                    .body(ByteStream::from_static(b"hello world"))
                    .build()
            });
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&list, &get]);
        let fs = mock_fs("s3://bucket/p", client)?;

        let mut reader = fs.open("hello.txt")?;
        let mut buf = String::new();
        reader.read_to_string(&mut buf)?;
        assert_eq!(buf, "hello world");
        Ok(())
    }

    #[test]
    fn lists_paginates_across_pages() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        // Page 1 is truncated with a continuation token, so the paginator must
        // issue a second call to collect page 2.
        let list = mock!(aws_sdk_s3::Client::list_objects_v2)
            .sequence()
            .output(|| {
                ListObjectsV2Output::builder()
                    .contents(obj("p/a.jpg", 1))
                    .is_truncated(true)
                    .next_continuation_token("tok")
                    .build()
            })
            .output(|| {
                ListObjectsV2Output::builder()
                    .contents(obj("p/b.jpg", 2))
                    .is_truncated(false)
                    .build()
            })
            .build();
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&list]);
        let fs = mock_fs("s3://bucket/p", client)?;
        assert_eq!(fs.walk(), vec!["a.jpg".to_string(), "b.jpg".to_string()]);
        Ok(())
    }

    #[test]
    fn write_puts_object_with_checksum_and_updates_cache() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let list = mock!(aws_sdk_s3::Client::list_objects_v2)
            .then_output(|| ListObjectsV2Output::builder().is_truncated(false).build());
        // The upload must target the prefix-rejoined key and request the SHA-256
        // checksum (Option A).
        let put = mock!(aws_sdk_s3::Client::put_object)
            .match_requests(|req| {
                req.key() == Some("out/2024/x.jpg")
                    && req.checksum_algorithm() == Some(&ChecksumAlgorithm::Sha256)
            })
            .then_output(|| PutObjectOutput::builder().build());
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&list, &put]);
        let fs = mock_fs("s3://bucket/out", client)?;

        assert!(!fs.exists("2024/x.jpg"));
        fs.write(
            false,
            "2024/x.jpg",
            &mut Cursor::new(b"photo-bytes".to_vec()),
        )?;
        // The write is recorded, so a same-run same-path collision is now seen.
        assert!(fs.exists("2024/x.jpg"));
        Ok(())
    }

    #[test]
    fn recorded_checksum_decodes_native_sha256() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let hex_cs = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        let b64 = BASE64_STANDARD.encode(hex::decode(hex_cs)?);
        let list = mock!(aws_sdk_s3::Client::list_objects_v2)
            .then_output(|| ListObjectsV2Output::builder().is_truncated(false).build());
        let head = mock!(aws_sdk_s3::Client::head_object).then_output(move || {
            HeadObjectOutput::builder()
                .checksum_sha256(b64.clone())
                .build()
        });
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&list, &head]);
        let fs = mock_fs("s3://bucket/out", client)?;
        assert_eq!(fs.recorded_checksum("x.jpg"), Some(hex_cs.to_string()));
        Ok(())
    }

    #[test]
    fn recorded_checksum_none_for_composite_multipart() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let list = mock!(aws_sdk_s3::Client::list_objects_v2)
            .then_output(|| ListObjectsV2Output::builder().is_truncated(false).build());
        // A multipart object's checksum is composite ("base64-N") - not comparable.
        let head = mock!(aws_sdk_s3::Client::head_object).then_output(|| {
            HeadObjectOutput::builder()
                .checksum_sha256("YWJjZA==-3")
                .build()
        });
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&list, &head]);
        let fs = mock_fs("s3://bucket/out", client)?;
        assert_eq!(fs.recorded_checksum("x.jpg"), None);
        Ok(())
    }

    #[test]
    fn write_if_changed_skips_identical_object() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let content = b"note body";
        let cs = crate::util::checksum_bytes(&mut Cursor::new(content.to_vec()))?.long_checksum;
        let b64 = BASE64_STANDARD.encode(hex::decode(&cs)?);
        // The bucket already holds the object, and its native checksum matches.
        let list = mock!(aws_sdk_s3::Client::list_objects_v2).then_output(|| {
            ListObjectsV2Output::builder()
                .contents(obj("out/note.md", 9))
                .is_truncated(false)
                .build()
        });
        let head = mock!(aws_sdk_s3::Client::head_object).then_output(move || {
            HeadObjectOutput::builder()
                .checksum_sha256(b64.clone())
                .build()
        });
        // No put_object rule on purpose: a skip must not upload anything (a stray
        // PutObject would have no matching rule and fail the test).
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&list, &head]);
        let fs = mock_fs("s3://bucket/out", client)?;

        let wrote = fs.write_if_changed(false, "note.md", content)?;
        assert!(!wrote, "identical content must be skipped (no upload)");
        Ok(())
    }
}
