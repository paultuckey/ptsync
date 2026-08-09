//! Parsing for `s3://bucket/prefix` locations. Pure string logic — no AWS, no
//! I/O.

/// True when `s` is *intended* as an S3 location. The factories branch on this
/// before parsing, so a malformed `s3://…` is reported as a bad URI rather than
/// silently treated as a local path.
pub(crate) fn is_s3_uri(s: &str) -> bool {
    s.starts_with("s3://")
}

#[derive(Debug, PartialEq)]
pub(crate) struct S3Uri {
    pub(crate) bucket: String,
    /// No leading or trailing slash, so a relative path joins as
    /// `{prefix}/{rel}`. Empty means the bucket root.
    pub(crate) prefix: String,
}

impl S3Uri {
    /// `None` for a non-`s3://` string or an `s3://` URI with no bucket.
    pub(crate) fn parse(s: &str) -> Option<S3Uri> {
        let rest = s.strip_prefix("s3://")?;
        let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
        if bucket.is_empty() {
            return None;
        }
        Some(S3Uri {
            bucket: bucket.to_string(),
            prefix: prefix.trim_matches('/').to_string(),
        })
    }

    /// The inverse of [`S3Uri::relative_of`].
    pub(crate) fn key_for(&self, rel: &str) -> String {
        if self.prefix.is_empty() {
            rel.to_string()
        } else {
            format!("{}/{}", self.prefix, rel)
        }
    }

    /// How `walk` turns bucket keys into the `/`-separated relative paths the
    /// zip and directory scanners also produce. `None` when the key does not sit
    /// *under* the prefix — a near-miss sharing only leading characters, or the
    /// bare prefix marker itself.
    pub(crate) fn relative_of(&self, key: &str) -> Option<String> {
        if self.prefix.is_empty() {
            return Some(key.to_string());
        }
        key.strip_prefix(&self.prefix)
            .and_then(|rest| rest.strip_prefix('/'))
            .map(|rel| rel.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bucket_and_prefix() {
        assert_eq!(
            S3Uri::parse("s3://my-bucket/photos/2024"),
            Some(S3Uri {
                bucket: "my-bucket".to_string(),
                prefix: "photos/2024".to_string(),
            })
        );
    }

    #[test]
    fn bucket_root_has_empty_prefix() {
        for s in ["s3://my-bucket", "s3://my-bucket/"] {
            assert_eq!(
                S3Uri::parse(s),
                Some(S3Uri {
                    bucket: "my-bucket".to_string(),
                    prefix: String::new(),
                }),
                "input {s:?}"
            );
        }
    }

    #[test]
    fn surrounding_slashes_are_trimmed_from_prefix() {
        assert_eq!(
            S3Uri::parse("s3://b/a/c/").map(|u| u.prefix),
            Some("a/c".to_string())
        );
    }

    #[test]
    fn rejects_missing_bucket_and_non_s3() {
        assert_eq!(S3Uri::parse("s3://"), None);
        assert_eq!(S3Uri::parse("s3:///prefix-only"), None);
        assert_eq!(S3Uri::parse("/local/dir"), None);
        assert_eq!(S3Uri::parse("takeout.zip"), None);
    }

    #[test]
    fn key_for_and_relative_of_round_trip() {
        let u = S3Uri {
            bucket: "b".to_string(),
            prefix: "photos/2024".to_string(),
        };
        assert_eq!(u.key_for("05/22/x.jpg"), "photos/2024/05/22/x.jpg");
        assert_eq!(
            u.relative_of("photos/2024/05/22/x.jpg"),
            Some("05/22/x.jpg".to_string())
        );
        // Outside the prefix, the bare prefix marker, and a near-miss that does
        // not fall on a path boundary.
        assert_eq!(u.relative_of("other/x.jpg"), None);
        assert_eq!(u.relative_of("photos/2024"), None);
        assert_eq!(u.relative_of("photos/2024x/y.jpg"), None);
    }

    #[test]
    fn bucket_root_prefix_is_identity() {
        let u = S3Uri {
            bucket: "b".to_string(),
            prefix: String::new(),
        };
        assert_eq!(u.key_for("a/b.jpg"), "a/b.jpg");
        assert_eq!(u.relative_of("a/b.jpg"), Some("a/b.jpg".to_string()));
    }
}
