//! Everything ptsync knows about a media file, and where it learned it.
//!
//! Four sources, on two axes. Two are **embedded** in the media file's own
//! bytes and read from an open reader:
//!
//! - [`exif`] - the camera's tags in an image, with [`apple_makernote`] beneath
//!   it for the vendor block `nom_exif` leaves opaque
//! - [`track`] - a video's `moov` metadata, with [`isobmff`] beneath it for the
//!   keys and display matrix `nom_exif` discards
//!
//! Two are **sidecars**, separate files found by name beside the media:
//!
//! - [`xmp`] - what a photo tool (Lightroom, darktable, digiKam) wrote
//! - [`supplemental`] - what Google Takeout exported as JSON
//!
//! The two container-format readers are private to this module: they are
//! implementation details of one parser each, and nothing outside `metadata`
//! has any business calling them.
//!
//! [`MediaFileInfo`] is the union of all four. Where they disagree,
//! [`reconcile`] decides - and it is the only place that does, so a photo's note
//! and its database row cannot come to different conclusions.

use crate::file_type::{
    AccurateFileType, MetadataType, QuickFileType, determine_file_type, metadata_type,
};
use crate::util::{HashInfo, ScanInfo};
use anyhow::anyhow;
use serde::Serialize;
use std::io::{Read, Seek};
use tracing::warn;

// Format readers for the blocks `nom_exif` hands over undecoded. Private: only
// `exif` and `track` may reach them, and the compiler enforces it.
mod apple_makernote;
mod isobmff;

pub(crate) mod exif;
pub(crate) mod reconcile;
pub(crate) mod supplemental;
pub(crate) mod track;
pub(crate) mod xmp;

use exif::PsExifInfo;
use supplemental::PsSupplementalInfo;
use track::PsTrackInfo;
use xmp::PsXmpInfo;

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all(serialize = "camelCase"))]
pub(crate) struct MediaFileInfo {
    pub(crate) original_file_this_run: String,
    pub(crate) original_path: Vec<String>,
    pub(crate) quick_file_type: QuickFileType,
    pub(crate) exif_info: Option<PsExifInfo>,
    pub(crate) track_info: Option<PsTrackInfo>,
    pub(crate) accurate_file_type: AccurateFileType,
    pub(crate) hash_info: HashInfo,
    pub(crate) supp_info: Option<PsSupplementalInfo>,
    pub(crate) xmp_info: Option<PsXmpInfo>,
    // Modified time of the file
    pub(crate) modified: Option<i64>,
    pub(crate) created: Option<i64>,
    // Size of the file in bytes
    pub(crate) file_size: u64,
}

/// Read the embedded metadata and fold it together with the sidecars the caller
/// already found.
///
/// Which embedded parser runs is decided by the file's real type rather than its
/// extension, so a `.jpg` that is actually a video still reaches [`track`].
pub(crate) fn media_file_info_from_readable<R: Read + Seek>(
    si: &ScanInfo,
    reader: &mut R,
    supp_info: &Option<PsSupplementalInfo>,
    xmp_info: &Option<PsXmpInfo>,
    hash_info: &HashInfo,
) -> anyhow::Result<MediaFileInfo> {
    let name = &si.file_path;
    let guessed_ff = determine_file_type(&mut *reader, name)?;
    if guessed_ff == AccurateFileType::Unsupported {
        warn!("Not a valid media file {name:?}");
        return Err(anyhow!("File is not a valid media file"));
    }

    let mut exif_o = None;
    let mut track_o = None;
    match metadata_type(&guessed_ff) {
        MetadataType::ExifTags => {
            exif_o = exif::parse_exif_info(&mut *reader)?;
        }
        MetadataType::Track => {
            track_o = track::parse_track_info(&mut *reader)?;
        }
        MetadataType::NoMetadata => {}
    }
    let hash_info = hash_info.clone();

    let media_file_info = MediaFileInfo {
        original_file_this_run: name.clone(),
        original_path: vec![name.clone()],
        accurate_file_type: guessed_ff.clone(),
        quick_file_type: si.quick_file_type.clone(),
        exif_info: exif_o.clone(),
        track_info: track_o.clone(),
        hash_info,
        supp_info: supp_info.clone(),
        xmp_info: xmp_info.clone(),
        modified: si.modified_datetime,
        created: si.created_datetime,
        file_size: si.file_size,
    };
    Ok(media_file_info)
}

#[cfg(test)]
impl MediaFileInfo {
    pub(crate) fn new_for_test() -> Self {
        MediaFileInfo {
            original_file_this_run: "".to_string(),
            original_path: vec![],
            quick_file_type: QuickFileType::Media,
            exif_info: None,
            track_info: None,
            accurate_file_type: AccurateFileType::Jpg,
            hash_info: HashInfo {
                short_checksum: "tsc".to_string(),
                long_checksum: "tlc".to_string(),
            },
            supp_info: None,
            xmp_info: None,
            modified: None,
            created: None,
            file_size: 0,
        }
    }
}
