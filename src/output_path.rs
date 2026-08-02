//! Where a media file lands in the output tree.
//!
//! This is a decision about the archive's layout, not about the file's
//! metadata - it only *consumes* [`crate::metadata::reconcile`]'s answer for the
//! capture date. It lives apart from `metadata` for that reason: a change to how
//! photos are foldered should not touch a parser, and vice versa.

use crate::file_type::file_ext_from_file_type;
use crate::metadata::MediaFileInfo;
use crate::metadata::reconcile::best_guess_taken;
use crate::metadata::taken::Taken;
use chrono::{Datelike, Timelike};

#[derive(Debug)]
pub(crate) struct MediaFileDerivedInfo {
    /// Desired path relative to output directory, minus the dot and file extension (eg, 2025/09/10/1234-56-789)
    pub(crate) desired_media_path: String,
    /// Desired file extension (eg, jpg, mp4)
    pub(crate) desired_media_extension: String,
}

pub(crate) fn media_file_derived_from_media_info(
    media_info: &MediaFileInfo,
) -> MediaFileDerivedInfo {
    MediaFileDerivedInfo {
        desired_media_path: get_desired_media_path(
            &media_info.hash_info.short_checksum,
            best_guess_taken(media_info).as_ref(),
        ),
        desired_media_extension: file_ext_from_file_type(&media_info.accurate_file_type),
    }
}

/// `yyyy/mm/dd/hhmm-ssms`, or `undated/checksum` when no source had a date.
///
/// Only [`Taken::local`] is read. The digits a source recorded are the digits
/// that go in the path - whether it also knew the zone they were read in is
/// [`crate::metadata::reconcile`]'s business, and settled before this is called.
///
/// This used to take the RFC 3339 string and parse it back, which meant a date
/// could be lost to a failed round trip and filed under `undated/` despite
/// having been found. Taking the value itself means the only way to be undated
/// is to have no date at all.
pub(crate) fn get_desired_media_path(short_checksum: &str, taken: Option<&Taken>) -> String {
    let Some(dt) = taken.map(|t| t.local) else {
        return format!("undated/{short_checksum}");
    };
    // The year is padded to four like everything else, so a camera with a dead
    // clock reporting year 12 cannot land in `12/` beside a real `2012/`.
    format!(
        "{:04}/{:02}/{:02}/{:02}{:02}-{:02}{:03}",
        dt.year(),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        dt.and_utc().timestamp_subsec_millis()
    )
}

#[cfg(test)]
impl MediaFileDerivedInfo {
    pub(crate) fn new_for_test(desired_media_path: &str, desired_media_extension: &str) -> Self {
        MediaFileDerivedInfo {
            desired_media_path: desired_media_path.to_string(),
            desired_media_extension: desired_media_extension.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{FileSystem, OsFileSystem};

    #[test]
    fn test_desired_media_path() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        use crate::util::checksum_bytes;

        let c = OsFileSystem::new("test");
        let mut reader = c.open("Canon_40D.jpg")?;
        let short_checksum = checksum_bytes(&mut reader)?.short_checksum;
        let taken = |s: &str| Taken::parse(s);

        assert_eq!(
            get_desired_media_path(&short_checksum, None),
            "undated/6bfdabd".to_string()
        );
        assert_eq!(
            get_desired_media_path(&short_checksum, taken("2008-05-30T15:56:01Z").as_ref()),
            "2008/05/30/1556-01000".to_string()
        );
        assert_eq!(
            get_desired_media_path(&short_checksum, taken("2008-05-30T15:56:01.009Z").as_ref()),
            "2008/05/30/1556-01009".to_string()
        );
        Ok(())
    }

    /// The digits are filed exactly as the source recorded them. A reading taken
    /// at UTC+13 belongs in that evening's directory, not in the UTC morning it
    /// happens to coincide with - which is what reading the local part, rather
    /// than an instant, guarantees.
    #[test]
    fn test_a_zoned_reading_files_under_its_own_clock() -> anyhow::Result<()> {
        use anyhow::anyhow;
        let zoned = Taken::parse("2023-01-18T21:05:38+13:00").ok_or_else(|| anyhow!("parse"))?;
        assert_eq!(
            get_desired_media_path("abc1234", Some(&zoned)),
            "2023/01/18/2105-38000"
        );
        Ok(())
    }

    /// The year is padded like every other component, so a camera with a dead
    /// clock cannot produce a directory that sorts among the real ones.
    #[test]
    fn test_short_year_is_padded() -> anyhow::Result<()> {
        use anyhow::anyhow;
        let early = Taken::parse("0012-03-04T05:06:07").ok_or_else(|| anyhow!("parse"))?;
        assert_eq!(
            get_desired_media_path("abc1234", Some(&early)),
            "0012/03/04/0506-07000"
        );
        Ok(())
    }
}
