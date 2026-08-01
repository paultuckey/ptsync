//! Reading XMP sidecars — the metadata format Lightroom, darktable, digiKam,
//! Capture One and ExifTool all speak.
//!
//! ptsync *reads* XMP and never writes it. The Markdown note is the archive's
//! master copy; XMP is an inbound source, on the same footing as Google's
//! supplemental JSON and iCloud's CSV. Someone arriving from a darktable or
//! Lightroom library carries years of ratings, keywords and named faces in
//! these files, and without this module all of it would be dropped on the floor.
//!
//! # What is read
//!
//! | Frontmatter key | XMP property |
//! |---|---|
//! | `datetime`  | `photoshop:DateCreated`, `xmp:CreateDate`, `exif:DateTimeOriginal` |
//! | `latitude` / `longitude` | `exif:GPSLatitude` / `exif:GPSLongitude` |
//! | `rating`    | `xmp:Rating` |
//! | `label`     | `xmp:Label` |
//! | `title`     | `dc:title` (also heads the note body, first write only) |
//! | `tags`      | `dc:subject`, `lr:hierarchicalSubject`, `digiKam:TagsList` |
//! | `people`    | `Iptc4xmpExt:PersonInImage`, MWG face-region names |
//! | note body   | `dc:description` (first write only) |
//!
//! Title and description are pooled with Google's supplemental json by
//! [`crate::metadata::reconcile::best_guess_title`] and [`crate::metadata::reconcile::best_guess_description`],
//! which rank XMP first.
//!
//! # Why namespace URIs, not prefixes
//!
//! RDF/XML lets a writer pick any prefix it likes for a namespace. digiKam
//! writes `Iptc4xmpExt:PersonInImage`, other tools write `iptcExt:PersonInImage`,
//! and both mean the same property. Matching on the prefix would silently miss
//! half the ecosystem, so every lookup here is by namespace URI.
//!
//! # Property shapes
//!
//! A property can appear either as an attribute on `rdf:Description` or as a
//! child element, and RDF/XML treats the two as identical. Array-valued
//! properties wrap their items in `rdf:Bag` (unordered), `rdf:Seq` (ordered) or
//! `rdf:Alt` (language alternatives). [`prop_values`] flattens all of it.

use crate::fs::FileSystem;
use crate::metadata::exif::parse_exif_datetime;
use crate::util::non_zero_coords;
use serde::{Deserialize, Serialize};
use std::io::Read;
use tracing::{debug, warn};

const NS_RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const NS_DC: &str = "http://purl.org/dc/elements/1.1/";
const NS_XMP: &str = "http://ns.adobe.com/xap/1.0/";
const NS_PHOTOSHOP: &str = "http://ns.adobe.com/photoshop/1.0/";
const NS_EXIF: &str = "http://ns.adobe.com/exif/1.0/";
const NS_LR: &str = "http://ns.adobe.com/lightroom/1.0/";
const NS_IPTC_EXT: &str = "http://iptc.org/std/Iptc4xmpExt/2008-02-29/";
const NS_MWG_RS: &str = "http://www.metadataworkinggroup.com/schemas/regions/";
const NS_DIGIKAM: &str = "http://www.digikam.org/ns/1.0/";

/// Refuse to buffer a "sidecar" larger than this. Real ones are a few KB; a
/// multi-megabyte file under an `.xmp` name is a mistake or a hostile input, and
/// either way is not worth an unbounded allocation per media file.
const MAX_XMP_BYTES: u64 = 4 * 1024 * 1024;

/// Metadata lifted out of an XMP sidecar. Every field is optional because XMP
/// writers emit wildly different subsets — darktable writes ratings and little
/// else, digiKam writes faces and hierarchical tags.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(rename_all(serialize = "camelCase"))]
pub(crate) struct PsXmpInfo {
    /// Capture time, spelled as [`crate::metadata::taken::TakenAt`] writes one
    /// and read back with `TakenAt::parse` by
    /// [`crate::metadata::reconcile::best_guess_taken`].
    ///
    /// That means an offset appears here only when the sidecar carried one. A
    /// `photoshop:DateCreated` of `2024-07-15T14:30:22.417` is a wall clock and
    /// is kept as one, rather than being normalised to `+00:00` and afterwards
    /// mistaken for a photographer who really was in UTC.
    pub(crate) datetime: Option<String>,
    pub(crate) latitude: Option<f64>,
    pub(crate) longitude: Option<f64>,
    /// `xmp:Rating`: 0–5, or -1 for "rejected", which is kept as -1 rather than
    /// flattened to 0 — a rejected photo is not an unrated one.
    pub(crate) rating: Option<i64>,
    /// `xmp:Label`. A colour name in most tools, but it is a free-text field.
    pub(crate) label: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) description: Option<String>,
    /// Keywords, with hierarchy re-spelled for Obsidian (see [`normalise_tag`]).
    pub(crate) tags: Vec<String>,
    /// Named people, from `PersonInImage` and from MWG face regions.
    pub(crate) people: Vec<String>,
}

impl PsXmpInfo {
    /// Whether anything at all was found. A sidecar that parses but yields no
    /// recognised property is not worth carrying around.
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Find the XMP sidecar describing `path`, if one exists.
///
/// Two conventions are in the wild and both are tried, preferred form first:
///
/// 1. **Swapped** — `IMG_1234.xmp`. Adobe's convention, inherited from raw
///    workflows where one negative had one sidecar. Ambiguous when
///    `IMG_1234.JPG` and `IMG_1234.HEIC` share a directory, since both claim the
///    same sidecar, but that is rare next to how common the form is.
/// 2. **Appended** — `IMG_1234.JPG.xmp`. darktable, digiKam and ExifTool's usual
///    recipe. Unambiguous: the media extension is part of the name.
///
/// Each is tried in lower- and upper-case, since archives come off
/// case-preserving and case-sensitive filesystems alike.
pub(crate) fn detect_xmp(path: &str, container: &dyn FileSystem) -> Option<String> {
    for candidate in xmp_candidates(path) {
        if container.exists(&candidate) {
            debug!("Found xmp sidecar {candidate} for {path}");
            return Some(candidate);
        }
    }
    None
}

/// Sidecar names to try for `path`, preferred form first. Split out from
/// [`detect_xmp`] so the ordering can be asserted without a filesystem.
fn xmp_candidates(path: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    // The swapped form only differs when there is an extension to swap; a
    // dot-less name would just re-derive the appended candidates below.
    let last_slash = path.rfind('/').map_or(0, |i| i + 1);
    if let Some(dot) = path[last_slash..].rfind('.') {
        let stem = &path[..last_slash + dot];
        candidates.push(format!("{stem}.xmp"));
        candidates.push(format!("{stem}.XMP"));
    }
    candidates.push(format!("{path}.xmp"));
    candidates.push(format!("{path}.XMP"));
    candidates
}

/// Read and parse the sidecar at `path`. Returns `None` when the file cannot be
/// read, is not parseable XML, or carries nothing recognised — a broken sidecar
/// downgrades the metadata available for a photo, it never fails the sync.
pub(crate) fn load_xmp(path: &str, container: &dyn FileSystem) -> Option<PsXmpInfo> {
    if let Ok(meta) = container.metadata(path)
        && meta.len > MAX_XMP_BYTES
    {
        warn!(
            "Ignoring xmp sidecar {path}: {} bytes exceeds the {MAX_XMP_BYTES} byte cap",
            meta.len
        );
        return None;
    }
    let mut reader = container.open(path).ok()?;
    let mut bytes = Vec::new();
    if let Err(e) = reader.read_to_end(&mut bytes) {
        warn!("Could not read xmp sidecar {path}: {e}");
        return None;
    }
    let text = String::from_utf8_lossy(&bytes);
    match parse_xmp(&text) {
        Some(info) => Some(info),
        None => {
            debug!("No usable metadata in xmp sidecar {path}");
            None
        }
    }
}

/// Parse XMP/RDF text into [`PsXmpInfo`]. Public to the crate so tests and
/// `info` can exercise it without touching a filesystem.
pub(crate) fn parse_xmp(text: &str) -> Option<PsXmpInfo> {
    // A UTF-8 BOM survives `from_utf8_lossy` as U+FEFF and is not valid content
    // before the XML declaration, so roxmltree would reject the whole document.
    let text = text.trim_start_matches('\u{feff}').trim_start();
    let doc = match roxmltree::Document::parse(text) {
        Ok(doc) => doc,
        Err(e) => {
            debug!("Could not parse xmp as xml: {e}");
            return None;
        }
    };

    let mut info = PsXmpInfo::default();
    // Writers may split properties across several `rdf:Description` elements
    // (one per namespace is a common style), so every one contributes.
    for desc in doc.descendants().filter(|n| {
        n.tag_name().namespace() == Some(NS_RDF) && n.tag_name().name() == "Description"
    }) {
        collect_from_description(&desc, &mut info);
    }

    dedup_preserving_order(&mut info.tags);
    dedup_preserving_order(&mut info.people);

    // "No fix" is written as zeros rather than as an absent property, and a lone
    // latitude locates nothing, so neither survives parsing. Dropping them here
    // means no consumer has to know the convention - a `latitude` on
    // `PsXmpInfo` is always a real one. Same rule as
    // [`crate::metadata::exif::parse_exif_info`] and
    // [`crate::metadata::supplemental::parse_supplemental_info`].
    (info.latitude, info.longitude) = non_zero_coords(info.latitude, info.longitude).unzip();

    if info.is_empty() { None } else { Some(info) }
}

/// Fold one `rdf:Description` into `info`. Earlier values win: a document that
/// states a property twice is malformed, and picking the first keeps parsing
/// deterministic rather than order-of-appearance dependent.
fn collect_from_description(desc: &roxmltree::Node, info: &mut PsXmpInfo) {
    // Dates, most trustworthy first. `photoshop:DateCreated` is what a user's
    // correction in Lightroom lands in; `xmp:CreateDate` is usually a copy of
    // EXIF; `exif:DateTimeOriginal` is the camera's own reading.
    if info.datetime.is_none() {
        info.datetime = [
            (NS_PHOTOSHOP, "DateCreated"),
            (NS_XMP, "CreateDate"),
            (NS_EXIF, "DateTimeOriginal"),
        ]
        .into_iter()
        .find_map(|(ns, name)| {
            prop_value(desc, ns, name)
                .and_then(|v| parse_exif_datetime(&v))
                .map(|t| t.to_string())
        });
    }

    if info.latitude.is_none() {
        info.latitude = prop_value(desc, NS_EXIF, "GPSLatitude").and_then(|v| parse_gps(&v));
    }
    if info.longitude.is_none() {
        info.longitude = prop_value(desc, NS_EXIF, "GPSLongitude").and_then(|v| parse_gps(&v));
    }

    if info.rating.is_none() {
        info.rating = prop_value(desc, NS_XMP, "Rating")
            .and_then(|v| v.trim().parse::<f64>().ok())
            // Some writers spell the rating `4.0`; truncating is right for the
            // half-star ratings a few tools emit, which XMP has no room for.
            .map(|v| v.trunc() as i64)
            .filter(|v| (-1..=5).contains(v));
    }
    if info.label.is_none() {
        info.label = prop_value(desc, NS_XMP, "Label").filter(|s| !s.is_empty());
    }
    if info.title.is_none() {
        info.title = prop_value(desc, NS_DC, "title").filter(|s| !s.is_empty());
    }
    if info.description.is_none() {
        info.description = prop_value(desc, NS_DC, "description").filter(|s| !s.is_empty());
    }

    for (ns, name) in [
        (NS_DC, "subject"),
        (NS_LR, "hierarchicalSubject"),
        (NS_DIGIKAM, "TagsList"),
    ] {
        for raw in prop_values(desc, ns, name) {
            if let Some(tag) = normalise_tag(&raw) {
                info.tags.push(tag);
            }
        }
    }

    for raw in prop_values(desc, NS_IPTC_EXT, "PersonInImage") {
        let name = raw.trim();
        if !name.is_empty() {
            info.people.push(name.to_string());
        }
    }
    info.people.extend(face_region_names(desc));
}

/// Names attached to MWG face regions:
/// `mwg-rs:Regions` → `mwg-rs:RegionList` → `rdf:Bag` → `rdf:li` → `mwg-rs:Name`.
///
/// This is where digiKam, Picasa and Lightroom put the faces a user has actually
/// named, so for many libraries it is the *only* source of people. Regions that
/// declare a `Type` other than `Face` (tools also box pets and focus points) are
/// skipped; a region with no `Type` is taken as a face, which is what writers
/// that omit it mean.
fn face_region_names(desc: &roxmltree::Node) -> Vec<String> {
    let mut names = Vec::new();
    for regions in descendants_named(desc, NS_MWG_RS, "Regions") {
        for li in descendants_named(&regions, NS_RDF, "li") {
            let kind = prop_value(&li, NS_MWG_RS, "Type");
            if let Some(kind) = &kind
                && !kind.eq_ignore_ascii_case("face")
            {
                continue;
            }
            if let Some(name) = prop_value(&li, NS_MWG_RS, "Name") {
                let name = name.trim();
                if !name.is_empty() {
                    names.push(name.to_string());
                }
            }
        }
    }
    names
}

/// Every descendant of `node` (itself included) with the given namespace and
/// local name.
fn descendants_named<'a, 'input>(
    node: &roxmltree::Node<'a, 'input>,
    ns: &str,
    name: &str,
) -> Vec<roxmltree::Node<'a, 'input>> {
    node.descendants()
        .filter(|n| n.tag_name().namespace() == Some(ns) && n.tag_name().name() == name)
        .collect()
}

/// The first value of a property, for the ones that hold a single item.
fn prop_value(node: &roxmltree::Node, ns: &str, name: &str) -> Option<String> {
    prop_values(node, ns, name).into_iter().next()
}

/// Every value of a property, whatever shape it takes.
///
/// RDF/XML offers three spellings and tools use all of them:
///   - an attribute on the description (`xmp:Rating="4"`);
///   - a child element holding text (`<dc:title>Sunset</dc:title>`);
///   - a child element wrapping `rdf:Bag`/`Seq`/`Alt` of `rdf:li` items.
///
/// Only *direct* children are considered, so a `dc:subject` on a nested face
/// region cannot be mistaken for one on the photo.
fn prop_values(node: &roxmltree::Node, ns: &str, name: &str) -> Vec<String> {
    if let Some(attr) = node.attribute((ns, name)) {
        let attr = attr.trim();
        if !attr.is_empty() {
            return vec![attr.to_string()];
        }
    }
    let Some(child) = node
        .children()
        .find(|n| n.tag_name().namespace() == Some(ns) && n.tag_name().name() == name)
    else {
        return Vec::new();
    };

    let items: Vec<String> = child
        .children()
        .filter(|n| {
            n.tag_name().namespace() == Some(NS_RDF)
                && matches!(n.tag_name().name(), "Bag" | "Seq" | "Alt")
        })
        .flat_map(|container| container.children().collect::<Vec<_>>())
        .filter(|n| n.tag_name().namespace() == Some(NS_RDF) && n.tag_name().name() == "li")
        .filter_map(|li| li.text().map(str::trim).filter(|t| !t.is_empty()))
        .map(str::to_string)
        .collect();
    if !items.is_empty() {
        return items;
    }

    match child.text().map(str::trim).filter(|t| !t.is_empty()) {
        Some(text) => vec![text.to_string()],
        None => Vec::new(),
    }
}

/// Re-spell a keyword for Obsidian, or drop it.
///
/// Lightroom and digiKam separate hierarchy levels with `|`
/// (`Places|Europe|Paris`); Obsidian's nested tags use `/` (`Places/Europe/Paris`),
/// so the separator is swapped and the hierarchy survives the trip. Spaces
/// inside a level become `-`, since a space would end the tag in Obsidian's
/// inline `#tag` syntax and split one tag into two.
fn normalise_tag(raw: &str) -> Option<String> {
    let joined = raw
        .split('|')
        .map(|level| level.trim().replace(char::is_whitespace, "-"))
        .filter(|level| !level.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// Parse an XMP GPS coordinate into signed decimal degrees.
///
/// XMP's own spelling is degrees, decimal minutes and a hemisphere letter
/// (`51,30.5N`); the degrees/minutes/seconds form (`51,30,30N`) also appears,
/// and some writers ignore the spec and emit plain signed decimal (`51.508333`).
/// All three are accepted.
fn parse_gps(raw: &str) -> Option<f64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let (body, sign) = match raw.chars().last() {
        Some(c) if c.eq_ignore_ascii_case(&'N') || c.eq_ignore_ascii_case(&'E') => {
            (&raw[..raw.len() - 1], 1.0)
        }
        Some(c) if c.eq_ignore_ascii_case(&'S') || c.eq_ignore_ascii_case(&'W') => {
            (&raw[..raw.len() - 1], -1.0)
        }
        // No hemisphere letter: the value has to carry its own sign.
        _ => (raw, 1.0),
    };

    let mut parts = body.split(',');
    let degrees: f64 = parts.next()?.trim().parse().ok()?;
    let minutes: f64 = match parts.next() {
        Some(m) => m.trim().parse().ok()?,
        None => 0.0,
    };
    let seconds: f64 = match parts.next() {
        Some(s) => s.trim().parse().ok()?,
        None => 0.0,
    };
    if parts.next().is_some() {
        return None;
    }
    // A negative degrees component already encodes the hemisphere, so applying
    // `sign` on top of it would cancel a `W` back to east.
    let magnitude = degrees.abs() + minutes / 60.0 + seconds / 3600.0;
    let signed = if degrees.is_sign_negative() {
        -magnitude
    } else {
        sign * magnitude
    };
    if signed.is_finite() && signed.abs() <= 180.0 {
        Some(signed)
    } else {
        None
    }
}

/// Drop repeats while keeping first-seen order, so output stays deterministic
/// across runs (a `HashSet` would not be).
fn dedup_preserving_order(items: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    items.retain(|item| seen.insert(item.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::OsFileSystem;

    /// A sidecar in the shape digiKam writes: properties as child elements,
    /// hierarchical tags, named faces as MWG regions.
    const DIGIKAM_STYLE: &str = r#"<?xpacket begin="﻿" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:dc="http://purl.org/dc/elements/1.1/"
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:photoshop="http://ns.adobe.com/photoshop/1.0/"
    xmlns:exif="http://ns.adobe.com/exif/1.0/"
    xmlns:lr="http://ns.adobe.com/lightroom/1.0/"
    xmlns:Iptc4xmpExt="http://iptc.org/std/Iptc4xmpExt/2008-02-29/"
    xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/">
   <xmp:Rating>4</xmp:Rating>
   <xmp:Label>Red</xmp:Label>
   <photoshop:DateCreated>2024-07-15T14:30:22.417</photoshop:DateCreated>
   <exif:GPSLatitude>51,30.5N</exif:GPSLatitude>
   <exif:GPSLongitude>0,7.4W</exif:GPSLongitude>
   <dc:title><rdf:Alt><rdf:li xml:lang="x-default">Sunset</rdf:li></rdf:Alt></dc:title>
   <dc:description><rdf:Alt><rdf:li xml:lang="x-default">Low tide.</rdf:li></rdf:Alt></dc:description>
   <dc:subject><rdf:Bag><rdf:li>beach</rdf:li><rdf:li>holiday</rdf:li></rdf:Bag></dc:subject>
   <lr:hierarchicalSubject><rdf:Bag><rdf:li>Places|United Kingdom|Brighton</rdf:li></rdf:Bag></lr:hierarchicalSubject>
   <Iptc4xmpExt:PersonInImage><rdf:Bag><rdf:li>Paul</rdf:li></rdf:Bag></Iptc4xmpExt:PersonInImage>
   <mwg-rs:Regions rdf:parseType="Resource">
    <mwg-rs:RegionList>
     <rdf:Bag>
      <rdf:li rdf:parseType="Resource">
       <mwg-rs:Name>Ada</mwg-rs:Name>
       <mwg-rs:Type>Face</mwg-rs:Type>
      </rdf:li>
      <rdf:li rdf:parseType="Resource">
       <mwg-rs:Name>Rex</mwg-rs:Name>
       <mwg-rs:Type>Pet</mwg-rs:Type>
      </rdf:li>
     </rdf:Bag>
    </mwg-rs:RegionList>
   </mwg-rs:Regions>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
<?xpacket end="w"?>"#;

    /// The same facts in the other legal spelling — properties as attributes,
    /// and a different prefix for the IPTC namespace — which must parse alike.
    const ATTRIBUTE_STYLE: &str = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:photoshop="http://ns.adobe.com/photoshop/1.0/"
    xmlns:iptcExt="http://iptc.org/std/Iptc4xmpExt/2008-02-29/"
    xmp:Rating="4"
    xmp:Label="Red"
    photoshop:DateCreated="2024-07-15T14:30:22.417">
   <iptcExt:PersonInImage><rdf:Bag><rdf:li>Paul</rdf:li></rdf:Bag></iptcExt:PersonInImage>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

    #[test]
    fn test_parse_digikam_style() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let info = parse_xmp(DIGIKAM_STYLE).ok_or_else(|| anyhow::anyhow!("no xmp parsed"))?;
        assert_eq!(info.rating, Some(4));
        assert_eq!(info.label.as_deref(), Some("Red"));
        assert_eq!(info.title.as_deref(), Some("Sunset"));
        assert_eq!(info.description.as_deref(), Some("Low tide."));
        // The sidecar's `photoshop:DateCreated` carries no offset, so neither
        // does this: it is the wall clock someone saw, and saying `+00:00` would
        // be claiming they were in London.
        assert_eq!(info.datetime.as_deref(), Some("2024-07-15T14:30:22.417"));
        // 51 + 30.5/60
        assert_coord(info.latitude, 51.508_333_333);
        // West is negative.
        assert_coord(info.longitude, -0.123_333_333);
        assert_eq!(
            info.tags,
            vec!["beach", "holiday", "Places/United-Kingdom/Brighton"]
        );
        // `PersonInImage` and the face region both contribute; the pet does not.
        assert_eq!(info.people, vec!["Paul", "Ada"]);
        Ok(())
    }

    #[test]
    fn test_zero_coords_do_not_survive_parsing() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        // A writer that fills in GPS whether or not it has a fix. Zeros are that
        // absence, not a position in the Gulf of Guinea, so neither half is kept
        // and nothing downstream has to test for the sentinel.
        let xmp = r#"<?xml version="1.0"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description xmlns:exif="http://ns.adobe.com/exif/1.0/"
                   xmlns:xmp="http://ns.adobe.com/xap/1.0/"
                   exif:GPSLatitude="0" exif:GPSLongitude="0" xmp:Rating="3"/>
 </rdf:RDF>
</x:xmpmeta>"#;
        let info = parse_xmp(xmp).ok_or_else(|| anyhow::anyhow!("no xmp parsed"))?;
        assert_eq!(info.latitude, None);
        assert_eq!(info.longitude, None);
        // The rest of the sidecar is untouched by the coordinate rule.
        assert_eq!(info.rating, Some(3));
        Ok(())
    }

    #[test]
    fn test_lone_coordinate_does_not_survive_parsing() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        // Half a pair locates nothing either, so it goes the same way as zeros.
        let xmp = r#"<?xml version="1.0"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description xmlns:exif="http://ns.adobe.com/exif/1.0/"
                   xmlns:xmp="http://ns.adobe.com/xap/1.0/"
                   exif:GPSLatitude="51,30.5N" xmp:Rating="3"/>
 </rdf:RDF>
</x:xmpmeta>"#;
        let info = parse_xmp(xmp).ok_or_else(|| anyhow::anyhow!("no xmp parsed"))?;
        assert_eq!(info.latitude, None);
        assert_eq!(info.longitude, None);
        assert_eq!(info.rating, Some(3));
        Ok(())
    }

    #[test]
    fn test_attribute_style_matches_element_style() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let info = parse_xmp(ATTRIBUTE_STYLE).ok_or_else(|| anyhow::anyhow!("no xmp parsed"))?;
        // Same facts, written as attributes and under a different prefix for the
        // IPTC namespace - resolving by URI makes the two indistinguishable.
        assert_eq!(info.rating, Some(4));
        assert_eq!(info.label.as_deref(), Some("Red"));
        assert_eq!(info.datetime.as_deref(), Some("2024-07-15T14:30:22.417"));
        assert_eq!(info.people, vec!["Paul"]);
        Ok(())
    }

    #[test]
    fn test_parse_rejects_junk() {
        crate::test_util::setup_log();
        // Not XML at all.
        assert_eq!(parse_xmp("this is not xml"), None);
        // Valid XML carrying no XMP property worth keeping.
        assert_eq!(parse_xmp("<hello><world/></hello>"), None);
        // Well-formed XMP with an empty description yields nothing.
        assert_eq!(
            parse_xmp(
                r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
                     <rdf:Description rdf:about=""/>
                   </rdf:RDF>"#
            ),
            None
        );
    }

    /// Coordinates are compared to well under a metre rather than bit-exactly,
    /// since the arithmetic route differs between the forms being parsed.
    #[track_caller]
    fn assert_coord(actual: Option<f64>, expected: f64) {
        match actual {
            Some(v) => assert!((v - expected).abs() < 1e-9, "expected ~{expected}, got {v}"),
            None => panic!("expected ~{expected}, got None"),
        }
    }

    #[test]
    fn test_gps_forms() {
        // Degrees + decimal minutes, the spec's own form.
        assert_coord(parse_gps("51,30.5N"), 51.508_333_333);
        // Degrees, minutes, seconds.
        assert_coord(parse_gps("51,30,30N"), 51.508_333_333);
        // Plain signed decimal, as some writers emit.
        assert_coord(parse_gps("-33.8688"), -33.8688);
        // Hemisphere letter drives the sign.
        assert_coord(parse_gps("33,52.1S"), -33.868_333_333);
        // A negative degrees component is not cancelled by a hemisphere letter.
        assert_coord(parse_gps("-33,52.1S"), -33.868_333_333);
        assert_eq!(parse_gps(""), None);
        assert_eq!(parse_gps("not a coordinate"), None);
        // Out of range for a latitude/longitude.
        assert_eq!(parse_gps("999,0,0N"), None);
        // Too many components.
        assert_eq!(parse_gps("1,2,3,4N"), None);
    }

    #[test]
    fn test_normalise_tag() {
        // Hierarchy separator becomes Obsidian's.
        assert_eq!(
            normalise_tag("Places|United Kingdom|Brighton"),
            Some("Places/United-Kingdom/Brighton".to_string())
        );
        // A space would end an inline `#tag`, so it is hyphenated.
        assert_eq!(
            normalise_tag("summer holiday"),
            Some("summer-holiday".to_string())
        );
        // Empty levels are dropped rather than producing `//`.
        assert_eq!(normalise_tag("a||b"), Some("a/b".to_string()));
        assert_eq!(normalise_tag("   "), None);
        assert_eq!(normalise_tag(""), None);
    }

    #[test]
    fn test_rating_bounds() -> anyhow::Result<()> {
        let with_rating = |v: &str| {
            parse_xmp(&format!(
                r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
                     <rdf:Description rdf:about=""
                       xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="{v}"/>
                   </rdf:RDF>"#
            ))
            .and_then(|i| i.rating)
        };
        assert_eq!(with_rating("0"), Some(0));
        assert_eq!(with_rating("5"), Some(5));
        // "Rejected" is a real state, distinct from unrated.
        assert_eq!(with_rating("-1"), Some(-1));
        // Half stars truncate; XMP has no room for them.
        assert_eq!(with_rating("3.5"), Some(3));
        // Out of range and non-numeric are dropped.
        assert_eq!(with_rating("6"), None);
        assert_eq!(with_rating("banana"), None);
        Ok(())
    }

    #[test]
    fn test_candidate_order_prefers_swapped_form() {
        // Adobe's swapped form leads; darktable's appended form follows.
        let candidates = xmp_candidates("a/IMG_1234.JPG");
        assert_eq!(candidates[0], "a/IMG_1234.xmp");
        assert_eq!(candidates[1], "a/IMG_1234.XMP");
        assert_eq!(candidates[2], "a/IMG_1234.JPG.xmp");
        assert_eq!(candidates[3], "a/IMG_1234.JPG.XMP");
        // No extension to swap: only the appended form is meaningful.
        assert_eq!(xmp_candidates("plain"), vec!["plain.xmp", "plain.XMP"]);
    }

    #[test]
    fn test_detect_and_load_from_disk() -> anyhow::Result<()> {
        crate::test_util::setup_log();
        let c = OsFileSystem::new("test");
        let path =
            detect_xmp("Canon_40D.jpg", &c).ok_or_else(|| anyhow::anyhow!("no sidecar found"))?;
        assert_eq!(path, "Canon_40D.jpg.xmp");
        let info = load_xmp(&path, &c).ok_or_else(|| anyhow::anyhow!("sidecar did not parse"))?;
        assert_eq!(info.rating, Some(5));
        assert_eq!(info.people, vec!["Ada Lovelace"]);
        // `dc:subject` is read before `lr:hierarchicalSubject`, so the flat
        // keyword leads and the hierarchical one follows.
        assert_eq!(info.tags, vec!["test-fixture", "cameras/canon"]);
        Ok(())
    }

    #[test]
    fn test_detect_returns_none_without_sidecar() {
        crate::test_util::setup_log();
        let c = OsFileSystem::new("test");
        assert_eq!(detect_xmp("Hello.mpg", &c), None);
    }
}
