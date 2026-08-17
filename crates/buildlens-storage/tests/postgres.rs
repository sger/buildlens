//! Integration tests for the collector/dashboard store against a real Postgres.
//!
//! Skipped unless BUILDLENS_TEST_DATABASE_URL is set, so `cargo test` stays
//! green on machines without a database:
//!
//!   BUILDLENS_TEST_DATABASE_URL=postgres://buildlens:buildlens@localhost:5433/buildlens \
//!     cargo test -p buildlens-storage
//!
//! These exercise the SQL itself. Every query here has a parameter-typing or
//! aggregation trap that only a live server catches — `target_regressions`
//! shipped a 500 because one placeholder was used as both a row-number bound
//! and a LIMIT, which compiles fine and fails only against Postgres.

use buildlens_core::{
    BuildAnalysis, BuildCategory, BuildMetrics, BuildStepMetric, CacheMetrics, DiagnosticAggregate,
    DiagnosticCategory, DiagnosticExample, DiagnosticSeverity, FileMetric, MetricsEnvironment,
    MetricsSourceKind, PhaseMetric, RegressionCaveat, RegressionConfidence, SwiftTimingKind,
    SwiftTimingMetric, TargetMetric, TestResult, TestStatus,
};
use buildlens_storage::PostgresStore;
use postgres::{Client, NoTls};

fn database_url() -> Option<String> {
    match std::env::var("BUILDLENS_TEST_DATABASE_URL") {
        Ok(url) if !url.is_empty() => Some(url),
        // CI that is meant to have a database sets this, so a missing URL
        // fails loudly instead of reporting coverage that never ran.
        _ if std::env::var("BUILDLENS_REQUIRE_DATABASE").is_ok_and(|value| value == "1") => {
            panic!("BUILDLENS_REQUIRE_DATABASE=1 but BUILDLENS_TEST_DATABASE_URL is unset")
        }
        _ => None,
    }
}

/// Tests run in parallel, so each gets its own schema rather than sharing
/// `public` — concurrent CREATE TABLE in one schema deadlocks on pg_type
/// regardless of IF NOT EXISTS.
fn isolated_url(base: &str, name: &str) -> String {
    let mut client = Client::connect(base, NoTls).expect("connect to test database");
    client
        .batch_execute(&format!("DROP SCHEMA IF EXISTS {name} CASCADE; CREATE SCHEMA {name};"))
        .expect("create test schema");
    let separator = if base.contains('?') { '&' } else { '?' };
    format!("{base}{separator}options=-c%20search_path%3D{name}")
}

/// A store on an isolated schema, or None when no test database is configured.
///
/// Skipping is announced rather than silent: a run that reports 16 passing
/// tests while every one of them returned early is worse than no tests at all,
/// because it reads as coverage that does not exist.
fn connect(name: &str) -> Option<PostgresStore> {
    let Some(base) = database_url() else {
        eprintln!("skipping {name}: BUILDLENS_TEST_DATABASE_URL not set");
        return None;
    };
    Some(PostgresStore::connect(&isolated_url(&base, name)).expect("connect and migrate"))
}

fn metrics(id: &str, seconds: f64, started_at: f64) -> BuildMetrics {
    BuildMetrics {
        metrics_schema_version: 2,
        build_id: Some(id.into()),
        source_log: Some("<home>/DerivedData/App-x/Logs/Build/a.xcactivitylog".into()),
        project: Some("App".into()),
        scheme: None,
        source_kind: MetricsSourceKind::Xcactivitylog,
        category: BuildCategory::Clean,
        compiled_count: 10,
        total_seconds: Some(seconds),
        started_at: Some(started_at),
        ended_at: Some(started_at + seconds),
        phases: vec![PhaseMetric {
            name: "Prepare build".into(),
            seconds: seconds / 4.0,
            started_at: None,
            ended_at: None,
        }],
        targets: vec![TargetMetric {
            fingerprint: "target:Core".into(),
            name: "Core".into(),
            seconds: seconds / 2.0,
            started_at: None,
            ended_at: None,
            fetched_from_cache: false,
            category: BuildCategory::Clean,
            compiled_count: 10,
            steps: vec![],
        }],
        files: vec![FileMetric {
            file: "<repo>/Sources/Slow.swift".into(),
            seconds: seconds / 3.0,
            target: Some("Core".into()),
            step_type: "swift".into(),
            architecture: Some("arm64".into()),
            occurrences: 2,
        }],
        swift_timings: vec![SwiftTimingMetric {
            kind: SwiftTimingKind::FunctionBody,
            file: "<repo>/Sources/Slow.swift".into(),
            line: 42,
            column: 5,
            symbol: Some("slowFunction()".into()),
            milliseconds: 900.0,
            target: Some("Core".into()),
        }],
        environment: MetricsEnvironment {
            xcode_version: Some("16.2".into()),
            sdk: None,
            platform: Some("iOS Simulator".into()),
            architecture: Some("arm64".into()),
            machine: None,
        },
        cache: CacheMetrics { status: "cold".into(), hit_rate: Some(0.25) },
        warnings: vec![],
        truncations: vec![],
        error_count: 0,
        warning_count: 0,
        diagnostics: vec![],
        status: None,
    }
}

fn analysis(id: &str, seconds: f64, started_at: f64) -> BuildAnalysis {
    let mut analysis = BuildAnalysis {
        metrics: Some(metrics(id, seconds, started_at)),
        ..Default::default()
    };
    analysis.diagnostics.diagnostics = vec![DiagnosticAggregate {
        fingerprint: format!("warn:{id}"),
        severity: DiagnosticSeverity::Warning,
        category: DiagnosticCategory::SwiftConcurrency,
        occurrences: 3,
        example: DiagnosticExample {
            file: Some("<repo>/Sources/Slow.swift".into()),
            line: Some(42),
            column: Some(5),
            message: "non-sendable type crosses actor boundary".into(),
            target: Some("Core".into()),
        },
    }];
    analysis.tests.tests = vec![TestResult {
        suite: "CoreTests".into(),
        test: "testThing".into(),
        status: TestStatus::Failed,
        duration_seconds: Some(0.5),
        message: Some("boom".into()),
        fingerprint: None,
    }];
    analysis.metadata.entries.insert("git.branch".into(), "main".into());
    analysis.metadata.entries.insert("git.commit".into(), "abc123".into());
    analysis
}

/// A step naming a `.xctest` product, which is what marks a build as one that
/// will be followed by a test run. Real titles look like
/// `Sign UnitTestsAppTests.xctest`.
fn test_bundle_step() -> BuildStepMetric {
    BuildStepMetric {
        fingerprint: "sign".into(),
        step_type: "other".into(),
        title: "Sign AppTests.xctest".into(),
        file: None,
        architecture: None,
        seconds: 0.1,
        started_at: None,
        ended_at: None,
        fetched_from_cache: false,
        warning_count: 0,
        error_count: 0,
    }
}

fn items(value: &serde_json::Value) -> &Vec<serde_json::Value> {
    value["items"].as_array().expect("items array")
}

#[test]
fn saves_a_build_with_all_its_local_detail() {
    let Some(mut store) = connect("detail") else { return };
    assert!(store.save_analysis(&analysis("b1", 100.0, 1_700_000_000.0), "App", None, false).unwrap());

    let snapshot = store.build_snapshot("b1").unwrap().expect("build exists");
    assert_eq!(snapshot["project"], "App");
    assert_eq!(snapshot["category"], "clean");
    assert_eq!(snapshot["compiled_count"], 10);
    assert_eq!(snapshot["targets"].as_array().unwrap().len(), 1);
    assert_eq!(snapshot["phases"].as_array().unwrap().len(), 1);
    // Collected metadata must survive as a keyed object, not a flat list —
    // the detail page renders it by key.
    assert_eq!(snapshot["metadata"]["git.branch"], "main");
}

/// The detail page has to answer "why was *this* build slow, and why did it
/// fail" without leaving the page. The Performance tab averages these same
/// dimensions across many builds, which hides both answers, so the snapshot
/// must carry the per-build rows too.
#[test]
fn a_build_snapshot_carries_its_files_hotspots_diagnostics_and_tests() {
    let Some(mut store) = connect("detail_rows") else { return };
    assert!(store.save_analysis(&analysis("d1", 100.0, 1_700_000_000.0), "App", None, false).unwrap());

    let snapshot = store.build_snapshot("d1").unwrap().expect("build exists");

    let files = snapshot["files"].as_array().expect("files array");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["file"], "<repo>/Sources/Slow.swift");
    assert_eq!(files[0]["target"], "Core");

    // The type-check hotspots the -warn-long-* flags produce. Milliseconds,
    // not seconds: these are per-function, and rounding them to seconds would
    // collapse every hotspot to 0.
    let swift = snapshot["swift"].as_array().expect("swift array");
    assert_eq!(swift.len(), 1);
    assert_eq!(swift[0]["symbol"], "slowFunction()");
    assert_eq!(swift[0]["milliseconds"], 900.0);
    assert_eq!(swift[0]["line"], 42);

    let diagnostics = snapshot["diagnostics"].as_array().expect("diagnostics array");
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0]["severity"], "warning");
    assert_eq!(diagnostics[0]["occurrences"], 3);
    assert_eq!(diagnostics[0]["message"], "non-sendable type crosses actor boundary");

    let tests = snapshot["tests"].as_array().expect("tests array");
    assert_eq!(tests.len(), 1);
    assert_eq!(tests[0]["suite"], "CoreTests");
    assert_eq!(tests[0]["status"], "failed");
    assert_eq!(tests[0]["message"], "boom");

    // Totals are computed over every stored row, not over the 50 returned, so
    // a build with hundreds of tests still reports an honest failure count.
    assert_eq!(snapshot["test_totals"]["total"], 1);
    assert_eq!(snapshot["test_totals"]["failed"], 1);
}

/// A build with nothing recorded in these dimensions must return empty arrays
/// rather than null. The UI maps over them, and a null renders as a crash
/// instead of an empty state.
#[test]
fn absent_detail_rows_are_empty_arrays_not_null() {
    let Some(mut store) = connect("detail_empty") else { return };
    let mut bare = BuildAnalysis {
        metrics: Some(metrics("d2", 10.0, 1_700_000_000.0)),
        ..Default::default()
    };
    bare.metrics.as_mut().unwrap().files.clear();
    bare.metrics.as_mut().unwrap().swift_timings.clear();
    assert!(store.save_analysis(&bare, "App", None, false).unwrap());

    let snapshot = store.build_snapshot("d2").unwrap().expect("build exists");
    for key in ["files", "swift", "diagnostics", "tests"] {
        assert_eq!(snapshot[key].as_array().map(Vec::len), Some(0), "{key} should be an empty array");
    }
    assert_eq!(snapshot["test_totals"]["failed"], 0);
}

/// A failed build's error must not sit below a repeated warning: the reason
/// the build failed is the first thing the page has to answer.
#[test]
fn errors_are_ordered_ahead_of_more_frequent_warnings() {
    let Some(mut store) = connect("detail_severity") else { return };
    let mut mixed = analysis("d3", 100.0, 1_700_000_000.0);
    mixed.diagnostics.diagnostics = vec![
        DiagnosticAggregate {
            fingerprint: "warn:many".into(),
            severity: DiagnosticSeverity::Warning,
            category: DiagnosticCategory::SwiftConcurrency,
            occurrences: 99,
            example: DiagnosticExample {
                file: None, line: None, column: None,
                message: "a very repetitive warning".into(),
                target: None,
            },
        },
        DiagnosticAggregate {
            fingerprint: "err:one".into(),
            severity: DiagnosticSeverity::Error,
            category: DiagnosticCategory::Unknown,
            occurrences: 1,
            example: DiagnosticExample {
                file: None, line: None, column: None,
                message: "the single reason the build failed".into(),
                target: None,
            },
        },
    ];
    assert!(store.save_analysis(&mixed, "App", None, false).unwrap());

    let snapshot = store.build_snapshot("d3").unwrap().expect("build exists");
    let diagnostics = snapshot["diagnostics"].as_array().unwrap();
    assert_eq!(diagnostics[0]["severity"], "error", "the error must come first despite occurring 1× to the warning's 99×");
}

/// Re-collecting the same activity log must not double-count it. The whole
/// `collect --all` backfill depends on this being idempotent.
#[test]
fn storing_the_same_build_twice_is_a_no_op() {
    let Some(mut store) = connect("dedup") else { return };
    let build = analysis("dup", 50.0, 1_700_000_000.0);
    assert!(store.save_analysis(&build, "App", None, false).unwrap());
    assert!(!store.save_analysis(&build, "App", None, false).unwrap());
    assert_eq!(items(&store.projects().unwrap()).len(), 1);
    assert_eq!(items(&store.projects().unwrap())[0]["builds"], 1);
    assert_eq!(store.build_id_for_activity_log("dup").unwrap().as_deref(), Some("dup"));
    assert_eq!(store.build_id_for_activity_log("never").unwrap(), None);
}

/// A build with no usable metrics must be refused rather than stored as a
/// phantom that pollutes baselines and percentiles.
#[test]
fn unusable_metrics_are_rejected() {
    let Some(mut store) = connect("unusable") else { return };
    let mut broken = analysis("bad", 10.0, 1_700_000_000.0);
    if let Some(m) = broken.metrics.as_mut() {
        m.total_seconds = None;
        m.targets.clear();
        m.phases.clear();
        m.files.clear();
        m.swift_timings.clear();
    }
    assert!(store.save_analysis(&broken, "App", None, false).is_err());
    assert!(items(&store.projects().unwrap()).is_empty());
}

/// Every project-scoped query must actually filter. A query that ignores its
/// project argument mixes a 400s project into a 12s project's numbers, which
/// is the bug class this whole parameter exists to prevent.
#[test]
fn project_scoped_queries_filter_by_project() {
    let Some(mut store) = connect("scoping") else { return };
    store.save_analysis(&analysis("a1", 100.0, 1_700_000_000.0), "Alpha", None, false).unwrap();
    store.save_analysis(&analysis("b1", 10.0, 1_700_100_000.0), "Beta", None, false).unwrap();

    assert_eq!(items(&store.duration_trend_for(50, Some("Alpha")).unwrap()).len(), 1);
    assert_eq!(items(&store.target_trend(30, 10, Some("Alpha")).unwrap()).len(), 1);
    assert_eq!(items(&store.phase_trend(30, 10, Some("Alpha")).unwrap()).len(), 1);
    assert_eq!(items(&store.slowest_files(30, 10, Some("Alpha")).unwrap()).len(), 1);
    assert_eq!(items(&store.slowest_swift_timings(30, 10, Some("Alpha")).unwrap()).len(), 1);
    assert_eq!(items(&store.diagnostic_clusters(30, 10, Some("Alpha")).unwrap()).len(), 1);
    assert_eq!(items(&store.flaky_tests(30, 10, Some("Alpha")).unwrap()).len(), 1);
    assert_eq!(items(&store.environment_breakdown(30, Some("Alpha")).unwrap()).len(), 1);
    assert_eq!(items(&store.git_context(30, Some("Alpha")).unwrap()).len(), 1);
    // The daily view is a rolling window on CURRENT_DATE, and these fixtures
    // are stamped in the past, so the window has to be wide enough to reach
    // them — a 30-day window would correctly return nothing.
    assert_eq!(items(&store.daily_percentiles_for(3650, Some("Alpha")).unwrap()).len(), 1);
    assert_eq!(items(&store.diagnostic_trend_for(50, Some("Alpha")).unwrap()).len(), 1);

    // An unknown project must come back empty, never fall back to everything.
    for empty in [
        store.duration_trend_for(50, Some("Nope")).unwrap(),
        store.target_trend(30, 10, Some("Nope")).unwrap(),
        store.slowest_files(30, 10, Some("Nope")).unwrap(),
        store.git_context(30, Some("Nope")).unwrap(),
    ] {
        assert!(items(&empty).is_empty(), "unknown project leaked rows: {empty}");
    }

    // No filter means every project.
    assert_eq!(items(&store.duration_trend_for(50, None).unwrap()).len(), 2);
}

/// Percentiles over one or two builds describe the sample, not the project.
/// The dashboard shows "needs 5+ builds" off this flag.
#[test]
fn percentiles_report_whether_history_is_sufficient() {
    let Some(mut store) = connect("percentiles") else { return };
    for i in 0..3 {
        store
            .save_analysis(
                &analysis(&format!("p{i}"), 100.0 + i as f64, 1_700_000_000.0 + i as f64 * 86_400.0),
                "App",
                None,
                false,
            )
            .unwrap();
    }
    let few = store.duration_percentiles_for(100, Some("App")).unwrap();
    assert_eq!(few["builds"], 3);
    assert_eq!(few["enough_history"], false);

    for i in 3..6 {
        store
            .save_analysis(
                &analysis(&format!("p{i}"), 100.0 + i as f64, 1_700_000_000.0 + i as f64 * 86_400.0),
                "App",
                None,
                false,
            )
            .unwrap();
    }
    let enough = store.duration_percentiles_for(100, Some("App")).unwrap();
    assert_eq!(enough["builds"], 6);
    assert_eq!(enough["enough_history"], true);
    assert!(enough["p50"].as_f64().unwrap() > 0.0);
    assert!(enough["p95"].as_f64().unwrap() >= enough["p50"].as_f64().unwrap());

    // An empty scope must not error, just report nothing.
    let none = store.duration_percentiles_for(100, Some("Nope")).unwrap();
    assert_eq!(none["builds"], 0);
    assert_eq!(none["enough_history"], false);
    assert!(none["p50"].is_null());
}

/// `target_regressions` reuses its window bound in two places and takes a
/// separate LIMIT; getting that wrong is a runtime 500, not a compile error.
#[test]
fn target_regressions_runs_and_finds_a_slower_target() {
    let Some(mut store) = connect("regressions") else { return };
    // Older, fast builds first, then newer slow ones.
    for (i, seconds) in [(0, 20.0), (1, 20.0), (2, 80.0), (3, 80.0)] {
        store
            .save_analysis(
                &analysis(&format!("r{i}"), seconds, 1_700_000_000.0 + i as f64 * 86_400.0),
                "App",
                None,
                false,
            )
            .unwrap();
    }
    let found = store.target_regressions(2, 10, Some("App")).unwrap();
    let rows = items(&found);
    assert_eq!(rows.len(), 1, "expected Core to be flagged: {found}");
    assert_eq!(rows[0]["name"], "Core");
    assert!(rows[0]["delta_seconds"].as_f64().unwrap() > 0.0);
    assert_eq!(rows[0]["confidence"], "trend");

    // Stable timings must produce no regressions at all.
    let Some(mut steady) = connect("regressions_steady") else { return };
    for i in 0..4 {
        steady
            .save_analysis(
                &analysis(&format!("s{i}"), 50.0, 1_700_000_000.0 + i as f64 * 86_400.0),
                "App",
                None,
                false,
            )
            .unwrap();
    }
    assert!(items(&steady.target_regressions(2, 10, Some("App")).unwrap()).is_empty());
}

/// Per-file rows carry `occurrences`, which separates "this file is slow" from
/// "this file compiles once per architecture".
#[test]
fn slowest_files_reports_compilation_counts() {
    let Some(mut store) = connect("files") else { return };
    store.save_analysis(&analysis("f1", 90.0, 1_700_000_000.0), "App", None, false).unwrap();
    let rows = store.slowest_files(30, 10, Some("App")).unwrap();
    let first = &items(&rows)[0];
    assert!(first["file"].as_str().unwrap().contains("Slow.swift"));
    assert_eq!(first["target"], "Core");
    assert_eq!(first["compilations"], 2);
    assert!(first["avg_seconds"].as_f64().unwrap() > 0.0);
}

/// Swift timings only exist with the -warn-long-* flags; when present they
/// must group by file+line+kind so one hotspot is one row.
#[test]
fn swift_timings_group_by_location() {
    let Some(mut store) = connect("swift") else { return };
    store.save_analysis(&analysis("s1", 90.0, 1_700_000_000.0), "App", None, false).unwrap();
    store.save_analysis(&analysis("s2", 90.0, 1_700_100_000.0), "App", None, false).unwrap();
    let rows = items(&store.slowest_swift_timings(30, 10, Some("App")).unwrap()).clone();
    assert_eq!(rows.len(), 1, "same location across builds must be one row");
    assert_eq!(rows[0]["line"], 42);
    assert_eq!(rows[0]["kind"], "function_body");
    assert_eq!(rows[0]["observations"], 2);
    assert_eq!(rows[0]["symbol"], "slowFunction()");
}

/// A test that fails in one build and passes in another is flaky; one that
/// only ever fails is broken. The dashboard distinguishes them.
#[test]
fn flaky_tests_separate_mixed_outcomes_from_always_failing() {
    let Some(mut store) = connect("flaky") else { return };
    let mut failing = analysis("t1", 30.0, 1_700_000_000.0);
    store.save_analysis(&failing, "App", None, false).unwrap();

    failing = analysis("t2", 30.0, 1_700_100_000.0);
    if let Some(test) = failing.tests.tests.first_mut() {
        test.status = TestStatus::Passed;
        test.message = None;
    }
    store.save_analysis(&failing, "App", None, false).unwrap();

    let rows = items(&store.flaky_tests(30, 10, Some("App")).unwrap()).clone();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["test"], "testThing");
    assert_eq!(rows[0]["failed"], 1);
    assert_eq!(rows[0]["passed"], 1);
    assert_eq!(rows[0]["flaky"], true);
}

/// A ⌘U is two events on disk: Xcode writes the build log when the compile
/// finishes and the `.xcresult` 70–92 seconds later when the tests do. So
/// results always arrive after their build is stored, and must attach to a row
/// that already exists — and correct its status when a test failed.
#[test]
fn test_results_attach_to_a_build_already_in_history() {
    let Some(mut store) = connect("attach") else { return };
    let mut build = analysis("at1", 30.0, 1_700_000_000.0);
    // Stored as the collector stores it: a compile that succeeded, no tests.
    build.tests.tests.clear();
    if let Some(metrics) = build.metrics.as_mut() {
        metrics.status = Some("succeeded".into());
    }
    store.save_analysis(&build, "Attach", None, false).unwrap();
    assert_eq!(store.build_snapshot("at1").unwrap().unwrap()["status"], "succeeded");

    let failing = buildlens_core::TestResult {
        suite: "FlakyTests".into(),
        test: "testAlwaysBroken()".into(),
        status: TestStatus::Failed,
        duration_seconds: Some(0.004),
        message: Some("XCTFail".into()),
        fingerprint: None,
    };
    let passing = buildlens_core::TestResult {
        suite: "FlakyTests".into(),
        test: "testStable()".into(),
        status: TestStatus::Passed,
        ..failing.clone()
    };
    let attached = store
        .attach_tests("at1", &[(failing.clone(), 1), (passing, 1)])
        .unwrap();
    assert_eq!(attached, 2, "both results were new");

    let snapshot = store.build_snapshot("at1").unwrap().unwrap();
    assert_eq!(snapshot["status"], "failed", "a failing test fails the build");
    assert_eq!(snapshot["test_totals"]["total"], 2);

    // A watcher re-reading the same manifest must not double-count.
    assert_eq!(
        store.attach_tests("at1", &[(failing, 1)]).unwrap(),
        0,
        "attaching the same result twice inserts nothing"
    );
}

/// A test build is stored pending, not succeeded: its results do not exist
/// yet, and calling it green reads wrong for the ~90 seconds Xcode takes to
/// write them — and permanently, if they never arrive.
#[test]
fn a_test_build_with_no_results_yet_is_pending_not_succeeded() {
    let Some(mut store) = connect("pending") else { return };
    let mut build = analysis("p1", 30.0, 1_700_000_000.0);
    build.tests.tests.clear();
    if let Some(metrics) = build.metrics.as_mut() {
        metrics.status = Some("succeeded".into());
        // What marks this a test build: it produced a .xctest bundle.
        if let Some(target) = metrics.targets.first_mut() {
            target.steps.push(test_bundle_step());
        }
    }
    store.save_analysis(&build, "Pending", None, false).unwrap();
    assert_eq!(store.build_snapshot("p1").unwrap().unwrap()["status"], "pending_tests");
}

/// A plain build compiles nothing testable and must not be left pending
/// forever waiting for results that will never come.
#[test]
fn an_ordinary_build_is_never_left_pending() {
    let Some(mut store) = connect("pending_plain") else { return };
    let mut build = analysis("p2", 30.0, 1_700_000_000.0);
    build.tests.tests.clear();
    if let Some(metrics) = build.metrics.as_mut() {
        metrics.status = Some("succeeded".into());
    }
    store.save_analysis(&build, "PendingPlain", None, false).unwrap();
    assert_eq!(store.build_snapshot("p2").unwrap().unwrap()["status"], "succeeded");
}

/// Pending resolves both ways: green when the results are clean, red when they
/// are not. A build stuck at pending is one whose tests never reported.
#[test]
fn attaching_passing_results_resolves_a_pending_build() {
    let Some(mut store) = connect("pending_resolve") else { return };
    let mut build = analysis("p3", 30.0, 1_700_000_000.0);
    build.tests.tests.clear();
    if let Some(metrics) = build.metrics.as_mut() {
        metrics.status = Some("succeeded".into());
        if let Some(target) = metrics.targets.first_mut() {
            target.steps.push(test_bundle_step());
        }
    }
    store.save_analysis(&build, "PendingResolve", None, false).unwrap();
    assert_eq!(store.build_snapshot("p3").unwrap().unwrap()["status"], "pending_tests");

    let passing = buildlens_core::TestResult {
        suite: "S".into(),
        test: "t()".into(),
        status: TestStatus::Passed,
        duration_seconds: Some(0.1),
        message: None,
        fingerprint: None,
    };
    store.attach_tests("p3", &[(passing, 1)]).unwrap();
    assert_eq!(store.build_snapshot("p3").unwrap().unwrap()["status"], "succeeded");
}

/// Resolving a pending build must never overwrite a real verdict: a compile
/// that failed stays failed however well its tests went.
#[test]
fn resolving_never_overwrites_a_failed_compile() {
    let Some(mut store) = connect("pending_failed") else { return };
    let mut build = analysis("p4", 30.0, 1_700_000_000.0);
    build.tests.tests.clear();
    if let Some(metrics) = build.metrics.as_mut() {
        metrics.status = Some("failed".into());
    }
    store.save_analysis(&build, "PendingFailed", None, false).unwrap();
    let passing = buildlens_core::TestResult {
        suite: "S".into(),
        test: "t()".into(),
        status: TestStatus::Passed,
        duration_seconds: None,
        message: None,
        fingerprint: None,
    };
    store.attach_tests("p4", &[(passing, 1)]).unwrap();
    assert_eq!(
        store.build_snapshot("p4").unwrap().unwrap()["status"],
        "failed",
        "the compile failure stands"
    );
}

/// Results for a build that was never collected are dropped rather than
/// attached to nothing — the watcher may have started after that build ran.
#[test]
fn attaching_to_an_unknown_build_is_a_no_op() {
    let Some(mut store) = connect("attach_unknown") else { return };
    let test = buildlens_core::TestResult {
        suite: "S".into(),
        test: "t()".into(),
        status: TestStatus::Failed,
        duration_seconds: None,
        message: None,
        fingerprint: None,
    };
    assert_eq!(store.attach_tests("never-collected", &[(test, 1)]).unwrap(), 0);
}

/// A build whose tests failed must be stored as failed, even though the
/// activity log says the compile succeeded.
///
/// The regression: `WireBuild::from_metrics` takes its verdict from the
/// activity log, which reports on the compile. A ⌘U run with a red suite
/// compiled fine, so it was stored "succeeded" — the dashboard's failed-build
/// tile missed it entirely and `--fail-on failures` had nothing to gate on.
#[test]
fn failing_tests_fail_the_build_even_when_the_compile_succeeded() {
    let Some(mut store) = connect("status_tests") else { return };
    let mut build = analysis("s1", 30.0, 1_700_000_000.0);
    // What the activity log observed: the compile worked.
    if let Some(metrics) = build.metrics.as_mut() {
        metrics.status = Some("succeeded".into());
    }
    // What the log's test lines observed: one of them did not.
    build.status.mark_failed();
    assert!(build.tests.tests.iter().any(|t| t.status == TestStatus::Failed));
    store.save_analysis(&build, "StatusTests", None, false).unwrap();

    let snapshot = store.build_snapshot("s1").unwrap().expect("the build");
    assert_eq!(snapshot["status"], "failed", "a red suite is a red build");
}

/// The override runs one way only. An analysis that saw no failure must not
/// turn a compile failure green: the activity log observed something the text
/// log did not, and downgrading it would hide a broken build.
#[test]
fn a_clean_analysis_never_overrides_a_failed_compile() {
    let Some(mut store) = connect("status_compile") else { return };
    let mut build = analysis("s2", 30.0, 1_700_000_000.0);
    if let Some(metrics) = build.metrics.as_mut() {
        metrics.status = Some("failed".into());
    }
    build.status = buildlens_core::AnalysisStatus::Passed;
    build.tests.tests.clear();
    store.save_analysis(&build, "StatusCompile", None, false).unwrap();

    let snapshot = store.build_snapshot("s2").unwrap().expect("the build");
    assert_eq!(snapshot["status"], "failed", "the compile failure stands");
}

/// Xcode retries a failing test in place, so one build can report the same
/// test twice. Both runs must survive: keyed without `attempt`, the second
/// INSERT hit `ON CONFLICT DO NOTHING` and the retry vanished, recording a
/// fail-then-pass as a plain failure — the most common CI flakiness signal
/// there is, discarded one step before the database.
#[test]
fn a_test_retried_within_one_build_keeps_every_attempt() {
    let Some(mut store) = connect("retry") else { return };
    let mut build = analysis("r1", 30.0, 1_700_000_000.0);
    // Failed first, passed on the retry — same test, same build.
    let mut retry = build.tests.tests[0].clone();
    retry.status = TestStatus::Passed;
    retry.message = None;
    build.tests.tests.push(retry);
    store.save_analysis(&build, "Retry", None, false).unwrap();

    let rows = items(&store.flaky_tests(30, 10, Some("Retry")).unwrap()).clone();
    assert_eq!(rows.len(), 1, "the test is reported once, not once per attempt");
    assert_eq!(rows[0]["failed"], 1, "the failing attempt survived");
    assert_eq!(rows[0]["passed"], 1, "the passing retry survived");
    assert_eq!(rows[0]["flaky"], true);
    assert_eq!(
        rows[0]["retried_builds"], 1,
        "mixed outcomes inside one build is the strongest flakiness signal"
    );
}

/// The build page counts distinct tests, not attempts: a retry must not make
/// the suite look bigger, and a test that failed then passed is a pass.
#[test]
fn build_detail_counts_tests_not_attempts() {
    let Some(mut store) = connect("retry_detail") else { return };
    let mut build = analysis("r2", 30.0, 1_700_000_000.0);
    let mut retry = build.tests.tests[0].clone();
    retry.status = TestStatus::Passed;
    retry.message = None;
    build.tests.tests.push(retry);
    store.save_analysis(&build, "RetryDetail", None, false).unwrap();

    let snapshot = store.build_snapshot("r2").unwrap().expect("the build");
    assert_eq!(snapshot["test_totals"]["total"], 1, "one test, not two attempts");
    assert_eq!(snapshot["test_totals"]["failed"], 0, "its last attempt passed");
    let tests = snapshot["tests"].as_array().expect("tests");
    assert_eq!(tests.len(), 1, "one row per test");
    assert_eq!(tests[0]["status"], "passed", "the outcome that decided the build");
    assert_eq!(tests[0]["attempts"], 2, "and it took two runs to get there");
}

/// Cross-build mixing is still flaky, but it is not a within-build retry and
/// must not be counted as one: a source change between builds explains it.
#[test]
fn flakiness_across_builds_is_not_reported_as_a_retry() {
    let Some(mut store) = connect("retry_across") else { return };
    let failing = analysis("a1", 30.0, 1_700_000_000.0);
    store.save_analysis(&failing, "Across", None, false).unwrap();
    let mut passing = analysis("a2", 30.0, 1_700_100_000.0);
    passing.tests.tests[0].status = TestStatus::Passed;
    passing.tests.tests[0].message = None;
    store.save_analysis(&passing, "Across", None, false).unwrap();

    let rows = items(&store.flaky_tests(30, 10, Some("Across")).unwrap()).clone();
    assert_eq!(rows[0]["flaky"], true);
    assert_eq!(rows[0]["retried_builds"], 0, "no single build both failed and passed");
}

/// Diagnostics are deduplicated by fingerprint per build; the cluster view
/// ranks by how many builds each one appears in.
#[test]
fn diagnostic_clusters_rank_by_build_count() {
    let Some(mut store) = connect("diagnostics") else { return };
    for i in 0..2 {
        let mut build = analysis(&format!("d{i}"), 30.0, 1_700_000_000.0 + i as f64 * 86_400.0);
        // Same fingerprint across both builds so it clusters.
        build.diagnostics.diagnostics[0].fingerprint = "warn:shared".into();
        store.save_analysis(&build, "App", None, false).unwrap();
    }
    let rows = items(&store.diagnostic_clusters(30, 10, Some("App")).unwrap()).clone();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["builds"], 2);
    assert_eq!(rows[0]["occurrences"], 6); // 3 occurrences × 2 builds
    assert_eq!(rows[0]["severity"], "warning");
    assert_eq!(rows[0]["category"], "swift_concurrency");

    let trend = items(&store.diagnostic_trend_for(50, Some("App")).unwrap()).clone();
    assert_eq!(trend.len(), 2);
    assert_eq!(trend[0]["warnings"], 3);
    assert_eq!(trend[0]["errors"], 0);
}

/// Git metadata is stored as key/value pairs; the git panel pivots it back
/// into branch/commit/dirty columns.
#[test]
fn git_context_pivots_metadata_into_columns() {
    let Some(mut store) = connect("git") else { return };
    store.save_analysis(&analysis("g1", 30.0, 1_700_000_000.0), "App", None, false).unwrap();
    let rows = items(&store.git_context(30, Some("App")).unwrap()).clone();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["branch"], "main");
    assert_eq!(rows[0]["commit"], "abc123");
    assert_eq!(rows[0]["category"], "clean");
}

/// Anonymous attribution must never persist a machine id.
#[test]
fn anonymous_builds_store_no_machine_id() {
    let Some(mut store) = connect("anonymous") else { return };
    store
        .save_analysis(&analysis("a1", 30.0, 1_700_000_000.0), "App", Some("machine-1".into()), true)
        .unwrap();
    let snapshot = store.build_snapshot("a1").unwrap().expect("build exists");
    assert!(snapshot["machine_id"].is_null(), "anonymous build leaked a machine id");

    let Some(mut named) = connect("pseudonymous") else { return };
    named
        .save_analysis(&analysis("a2", 30.0, 1_700_000_000.0), "App", Some("machine-1".into()), false)
        .unwrap();
    assert_eq!(
        named.build_snapshot("a2").unwrap().expect("build exists")["machine_id"],
        "machine-1"
    );
}

/// The environment panel exists so a duration shift that lines up with an
/// Xcode upgrade reads as a setup change rather than a code regression.
#[test]
fn environment_breakdown_groups_by_toolchain() {
    let Some(mut store) = connect("environment") else { return };
    store.save_analysis(&analysis("e1", 30.0, 1_700_000_000.0), "App", None, false).unwrap();
    let mut newer = analysis("e2", 60.0, 1_700_100_000.0);
    if let Some(m) = newer.metrics.as_mut() {
        m.environment.xcode_version = Some("16.3".into());
    }
    store.save_analysis(&newer, "App", None, false).unwrap();

    let rows = items(&store.environment_breakdown(50, Some("App")).unwrap()).clone();
    assert_eq!(rows.len(), 2, "each Xcode version is its own row: {rows:?}");
    let versions: Vec<&str> =
        rows.iter().map(|r| r["xcode_version"].as_str().unwrap()).collect();
    assert!(versions.contains(&"16.2") && versions.contains(&"16.3"));
}

/// Day-partitioned percentiles are what make a week-over-week shift visible.
#[test]
fn daily_percentiles_group_by_calendar_day() {
    let Some(mut store) = connect("daily") else { return };
    // Two builds on one day, one on the next.
    store.save_analysis(&analysis("d1", 10.0, 1_700_000_000.0), "App", None, false).unwrap();
    store.save_analysis(&analysis("d2", 30.0, 1_700_003_600.0), "App", None, false).unwrap();
    store.save_analysis(&analysis("d3", 50.0, 1_700_086_400.0), "App", None, false).unwrap();

    let rows = items(&store.daily_percentiles_for(3650, Some("App")).unwrap()).clone();
    assert_eq!(rows.len(), 2, "expected two calendar days: {rows:?}");
    assert_eq!(rows[0]["builds"], 2);
    assert_eq!(rows[1]["builds"], 1);
    // Days must come back in ascending order so the chart reads left to right.
    assert!(rows[0]["day"].as_str().unwrap() < rows[1]["day"].as_str().unwrap());
}

/// The dashboard's build list and project list drive the whole overview.
#[test]
fn snapshot_and_projects_summarize_every_build() {
    let Some(mut store) = connect("snapshot") else { return };
    store.save_analysis(&analysis("s1", 10.0, 1_700_000_000.0), "Alpha", None, false).unwrap();
    store.save_analysis(&analysis("s2", 20.0, 1_700_100_000.0), "Beta", None, false).unwrap();
    store.save_analysis(&analysis("s3", 30.0, 1_700_200_000.0), "Beta", None, false).unwrap();

    let projects = items(&store.projects().unwrap()).clone();
    assert_eq!(projects.len(), 2);
    // Ordered by build count descending, so the busiest project leads.
    assert_eq!(projects[0]["project"], "Beta");
    assert_eq!(projects[0]["builds"], 2);

    let snapshot = store.dashboard_snapshot().unwrap();
    assert_eq!(snapshot["builds"].as_array().unwrap().len(), 3);
}

/// An unknown build id is a normal answer to a stale dashboard link, not a
/// failure: `None` lets a caller tell it apart from a real database error
/// without reading error text.
#[test]
fn an_unknown_build_snapshot_is_absent_not_an_error() {
    let Some(mut store) = connect("missing") else { return };
    assert!(store.build_snapshot("nope").unwrap().is_none());
}

/// The schema's `ON DELETE CASCADE` clauses are inert — this connection does
/// not enable `foreign_keys` — so prune deletes child rows explicitly. A new
/// build-scoped table missing from `BUILD_SCOPED_TABLES` would silently orphan
/// its rows on every prune, which this catches by comparing the list against
/// the schema itself.
#[test]
fn prune_covers_every_build_scoped_table() {
    let schema = include_str!("../../buildlens-server/schema.sql");
    let mut tables: Vec<&str> = schema
        .lines()
        .filter_map(|line| line.split_once("CREATE TABLE IF NOT EXISTS ")?.1.split_whitespace().next())
        .filter(|name| name.starts_with("build_") && !name.ends_with("_default"))
        .collect();
    tables.sort_unstable();
    tables.dedup();

    let covered = buildlens_storage::BUILD_SCOPED_TABLES;
    for table in tables {
        assert!(
            covered.contains(&table),
            "{table} is keyed by build_key but prune never deletes from it, \
             so pruning a build would orphan its rows"
        );
    }
}

/// `compare_to_baseline` had no test at all, which is how it shipped setting
/// the environment caveat only in prose. Consumers are told to branch on
/// `caveats`, so prose alone leaves the signal dead.
#[test]
fn a_shifted_environment_is_reported_as_a_machine_readable_caveat() {
    let Some(mut store) = connect("caveat") else { return };
    store
        .save_analysis(&analysis("base", 10.0, 1_700_000_000.0), "App", None, false)
        .unwrap();

    // Same project, a slower target, and a different Xcode.
    let mut newer = analysis("newer", 10.0, 1_700_003_600.0);
    if let Some(m) = newer.metrics.as_mut() {
        m.environment.xcode_version = Some("16.3".into());
        m.targets[0].seconds = 50.0;
    }
    store.save_analysis(&newer, "App", None, false).unwrap();

    let comparison = store
        .compare_to_baseline("newer")
        .unwrap()
        .expect("a baseline exists");
    assert!(comparison.environment_changed);
    let regression = comparison
        .regressions
        .first()
        .expect("a slower target is a regression");
    assert_eq!(regression.confidence, RegressionConfidence::Low);
    assert!(
        regression.caveats.contains(&RegressionCaveat::EnvironmentShifted),
        "the caveat must be machine-readable, not only prose: {:?}",
        regression.caveats
    );
}

/// With a stable environment the caveat must be absent, or every regression
/// would carry it and the signal would mean nothing.
#[test]
fn a_stable_environment_carries_no_caveat() {
    let Some(mut store) = connect("nocaveat") else { return };
    store
        .save_analysis(&analysis("base", 10.0, 1_700_000_000.0), "App", None, false)
        .unwrap();
    let mut newer = analysis("newer", 10.0, 1_700_003_600.0);
    if let Some(m) = newer.metrics.as_mut() {
        m.targets[0].seconds = 50.0;
    }
    store.save_analysis(&newer, "App", None, false).unwrap();

    let comparison = store.compare_to_baseline("newer").unwrap().expect("baseline");
    assert!(!comparison.environment_changed);
    let regression = comparison.regressions.first().expect("a regression");
    assert_eq!(regression.confidence, RegressionConfidence::High);
    assert!(regression.caveats.is_empty(), "{:?}", regression.caveats);
}

/// No earlier build means no claim, rather than a comparison against zero.
#[test]
fn a_build_with_no_earlier_build_has_no_baseline() {
    let Some(mut store) = connect("nobaseline") else { return };
    store
        .save_analysis(&analysis("only", 10.0, 1_700_000_000.0), "App", None, false)
        .unwrap();
    assert!(store.compare_to_baseline("only").unwrap().is_none());
    // And an unknown key is absent rather than an error.
    assert!(store.compare_to_baseline("nope").unwrap().is_none());
}

/// `day > CURRENT_DATE - $n` excluded the boundary, so days=N covered N-1.
/// A build exactly N days old must be inside an N-day window.
#[test]
fn the_daily_window_includes_its_boundary_day() {
    let Some(mut store) = connect("dailyedge") else { return };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as f64;
    store
        .save_analysis(
            &analysis("edge", 10.0, now - 7.0 * 86_400.0),
            "App",
            None,
            false,
        )
        .unwrap();

    let rows = items(&store.daily_percentiles_for(7, Some("App")).unwrap()).clone();
    assert_eq!(
        rows.len(),
        1,
        "a build exactly 7 days old must fall inside a 7-day window: {rows:?}"
    );
}
