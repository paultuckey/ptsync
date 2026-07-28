use crate::fs::OsFileSystem;
use crate::supplemental_info::load_supplemental_info;
use crate::test_util::setup_log;
use anyhow::Result;
use std::io::Write;

#[test]
fn supplemental_json_never_panics_on_malformed() -> Result<()> {
    setup_log();
    let dir = tempfile::tempdir()?;
    let fs = OsFileSystem::new(&dir.path().to_string_lossy());

    let nested = deep_json();
    let cases: Vec<(&str, &str)> = vec![
        ("empty.json", ""),
        ("garbage.json", "\u{0}\u{1}\u{2}not json at all"),
        ("truncated.json", "{\"geoData\": {\"latitude\": 1.0"),
        ("array.json", "[1, 2, 3]"),
        ("scalar.json", "42"),
        ("null.json", "null"),
        // Right shape, wrong value types for every field.
        (
            "wrong_types.json",
            r#"{"geoData":"nope","people":"nobody","photoTakenTime":5}"#,
        ),
        // geoData present but lat/long are strings, not numbers.
        (
            "bad_geo.json",
            r#"{"geoData":{"latitude":"north","longitude":"west"}}"#,
        ),
        // Enormous but structurally valid: a huge person list.
        (
            "huge.json",
            "big", // replaced below
        ),
        // Deep nesting under an unexpected key.
        ("nested.json", &nested),
        // Unicode everywhere, plus a non-numeric timestamp.
        (
            "unicode.json",
            r#"{"people":[{"name":"Ñoño 📸"}],"photoTakenTime":{"timestamp":"not-a-number"}}"#,
        ),
        // Free text where a scalar string is expected, and vice versa.
        ("bad_title.json", r#"{"title":42,"description":["a","b"]}"#),
        // Blank title and description: both must come back as absent, not "".
        ("blank_text.json", r#"{"title":"  ","description":""}"#),
        // A title that is the media file's own name, in the other case.
        ("file_name_title.json", r#"{"title":"PHOTO.JPG"}"#),
    ];

    for (name, body) in cases {
        let path = dir.path().join(name);
        if name == "huge.json" {
            let mut f = std::fs::File::create(&path)?;
            f.write_all(br#"{"people":["#)?;
            for i in 0..5000 {
                if i > 0 {
                    f.write_all(b",")?;
                }
                f.write_all(br#"{"name":"p"}"#)?;
            }
            f.write_all(b"]}")?;
        } else {
            std::fs::write(&path, body)?;
        }
        // The only invariant is "does not panic". Malformed JSON returns None,
        // valid-but-odd JSON may return Some; either is acceptable here.
        let info = load_supplemental_info(name, "photo.jpg", &fs);
        // Whatever the input, no consumer should ever see a blank title or
        // description, or a title that is only the file's name back again.
        if let Some(info) = info {
            assert_ne!(info.title.as_deref().map(str::trim), Some(""));
            assert_ne!(info.description.as_deref().map(str::trim), Some(""));
            assert_eq!(
                info.title
                    .as_deref()
                    .filter(|t| t.eq_ignore_ascii_case("photo.jpg")),
                None,
                "{name}: a file-name title must be dropped"
            );
        }
    }
    Ok(())
}

/// JSON nested deeply enough to catch a naive recursive parser, but shaped so
/// serde skips it as an unknown field rather than accepting it.
fn deep_json() -> String {
    let mut s = String::from("{\"unknown\":");
    let depth = 500;
    for _ in 0..depth {
        s.push('[');
    }
    for _ in 0..depth {
        s.push(']');
    }
    s.push('}');
    s
}
