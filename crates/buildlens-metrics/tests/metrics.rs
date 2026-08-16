use buildlens_core::{BuildCategory, Detail, MetricsSourceKind};
use buildlens_metrics::{analyze_bytes, analyze_file, analyze_text, redacted};

#[test]
fn parses_text_timing_summary() {
    let metrics = analyze_text("CompileSwiftSources (App) | 2.5 seconds\nLd (App) | 1.0 seconds\n");
    assert!(matches!(
        metrics.source_kind,
        MetricsSourceKind::XcodebuildText
    ));
    assert_eq!(metrics.phases.len(), 2);
    assert_eq!(metrics.targets.len(), 2);
}

#[test]
fn malformed_activity_log_becomes_warning() {
    let metrics = analyze_bytes(
        b"not an slf stream",
        Some("xcactivitylog"),
        Detail::Standard,
    )
    .unwrap();
    assert!(matches!(
        metrics.source_kind,
        MetricsSourceKind::Xcactivitylog
    ));
    assert!(!metrics.warnings.is_empty());
}

#[test]
fn parses_real_xcactivitylog_fixture() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/packagesbench.xcactivitylog"
    );
    let metrics = analyze_file(path, Detail::Standard).unwrap();
    assert!(matches!(
        metrics.source_kind,
        MetricsSourceKind::Xcactivitylog
    ));
    assert!(
        metrics.warnings.is_empty(),
        "expected clean parse, got warnings: {:?}",
        metrics.warnings
    );
    assert!(!metrics.targets.is_empty(), "expected targets");
    assert!(
        metrics.total_seconds.unwrap_or(0.0) > 0.0,
        "expected a build duration"
    );
    assert!(metrics.build_id.is_some());
    assert_ne!(metrics.category, BuildCategory::Unknown);
    let with_steps = metrics
        .targets
        .iter()
        .filter(|target| !target.steps.is_empty())
        .count();
    assert!(with_steps > 0, "expected at least one target with steps");
    assert!(
        !metrics.files.is_empty(),
        "expected per-file compile timings"
    );
}

/// Capping is normal operation, so it must not land in `warnings` — callers
/// read an empty `warnings` as "this log decoded cleanly". It must still be
/// reported, though: "50 files" from a build that compiled thousands is
/// misleading on its own.
#[test]
fn truncation_is_reported_without_being_a_warning() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/packagesbench.xcactivitylog"
    );
    let metrics = analyze_file(path, Detail::Standard).unwrap();
    assert!(
        metrics.warnings.is_empty(),
        "truncation must not be a warning: {:?}",
        metrics.warnings
    );
    assert!(
        !metrics.truncations.is_empty(),
        "this fixture is large enough to be capped"
    );
    assert!(
        metrics
            .truncations
            .iter()
            .any(|note| note.contains("per-file timings")),
        "expected a per-file cap note, got {:?}",
        metrics.truncations
    );
}

/// `--detail full` keeps everything, so nothing is capped and nothing is
/// reported as capped.
#[test]
fn full_detail_keeps_everything_and_reports_no_truncation() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/packagesbench.xcactivitylog"
    );
    let standard = analyze_file(path, Detail::Standard).unwrap();
    let full = analyze_file(path, Detail::Full).unwrap();
    assert!(full.truncations.is_empty(), "{:?}", full.truncations);
    assert!(
        full.files.len() > standard.files.len(),
        "full detail should keep more files than the capped run ({} vs {})",
        full.files.len(),
        standard.files.len()
    );
}

#[test]
fn redaction_scrubs_home_paths_from_fixture() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/packagesbench.xcactivitylog"
    );
    let raw = analyze_file(path, Detail::Standard).unwrap();
    // Guard against passing for the wrong reason: if the fixture carried no
    // home paths to begin with, this test would prove nothing.
    assert!(
        raw.files.iter().any(|file| file.file.contains("/Users/")),
        "fixture should contain home paths before redaction"
    );

    let metrics = redacted(raw, None);
    for file in &metrics.files {
        assert!(
            !file.file.contains("/Users/"),
            "unredacted home path: {}",
            file.file
        );
    }
    // Every field that can carry a path, not just `files`.
    for target in &metrics.targets {
        for step in &target.steps {
            assert!(
                !step.file.as_deref().is_some_and(|f| f.contains("/Users/")),
                "unredacted step path: {:?}",
                step.file
            );
            assert!(
                !step.fingerprint.contains("/Users/"),
                "unredacted fingerprint: {}",
                step.fingerprint
            );
        }
    }
    for timing in &metrics.swift_timings {
        assert!(!timing.file.contains("/Users/"), "{}", timing.file);
    }
    for diagnostic in &metrics.diagnostics {
        assert!(
            !diagnostic.message.contains("/Users/"),
            "unredacted path in message: {}",
            diagnostic.message
        );
    }
}

/// Text logs carry these warnings too, and used to be dropped entirely: the
/// extraction only ran on `.xcactivitylog` sections. Lines below are verbatim
/// from a real `-warn-long-function-bodies` / `-warn-long-expression-type-checking`
/// build.
#[test]
fn text_logs_yield_swift_timings() {
    use buildlens_core::SwiftTimingKind;
    let log = "\
/repo/Sources/ConcurrentMap.swift:6:25: warning: expression took 3ms to type-check (limit: 1ms)
/repo/Sources/NetworkService.swift:85:18: warning: instance method 'resolve(error:)' took 4ms to type-check (limit: 1ms)
note: unrelated line that must not match
";
    let metrics = analyze_text(log);
    assert_eq!(metrics.swift_timings.len(), 2);

    // A bare "expression" is the type-checking flag.
    let expression = &metrics.swift_timings[0];
    assert_eq!(expression.kind, SwiftTimingKind::TypeCheck);
    assert_eq!(expression.file, "/repo/Sources/ConcurrentMap.swift");
    assert_eq!((expression.line, expression.column), (6, 25));
    assert_eq!(expression.milliseconds, 3.0);
    assert!(expression.symbol.is_none());

    // A named symbol means -warn-long-function-bodies fired.
    let function = &metrics.swift_timings[1];
    assert_eq!(function.kind, SwiftTimingKind::FunctionBody);
    assert_eq!(function.symbol.as_deref(), Some("resolve(error:)"));
    assert_eq!(function.milliseconds, 4.0);
}
