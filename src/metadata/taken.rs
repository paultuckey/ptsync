//! A capture time, and how much its source said about the zone it was read in.
//!
//! The archive's layout is a **wall clock**: `2024/07/15/1430-22417.jpg` means
//! half past two in the afternoon, and there is no way to spell an instant in
//! that name. So every date source has to be reduced to a wall clock before
//! [`crate::output_path`] can file it - and the sources do not all have one to
//! give.
//!
//! Three states, and the archive treats them very differently:
//!
//! - [`TakenAt::WallClock`] - the reading a camera's display showed, with no
//!   offset recorded beside it. We know where to file it and not what instant it
//!   was. Bare EXIF `DateTimeOriginal` is this, and it is the *common* case.
//! - [`TakenAt::Zoned`] - a wall clock and the offset it was read in. Both known.
//!   EXIF with an `OffsetTime` tag, an XMP date someone wrote with a zone, and
//!   Apple's `com.apple.quicktime.creationdate`.
//! - [`TakenAt::Instant`] - an epoch-valued reading with no local clock beside
//!   it. We know what instant it was and not where to file it. Google's
//!   `photoTakenTime` and a file's mtime are this.
//!
//! Before this type there was only an RFC 3339 string, and `+00:00` on the end
//! of one meant either "read in UTC" or "no idea, and these digits are the wall
//! clock" depending on which parser produced it. Those are opposite claims: the
//! first says shift these digits to file them, the second says never touch them.
//! Collapsing them is why a Takeout photo taken at a quarter to three in the
//! afternoon at UTC+11 is filed under `0351`, in the previous day's directory.
//!
//! This type does not fix that - an `Instant` still files under its UTC digits,
//! because with no offset there is nothing better to do. What it fixes is that
//! the three cases are now told apart, so a later pass that *learns* an offset
//! has somewhere to put it: promoting an `Instant` to a `Zoned` is the whole of
//! that repair, and every consumer picks it up for free.

use chrono::{DateTime, FixedOffset, NaiveDateTime, Timelike, Utc};
use std::fmt;

/// A capture time together with what its source knew about the zone. See the
/// module docs for why the three cases cannot be flattened into one string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TakenAt {
    /// A local reading with no offset recorded: the digits are the wall clock,
    /// and the instant they name is unknown.
    WallClock(NaiveDateTime),
    /// A local reading and the offset it was taken in: both known.
    Zoned(DateTime<FixedOffset>),
    /// An instant with no local reading: the moment is known, the wall clock the
    /// photographer saw is not.
    Instant(DateTime<Utc>),
}

impl TakenAt {
    /// The moment this names, on the best terms available.
    ///
    /// A [`TakenAt::WallClock`] has no instant to give, and reading it as UTC is
    /// the only answer that leaves its digits alone. That is an assumption, not
    /// a fact, so this is for comparing two readings of the *same* shutter press
    /// - where the assumption cancels - and not for filing.
    pub(crate) fn instant(&self) -> DateTime<Utc> {
        match self {
            Self::WallClock(dt) => dt.and_utc(),
            Self::Zoned(dt) => dt.to_utc(),
            Self::Instant(dt) => *dt,
        }
    }

    /// Put `millis` onto the reading, keeping its kind and its offset. `None`
    /// when the result would not be a valid time, which callers read as "leave
    /// the whole second alone".
    pub(crate) fn with_millis(&self, millis: u32) -> Option<Self> {
        let nanos = millis.checked_mul(1_000_000)?;
        Some(match self {
            Self::WallClock(dt) => Self::WallClock(dt.with_nanosecond(nanos)?),
            Self::Zoned(dt) => Self::Zoned(dt.with_nanosecond(nanos)?),
            Self::Instant(dt) => Self::Instant(dt.with_nanosecond(nanos)?),
        })
    }

    /// RFC 3339, the one spelling [`crate::output_path::get_desired_media_path`]
    /// can parse and the form that reaches a note's frontmatter and the `db`
    /// index.
    ///
    /// A `WallClock` is written with a `+00:00` it does not really have. That is
    /// a lie the archive has always told, and it is deliberate: the alternative
    /// is a bare local datetime, which `output_path` would file under `undated/`
    /// rather than reading the digits it was handed. Use [`TakenAt::to_string`]
    /// where the distinction matters more than the parse.
    pub(crate) fn to_rfc3339(&self) -> String {
        match self {
            Self::WallClock(dt) => dt.and_utc().to_rfc3339(),
            Self::Zoned(dt) => dt.to_rfc3339(),
            Self::Instant(dt) => dt.to_rfc3339(),
        }
    }

    /// Read back what [`TakenAt::to_string`] wrote, which is how a reading
    /// survives being parked in a `String` field on a parser's struct.
    ///
    /// Only two of the three states round-trip: a string either carries an
    /// offset or it does not, so this yields `Zoned` or `WallClock` and never
    /// `Instant`. That is enough for the one caller that needs it - XMP, whose
    /// dates are always local readings - and epoch-valued sources are wrapped as
    /// `Instant` at the point they are converted, where the knowledge actually
    /// is.
    pub(crate) fn parse(s: &str) -> Option<Self> {
        if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
            return Some(Self::Zoned(dt));
        }
        NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
            .ok()
            .map(Self::WallClock)
    }
}

impl fmt::Display for TakenAt {
    /// The honest spelling: a `WallClock` prints with no offset at all, because
    /// it does not have one. `Zoned` and `Instant` print as RFC 3339.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WallClock(dt) => write!(f, "{}", dt.format("%Y-%m-%dT%H:%M:%S%.f")),
            _ => f.write_str(&self.to_rfc3339()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wall(s: &str) -> anyhow::Result<TakenAt> {
        Ok(TakenAt::WallClock(NaiveDateTime::parse_from_str(
            s,
            "%Y-%m-%dT%H:%M:%S%.f",
        )?))
    }

    fn zoned(s: &str) -> anyhow::Result<TakenAt> {
        Ok(TakenAt::Zoned(DateTime::parse_from_rfc3339(s)?))
    }

    /// The two spellings, and the reason there are two: `to_rfc3339` is what the
    /// archive can parse, `Display` is what the reading actually claims.
    #[test]
    fn test_wall_clock_admits_it_has_no_offset() -> anyhow::Result<()> {
        let w = wall("2014-12-25T14:51:26.674")?;
        assert_eq!(w.to_string(), "2014-12-25T14:51:26.674");
        assert_eq!(w.to_rfc3339(), "2014-12-25T14:51:26.674+00:00");

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
        Ok(())
    }

    /// The pair that used to be indistinguishable. Same digits, same RFC 3339,
    /// opposite claims - and now different values.
    #[test]
    fn test_wall_clock_and_utc_instant_are_not_the_same_reading() -> anyhow::Result<()> {
        let w = wall("2024-05-22T00:17:51")?;
        let i = TakenAt::Instant(w.instant());
        assert_eq!(w.to_rfc3339(), i.to_rfc3339());
        assert_ne!(w, i);
        Ok(())
    }

    /// `Display` and `parse` are inverses for the two states a string can hold.
    #[test]
    fn test_display_round_trips_through_parse() -> anyhow::Result<()> {
        for original in [
            wall("2014-12-25T14:51:26.674")?,
            wall("2014-12-25T14:51:26")?,
            zoned("2023-01-18T21:05:38+13:00")?,
            zoned("2023-01-18T21:05:38.489-05:30")?,
        ] {
            assert_eq!(
                TakenAt::parse(&original.to_string()),
                Some(original.clone()),
                "round-tripping {original}"
            );
        }
        assert_eq!(TakenAt::parse("not a date"), None);
        assert_eq!(TakenAt::parse(""), None);
        Ok(())
    }

    /// The fraction lands on the reading without disturbing the offset, whatever
    /// the kind.
    #[test]
    fn test_with_millis_keeps_the_kind_and_the_offset() -> anyhow::Result<()> {
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

    /// A `WallClock` read as UTC is the assumption `instant` documents; the
    /// point of it is that it cancels when two readings of one shutter press are
    /// compared, which is all it is used for.
    #[test]
    fn test_instant_reads_a_bare_wall_clock_as_utc() -> anyhow::Result<()> {
        assert_eq!(
            wall("2014-12-25T14:51:26")?.instant().to_rfc3339(),
            "2014-12-25T14:51:26+00:00"
        );
        assert_eq!(
            zoned("2014-12-25T14:51:26+11:00")?.instant().to_rfc3339(),
            "2014-12-25T03:51:26+00:00"
        );
        Ok(())
    }
}
