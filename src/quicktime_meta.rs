//! The QuickTime metadata Apple writes into a Live Photo's video: the
//! `com.apple.quicktime.content.identifier` key, which holds the same UUID as the
//! still's maker note.
//!
//! `nom_exif` walks these atoms but only surfaces the handful it maps onto
//! `TrackInfoTag`, and the content identifier is not one of them — hence this
//! module.
//!
//! An ISO base media file is a tree of *atoms*, each `[u32 size][4-byte type]`
//! followed by its body. The metadata sits at `moov/meta`, split across two
//! sibling atoms: `keys` names the fields in order, and `ilst` holds the values,
//! each labelled with its 1-based position in that list. So reading one field
//! means finding its name's position, then the value wearing that number.
//!
//! Only the `meta` atom is ever read into memory. `moov` also holds the sample
//! tables, which for a long video run to megabytes.

use std::io::{Cursor, Read, Seek, SeekFrom};

const CONTENT_IDENTIFIER_KEY: &str = "com.apple.quicktime.content.identifier";

/// An atom header is a 32-bit size and a four-character type.
const HEADER_BYTES: u64 = 8;
/// A size of 1 means the real size is a 64-bit value following the header.
const LARGE_SIZE_MARKER: u32 = 1;
const LARGE_HEADER_BYTES: u64 = 16;

/// Version and flags, on the atoms that carry them.
const FULL_ATOM_PREFIX: u64 = 4;
/// `keys` is a full atom whose body then opens with a 32-bit entry count.
const KEYS_PREFIX: u64 = FULL_ATOM_PREFIX + 4;
/// A `data` atom's body opens with a 32-bit value type and a 32-bit locale.
const DATA_PREFIX: u64 = 8;

/// Room for a long list of keys and their values, while still being far less
/// than a misread size would ask for.
const MAX_META_BYTES: u64 = 1 << 20;
/// Generous for one key name or one value.
const MAX_VALUE_BYTES: u64 = 4096;

/// One atom: its four-character type — which `ilst` uses to hold a number rather
/// than a name — and the byte range of its body.
struct Atom {
    kind: [u8; 4],
    start: u64,
    end: u64,
}

impl Atom {
    /// The type read as the 1-based position in `keys` that an `ilst` item uses
    /// to say which field it is the value of.
    fn key_index(&self) -> u32 {
        u32::from_be_bytes(self.kind)
    }
}

/// The Apple content identifier in this video's QuickTime metadata, or `None`
/// for a file that is not an ISO base media file or does not carry one.
pub(crate) fn content_identifier<R: Read + Seek>(reader: &mut R) -> Option<String> {
    let file_end = reader.seek(SeekFrom::End(0)).ok()?;
    let moov = find_atom(reader, b"moov", 0, file_end)?;
    let meta = find_atom(reader, b"meta", moov.start, moov.end)?;

    // Read the atom once; everything below is a few hundred bytes of lookups
    // within it.
    let bytes = read_body(reader, meta.start, meta.end, MAX_META_BYTES)?;
    let end = bytes.len() as u64;
    let mut body = Cursor::new(bytes);

    // QuickTime's `meta` holds its children directly; the ISO base media spec
    // makes it a full atom, so an `.mp4` puts version and flags in front of
    // them. Whichever offset yields both atoms is the one this file used.
    let (keys, ilst) = [0, FULL_ATOM_PREFIX].into_iter().find_map(|from| {
        Some((
            find_atom(&mut body, b"keys", from, end)?,
            find_atom(&mut body, b"ilst", from, end)?,
        ))
    })?;

    let index = key_index(&mut body, &keys, CONTENT_IDENTIFIER_KEY)?;
    let item = atoms(&mut body, ilst.start, ilst.end)
        .into_iter()
        .find(|atom| atom.key_index() == index)?;
    let data = find_atom(&mut body, b"data", item.start, item.end)?;

    let value = read_body(
        &mut body,
        data.start + DATA_PREFIX,
        data.end,
        MAX_VALUE_BYTES,
    )?;
    let text = String::from_utf8(value).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// The 1-based position of `name` in a `keys` atom, which is how `ilst` labels
/// the value that belongs to it.
fn key_index<R: Read + Seek>(reader: &mut R, keys: &Atom, name: &str) -> Option<u32> {
    for (position, entry) in atoms(reader, keys.start + KEYS_PREFIX, keys.end)
        .into_iter()
        .enumerate()
    {
        // A key too long to read is one that cannot be the one being looked
        // for, so it is skipped rather than abandoning the search.
        let Some(key) = read_body(reader, entry.start, entry.end, MAX_VALUE_BYTES) else {
            continue;
        };
        if key == name.as_bytes() {
            return u32::try_from(position + 1).ok();
        }
    }
    None
}

fn find_atom<R: Read + Seek>(reader: &mut R, kind: &[u8; 4], start: u64, end: u64) -> Option<Atom> {
    atoms(reader, start, end)
        .into_iter()
        .find(|atom| atom.kind == *kind)
}

/// An atom's body, never more than `limit` bytes. Every length here comes from
/// a size field in the file, so the cap is what keeps a misread size from
/// becoming a huge read.
fn read_body<R: Read + Seek>(reader: &mut R, start: u64, end: u64, limit: u64) -> Option<Vec<u8>> {
    read_exact_at(reader, start, end.saturating_sub(start).min(limit))
}

/// Exactly `len` bytes from `start`; `None` on a short read, so a caller reading
/// a length the file declared cannot act on less than it asked for.
fn read_exact_at<R: Read + Seek>(reader: &mut R, start: u64, len: u64) -> Option<Vec<u8>> {
    reader.seek(SeekFrom::Start(start)).ok()?;
    let mut buffer = vec![0u8; usize::try_from(len).ok()?];
    reader.read_exact(&mut buffer).ok()?;
    Some(buffer)
}

/// The atoms laid out between `start` and `end`, headers only — no body is read.
///
/// Stops at the first header that does not make sense rather than failing, so a
/// truncated or padded tail costs the atoms beyond it and nothing before.
fn atoms<R: Read + Seek>(reader: &mut R, start: u64, end: u64) -> Vec<Atom> {
    let mut found = Vec::new();
    let mut pos = start;
    while pos + HEADER_BYTES <= end {
        let Some(header) = read_exact_at(reader, pos, HEADER_BYTES) else {
            break;
        };
        let (Ok(size_bytes), Ok(kind)) = (
            <[u8; 4]>::try_from(&header[..4]),
            <[u8; 4]>::try_from(&header[4..8]),
        ) else {
            break;
        };
        let declared = u32::from_be_bytes(size_bytes);
        let (header_len, size) = if declared == LARGE_SIZE_MARKER {
            let large = read_exact_at(reader, pos + HEADER_BYTES, 8)
                .and_then(|bytes| <[u8; 8]>::try_from(bytes.as_slice()).ok());
            let Some(large) = large else {
                break;
            };
            (LARGE_HEADER_BYTES, u64::from_be_bytes(large))
        } else if declared == 0 {
            // Zero means the atom runs to the end of its parent.
            (HEADER_BYTES, end - pos)
        } else {
            (HEADER_BYTES, declared as u64)
        };
        if size < header_len || pos + size > end {
            break;
        }
        found.push(Atom {
            kind,
            start: pos + header_len,
            end: pos + size,
        });
        pos += size;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{FileSystem, OsFileSystem};

    /// One atom: a big-endian size, a four-character type, then the body.
    fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    fn keys_atom(names: &[&str]) -> Vec<u8> {
        let mut body = vec![0, 0, 0, 0]; // version and flags
        body.extend_from_slice(&(names.len() as u32).to_be_bytes());
        for name in names {
            body.extend_from_slice(&atom(b"mdta", name.as_bytes()));
        }
        atom(b"keys", &body)
    }

    fn ilst_atom(values: &[(u32, &str)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (index, value) in values {
            let mut data = vec![0, 0, 0, 1, 0, 0, 0, 0]; // UTF-8, no locale
            data.extend_from_slice(value.as_bytes());
            let item = atom(b"data", &data);
            let mut framed = ((item.len() + 8) as u32).to_be_bytes().to_vec();
            framed.extend_from_slice(&index.to_be_bytes());
            framed.extend_from_slice(&item);
            body.extend_from_slice(&framed);
        }
        atom(b"ilst", &body)
    }

    /// A whole file: an `mdat` standing in for the video data, then the `moov`
    /// the metadata hangs off.
    fn quicktime_file(meta_body: Vec<u8>, full_meta_atom: bool) -> Vec<u8> {
        let meta_body = if full_meta_atom {
            let mut prefixed = vec![0, 0, 0, 0];
            prefixed.extend_from_slice(&meta_body);
            prefixed
        } else {
            meta_body
        };
        let moov = atom(b"moov", &atom(b"meta", &meta_body));
        let mut file = atom(b"ftyp", b"qt  ");
        file.extend_from_slice(&atom(b"mdat", &[0u8; 64]));
        file.extend_from_slice(&moov);
        file
    }

    fn meta_body(names: &[&str], values: &[(u32, &str)]) -> Vec<u8> {
        let mut body = atom(b"hdlr", &[0u8; 24]);
        body.extend_from_slice(&keys_atom(names));
        body.extend_from_slice(&ilst_atom(values));
        body
    }

    const IDENTIFIER: &str = "11111111-2222-3333-4444-555555555555";

    /// The value is found by matching its key's position, not by assuming an
    /// order, so the identifier here is deliberately neither first nor last.
    /// Both `meta` spellings are covered: QuickTime's plain atom and the ISO
    /// base media full atom an `.mp4` uses.
    #[test]
    fn test_content_identifier_from_keys_and_values() {
        let names = [
            "com.apple.quicktime.make",
            CONTENT_IDENTIFIER_KEY,
            "com.apple.quicktime.model",
        ];
        let values = [(1, "Apple"), (2, IDENTIFIER), (3, "iPhone 15")];
        for full_meta_atom in [false, true] {
            let file = quicktime_file(meta_body(&names, &values), full_meta_atom);
            assert_eq!(
                content_identifier(&mut Cursor::new(file)).as_deref(),
                Some(IDENTIFIER),
                "full_meta_atom={full_meta_atom}"
            );
        }
    }

    #[test]
    fn test_files_without_the_key_yield_nothing() {
        let named = |names: &[&str], values: &[(u32, &str)]| {
            quicktime_file(meta_body(names, values), false)
        };
        for (why, bytes) in [
            ("empty", Vec::new()),
            ("not an ISO base media file", b"just some bytes".to_vec()),
            ("no moov", atom(b"ftyp", b"qt  ")),
            ("no meta", atom(b"moov", &atom(b"mvhd", &[0u8; 32]))),
            (
                "keys without the content identifier",
                named(&["com.apple.quicktime.make"], &[(1, "Apple")]),
            ),
            (
                "a key with no matching value",
                named(&[CONTENT_IDENTIFIER_KEY], &[(2, IDENTIFIER)]),
            ),
            (
                "an empty value",
                named(&[CONTENT_IDENTIFIER_KEY], &[(1, "  ")]),
            ),
        ] {
            assert_eq!(content_identifier(&mut Cursor::new(bytes)), None, "{why}");
        }
    }

    /// Videos are read straight off whatever an export wrote, so a truncated or
    /// scrambled file must come back empty rather than panic or hang.
    #[test]
    fn test_malformed_files_never_panic() {
        let good = quicktime_file(
            meta_body(&[CONTENT_IDENTIFIER_KEY], &[(1, IDENTIFIER)]),
            false,
        );
        for cut in [1usize, 8, 16, 40, good.len() / 2, good.len() - 1] {
            let _ = content_identifier(&mut Cursor::new(good[..cut].to_vec()));
        }
        // An atom claiming to be far larger than the file, and one claiming to
        // be smaller than its own header.
        for size in [u32::MAX, 0, 1, 3] {
            let mut broken = good.clone();
            broken[..4].copy_from_slice(&size.to_be_bytes());
            let _ = content_identifier(&mut Cursor::new(broken));
        }
    }

    /// The real thing, on a fixture carrying the metadata an iPhone writes.
    #[test]
    fn test_content_identifier_from_a_real_video() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let c = OsFileSystem::new("test/live_photo");
        let mut reader = c.open("clip.mov")?;
        assert_eq!(content_identifier(&mut reader).as_deref(), Some(IDENTIFIER));

        // A video that is not half of a Live Photo carries no identifier.
        let c = OsFileSystem::new("test");
        let mut reader = c.open("Hello.mov")?;
        assert_eq!(content_identifier(&mut reader), None);
        Ok(())
    }
}
