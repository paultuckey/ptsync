//! Content sniffing (`src/file_type.rs`): a file must be classified by its bytes,
//! not its extension, so "a JPEG that's really a PNG" is caught.

use super::{fake_png, real_jpeg};
use crate::exif_util::parse_exif_info;
use crate::file_type::{AccurateFileType, determine_file_type};
use crate::test_util::setup_log;
use anyhow::Result;
use std::io::Cursor;

#[test]
fn file_type_detection_is_content_based_not_extension() -> Result<()> {
    setup_log();
    // A file that lies about its type via its extension must be classified by
    // its bytes, so a re-extension attack cannot smuggle unsupported content
    // through as media.
    let png_named_jpg = fake_png();
    assert_eq!(
        determine_file_type(Cursor::new(png_named_jpg.clone()), &"photo.jpg".to_string())?,
        AccurateFileType::Png,
        "a PNG named .jpg must be detected as PNG from its content"
    );
    // ...and feeding that same mislabelled file to the EXIF parser must not panic.
    assert!(parse_exif_info(Cursor::new(png_named_jpg)).is_ok());

    // A real JPEG named .png is still a JPEG.
    assert_eq!(
        determine_file_type(Cursor::new(real_jpeg()?), &"photo.png".to_string())?,
        AccurateFileType::Jpg
    );

    // Empty and garbage content resolve to Unsupported (never a panic).
    assert_eq!(
        determine_file_type(Cursor::new(Vec::<u8>::new()), &"x.jpg".to_string())?,
        AccurateFileType::Unsupported
    );
    assert_eq!(
        determine_file_type(
            Cursor::new(vec![0xde, 0xad, 0xbe, 0xef]),
            &"x.mp4".to_string()
        )?,
        AccurateFileType::Unsupported
    );
    Ok(())
}
