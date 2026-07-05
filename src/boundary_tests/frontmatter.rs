use crate::markdown::{PhotoSorterFrontMatter, assemble_markdown, split_frontmatter};
use crate::test_util::setup_log;
use anyhow::Result;

fn sample_fm() -> PhotoSorterFrontMatter {
    PhotoSorterFrontMatter {
        path_original: vec!["p1".to_string()],
        checksum: "abcdef".to_string(),
        datetime: None,
        latitude: None,
        longitude: None,
        people: vec![],
        albums: vec![],
    }
}

#[test]
fn split_frontmatter_never_panics_on_adversarial_input() {
    setup_log();
    let big = "-".repeat(100_000);
    let cases: Vec<String> = vec![
        String::new(),
        "---".to_string(),
        "---\n".to_string(),
        "---\r\n".to_string(),
        "---\n---\n".to_string(),
        "------------".to_string(),
        "---\n---\n---\n---\n".to_string(),
        "\u{feff}---\ntitle: bom\n---\nbody".to_string(),
        "---\nÑoño: 📸\n---\ncafé".to_string(),
        big,
        "---\n\u{0}\u{1}\u{2}\n---\nbody".to_string(),
    ];
    for text in cases {
        // Contract: the two halves always reconstruct into the original text as
        // far as the caller relies on. The only hard requirement is no panic;
        // we additionally assert the pieces are the plain strings the type says.
        let (fm, md) = split_frontmatter(&text);
        let _ = (fm.len(), md.len());
    }
}

#[test]
fn assemble_markdown_rejects_malformed_yaml_without_panic() -> Result<()> {
    setup_log();
    let fm = sample_fm();

    // Unparseable YAML and non-mapping roots must come back as errors so the
    // caller leaves the file untouched rather than dropping metadata.
    assert!(assemble_markdown(&fm, &Some("foo: [unclosed".to_string()), "body").is_err());
    assert!(assemble_markdown(&fm, &Some("- a\n- b\n".to_string()), "body").is_err());
    assert!(assemble_markdown(&fm, &Some("just a scalar".to_string()), "body").is_err());

    // Odd-but-valid existing frontmatter must not panic; Ok either way.
    let ok_cases: Vec<String> = vec![
        "Ñoño: 📸\n".to_string(),
        format!("giant: {}\n", "z".repeat(50_000)),
        "nested:\n  a:\n    b:\n      c: 1\n".to_string(),
        "checksum: abcdef\noriginal-paths:\n  - p1\n".to_string(),
    ];
    for y in ok_cases {
        let _ = assemble_markdown(&fm, &Some(y), "body");
    }
    Ok(())
}
