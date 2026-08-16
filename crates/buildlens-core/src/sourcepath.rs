//! Comparing source paths that came from different places.
//!
//! The same file is named differently depending on who is talking: git reports
//! repo-relative paths, build metrics record absolute ones, and logs may carry
//! either with Windows separators. Deciding whether two such strings mean the
//! same file is needed by several crates, so it lives here — one correct
//! implementation rather than a copy per crate that each drift.

/// Normalizes separators so a Windows-style path compares against the
/// forward-slash paths git reports.
pub fn normalize_separators(path: &str) -> String {
    path.replace('\\', "/")
}

/// True when two paths refer to the same file.
///
/// One is typically a suffix of the other (absolute vs repo-relative), so a
/// suffix match is what is wanted — but it must land on a path boundary.
/// A plain `ends_with` made `a.swift` match `xa.swift`, and `Thing.swift`
/// match `Vendor/Thing.swift`, silently attributing work to the wrong file.
pub fn same_file(left: &str, right: &str) -> bool {
    let left = normalize_separators(left);
    let right = normalize_separators(right);
    if left == right {
        return true;
    }
    is_path_suffix(&left, &right) || is_path_suffix(&right, &left)
}

/// True when `suffix` is a trailing sequence of whole path segments of `full`.
fn is_path_suffix(full: &str, suffix: &str) -> bool {
    if suffix.is_empty() || full.len() <= suffix.len() {
        return false;
    }
    full.ends_with(suffix) && full.as_bytes()[full.len() - suffix.len() - 1] == b'/'
}

/// True when any of `candidates` refers to the same file as `path`.
pub fn matches_any<S: AsRef<str>>(path: &str, candidates: &[S]) -> bool {
    candidates
        .iter()
        .any(|candidate| same_file(path, candidate.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absolute_path_matches_its_repo_relative_form() {
        assert!(same_file(
            "/Users/me/proj/Sources/App/Foo.swift",
            "Sources/App/Foo.swift"
        ));
    }

    #[test]
    fn identical_paths_match() {
        assert!(same_file("Sources/Foo.swift", "Sources/Foo.swift"));
    }

    /// The bug this exists to prevent, in both directions.
    #[test]
    fn a_partial_filename_does_not_match() {
        assert!(!same_file("/repo/Sources/xa.swift", "a.swift"));
        assert!(!same_file("a.swift", "/repo/Sources/xa.swift"));
        assert!(!same_file("/repo/Sources/EvilFoo.swift", "Foo.swift"));
    }

    #[test]
    fn the_same_name_in_different_directories_does_not_match() {
        assert!(!same_file(
            "/repo/Vendor/Thing.swift",
            "Sources/Thing.swift"
        ));
    }

    #[test]
    fn windows_separators_normalize() {
        assert!(same_file(
            r"C:\proj\Sources\App\Foo.swift",
            "Sources/App/Foo.swift"
        ));
    }

    #[test]
    fn an_empty_path_matches_nothing() {
        assert!(!same_file("", "Sources/Foo.swift"));
        assert!(!same_file("Sources/Foo.swift", ""));
    }

    #[test]
    fn matches_any_scans_candidates() {
        let candidates = ["Sources/A.swift".to_owned(), "Sources/B.swift".to_owned()];
        assert!(matches_any("/repo/Sources/B.swift", &candidates));
        assert!(!matches_any("/repo/Sources/C.swift", &candidates));
        assert!(!matches_any("/repo/Sources/B.swift", &[] as &[String]));
    }
}
