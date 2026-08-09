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

/// Collects inspected media and removes duplicates, by content
/// ([`Deduplicator::add`], on the sha256) and by output path
/// ([`Deduplicator::resolve_output_path`]).
///
/// Both are deliberately deterministic: re-running over the same input produces
/// the same canonical entries, write order and output names. Parallel inspection
/// yields files in whatever order the workers finish, so anything order-sensitive
/// has to be pinned here rather than left to that race.
pub(crate) struct Deduplicator {
    by_checksum: HashMap<String, MediaFileInfo>,
}

impl Deduplicator {
    pub(crate) fn new() -> Self {
        Self {
            by_checksum: HashMap::new(),
        }
    }

    /// Fold one inspected file in, collapsing it onto any entry with the same
    /// content hash.
    pub(crate) fn add(&mut self, media: MediaFileInfo) {
        let checksum = media.hash_info.long_checksum.clone();
        match self.by_checksum.get_mut(&checksum) {
            Some(existing) => merge_into(existing, media),
            None => {
                self.by_checksum.insert(checksum, media);
            }
        }
    }

    pub(crate) fn by_checksum(&self) -> &HashMap<String, MediaFileInfo> {
        &self.by_checksum
    }

    /// All deduplicated media ordered by long checksum.
    ///
    /// When two distinct files want the same bare name the first writer claims
    /// it and the rest take a checksum suffix. Iterating the `HashMap` directly
    /// would let that first writer flip every run.
    pub(crate) fn sorted_media(&self) -> Vec<&MediaFileInfo> {
        let mut media: Vec<&MediaFileInfo> = self.by_checksum.values().collect();
        media.sort_by(|a, b| a.hash_info.long_checksum.cmp(&b.hash_info.long_checksum));
        media
    }

    /// Resolve the path a media file should be written to, given what is already
    /// in `output_container`.
    ///
    /// Names are tried bare, then suffixed with the short then the long checksum.
    /// A free name becomes the write target, one already holding the *same* bytes
    /// means the write can be skipped, and one holding *different* bytes falls
    /// through to the next candidate; running out is an error.
    ///
    /// The suffix comes from the file's own bytes, so a collision name never
    /// depends on processing order or on what else landed on the same name.
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
/// The entry whose source path sorts first is kept, rather than the first one
/// seen — under parallel inspection that would let a thread race decide whose
/// metadata survives.
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

    /// The candidate ladder, walked against the `test/duplicates` fixtures. A
    /// name already taken by *different* content escalates to the short checksum
    /// (`tsc` for a test record), then the long one (`tlc`), then gives up —
    /// there is deliberately no `-1`/`-2` counter, so a name is reproducible from
    /// the file's content alone.
    #[test]
    fn test_resolve_output_path_candidate_ladder() -> anyhow::Result<()> {
        let c = OsFileSystem::new("test");
        let mfi = MediaFileInfo::new_for_test();

        // desired stem, resolved name — None when every candidate is taken
        let cases: [(&str, Option<&str>); 4] = [
            ("duplicates/fresh-name", Some("duplicates/fresh-name.txt")),
            ("duplicates/one", Some("duplicates/one-tsc.txt")),
            (
                "duplicates/short-clash",
                Some("duplicates/short-clash-tlc.txt"),
            ),
            ("duplicates/too-many", None),
        ];

        for (stem, expected) in cases {
            let derived = MediaFileDerivedInfo::new_for_test(Some(stem.to_string()), "txt");
            let res = Deduplicator::resolve_output_path(&mfi, &derived, &c);
            match expected {
                Some(path) => assert_eq!(
                    res.ok(),
                    Some(DeDuplicationResult::WritePath(path.to_string())),
                    "resolving {stem}"
                ),
                None => assert_eq!(res.ok(), None, "resolving {stem}"),
            }
        }
        Ok(())
    }

    #[test]
    fn test_resolve_skips_when_identical_file_exists() -> anyhow::Result<()> {
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

        // Lowest-sorting source path wins, regardless of add order.
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
