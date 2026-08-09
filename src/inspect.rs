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
/// Rayon workers push results through a bounded channel that the returned
/// iterator drains on the calling thread. Streaming rather than collecting lets
/// the caller fold each item straight into sqlite or a dedup map without holding
/// the whole library in memory.
///
/// `container` and `prog` are [`Arc`]s because the worker thread is owned by the
/// returned iterator and so outlives this call.
///
/// Files that yield no [`MediaFileInfo`] — not valid media, or unreadable — are
/// dropped from the stream but counted in
/// [`InspectMediaIter::skipped_count`].
pub(crate) fn inspect_media_files(
    container: Arc<dyn FileSystem>,
    media_si_files: Vec<ScanInfo>,
    prog: Arc<Progress>,
) -> InspectMediaIter {
    // Bounded so the parallel producers can't outrun the single consumer and pile
    // up in memory.
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
    /// Only final once the iterator is fully drained: the last `next` joins the
    /// producer thread, which is what publishes every worker's increment here.
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
        // Channel closed, so the producer is done. Joining reclaims the thread and
        // re-raises any worker panic rather than swallowing it.
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
        // The consumer stopped early, so drain to unpark a producer blocked on the
        // full channel. Unlike `next`, a panic is not re-raised: a drop can run
        // while already unwinding, and a double panic aborts the process.
        for _ in self.rx.iter() {}
        let _ = handle.join();
    }
}

/// `Ok(None)` when the file isn't a supported media type, `Err` when it can't be
/// read or hashed.
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

        // Isolated input dir so the skipped count is deterministic.
        let test_dir = tempfile::tempdir()?;
        let test_dir = test_dir.path();
        fs::copy("test/Canon_40D.jpg", test_dir.join("good.jpg"))?;
        // A .jpg extension over plain text: classifies as media, but is not a
        // valid image.
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
        Ok(())
    }
}
