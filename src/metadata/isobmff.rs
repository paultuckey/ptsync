//! The parts of a QuickTime/ISO-BMFF `moov` atom that `nom_exif` discards.
//!
//! `nom_exif` reads `moov/meta` in full and then keeps six well-known keys and
//! drops the rest, so `com.apple.quicktime.content.identifier` - the uuid Apple
//! stamps on *both* halves of a live photo - never reaches a caller. It parses
//! `tkhd` too, but leaves the display matrix commented out, so a portrait clip
//! reports the 1920x1440 it is stored as with no hint that it plays rotated.
//!
//! Both are recovered here by walking the boxes directly. This is a reader, not
//! a parser: it answers those two questions and ignores everything else in the
//! file.

use std::io::{Read, Seek, SeekFrom};
use tracing::debug;

/// A `moov` larger than this is taken as malformed rather than read into
/// memory. Metadata atoms run to tens of kilobytes; the cap is only here so a
/// corrupt length field can't ask for an arbitrary allocation.
const MAX_MOOV_BYTES: u64 = 64 * 1024 * 1024;

/// What [`parse_mov_extras`] recovered from a track's `moov`.
#[derive(Debug, Default, Clone)]
pub(crate) struct MovExtras {
    /// Every `moov/meta` key/value pair, under the full key name Apple writes
    /// (`com.apple.quicktime.content.identifier`, ...). Sorted, so the `info`
    /// report renders in a stable order.
    pub(crate) keys: std::collections::BTreeMap<String, String>,
    /// The video track's display transform as `(mirrored, rotate)`, in the same
    /// shape [`crate::metadata::exif::exif_display_transform`] returns for images: a
    /// horizontal flip applied *before* a clockwise rotation. `None` when there
    /// is no video track, or its matrix isn't one of the eight rigid forms.
    pub(crate) display_transform: Option<(bool, i32)>,
}

/// Read the `moov` atom and pull out its metadata keys and display matrix.
///
/// Returns `None` for anything that isn't ISO-BMFF (Matroska reaches the track
/// parser too) or whose `moov` is unreadable - a file this fails on still has
/// everything `nom_exif` found, so there is nothing here worth failing a scan
/// over.
pub(crate) fn parse_mov_extras<R: Read + Seek>(reader: &mut R) -> Option<MovExtras> {
    let moov = match read_moov(reader) {
        Ok(Some(moov)) => moov,
        Ok(None) => return None,
        Err(e) => {
            debug!("Could not read moov atom: {e}");
            return None;
        }
    };
    Some(MovExtras {
        keys: meta_keys(&moov),
        display_transform: video_display_transform(&moov),
    })
}

/// Seek over the top-level boxes and return the body of `moov`.
///
/// Apple writes `mdat` first and `moov` last, so this walks by seeking rather
/// than reading: the clip's several megabytes of frames are stepped over, not
/// pulled into memory.
fn read_moov<R: Read + Seek>(reader: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    let end = reader.seek(SeekFrom::End(0))?;
    let mut pos = 0u64;
    while pos.saturating_add(8) <= end {
        reader.seek(SeekFrom::Start(pos))?;
        let mut header = [0u8; 8];
        reader.read_exact(&mut header)?;
        let mut size = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as u64;
        let box_type = [header[4], header[5], header[6], header[7]];
        let mut header_len = 8u64;
        if size == 1 {
            // 64-bit `largesize` follows the header for boxes over 4 GiB.
            let mut ext = [0u8; 8];
            reader.read_exact(&mut ext)?;
            size = u64::from_be_bytes(ext);
            header_len = 16;
        } else if size == 0 {
            // "To the end of the file" - only legal for the last box.
            size = end - pos;
        }
        if size < header_len {
            return Ok(None); // malformed length; nothing further is trustworthy
        }
        let body_len = size - header_len;
        if &box_type == b"moov" {
            if body_len > MAX_MOOV_BYTES {
                debug!("moov claims {body_len} bytes, refusing to read");
                return Ok(None);
            }
            let mut body = vec![0u8; body_len as usize];
            reader.read_exact(&mut body)?;
            return Ok(Some(body));
        }
        pos = pos.saturating_add(size);
    }
    Ok(None)
}

/// The child boxes of a container body, as `(type, body)`.
///
/// The type is kept as raw bytes because it is not always text: `ilst` numbers
/// its children with a big-endian index in the same four bytes, and Android
/// writes `©xyz`.
struct Boxes<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for Boxes<'a> {
    type Item = ([u8; 4], &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        let head = self.data.get(self.pos..self.pos.checked_add(8)?)?;
        let declared = u32::from_be_bytes(head[0..4].try_into().ok()?) as usize;
        let box_type: [u8; 4] = head[4..8].try_into().ok()?;
        let (size, header_len) = match declared {
            1 => {
                let ext = self.data.get(self.pos + 8..self.pos + 16)?;
                (u64::from_be_bytes(ext.try_into().ok()?) as usize, 16)
            }
            0 => (self.data.len().checked_sub(self.pos)?, 8),
            n => (n, 8),
        };
        if size < header_len {
            return None;
        }
        let body = self
            .data
            .get(self.pos + header_len..self.pos.checked_add(size)?)?;
        self.pos += size;
        Some((box_type, body))
    }
}

impl<'a> Boxes<'a> {
    fn new(data: &'a [u8]) -> Self {
        Boxes { data, pos: 0 }
    }
}

/// The body of the first child of `data` with type `box_type`.
fn child<'a>(data: &'a [u8], box_type: &[u8; 4]) -> Option<&'a [u8]> {
    Boxes::new(data)
        .find(|(t, _)| t == box_type)
        .map(|(_, b)| b)
}

/// Every `moov/meta` key/value pair, resolved through the `keys` index.
fn meta_keys(moov: &[u8]) -> std::collections::BTreeMap<String, String> {
    let Some(meta) = child(moov, b"meta").and_then(meta_children) else {
        return Default::default();
    };
    let Some(ilst) = child(meta, b"ilst") else {
        return Default::default();
    };
    let names = child(meta, b"keys").map(parse_keys).unwrap_or_default();
    parse_ilst(ilst, &names)
}

/// `meta` is a container in QuickTime and a full box - four leading bytes of
/// version and flags - in ISO-BMFF, and both spellings turn up in files that
/// call themselves `.mov`. Try each and keep whichever actually holds the
/// metadata boxes.
fn meta_children(meta: &[u8]) -> Option<&[u8]> {
    [0usize, 4].into_iter().find_map(|offset| {
        let body = meta.get(offset..)?;
        (child(body, b"keys").is_some() || child(body, b"ilst").is_some()).then_some(body)
    })
}

/// The `keys` box: a full box holding a count and then length-prefixed key
/// names, whose 1-based position is what `ilst` refers to them by.
fn parse_keys(keys: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let Some(count) = keys.get(4..8) else {
        return names;
    };
    let count = u32::from_be_bytes(count.try_into().unwrap_or_default()) as usize;
    let mut pos = 8;
    for _ in 0..count {
        let Some(head) = keys.get(pos..pos + 8) else {
            break;
        };
        // The size covers the 8 bytes of size and namespace as well as the name.
        let size = u32::from_be_bytes(head[0..4].try_into().unwrap_or_default()) as usize;
        let Some(name) = size
            .checked_add(pos)
            .filter(|end| size >= 8 && *end <= keys.len())
            .and_then(|end| keys.get(pos + 8..end))
        else {
            break;
        };
        names.push(String::from_utf8_lossy(name).into_owned());
        pos += size;
    }
    names
}

/// The `ilst` box: one child per value, its box "type" being the 1-based index
/// of the key it belongs to, wrapping a `data` box holding the value itself.
fn parse_ilst(ilst: &[u8], names: &[String]) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for (index, body) in Boxes::new(ilst) {
        let index = u32::from_be_bytes(index) as usize;
        let Some(name) = index.checked_sub(1).and_then(|i| names.get(i)) else {
            continue;
        };
        if let Some(value) = child(body, b"data").and_then(decode_data) {
            out.insert(name.clone(), value);
        }
    }
    out
}

/// A `data` box: a type indicator, a locale, then the payload. Rendered to a
/// string because that is what both consumers - the note and the `info` report -
/// want; a type this doesn't know is skipped rather than guessed at.
fn decode_data(data: &[u8]) -> Option<String> {
    let indicator = u32::from_be_bytes(data.get(0..4)?.try_into().ok()?) & 0x00ff_ffff;
    let payload = data.get(8..)?;
    let value = match indicator {
        // UTF-8 text.
        1 => String::from_utf8_lossy(payload)
            .trim_end_matches('\0')
            .to_string(),
        // Big-endian signed and unsigned integers, 1 to 8 bytes wide.
        21 => be_int(payload)?.to_string(),
        22 => be_uint(payload)?.to_string(),
        23 => f32::from_be_bytes(payload.try_into().ok()?).to_string(),
        24 => f64::from_be_bytes(payload.try_into().ok()?).to_string(),
        _ => return None,
    };
    (!value.is_empty()).then_some(value)
}

fn be_uint(bytes: &[u8]) -> Option<u64> {
    (1..=8)
        .contains(&bytes.len())
        .then(|| bytes.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b)))
}

fn be_int(bytes: &[u8]) -> Option<i64> {
    let unsigned = be_uint(bytes)?;
    let bits = bytes.len() * 8;
    // Sign-extend from the value's own width.
    Some(if bits < 64 && unsigned & (1 << (bits - 1)) != 0 {
        (unsigned as i64) - (1i64 << bits)
    } else {
        unsigned as i64
    })
}

/// The display transform of the first track that has one, which is the video
/// track: sound and metadata tracks carry a zeroed width and height.
fn video_display_transform(moov: &[u8]) -> Option<(bool, i32)> {
    for (box_type, trak) in Boxes::new(moov) {
        if &box_type != b"trak" {
            continue;
        }
        let Some(tkhd) = child(trak, b"tkhd") else {
            continue;
        };
        // A full box, and its version decides the width of the three times and
        // the duration that precede the fixed-size tail we want.
        let matrix_at = match tkhd.first() {
            Some(0) => 40,
            Some(1) => 52,
            _ => continue,
        };
        let Some(dimensions) = tkhd.get(matrix_at + 36..matrix_at + 44) else {
            continue;
        };
        let has_picture = dimensions.iter().any(|b| *b != 0);
        let Some(matrix) = tkhd.get(matrix_at..matrix_at + 36) else {
            continue;
        };
        if has_picture {
            return matrix_transform(matrix);
        }
    }
    None
}

/// Decompose a `tkhd` display matrix into `(mirrored, rotate)`.
///
/// The matrix is nine fixed-point values mapping source to screen, of which
/// only the four that can rotate or flip matter. Every camera writes one of the
/// eight rigid transforms, so they are matched exactly and anything else -
/// a scale, a shear, a translation-only crop - returns `None` rather than an
/// approximation. Mirroring is applied before the rotation, matching
/// [`crate::metadata::exif::exif_display_transform`].
fn matrix_transform(matrix: &[u8]) -> Option<(bool, i32)> {
    // The first six values are 16.16 fixed point, so 1.0 is 65536.
    let unit = |i: usize| match i32::from_be_bytes(matrix.get(i * 4..i * 4 + 4)?.try_into().ok()?) {
        65536 => Some(1i32),
        -65536 => Some(-1),
        0 => Some(0),
        _ => None,
    };
    let (a, b, c, d) = (unit(0)?, unit(1)?, unit(3)?, unit(4)?);
    Some(match (a, b, c, d) {
        (1, 0, 0, 1) => (false, 0),
        (0, 1, -1, 0) => (false, 90),
        (-1, 0, 0, -1) => (false, 180),
        (0, -1, 1, 0) => (false, -90),
        (-1, 0, 0, 1) => (true, 0),
        (0, -1, -1, 0) => (true, 90),
        (1, 0, 0, -1) => (true, 180),
        (0, 1, 1, 0) => (true, -90),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A box: four bytes of big-endian size, four of type, then the body.
    fn bx(box_type: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(box_type);
        out.extend_from_slice(body);
        out
    }

    /// A 16.16 fixed-point matrix value.
    fn fixed(v: i32) -> [u8; 4] {
        (v * 65536).to_be_bytes()
    }

    fn matrix(a: i32, b: i32, c: i32, d: i32) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(&fixed(a));
        m.extend_from_slice(&fixed(b));
        m.extend_from_slice(&[0; 4]); // u
        m.extend_from_slice(&fixed(c));
        m.extend_from_slice(&fixed(d));
        m.extend_from_slice(&[0; 4]); // v
        m.extend_from_slice(&[0; 12]); // x, y, w
        m
    }

    /// A version-0 `tkhd`: version and flags, then the times, track id,
    /// reserved and duration that put the matrix at byte 40.
    fn tkhd(a: i32, b: i32, c: i32, d: i32, width: u32, height: u32) -> Vec<u8> {
        let mut body = vec![0u8; 40];
        body.extend_from_slice(&matrix(a, b, c, d));
        body.extend_from_slice(&(width << 16).to_be_bytes());
        body.extend_from_slice(&(height << 16).to_be_bytes());
        bx(b"tkhd", &body)
    }

    fn keys_box(names: &[&str]) -> Vec<u8> {
        let mut body = vec![0u8; 4]; // version + flags
        body.extend_from_slice(&(names.len() as u32).to_be_bytes());
        for name in names {
            body.extend_from_slice(&((name.len() + 8) as u32).to_be_bytes());
            body.extend_from_slice(b"mdta");
            body.extend_from_slice(name.as_bytes());
        }
        bx(b"keys", &body)
    }

    /// One `ilst` entry: a box whose type is the 1-based key index, holding a
    /// `data` box of UTF-8 text.
    fn ilst_text(index: u32, value: &str) -> Vec<u8> {
        let mut data = 1u32.to_be_bytes().to_vec(); // type indicator: UTF-8
        data.extend_from_slice(&[0; 4]); // locale
        data.extend_from_slice(value.as_bytes());
        let inner = bx(b"data", &data);
        let mut out = ((inner.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(&index.to_be_bytes());
        out.extend_from_slice(&inner);
        out
    }

    /// A whole file: `mdat` first and `moov` last, the way Apple writes them.
    fn mov_file(moov_body: &[u8]) -> std::io::Cursor<Vec<u8>> {
        let mut file = bx(b"ftyp", b"qt  \0\0\0\0qt  ");
        file.extend_from_slice(&bx(b"mdat", &vec![0xab; 4096]));
        file.extend_from_slice(&bx(b"moov", moov_body));
        std::io::Cursor::new(file)
    }

    /// The motion clip of the live photo in `test/livephoto`, off an iPhone 13
    /// and through Google Takeout - which *renames* Apple's `.MOV` to `.MP4`
    /// without repacking it. The container is still QuickTime, brand `qt  `,
    /// with every `com.apple.quicktime.*` key where the phone left it.
    fn fixture_clip() -> anyhow::Result<Box<dyn crate::fs::ReadSeek>> {
        use crate::fs::{FileSystem, OsFileSystem};
        OsFileSystem::new("test/livephoto").open("IMG_3221.MP4")
    }

    #[test]
    fn test_reads_a_real_clips_keys_past_its_mdat() -> anyhow::Result<()> {
        use anyhow::anyhow;
        crate::test_util::setup_log();
        // The identifier is the whole point: Apple writes the same uuid here and
        // in the still's maker note (see `exif::tests`), and nom_exif drops it.
        // The file is 4 MB of frames before a `moov` at the very end, so this
        // also covers seeking past the `mdat` rather than reading through it.
        let extras =
            parse_mov_extras(&mut fixture_clip()?).ok_or_else(|| anyhow!("no moov found"))?;
        let key = |k: &str| extras.keys.get(k).map(String::as_str);

        assert_eq!(
            key("com.apple.quicktime.content.identifier"),
            Some("E1F3ADCB-67D9-48E5-A716-25F90BB2B50B")
        );
        // The keys nothing reads yet are still collected, which is the point of
        // holding them open-ended rather than picking six out as nom_exif does.
        assert_eq!(key("com.apple.quicktime.live-photo.auto"), Some("1"));
        assert_eq!(
            key("com.apple.quicktime.live-photo.vitality-scoring-version"),
            Some("4")
        );
        assert_eq!(key("com.apple.quicktime.model"), Some("iPhone 13"));
        assert_eq!(
            key("com.apple.quicktime.location.ISO6709"),
            Some("-41.2818+174.7650+064.520/")
        );
        // A float value, which takes a different branch of `decode_data` than
        // the text above.
        assert_eq!(
            key("com.apple.quicktime.location.accuracy.horizontal"),
            Some("3.535534")
        );
        Ok(())
    }

    #[test]
    fn test_reads_a_real_clips_rotation() -> anyhow::Result<()> {
        use anyhow::anyhow;
        crate::test_util::setup_log();
        // Shot in portrait: stored 1920x1440 and played rotated, which is
        // exactly the case nom_exif's discarded matrix used to hide. The still
        // beside it records the same quarter turn as EXIF `Orientation` 6.
        let extras =
            parse_mov_extras(&mut fixture_clip()?).ok_or_else(|| anyhow!("no moov found"))?;
        assert_eq!(extras.display_transform, Some((false, 90)));
        Ok(())
    }

    #[test]
    fn test_reads_meta_as_a_full_box() -> anyhow::Result<()> {
        use anyhow::anyhow;
        crate::test_util::setup_log();
        // The ISO-BMFF spelling of `meta`, with four bytes of version and flags
        // before its children. Synthetic because the fixture is the other
        // spelling - its `meta` is a plain QuickTime container, first child
        // `hdlr` at offset zero - and both turn up in files calling themselves
        // videos.
        let mut meta = vec![0u8; 4];
        meta.extend_from_slice(&keys_box(&["com.apple.quicktime.model"]));
        meta.extend_from_slice(&bx(b"ilst", &ilst_text(1, "iPhone 15")));
        let moov = bx(b"meta", &meta);

        let extras =
            parse_mov_extras(&mut mov_file(&moov)).ok_or_else(|| anyhow!("no moov found"))?;
        assert_eq!(
            extras
                .keys
                .get("com.apple.quicktime.model")
                .map(String::as_str),
            Some("iPhone 15")
        );
        Ok(())
    }

    #[test]
    fn test_display_transform_skips_tracks_without_a_picture() -> anyhow::Result<()> {
        use anyhow::anyhow;
        crate::test_util::setup_log();
        // A sound track first, with no dimensions, so the matrix must come from
        // the video track behind it - otherwise the identity matrix on the
        // audio track masks the rotation. Synthetic because the fixture puts
        // its video track first and so never reaches this branch.
        let mut moov = bx(b"trak", &tkhd(1, 0, 0, 1, 0, 0));
        moov.extend_from_slice(&bx(b"trak", &tkhd(0, 1, -1, 0, 1920, 1440)));
        let extras =
            parse_mov_extras(&mut mov_file(&moov)).ok_or_else(|| anyhow!("no moov found"))?;
        assert_eq!(extras.display_transform, Some((false, 90)));
        Ok(())
    }

    #[test]
    fn test_matrix_transform_covers_the_eight_rigid_forms() {
        // Mirroring is applied before the rotation, so the mirrored quarter
        // turns are the transposes of the plain ones, not their negations.
        for (a, b, c, d, expected) in [
            (1, 0, 0, 1, (false, 0)),
            (0, 1, -1, 0, (false, 90)),
            (-1, 0, 0, -1, (false, 180)),
            (0, -1, 1, 0, (false, -90)),
            (-1, 0, 0, 1, (true, 0)),
            (0, -1, -1, 0, (true, 90)),
            (1, 0, 0, -1, (true, 180)),
            (0, 1, 1, 0, (true, -90)),
        ] {
            assert_eq!(
                matrix_transform(&matrix(a, b, c, d)),
                Some(expected),
                "matrix {a} {b} {c} {d}"
            );
        }
        // A scale is not a rigid transform, and guessing a rotation from it
        // would be worse than admitting there isn't one.
        let mut scaled = matrix(1, 0, 0, 1);
        scaled[0..4].copy_from_slice(&(65536 / 2i32).to_be_bytes());
        assert_eq!(matrix_transform(&scaled), None);
    }

    #[test]
    fn test_no_moov_is_not_an_error() {
        crate::test_util::setup_log();
        // Matroska reaches the track parser too, and has no moov at all.
        let mut webm = std::io::Cursor::new(vec![0x1a, 0x45, 0xdf, 0xa3, 0x9f, 0x42, 0x86, 0x81]);
        assert!(parse_mov_extras(&mut webm).is_none());
    }

    #[test]
    fn test_truncated_boxes_do_not_panic() {
        crate::test_util::setup_log();
        // Every length in the file is attacker-controlled; none of them may
        // index past the buffer.
        let mut meta = keys_box(&["com.apple.quicktime.content.identifier"]);
        meta.extend_from_slice(&bx(b"ilst", &ilst_text(1, "an-id")));
        let full = bx(b"moov", &bx(b"meta", &meta));
        for cut in 0..full.len() {
            let mut truncated = std::io::Cursor::new(full[..cut].to_vec());
            let _ = parse_mov_extras(&mut truncated);
        }
        // A size field claiming more than the file holds.
        let mut lying = full.clone();
        lying[0..4].copy_from_slice(&u32::MAX.to_be_bytes());
        let _ = parse_mov_extras(&mut std::io::Cursor::new(lying));
    }

    #[test]
    fn test_a_truncated_real_clip_does_not_panic() -> anyhow::Result<()> {
        use std::io::Read;
        crate::test_util::setup_log();
        // The synthetic corpus above only has the boxes this module reads. A
        // real file is full of ones it doesn't - `wide`, `free`, five `trak`s,
        // `udta` - and a copy cut short mid-atom is what a failed download or a
        // half-written export actually looks like.
        let mut whole = Vec::new();
        fixture_clip()?.read_to_end(&mut whole)?;

        // Walking every one of four million offsets would dominate the suite;
        // stepping through it hits every box boundary region without that.
        for cut in (0..whole.len()).step_by(4099) {
            let mut truncated = std::io::Cursor::new(whole[..cut].to_vec());
            let _ = parse_mov_extras(&mut truncated);
        }
        // And the last bytes one at a time, where the `moov` actually is.
        for cut in whole.len().saturating_sub(512)..whole.len() {
            let mut truncated = std::io::Cursor::new(whole[..cut].to_vec());
            let _ = parse_mov_extras(&mut truncated);
        }
        Ok(())
    }
}
