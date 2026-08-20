//! Test results read from an `.xcresult` bundle.
//!
//! The text-log parser in `buildlens-parser` recognises test results by
//! matching the lines Xcode prints for humans. That works, and it is the only
//! option when all you have is a log, but it infers three things it cannot
//! know: where a suite name ends and a test name begins, which run of a
//! retried test came first, and what Xcode's own verdict for a test was. Each
//! inference has already been wrong — Xcode 26 prints
//! `Test case 'FlakyTests.testAlwaysBroken()' failed on 'Clone 1 of iPhone 17
//! Pro' (0.004 seconds)`, which the Swift Testing pattern matches, recording
//! every XCTest in the run under a suite literally named "SwiftTesting".
//!
//! The bundle states all three outright. `xcresulttool` returns a tree of
//! typed nodes: a `Test Case` holds `Repetition` children when Xcode retried
//! it, each numbered, each with its own result. So attempts come from Xcode
//! rather than from the order results happened to appear in a log — which
//! matters because parallel destinations interleave that order, and the
//! interleaving is invisible after the fact.
//!
//! Scope is deliberately tests only. Build timings, phases and diagnostics
//! keep coming from the activity log and the text log; this crate does not
//! read them even though the bundle carries some of them, because two sources
//! for one number is how they start to disagree.

use buildlens_core::{TestResult, TestStatus};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum XcresultError {
    #[error("{path} is not an .xcresult bundle")]
    NotABundle { path: String },
    #[error("{path} does not exist")]
    Missing { path: String },
    #[error(
        "xcresulttool is not available; it ships with Xcode, so check `xcode-select -p` points at an Xcode installation rather than the Command Line Tools"
    )]
    ToolMissing,
    /// Non-zero exit. Xcode 15 and earlier have no `get test-results`
    /// subcommand, which is the likeliest cause and worth naming, since the
    /// raw stderr says only "unexpected argument".
    #[error("xcresulttool could not read {path}: {message}")]
    ToolFailed { path: String, message: String },
    #[error("xcresulttool returned JSON this version does not understand: {0}")]
    Malformed(#[from] serde_json::Error),
}

impl XcresultError {
    /// Whether this is a bundle Xcode has not finished writing yet.
    ///
    /// Xcode adds the manifest entry before the bundle is readable — measured
    /// at four seconds apart on a large suite, manifest at 23:16:26 and
    /// `Info.plist` at 23:16:30. A watcher polling in that window sees a run it
    /// is supposed to attach and a bundle it cannot open, which is normal and
    /// resolves itself on the next scan.
    ///
    /// Matched on the message because `xcresulttool` reports it as a plain
    /// non-zero exit with no distinguishing code. Narrow on purpose: a bundle
    /// that is genuinely corrupt says the same thing, so this only decides
    /// whether to *report* the failure, never whether to retry it.
    pub fn is_incomplete_bundle(&self) -> bool {
        match self {
            Self::ToolFailed { message, .. } => {
                message.contains("Info.plist") && message.contains("does not exist")
            }
            _ => false,
        }
    }
}

/// One run of one test, as the bundle records it.
///
/// Distinct from [`buildlens_core::TestResult`] because that type is what the
/// rest of BuildLens consumes and carries no attempt: the text-log path cannot
/// populate one. [`TestRun::into_result`] converts, and the caller keeps the
/// attempt alongside.
#[derive(Debug, Clone, PartialEq)]
pub struct TestRun {
    pub suite: String,
    pub test: String,
    pub status: TestStatus,
    pub duration_seconds: Option<f64>,
    pub message: Option<String>,
    /// 1 for a test that ran once. Xcode's own numbering, taken from the
    /// `Repetition` node rather than inferred from position.
    pub attempt: u32,
}

impl TestRun {
    pub fn into_result(self) -> TestResult {
        TestResult {
            suite: self.suite,
            test: self.test,
            status: self.status,
            duration_seconds: self.duration_seconds,
            message: self.message,
            fingerprint: None,
        }
    }
}

/// One completed test run, as `Logs/Test/LogStoreManifest.plist` records it.
///
/// The manifest is what makes a test run detectable without guessing. Xcode
/// writes one `.xcactivitylog` per ⌘U — when the *compile* finishes — and then
/// never touches it again; the test run that follows produces only an
/// `.xcresult`, measured at 70–92 seconds later on a real project. So the
/// build and its results are two events, and nothing in the build log says
/// results are coming.
///
/// The manifest closes that gap. Each entry names the bundle *and* the build
/// log it belongs to, so results attach to the right build by Xcode's own
/// statement rather than by matching timestamps.
#[derive(Debug, Clone, PartialEq)]
pub struct ManifestEntry {
    /// The `.xcresult` directory name, relative to `Logs/Test/`.
    pub file_name: String,
    /// The `.xcactivitylog` this run built from, without its extension — the
    /// same UUID the collector uses as a build key.
    pub activity_log_id: Option<String>,
    /// Xcode's count of failing tests, available without opening the bundle.
    pub test_failures: Option<u32>,
    /// `com.apple.dt.unit.cocoaUnitTest` for a unit-test run. Recorded rather
    /// than matched on: a UI-test run is still a test run, and filtering by a
    /// string Apple owns would silently drop results when it changes.
    pub domain_type: Option<String>,
}

/// Reads the completed test runs from a DerivedData directory's manifest.
///
/// Parsed with a small scanner rather than a plist crate: the file is flat XML
/// with the four keys below at a known nesting, and a dependency for that is
/// not worth the supply chain. A binary plist would defeat this — every
/// manifest observed is XML — so an unreadable file yields no entries rather
/// than an error, and the caller falls back to no results instead of failing.
pub fn manifest_entries(project_dir: impl AsRef<Path>) -> Vec<ManifestEntry> {
    let path = project_dir
        .as_ref()
        .join("Logs/Test/LogStoreManifest.plist");
    let Ok(xml) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    parse_manifest(&xml)
}

/// Split out so it is testable against captured XML.
pub fn parse_manifest(xml: &str) -> Vec<ManifestEntry> {
    let mut entries: Vec<ManifestEntry> = Vec::new();
    // Fields accumulate into these because plist dicts are serialised in
    // alphabetical key order: `auxiliaryLogUniqueIdentifier` and `domainType`
    // both precede `fileName`, so an entry cannot be created when its name is
    // seen and populated afterwards. Instead each entry is flushed when the
    // *next* one's first key arrives, and the last at end of input.
    let mut activity_log_id: Option<String> = None;
    let mut domain_type: Option<String> = None;
    let mut test_failures: Option<u32> = None;
    let mut file_name: Option<String> = None;
    let mut pending_key: Option<String> = None;
    // `totalNumberOfTestFailures` appears twice per entry: once under
    // `auxiliaryObservable` (the build's, always 0) and once under
    // `primaryObservable` (the test run's). Only the second is wanted, and it
    // is the one that follows the `primaryObservable` key.
    let mut in_primary = false;
    for line in xml.lines() {
        let line = line.trim();
        if let Some(key) = tag_value(line, "key") {
            match key {
                "primaryObservable" => in_primary = true,
                "auxiliaryObservable" => in_primary = false,
                _ => {}
            }
            pending_key = Some(key.to_owned());
            continue;
        }
        let Some(key) = pending_key.take() else {
            continue;
        };
        match key.as_str() {
            "auxiliaryLogUniqueIdentifier" => {
                // First key of an entry, so the previous entry is complete.
                if let Some(name) = file_name.take() {
                    entries.push(ManifestEntry {
                        file_name: name,
                        activity_log_id: activity_log_id.take(),
                        test_failures: test_failures.take(),
                        domain_type: domain_type.take(),
                    });
                }
                activity_log_id = tag_value(line, "string").map(str::to_owned);
            }
            "domainType" => domain_type = tag_value(line, "string").map(str::to_owned),
            "fileName" => file_name = tag_value(line, "string").map(str::to_owned),
            "totalNumberOfTestFailures" if in_primary => {
                test_failures = tag_value(line, "integer").and_then(|value| value.parse().ok());
            }
            _ => {}
        }
    }
    if let Some(name) = file_name {
        entries.push(ManifestEntry {
            file_name: name,
            activity_log_id,
            test_failures,
            domain_type,
        });
    }
    entries
}

/// The text inside `<tag>…</tag>` on a single line, if this line is that tag.
fn tag_value<'a>(line: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    line.strip_prefix(&open)?.strip_suffix(&close)
}

/// The newest `.xcresult` a project's DerivedData holds, if any.
///
/// Xcode.app writes one per test run into `Logs/Test/` without being asked,
/// named `Test-<Scheme>-<timestamp>.xcresult`. That is what makes ⌘U work the
/// same as `xcodebuild test`: the terminal needs `-resultBundlePath` to get a
/// bundle at a known location, but a UI run has already produced one, so
/// BuildLens looks rather than requiring the user to pass a path.
///
/// `project_dir` is a `<Project>-<hash>` DerivedData directory — the same root
/// the activity-log collector walks, so a caller that has resolved one for
/// build metrics can reuse it for tests.
///
/// Returns `Ok(None)` when the directory holds no bundles: a build-only run
/// writes none, which is normal and not an error.
pub fn newest_bundle(project_dir: impl AsRef<Path>) -> Result<Option<PathBuf>, XcresultError> {
    let logs = project_dir.as_ref().join("Logs/Test");
    let Ok(entries) = std::fs::read_dir(&logs) else {
        return Ok(None);
    };
    let mut bundles: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "xcresult")
        })
        .filter_map(|path| {
            let modified = path.metadata().and_then(|meta| meta.modified()).ok()?;
            Some((modified, path))
        })
        .collect();
    // Newest first. Sorted by mtime rather than by the timestamp in the name:
    // the name's format is Xcode's to change, and a bundle rewritten in place
    // by a rerun keeps its original name.
    bundles.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    Ok(bundles.into_iter().next().map(|(_, path)| path))
}

/// Every test run in the bundle, in tree order: a test's attempts stay
/// together and in Xcode's numbering.
pub fn test_runs(bundle: impl AsRef<Path>) -> Result<Vec<TestRun>, XcresultError> {
    let path = bundle.as_ref();
    let display = path.display().to_string();
    if !path.exists() {
        return Err(XcresultError::Missing { path: display });
    }
    if path
        .extension()
        .is_none_or(|extension| extension != "xcresult")
    {
        return Err(XcresultError::NotABundle { path: display });
    }
    let output = Command::new("xcrun")
        .args([
            "xcresulttool",
            "get",
            "test-results",
            "tests",
            "--compact",
            "--path",
        ])
        .arg(path)
        .output()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => XcresultError::ToolMissing,
            _ => XcresultError::ToolFailed {
                path: display.clone(),
                message: error.to_string(),
            },
        })?;
    if !output.status.success() {
        return Err(XcresultError::ToolFailed {
            path: display,
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    parse_report(&output.stdout)
}

/// The parsing half, split from the process call so it is testable against
/// captured JSON without an Xcode installation.
pub fn parse_report(json: &[u8]) -> Result<Vec<TestRun>, XcresultError> {
    let report: Report = serde_json::from_slice(json)?;
    let mut runs = Vec::new();
    for node in &report.test_nodes {
        collect(node, None, &mut runs);
    }
    Ok(runs)
}

#[derive(Debug, Deserialize)]
struct Report {
    #[serde(default, rename = "testNodes")]
    test_nodes: Vec<Node>,
}

/// One node of the report tree.
///
/// Untyped on `node_type` on purpose: the schema names sixteen kinds and adds
/// to them between Xcode releases, so an enum would turn a new node type into
/// a hard parse failure for the whole run. Matching the handful this crate
/// cares about and ignoring the rest degrades instead.
#[derive(Debug, Deserialize)]
struct Node {
    #[serde(default, rename = "nodeType")]
    node_type: String,
    #[serde(default)]
    name: String,
    #[serde(default, rename = "nodeIdentifier")]
    node_identifier: Option<String>,
    #[serde(default)]
    result: Option<String>,
    #[serde(default, rename = "durationInSeconds")]
    duration_in_seconds: Option<f64>,
    #[serde(default)]
    children: Vec<Node>,
}

/// Walks the tree, emitting one [`TestRun`] per actual run.
///
/// `suite` threads the nearest enclosing `Test Suite` name down to the leaves.
/// A test case nested in suites keeps the innermost, which is the class name a
/// developer recognises; the bundle and plan names above it are not suites.
fn collect(node: &Node, suite: Option<&str>, runs: &mut Vec<TestRun>) {
    match node.node_type.as_str() {
        "Test Suite" => {
            for child in &node.children {
                collect(child, Some(&node.name), runs);
            }
        }
        "Test Case" => {
            let suite = suite.unwrap_or("").to_owned();
            // Retries and parallel destinations both nest the real runs one
            // level down. Without them the case node *is* the run.
            let mut attempts: Vec<&Node> = node
                .children
                .iter()
                .filter(|child| matches!(child.node_type.as_str(), "Repetition" | "Test Case Run"))
                .collect();
            if attempts.is_empty() {
                runs.push(TestRun {
                    suite,
                    test: node.name.clone(),
                    status: status_of(node.result.as_deref()),
                    duration_seconds: node.duration_in_seconds,
                    message: failure_message(node),
                    attempt: 1,
                });
                return;
            }
            // Xcode emits these in order, but the number is what identifies an
            // attempt; sorting by it means a reordered report still numbers
            // the first run 1. Nodes without one keep their relative position.
            attempts.sort_by_key(|attempt| attempt_number(attempt).unwrap_or(u32::MAX));
            for (index, attempt) in attempts.iter().enumerate() {
                runs.push(TestRun {
                    suite: suite.clone(),
                    test: node.name.clone(),
                    status: status_of(attempt.result.as_deref()),
                    duration_seconds: attempt.duration_in_seconds,
                    message: failure_message(attempt),
                    // Falls back to position for a node carrying no
                    // identifier, so an attempt is never numbered 0.
                    attempt: attempt_number(attempt).unwrap_or(index as u32 + 1),
                });
            }
        }
        // Test Plan, Unit test bundle, UI test bundle, Device, and anything
        // added in a later Xcode: not results themselves, but suites and cases
        // hang below them.
        _ => {
            for child in &node.children {
                collect(child, suite, runs);
            }
        }
    }
}

fn attempt_number(node: &Node) -> Option<u32> {
    node.node_identifier.as_deref()?.parse().ok()
}

/// The first failure message under a node, which is where the assertion text
/// and its `file:line` live.
fn failure_message(node: &Node) -> Option<String> {
    node.children
        .iter()
        .find(|child| child.node_type == "Failure Message")
        .map(|child| child.name.clone())
}

/// Maps Xcode's verdict onto the three states BuildLens stores.
///
/// `Skipped` and `Expected Failure` are deliberately not failures: a skipped
/// test did not run, and an expected failure passing *is* the assertion. Both
/// map to `Passed` because [`TestStatus`] has no fourth state, which loses the
/// distinction — worth widening when the dashboard has somewhere to show it,
/// but never worth reporting a green build as red in the meantime.
///
/// `unknown`, and anything a later Xcode adds, becomes `Started`: the status
/// meaning "ran but never reported an outcome", which is how a crash already
/// appears. Guessing `Passed` for an unrecognised verdict would hide failures.
fn status_of(result: Option<&str>) -> TestStatus {
    match result {
        Some("Passed") | Some("Skipped") | Some("Expected Failure") => TestStatus::Passed,
        Some("Failed") => TestStatus::Failed,
        _ => TestStatus::Started,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real message Xcode produces while it is still writing a bundle,
    /// captured from a Kaizen run whose manifest entry landed four seconds
    /// before its `Info.plist`.
    #[test]
    fn an_unfinished_bundle_is_recognized_as_transient() {
        let error = XcresultError::ToolFailed {
            path: "/tmp/T.xcresult".into(),
            message: "Error: Failed to create a new result bundle reader, underlying error: \
                      Info.plist at /tmp/T.xcresult/Info.plist does not exist, the result bundle \
                      might be corrupted or the provided path is not a result bundle"
                .into(),
        };
        assert!(error.is_incomplete_bundle());
    }

    /// Every other failure must still be reported: this flag decides whether a
    /// message is printed, so a real fault silently classified as transient
    /// would never be seen.
    #[test]
    fn other_failures_are_not_treated_as_transient() {
        assert!(
            !XcresultError::ToolFailed {
                path: "/tmp/T.xcresult".into(),
                message: "Error: unexpected argument 'test-results'".into(),
            }
            .is_incomplete_bundle()
        );
        assert!(!XcresultError::ToolMissing.is_incomplete_bundle());
    }

    /// Captured from a real Xcode 26 run of a project with a deliberately
    /// flaky test, via `xcresulttool get test-results tests`. Trimmed to the
    /// nodes this crate reads.
    const REPORT: &str = include_str!("../fixtures/retry-report.json");

    fn runs() -> Vec<TestRun> {
        parse_report(REPORT.as_bytes()).expect("the captured report parses")
    }

    fn find<'a>(runs: &'a [TestRun], test: &str) -> Vec<&'a TestRun> {
        runs.iter().filter(|run| run.test == test).collect()
    }

    /// The bug that motivated this crate: the text-log parser recorded every
    /// one of these under a suite named "SwiftTesting", because Xcode 26's
    /// XCTest output matches the Swift Testing pattern.
    #[test]
    fn suites_and_tests_are_read_not_inferred() {
        let runs = runs();
        let flaky = find(&runs, "testFlakyNetworkCall()");
        assert!(!flaky.is_empty(), "the flaky test is present");
        assert_eq!(flaky[0].suite, "FlakyTests", "the real class name");
        assert!(
            runs.iter().all(|run| run.suite != "SwiftTesting"),
            "no XCTest may be attributed to Swift Testing"
        );
        assert!(runs.iter().any(|run| run.suite == "UnitTestsAppTests"));
    }

    /// Every attempt survives, numbered by Xcode rather than by position, and
    /// a fail-then-pass reads in that order.
    #[test]
    fn a_retried_test_keeps_every_attempt_in_order() {
        let runs = runs();
        let flaky = find(&runs, "testFlakyNetworkCall()");
        assert_eq!(flaky.len(), 2, "first run and its retry");
        assert_eq!(flaky[0].attempt, 1);
        assert_eq!(flaky[0].status, TestStatus::Failed);
        assert_eq!(flaky[1].attempt, 2);
        assert_eq!(flaky[1].status, TestStatus::Passed);
    }

    /// A test retried to exhaustion is broken, not flaky, and all three runs
    /// are recorded so the distinction is visible.
    #[test]
    fn a_test_that_never_passes_records_all_of_its_failures() {
        let broken = runs();
        let broken = find(&broken, "testAlwaysBroken()");
        assert_eq!(broken.len(), 3);
        assert!(broken.iter().all(|run| run.status == TestStatus::Failed));
        assert_eq!(
            broken.iter().map(|run| run.attempt).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    /// A test that ran once has no Repetition nodes at all; the case node is
    /// the run, and it is attempt 1 rather than attempt 0.
    #[test]
    fn a_test_that_ran_once_is_attempt_one() {
        let runs = runs();
        let stable = find(&runs, "testStable()");
        assert_eq!(stable.len(), 1);
        assert_eq!(stable[0].attempt, 1);
        assert_eq!(stable[0].status, TestStatus::Passed);
    }

    #[test]
    fn failure_messages_carry_their_source_location() {
        let runs = runs();
        let message = find(&runs, "testAlwaysBroken()")[0]
            .message
            .clone()
            .expect("a failure message");
        assert!(message.contains("FlakyTests.swift:34"), "got {message}");
    }

    #[test]
    fn durations_are_read_from_each_attempt() {
        let runs = runs();
        let flaky = find(&runs, "testFlakyNetworkCall()");
        assert!(
            flaky[0]
                .duration_seconds
                .is_some_and(|seconds| seconds > 0.0)
        );
        assert_ne!(flaky[0].duration_seconds, flaky[1].duration_seconds);
    }

    /// Skipped and expected failures are not failures. Reporting either as one
    /// turns a green build red.
    #[test]
    fn skipped_and_expected_failures_are_not_failures() {
        assert_eq!(status_of(Some("Skipped")), TestStatus::Passed);
        assert_eq!(status_of(Some("Expected Failure")), TestStatus::Passed);
    }

    /// A verdict from a later Xcode must not be guessed as a pass.
    #[test]
    fn an_unknown_verdict_is_never_treated_as_passing() {
        assert_eq!(status_of(Some("unknown")), TestStatus::Started);
        assert_eq!(
            status_of(Some("Something Apple Adds In 2027")),
            TestStatus::Started
        );
        assert_eq!(status_of(None), TestStatus::Started);
    }

    /// A node type this version does not know must not drop the tests beneath
    /// it: the walker recurses through anything it does not recognise.
    #[test]
    fn unknown_node_types_are_traversed_rather_than_rejected() {
        let json = br#"{"testNodes":[{"nodeType":"Something New","children":[
            {"nodeType":"Test Suite","name":"S","children":[
                {"nodeType":"Test Case","name":"t()","result":"Passed","durationInSeconds":0.5}]}]}]}"#;
        let runs = parse_report(json).expect("parses");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].suite, "S");
    }

    #[test]
    fn a_report_with_no_tests_is_empty_not_an_error() {
        assert!(parse_report(br#"{"testNodes":[]}"#).unwrap().is_empty());
        assert!(parse_report(br#"{}"#).unwrap().is_empty());
    }

    #[test]
    fn malformed_json_is_reported_as_such() {
        assert!(matches!(
            parse_report(b"not json"),
            Err(XcresultError::Malformed(_))
        ));
    }

    /// A DerivedData directory with no test run in it is the normal state
    /// after a plain build, not an error.
    #[test]
    fn a_project_with_no_test_run_yields_no_bundle() {
        assert!(
            newest_bundle("/tmp/definitely-not-a-derived-data-dir")
                .unwrap()
                .is_none()
        );
    }

    /// Xcode.app names its bundles `Test-<Scheme>-<timestamp>.xcresult` and
    /// keeps every run, so discovery has to pick the newest rather than the
    /// first the filesystem happens to return.
    #[test]
    fn discovery_picks_the_newest_bundle() {
        let root = std::env::temp_dir().join(format!("buildlens-xcr-{}", std::process::id()));
        let logs = root.join("Logs/Test");
        std::fs::create_dir_all(&logs).expect("temp dirs");
        for name in [
            "Test-App-2026.01.01_10-00-00.xcresult",
            "Test-App-2026.06.01_10-00-00.xcresult",
        ] {
            std::fs::create_dir_all(logs.join(name)).expect("bundle dir");
            // Touched in order, so the second is newer by mtime.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // A non-bundle sibling must be ignored rather than chosen.
        std::fs::write(logs.join("LogStoreManifest.plist"), b"x").expect("manifest");
        let found = newest_bundle(&root)
            .expect("discovery succeeds")
            .expect("a bundle");
        assert_eq!(
            found.file_name().unwrap().to_string_lossy(),
            "Test-App-2026.06.01_10-00-00.xcresult"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Captured from a real Xcode 26 DerivedData after three ⌘U runs.
    const MANIFEST: &str = include_str!("../fixtures/manifest.plist");

    /// The pairing this whole mechanism rests on: Xcode states which build log
    /// each test run belongs to, so results attach by its word rather than by
    /// matching timestamps against a build that finished 90 seconds earlier.
    #[test]
    fn the_manifest_pairs_each_bundle_with_its_build_log() {
        let entries = parse_manifest(MANIFEST);
        assert!(!entries.is_empty(), "the captured manifest has entries");
        for entry in &entries {
            assert!(entry.file_name.ends_with(".xcresult"));
            assert!(
                entry.activity_log_id.is_some(),
                "{} names no build log",
                entry.file_name
            );
        }
    }

    /// The failure count comes from `primaryObservable` — the test run's —
    /// never from `auxiliaryObservable`, which is the build's and is always 0.
    /// Reading the wrong one would report every red suite as green.
    #[test]
    fn failure_counts_come_from_the_test_run_not_the_build() {
        let entries = parse_manifest(MANIFEST);
        assert!(
            entries.iter().any(|entry| entry.test_failures == Some(2)),
            "the captured runs each had two failing tests, got {:?}",
            entries.iter().map(|e| e.test_failures).collect::<Vec<_>>()
        );
    }

    #[test]
    fn unit_test_runs_are_labelled_as_such() {
        assert!(
            parse_manifest(MANIFEST).iter().any(
                |entry| entry.domain_type.as_deref() == Some("com.apple.dt.unit.cocoaUnitTest")
            )
        );
    }

    /// A manifest that cannot be read is "no test runs", not a failure: a
    /// project that has only ever been built has no such file.
    #[test]
    fn an_absent_manifest_yields_no_entries() {
        assert!(manifest_entries("/tmp/definitely-not-derived-data").is_empty());
        assert!(parse_manifest("").is_empty());
    }

    #[test]
    fn a_path_that_is_not_a_bundle_is_rejected_before_running_anything() {
        assert!(matches!(
            test_runs("/tmp"),
            Err(XcresultError::NotABundle { .. })
        ));
        assert!(matches!(
            test_runs("/tmp/definitely-absent.xcresult"),
            Err(XcresultError::Missing { .. })
        ));
    }
}
