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
    let c = re()[1].captures(l)?;
    let status = if &c[3] == "passed" {
        TestStatus::Passed
    } else {
        TestStatus::Failed
    };
    let fp = matches!(status, TestStatus::Failed)
        .then(|| format!("XCTEST:{}:{}", &c[1], c[2].trim().to_lowercase()));
    Some(TestResult {
        suite: c[1].into(),
        test: c[2].trim().into(),
        status,
        duration_seconds: c[4].parse().ok(),
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
        Regex::new(r"(?:Test|Test case) '([^']+)' (passed|failed)(?: \(([0-9.]+) seconds?\))?")
            .unwrap()
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
