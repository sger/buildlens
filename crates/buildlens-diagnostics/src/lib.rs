//! Turns compiler and toolchain output into classified, fingerprinted
//! diagnostics.
//!
//! Two sources feed this crate and must agree: a text log's formatted lines
//! (`parse`) and an `.xcactivitylog`'s already-separated fields
//! (`from_parts`). Both funnel through [`build`], so the same diagnostic found
//! either way produces the same fingerprint and deduplicates against itself.

use buildlens_core::{
    DiagnosticAggregate, DiagnosticCategory, DiagnosticExample, DiagnosticSeverity, Swift6Summary,
};
use regex::Regex;
use std::collections::BTreeMap;
use std::sync::OnceLock;

/// A diagnostic line carrying a file, line and column:
/// `/path/File.swift:12:5: warning: message`.
fn located_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(.+?):(\d+):(\d+):\s*(note|warning|error|fatal error):\s*(.*)$").unwrap()
    })
}

/// A diagnostic with no location — linker and toolchain errors mostly.
fn bare_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^.*?\b(note|warning|error|fatal error):\s*(.*)$").unwrap())
}

/// Volatile substrings that differ between two runs of the *same* diagnostic:
/// pointer addresses, mangled-symbol hashes, temporary build paths. Replaced
/// before fingerprinting so those two runs aggregate together.
fn volatile_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?x)
              0x[0-9a-fA-F]+                    # hex addresses: 0xDEADBEEF
            | \b[0-9a-fA-F]{16,}\b              # long bare hashes
            | \b\d{4,}\b                        # long bare numbers
            ",
        )
        .unwrap()
    })
}

/// Parses one formatted log line. Returns `None` for lines that are not
/// diagnostics at all, which is most of a build log.
pub fn parse(line: &str) -> Option<DiagnosticAggregate> {
    let trimmed = line.trim();
    let (file, line_no, column, severity_name, message) =
        if let Some(captures) = located_re().captures(trimmed) {
            (
                Some(captures[1].to_string()),
                captures[2].parse().ok(),
                captures[3].parse().ok(),
                captures[4].to_string(),
                captures[5].to_string(),
            )
        } else {
            let captures = bare_re().captures(trimmed)?;
            (
                None,
                None,
                None,
                captures[1].to_string(),
                captures[2].trim().to_string(),
            )
        };
    Some(build(
        parse_severity(&severity_name)?,
        file,
        line_no,
        column,
        message,
    ))
}

/// Maps the severity word the compiler printed onto the enum.
///
/// Exhaustive on purpose: there is no catch-all arm, because an unrecognized
/// word previously fell through to `Fatal` — the most severe value — which
/// turns "we did not understand this" into "the build is broken". Anything
/// outside the known set is not a diagnostic.
fn parse_severity(word: &str) -> Option<DiagnosticSeverity> {
    match word {
        "note" => Some(DiagnosticSeverity::Note),
        "warning" => Some(DiagnosticSeverity::Warning),
        "error" => Some(DiagnosticSeverity::Error),
        "fatal error" => Some(DiagnosticSeverity::Fatal),
        _ => None,
    }
}

/// Builds a diagnostic from parts that are already separated, for callers that
/// did not start from a formatted log line — an `.xcactivitylog` records the
/// message, severity and location as distinct fields. Classification and
/// fingerprinting stay here so a diagnostic found in an activity log
/// deduplicates against the identical one found in a text log.
pub fn from_parts(
    severity: DiagnosticSeverity,
    file: Option<String>,
    line: Option<u32>,
    column: Option<u32>,
    message: String,
) -> DiagnosticAggregate {
    build(severity, file, line, column, message)
}

fn build(
    severity: DiagnosticSeverity,
    file: Option<String>,
    line: Option<u32>,
    column: Option<u32>,
    message: String,
) -> DiagnosticAggregate {
    let category = classify(&message);
    // `as_str`, not `{:?}`: fingerprints are persisted and compared across
    // runs, and `Debug` output carries no stability guarantee.
    let fingerprint = format!(
        "{}:{}:{}:{}",
        category.as_str(),
        severity.as_str(),
        file.as_deref().unwrap_or_default(),
        normalize(&message)
    );
    DiagnosticAggregate {
        fingerprint,
        severity,
        category,
        occurrences: 1,
        example: DiagnosticExample {
            file,
            line,
            column,
            message,
            target: None,
        },
    }
}

/// Collapses diagnostics sharing a fingerprint into one row carrying the
/// occurrence count.
///
/// This is the crate's reason to exist: a `DiagnosticAggregate` straight out of
/// [`parse`] has `occurrences: 1` and has aggregated nothing. A single Sendable
/// warning in a header can appear thousands of times in one build, and
/// reporting it once with a count is the difference between a readable summary
/// and a wall of text.
///
/// The first occurrence wins as the retained example, so the reported location
/// is the earliest one in the log rather than an arbitrary one.
pub fn aggregate(
    diagnostics: impl IntoIterator<Item = DiagnosticAggregate>,
) -> Vec<DiagnosticAggregate> {
    let mut by_fingerprint: Vec<DiagnosticAggregate> = Vec::new();
    let mut index: BTreeMap<String, usize> = BTreeMap::new();
    for diagnostic in diagnostics {
        match index.get(&diagnostic.fingerprint) {
            Some(&at) => by_fingerprint[at].occurrences += diagnostic.occurrences,
            None => {
                index.insert(diagnostic.fingerprint.clone(), by_fingerprint.len());
                by_fingerprint.push(diagnostic);
            }
        }
    }
    by_fingerprint
}

/// Classifies a diagnostic by its message.
///
/// **Order is priority, and it is deliberate.** Real Swift 6 diagnostics
/// routinely name several concepts at once ("Sendable closure in a deprecated
/// API", "main actor-isolated value passed across an isolation boundary"), so
/// something must break the tie. The rule is most-specific-first: the exact
/// concurrency mechanism before the general concurrency bucket, and any
/// actionable category before `Deprecation`, which is the most common word in
/// Swift build output and would otherwise swallow everything.
///
/// Patterns are matched against a lowercased message and are deliberately
/// narrow — an earlier version matched bare `assert` and `retain`, which
/// caught ordinary compiler messages that merely mentioned the words.
pub fn classify(message: &str) -> DiagnosticCategory {
    let text = message.to_lowercase();
    let has_any = |needles: &[&str]| needles.iter().any(|needle| text.contains(needle));

    // Concurrency, most specific mechanism first.
    if has_any(&["actor-isolated", "main actor-isolated", "actor isolation"]) {
        DiagnosticCategory::SwiftActorIsolation
    } else if has_any(&["sendable", "does not conform to the sendable"]) {
        DiagnosticCategory::SwiftSendable
    } else if has_any(&[
        "isolation boundary",
        "swift 6",
        "data race",
        "concurrency-safe",
        "nonisolated",
    ]) {
        DiagnosticCategory::SwiftConcurrency
    } else if has_any(&[
        "undefined symbol",
        "duplicate symbol",
        "linker command",
        "ld:",
    ]) {
        DiagnosticCategory::Linker
    } else if has_any(&[
        "code sign",
        "codesign",
        "provisioning profile",
        "no signing certificate",
    ]) {
        DiagnosticCategory::CodeSigning
    } else if has_any(&[
        "xctest",
        "xctassert",
        "test suite",
        "testing failed",
        "failed to run tests",
    ]) {
        DiagnosticCategory::XCTest
    } else if has_any(&["simulator", "simctl", "unable to boot", "device not found"]) {
        DiagnosticCategory::Simulator
    } else if has_any(&[
        "swiftpm",
        "package resolution",
        "package.swift",
        "missing package product",
        "dependency graph",
    ]) {
        DiagnosticCategory::Spm
    } else if has_any(&[
        "retain cycle",
        "strong reference cycle",
        "memory leak",
        "deallocated while",
        "over-released",
    ]) {
        DiagnosticCategory::MemoryLifecycle
    } else if has_any(&["app intent", "appintents"]) {
        DiagnosticCategory::AppIntents
    } else if has_any(&[
        "build setting",
        "deployment target",
        "unsupported architecture",
        "no such module",
    ]) {
        DiagnosticCategory::BuildConfiguration
    } else if has_any(&[
        "crashed",
        "signal sigabrt",
        "fatal error:",
        "exc_bad_access",
    ]) {
        DiagnosticCategory::Crash
    } else if has_any(&["is deprecated", "deprecated in", "was deprecated"]) {
        DiagnosticCategory::Deprecation
    } else {
        DiagnosticCategory::Unknown
    }
}

/// Strips the parts of a message that differ between two occurrences of the
/// same underlying problem, so both land on one fingerprint.
///
/// Whitespace is collapsed and case folded because the same diagnostic can be
/// wrapped differently depending on terminal width.
pub fn normalize(message: &str) -> String {
    volatile_re()
        .replace_all(message, "<id>")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Counts the unique diagnostics that block a Swift 6 language-mode migration.
///
/// Reads `category` rather than re-scanning the message text: `classify`
/// already made that judgment, and repeating the keyword list here would let
/// the two drift apart.
pub fn swift6(diagnostics: &[DiagnosticAggregate]) -> Swift6Summary {
    let mut summary = Swift6Summary::default();
    let mut seen = std::collections::BTreeSet::new();
    for diagnostic in diagnostics {
        if diagnostic.category.is_swift6_blocker() && seen.insert(&diagnostic.fingerprint) {
            summary.unique_blockers += 1;
            *summary
                .by_category
                .entry(diagnostic.category.as_str().to_owned())
                .or_default() += 1;
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_located_diagnostic() {
        let d = parse("/App/Foo.swift:12:5: warning: 'old()' is deprecated").unwrap();
        assert_eq!(d.severity, DiagnosticSeverity::Warning);
        assert_eq!(d.category, DiagnosticCategory::Deprecation);
        assert_eq!(d.example.file.as_deref(), Some("/App/Foo.swift"));
        assert_eq!(d.example.line, Some(12));
        assert_eq!(d.example.column, Some(5));
        assert_eq!(d.example.message, "'old()' is deprecated");
    }

    #[test]
    fn parses_a_diagnostic_with_no_location() {
        let d = parse("error: linker command failed with exit code 1").unwrap();
        assert_eq!(d.severity, DiagnosticSeverity::Error);
        assert_eq!(d.category, DiagnosticCategory::Linker);
        assert_eq!(d.example.file, None);
        assert_eq!(d.example.line, None);
    }

    /// `DiagnosticSeverity::Note` exists in the model, so it must be
    /// producible. Notes carry the compiler's explanation of an adjacent error
    /// ("add '@MainActor' to make this explicit") and were previously dropped
    /// on the floor by a regex that only matched warning/error.
    #[test]
    fn parses_notes() {
        let d = parse("/App/Foo.swift:3:1: note: add '@MainActor' to make this explicit").unwrap();
        assert_eq!(d.severity, DiagnosticSeverity::Note);
        assert_eq!(d.example.line, Some(3));
    }

    #[test]
    fn parses_fatal_errors_as_fatal() {
        let d = parse("/App/Foo.swift:1:1: fatal error: unreachable").unwrap();
        assert_eq!(d.severity, DiagnosticSeverity::Fatal);
    }

    /// An unrecognized severity word must not become `Fatal`. The previous
    /// catch-all arm turned "we did not understand this" into the most severe
    /// value in the enum.
    #[test]
    fn an_unknown_severity_is_not_a_diagnostic() {
        assert!(parse("/App/Foo.swift:1:1: remark: made an optimization").is_none());
        assert!(parse("just a line of build output").is_none());
        assert!(parse("").is_none());
    }

    #[test]
    fn a_windows_style_path_does_not_break_parsing() {
        // The location regex is non-greedy on the path, so a drive-letter
        // colon must not be mistaken for the line-number separator.
        let d = parse(r"C:\src\Foo.swift:12:5: error: bad").unwrap();
        assert_eq!(d.example.line, Some(12));
        assert_eq!(d.example.column, Some(5));
    }

    #[test]
    fn classify_prefers_the_specific_concurrency_mechanism() {
        // Both words appear; actor isolation is the actionable one.
        assert_eq!(
            classify("main actor-isolated property cannot satisfy Sendable"),
            DiagnosticCategory::SwiftActorIsolation
        );
        assert_eq!(
            classify("type 'Foo' does not conform to the Sendable protocol"),
            DiagnosticCategory::SwiftSendable
        );
        assert_eq!(
            classify("passed across an isolation boundary"),
            DiagnosticCategory::SwiftConcurrency
        );
    }

    /// `Deprecation` is last among the actionable categories because
    /// "deprecated" is the most common word in Swift build output; matching it
    /// first buried more specific problems.
    #[test]
    fn a_specific_category_wins_over_deprecation() {
        assert_eq!(
            classify("undefined symbol in a deprecated API"),
            DiagnosticCategory::Linker
        );
        assert_eq!(
            classify("Sendable closure in a deprecated API"),
            DiagnosticCategory::SwiftSendable
        );
        // On its own, it still classifies.
        assert_eq!(
            classify("'old()' is deprecated"),
            DiagnosticCategory::Deprecation
        );
    }

    /// The loose patterns caught ordinary messages that merely used the word.
    #[test]
    fn narrow_patterns_do_not_over_match() {
        assert_eq!(
            classify("cannot convert value of type 'Assertion' to 'String'"),
            DiagnosticCategory::Unknown
        );
        assert_eq!(
            classify("the value is retained by the enclosing closure"),
            DiagnosticCategory::Unknown
        );
        assert_eq!(
            classify("XCTAssertEqual failed: 1 is not 2"),
            DiagnosticCategory::XCTest
        );
        assert_eq!(
            classify("strong reference cycle between Foo and Bar"),
            DiagnosticCategory::MemoryLifecycle
        );
    }

    /// Fingerprints are persisted and compared across runs, so a pointer
    /// address embedded in the message must not make two occurrences of the
    /// same problem look different.
    #[test]
    fn normalize_strips_volatile_addresses_and_hashes() {
        assert_eq!(normalize("crash at 0xDEADBEEF"), "crash at <id>");
        assert_eq!(normalize("build 12345678 failed"), "build <id> failed");
        // A standalone hash is stripped; one glued to a mangled symbol is not,
        // because `\b` needs a boundary. Mangled symbols are already stable
        // across runs, so leaving them intact is correct — recorded here so
        // the limit is deliberate rather than discovered later.
        assert_eq!(normalize("hash 0123456789abcdef0 here"), "hash <id> here");
        assert_eq!(
            normalize("symbol $s3App4FooCACycfc0123456789abcdef"),
            "symbol $s3app4foocacycfc0123456789abcdef"
        );
        // Ordinary short numbers are meaningful and must survive.
        assert_eq!(normalize("expected 2 arguments"), "expected 2 arguments");
        // An ordinary word made of hex letters is not an id.
        assert_eq!(normalize("the cafe is closed"), "the cafe is closed");
    }

    #[test]
    fn normalize_collapses_whitespace_and_case() {
        assert_eq!(normalize("Too   Many\tSpaces"), "too many spaces");
    }

    #[test]
    fn the_same_problem_at_one_address_fingerprints_identically() {
        let first = parse("/App/Foo.swift:1:1: error: crash at 0xAAAA").unwrap();
        let second = parse("/App/Foo.swift:1:1: error: crash at 0xBBBB").unwrap();
        assert_eq!(first.fingerprint, second.fingerprint);
    }

    #[test]
    fn different_files_fingerprint_differently() {
        let a = parse("/App/Foo.swift:1:1: error: boom").unwrap();
        let b = parse("/App/Bar.swift:1:1: error: boom").unwrap();
        assert_ne!(a.fingerprint, b.fingerprint);
    }

    #[test]
    fn severity_is_part_of_the_fingerprint() {
        // The same text as a warning and as an error are different problems.
        let warning = parse("/App/Foo.swift:1:1: warning: boom").unwrap();
        let error = parse("/App/Foo.swift:1:1: error: boom").unwrap();
        assert_ne!(warning.fingerprint, error.fingerprint);
    }

    /// The whole point of the crate: a text log and an activity log describing
    /// the same problem must land on one row.
    #[test]
    fn text_and_activity_log_diagnostics_deduplicate_against_each_other() {
        let from_text = parse("/App/Foo.swift:12:5: warning: 'old()' is deprecated").unwrap();
        let from_activity = from_parts(
            DiagnosticSeverity::Warning,
            Some("/App/Foo.swift".into()),
            Some(12),
            Some(5),
            "'old()' is deprecated".into(),
        );
        assert_eq!(from_text.fingerprint, from_activity.fingerprint);
        assert_eq!(aggregate([from_text, from_activity]).len(), 1);
    }

    #[test]
    fn aggregate_counts_occurrences_and_keeps_the_first_example() {
        let diagnostics = vec![
            parse("/App/A.swift:1:1: warning: 'old()' is deprecated").unwrap(),
            parse("/App/A.swift:9:9: warning: 'old()' is deprecated").unwrap(),
            parse("/App/B.swift:1:1: error: undefined symbol _main").unwrap(),
        ];
        let rows = aggregate(diagnostics);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].occurrences, 2);
        // First occurrence wins, so the reported line is the earliest.
        assert_eq!(rows[0].example.line, Some(1));
        assert_eq!(rows[1].occurrences, 1);
    }

    #[test]
    fn aggregate_sums_counts_rather_than_overwriting() {
        let mut a = parse("/App/A.swift:1:1: warning: 'old()' is deprecated").unwrap();
        let mut b = a.clone();
        a.occurrences = 3;
        b.occurrences = 4;
        assert_eq!(aggregate([a, b])[0].occurrences, 7);
    }

    #[test]
    fn aggregate_of_nothing_is_empty() {
        assert!(aggregate([]).is_empty());
    }

    #[test]
    fn swift6_counts_unique_blockers_by_category() {
        let diagnostics =
            vec![
            parse("/App/A.swift:1:1: warning: type 'Foo' does not conform to the Sendable protocol")
                .unwrap(),
            // Same problem again — must not double-count.
            parse("/App/A.swift:1:1: warning: type 'Foo' does not conform to the Sendable protocol")
                .unwrap(),
            parse("/App/B.swift:2:2: error: main actor-isolated property cannot be referenced")
                .unwrap(),
            // Not a concurrency problem at all.
            parse("/App/C.swift:3:3: warning: 'old()' is deprecated").unwrap(),
        ];
        let summary = swift6(&diagnostics);
        assert_eq!(summary.unique_blockers, 2);
        assert_eq!(summary.by_category.get("swift_sendable"), Some(&1));
        assert_eq!(summary.by_category.get("swift_actor_isolation"), Some(&1));
        assert_eq!(summary.by_category.get("deprecation"), None);
    }

    /// Keys come from `as_str`, not `{:?}`, so they are stable across a
    /// variant rename and match what serde puts on the wire.
    #[test]
    fn swift6_keys_are_the_stable_spelling() {
        let d = parse(
            "/App/A.swift:1:1: warning: type 'Foo' does not conform to the Sendable protocol",
        )
        .unwrap();
        let summary = swift6(&[d]);
        assert!(summary.by_category.contains_key("swift_sendable"));
        assert!(!summary.by_category.contains_key("SwiftSendable"));
    }

    #[test]
    fn swift6_of_nothing_is_zero() {
        let summary = swift6(&[]);
        assert_eq!(summary.unique_blockers, 0);
        assert!(summary.by_category.is_empty());
    }
}
