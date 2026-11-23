//! Drive paths: the canonical, NFC-normalized, relative names clients address files by.
//!
//! Drive paths are metadata only. Blobs live under server-generated object keys, so a
//! user path never touches a filesystem and cannot escape through `..` or symlinks.

use std::fmt;

use unicode_normalization::UnicodeNormalization;

/// Longest accepted path, in UTF-8 bytes after NFC normalization.
pub const MAX_PATH_BYTES: usize = 1024;

/// A validated drive path. The empty path is the drive root.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DrivePath(String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("path is required")]
    Empty,
    #[error("path contains a backslash or control character")]
    ForbiddenChar,
    #[error("path must be relative")]
    Absolute,
    #[error("path contains an empty, '.' or '..' segment")]
    BadSegment,
    #[error("path exceeds {MAX_PATH_BYTES} bytes")]
    TooLong,
}

impl DrivePath {
    pub const ROOT: DrivePath = DrivePath(String::new());

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// The containing folder; the root's parent is the root.
    pub fn parent(&self) -> DrivePath {
        DrivePath(parent_path(&self.0).to_owned())
    }

    /// The last segment (`docs/spec.pdf` -> `spec.pdf`).
    pub fn name(&self) -> &str {
        self.0.rsplit_once('/').map_or(&self.0, |(_, name)| name)
    }

    /// Every proper ancestor, outermost first: `a/b/c` -> [`a`, `a/b`].
    pub fn ancestors(&self) -> Vec<DrivePath> {
        self.0
            .match_indices('/')
            .map(|(i, _)| DrivePath(self.0[..i].to_owned()))
            .collect()
    }
}

impl fmt::Display for DrivePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Parse a client-supplied path into its canonical form.
///
/// NFC matters: macOS hands out NFD filenames while Android and browsers send NFC, and
/// both must address the same file.
pub fn parse_path(raw: &str) -> Result<DrivePath, PathError> {
    if raw.is_empty() {
        return Err(PathError::Empty);
    }
    let path: String = raw.nfc().collect();
    if path.chars().any(|c| c == '\\' || c.is_control()) {
        return Err(PathError::ForbiddenChar);
    }
    if path.starts_with('/') {
        return Err(PathError::Absolute);
    }
    if path
        .split('/')
        .any(|s| s.is_empty() || s == "." || s == "..")
    {
        return Err(PathError::BadSegment);
    }
    if path.len() > MAX_PATH_BYTES {
        return Err(PathError::TooLong);
    }
    Ok(DrivePath(path))
}

/// Like [`parse_path`], but a missing or empty path means the root folder.
pub fn parse_folder_path(raw: Option<&str>) -> Result<DrivePath, PathError> {
    match raw {
        None | Some("") => Ok(DrivePath::ROOT),
        Some(raw) => parse_path(raw),
    }
}

/// `docs/q4/spec.pdf` -> `docs/q4`; a top-level name's parent is `""`.
pub fn parent_path(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// Where a sync client saves its copy when the server copy changed underneath it.
///
/// `docs/spec.pdf` from host `mbp` at `20261122T190000Z` becomes
/// `docs/spec (conflict mbp-20261122T190000Z).pdf`. A leading dot is part of the stem,
/// so `.env` becomes `.env (conflict ...)`. Characters a path may not contain are
/// dropped from `host`, which keeps the result valid for [`parse_path`].
pub fn conflict_path(path: &DrivePath, host: &str, timestamp: &str) -> DrivePath {
    let tag: String = format!("{host}-{timestamp}")
        .chars()
        .filter(|c| *c != '/' && *c != '\\' && !c.is_control())
        .nfc()
        .collect();
    let name = path.name();
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    };
    let parent = parent_path(path.as_str());
    let budget = MAX_PATH_BYTES - parent.len() - usize::from(!parent.is_empty());
    let suffix = format!(" (conflict {tag}){ext}");
    let mut renamed = if suffix.len() <= budget {
        format!(
            "{}{suffix}",
            &stem[..stem.floor_char_boundary(budget - suffix.len())]
        )
    } else {
        let hash = crate::hash::sha256_hex(format!("{name}{suffix}"));
        let fallback = format!("{hash}.conflict");
        fallback[..budget.min(fallback.len())].to_owned()
    };
    if renamed == name {
        let first = renamed
            .chars()
            .next()
            .expect("conflict filename is nonempty");
        renamed.replace_range(..first.len_utf8(), if first == 'c' { "d" } else { "c" });
    }
    DrivePath(if parent.is_empty() {
        renamed
    } else {
        format!("{parent}/{renamed}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_and_keeps_nested_relative_paths() {
        assert_eq!(
            parse_path("docs/q4/spec.pdf").unwrap().as_str(),
            "docs/q4/spec.pdf"
        );
        assert_eq!(parse_path("🎉/café.txt").unwrap().as_str(), "🎉/café.txt");
    }

    #[test]
    fn nfd_and_nfc_spellings_address_the_same_path() {
        let nfd = "cafe\u{301}.txt";
        let nfc = "caf\u{e9}.txt";
        assert_ne!(nfd, nfc);
        assert_eq!(parse_path(nfd).unwrap(), parse_path(nfc).unwrap());
    }

    #[test]
    fn rejects_traversal_and_malformed_segments() {
        for raw in [
            "..",
            "../etc/passwd",
            "a/../b",
            "a/./b",
            ".",
            "a//b",
            "a/",
            "/abs",
            "//x",
        ] {
            assert!(parse_path(raw).is_err(), "{raw:?} should be rejected");
        }
        assert_eq!(parse_path("/abs"), Err(PathError::Absolute));
        assert_eq!(parse_path("a/../b"), Err(PathError::BadSegment));
    }

    #[test]
    fn rejects_backslashes_and_control_characters() {
        for raw in [
            "a\\b",
            "..\\x",
            "nul\0byte",
            "tab\there",
            "line\nbreak",
            "del\u{7f}",
            "c1\u{85}",
        ] {
            assert_eq!(parse_path(raw), Err(PathError::ForbiddenChar), "{raw:?}");
        }
    }

    #[test]
    fn rejects_empty_and_oversized_paths() {
        assert_eq!(parse_path(""), Err(PathError::Empty));
        assert!(parse_path(&"a".repeat(MAX_PATH_BYTES)).is_ok());
        assert_eq!(
            parse_path(&"a".repeat(MAX_PATH_BYTES + 1)),
            Err(PathError::TooLong)
        );
        // The limit counts UTF-8 bytes, not characters.
        assert_eq!(
            parse_path(&"é".repeat(MAX_PATH_BYTES / 2 + 1)),
            Err(PathError::TooLong)
        );
    }

    #[test]
    fn folder_paths_default_to_root() {
        assert!(parse_folder_path(None).unwrap().is_root());
        assert!(parse_folder_path(Some("")).unwrap().is_root());
        assert_eq!(parse_folder_path(Some("..")), Err(PathError::BadSegment));
    }

    #[test]
    fn parent_name_and_ancestors() {
        let p = parse_path("a/b/c.txt").unwrap();
        assert_eq!(p.parent().as_str(), "a/b");
        assert_eq!(p.name(), "c.txt");
        let ancestors: Vec<_> = p
            .ancestors()
            .iter()
            .map(|a| a.as_str().to_owned())
            .collect();
        assert_eq!(ancestors, ["a", "a/b"]);
        let top = parse_path("top").unwrap();
        assert!(top.parent().is_root());
        assert!(top.ancestors().is_empty());
        assert_eq!(parent_path(""), "");
    }

    #[test]
    fn conflict_path_inserts_tag_before_extension() {
        let p = |s| parse_path(s).unwrap();
        let ts = "20261122T190000Z";
        assert_eq!(
            conflict_path(&p("docs/spec.pdf"), "mbp", ts).as_str(),
            "docs/spec (conflict mbp-20261122T190000Z).pdf"
        );
        assert_eq!(
            conflict_path(&p("README"), "mbp", ts).as_str(),
            "README (conflict mbp-20261122T190000Z)"
        );
        assert_eq!(
            conflict_path(&p(".env"), "mbp", ts).as_str(),
            ".env (conflict mbp-20261122T190000Z)"
        );
        assert_eq!(
            conflict_path(&p("a.tar.gz"), "mbp", ts).as_str(),
            "a.tar (conflict mbp-20261122T190000Z).gz"
        );
    }

    #[test]
    fn conflict_path_stays_a_valid_path_for_hostile_hosts() {
        let out = conflict_path(&parse_path("x.txt").unwrap(), "../evil\\\0host/", "t");
        assert_eq!(parse_path(out.as_str()).unwrap(), out);
        assert_eq!(out.as_str(), "x (conflict ..evilhost-t).txt");
    }

    #[test]
    fn conflict_names_preserve_nfc_and_fit_the_path_limit() {
        let parent = "p".repeat(MAX_PATH_BYTES - 2);
        let path = parse_path(&format!("{parent}/q")).unwrap();
        let conflict = conflict_path(&path, "cafe\u{301}", "t");
        assert_eq!(conflict.parent(), path.parent());
        assert_ne!(conflict, path);
        assert_eq!(parse_path(conflict.as_str()).unwrap(), conflict);

        let path = parse_path(&format!("{}.txt", "é".repeat(510))).unwrap();
        let conflict = conflict_path(&path, "cafe\u{301}", "t");
        assert!(conflict.name().ends_with(" (conflict café-t).txt"));
        assert_eq!(parse_path(conflict.as_str()).unwrap(), conflict);
    }
}
