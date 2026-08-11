//! Apple's EXIF `MakerNote` (tag `0x927C`), which is where the still half of a
//! Live Photo records the identifier it shares with its video.
//!
//! The block is an ordinary TIFF IFD behind a 14-byte header: `Apple iOS\0`, two
//! version bytes, then `MM` or `II` for the byte order. Offsets inside it are
//! counted from the start of that header rather than from the file's own TIFF
//! header, so these bytes are self-contained and can be parsed on their own.
//!
//! `nom_exif` hands the block over as raw bytes and does not read it, hence this
//! module. Everything is bounds-checked and any surprise is read as "no
//! identifier" — a maker note is whatever the camera wrote, and half the field
//! is undocumented.

const APPLE_HEADER: &[u8] = b"Apple iOS\0";

/// `Apple iOS\0`, two version bytes, then the two-byte order mark.
const IFD_OFFSET: usize = 14;
const BYTE_ORDER_OFFSET: usize = 12;
const ENTRY_SIZE: usize = 12;

/// ExifTool's `ContentIdentifier`: the UUID a Live Photo's still and video both
/// carry.
const TAG_CONTENT_IDENTIFIER: u16 = 0x0011;

/// TIFF format 2, NUL-terminated ASCII.
const FORMAT_ASCII: u16 = 2;

/// A value of four bytes or fewer sits in the entry itself; anything longer is
/// reached through the offset stored there.
const INLINE_VALUE_BYTES: usize = 4;

/// Long enough for the UUID Apple writes, short enough that a misread length
/// cannot turn into a large allocation.
const MAX_IDENTIFIER_BYTES: usize = 128;

/// The content identifier in an Apple maker note, or `None` for any other
/// maker note.
pub(crate) fn content_identifier(maker_note: &[u8]) -> Option<String> {
    let big_endian = byte_order(maker_note)?;
    let entry_count = read_u16(maker_note, IFD_OFFSET, big_endian)?;
    for index in 0..entry_count as usize {
        let entry = IFD_OFFSET + 2 + index * ENTRY_SIZE;
        if read_u16(maker_note, entry, big_endian)? != TAG_CONTENT_IDENTIFIER {
            continue;
        }
        if read_u16(maker_note, entry + 2, big_endian)? != FORMAT_ASCII {
            return None;
        }
        let length = read_u32(maker_note, entry + 4, big_endian)? as usize;
        if length > MAX_IDENTIFIER_BYTES {
            return None;
        }
        let value = value_bytes(maker_note, entry + 8, length, big_endian)?;
        return ascii_value(value);
    }
    None
}

/// `Some(true)` for big-endian, and `None` for a maker note that is not Apple's
/// or does not spell its byte order.
fn byte_order(maker_note: &[u8]) -> Option<bool> {
    if !maker_note.starts_with(APPLE_HEADER) {
        return None;
    }
    match maker_note.get(BYTE_ORDER_OFFSET..BYTE_ORDER_OFFSET + 2)? {
        b"MM" => Some(true),
        b"II" => Some(false),
        _ => None,
    }
}

fn read_u16(bytes: &[u8], offset: usize, big_endian: bool) -> Option<u16> {
    let raw: [u8; 2] = bytes.get(offset..offset + 2)?.try_into().ok()?;
    Some(if big_endian {
        u16::from_be_bytes(raw)
    } else {
        u16::from_le_bytes(raw)
    })
}

fn read_u32(bytes: &[u8], offset: usize, big_endian: bool) -> Option<u32> {
    let raw: [u8; 4] = bytes.get(offset..offset + 4)?.try_into().ok()?;
    Some(if big_endian {
        u32::from_be_bytes(raw)
    } else {
        u32::from_le_bytes(raw)
    })
}

/// The bytes an IFD entry points at, whether it stores them inline or by offset.
fn value_bytes(
    maker_note: &[u8],
    value_field: usize,
    length: usize,
    big_endian: bool,
) -> Option<&[u8]> {
    if length <= INLINE_VALUE_BYTES {
        return maker_note.get(value_field..value_field + length);
    }
    let offset = read_u32(maker_note, value_field, big_endian)? as usize;
    maker_note.get(offset..offset.checked_add(length)?)
}

/// An ASCII IFD value as a string: everything up to the terminating NUL, and
/// only if it is printable — a mis-parsed offset lands on arbitrary bytes, and
/// this identifier goes on to name output files.
fn ascii_value(bytes: &[u8]) -> Option<String> {
    let text = bytes.split(|b| *b == 0).next().unwrap_or(bytes);
    if !text.iter().all(|b| (0x20..0x7f).contains(b)) {
        return None;
    }
    let text = String::from_utf8_lossy(text).trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDENTIFIER: &str = "11111111-2222-3333-4444-555555555555";

    /// A maker note holding a single entry, laid out the way an iPhone writes
    /// one: the value is too long to sit inline, so it lives past the IFD and is
    /// reached by an offset counted from byte zero of the block.
    fn apple_maker_note(big_endian: bool, tag: u16, format: u16, value: &[u8]) -> Vec<u8> {
        let u16_bytes = |v: u16| {
            if big_endian {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        let u32_bytes = |v: u32| {
            if big_endian {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };

        let mut out = Vec::new();
        out.extend_from_slice(APPLE_HEADER);
        out.extend_from_slice(&[0x00, 0x01]);
        out.extend_from_slice(if big_endian { b"MM" } else { b"II" });
        out.extend_from_slice(&u16_bytes(1)); // one entry
        out.extend_from_slice(&u16_bytes(tag));
        out.extend_from_slice(&u16_bytes(format));
        out.extend_from_slice(&u32_bytes(value.len() as u32));
        // Value area starts after the entry and the next-IFD offset.
        let value_offset = (IFD_OFFSET + 2 + ENTRY_SIZE + 4) as u32;
        if value.len() <= INLINE_VALUE_BYTES {
            let mut inline = value.to_vec();
            inline.resize(INLINE_VALUE_BYTES, 0);
            out.extend_from_slice(&inline);
        } else {
            out.extend_from_slice(&u32_bytes(value_offset));
        }
        out.extend_from_slice(&u32_bytes(0)); // no next IFD
        out.extend_from_slice(value);
        out
    }

    fn nul_terminated(s: &str) -> Vec<u8> {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        v
    }

    #[test]
    fn test_content_identifier_is_read_in_either_byte_order() {
        for big_endian in [true, false] {
            let mn = apple_maker_note(
                big_endian,
                TAG_CONTENT_IDENTIFIER,
                FORMAT_ASCII,
                &nul_terminated(IDENTIFIER),
            );
            assert_eq!(
                content_identifier(&mn).as_deref(),
                Some(IDENTIFIER),
                "big_endian={big_endian}"
            );
        }
    }

    /// A maker note is whatever the camera wrote, so every one of these has to
    /// come back as "no identifier" rather than a panic or a wrong answer.
    #[test]
    fn test_unreadable_maker_notes_yield_nothing() {
        let good = apple_maker_note(
            true,
            TAG_CONTENT_IDENTIFIER,
            FORMAT_ASCII,
            &nul_terminated(IDENTIFIER),
        );

        // Another vendor's maker note, and Apple's with a byte order it never
        // writes.
        let mut wrong_order = good.clone();
        wrong_order[BYTE_ORDER_OFFSET] = b'X';
        // The tag is absent, or present with a format that is not a string.
        let other_tag = apple_maker_note(true, 0x000b, FORMAT_ASCII, &nul_terminated(IDENTIFIER));
        let wrong_format =
            apple_maker_note(true, TAG_CONTENT_IDENTIFIER, 7, &nul_terminated(IDENTIFIER));
        // A length that runs off the end of the block, and one large enough to
        // matter if it were ever allocated.
        let mut overlong = good.clone();
        overlong.truncate(overlong.len() - 4);
        let huge = apple_maker_note(
            true,
            TAG_CONTENT_IDENTIFIER,
            FORMAT_ASCII,
            &[b'a'; MAX_IDENTIFIER_BYTES + 1],
        );
        // Bytes that are not text at all, and an empty value.
        let binary = apple_maker_note(true, TAG_CONTENT_IDENTIFIER, FORMAT_ASCII, &[1, 2, 3, 4, 5]);
        let blank = apple_maker_note(
            true,
            TAG_CONTENT_IDENTIFIER,
            FORMAT_ASCII,
            &nul_terminated("   "),
        );

        for (why, bytes) in [
            ("empty", Vec::new()),
            ("not Apple's", b"Nikon\0truncated".to_vec()),
            ("header only", APPLE_HEADER.to_vec()),
            ("unknown byte order", wrong_order),
            ("a different tag", other_tag),
            ("a non-string format", wrong_format),
            ("a value that runs off the end", overlong),
            ("an absurd length", huge),
            ("non-printable bytes", binary),
            ("nothing but spaces", blank),
            ("truncated mid-IFD", good[..IFD_OFFSET + 4].to_vec()),
        ] {
            assert_eq!(content_identifier(&bytes), None, "{why}");
        }
    }

    /// Long values are reached by offset, short ones sit in the entry — and
    /// tag 0x0011 is only ever the long kind, so the inline path is asserted on
    /// its own.
    #[test]
    fn test_short_values_are_read_inline() {
        let mn = apple_maker_note(true, TAG_CONTENT_IDENTIFIER, FORMAT_ASCII, b"ab\0");
        assert_eq!(content_identifier(&mn).as_deref(), Some("ab"));
    }
}
