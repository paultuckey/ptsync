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
use super::taken::TakenAt;
use chrono::{FixedOffset, Timelike};

/// Best guess at the date the photo was taken from messy optional data.
///
/// *Human tier* — someone chose this value:
/// 1. XMP sidecar `photoshop:DateCreated` - a correction made in Lightroom,
///    darktable or digiKam
/// 2. SupplementalInfo photo_taken_time - the date Google Photos displays, which
///    the user can edit in its UI
///
/// *Camera tier* — the device's own reading:
/// 3. EXIF DateTimeOriginal
/// 4. EXIF DateTime
/// 5. EXIF GPSDateStamp - only accurate up to minute
/// 6. Track creation_time - the embedded capture time for videos (already
///    rfc3339); videos have no EXIF, so this is their equivalent of 3
///
/// *Fallback tier* — not a capture time at all, only better than nothing:
/// 7. SupplementalInfo creation_time - when the file was *uploaded* to Google,
///    which is why it ranks below the camera despite sharing a file with 2
/// 8. File modified time
///   - no timezone info, unreliable in zips, somewhat unreliable in directories due to file
///     copying / syncing not preserving, only use as second to last resort
/// 9. File creation time
///   - no timezone info, unavailable in zips, somewhat unreliable in directories due to file
///     copying / syncing not preserving, only use as a last resort
///
/// Result returned as an RFC 3339 string. Every source above normalizes to that
/// one form, because [`crate::output_path::get_desired_media_path`] parses it
/// back and files anything it cannot read under `undated/` — a source that
/// returns its own native spelling silently loses the date it just found.
///
/// The ranking settles *which reading to believe*, and only that. Two things the
/// winner may be missing are then taken from the camera's own reading by
/// [`refine_with_exif`]: the zone it was read in, and the fraction of a second.
/// Both are EXIF's alone to give, and neither is a question about whose date is
/// right - which is why they are resolved apart from the ranking and from each
/// other.
pub(crate) fn best_guess_taken_dt(info: &MediaFileInfo) -> Option<String> {
    Some(best_guess_taken(info)?.to_rfc3339())
}

/// The same answer as [`best_guess_taken_dt`], before it is flattened into a
/// string that cannot say whether the offset on the end is real.
///
/// The ranking above settles *which reading* to believe. This keeps what the
/// winning source knew about its zone, which is a separate question and the one
/// that decides whether the reading can be filed as it stands: a
/// [`TakenAt::WallClock`] or [`TakenAt::Zoned`] is already the local time the
/// photographer saw, and a [`TakenAt::Instant`] is not and has nothing to say
/// what it was.
///
/// An `Instant` that reaches a consumer still files under its UTC digits, which
/// is wrong by one offset and the best that can be done with no offset to hand.
/// [`refine_with_exif`] is what stops most of them getting that far.
pub(crate) fn best_guess_taken(info: &MediaFileInfo) -> Option<TakenAt> {
    let whole_second = best_guess_taken_to_the_second(info)?;
    Some(refine_with_exif(whole_second, info))
}

/// Take from the camera's own reading the two things the winner may be missing:
/// the zone it was read in, and the fraction of a second.
///
/// Both are answers EXIF alone holds, both are wanted only when the two readings
/// describe the same shutter press, and that shared question is
/// [`same_shutter_press`]. They are otherwise independent — a photo can want one,
/// the other, both or neither — so they are applied in turn rather than as one
/// decision.
///
/// The zone goes on first. Grafting the fraction afterwards keeps whatever
/// offset that established, because [`TakenAt::with_millis`] preserves the kind
/// of reading it is handed.
fn refine_with_exif(whole_second: TakenAt, info: &MediaFileInfo) -> TakenAt {
    let Some(exif) = best_guess_taken_exif(&info.exif_info) else {
        return whole_second;
    };
    if !same_shutter_press(&whole_second, &exif) {
        return whole_second;
    }
    with_exif_subsec(with_exif_zone(whole_second, &exif), &exif)
}

/// Whether two readings are one shutter press spelled two ways.
///
/// They are matched on **second-of-minute**, not on the instant. Requiring the
/// same epoch second is the obvious rule and it fails on the common case: a
/// camera that writes no `OffsetTime` gives a bare wall clock, which has no
/// instant to compare and which [`TakenAt::instant`] can only read as UTC, so a
/// photo taken at UTC+13 reads 13 hours off the `photoTakenTime` for the same
/// shutter press. Every real UTC offset is a whole number of minutes, which
/// leaves the second-of-minute untouched by the discrepancy — it is the one
/// field of the two readings that can be compared without knowing the zone.
///
/// That match is loose enough to hit by chance once in sixty, so it is bounded
/// by 26 hours, the spread from UTC-12 to UTC+14. Beyond that the two are not
/// one instant spelled two ways but two different instants — someone re-dated
/// the photo in Google Photos — and nothing the camera recorded belongs to the
/// second they chose.
fn same_shutter_press(a: &TakenAt, b: &TakenAt) -> bool {
    const MAX_UTC_OFFSET_SPREAD_S: i64 = 26 * 60 * 60;

    // Second-of-minute is the same in either reading of a clock, offsets being
    // whole minutes, so it is read off the instant regardless of what kind of
    // reading each side turned out to be.
    let (a, b) = (a.instant(), b.instant());
    a.second() == b.second() && (a - b).num_seconds().abs() <= MAX_UTC_OFFSET_SPREAD_S
}

/// Give an [`TakenAt::Instant`] the wall clock EXIF kept for it.
///
/// Takeout exports `photoTakenTime` as a bare epoch and no offset, so a Google
/// photo arrives knowing when it was taken and not what the clock said - and
/// the archive files by what the clock said. The camera's own reading is the
/// missing half, and it is usually still in the file: Takeout strips plenty but
/// rarely the EXIF date.
///
/// Two ways to get there, and only the second is arithmetic:
///
/// - EXIF carried an `OffsetTime` tag, so the offset is *stated*. Nothing is
///   derived and nothing can go wrong beyond the two readings not belonging
///   together, which [`same_shutter_press`] has already ruled out.
/// - EXIF is a bare wall clock, so the offset is the difference between the two.
///   That difference is exact rather than approximate: `photoTakenTime` is not
///   an independent clock but Google's own reading of this EXIF value in a zone
///   it inferred, so when the two describe one shutter press the gap between
///   them is precisely that zone. Hence [`whole_minute_offset`] rejects rather
///   than rounds - a ragged difference is not a timezone measured sloppily, it
///   is evidence that these are two different instants after all.
///
/// Only an `Instant` is touched. A `WallClock` or `Zoned` winner is already the
/// local reading the archive wants, and a wall clock with no offset files
/// correctly whether or not anyone ever works out which zone it was.
fn with_exif_zone(winner: TakenAt, exif: &TakenAt) -> TakenAt {
    let TakenAt::Instant(instant) = &winner else {
        return winner;
    };
    let (wall, offset) = match exif {
        TakenAt::Zoned(dt) => (dt.naive_local(), *dt.offset()),
        TakenAt::WallClock(wall) => {
            // Whole seconds on both sides. EXIF's fraction is one Takeout
            // truncated away when it stored the timestamp, and keeping it here
            // would make an exact offset look like a ragged one.
            let Some(offset) =
                whole_minute_offset(wall.and_utc().timestamp() - instant.timestamp())
            else {
                return winner;
            };
            (*wall, offset)
        }
        // Two instants say nothing about a wall clock between them.
        TakenAt::Instant(_) => return winner,
    };
    wall.and_local_timezone(offset)
        .single()
        .map_or(winner, TakenAt::Zoned)
}

/// A difference between two readings, as a UTC offset — or `None` when it is not
/// one.
///
/// Nothing is rounded to get here. Every real UTC offset is a whole number of
/// minutes inside `-12:00 ..= +14:00`, and the difference this is handed is
/// exact when the two readings are the same shutter press, so anything failing
/// either test is evidence about the *readings* rather than a zone in need of
/// tidying up. Snapping it to the nearest quarter hour would manufacture an
/// offset out of the one signal that says there isn't one.
fn whole_minute_offset(seconds: i64) -> Option<FixedOffset> {
    if seconds % 60 != 0 || !(-12 * 3600..=14 * 3600).contains(&seconds) {
        return None;
    }
    FixedOffset::east_opt(i32::try_from(seconds).ok()?)
}

/// Fill in the sub-second part of a date whose whole second is already settled.
///
/// Ranking the sources and choosing the precision are separate questions, and
/// collapsing them gets one of the two wrong. `photoTakenTime` outranks EXIF —
/// it is editable in the Google Photos UI, so it can be a human correction — but
/// Takeout stores it as integer *seconds*. Let it win outright and every photo
/// in a Takeout lands on `...000`, including the bursts, where the frames differ
/// only in the fraction. So the higher-ranked source keeps the second it chose
/// and EXIF is asked only for the digits it alone holds.
///
/// A winner that already carries a fraction (an XMP `photoshop:DateCreated`) is
/// left alone: it outranks EXIF on precision for the same reason it outranks it
/// on the date. When EXIF is itself the winner this is a no-op — the fraction is
/// already there, and grafting it back on changes nothing.
///
/// Whether the two readings belong together is [`same_shutter_press`]'s
/// question, already settled by the time this is called.
fn with_exif_subsec(whole_second: TakenAt, exif: &TakenAt) -> TakenAt {
    // The fraction is the same in either reading of a clock, offsets being whole
    // minutes, so it is read off the instant regardless of what kind of reading
    // each side turned out to be.
    let millis = exif.instant().timestamp_subsec_millis();
    if millis == 0 || whole_second.instant().nanosecond() != 0 {
        return whole_second;
    }
    whole_second.with_millis(millis).unwrap_or(whole_second)
}

/// The source ranking itself — see [`best_guess_taken`], which adds the fraction
/// to what this returns.
///
/// Each source is wrapped in the [`TakenAt`] case that says what it actually
/// knew. That is not a property of the *date* but of where it came from, which
/// is why it is decided here beside the ranking rather than inferred later from
/// a string: an epoch-valued source can only ever yield an `Instant`, and no
/// amount of looking at `2024-05-22T00:17:51+00:00` afterwards will reveal that
/// the `+00:00` was Google's and not a photographer's.
fn best_guess_taken_to_the_second(info: &MediaFileInfo) -> Option<TakenAt> {
    // -- human tier --
    if let Some(dt) = info
        .xmp_info
        .as_ref()
        .and_then(|x| x.datetime.as_deref())
        .and_then(TakenAt::parse)
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
        return Some(TakenAt::Instant(dt));
    }

    // -- camera tier --
    let time_taken_from_exif = best_guess_taken_exif(&info.exif_info);
    if let Some(dt) = time_taken_from_exif {
        return Some(dt);
    }
    // Videos have no EXIF; their capture time lives in the track metadata, which
    // is the embedded-metadata equivalent of EXIF DateTimeOriginal for images.
    if let Some(dt) = info.track_info.as_ref().and_then(|ti| ti.taken_at()) {
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
        return Some(TakenAt::Instant(dt));
    }
    // A filesystem timestamp is an epoch count like Google's, and just as silent
    // about the clock on the wall.
    if let Some(dt) = info.modified.and_then(crate::util::timestamp_to_utc) {
        return Some(TakenAt::Instant(dt));
    }
    if let Some(dt) = info.created.and_then(crate::util::timestamp_to_utc) {
        return Some(TakenAt::Instant(dt));
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

    /// The kinds, not the digits. Every source lands in the [`TakenAt`] case
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

        let kind_of = |info: &MediaFileInfo| -> anyhow::Result<String> {
            let taken = best_guess_taken(info).ok_or_else(|| anyhow!("no date guessed"))?;
            Ok(match taken {
                TakenAt::WallClock(_) => "wall".to_string(),
                TakenAt::Zoned(_) => "zoned".to_string(),
                TakenAt::Instant(_) => "instant".to_string(),
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
            crate::output_path::get_desired_media_path("abc1234", &best_guess_taken_dt(&info)),
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
                crate::output_path::get_desired_media_path("abc1234", &best_guess_taken_dt(&info)),
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
        // inventing one.
        assert_eq!(
            filed_from("2014:12:25 14:51:56").as_deref(),
            Some("2014-12-25T03:51:26+00:00"),
            "a difference of whole minutes is required"
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
