//! Pure path reasoning: matching a diagnostic's file against the commit
//! range's changed files, and pulling a location out of a failure message.
//!
//! Nothing here shells out, so all of it is testable without a repository.

use std::path::Path;

/// Normalizes separators so a Windows-style path compares against the
/// forward-slash paths git reports.
pub fn normalize_separators(path: &str) -> String {
    path.replace('\\', "/")
}

/// True when a diagnostic's file refers to the same file as a repo-relative
/// path from `git diff --name-only`.
///
/// A diagnostic carries an absolute path (`/Users/x/proj/Sources/App/Foo.swift`)
/// while git reports repo-relative (`Sources/App/Foo.swift`), so one is a
/// suffix of the other. The match must fall on a path boundary: comparing raw
/// suffixes made `Sources/a.swift` match a diagnostic in `.../xa.swift`,
/// because `"xa.swift".ends_with("a.swift")` is true. That silently attributed
/// a diagnostic to an unrelated file.
pub fn same_file(diagnostic_path: &str, changed_path: &str) -> bool {
    let diagnostic = normalize_separators(diagnostic_path);
    let changed = normalize_separators(changed_path);
    if diagnostic == changed {
        return true;
    }
    is_path_suffix(&diagnostic, &changed) || is_path_suffix(&changed, &diagnostic)
}

/// True when `suffix` is a trailing *path segment* sequence of `full`, i.e.
/// the character before the match is a separator.
fn is_path_suffix(full: &str, suffix: &str) -> bool {
    if suffix.is_empty() || full.len() <= suffix.len() {
        return false;
    }
    full.ends_with(suffix) && full.as_bytes()[full.len() - suffix.len() - 1] == b'/'
}

/// True when any changed file refers to the same file as this diagnostic.
pub fn matches_any(diagnostic_path: &str, changed: &[String]) -> bool {
    changed.iter().any(|path| same_file(diagnostic_path, path))
}

/// Pulls the first `File.swift:line` location out of a test failure message.
///
/// Returns the line only when it parses as a positive integer. Git blame's
/// `-L` is 1-based, so a `0` would make the blame call fail; treating it as
/// "no line" and blaming the whole file is the honest answer.
pub fn location_from_message(message: &str) -> Option<(String, Option<u32>)> {
    const MARKER: &str = ".swift:";
    let (index, _) = message.match_indices(MARKER).next()?;
    let start = message[..index]
        .rfind(char::is_whitespace)
        .map(|at| at + 1)
        .unwrap_or(0);
    let file = &message[start..index + ".swift".len()];

    // Take only the leading digits. Splitting on ':' alone caught the rest of
    // the sentence when a message named two locations ("a/A.swift:1 and
    // b/B.swift:2"), which parsed as nothing and lost the line entirely.
    let rest = &message[index + MARKER.len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let line = digits.parse::<u32>().ok().filter(|&line| line > 0);

    Some((file.to_owned(), line))
}

/// Turns a path found in a log into a repo-relative path, when the file
/// actually exists in the repo.
///
/// Absolute paths recorded on another machine (a CI runner's checkout
/// directory) cannot be resolved here, and are rejected rather than guessed
/// at: an earlier version stripped one specific CI provider's checkout prefix,
/// which silently did the wrong thing everywhere else.
pub fn resolve_repo_file(repo: &Path, file: &str) -> Option<String> {
    let normalized = normalize_separators(file);
    let repo_text = normalize_separators(&repo.to_string_lossy());
    let relative = normalized
        .strip_prefix(&format!("{repo_text}/"))
        .unwrap_or(&normalized);
    repo.join(relative).is_file().then(|| relative.to_owned())
}

/// True for the package manifests whose change invalidates dependency
/// resolution, and so plausibly explains a build failure on its own.
pub fn is_package_manifest(path: &str) -> bool {
    let normalized = normalize_separators(path);
    let name = normalized.rsplit('/').next().unwrap_or(&normalized);
    matches!(name, "Package.swift" | "Package.resolved")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absolute_diagnostic_path_matches_its_repo_relative_change() {
        assert!(same_file(
            "/Users/me/proj/Sources/App/Foo.swift",
            "Sources/App/Foo.swift"
        ));
    }

    #[test]
    fn identical_paths_match() {
        assert!(same_file("Sources/App/Foo.swift", "Sources/App/Foo.swift"));
    }

    /// The bug this function exists to prevent: a raw `ends_with` made
    /// `xa.swift` match a change to `a.swift`.
    #[test]
    fn a_partial_filename_does_not_match() {
        assert!(!same_file("/repo/Sources/xa.swift", "a.swift"));
        assert!(!same_file("/repo/Sources/EvilFoo.swift", "Foo.swift"));
        assert!(!same_file("Foo.swift", "/repo/Sources/EvilFoo.swift"));
    }

    #[test]
    fn different_directories_do_not_match() {
        assert!(!same_file("/repo/Vendor/Foo.swift", "Sources/Foo.swift"));
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
    fn matches_any_finds_one_of_several() {
        let changed = vec!["Sources/A.swift".to_owned(), "Sources/B.swift".to_owned()];
        assert!(matches_any("/repo/Sources/B.swift", &changed));
        assert!(!matches_any("/repo/Sources/C.swift", &changed));
        assert!(!matches_any("/repo/Sources/B.swift", &[]));
    }

    #[test]
    fn extracts_a_file_and_line_from_a_failure_message() {
        let (file, line) =
            location_from_message("XCTAssertEqual failed at /repo/Tests/FooTests.swift:42:5")
                .unwrap();
        assert_eq!(file, "/repo/Tests/FooTests.swift");
        assert_eq!(line, Some(42));
    }

    /// Splitting on ':' caught the rest of the sentence when a message named
    /// two locations, so the line was lost entirely.
    #[test]
    fn takes_the_first_location_when_a_message_names_two() {
        let (file, line) = location_from_message("two /a/A.swift:1 and /b/B.swift:2").unwrap();
        assert_eq!(file, "/a/A.swift");
        assert_eq!(line, Some(1));
    }

    #[test]
    fn a_location_at_the_start_of_a_message_is_found() {
        let (file, line) = location_from_message("/repo/Tests/FooTests.swift:7: failed").unwrap();
        assert_eq!(file, "/repo/Tests/FooTests.swift");
        assert_eq!(line, Some(7));
    }

    #[test]
    fn a_message_with_no_location_yields_none() {
        assert!(location_from_message("assertion failed").is_none());
        assert!(location_from_message("").is_none());
    }

    #[test]
    fn an_unparseable_line_number_is_none_but_the_file_survives() {
        let (file, line) = location_from_message("prefix/FooTests.swift:oops: x").unwrap();
        assert_eq!(file, "prefix/FooTests.swift");
        assert_eq!(line, None);
    }

    /// Git blame's `-L` is 1-based, so a zero line must not reach it.
    #[test]
    fn line_zero_is_treated_as_no_line() {
        let (_, line) = location_from_message("/a/Foo.swift:0: boom").unwrap();
        assert_eq!(line, None);
    }

    #[test]
    fn recognizes_package_manifests_only_as_whole_filenames() {
        assert!(is_package_manifest("Package.swift"));
        assert!(is_package_manifest("Sub/Package.resolved"));
        assert!(is_package_manifest(r"Sub\Package.swift"));
        // Not a manifest merely because the name ends the same way.
        assert!(!is_package_manifest("Sources/MyPackage.swift"));
        assert!(!is_package_manifest("Sources/App.swift"));
    }

    #[test]
    fn resolve_repo_file_rejects_a_path_that_is_not_in_the_repo() {
        let repo = Path::new("/definitely/not/a/repo");
        assert_eq!(resolve_repo_file(repo, "/elsewhere/Foo.swift"), None);
    }
}
