//! A capture time, in the only terms the archive can file: digits on a clock.
//!
//! The layout is a **wall clock**: `2024/07/15/1430-22417.jpg` means half past
//! two in the afternoon, and there is no way to spell an instant in that name.
//! So the only question a date source has to answer is *what digits go in the
//! path* - and the sources differ on whether they have any to give.
//!
//! [`Taken`] is those digits, plus two notes about where they came from:
//!
//! - `offset` - the zone the digits are expressed in, when a source recorded
//!   one. Bare EXIF `DateTimeOriginal` and a video's `mvhd` date record none,
//!   and that is the *common* case. Nothing in the output path reads this; it
//!   exists so the note and the index can say what they actually know.
//! - `certain` - whether the digits are the reading the photographer saw. An
//!   epoch-valued source (Google's `photoTakenTime`, a file's mtime) knows the
//!   instant and not the clock, so its digits are a UTC rendering standing in
//!   for a wall clock nobody recorded.
//!
//! Before this type there was only an RFC 3339 string, and `+00:00` on the end
//! of one meant either "read in UTC" or "no idea, and these digits are the wall
//! clock" depending on which parser produced it. Those are opposite claims: the
//! first says shift these digits to file them, the second says never touch them.
//! Collapsing them is why a Takeout photo taken at a quarter to three in the
//! afternoon at UTC+11 was filed under `0351`, in the previous day's directory.
//!
//! `certain` is what tells the two apart, and it is the whole of the repair:
//! [`crate::metadata::reconcile`] looks for a source that has digits when the
//! winner has none, and swaps them in. A reading that stays uncertain files
//! under its UTC digits, because with nothing to corroborate it there is
//! nothing better to do.

use chrono::{DateTime, FixedOffset, NaiveDateTime, Offset, Timelike, Utc};
use std::fmt;

/// A capture time together with what its source knew about the zone. See the
/// module docs for what each field is for.
///
/// Build one through [`Taken::wall`], [`Taken::zoned`] or [`Taken::instant`],
/// which are the three shapes a source can arrive in and which keep `offset`
/// and `certain` consistent with each other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Taken {
    /// The digits the archive files under. Always present - this is the one
    /// field [`crate::output_path`] reads.
    pub(crate) local: NaiveDateTime,
    /// The zone `local` is expressed in, when a source recorded one. `None` is a
    /// camera that wrote a bare reading: the digits stand, the zone is unknown.
    pub(crate) offset: Option<FixedOffset>,
    /// Whether `local` is the reading the photographer saw, as opposed to UTC
    /// digits standing in for one nobody recorded.
    pub(crate) certain: bool,
}

impl Taken {
    /// A local reading with no offset recorded: the digits are the wall clock,
    /// and the instant they name is unknown. Bare EXIF, and a video's `mvhd`.
    pub(crate) fn wall(local: NaiveDateTime) -> Self {
        Self {
            local,
            offset: None,
            certain: true,
        }
    }

    /// A local reading and the offset it was taken in: both known. EXIF with an
    /// `OffsetTime` tag, an XMP date written with a zone, and Apple's
    /// `com.apple.quicktime.creationdate`.
    pub(crate) fn zoned(dt: DateTime<FixedOffset>) -> Self {
        Self {
            local: dt.naive_local(),
            offset: Some(*dt.offset()),
            certain: true,
        }
    }

    /// An instant with no local reading beside it. The UTC digits are kept
    /// because something has to go in the path, but `certain` is false to say
    /// they are a stand-in. Google's `photoTakenTime`, and a file's mtime.
    pub(crate) fn instant(dt: DateTime<Utc>) -> Self {
        Self {
            local: dt.naive_utc(),
            offset: Some(Utc.fix()),
            certain: false,
        }
    }

    /// The same digits, reread as UTC rather than as somebody's wall clock.
    ///
    /// For `GPSDateStamp`, which spells itself like any other bare EXIF date but
    /// comes off the GPS receiver, where time is UTC by definition.
    pub(crate) fn into_instant(self) -> Self {
        Self {
            offset: Some(Utc.fix()),
            certain: false,
            ..self
        }
    }

    /// The same reading with `offset` filled in, if it did not have one. A
    /// stated offset always beats a derived one, so this never overwrites.
    pub(crate) fn or_offset(self, offset: FixedOffset) -> Self {
        Self {
            offset: self.offset.or(Some(offset)),
            ..self
        }
    }

    /// Put `millis` onto the reading. `None` when the result would not be a
    /// valid time, which callers read as "leave the whole second alone".
    pub(crate) fn with_millis(&self, millis: u32) -> Option<Self> {
        let nanos = millis.checked_mul(1_000_000)?;
        Some(Self {
            local: self.local.with_nanosecond(nanos)?,
            ..self.clone()
        })
    }

    /// Take `other`'s fraction of a second when this reading has none of its own.
    ///
    /// Ranking the sources and choosing the precision are separate questions,
    /// and collapsing them gets one of the two wrong. `photoTakenTime` outranks
    /// EXIF - it is editable in the Google Photos UI, so it can be a human
    /// correction - but Takeout stores it as integer *seconds*. Let it win
    /// outright and every photo in a Takeout lands on `...000`, including the
    /// bursts, where the frames differ only in the fraction.
    ///
    /// A reading that already carries a fraction is left alone: whichever source
    /// won the ranking outranks the other on precision for the same reason it
    /// outranked it on the date.
    pub(crate) fn with_fraction_from(self, other: &Self) -> Self {
        let millis = other.local.and_utc().timestamp_subsec_millis();
        if millis == 0 || self.local.nanosecond() != 0 {
            return self;
        }
        self.with_millis(millis).unwrap_or(self)
    }

    /// RFC 3339: the form that reaches a note's frontmatter and the `db` index.
    ///
    /// A reading with no recorded offset is written with a `+00:00` it does not
    /// really have, which is a lie the archive has always told. The `offset`
    /// field beside it is what makes the lie detectable: `None` there means the
    /// zone on the end of this string is a placeholder. Use the [`Display`]
    /// impl where the distinction matters more than the parse.
    ///
    /// [`Display`]: std::fmt::Display
    pub(crate) fn to_rfc3339(&self) -> String {
        let offset = self.offset.unwrap_or_else(|| Utc.fix());
        DateTime::<FixedOffset>::from_naive_utc_and_offset(self.local - offset, offset).to_rfc3339()
    }

    /// Read back what the [`Display`](std::fmt::Display) impl wrote, which is how a reading
    /// survives being parked in a `String` field on a parser's struct.
    ///
    /// Only two of the three shapes round-trip: a string either carries an
    /// offset or it does not, so this yields `zoned` or `wall` and never
    /// `instant`. That is enough for the one caller that needs it - XMP, whose
    /// dates are always local readings - and epoch-valued sources are wrapped at
    /// the point they are converted, where the knowledge actually is.
    pub(crate) fn parse(s: &str) -> Option<Self> {
        if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
            return Some(Self::zoned(dt));
        }
        NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
            .ok()
            .map(Self::wall)
    }
}

impl fmt::Display for Taken {
    /// The honest spelling: the digits, and an offset only when one is known.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.local.format("%Y-%m-%dT%H:%M:%S%.f"))?;
        match self.offset {
            Some(offset) => write!(f, "{offset}"),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wall(s: &str) -> anyhow::Result<Taken> {
        Ok(Taken::wall(NaiveDateTime::parse_from_str(
            s,
            "%Y-%m-%dT%H:%M:%S%.f",
        )?))
    }

    fn zoned(s: &str) -> anyhow::Result<Taken> {
        Ok(Taken::zoned(DateTime::parse_from_rfc3339(s)?))
    }

    /// The two spellings, and the reason there are two: `to_rfc3339` is what the
    /// note and the index carry, `Display` is what the reading actually claims.
    #[test]
    fn test_wall_clock_admits_it_has_no_offset() -> anyhow::Result<()> {
        let w = wall("2014-12-25T14:51:26.674")?;
        assert_eq!(w.to_string(), "2014-12-25T14:51:26.674");
        assert_eq!(w.to_rfc3339(), "2014-12-25T14:51:26.674+00:00");
        assert_eq!(w.offset, None, "the placeholder zone must not be recorded");

        // A zero fraction is printed by neither, so the common case stays short.
        let w = wall("2014-12-25T14:51:26")?;
        assert_eq!(w.to_string(), "2014-12-25T14:51:26");
        assert_eq!(w.to_rfc3339(), "2014-12-25T14:51:26+00:00");
        Ok(())
    }

    /// A real offset is kept in both spellings - it is the thing worth keeping.
    #[test]
    fn test_zoned_keeps_its_offset() -> anyhow::Result<()> {
        let z = zoned("2023-01-18T21:05:38+13:00")?;
        assert_eq!(z.to_string(), "2023-01-18T21:05:38+13:00");
        assert_eq!(z.to_rfc3339(), "2023-01-18T21:05:38+13:00");
        // The digits are the local reading, not the UTC one: that is what the
        // path is built from.
        assert_eq!(z.local.format("%H%M").to_string(), "2105");
        Ok(())
    }

    /// The pair that used to be indistinguishable. Same digits, same RFC 3339,
    /// opposite claims - and now different values.
    #[test]
    fn test_wall_clock_and_utc_instant_are_not_the_same_reading() -> anyhow::Result<()> {
        let w = wall("2024-05-22T00:17:51")?;
        let i = Taken::instant(w.local.and_utc());
        assert_eq!(w.to_rfc3339(), i.to_rfc3339());
        assert_ne!(w, i);
        assert!(w.certain && !i.certain);
        Ok(())
    }

    /// `Display` and `parse` are inverses for the two shapes a string can hold.
    #[test]
    fn test_display_round_trips_through_parse() -> anyhow::Result<()> {
        for original in [
            wall("2014-12-25T14:51:26.674")?,
            wall("2014-12-25T14:51:26")?,
            zoned("2023-01-18T21:05:38+13:00")?,
            zoned("2023-01-18T21:05:38.489-05:30")?,
        ] {
            assert_eq!(
                Taken::parse(&original.to_string()),
                Some(original.clone()),
                "round-tripping {original}"
            );
        }
        assert_eq!(Taken::parse("not a date"), None);
        assert_eq!(Taken::parse(""), None);
        Ok(())
    }

    /// The fraction lands on the reading without disturbing the offset, whatever
    /// the shape.
    #[test]
    fn test_with_millis_keeps_the_offset() -> anyhow::Result<()> {
        assert_eq!(
            wall("2014-12-25T14:51:26")?.with_millis(674),
            Some(wall("2014-12-25T14:51:26.674")?)
        );
        assert_eq!(
            zoned("2023-01-18T21:05:38+13:00")?.with_millis(489),
            Some(zoned("2023-01-18T21:05:38.489+13:00")?)
        );
        // Replaces rather than accumulates, so refining twice is idempotent.
        assert_eq!(
            wall("2014-12-25T14:51:26.111")?.with_millis(674),
            Some(wall("2014-12-25T14:51:26.674")?)
        );
        Ok(())
    }

    /// A reading with no fraction takes one; a reading that has its own keeps it.
    #[test]
    fn test_with_fraction_from() -> anyhow::Result<()> {
        let fine = wall("2014-12-25T14:51:26.674")?;
        assert_eq!(
            wall("2014-12-25T14:51:26")?.with_fraction_from(&fine),
            wall("2014-12-25T14:51:26.674")?
        );
        assert_eq!(
            wall("2014-12-25T14:51:26.417")?.with_fraction_from(&fine),
            wall("2014-12-25T14:51:26.417")?
        );
        // Nothing to give: a whole-second donor is a no-op, not a truncation.
        assert_eq!(
            fine.clone()
                .with_fraction_from(&wall("2014-12-25T14:51:26")?),
            fine
        );
        Ok(())
    }

    /// An offset is filled in only where one is missing - a stated zone always
    /// beats a derived one.
    #[test]
    fn test_or_offset_never_overwrites() -> anyhow::Result<()> {
        let plus11 = FixedOffset::east_opt(11 * 3600).ok_or_else(|| anyhow::anyhow!("offset"))?;
        assert_eq!(
            wall("2014-12-25T14:51:26")?.or_offset(plus11),
            zoned("2014-12-25T14:51:26+11:00")?
        );
        assert_eq!(
            zoned("2014-12-25T14:51:26+13:00")?.or_offset(plus11),
            zoned("2014-12-25T14:51:26+13:00")?
        );
        Ok(())
    }

    /// An instant's digits really are UTC, so it says so - unlike a bare wall
    /// clock, whose `+00:00` is only a placeholder.
    #[test]
    fn test_instant_states_the_zone_its_digits_are_in() -> anyhow::Result<()> {
        let i = Taken::instant(DateTime::parse_from_rfc3339("2014-12-25T03:51:26Z")?.to_utc());
        assert_eq!(i.to_string(), "2014-12-25T03:51:26+00:00");
        assert!(!i.certain, "the photographer's clock is still unknown");

        // `into_instant` reinterprets digits already in hand, and makes the same
        // claim about them.
        let gps = wall("2015-04-17T00:00:00")?.into_instant();
        assert_eq!(gps.to_string(), "2015-04-17T00:00:00+00:00");
        assert!(!gps.certain);
        Ok(())
    }
}
