//! Explicit virtual-host file access rules shared by routing, static files,
//! and FastCGI's static and script resolution.

use std::path::Path;

#[derive(Debug, Default)]
pub struct AccessPolicy {
    paths: Vec<Vec<String>>,
    names: Vec<String>,
}

impl AccessPolicy {
    pub(crate) fn has_rules(&self) -> bool {
        !self.paths.is_empty() || !self.names.is_empty()
    }

    pub(crate) fn compile(paths: Vec<String>, names: Vec<String>) -> Result<Self, String> {
        let mut compiled_paths = Vec::with_capacity(paths.len());
        for path in paths {
            if !path.starts_with('/') || path == "/" || path.ends_with('/') {
                return Err(format!(
                    "deny_paths entry {path:?} must be an absolute non-root path without a trailing slash"
                ));
            }
            let mut segments = Vec::new();
            for segment in path[1..].split('/') {
                validate_segment(segment)?;
                segments.push(segment.to_ascii_lowercase());
            }
            if compiled_paths.contains(&segments) {
                return Err(format!("duplicate deny_paths entry {path:?}"));
            }
            compiled_paths.push(segments);
        }
        let mut compiled_names = Vec::with_capacity(names.len());
        for name in names {
            validate_segment(&name)?;
            let lower = name.to_ascii_lowercase();
            if compiled_names.contains(&lower) {
                return Err(format!("duplicate deny_names entry {name:?}"));
            }
            compiled_names.push(lower);
        }
        Ok(Self {
            paths: compiled_paths,
            names: compiled_names,
        })
    }

    /// Explicit rules apply to every handler on the host. A private URL must
    /// not reach a front controller or an overlapping, more-specific location.
    pub(crate) fn denies_uri(&self, segments: &[String]) -> bool {
        self.paths.iter().any(|prefix| {
            segments.len() >= prefix.len()
                && prefix
                    .iter()
                    .zip(segments)
                    .all(|(expected, actual)| segment_matches(expected, actual))
        }) || segments.iter().any(|segment| {
            self.names
                .iter()
                .any(|expected| segment_matches(expected, segment))
        })
    }

    /// Filesystem handlers also refuse dotfile components by default. The
    /// first `.well-known` component remains usable for public well-known
    /// resources, while a nested dotfile under it stays private.
    pub(crate) fn denies_filesystem_uri(&self, segments: &[String]) -> bool {
        self.denies_uri(segments) || has_private_dot_segment(segments)
    }

    /// The caller must first establish that `final_path` is within `root`.
    /// This check catches an allowed URL that resolves through a junction or
    /// symlink to a private object elsewhere within that same root.
    pub(crate) fn denies_final_path(&self, root: &Path, final_path: &Path) -> bool {
        let Ok(relative) = final_path.strip_prefix(root) else {
            return true;
        };
        let segments: Vec<String> = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect();
        self.denies_filesystem_uri(&segments)
    }
}

fn validate_segment(segment: &str) -> Result<(), String> {
    if segment.is_empty()
        || matches!(segment, "." | "..")
        || !segment.is_ascii()
        || segment.contains(['/', '\\', ':', '%', '?', '#', '\0'])
        || segment.ends_with(['.', ' '])
    {
        return Err(format!(
            "invalid deny rule segment {segment:?}; use plain ASCII path segments"
        ));
    }
    Ok(())
}

fn segment_matches(expected: &str, actual: &str) -> bool {
    #[cfg(windows)]
    let actual = actual.trim_end_matches(['.', ' ']);
    expected.eq_ignore_ascii_case(actual)
}

fn has_private_dot_segment(segments: &[String]) -> bool {
    segments.iter().enumerate().any(|(index, segment)| {
        segment.starts_with('.') && !(index == 0 && segment.eq_ignore_ascii_case(".well-known"))
    })
}

/// Source-bearing PHP extensions must never fall through to byte serving.
/// `.php` is executable by FastCGI; the other extensions are not executable
/// in its v1 mapping and are therefore unavailable to either file handler.
pub(crate) fn is_php_source(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            [
                "php", "php3", "php4", "php5", "php7", "php8", "phtml", "phar", "phps",
            ]
            .iter()
            .any(|blocked| extension.eq_ignore_ascii_case(blocked))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_paths_use_component_boundaries_and_names_match_anywhere() {
        let policy = AccessPolicy::compile(
            vec!["/data".into(), "/conf/private".into()],
            vec!["composer.json".into()],
        )
        .unwrap();
        for path in [
            vec!["DATA".into(), "pages".into()],
            vec!["conf".into(), "private".into()],
            vec!["assets".into(), "composer.json".into()],
        ] {
            assert!(policy.denies_uri(&path), "{path:?}");
        }
        assert!(!policy.denies_uri(&["database".into()]));
        assert!(!policy.denies_uri(&["conf".into(), "public".into()]));
    }

    #[test]
    fn hidden_files_are_private_but_well_known_is_public() {
        let policy = AccessPolicy::default();
        assert!(policy.denies_filesystem_uri(&[".env".into()]));
        assert!(policy.denies_filesystem_uri(&["assets".into(), ".htaccess".into()]));
        assert!(!policy.denies_filesystem_uri(&[".well-known".into(), "x".into()]));
        assert!(policy.denies_filesystem_uri(&[".well-known".into(), ".env".into()]));
    }
}
