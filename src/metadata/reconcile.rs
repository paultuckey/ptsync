//! Deciding what to believe when the four metadata sources disagree.
//!
//! Every function here reduces a [`MediaFileInfo`] to one answer, and the ranking
//! is the same throughout: **human before camera**. A reading someone
//! deliberately corrected is worth more than the one a device happened to
//! record, because correcting it is precisely the act of saying the device was
//! wrong.
//!
//! This is the *only* place the sources are ranked. Both consumers - the
//! frontmatter in a photo's note ([`crate::markdown::mfm_from_media_file_info`])
//! and the `media_item` columns the `db` command writes ([`crate::db_cmd`]) -
//! call these, so the note and the index cannot disagree about a photo. They
//! once had separate copies that had drifted apart.

use super::MediaFileInfo;
use super::exif::best_guess_taken_exif;
use super::taken::Taken;
use chrono::{FixedOffset, Timelike};

/// Best guess at the date the photo was taken from messy optional data.
///
/// *Human tier* — someone chose this value:
/// 1. XMP sidecar `photoshop:DateCreated` - a correction made in Lightroom,
///    darktable or digiKam
/// 2. SupplementalInfo photo_taken_time - the date Google Photos displays, which
///    the user can edit in its UI
///
/// *Camera tier* — the device's own reading, whichever the file carries
/// ([`embedded_reading`]):
/// 3. EXIF DateTimeOriginal, then DateTime, then GPSDateStamp - the last
///    accurate only to the *day*, since `GPSTimeStamp` is not read
/// 4. Track metadata for a video, which has no EXIF - Apple's
///    `com.apple.quicktime.creationdate` if present, else the `mvhd` date
///
/// *Fallback tier* — not a capture time at all, only better than nothing:
/// 5. SupplementalInfo creation_time - when the file was *uploaded* to Google,
///    which is why it ranks below the camera despite sharing a file with 2
/// 6. File modified time
///   - no timezone info, unreliable in zips, somewhat unreliable in directories due to file
///     copying / syncing not preserving, only use as second to last resort
/// 7. File creation time
///   - no timezone info, unavailable in zips, somewhat unreliable in directories due to file
///     copying / syncing not preserving, only use as a last resort
///
/// Result returned as an RFC 3339 string, for the note and the index. The
/// output path does not go through this — it reads [`Taken::local`] directly,
/// so a date can never be lost to a round trip through a string.
///
/// The ranking settles *which reading to believe*, and only that. Whether the
/// winner has any digits worth filing is a separate question, settled by
/// [`best_guess_taken`].
pub(crate) fn best_guess_taken_dt(info: &MediaFileInfo) -> Option<String> {
    Some(best_guess_taken(info)?.to_rfc3339())
}

/// The same answer as [`best_guess_taken_dt`], before it is flattened into a
/// string that cannot say whether the offset on the end is real.
///
/// The ranking picks a winner. One thing can then go wrong with it: an
/// epoch-valued source knows the instant and not the clock, so its digits are
/// UTC standing in for a wall clock nobody recorded, and filing them lands the
/// photo in the small hours of the right day rather than the afternoon it was
/// taken.
///
/// The repair is to look at the file's *embedded* reading — the camera's own, or
/// the video container's — which is the half Takeout drops and the file usually
/// keeps. When the two describe one shutter press ([`plausible_offset`]), that
/// reading has the digits the winner lacks, and swapping them in is the whole of
/// it. Nothing is reconstructed: the digits are used as they were found.
///
/// A winner that already has digits keeps them, and asks the embedded reading
/// only for a fraction of a second it may be missing. A reading with nothing to
/// corroborate it stays uncertain and files under its UTC digits, which is wrong
/// by one offset and the best that can be done with no offset to hand.
pub(crate) fn best_guess_taken(info: &MediaFileInfo) -> Option<Taken> {
    let winner = best_guess_taken_ranked(info)?;
    let Some(embedded) = embedded_reading(info) else {
        return Some(winner);
    };
    let Some(offset) = plausible_offset(&winner, &embedded) else {
        return Some(winner);
    };
    Some(if winner.certain {
        // The winner is already a local reading; only the fraction is in doubt.
        winner.with_fraction_from(&embedded)
    } else {
        // The winner knew the instant and the embedded reading the digits.
        // Together they also name the zone, which is worth recording even though
        // no path reads it.
        embedded.with_fraction_from(&winner).or_offset(offset)
    })
}

/// The reading embedded in the file's own bytes: EXIF for an image, the track
/// metadata for a video.
///
/// Never both — [`crate::metadata::media_file_info_from_readable`] runs one
/// parser or the other according to the file's real type — so the `or_else` is a
/// choice between an image's answer and a video's, not a ranking.
///
/// Videos reach this for the same reason images do. A Takeout clip arrives with
/// `photoTakenTime` and no offset exactly as a photo does, and Apple's
/// `com.apple.quicktime.creationdate` states the zone outright; consulting only
/// EXIF here filed every one of them under UTC.
fn embedded_reading(info: &MediaFileInfo) -> Option<Taken> {
    best_guess_taken_exif(&info.exif_info)
        .or_else(|| info.track_info.as_ref().and_then(|ti| ti.taken_at()))
}

/// The offset between two readings, when they are one shutter press spelled two
/// ways — and `None` when they are not.
///
/// Two tests, and neither rounds anything:
///
/// - **Same second-of-minute.** Requiring the same epoch second is the obvious
///   rule and it fails on the common case: a camera that writes no `OffsetTime`
///   gives a bare wall clock, so a photo taken at UTC+13 reads 13 hours off the
///   `photoTakenTime` for the same shutter press. Every real UTC offset is a
///   whole number of minutes, which leaves the second-of-minute untouched by the
///   discrepancy — it is the one field that can be compared without knowing the
///   zone.
/// - **A difference no larger than a real offset**, `-12:00 ..= +14:00`. The
///   second-of-minute match is loose enough to hit by chance once in sixty, and
///   this is what bounds it. Beyond that the two are not one instant spelled two
///   ways but two different instants — someone re-dated the photo in Google
///   Photos — and nothing the camera recorded belongs to the second they chose.
///
/// The difference is taken on whole seconds. A fraction on either side is one
/// Takeout truncated away when it stored its timestamp, and keeping it would
/// make an exact offset look like a ragged one.
///
/// Given the first test the difference is necessarily a whole number of minutes,
/// since a Unix second modulo 60 *is* the second-of-minute — so there is no
/// separate check for that, and never was one that did any work.
fn plausible_offset(winner: &Taken, embedded: &Taken) -> Option<FixedOffset> {
    const MIN_OFFSET_S: i64 = -12 * 3600;
    const MAX_OFFSET_S: i64 = 14 * 3600;

    if winner.local.second() != embedded.local.second() {
        return None;
    }
    let seconds = embedded.local.and_utc().timestamp() - winner.local.and_utc().timestamp();
    if !(MIN_OFFSET_S..=MAX_OFFSET_S).contains(&seconds) {
        return None;
    }
    FixedOffset::east_opt(i32::try_from(seconds).ok()?)
}

/// The source ranking itself — see [`best_guess_taken`], which repairs what this
/// returns.
///
/// Each source is built through the [`Taken`] constructor that says what it
/// actually knew. That is not a property of the *date* but of where it came
/// from, which is why it is decided here beside the ranking rather than inferred
/// later from a string: an epoch-valued source can only ever yield an instant,
/// and no amount of looking at `2024-05-22T00:17:51+00:00` afterwards will
/// reveal that the `+00:00` was Google's and not a photographer's.
fn best_guess_taken_ranked(info: &MediaFileInfo) -> Option<Taken> {
    // -- human tier --
    if let Some(dt) = info
        .xmp_info
        .as_ref()
        .and_then(|x| x.datetime.as_deref())
        .and_then(Taken::parse)
    {
        return Some(dt);
    }
    // Takeout exports a Unix timestamp and no offset, so what a photo's clock
    // read is simply not in the json - see `SupplementalInfoDateTime`.
    if let Some(dt) = info
        .supp_info
        .as_ref()
        .and_then(|si| si.photo_taken_time.as_ref())
        .and_then(|si_dt| si_dt.timestamp_as_utc())
    {
        return Some(Taken::instant(dt));
    }

    // -- camera tier --
    // EXIF for an image, the track's own date for a video: whichever the file
    // carries, and the same value the repair above consults.
    if let Some(dt) = embedded_reading(info) {
        return Some(dt);
    }

    // -- fallback tier --
    // Google's `creationTime` is the upload timestamp, not a capture time, so it
    // sits below the camera's reading even though `photoTakenTime` - from the
    // same JSON file - sits above it.
    if let Some(dt) = info
        .supp_info
        .as_ref()
        .and_then(|si| si.creation_time.as_ref())
        .and_then(|si_dt| si_dt.timestamp_as_utc())
    {
        return Some(Taken::instant(dt));
    }
    // A filesystem timestamp is an epoch count like Google's, and just as silent
    // about the clock on the wall. Nothing can repair one: by the ranking, these
    // are only reached when the file carries no embedded reading at all.
    if let Some(dt) = info.modified.and_then(crate::util::timestamp_to_utc) {
        return Some(Taken::instant(dt));
    }
    if let Some(dt) = info.created.and_then(crate::util::timestamp_to_utc) {
        return Some(Taken::instant(dt));
    }
    None
}

/// Best guess at where the media was taken, as `(latitude, longitude)`.
///
/// *Human tier* — someone chose this value:
/// 1. XMP sidecar `exif:GPSLatitude`/`GPSLongitude` - a location set in
///    Lightroom, darktable or digiKam
/// 2. SupplementalInfo `geo_data` - the location Google Photos displays, which
///    the user can edit in its UI
///
/// *Camera tier* — the device's own fix:
/// 3. EXIF GPS (embedded in images)
/// 4. Track ISO 6709 GPS (embedded in videos)
/// 5. SupplementalInfo `geo_data_exif` - Google's *copy* of the EXIF fix, so it
///    belongs with the camera despite sharing a file with 2. It ranks last
///    because it only adds something when the file's own EXIF is gone, which
///    Takeout often does.
///
/// A `(0, 0)` pair is treated as absent at every source: EXIF and Takeout both
/// write zeros when they have no fix rather than omitting the value. Each parser
/// now drops those before they reach a struct
/// ([`crate::metadata::exif::parse_exif_info`], [`crate::metadata::xmp::parse_xmp`],
/// and `supplemental`'s own `parse_supplemental_info`), so nothing here or
/// downstream ever sees "null island" - the checks below stay as a backstop for
/// values built in code rather than read from a file.
pub(crate) fn best_guess_lat_long(info: &MediaFileInfo) -> Option<(f64, f64)> {
    use crate::util::non_zero_coords;

    // -- human tier --
    if let Some(xmp) = &info.xmp_info
        && let Some(coords) = non_zero_coords(xmp.latitude, xmp.longitude)
    {
        return Some(coords);
    }
    if let Some(supp) = &info.supp_info
        && let Some(geo) = supp.geo_data.as_ref()
        && let Some(coords) = non_zero_coords(geo.latitude, geo.longitude)
    {
        return Some(coords);
    }

    // -- camera tier --
    if let Some(exif) = &info.exif_info
        && let Some(coords) = non_zero_coords(exif.latitude, exif.longitude)
    {
        return Some(coords);
    }
    if let Some(track) = &info.track_info
        && let Some(coords) = non_zero_coords(track.latitude, track.longitude)
    {
        return Some(coords);
    }
    if let Some(supp) = &info.supp_info
        && let Some(geo) = supp.geo_data_exif.as_ref()
        && let Some(coords) = non_zero_coords(geo.latitude, geo.longitude)
    {
        return Some(coords);
    }
    None
}

/// Best guess at a title someone gave the media, or `None` when nobody did.
///
/// Both sources are the *human tier* — a title is nothing but a human opinion,
/// so there is no camera tier to fall back to:
///
/// 1. XMP sidecar `dc:title` — set in Lightroom, darktable or digiKam
/// 2. SupplementalInfo `title` — set in Google Photos, and only present when it
///    is not the file's own name (see
///    [`PsSupplementalInfo::drop_file_name_title`](crate::metadata::supplemental::PsSupplementalInfo))
///
/// XMP leads for the same reason it leads everywhere else here: a value written
/// by a tool the user drove deliberately outranks one from a service that fills
/// the field in by default.
pub(crate) fn best_guess_title(info: &MediaFileInfo) -> Option<String> {
    let xmp = info.xmp_info.as_ref().and_then(|x| x.title.as_deref());
    let supp = info.supp_info.as_ref().and_then(|s| s.title.as_deref());
    non_blank(xmp)
        .or_else(|| non_blank(supp))
        .map(str::to_string)
}

/// Best guess at a description someone wrote for the media, ranked exactly as
/// [`best_guess_title`]: XMP `dc:description`, then Google Photos' caption.
pub(crate) fn best_guess_description(info: &MediaFileInfo) -> Option<String> {
    let xmp = info
        .xmp_info
        .as_ref()
        .and_then(|x| x.description.as_deref());
    let supp = info
        .supp_info
        .as_ref()
        .and_then(|s| s.description.as_deref());
    non_blank(xmp)
        .or_else(|| non_blank(supp))
        .map(str::to_string)
}

/// The rating someone gave the media: 0–5, or -1 for "rejected".
///
/// XMP `xmp:Rating` is the only source — no export format ptsync reads carries a
/// star rating of its own. It is resolved here anyway so the note, the index and
/// the `info` report all read it the same way, and so this doc comment has one
/// place to say what does *not* feed it: a Google favourite is not a five-star
/// rating. Tools like PhotoSync and Synology Photos do write favourites out as
/// `xmp:Rating` 5, but reading that convention backwards would invent a rating
/// nobody gave, and since a rating is seeded into a note once and then left
/// alone, the invented value could never afterwards be told from a real one.
pub(crate) fn best_guess_rating(info: &MediaFileInfo) -> Option<i64> {
    info.xmp_info.as_ref().and_then(|x| x.rating)
}

/// Whether the media is a favourite.
///
/// Google's `favorited` is the only source. XMP has no favourite property at
/// all: the spec defines `xmp:Rating` (-1, or 0–5) and nothing else, and
/// Lightroom's pick flags never leave its catalog. `xmp:Rating == 5` is
/// deliberately *not* read as a favourite — someone rating a shoot in darktable
/// means "best of these", not "starred in Google Photos", and collapsing the two
/// would silently promote every top-rated photo. See [`best_guess_rating`] for
/// the same argument in the other direction.
pub(crate) fn best_guess_favorite(info: &MediaFileInfo) -> bool {
    info.supp_info.as_ref().is_some_and(|s| s.favorited)
}

/// Whether the media was archived — hidden from the main Google Photos grid but
/// not deleted.
///
/// Google's `archived` is the only source. `xmp:Rating == -1` ("rejected") is
/// deliberately not read as archived: rejecting a frame is a judgement about the
/// photograph, archiving it is a decision about which grid it shows up in, and
/// neither implies the other.
///
/// Takeout also expresses this structurally, by exporting the file under an
/// `Archive/` directory. That signal is *not* used here, because
/// [`classify_dir`](crate::classify::classify_dir)'s `^archive` pattern is
/// anchored at the scan root and so matches any ordinary folder of that name —
/// on a real archive it fires on things like `archive/2009/timesheets`. Reading
/// it would mark a whole backup tree as archived. Tightening the pattern first
/// is what would make the directory usable.
pub(crate) fn best_guess_archived(info: &MediaFileInfo) -> bool {
    info.supp_info.as_ref().is_some_and(|s| s.archived)
}

/// Trim a value and treat one that is all whitespace as absent — sidecar writers
/// emit empty strings for fields they have nothing for.
fn non_blank(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kinds, not the digits. Every source lands in the [`Taken`] shape
    /// that matches what it actually knew, and the two that spell themselves
    /// `+00:00` are no longer the same value.
    ///
    /// This is what makes a later offset repair possible: it can find the
    /// `Instant`s - the readings with no wall clock of their own - without
    /// having to guess which `+00:00` was real.
    #[test]
    fn test_each_source_reports_what_it_knew_about_the_zone() -> anyhow::Result<()> {
        use crate::metadata::exif::PsExifInfo;
        use crate::metadata::supplemental::{PsSupplementalInfo, SupplementalInfoDateTime};
        use crate::metadata::track::PsTrackInfo;
        use anyhow::anyhow;
        use std::collections::HashMap;

        // The three shapes a source can arrive in, read back off the two fields
        // that record them: whether the digits are the photographer's, and
        // whether anything said which zone they were read in.
        let kind_of = |info: &MediaFileInfo| -> anyhow::Result<String> {
            let taken = best_guess_taken(info).ok_or_else(|| anyhow!("no date guessed"))?;
            Ok(match (taken.certain, taken.offset) {
                (false, _) => "instant".to_string(),
                (true, None) => "wall".to_string(),
                (true, Some(_)) => "zoned".to_string(),
            })
        };

        // A file's mtime is an epoch count and says nothing about a wall clock.
        let mut info = MediaFileInfo::new_for_test();
        info.modified = Some(1_000_000_000_000);
        assert_eq!(kind_of(&info)?, "instant");

        // A video's `mvhd` date records no zone, so it claims none.
        info = MediaFileInfo::new_for_test();
        info.track_info = Some(PsTrackInfo {
            creation_time: Some("2024-04-18T11:24:26+00:00".to_string()),
            ..Default::default()
        });
        assert_eq!(kind_of(&info)?, "wall");

        // Apple's Keys atom on the same clip does, and outranks it.
        if let Some(ti) = info.track_info.as_mut() {
            ti.tags.insert(
                "com.apple.quicktime.creationdate".to_string(),
                "2023-01-18T21:05:38+1300".to_string(),
            );
        }
        assert_eq!(kind_of(&info)?, "zoned");

        // A bare EXIF reading is the camera's display and nothing more.
        info = MediaFileInfo::new_for_test();
        let with_date = |raw: &str| {
            let mut tags = HashMap::new();
            tags.insert(
                nom_exif::ExifTag::DateTimeOriginal.to_string(),
                raw.to_string(),
            );
            Some(PsExifInfo {
                tags,
                ..Default::default()
            })
        };
        info.exif_info = with_date("2014:12:25 14:51:26");
        assert_eq!(kind_of(&info)?, "wall");

        // ...and one with an `OffsetTime` tag knows both halves.
        info.exif_info = with_date("2014-12-25T14:51:26+11:00");
        assert_eq!(kind_of(&info)?, "zoned");

        // Google's `photoTakenTime` outranks EXIF and is a bare epoch, so on its
        // own it wins the ranking and loses the wall clock.
        let takeout = || {
            Some(PsSupplementalInfo {
                photo_taken_time: Some(SupplementalInfoDateTime::new_for_test("1419479486")),
                ..PsSupplementalInfo::default()
            })
        };
        info = MediaFileInfo::new_for_test();
        info.supp_info = takeout();
        assert_eq!(kind_of(&info)?, "instant");

        // ...but with the camera's reading still in the file to say what the
        // clock read, it is an instant no longer. This is `with_exif_zone`.
        info.exif_info = with_date("2014-12-25T14:51:26+11:00");
        assert_eq!(kind_of(&info)?, "zoned");
        info.exif_info = with_date("2014:12:25 14:51:26");
        assert_eq!(kind_of(&info)?, "zoned");
        Ok(())
    }

    /// A Takeout photo whose EXIF date Google stripped, which is the case
    /// nothing can repair: the instant is known, no reading of the clock
    /// survives anywhere, and there is nothing to derive an offset from.
    ///
    /// It files under its UTC digits, in the small hours of the right day rather
    /// than the afternoon it was taken. That is the residue `--assume-timezone`
    /// would be for; it is not something to guess at here.
    #[test]
    fn test_an_instant_with_nothing_to_corroborate_it_files_as_utc() -> anyhow::Result<()> {
        use crate::metadata::supplemental::{PsSupplementalInfo, SupplementalInfoDateTime};

        let mut info = MediaFileInfo::new_for_test();
        info.supp_info = Some(PsSupplementalInfo {
            // 2014-12-25T03:51:26Z - a quarter to three in the afternoon at
            // UTC+11, filed in the small hours because the offset is gone.
            photo_taken_time: Some(SupplementalInfoDateTime::new_for_test("1419479486")),
            ..PsSupplementalInfo::default()
        });
        assert_eq!(
            best_guess_taken_dt(&info).as_deref(),
            Some("2014-12-25T03:51:26+00:00")
        );
        assert_eq!(
            crate::output_path::get_desired_media_path("abc1234", best_guess_taken(&info).as_ref()),
            "2014/12/25/0351-26000"
        );
        Ok(())
    }

    /// The repair itself, end to end and in the units that matter: the same
    /// photo, the same Takeout json, filed in the afternoon it was taken because
    /// the camera's reading was still in the file to say so.
    ///
    /// Both spellings of the camera's half get there. The `+11:00` case is the
    /// offset stated outright; the bare case derives it, and 14:51:26 local
    /// against 03:51:26Z is exactly eleven hours - no rounding involved, and
    /// none wanted.
    #[test]
    fn test_exif_puts_a_takeout_photo_back_in_its_own_afternoon() -> anyhow::Result<()> {
        use crate::metadata::exif::PsExifInfo;
        use crate::metadata::supplemental::{PsSupplementalInfo, SupplementalInfoDateTime};
        use std::collections::HashMap;

        let mut info = MediaFileInfo::new_for_test();
        info.supp_info = Some(PsSupplementalInfo {
            photo_taken_time: Some(SupplementalInfoDateTime::new_for_test("1419479486")),
            ..PsSupplementalInfo::default()
        });
        for raw in ["2014-12-25T14:51:26+11:00", "2014:12:25 14:51:26"] {
            let mut tags = HashMap::new();
            tags.insert(nom_exif::ExifTag::DateTimeOriginal.to_string(), raw.into());
            info.exif_info = Some(PsExifInfo {
                tags,
                ..Default::default()
            });
            assert_eq!(
                best_guess_taken_dt(&info).as_deref(),
                Some("2014-12-25T14:51:26+11:00"),
                "from EXIF {raw:?}"
            );
            assert_eq!(
                crate::output_path::get_desired_media_path(
                    "abc1234",
                    best_guess_taken(&info).as_ref()
                ),
                "2014/12/25/1451-26000",
                "from EXIF {raw:?}"
            );
        }
        Ok(())
    }

    /// The guards, each on its own. A difference that is not a whole number of
    /// minutes, or is larger than any zone, is not an offset - and is left
    /// alone rather than tidied into one.
    #[test]
    fn test_a_ragged_difference_is_not_treated_as_a_zone() -> anyhow::Result<()> {
        use crate::metadata::exif::PsExifInfo;
        use crate::metadata::supplemental::{PsSupplementalInfo, SupplementalInfoDateTime};
        use std::collections::HashMap;

        // Google says 2014-12-25T03:51:26Z throughout.
        let filed_from = |exif_raw: &str| {
            let mut tags = HashMap::new();
            tags.insert(
                nom_exif::ExifTag::DateTimeOriginal.to_string(),
                exif_raw.to_string(),
            );
            let mut info = MediaFileInfo::new_for_test();
            info.supp_info = Some(PsSupplementalInfo {
                photo_taken_time: Some(SupplementalInfoDateTime::new_for_test("1419479486")),
                ..PsSupplementalInfo::default()
            });
            info.exif_info = Some(PsExifInfo {
                tags,
                ..Default::default()
            });
            best_guess_taken_dt(&info)
        };

        // Eleven hours and thirty seconds. A camera clock adrift, or two photos
        // conflated - either way not a zone, and snapping it to +11:00 would be
        // inventing one. The second-of-minute is what catches it: no offset can
        // move a clock's seconds, so 56 against 26 is already proof these are
        // two different presses.
        assert_eq!(
            filed_from("2014:12:25 14:51:56").as_deref(),
            Some("2014-12-25T03:51:26+00:00"),
            "readings that disagree on the second are not one shutter press"
        );
        // Same second-of-minute, but a year apart: someone re-dated the photo.
        assert_eq!(
            filed_from("2013:12:25 14:51:26").as_deref(),
            Some("2014-12-25T03:51:26+00:00"),
            "beyond 26 hours the two are different instants"
        );
        // A plausible whole-minute gap no zone actually reaches.
        assert_eq!(
            filed_from("2014:12:25 19:51:26").as_deref(),
            Some("2014-12-25T03:51:26+00:00"),
            "+16:00 is not a zone"
        );
        Ok(())
    }

    #[test]
    fn test_best_guess_taken_dt_timestamps() -> anyhow::Result<()> {
        use anyhow::anyhow;
        let mut info = MediaFileInfo::new_for_test();
        // 1000000000000 ms = 2001-09-09T01:46:40Z
        let ts = 1000000000000;

        info.created = Some(ts);
        info.modified = None;
        let dt =
            best_guess_taken_dt(&info).ok_or_else(|| anyhow!("Should have a date from created"))?;
        assert_eq!(dt, "2001-09-09T01:46:40+00:00");

        info.created = None;
        info.modified = Some(ts);
        let dt = best_guess_taken_dt(&info)
            .ok_or_else(|| anyhow!("Should have a date from modified"))?;
        assert_eq!(dt, "2001-09-09T01:46:40+00:00");

        // When both are present, modified wins over created (created is the very
        // last resort as it is unavailable in zips).
        info.modified = Some(ts);
        info.created = Some(1_600_000_000_000); // 2020-09-13T12:26:40Z
        let dt = best_guess_taken_dt(&info)
            .ok_or_else(|| anyhow!("Should have a date when both present"))?;
        assert_eq!(dt, "2001-09-09T01:46:40+00:00");
        Ok(())
    }

    /// Dates rank human before camera: a date someone corrected outranks the one
    /// the device recorded. Google's `creationTime` is the exception that proves
    /// the rule - it shares a file with `photoTakenTime` but is an *upload*
    /// timestamp, so it falls below the camera rather than above it.
    #[test]
    fn test_best_guess_taken_dt_human_beats_camera() {
        use crate::metadata::exif::PsExifInfo;
        use crate::metadata::supplemental::{
            PsSupplementalInfo, SupplementalInfoDateTime, SupplementalInfoPerson,
        };
        use std::collections::HashMap;

        // 2001-09-09T01:46:40Z, as seconds - the spelling Takeout uses.
        let supp = |photo_taken: Option<&str>, creation: Option<&str>| PsSupplementalInfo {
            people: Vec::<SupplementalInfoPerson>::new(),
            photo_taken_time: photo_taken.map(SupplementalInfoDateTime::new_for_test),
            creation_time: creation.map(SupplementalInfoDateTime::new_for_test),
            ..PsSupplementalInfo::default()
        };
        let exif_dated = |raw: &str| {
            let mut tags = HashMap::new();
            tags.insert("DateTimeOriginal".to_string(), raw.to_string());
            PsExifInfo {
                tags,
                ..Default::default()
            }
        };

        let mut info = MediaFileInfo::new_for_test();
        info.xmp_info = Some(crate::metadata::xmp::PsXmpInfo {
            datetime: Some("2015-01-01T00:00:00+00:00".to_string()),
            ..Default::default()
        });
        info.supp_info = Some(supp(Some("1000000000"), Some("1100000000")));
        info.exif_info = Some(exif_dated("2008:05:30 15:56:01"));

        // XMP is the most deliberate correction, so it wins outright.
        assert_eq!(
            best_guess_taken_dt(&info).as_deref(),
            Some("2015-01-01T00:00:00+00:00")
        );

        // Without XMP, Google's `photoTakenTime` - editable in the Photos UI -
        // is the remaining human source and still beats the camera's EXIF.
        info.xmp_info = None;
        assert_eq!(
            best_guess_taken_dt(&info).as_deref(),
            Some("2001-09-09T01:46:40+00:00")
        );

        // With no human source, EXIF - the camera's own reading - is used.
        info.supp_info = Some(supp(None, Some("1100000000")));
        assert_eq!(
            best_guess_taken_dt(&info).as_deref(),
            Some("2008-05-30T15:56:01+00:00")
        );

        // `creationTime` is an upload timestamp, so it only applies once the
        // camera has nothing to say - unlike `photoTakenTime` above it.
        info.exif_info = None;
        assert_eq!(
            best_guess_taken_dt(&info).as_deref(),
            Some("2004-11-09T11:33:20+00:00")
        );
    }

    /// A Takeout video, which is the case the repair used to walk straight past.
    ///
    /// The shape is identical to a photo's - `photoTakenTime` wins the ranking
    /// and knows only the instant - but the corroborating reading lives in the
    /// `moov` rather than in EXIF, and consulting only EXIF filed every clip in
    /// an export under its UTC digits. A phone shooting at nine in the evening
    /// at UTC+13 landed in the previous morning, a day away from the still
    /// beside it.
    #[test]
    fn test_a_takeout_video_is_repaired_from_its_own_track() -> anyhow::Result<()> {
        use crate::metadata::supplemental::{PsSupplementalInfo, SupplementalInfoDateTime};
        use crate::metadata::track::PsTrackInfo;

        // 2023-01-18T08:05:38Z, the instant Google recorded.
        let mut info = MediaFileInfo::new_for_test();
        info.supp_info = Some(PsSupplementalInfo {
            photo_taken_time: Some(SupplementalInfoDateTime::new_for_test("1674029138")),
            ..PsSupplementalInfo::default()
        });

        // Apple's Keys atom states the zone outright.
        info.track_info = Some(PsTrackInfo {
            creation_time: Some("2023-01-18T08:05:38+00:00".to_string()),
            tags: [(
                "com.apple.quicktime.creationdate".to_string(),
                "2023-01-18T21:05:38+1300".to_string(),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        });
        assert_eq!(
            best_guess_taken_dt(&info).as_deref(),
            Some("2023-01-18T21:05:38+13:00")
        );
        assert_eq!(
            crate::output_path::get_desired_media_path("abc1234", best_guess_taken(&info).as_ref()),
            "2023/01/18/2105-38000",
            "the clip files in the evening it was shot"
        );

        // With no Keys atom, the bare `mvhd` reading is a wall clock and the
        // offset falls out of the difference - exactly as it does for a camera
        // that wrote no `OffsetTime`.
        if let Some(ti) = info.track_info.as_mut() {
            ti.tags.clear();
            ti.creation_time = Some("2023-01-18T21:05:38+00:00".to_string());
        }
        assert_eq!(
            best_guess_taken_dt(&info).as_deref(),
            Some("2023-01-18T21:05:38+13:00")
        );

        // And a clip whose `mvhd` describes a different shutter press entirely
        // is left where Google put it, on the same terms as a re-dated photo.
        if let Some(ti) = info.track_info.as_mut() {
            ti.creation_time = Some("2019-06-02T21:05:38+00:00".to_string());
        }
        assert_eq!(
            best_guess_taken_dt(&info).as_deref(),
            Some("2023-01-18T08:05:38+00:00")
        );
        Ok(())
    }

    #[test]
    fn test_best_guess_taken_dt_video_track() {
        use crate::metadata::track::PsTrackInfo;

        let track = |ct: &str| PsTrackInfo {
            creation_time: Some(ct.to_string()),
            ..Default::default()
        };

        // A video's embedded track creation time is used when present...
        let mut info = MediaFileInfo::new_for_test();
        info.track_info = Some(track("2024-04-18T11:24:26+00:00"));
        assert_eq!(
            best_guess_taken_dt(&info).as_deref(),
            Some("2024-04-18T11:24:26+00:00")
        );

        // ...and is preferred over the file created/modified fallbacks.
        info.created = Some(1_000_000_000_000);
        info.modified = Some(1_000_000_000_000);
        assert_eq!(
            best_guess_taken_dt(&info).as_deref(),
            Some("2024-04-18T11:24:26+00:00")
        );
    }

    /// The second comes from the ranking, the fraction from EXIF. Google's
    /// `photoTakenTime` wins the second and has no fraction to give; the
    /// camera's `SubSecTimeOriginal` fills it in.
    #[test]
    fn test_subsec_refines_a_higher_ranked_whole_second() {
        use crate::metadata::exif::PsExifInfo;
        use crate::metadata::supplemental::{PsSupplementalInfo, SupplementalInfoDateTime};
        use std::collections::HashMap;

        let with_exif = |info: &mut MediaFileInfo, date: &str, subsec: &str| {
            let mut tags = HashMap::new();
            tags.insert(
                nom_exif::ExifTag::DateTimeOriginal.to_string(),
                date.to_string(),
            );
            tags.insert(
                nom_exif::ExifTag::SubSecTimeOriginal.to_string(),
                subsec.to_string(),
            );
            info.exif_info = Some(PsExifInfo {
                tags,
                ..Default::default()
            });
        };
        let taken_at = |ts: &str| PsSupplementalInfo {
            photo_taken_time: Some(SupplementalInfoDateTime::new_for_test(ts)),
            ..PsSupplementalInfo::default()
        };

        // The case the epoch-second rule cannot see. Takeout says
        // 2014-12-25T03:51:26Z; the camera wrote a bare local wall clock 11
        // hours ahead, read as UTC. Different instants as spelled, one shutter
        // press in fact - and the matching second-of-minute is what says so.
        //
        // Takeout still settles which second it was; EXIF supplies the fraction,
        // and - since the winner is an `Instant` with no wall clock of its own -
        // the zone as well. The result is the same moment Google recorded, now
        // spelled in the local time it was taken in.
        let mut info = MediaFileInfo::new_for_test();
        info.supp_info = Some(taken_at("1419479486"));
        with_exif(&mut info, "2014:12:25 14:51:26", "674");
        assert_eq!(
            best_guess_taken_dt(&info).as_deref(),
            Some("2014-12-25T14:51:26.674+11:00"),
            "the winner keeps its second; EXIF supplies the fraction and the zone"
        );

        // The same when the camera did write an offset: nothing is derived, the
        // offset is simply taken.
        with_exif(&mut info, "2014-12-25T14:51:26+11:00", "674");
        assert_eq!(
            best_guess_taken_dt(&info).as_deref(),
            Some("2014-12-25T14:51:26.674+11:00")
        );

        // Someone re-dated the photo in Google Photos: seconds coincide, but a
        // year apart is no timezone. The camera's fraction is not theirs.
        info.supp_info = Some(taken_at("1450990286")); // 2015-12-24T20:51:26Z
        with_exif(&mut info, "2014:12:25 14:51:26", "674");
        assert_eq!(
            best_guess_taken_dt(&info).as_deref(),
            Some("2015-12-24T20:51:26+00:00")
        );

        // Seconds that simply disagree: nothing to graft.
        info.supp_info = Some(taken_at("1419479487")); // ...:27
        assert_eq!(
            best_guess_taken_dt(&info).as_deref(),
            Some("2014-12-25T03:51:27+00:00")
        );

        // An XMP correction is the more deliberate value on precision too, so
        // its own fraction stands and EXIF's is not substituted.
        info.supp_info = Some(taken_at("1419479486"));
        info.xmp_info = Some(crate::metadata::xmp::PsXmpInfo {
            datetime: Some("2014-12-25T03:51:26.417+00:00".to_string()),
            ..Default::default()
        });
        assert_eq!(
            best_guess_taken_dt(&info).as_deref(),
            Some("2014-12-25T03:51:26.417+00:00")
        );

        // ...but an XMP date with no fraction still gets one, on the same terms.
        info.xmp_info = Some(crate::metadata::xmp::PsXmpInfo {
            datetime: Some("2014-12-25T03:51:26+00:00".to_string()),
            ..Default::default()
        });
        assert_eq!(
            best_guess_taken_dt(&info).as_deref(),
            Some("2014-12-25T03:51:26.674+00:00")
        );

        // With EXIF the winner outright, the fraction is already in place and
        // refining is a no-op rather than a second helping.
        info.xmp_info = None;
        info.supp_info = None;
        assert_eq!(
            best_guess_taken_dt(&info).as_deref(),
            Some("2014-12-25T14:51:26.674+00:00")
        );
    }

    /// A video has no fraction anywhere: the QuickTime container stores whole
    /// seconds, and there is no EXIF beside it to borrow from.
    #[test]
    fn test_video_stays_on_the_whole_second() {
        use crate::metadata::track::PsTrackInfo;

        let mut info = MediaFileInfo::new_for_test();
        info.track_info = Some(PsTrackInfo {
            creation_time: Some("2023-01-18T08:05:38+00:00".to_string()),
            ..Default::default()
        });
        assert_eq!(
            best_guess_taken_dt(&info).as_deref(),
            Some("2023-01-18T08:05:38+00:00")
        );
    }

    #[test]
    fn test_best_guess_lat_long_precedence() {
        use crate::metadata::exif::PsExifInfo;
        use crate::metadata::supplemental::{PsSupplementalInfo, SupplementalInfoGeoData};

        let exif = |lat, long| PsExifInfo {
            latitude: lat,
            longitude: long,
            ..Default::default()
        };
        let geo = |lat: f64, long: f64| SupplementalInfoGeoData {
            latitude: Some(lat),
            longitude: Some(long),
        };
        let supp = |geo_data_exif, geo_data| PsSupplementalInfo {
            geo_data,
            geo_data_exif,
            ..PsSupplementalInfo::default()
        };

        let xmp = |lat: f64, long: f64| crate::metadata::xmp::PsXmpInfo {
            latitude: Some(lat),
            longitude: Some(long),
            ..Default::default()
        };

        // Human beats camera: an XMP location outranks everything, including the
        // file's own EXIF fix.
        let mut info = MediaFileInfo::new_for_test();
        info.xmp_info = Some(xmp(9.0, 10.0));
        info.exif_info = Some(exif(Some(1.0), Some(2.0)));
        info.supp_info = Some(supp(Some(geo(3.0, 4.0)), Some(geo(5.0, 6.0))));
        assert_eq!(best_guess_lat_long(&info), Some((9.0, 10.0)));

        // No XMP: Google's `geo_data` - what Photos displays, and editable by the
        // user - is the next human source, and it too beats EXIF.
        info.xmp_info = None;
        assert_eq!(best_guess_lat_long(&info), Some((5.0, 6.0)));

        // With no human source left, the camera's own EXIF fix wins.
        info.supp_info = Some(supp(Some(geo(3.0, 4.0)), None));
        assert_eq!(best_guess_lat_long(&info), Some((1.0, 2.0)));

        // `geo_data_exif` is Google's copy of the EXIF fix, so it ranks last -
        // it only contributes when the file's own EXIF has been stripped.
        info.exif_info = None;
        assert_eq!(best_guess_lat_long(&info), Some((3.0, 4.0)));

        // (0, 0) is treated as absent at every source, human tier included.
        info.xmp_info = Some(xmp(0.0, 0.0));
        info.exif_info = Some(exif(Some(0.0), Some(0.0)));
        info.supp_info = Some(supp(Some(geo(7.0, 8.0)), Some(geo(0.0, 0.0))));
        assert_eq!(best_guess_lat_long(&info), Some((7.0, 8.0)));

        // Nothing usable anywhere.
        info.xmp_info = None;
        info.exif_info = None;
        info.supp_info = Some(supp(Some(geo(0.0, 0.0)), None));
        assert_eq!(best_guess_lat_long(&info), None);
    }

    #[test]
    fn test_best_guess_lat_long_video_track() {
        use crate::metadata::exif::PsExifInfo;
        use crate::metadata::track::PsTrackInfo;
        use std::collections::HashMap;

        let track = |lat: f64, long: f64| PsTrackInfo {
            gps_iso_6709: Some(format!("{lat:+}{long:+}/")),
            latitude: Some(lat),
            longitude: Some(long),
            ..Default::default()
        };

        // A video with only embedded track GPS: coordinates come from ISO 6709.
        let mut info = MediaFileInfo::new_for_test();
        info.exif_info = None;
        info.track_info = Some(track(27.5916, 86.5640));
        assert_eq!(best_guess_lat_long(&info), Some((27.5916, 86.5640)));

        // Embedded EXIF still wins over the track string when both exist.
        info.exif_info = Some(PsExifInfo {
            tags: HashMap::new(),
            gps: None,
            latitude: Some(1.0),
            longitude: Some(2.0),
            ..Default::default()
        });
        assert_eq!(best_guess_lat_long(&info), Some((1.0, 2.0)));
    }

    /// Title and description are pooled across both sidecars, XMP first: a value
    /// set in a photo tool the user drove outranks one from Google Photos, and
    /// either beats nothing at all.
    #[test]
    fn test_best_guess_title_and_description() {
        use crate::metadata::supplemental::PsSupplementalInfo;
        use crate::metadata::xmp::PsXmpInfo;

        let supp = |title: Option<&str>, description: Option<&str>| PsSupplementalInfo {
            title: title.map(str::to_string),
            description: description.map(str::to_string),
            ..PsSupplementalInfo::default()
        };
        let xmp = |title: Option<&str>, description: Option<&str>| PsXmpInfo {
            title: title.map(str::to_string),
            description: description.map(str::to_string),
            ..PsXmpInfo::default()
        };

        // Neither sidecar: nothing to show.
        let mut info = MediaFileInfo::new_for_test();
        assert_eq!(best_guess_title(&info), None);
        assert_eq!(best_guess_description(&info), None);

        // Google's caption alone reaches the note - the common Takeout case,
        // where there is no XMP anywhere in the archive.
        info.supp_info = Some(supp(Some("Beach day"), Some("Low tide.")));
        assert_eq!(best_guess_title(&info), Some("Beach day".to_string()));
        assert_eq!(best_guess_description(&info), Some("Low tide.".to_string()));

        // With both, XMP wins.
        info.xmp_info = Some(xmp(Some("Porthcurno"), Some("Shot on the 40D.")));
        assert_eq!(best_guess_title(&info), Some("Porthcurno".to_string()));
        assert_eq!(
            best_guess_description(&info),
            Some("Shot on the 40D.".to_string())
        );

        // A blank in the higher-ranked source is absent, not an override: the
        // lower one still gets its turn.
        info.xmp_info = Some(xmp(Some("  "), None));
        assert_eq!(best_guess_title(&info), Some("Beach day".to_string()));
        assert_eq!(best_guess_description(&info), Some("Low tide.".to_string()));
    }

    /// Rating, favourite and archived are three separate opinions that happen to
    /// live near each other, and no source of one is allowed to invent another.
    #[test]
    fn test_rating_favorite_and_archived_stay_independent() {
        use crate::metadata::supplemental::PsSupplementalInfo;
        use crate::metadata::xmp::PsXmpInfo;

        // A five-star XMP rating and no Google sidecar: rated, not favourited.
        let mut info = MediaFileInfo::new_for_test();
        info.xmp_info = Some(PsXmpInfo {
            rating: Some(5),
            ..PsXmpInfo::default()
        });
        assert_eq!(best_guess_rating(&info), Some(5));
        assert!(
            !best_guess_favorite(&info),
            "five stars is a rating, not a Google favourite"
        );

        // Rejected. That is a judgement about the photograph and says nothing
        // about which grid it appears in.
        info.xmp_info = Some(PsXmpInfo {
            rating: Some(-1),
            ..PsXmpInfo::default()
        });
        assert_eq!(best_guess_rating(&info), Some(-1));
        assert!(!best_guess_archived(&info), "rejected is not archived");
        assert!(!best_guess_favorite(&info));

        // A Google favourite with no XMP: favourited, and still unrated.
        let mut info = MediaFileInfo::new_for_test();
        info.supp_info = Some(PsSupplementalInfo {
            favorited: true,
            ..PsSupplementalInfo::default()
        });
        assert!(best_guess_favorite(&info));
        assert_eq!(
            best_guess_rating(&info),
            None,
            "a favourite must not be written back as a rating"
        );
        assert!(!best_guess_archived(&info));

        // Archived, and the two flags are independent of each other too.
        info.supp_info = Some(PsSupplementalInfo {
            archived: true,
            ..PsSupplementalInfo::default()
        });
        assert!(best_guess_archived(&info));
        assert!(!best_guess_favorite(&info));

        // No sidecars at all: nothing is asserted about the photo.
        let info = MediaFileInfo::new_for_test();
        assert_eq!(best_guess_rating(&info), None);
        assert!(!best_guess_favorite(&info));
        assert!(!best_guess_archived(&info));
    }
}
