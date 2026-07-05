use super::{fake_png, real_jpeg};
use crate::exif_util::parse_exif_info;
use crate::test_util::setup_log;
use anyhow::Result;
use std::io::Cursor;

#[test]
fn exif_never_panics_on_malformed_bytes() -> Result<()> {
    setup_log();
    let truncated = real_jpeg()?.into_iter().take(64).collect::<Vec<u8>>();
    let cases: Vec<Vec<u8>> = vec![
        vec![],           // empty
        vec![0x00],       // single byte
        vec![0xff, 0xd8], // SOI marker only
        b"this is plainly not an image".to_vec(),
        (0u8..=255).cycle().take(4096).collect(), // structured garbage
        truncated,                                // valid JPEG header, cut short
        fake_png(),                               // wrong container for the bytes
        b"GIF89a\x01\x00\x01\x00".to_vec(),       // a stubby GIF
    ];
    // parse_exif_info only surfaces an error from the initial seek, which an
    // in-memory cursor cannot fail, so every malformed case must come back Ok
    // (with None or an empty tag set) rather than panicking or erroring.
    for bytes in cases {
        let res = parse_exif_info(Cursor::new(bytes.clone()));
        assert!(
            res.is_ok(),
            "parse_exif_info should swallow malformed input, got Err for {} bytes",
            bytes.len()
        );
    }
    Ok(())
}
