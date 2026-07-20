use crate::fs::FileSystem;
use crate::media::{MediaFileDerivedInfo, MediaFileInfo};
use crate::util::is_existing_file_same;
use anyhow::anyhow;
use std::collections::HashMap;
use tracing::{debug, warn};

#[derive(Debug, PartialEq)]
pub(crate) enum DeDuplicationResult {
    /// Write the bytes here; nothing is at this path yet (or what was there is
    /// different content that we've suffixed past).
    WritePath(String),
    /// A byte-identical file already lives here, so there is nothing to write.
    SkipWrite(String),
}

/// Collects inspected media and removes duplicates. It performs two jobs:
///
/// 1. *Content dedup* - files that share a long (sha256) checksum are the same
///    bytes. [`Deduplicator::add`] keeps a single entry per checksum and records every original
///    path that resolved to it.
/// 2. *Path dedup* - [`Deduplicator::resolve_output_path`] finds a free name in
///    the output directory, reusing an existing file when its checksum matches
///    and otherwise suffixing the name with the file's own checksum.
///
/// Both are deliberately deterministic: re-running over the same input produces
/// the same canonical entries, the same write order, and the same output names
/// every time. Parallel inspection yields files in whatever order the worker
/// threads happen to finish, so anything order-sensitive has to be pinned here
/// rather than left to that race.
pub(crate) struct Deduplicator {
    by_checksum: HashMap<String, MediaFileInfo>,
}

impl Deduplicator {
    pub(crate) fn new() -> Self {
        Self {
            by_checksum: HashMap::new(),
        }
    }

    /// Fold one inspected file into the collection, collapsing it onto any
    /// existing entry that has the same content hash.
    pub(crate) fn add(&mut self, media: MediaFileInfo) {
        let checksum = media.hash_info.long_checksum.clone();
        match self.by_checksum.get_mut(&checksum) {
            Some(existing) => merge_into(existing, media),
            None => {
                self.by_checksum.insert(checksum, media);
            }
        }
    }

    /// The deduplicated media keyed by long checksum, for callers (albums) that
    /// look entries up by content hash.
    pub(crate) fn by_checksum(&self) -> &HashMap<String, MediaFileInfo> {
        &self.by_checksum
    }

    /// All deduplicated media in a stable order (by long checksum).
    ///
    /// The write loop decides which file claims a bare (un-suffixed) name when
    /// two distinct files want the same one: the first writer wins it, the rest
    /// fall back to a checksum suffix. Iterating a `HashMap` directly would let
    /// that "first writer" flip every run, so we pin the order by checksum.
    pub(crate) fn sorted_media(&self) -> Vec<&MediaFileInfo> {
        let mut media: Vec<&MediaFileInfo> = self.by_checksum.values().collect();
        media.sort_by(|a, b| a.hash_info.long_checksum.cmp(&b.hash_info.long_checksum));
        media
    }

    /// Resolve the path a media file should be written to, given what is already
    /// in `output_container`.
    ///
    /// Candidate names are tried in this order:
    ///   1. the bare desired path (`date/hhmm-ssms`) - keeps the readable name
    ///      for the common, no-collision case;
    ///   2. desired path + `-<short checksum>` - used as soon as the bare name is
    ///      taken by *different* content. The suffix comes from the file's own
    ///      bytes, so a file's collision name never depends on processing order
    ///      or on what else happens to land on the same name;
    ///   3. desired path + `-<long checksum>` - last resort for the
    ///      (astronomically unlikely) case of two different files sharing a
    ///      short checksum.
    ///
    /// At each candidate: a free name becomes the write target, a name already
    /// holding the *same* bytes means the write can be skipped, and a name
    /// holding *different* bytes falls through to the next candidate. Errors if
    /// even the long-checksum name is taken by different content.
    pub(crate) fn resolve_output_path(
        media_file: &MediaFileInfo,
        derived: &MediaFileDerivedInfo,
        output_container: &dyn FileSystem,
    ) -> anyhow::Result<DeDuplicationResult> {
        let Some(desired_output_path) = &derived.desired_media_path else {
            debug!("  No desired media path for file: {media_file:?}");
            return Err(anyhow!("No desired media path for file: {media_file:?}"));
        };
        let short_checksum = &media_file.hash_info.short_checksum;
        let long_checksum = &media_file.hash_info.long_checksum;
        let suffixes = [
            String::new(),
            format!("-{short_checksum}"),
            format!("-{long_checksum}"),
        ];

        for suffix in suffixes {
            let desired_output_path_with_ext = format!(
                "{}{}.{}",
                desired_output_path, suffix, derived.desired_media_extension
            );
            if !output_container.exists(&desired_output_path_with_ext) {
                return Ok(DeDuplicationResult::WritePath(desired_output_path_with_ext));
            }
            let es_o = is_existing_file_same(
                output_container,
                long_checksum,
                &desired_output_path_with_ext,
            );
            match es_o {
                Some(true) => {
                    debug!(
                        "  No need to write, file already exists with same checksum: {desired_output_path_with_ext}"
                    );
                    return Ok(DeDuplicationResult::SkipWrite(desired_output_path_with_ext));
                }
                Some(false) => {
                    warn!(
                        "  Existing file is different, trying a checksum-suffixed name: {desired_output_path_with_ext}"
                    );
                    // fall through to the next candidate suffix
                }
                None => {
                    warn!(
                        "  Could not determine if existing file is same or different {desired_output_path_with_ext}",
                    );
                    return Err(anyhow!(
                        "Could not determine if existing file is same or different: {desired_output_path:?}"
                    ));
                }
            }
        }
        Err(anyhow!(format!(
            "No free output name (bare, short- or long-checksum) for: {desired_output_path:?}"
        )))
    }
}

/// Collapse a byte-identical duplicate into `canonical`.
///
/// Parallel inspection means "first one wins" would let a thread race decide
/// which entry's metadata (modified time, sidecar, the path whose bytes get
/// written) survives. Instead the entry whose source path sorts first is kept
/// canonical, and every original path is gathered into a sorted, de-duplicated
/// list. The chosen entry is therefore independent of inspection order.
fn merge_into(canonical: &mut MediaFileInfo, dup: MediaFileInfo) {
    let mut paths = std::mem::take(&mut canonical.original_path);
    paths.extend(dup.original_path.iter().cloned());

    if dup.original_file_this_run < canonical.original_file_this_run {
        *canonical = dup;
    }

    paths.sort();
    paths.dedup();
    canonical.original_path = paths;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::OsFileSystem;
    use crate::util::HashInfo;
    use anyhow::anyhow;

    /// Build a media entry with a controllable source path and content hash so
    /// tests can exercise collapsing and ordering directly.
    fn media_with(path: &str, long_checksum: &str) -> MediaFileInfo {
        let mut m = MediaFileInfo::new_for_test();
        m.original_file_this_run = path.to_string();
        m.original_path = vec![path.to_string()];
        m.hash_info = HashInfo {
            short_checksum: long_checksum.chars().take(7).collect(),
            long_checksum: long_checksum.to_string(),
        };
        m
    }

    #[test]
    fn test_resolve_no_collision_uses_bare_name() -> anyhow::Result<()> {
        // Nothing exists at the desired path, so it is written as-is.
        let c = OsFileSystem::new("test");
        let mfi = MediaFileInfo::new_for_test();
        let derived =
            MediaFileDerivedInfo::new_for_test(Some("duplicates/fresh-name".to_string()), "txt");
        let res = Deduplicator::resolve_output_path(&mfi, &derived, &c)?;
        assert_eq!(
            res,
            DeDuplicationResult::WritePath("duplicates/fresh-name.txt".to_string())
        );
        Ok(())
    }

    #[test]
    fn test_resolve_base_collision_uses_short_checksum() -> anyhow::Result<()> {
        // `duplicates/one.txt` exists with different content, so we go straight
        // to the short-checksum suffix (no -1/-2 counter).
        let c = OsFileSystem::new("test");
        let mfi = MediaFileInfo::new_for_test();
        let derived = MediaFileDerivedInfo::new_for_test(Some("duplicates/one".to_string()), "txt");
        let res = Deduplicator::resolve_output_path(&mfi, &derived, &c)?;
        assert_eq!(
            res,
            DeDuplicationResult::WritePath("duplicates/one-tsc.txt".to_string())
        );
        Ok(())
    }

    #[test]
    fn test_resolve_short_checksum_collision_falls_back_to_long() -> anyhow::Result<()> {
        // Both `short-clash.txt` and `short-clash-tsc.txt` exist with different
        // content, so the long checksum is used.
        let c = OsFileSystem::new("test");
        let mfi = MediaFileInfo::new_for_test();
        let derived =
            MediaFileDerivedInfo::new_for_test(Some("duplicates/short-clash".to_string()), "txt");
        let res = Deduplicator::resolve_output_path(&mfi, &derived, &c)?;
        assert_eq!(
            res,
            DeDuplicationResult::WritePath("duplicates/short-clash-tlc.txt".to_string())
        );
        Ok(())
    }

    #[test]
    fn test_resolve_all_candidates_taken_errors() -> anyhow::Result<()> {
        // The bare, short- and long-checksum names all exist with different
        // content, so there is nowhere to write.
        let c = OsFileSystem::new("test");
        let mfi = MediaFileInfo::new_for_test();
        let derived =
            MediaFileDerivedInfo::new_for_test(Some("duplicates/too-many".to_string()), "txt");
        let res = Deduplicator::resolve_output_path(&mfi, &derived, &c);
        assert_eq!(res.ok(), None);
        Ok(())
    }

    #[test]
    fn test_resolve_skips_when_identical_file_exists() -> anyhow::Result<()> {
        // The desired path already holds a byte-identical file (matching long
        // checksum), so there is nothing to write.
        let c = OsFileSystem::new("test");
        let mut mfi = MediaFileInfo::new_for_test();
        mfi.hash_info = HashInfo {
            short_checksum: "6bfdabd".to_string(),
            long_checksum: "6bfdabd4fc33d112283c147acccc574e770bbe6fbdbc3d4da968ba7b606ecc2f"
                .to_string(),
        };
        let derived = MediaFileDerivedInfo::new_for_test(Some("Canon_40D".to_string()), "jpg");
        let res = Deduplicator::resolve_output_path(&mfi, &derived, &c)?;
        assert_eq!(
            res,
            DeDuplicationResult::SkipWrite("Canon_40D.jpg".to_string())
        );
        Ok(())
    }

    #[test]
    fn test_collapses_files_with_same_checksum() -> anyhow::Result<()> {
        let mut d = Deduplicator::new();
        d.add(media_with("a/photo.jpg", "hashX"));
        d.add(media_with("b/photo.jpg", "hashX"));

        assert_eq!(d.by_checksum().len(), 1);
        let entry = d
            .by_checksum()
            .get("hashX")
            .ok_or_else(|| anyhow!("collapsed entry missing"))?;
        assert_eq!(
            entry.original_path,
            vec!["a/photo.jpg".to_string(), "b/photo.jpg".to_string()]
        );
        Ok(())
    }

    #[test]
    fn test_collapse_canonical_entry_is_order_independent() -> anyhow::Result<()> {
        // The same two byte-identical files, added in opposite orders, must
        // collapse to the same canonical entry and the same path list.
        let mut forward = Deduplicator::new();
        forward.add(media_with("b/photo.jpg", "hashX"));
        forward.add(media_with("a/photo.jpg", "hashX"));

        let mut reverse = Deduplicator::new();
        reverse.add(media_with("a/photo.jpg", "hashX"));
        reverse.add(media_with("b/photo.jpg", "hashX"));

        let f = forward
            .by_checksum()
            .get("hashX")
            .ok_or_else(|| anyhow!("forward entry missing"))?;
        let r = reverse
            .by_checksum()
            .get("hashX")
            .ok_or_else(|| anyhow!("reverse entry missing"))?;

        // Lowest-sorting source path wins as canonical, regardless of add order.
        assert_eq!(f.original_file_this_run, "a/photo.jpg");
        assert_eq!(r.original_file_this_run, "a/photo.jpg");
        assert_eq!(f.original_path, r.original_path);
        Ok(())
    }

    #[test]
    fn test_sorted_media_is_stable() {
        let mut d = Deduplicator::new();
        d.add(media_with("p3", "cccc"));
        d.add(media_with("p1", "aaaa"));
        d.add(media_with("p2", "bbbb"));

        let order: Vec<String> = d
            .sorted_media()
            .iter()
            .map(|m| m.hash_info.long_checksum.clone())
            .collect();
        assert_eq!(order, vec!["aaaa", "bbbb", "cccc"]);
    }
}
