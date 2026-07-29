//! Apple's maker note, the still half of a live photo's pairing id.
//!
//! `nom_exif` reads the `MakerNote` tag but decodes no vendor sub-tags, handing
//! the block over as an opaque [`nom_exif::EntryValue::Undefined`] blob - which
//! [`crate::metadata::exif::parse_exif_info`] would otherwise drop on the floor.
//! Inside it, tag `0x0011` is the `ContentIdentifier`: the same uuid Apple
//! writes into the clip's `com.apple.quicktime.content.identifier`, and the
//! only thing in either file that actually says the two belong together.
//!
//! The block is a TIFF IFD behind a short Apple header, so it is read the same
//! way any IFD is - with the one wrinkle that its offsets are counted from the
//! start of the block rather than the start of the file.

/// Every Apple maker note opens with this; anything else is another vendor's.
const APPLE_HEADER: &[u8] = b"Apple iOS\0";

/// The byte-order mark sits at 12, and the IFD itself begins at 14 - after the
/// header, two bytes of version, and the mark.
const BYTE_ORDER_AT: usize = 12;
const IFD_AT: usize = 14;

/// TIFF format code for a NUL-terminated ASCII string.
const FORMAT_ASCII: u16 = 2;

/// Apple's tag for the uuid shared by both halves of a live photo.
const TAG_CONTENT_IDENTIFIER: u16 = 0x0011;

/// The `ContentIdentifier` from an Apple maker note, if this is one and it has
/// the tag. Stills that were edited or re-encoded lose the maker note entirely,
/// so absence is ordinary and not an error.
pub(crate) fn content_identifier(blob: &[u8]) -> Option<String> {
    ascii_tag(blob, TAG_CONTENT_IDENTIFIER)
}

/// Read one ASCII tag out of the maker note's IFD.
///
/// Values of four bytes or fewer are stored in the entry itself; longer ones -
/// a uuid is 37 bytes with its terminator - are stored at an offset counted
/// from the start of the maker note, which is why the whole blob is needed and
/// not just the IFD.
fn ascii_tag(blob: &[u8], want: u16) -> Option<String> {
    if !blob.starts_with(APPLE_HEADER) {
        return None;
    }
    let big_endian = match blob.get(BYTE_ORDER_AT..BYTE_ORDER_AT + 2)? {
        b"MM" => true,
        b"II" => false,
        _ => return None,
    };
    let at_u16 = |offset: usize| -> Option<u16> {
        let bytes = blob.get(offset..offset + 2)?.try_into().ok()?;
        Some(if big_endian {
            u16::from_be_bytes(bytes)
        } else {
            u16::from_le_bytes(bytes)
        })
    };
    let at_u32 = |offset: usize| -> Option<u32> {
        let bytes = blob.get(offset..offset + 4)?.try_into().ok()?;
        Some(if big_endian {
            u32::from_be_bytes(bytes)
        } else {
            u32::from_le_bytes(bytes)
        })
    };

    let entries = at_u16(IFD_AT)?;
    for i in 0..entries as usize {
        // Twelve bytes each: tag, format, component count, then value or offset.
        let entry = IFD_AT + 2 + i.checked_mul(12)?;
        if at_u16(entry)? != want {
            continue;
        }
        if at_u16(entry + 2)? != FORMAT_ASCII {
            return None;
        }
        let len = at_u32(entry + 4)? as usize;
        let start = if len <= 4 {
            entry + 8
        } else {
            at_u32(entry + 8)? as usize
        };
        let bytes = blob.get(start..start.checked_add(len)?)?;
        let text = String::from_utf8_lossy(bytes)
            .trim_end_matches('\0')
            .trim()
            .to_string();
        return (!text.is_empty()).then_some(text);
    }
    None
}

#[cfg(test)]
mod tests {
    //! The happy path is covered against a real iPhone still in
    //! `super::super::exif::tests` - the blob only ever arrives via `nom_exif`,
    //! so that is the only place one can be had without hand-assembling it.
    //! What is left here are the cases a fixture cannot show: another vendor's
    //! note, a byte order Apple does not write, and bytes cut short.

    use super::*;

    /// Build a big-endian Apple maker note holding one ASCII tag, laid out the
    /// way a real one is: the IFD first, then the values it points at.
    fn maker_note(tag: u16, value: &str) -> Vec<u8> {
        let mut blob = APPLE_HEADER.to_vec();
        blob.extend_from_slice(&[0x00, 0x01]); // version
        blob.extend_from_slice(b"MM");
        blob.extend_from_slice(&1u16.to_be_bytes()); // one entry
        let value_at = IFD_AT + 2 + 12 + 4; // IFD, entry, next-IFD pointer
        blob.extend_from_slice(&tag.to_be_bytes());
        blob.extend_from_slice(&FORMAT_ASCII.to_be_bytes());
        blob.extend_from_slice(&((value.len() + 1) as u32).to_be_bytes());
        blob.extend_from_slice(&(value_at as u32).to_be_bytes());
        blob.extend_from_slice(&0u32.to_be_bytes()); // no next IFD
        blob.extend_from_slice(value.as_bytes());
        blob.push(0);
        blob
    }

    #[test]
    fn test_ignores_other_vendors_and_other_tags() {
        // Canon and Nikon maker notes have their own layouts and their own
        // meaning for 0x0011; reading one as Apple's would invent a pairing.
        let mut canon = b"Canon\0\0\0".to_vec();
        canon.extend_from_slice(&[0u8; 64]);
        assert_eq!(content_identifier(&canon), None);

        // An Apple note without the tag - a still that isn't a live photo.
        let blob = maker_note(0x0001, "15");
        assert_eq!(content_identifier(&blob), None);
    }

    #[test]
    fn test_truncated_blobs_do_not_panic() {
        // The lengths and offsets in here are file data, so every one of them
        // has to be treated as a lie until it indexes successfully.
        let blob = maker_note(
            TAG_CONTENT_IDENTIFIER,
            "2A2E6BEF-6BF7-486C-8E74-8442A6F8648D",
        );
        for cut in 0..blob.len() {
            let _ = content_identifier(&blob[..cut]);
        }
        // An offset pointing past the end of the block.
        let mut lying = blob.clone();
        let offset_at = IFD_AT + 2 + 8;
        lying[offset_at..offset_at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(content_identifier(&lying), None);
        // A component count larger than the block.
        let mut long = blob.clone();
        let count_at = IFD_AT + 2 + 4;
        long[count_at..count_at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(content_identifier(&long), None);
    }

    #[test]
    fn test_reads_a_little_endian_note() {
        // Apple writes MM in practice, but the mark is there to be honoured.
        let uuid = "D1071490-0CFC-4FE3-B825-D7F77F7B7E08";
        let mut blob = APPLE_HEADER.to_vec();
        blob.extend_from_slice(&[0x00, 0x01]);
        blob.extend_from_slice(b"II");
        blob.extend_from_slice(&1u16.to_le_bytes());
        let value_at = IFD_AT + 2 + 12 + 4;
        blob.extend_from_slice(&TAG_CONTENT_IDENTIFIER.to_le_bytes());
        blob.extend_from_slice(&FORMAT_ASCII.to_le_bytes());
        blob.extend_from_slice(&((uuid.len() + 1) as u32).to_le_bytes());
        blob.extend_from_slice(&(value_at as u32).to_le_bytes());
        blob.extend_from_slice(&0u32.to_le_bytes());
        blob.extend_from_slice(uuid.as_bytes());
        blob.push(0);
        assert_eq!(content_identifier(&blob).as_deref(), Some(uuid));
    }
}
