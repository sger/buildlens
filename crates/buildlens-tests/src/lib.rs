//! Test-log parsing behind a common trait.
//!
//! The parsing itself lives in [`buildlens_parser::xctest`], which is what the
//! log parser already uses. This crate exposes it as a trait so a caller can
//! hold a `dyn TestLogParser` and try several formats over one line without
//! knowing which framework produced it — and so adding a third framework does
//! not mean touching the parser's hot loop.
//!
//! Deliberately a thin wrapper: the previous version reimplemented nothing and
//! returned `None` from every parser, so any caller saw "no tests" from every
//! log. A second copy of the regexes would have been worse — two
//! implementations of the same format drift, and only one of them gets fixed.

pub use buildlens_core::{CrashType, TestCrash, TestResult, TestStatus};

/// One test-log format.
///
/// [`parse`](TestLogParser::parse) returns `None` for a line this format does
/// not recognize, which is the common case: most lines of a build log are not
/// test results.
pub trait TestLogParser {
    /// The framework this parser recognizes, for diagnostics.
    fn name(&self) -> &'static str;

    /// A completed test result, if this line reports one.
    fn parse(&self, line: &str) -> Option<TestResult>;

    /// The name of a test this line reports as *starting*.
    ///
    /// Separate from [`parse`](TestLogParser::parse) because a start carries no
    /// outcome: a test that starts and never reports is how a crash appears,
    /// and collapsing the two would hide exactly that case. Defaults to `None`
    /// for formats that announce no start.
    fn started(&self, _line: &str) -> Option<String> {
        None
    }

    /// A crash this line describes, attributed to `current_test` when one is
    /// in flight. Defaults to `None`: crashes are reported by the runner
    /// rather than by a test framework's own output.
    fn crash(&self, _line: &str, _current_test: Option<String>) -> Option<TestCrash> {
        None
    }
}

/// XCTest, which prints `Test Case '-[Suite testName]' passed (0.1 seconds)`.
pub struct XCTestParser;

impl TestLogParser for XCTestParser {
    fn name(&self) -> &'static str {
        "xctest"
    }

    fn parse(&self, line: &str) -> Option<TestResult> {
        buildlens_parser::xctest::result(line)
    }

    fn started(&self, line: &str) -> Option<String> {
        buildlens_parser::xctest::started(line)
    }

    fn crash(&self, line: &str, current_test: Option<String>) -> Option<TestCrash> {
        buildlens_parser::xctest::crash(line, current_test)
    }
}

/// Swift Testing, which prints `Test 'name' passed (0.1 seconds)`.
pub struct SwiftTestingParser;

impl TestLogParser for SwiftTestingParser {
    fn name(&self) -> &'static str {
        "swift-testing"
    }

    fn parse(&self, line: &str) -> Option<TestResult> {
        buildlens_parser::xctest::swift_testing_result(line)
    }
}

/// Every parser, in the order a caller should try them.
///
/// The two formats do not currently overlap — XCTest prints `Test Case '`,
/// which the Swift Testing pattern does not match — so the order is not
/// load-bearing today. It is fixed rather than incidental so that a future
/// pattern which does overlap resolves predictably instead of by declaration
/// accident; `parse_any` returns the first match.
pub fn parsers() -> Vec<Box<dyn TestLogParser>> {
    vec![Box::new(XCTestParser), Box::new(SwiftTestingParser)]
}

/// The first result any parser recognizes in `line`.
pub fn parse_any(line: &str) -> Option<TestResult> {
    parsers().into_iter().find_map(|parser| parser.parse(line))
}

#[cfg(test)]
mod tests {
    use super::*;

    const XCTEST_PASS: &str = "Test Case '-[CoreTests testThing]' passed (0.123 seconds).";
    const XCTEST_FAIL: &str = "Test Case '-[CoreTests testThing]' failed (0.456 seconds).";
    const SWIFT_PASS: &str = "Test 'someExample()' passed (0.5 seconds)";
    const SWIFT_FAIL: &str = "Test 'someExample()' failed (0.5 seconds)";

    /// The whole point of the crate: the stubs used to return `None` here, so
    /// every caller saw an empty test run from a log full of results.
    #[test]
    fn xctest_results_are_actually_parsed() {
        let result = XCTestParser.parse(XCTEST_PASS).expect("a passing result");
        assert_eq!(result.suite, "CoreTests");
        assert_eq!(result.test, "testThing");
        assert_eq!(result.status, TestStatus::Passed);
        assert_eq!(result.duration_seconds, Some(0.123));
    }

    #[test]
    fn swift_testing_results_are_actually_parsed() {
        let result = SwiftTestingParser
            .parse(SWIFT_PASS)
            .expect("a passing result");
        assert_eq!(result.test, "someExample()");
        assert_eq!(result.status, TestStatus::Passed);
        assert_eq!(result.duration_seconds, Some(0.5));
    }

    /// A failure carries a fingerprint so the same failing test is recognizable
    /// across builds; a pass does not need one.
    #[test]
    fn only_failures_carry_a_fingerprint() {
        assert!(
            XCTestParser
                .parse(XCTEST_PASS)
                .unwrap()
                .fingerprint
                .is_none()
        );
        assert!(
            XCTestParser
                .parse(XCTEST_FAIL)
                .unwrap()
                .fingerprint
                .is_some()
        );
        assert!(
            SwiftTestingParser
                .parse(SWIFT_FAIL)
                .unwrap()
                .fingerprint
                .is_some()
        );
    }

    #[test]
    fn failures_are_reported_as_failed() {
        assert_eq!(
            XCTestParser.parse(XCTEST_FAIL).unwrap().status,
            TestStatus::Failed
        );
        assert_eq!(
            SwiftTestingParser.parse(SWIFT_FAIL).unwrap().status,
            TestStatus::Failed
        );
    }

    /// Most lines of a build log are not test results, so a parser must be
    /// quiet rather than guess.
    #[test]
    fn unrelated_lines_are_not_claimed() {
        for line in [
            "",
            "Compiling Core",
            "warning: unused variable 'x'",
            "** BUILD SUCCEEDED **",
        ] {
            assert!(XCTestParser.parse(line).is_none(), "{line:?}");
            assert!(SwiftTestingParser.parse(line).is_none(), "{line:?}");
            assert!(parse_any(line).is_none(), "{line:?}");
        }
    }

    /// A start reports no outcome. Keeping it separate from a result is what
    /// makes a test that started and never finished — a crash — visible.
    #[test]
    fn a_start_is_reported_separately_from_a_result() {
        let line = "Test Case '-[CoreTests testThing]' started.";
        assert_eq!(
            XCTestParser.started(line).as_deref(),
            Some("CoreTests:testThing")
        );
        assert!(
            XCTestParser.parse(line).is_none(),
            "a start is not a result"
        );
    }

    #[test]
    fn crashes_are_attributed_to_the_running_test() {
        let crash = XCTestParser
            .crash(
                "Fatal error: Unexpectedly found nil while unwrapping",
                Some("CoreTests:testThing".to_owned()),
            )
            .expect("a crash");
        assert_eq!(crash.test.as_deref(), Some("CoreTests:testThing"));
        assert_eq!(crash.crash_type, CrashType::UnexpectedNil);
    }

    /// Swift Testing announces no start and reports no crash of its own, so
    /// the defaults apply rather than a wrong answer.
    #[test]
    fn swift_testing_reports_no_start_or_crash() {
        assert!(SwiftTestingParser.started(SWIFT_PASS).is_none());
        assert!(
            SwiftTestingParser
                .crash("Fatal error: boom", None)
                .is_none()
        );
    }

    /// The two formats must not both claim one line, or `parse_any` would
    /// silently depend on declaration order. XCTest prints `Test Case '`,
    /// which the Swift Testing pattern does not match.
    #[test]
    fn the_two_formats_do_not_claim_each_others_lines() {
        assert!(
            SwiftTestingParser.parse(XCTEST_PASS).is_none(),
            "Swift Testing claimed an XCTest line"
        );
        assert!(
            XCTestParser.parse(SWIFT_PASS).is_none(),
            "XCTest claimed a Swift Testing line"
        );
        // So `parse_any` gives each format's own result regardless of order.
        assert_eq!(parse_any(XCTEST_PASS).unwrap().suite, "CoreTests");
        assert_eq!(parse_any(SWIFT_PASS).unwrap().suite, "SwiftTesting");
    }

    #[test]
    fn every_parser_is_named() {
        let names: Vec<_> = parsers().iter().map(|parser| parser.name()).collect();
        assert_eq!(names, vec!["xctest", "swift-testing"]);
    }

    /// The trait must stay object-safe: callers hold `dyn TestLogParser` to
    /// try several formats over one line.
    #[test]
    fn parsers_are_usable_as_trait_objects() {
        let parsers: Vec<Box<dyn TestLogParser>> = parsers();
        let found = parsers
            .iter()
            .filter_map(|parser| parser.parse(XCTEST_PASS))
            .count();
        assert!(found > 0);
    }
}
