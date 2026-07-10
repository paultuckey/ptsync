use crate::fs::FileSystem;
use crate::media::{MediaFileInfo, media_file_info_from_readable};
use crate::progress::Progress;
use crate::supplemental_info::{detect_supplemental_info, load_supplemental_info};
use crate::util::{ScanInfo, checksum_bytes};
use anyhow::anyhow;
use rayon::prelude::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;
use tracing::debug;

/// Hash and parse media files in parallel, yielding a [`MediaFileInfo`] as each
/// one finishes.
///
/// A pool of rayon workers inspect the files concurrently and push results
/// through a bounded channel; the returned iterator drains that channel on the
/// calling thread. Streaming results (rather than collecting them) lets the
/// caller fold each item straight into sqlite or a dedup map without holding the
/// whole library in memory.
///
/// `container` and `prog` are taken as [`Arc`]s because the worker thread
/// outlives this call (it is owned by the returned iterator), so they can't be
/// borrowed from the caller's stack.
///
/// Files that were classified as media but produce no [`MediaFileInfo`] (they
/// turned out not to be valid media, or could not be read or hashed) are dropped
/// from the stream but counted; read the total back with
/// [`InspectMediaIter::skipped_count`] once the iterator is drained.
pub(crate) fn inspect_media_files(
    container: Arc<dyn FileSystem>,
    media_si_files: Vec<ScanInfo>,
    prog: Arc<Progress>,
) -> InspectMediaIter {
    // Bound the channel so fast parallel producers can't outrun the single
    // consumer and pile up in memory.
    let channel_capacity = rayon::current_num_threads().saturating_mul(4).max(1);
    let (tx, rx) = std::sync::mpsc::sync_channel(channel_capacity);

    let skipped = Arc::new(AtomicUsize::new(0));
    let worker_skipped = Arc::clone(&skipped);
    let handle = std::thread::spawn(move || {
        media_si_files.par_iter().for_each(|media_si| {
            match analyze_file(container.as_ref(), media_si) {
                Ok(Some(info)) => {
                    let _ = tx.send(info);
                }
                Ok(None) | Err(_) => {
                    worker_skipped.fetch_add(1, Ordering::Relaxed);
                }
            }
            prog.inc();
        });
    });

    InspectMediaIter {
        rx,
        handle: Some(handle),
        skipped,
    }
}

/// Iterator over inspected media that owns the producer thread, joining it once
/// the channel drains (or on drop) so the worker never outlives the iterator.
pub(crate) struct InspectMediaIter {
    rx: Receiver<MediaFileInfo>,
    handle: Option<JoinHandle<()>>,
    skipped: Arc<AtomicUsize>,
}

impl InspectMediaIter {
    /// Number of media-classified files that yielded no [`MediaFileInfo`] and so
    /// were dropped from the output. Only final once the iterator is fully
    /// drained — the producer thread is joined on the last `next`, which
    /// publishes every worker's increment to this thread.
    pub(crate) fn skipped_count(&self) -> usize {
        self.skipped.load(Ordering::Relaxed)
    }
}

impl Iterator for InspectMediaIter {
    type Item = MediaFileInfo;

    fn next(&mut self) -> Option<Self::Item> {
        if let Ok(info) = self.rx.recv() {
            return Some(info);
        }
        // Channel closed: the producer dropped its sender, so it is done. Join
        // to reclaim the thread and re-raise any worker panic, matching the
        // previous scoped-thread behavior where a panic aborted the run.
        if let Some(handle) = self.handle.take()
            && let Err(panic) = handle.join()
        {
            std::panic::resume_unwind(panic);
        }
        None
    }
}

impl Drop for InspectMediaIter {
    fn drop(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        // The consumer stopped early. Drain so a producer parked on the full
        // bounded channel can finish and drop its sender, then join rather than
        // leaving the worker detached. Don't re-raise a panic here: a drop may
        // run while already unwinding, and a double panic aborts the process.
        for _ in self.rx.iter() {}
        let _ = handle.join();
    }
}

/// Inspect a single media file: load any supplemental info, checksum the bytes,
/// then derive its type and metadata. Returns `Ok(None)` when the file isn't a
/// supported media type, and `Err` when it can't be read or hashed.
pub(crate) fn analyze_file(
    root: &dyn FileSystem,
    media_si: &ScanInfo,
) -> anyhow::Result<Option<MediaFileInfo>> {
    let mut supp_info_o = None;
    let supp_info_path_o = detect_supplemental_info(&media_si.file_path, root);
    if let Some(supp_info_path) = supp_info_path_o {
        supp_info_o = load_supplemental_info(&supp_info_path, root);
    }

    let mut reader = root.open(&media_si.file_path)?;
    let hash_info_o = checksum_bytes(&mut reader).ok();
    let Some(hash_info) = hash_info_o else {
        debug!(
            "Could not calculate checksum for file: {:?}",
            media_si.file_path
        );
        return Err(anyhow!(
            "Could not calculate checksum for file: {:?}",
            media_si.file_path
        ));
    };

    let media_info_r =
        media_file_info_from_readable(media_si, &mut reader, &supp_info_o, &hash_info);
    match media_info_r {
        Ok(media_info) => Ok(Some(media_info)),
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_type::QuickFileType;
    use crate::fs::OsFileSystem;
    use crate::util::scan_fs;

    #[test]
    fn test_inspect_media_files_yields_media() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let container: Arc<dyn FileSystem> = Arc::new(OsFileSystem::new("test"));
        let media_si_files: Vec<ScanInfo> = scan_fs(container.as_ref())
            .into_iter()
            .filter(|m| m.quick_file_type == QuickFileType::Media)
            .collect();
        let prog = Arc::new(Progress::new(media_si_files.len() as u64));

        let results: Vec<MediaFileInfo> =
            inspect_media_files(container, media_si_files, prog).collect();

        assert!(
            results
                .iter()
                .any(|m| m.original_file_this_run == "Canon_40D.jpg")
        );
        assert!(
            results
                .iter()
                .any(|m| m.original_file_this_run == "Hello.mp4")
        );
        Ok(())
    }

    #[test]
    fn test_inspect_counts_unprocessable_files() -> anyhow::Result<()> {
        use std::fs;
        use std::io::Write;
        crate::test_util::setup_log();

        // Isolated input dir so the skipped count is deterministic: one valid
        // media file plus one that looks like media by extension but isn't.
        let test_dir = std::path::Path::new("target/test_inspect_skipped");
        if test_dir.exists() {
            fs::remove_dir_all(test_dir)?;
        }
        fs::create_dir_all(test_dir)?;
        fs::copy("test/Canon_40D.jpg", test_dir.join("good.jpg"))?;
        // A .jpg extension over plain-text bytes: classified as media, but not a
        // valid image, so inspection drops it rather than emitting a MediaFileInfo.
        let mut bad = fs::File::create(test_dir.join("bad.jpg"))?;
        bad.write_all(b"this is not an image")?;

        let test_dir_str = test_dir.to_string_lossy();
        let container: Arc<dyn FileSystem> = Arc::new(OsFileSystem::new(&test_dir_str));
        let media_si_files: Vec<ScanInfo> = scan_fs(container.as_ref())
            .into_iter()
            .filter(|m| m.quick_file_type == QuickFileType::Media)
            .collect();
        assert_eq!(media_si_files.len(), 2, "both files classify as media");

        let prog = Arc::new(Progress::new(media_si_files.len() as u64));
        let mut inspected = inspect_media_files(container, media_si_files, prog);
        let results: Vec<MediaFileInfo> = inspected.by_ref().collect();

        assert_eq!(results.len(), 1, "only the valid media file is yielded");
        assert_eq!(results[0].original_file_this_run, "good.jpg");
        assert_eq!(
            inspected.skipped_count(),
            1,
            "the invalid media file is counted as could-not-process"
        );

        fs::remove_dir_all(test_dir)?;
        Ok(())
    }
}
