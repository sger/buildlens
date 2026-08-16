//! Parses `xcodebuild` text output into a [`BuildAnalysis`].
//!
//! The counterpart to `buildlens-metrics`, which reads Xcode's binary
//! `.xcactivitylog`. Text logs carry less structure, so most of this crate is
//! recognizing lines by shape and accumulating what they say.

mod graph;
mod metadata;
mod packages;
mod timeline;
mod timing;
mod xctest;

use buildlens_core::{
    AnalyzeOptions, BuildAnalysis, Detail, DiagnosticAggregate, DiagnosticSeverity, FailureCluster,
    TestResult, TestStatus,
};
use std::{collections::BTreeMap, io::BufRead, path::Path};
use thiserror::Error;
use timeline::{Phase, Timeline};

/// Slowest tests retained in the summary.
const MAX_SLOWEST_TESTS: usize = 20;

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("I/O while reading build log: {0}")]
    Io(#[from] std::io::Error),
    #[error("input contained no non-empty xcodebuild log lines")]
    EmptyInput,
    #[error(".xcresult bundles are not supported; export or provide the raw xcodebuild text log")]
    UnsupportedResultBundle,
}

pub fn analyze_file(
    path: impl AsRef<Path>,
    options: AnalyzeOptions,
) -> Result<BuildAnalysis, ParseError> {
    if path
        .as_ref()
        .extension()
        .is_some_and(|extension| extension == "xcresult")
    {
        return Err(ParseError::UnsupportedResultBundle);
    }
    let file = std::fs::File::open(path)?;
    analyze_reader(std::io::BufReader::new(file), options)
}

/// Everything accumulated while walking a log, kept together so the per-line
/// handlers can be separate functions rather than one 280-line loop.
#[derive(Default)]
struct Accumulator {
    analysis: BuildAnalysis,
    timeline: Timeline,
    diagnostics: BTreeMap<String, DiagnosticAggregate>,
    failures: BTreeMap<String, FailureCluster>,
    current_test: Option<String>,
    pending_failure: Option<String>,
    /// The first `N elapsed` seen. Later ones belong to subsequent
    /// destinations in a multi-destination run and would silently overwrite it.
    elapsed_seconds: Option<f64>,
}

pub fn analyze_reader<R: BufRead>(
    reader: R,
    options: AnalyzeOptions,
) -> Result<BuildAnalysis, ParseError> {
    let mut state = Accumulator::default();
    let mut line_number = 0u64;
    let mut saw_content = false;

    for raw in reader.lines() {
        line_number += 1;
        let line = raw?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        saw_content = true;
        handle_line(&mut state, trimmed, line_number, &options);
    }

    if !saw_content {
        return Err(ParseError::EmptyInput);
    }
    Ok(finish(state, &options))
}

/// Routes one line to whichever handler recognizes it.
///
/// Handlers run in order and the first that claims the line wins, so a line is
/// counted once. Each returns `true` when it consumed the line.
fn handle_line(state: &mut Accumulator, line: &str, line_number: u64, options: &AnalyzeOptions) {
    record_phase(state, line, line_number);

    if handle_graph(state, line, line_number)
        || handle_diagnostic(state, line, line_number)
        || handle_test(state, line, line_number, options)
        || handle_crash(state, line, line_number)
    {
        return;
    }
    handle_trailing(state, line);
}

/// Records the line's phase and any timing/metadata/package content.
///
/// These are not exclusive with each other or with the handlers below: a
/// timing line is also a build-timing phase marker.
fn record_phase(state: &mut Accumulator, line: &str, line_number: u64) {
    // A timing summary line is recognized either by the banner or by parsing
    // as a timing row. Both record the same phase; an earlier version wrote
    // this as an `if/else if` whose branches were identical, which meant the
    // banner line silently skipped `timing::parse`.
    let is_banner = line.contains("Build Timing Summary");
    let parsed_timing = timing::parse(line, &mut state.analysis.timings);
    if is_banner || parsed_timing {
        state.timeline.record(Phase::BuildTiming, line_number, line);
    }

    if let Some(phase) = timeline::classify(line) {
        state.timeline.record(phase, line_number, line);
    }

    if timeline::looks_like_metadata(line) && metadata::parse(line, &mut state.analysis.build) {
        state
            .timeline
            .record(Phase::BuildMetadata, line_number, line);
    }

    if packages::looks_like_package_line(line)
        && let Some(package) = packages::parse(line)
    {
        let already_known = state
            .analysis
            .packages
            .iter()
            .any(|known| known.name == package.name && known.version == package.version);
        if !already_known {
            state.analysis.packages.push(package);
        }
        state
            .timeline
            .record(Phase::PackageResolution, line_number, line);
    }
}

fn handle_graph(state: &mut Accumulator, line: &str, line_number: u64) -> bool {
    let looks_like_graph = line.contains("Target dependency graph")
        || line.contains("Target '")
        || line.contains("dependency on target");
    if !looks_like_graph || !graph::parse(line, &mut state.analysis.graph) {
        return false;
    }
    state
        .timeline
        .record(Phase::DependencyGraph, line_number, line);
    true
}

fn handle_diagnostic(state: &mut Accumulator, line: &str, line_number: u64) -> bool {
    let looks_like_diagnostic =
        line.contains("warning:") || line.contains("error:") || line.contains("fatal error:");
    if !looks_like_diagnostic {
        return false;
    }
    // Parsed once. The previous shape called `parse` in the guard and again in
    // the body, doing every regex match, classification and normalization
    // twice per diagnostic line.
    let Some(diagnostic) = buildlens_diagnostics::parse(line) else {
        return false;
    };

    match diagnostic.severity {
        DiagnosticSeverity::Warning => state.analysis.diagnostics.raw_warnings += 1,
        DiagnosticSeverity::Error | DiagnosticSeverity::Fatal => {
            state.analysis.diagnostics.raw_errors += 1;
            state.analysis.status.mark_failed();
        }
        DiagnosticSeverity::Note => {}
    }
    state
        .diagnostics
        .entry(diagnostic.fingerprint.clone())
        .and_modify(|existing| existing.occurrences += 1)
        .or_insert(diagnostic);

    if let Some(crash) = xctest::crash(line, state.current_test.clone()) {
        state.analysis.crashes.push(crash);
    }
    if let Some(message) = xctest::assertion(line) {
        state.pending_failure = Some(message);
    }
    if line.contains("fatal error:") {
        state.timeline.record(Phase::Crash, line_number, line);
    }
    true
}

fn handle_test(
    state: &mut Accumulator,
    line: &str,
    line_number: u64,
    options: &AnalyzeOptions,
) -> bool {
    if let Some(message) = xctest::assertion(line) {
        state.pending_failure = Some(message);
        return true;
    }

    if line.contains("Test Case")
        && line.contains(" started")
        && let Some(started) = xctest::started(line)
    {
        state.current_test = Some(started);
        state.pending_failure = None;
        return true;
    }

    if line.contains("Test Case")
        && (line.contains(" passed ") || line.contains(" failed "))
        && let Some(mut result) = xctest::result(line)
    {
        if result.status == TestStatus::Failed {
            state.analysis.status.mark_failed();
            result.message = state.pending_failure.take();
            if let Some(message) = &result.message {
                result.fingerprint =
                    Some(format!("XCTEST_ASSERT:{}", failure_fingerprint(message)));
            }
            cluster_failure(state, &result);
        }
        record_test(state, result, options);
        return true;
    }

    if (line.starts_with("Test '") || line.starts_with("Test case '"))
        && let Some(result) = xctest::swift_testing_result(line)
    {
        if result.status == TestStatus::Failed {
            state.analysis.status.mark_failed();
        }
        record_test(state, result, options);
        return true;
    }

    if line.contains("Restarting after unexpected exit") {
        state.analysis.tests.restarted += 1;
        state.timeline.record(Phase::TestRestart, line_number, line);
        return true;
    }
    false
}

fn handle_crash(state: &mut Accumulator, line: &str, line_number: u64) -> bool {
    let looks_like_crash = line.contains("Fatal error:")
        || line.contains("Unexpectedly found nil")
        || line.contains("signal SIG")
        || line.contains("EXC_CRASH");
    if !looks_like_crash {
        return false;
    }
    let Some(crash) = xctest::crash(line, state.current_test.clone()) else {
        return false;
    };
    state.analysis.status.mark_failed();
    state.analysis.crashes.push(crash);
    state.timeline.record(Phase::Crash, line_number, line);
    true
}

/// Lines that contribute a value without being "about" one thing.
fn handle_trailing(state: &mut Accumulator, line: &str) {
    if line.contains("elapsed")
        && state.elapsed_seconds.is_none()
        && let Some(seconds) = xctest::elapsed(line)
    {
        // First wins. Taking the last silently reported a later destination's
        // duration for the whole run.
        state.elapsed_seconds = Some(seconds);
    }
}

fn cluster_failure(state: &mut Accumulator, result: &TestResult) {
    let Some(fingerprint) = &result.fingerprint else {
        return;
    };
    let name = format!("{}:{}", result.suite, result.test);
    state
        .failures
        .entry(fingerprint.clone())
        .and_modify(|cluster| cluster.tests.push(name.clone()))
        .or_insert_with(|| FailureCluster {
            fingerprint: fingerprint.clone(),
            category: "test_failure".to_owned(),
            tests: vec![name],
            message: result
                .message
                .clone()
                .unwrap_or_else(|| "Test assertion failed".to_owned()),
        });
}

fn record_test(state: &mut Accumulator, result: TestResult, options: &AnalyzeOptions) {
    if options.detail == Detail::Full {
        state.analysis.tests.tests.push(result.clone());
    }
    state.analysis.tests.slowest.push(result);
}

/// Derives the totals and summaries that need the whole log.
fn finish(state: Accumulator, options: &AnalyzeOptions) -> BuildAnalysis {
    let Accumulator {
        mut analysis,
        timeline,
        diagnostics,
        failures,
        elapsed_seconds,
        ..
    } = state;

    analysis.timeline = timeline.into_events();
    analysis.timings.test_operation_seconds = elapsed_seconds;
    analysis.diagnostics.diagnostics = diagnostics.into_values().collect();
    analysis.diagnostics.unique_warnings = analysis
        .diagnostics
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == DiagnosticSeverity::Warning)
        .count();
    analysis.diagnostics.unique_errors = analysis
        .diagnostics
        .diagnostics
        .iter()
        .filter(|diagnostic| {
            matches!(
                diagnostic.severity,
                DiagnosticSeverity::Error | DiagnosticSeverity::Fatal
            )
        })
        .count();
    analysis.diagnostics.swift6 = buildlens_diagnostics::swift6(&analysis.diagnostics.diagnostics);
    analysis.failure_clusters = failures.into_values().collect();

    analysis.tests.crashed = analysis.crashes.len();
    // Counted before truncation, so the total reflects the whole run rather
    // than the slowest few.
    analysis.tests.total = analysis.tests.slowest.len();
    for test in &analysis.tests.slowest {
        match test.status {
            TestStatus::Passed => analysis.tests.passed += 1,
            TestStatus::Failed => analysis.tests.failed += 1,
            TestStatus::Started => {}
        }
    }
    analysis.tests.slowest.sort_by(|left, right| {
        right
            .duration_seconds
            .partial_cmp(&left.duration_seconds)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    analysis.tests.slowest.truncate(MAX_SLOWEST_TESTS);
    if options.detail != Detail::Full {
        analysis.tests.tests.clear();
    }

    graph::finalize(&mut analysis.graph);
    timing::finalize(&mut analysis.timings);
    analysis.investigation = investigation(&analysis);
    analysis
}

fn failure_fingerprint(message: &str) -> String {
    let suffix = message
        .split_once(": XCTAssert")
        .map(|(_, rest)| rest)
        .unwrap_or(message);
    buildlens_diagnostics::normalize(suffix)
}

fn investigation(analysis: &BuildAnalysis) -> buildlens_core::Investigation {
    let primary_issue = analysis
        .failure_clusters
        .first()
        .map(|cluster| cluster.message.clone())
        .or_else(|| analysis.crashes.first().map(|crash| crash.message.clone()))
        .or_else(|| {
            analysis
                .diagnostics
                .diagnostics
                .iter()
                .find(|diagnostic| {
                    matches!(
                        diagnostic.severity,
                        DiagnosticSeverity::Error | DiagnosticSeverity::Fatal
                    )
                })
                .map(|diagnostic| diagnostic.example.message.clone())
        });
    let mut next_steps = Vec::new();
    if !analysis.crashes.is_empty() {
        next_steps.push("Inspect the XCTest crash context and restart boundary.".to_owned());
    }
    if analysis.diagnostics.swift6.unique_blockers > 0 {
        next_steps.push("Review Swift 6 concurrency blockers by category and target.".to_owned());
    }
    if !analysis.graph.hotspots.is_empty() {
        next_steps.push("Review high fan-in targets as rebuild-impact candidates.".to_owned());
    }
    buildlens_core::Investigation {
        primary_issue,
        next_steps,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buildlens_core::AnalysisStatus;
    use std::io::Cursor;

    fn parse(log: &str) -> BuildAnalysis {
        analyze_reader(Cursor::new(log.to_owned()), AnalyzeOptions::default()).unwrap()
    }

    #[test]
    fn parses_fixture_and_aggregates_duplicates() {
        let analysis = analyze_reader(
            Cursor::new(include_str!("../../../fixtures/sample.log")),
            AnalyzeOptions::default(),
        )
        .unwrap();
        assert_eq!(analysis.schema_version, "3");
        assert_eq!(analysis.graph.declared_count, Some(3));
        assert_eq!(analysis.diagnostics.raw_warnings, 3);
        assert_eq!(analysis.diagnostics.unique_warnings, 2);
        assert_eq!(analysis.tests.failed, 1);
        assert_eq!(analysis.tests.crashed, 1);
        assert!(
            analysis
                .diagnostics
                .diagnostics
                .iter()
                .any(|d| d.example.file.as_deref() == Some("/Users/ci/Source.swift"))
        );
        assert!(!analysis.timeline.is_empty());
    }

    /// The fixture lists packages in the `+ name url version` form; they were
    /// being parsed, but the `Resolved package:` form silently was not.
    #[test]
    fn packages_from_the_fixture_are_collected() {
        let analysis = analyze_reader(
            Cursor::new(include_str!("../../../fixtures/sample.log")),
            AnalyzeOptions::default(),
        )
        .unwrap();
        let names: Vec<&str> = analysis
            .packages
            .iter()
            .map(|package| package.name.as_str())
            .collect();
        assert!(names.contains(&"Firebase"), "got {names:?}");
        assert!(names.contains(&"LocalKit"), "got {names:?}");
    }

    #[test]
    fn a_resolved_package_line_is_collected() {
        let analysis = parse(
            "Resolved package: SwiftSyntax: https://github.com/apple/swift-syntax @ 509.0.0\n",
        );
        assert_eq!(analysis.packages.len(), 1);
        assert_eq!(analysis.packages[0].name, "SwiftSyntax");
        assert_eq!(analysis.packages[0].version.as_deref(), Some("509.0.0"));
    }

    #[test]
    fn duplicate_package_lines_are_recorded_once() {
        let analysis = parse(
            "+ Firebase https://github.com/firebase/firebase-ios-sdk.git 12.17.0\n\
             + Firebase https://github.com/firebase/firebase-ios-sdk.git 12.17.0\n",
        );
        assert_eq!(analysis.packages.len(), 1);
    }

    #[test]
    fn detects_graph_cycles_and_swift_testing() {
        let cycle = analyze_reader(
            Cursor::new(include_str!("../../../fixtures/graph-cycle.log")),
            AnalyzeOptions::default(),
        )
        .unwrap();
        assert!(!cycle.graph.cycles.is_empty());
        let swift = parse("Test 'Feature loads' passed (0.42 seconds)\n");
        assert_eq!(swift.tests.total, 1);
        assert_eq!(swift.tests.passed, 1);
    }

    /// A cycle is one finding, however many members it has. The hand-rolled
    /// detector reported it once per rotation.
    #[test]
    fn a_cycle_is_reported_once_not_once_per_member() {
        let analysis = parse(
            "Target 'A' in project 'P'\n\
             \x20   ➜ Explicit dependency on target 'B' in project 'P'\n\
             Target 'B' in project 'P'\n\
             \x20   ➜ Explicit dependency on target 'C' in project 'P'\n\
             Target 'C' in project 'P'\n\
             \x20   ➜ Explicit dependency on target 'A' in project 'P'\n",
        );
        assert_eq!(analysis.graph.cycles.len(), 1);
        assert_eq!(analysis.graph.cycles[0].len(), 3);
    }

    #[test]
    fn an_acyclic_graph_reports_no_cycles() {
        let analysis = parse(
            "Target 'App' in project 'P'\n\
             \x20   ➜ Explicit dependency on target 'Core' in project 'P'\n\
             Target 'Core' in project 'P'\n",
        );
        assert!(analysis.graph.cycles.is_empty());
    }

    #[test]
    fn parses_failure_fixture_families_without_panicking() {
        for fixture in [
            include_str!("../../../fixtures/linker-error.log"),
            include_str!("../../../fixtures/package-resolution-error.log"),
            include_str!("../../../fixtures/codesign-failure.log"),
            include_str!("../../../fixtures/simulator-failure.log"),
            include_str!("../../../fixtures/xctest-memory-leak.log"),
        ] {
            let analysis = analyze_reader(Cursor::new(fixture), AnalyzeOptions::default()).unwrap();
            // Not just `is_ok`: an all-empty result would have passed that.
            assert!(
                !analysis.timeline.is_empty() || !analysis.diagnostics.diagnostics.is_empty(),
                "a failure fixture should yield diagnostics or a timeline"
            );
        }
    }

    #[test]
    fn aggregates_xcode_build_timing_summary() {
        let analysis = parse(
            "Build Timing Summary\n\
             CompileSwiftSources (App) | 2.50 seconds\n\
             Ld (App) | 1.25 seconds\n\
             CompileSwiftSources (Tests) | 0.75 seconds\n",
        );
        assert_eq!(
            analysis.timings.phases.get("CompileSwiftSources"),
            Some(&3.25)
        );
        assert_eq!(analysis.timings.targets.get("App"), Some(&3.75));
        assert_eq!(
            analysis
                .timings
                .slowest_targets
                .first()
                .map(|timing| timing.target.as_str()),
            Some("App")
        );
    }

    #[test]
    fn an_empty_log_is_an_error() {
        assert!(matches!(
            analyze_reader(Cursor::new("\n  \n\n"), AnalyzeOptions::default()),
            Err(ParseError::EmptyInput)
        ));
    }

    #[test]
    fn an_xcresult_bundle_is_rejected_with_guidance() {
        let error = analyze_file("/tmp/whatever.xcresult", AnalyzeOptions::default()).unwrap_err();
        assert!(matches!(error, ParseError::UnsupportedResultBundle));
        assert!(error.to_string().contains("xcodebuild text log"));
    }

    #[test]
    fn a_clean_log_passes_and_an_error_fails() {
        assert_eq!(
            parse("Test Case '-[S t]' passed (0.1 seconds)\n").status,
            AnalysisStatus::Passed
        );
        assert_eq!(
            parse("/a/B.swift:1:1: error: boom\n").status,
            AnalysisStatus::Failed
        );
    }

    /// A failure seen early must not be undone by later passing output.
    #[test]
    fn a_failure_is_sticky() {
        let analysis = parse(
            "/a/B.swift:1:1: error: boom\n\
             Test Case '-[S t]' passed (0.1 seconds)\n",
        );
        assert_eq!(analysis.status, AnalysisStatus::Failed);
    }

    /// `total` counts every test; `slowest` keeps only the worst few.
    #[test]
    fn every_test_is_counted_even_beyond_the_slowest_cap() {
        let log: String = (0..25)
            .map(|index| format!("Test Case '-[Suite test{index}]' passed (0.1 seconds)\n"))
            .collect();
        let analysis = parse(&log);
        assert_eq!(analysis.tests.total, 25);
        assert_eq!(analysis.tests.passed, 25);
        assert_eq!(analysis.tests.slowest.len(), MAX_SLOWEST_TESTS);
    }

    #[test]
    fn full_detail_keeps_every_test_and_standard_keeps_none() {
        let log =
            "Test Case '-[S a]' passed (0.1 seconds)\nTest Case '-[S b]' passed (0.2 seconds)\n";
        let standard = parse(log);
        assert!(standard.tests.tests.is_empty());
        let full = analyze_reader(
            Cursor::new(log),
            AnalyzeOptions {
                detail: Detail::Full,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(full.tests.tests.len(), 2);
    }

    /// The last `elapsed` used to overwrite the first, so a multi-destination
    /// run reported the wrong duration.
    #[test]
    fn the_first_elapsed_measurement_wins() {
        let analysis = parse("1.5 elapsed\n99.0 elapsed\n");
        assert_eq!(analysis.timings.test_operation_seconds, Some(1.5));
    }

    #[test]
    fn a_failing_test_carries_its_assertion_message() {
        let analysis = parse(
            "/a/T.swift:5: error: -[S t] : XCTAssertEqual failed: 1 is not 2\n\
             Test Case '-[S t]' failed (0.3 seconds)\n",
        );
        assert_eq!(analysis.tests.failed, 1);
        assert_eq!(analysis.failure_clusters.len(), 1);
        assert!(
            analysis.failure_clusters[0]
                .message
                .contains("XCTAssertEqual"),
            "got {:?}",
            analysis.failure_clusters[0].message
        );
    }

    /// Two tests failing the same assertion are one cluster.
    #[test]
    fn identical_failures_cluster_together() {
        let analysis = parse(
            "/a/T.swift:5: error: -[S one] : XCTAssertEqual failed: 1 is not 2\n\
             Test Case '-[S one]' failed (0.3 seconds)\n\
             /a/T.swift:9: error: -[S two] : XCTAssertEqual failed: 1 is not 2\n\
             Test Case '-[S two]' failed (0.4 seconds)\n",
        );
        assert_eq!(analysis.tests.failed, 2);
        assert_eq!(analysis.failure_clusters.len(), 1);
        assert_eq!(analysis.failure_clusters[0].tests.len(), 2);
    }

    #[test]
    fn a_crash_is_recorded_once_and_fails_the_build() {
        let analysis = parse("Fatal error: Unexpectedly found nil while unwrapping\n");
        assert_eq!(analysis.crashes.len(), 1);
        assert_eq!(analysis.tests.crashed, 1);
        assert_eq!(analysis.status, AnalysisStatus::Failed);
    }

    #[test]
    fn a_test_restart_is_counted() {
        let analysis = parse("Restarting after unexpected exit, crash, or test timeout\n");
        assert_eq!(analysis.tests.restarted, 1);
    }

    #[test]
    fn duplicate_diagnostics_aggregate_with_a_count() {
        let analysis = parse(
            "/a/B.swift:1:1: warning: 'old()' is deprecated\n\
             /a/B.swift:1:1: warning: 'old()' is deprecated\n",
        );
        assert_eq!(analysis.diagnostics.raw_warnings, 2);
        assert_eq!(analysis.diagnostics.unique_warnings, 1);
        assert_eq!(analysis.diagnostics.diagnostics[0].occurrences, 2);
    }

    #[test]
    fn the_investigation_names_the_primary_issue() {
        let analysis = parse("/a/B.swift:1:1: error: undefined symbol _main\n");
        assert!(
            analysis
                .investigation
                .primary_issue
                .as_deref()
                .is_some_and(|issue| issue.contains("undefined symbol")),
            "got {:?}",
            analysis.investigation.primary_issue
        );
    }

    /// One event per phase transition, not one per line.
    #[test]
    fn the_timeline_records_transitions_not_lines() {
        let log: String =
            std::iter::repeat_n("SwiftCompile normal arm64 Foo.swift\n", 50).collect();
        let analysis = parse(&log);
        let compilations = analysis
            .timeline
            .iter()
            .filter(|event| event.phase == "compilation")
            .count();
        assert_eq!(compilations, 1);
    }
}
