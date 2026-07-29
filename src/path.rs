//! Splitting up the `/`-separated relative paths that
//! [`crate::fs::FileSystem::walk`] yields.
//!
//! There were three copies of this before, and two of them defined `split_dir`
//! and `split_ext` with *opposite* conventions - one keeping the separator and
//! the dot, the other dropping them. Same names, different answers, in files far
//! enough apart that nothing ever pointed it out.
//!
//! The versions here keep the separator and the dot, because that is the
//! convention that composes: `format!("{dir}{name}")` rebuilds the path even at
//! the scan root, where the directory is empty and inserting a `/` would produce
//! a wrong absolute-looking path. Callers that want a bare extension say so at
//! the point of use.

/// Split a relative path into its directory prefix - *with* its trailing `/`,
/// and empty at the root - and its file name.
pub(crate) fn split_dir(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..=i], &path[i + 1..]),
        None => ("", path),
    }
}

/// Split a file name into stem and extension, where the extension keeps its dot
/// and is empty when there is none. Leading dots belong to the stem, so a
/// `.hidden` file is all stem.
pub(crate) fn split_ext(file_name: &str) -> (&str, &str) {
    match file_name.rfind('.') {
        Some(i) if i > 0 => (&file_name[..i], &file_name[i..]),
        _ => (file_name, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_dir_composes_at_every_depth() {
        // The property the whole module exists for: the two halves always
        // concatenate back into the path they came from.
        for path in [
            "IMG_1.jpg",
            "a/IMG_1.jpg",
            "a/b/IMG_1.jpg",
            "Google Photos/2024/IMG_3716.HEIC",
            "",
        ] {
            let (dir, name) = split_dir(path);
            assert_eq!(format!("{dir}{name}"), path, "splitting {path:?}");
        }
        assert_eq!(split_dir("a/b/c.jpg"), ("a/b/", "c.jpg"));
        // At the root the prefix is empty, which is exactly why it carries its
        // own separator: a "/" inserted here would be wrong.
        assert_eq!(split_dir("c.jpg"), ("", "c.jpg"));
    }

    #[test]
    fn test_split_ext_composes_and_keeps_the_dot() {
        for name in ["IMG_1.jpg", "IMG_1", ".hidden", "a.b.c", ""] {
            let (stem, ext) = split_ext(name);
            assert_eq!(format!("{stem}{ext}"), name, "splitting {name:?}");
        }
        assert_eq!(split_ext("IMG_1.jpg"), ("IMG_1", ".jpg"));
        assert_eq!(split_ext("IMG_1"), ("IMG_1", ""));
        // A leading dot is a hidden file, not an extension.
        assert_eq!(split_ext(".hidden"), (".hidden", ""));
        // Only the last dot separates.
        assert_eq!(split_ext("IMG_1.HEIC.json"), ("IMG_1.HEIC", ".json"));
    }
}
