use buildlens_core::{CrashType, TestCrash, TestResult, TestStatus};
use regex::Regex;
use std::sync::OnceLock;
static RES: OnceLock<Vec<Regex>> = OnceLock::new();
fn re() -> &'static Vec<Regex> {
    RES.get_or_init(|| {
        [
            // XCTest prints `Test Case '-[Suite testName]' started`. The
            // optional `-[` is one unit: matching a single character from
            // `[-\[]` left the bracket on the suite name ("[MySuite") and the
            // closing one on the test ("testThing]").
            r"Test Case '(?:-\[)?([^\]\s]+)[\s\]]+([^'\]]+)\]?' started",
            r"Test Case '(?:-\[)?([^\]\s]+)[\s\]]+([^'\]]+)\]?' (passed|failed) \(([0-9.]+) seconds?\)",
            r"([0-9]+(?:\.[0-9]+)?)\s+elapsed",
            // Xcode 16+ prints XCTest results in a second form:
            // `Test case 'FlakyTests.testThing()' failed on 'Clone 1 of
            // iPhone 17 Pro - App (12345)' (0.004 seconds)`. Lowercase "case",
            // suite and test joined by a dot, and an optional destination
            // clause before the duration.
            //
            // This must be matched as XCTest. It is close enough to Swift
            // Testing's `Test 'name' passed (0.1 seconds)` that the Swift
            // pattern claimed it, filing every XCTest in the run under a suite
            // literally named "SwiftTesting" with the class folded into the
            // test name.
            r"Test case '([^'.]+)\.([^']+)' (passed|failed)(?: on '[^']*')?(?: \(([0-9.]+) seconds?\))?",
        ]
        .into_iter()
        .map(|p| Regex::new(p).unwrap())
        .collect()
    })
}
pub fn started(l: &str) -> Option<String> {
    let c = re()[0].captures(l)?;
    Some(format!("{}:{}", &c[1], c[2].trim()))
}
pub fn result(l: &str) -> Option<TestResult> {
    // The bracketed form first, then Xcode 16+'s dotted one. Both are XCTest,
    // and a line matches at most one, so the order is for predictability
    // rather than correctness.
    let (c, status_group, duration_group) = match re()[1].captures(l) {
        Some(c) => (c, 3, 4),
        None => (re()[3].captures(l)?, 3, 4),
    };
    let status = if &c[status_group] == "passed" {
        TestStatus::Passed
    } else {
        TestStatus::Failed
    };
    let suite = c[1].trim();
    let test = c[2].trim();
    let fp = matches!(status, TestStatus::Failed)
        .then(|| format!("XCTEST:{}:{}", suite, test.to_lowercase()));
    Some(TestResult {
        suite: suite.into(),
        test: test.into(),
        status,
        // Absent when Xcode names a destination but no duration.
        duration_seconds: c.get(duration_group).and_then(|d| d.as_str().parse().ok()),
        message: None,
        fingerprint: fp,
    })
}
pub fn assertion(l: &str) -> Option<String> {
    let lower = l.to_lowercase();
    if lower.contains("xctassert") || lower.contains("assertion failed") {
        Some(l.trim().to_string())
    } else {
        None
    }
}
pub fn swift_testing_result(l: &str) -> Option<TestResult> {
    let c = re_swift().captures(l)?;
    let status = if &c[2] == "passed" {
        TestStatus::Passed
    } else {
        TestStatus::Failed
    };
    let fp = matches!(status, TestStatus::Failed)
        .then(|| format!("SWIFT_TEST:{}", c[1].trim().to_lowercase()));
    Some(TestResult {
        suite: "SwiftTesting".into(),
        test: c[1].trim().into(),
        status,
        duration_seconds: c.get(3).and_then(|x| x.as_str().parse().ok()),
        message: None,
        fingerprint: fp,
    })
}
fn re_swift() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        // Only `Test 'name'`, never `Test case 'Suite.name'`. This used to
        // accept both, which meant Xcode 16+'s XCTest lines were recorded as
        // Swift Testing results under a suite named "SwiftTesting" — see the
        // dotted pattern in `re()`, which now claims them. Swift Testing
        // display names can contain dots and spaces, so the name itself stays
        // unconstrained; it is the `case` keyword that distinguishes them.
        Regex::new(r"Test '([^']+)' (passed|failed)(?: \(([0-9.]+) seconds?\))?").unwrap()
    })
}
pub fn crash(l: &str, test: Option<String>) -> Option<TestCrash> {
    let x = l.to_lowercase();
    let (kind, msg) = if x.contains("unexpectedly found nil") {
        (CrashType::UnexpectedNil, l)
    } else if x.contains("fatal error:") {
        (CrashType::FatalError, l)
    } else if x.contains("signal sig") || x.contains("received signal") || x.contains("exc_crash") {
        (CrashType::Signal, l)
    } else {
        return None;
    };
    Some(TestCrash {
        test,
        crash_type: kind,
        message: msg.trim().into(),
        file: None,
        line: None,
    })
}
pub fn elapsed(l: &str) -> Option<f64> {
    re()[2].captures(l).and_then(|c| c[1].parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Xcode 16+ prints XCTest results as `Test case 'Suite.test()' failed on
    /// 'Clone 1 of iPhone 17 Pro - App (12345)' (0.004 seconds)`. This was
    /// matched by the Swift Testing pattern, which files everything under a
    /// suite named "SwiftTesting" with the class folded into the test name.
    #[test]
    fn reads_the_dotted_form_xcode_16_prints() {
        let line = "Test case 'FlakyTests.testFlakyNetworkCall()' failed on 'Clone 1 of iPhone 17 Pro - UnitTestsApp (78886)' (0.004 seconds)";
        let result = result(line).expect("an XCTest result");
        assert_eq!(
            result.suite, "FlakyTests",
            "the class, not \"SwiftTesting\""
        );
        assert_eq!(result.test, "testFlakyNetworkCall()");
        assert_eq!(result.status, TestStatus::Failed);
        assert_eq!(result.duration_seconds, Some(0.004));
    }

    #[test]
    fn the_dotted_form_parses_without_a_destination_clause() {
        let result = result("Test case 'MySuite.testThing()' passed (0.5 seconds)").unwrap();
        assert_eq!(result.suite, "MySuite");
        assert_eq!(result.test, "testThing()");
        assert_eq!(result.status, TestStatus::Passed);
    }

    /// Swift Testing prints `Test 'name' passed`, with no `case` keyword. It
    /// must still parse, and must not be confused with the dotted XCTest form.
    #[test]
    fn swift_testing_lines_are_still_swift_testing() {
        let swift = swift_testing_result("Test 'Feature loads' passed (0.42 seconds)").unwrap();
        assert_eq!(swift.suite, "SwiftTesting");
        assert_eq!(swift.test, "Feature loads");
    }

    /// The regression itself: an XCTest line must not be claimed by the Swift
    /// Testing parser, whichever order a caller tries them in.
    #[test]
    fn the_swift_testing_pattern_no_longer_claims_xctest_lines() {
        let line = "Test case 'FlakyTests.testAlwaysBroken()' failed on 'Clone 1 of iPhone 17 Pro' (0.004 seconds)";
        assert!(
            swift_testing_result(line).is_none(),
            "Swift Testing must not match an XCTest line"
        );
        assert_eq!(result(line).unwrap().suite, "FlakyTests");
    }

    #[test]
    fn reads_a_started_test() {
        assert_eq!(
            started("Test Case '-[MySuite testThing]' started.").as_deref(),
            Some("MySuite:testThing")
        );
    }

    #[test]
    fn reads_a_passing_result() {
        let result = result("Test Case '-[MySuite testThing]' passed (0.42 seconds).").unwrap();
        assert_eq!(result.suite, "MySuite");
        assert_eq!(result.test, "testThing");
        assert_eq!(result.status, TestStatus::Passed);
        assert_eq!(result.duration_seconds, Some(0.42));
        // Only failures carry a fingerprint; there is nothing to cluster on a pass.
        assert!(result.fingerprint.is_none());
    }

    #[test]
    fn reads_a_failing_result_with_a_fingerprint() {
        let result = result("Test Case '-[MySuite testThing]' failed (1.5 seconds).").unwrap();
        assert_eq!(result.status, TestStatus::Failed);
        assert!(
            result
                .fingerprint
                .as_deref()
                .is_some_and(|fingerprint| fingerprint.starts_with("XCTEST:")),
            "got {:?}",
            result.fingerprint
        );
    }

    #[test]
    fn an_ordinary_line_is_not_a_test_result() {
        assert!(result("Compiling Foo.swift").is_none());
        assert!(started("Compiling Foo.swift").is_none());
    }

    #[test]
    fn recognizes_assertion_messages() {
        assert!(assertion("/a/T.swift:5: error: XCTAssertEqual failed").is_some());
        assert!(assertion("Assertion Failed: bad state").is_some());
        assert!(assertion("Compiling Foo.swift").is_none());
    }

    #[test]
    fn reads_a_swift_testing_result() {
        let result = swift_testing_result("Test 'Feature loads' passed (0.42 seconds)").unwrap();
        assert_eq!(result.suite, "SwiftTesting");
        assert_eq!(result.test, "Feature loads");
        assert_eq!(result.status, TestStatus::Passed);
        assert_eq!(result.duration_seconds, Some(0.42));
    }

    #[test]
    fn a_swift_testing_result_may_omit_its_duration() {
        let result = swift_testing_result("Test 'Feature loads' failed").unwrap();
        assert_eq!(result.status, TestStatus::Failed);
        assert_eq!(result.duration_seconds, None);
        assert!(result.fingerprint.is_some());
    }

    #[test]
    fn classifies_each_crash_kind() {
        assert_eq!(
            crash("Fatal error: Unexpectedly found nil", None)
                .unwrap()
                .crash_type,
            CrashType::UnexpectedNil
        );
        assert_eq!(
            crash("Fatal error: something broke", None)
                .unwrap()
                .crash_type,
            CrashType::FatalError
        );
        assert_eq!(
            crash("Terminated due to signal SIGABRT", None)
                .unwrap()
                .crash_type,
            CrashType::Signal
        );
        assert_eq!(
            crash("EXC_CRASH (SIGABRT)", None).unwrap().crash_type,
            CrashType::Signal
        );
    }

    #[test]
    fn a_crash_records_the_test_it_happened_in() {
        let crash = crash("Fatal error: boom", Some("Suite:testThing".into())).unwrap();
        assert_eq!(crash.test.as_deref(), Some("Suite:testThing"));
    }

    #[test]
    fn an_ordinary_line_is_not_a_crash() {
        assert!(crash("Compiling Foo.swift", None).is_none());
    }

    #[test]
    fn reads_an_elapsed_duration() {
        assert_eq!(elapsed("** TEST SUCCEEDED ** 12.5 elapsed"), Some(12.5));
        assert_eq!(elapsed("no duration here"), None);
    }
}
