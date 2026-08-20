//! Deterministic build intelligence: which targets a change touches, which
//! targets bottleneck the build, and evidence chains connecting changes,
//! timings, diagnostics, and test failures.
//!
//! Everything here is derived from data already gathered — no heuristics that
//! reach back out to the filesystem, and no wording that assigns
//! responsibility (see [`language`]).

mod advice;
mod bottleneck;
mod evidence;
mod impact;
mod language;

use buildlens_core::{BuildAnalysis, Intelligence, MetricRegression};
use buildlens_graph::TargetGraph;

/// Runs every analysis over one build.
///
/// The dependency graph is built once here and shared, rather than rebuilt per
/// query.
pub fn analyze(analysis: &BuildAnalysis, regressions: &[MetricRegression]) -> Intelligence {
    let graph = TargetGraph::new(&analysis.graph);
    let impacts = impact::impacts(analysis, &graph);
    let bottlenecks = bottleneck::bottlenecks(analysis, &graph);
    let chains = evidence::chains(analysis, &impacts, regressions);
    // Empty unless the project builds with the `-warn-long-*` frontend flags;
    // see `advice::advice`.
    let advice = advice::advice(analysis);
    Intelligence {
        impacts,
        bottlenecks,
        chains,
        advice,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use buildlens_core::{
        BuildCategory, BuildMetrics, CacheMetrics, DiagnosticAggregate, DiagnosticCategory,
        DiagnosticExample, DiagnosticSeverity, FileMetric, GitCorrelation, LikelyRelated,
        MetricKind, MetricsEnvironment, MetricsSourceKind, RegressionCaveat, RegressionConfidence,
        TargetDependency, TargetMetric, TargetNode, TestResult, TestStatus,
    };

    pub fn node(name: &str) -> TargetNode {
        TargetNode {
            name: name.into(),
            project: "P".into(),
        }
    }

    fn metrics() -> BuildMetrics {
        BuildMetrics {
            metrics_schema_version: 2,
            build_id: None,
            source_log: None,
            project: None,
            scheme: None,
            source_kind: MetricsSourceKind::Xcactivitylog,
            category: BuildCategory::Incremental,
            compiled_count: 10,
            total_seconds: Some(100.0),
            started_at: None,
            ended_at: None,
            phases: vec![],
            targets: vec![],
            files: vec![],
            swift_timings: vec![],
            environment: MetricsEnvironment::default(),
            cache: CacheMetrics {
                status: "cold".into(),
                hit_rate: Some(0.0),
            },
            warnings: vec![],
            truncations: vec![],
            replayed_steps: 0,
            error_count: 0,
            warning_count: 0,
            diagnostics: vec![],
            status: None,
        }
    }

    fn target_metric(name: &str, seconds: f64) -> TargetMetric {
        TargetMetric {
            fingerprint: format!("target:{name}"),
            name: name.into(),
            seconds,
            started_at: None,
            ended_at: None,
            fetched_from_cache: false,
            category: BuildCategory::Incremental,
            compiled_count: 1,
            steps: vec![],
        }
    }

    fn correlation(changed: &[&str]) -> GitCorrelation {
        GitCorrelation {
            base: "a".into(),
            head: "b".into(),
            changed_files: changed.iter().map(|file| (*file).to_owned()).collect(),
            likely_related: LikelyRelated::Yes,
            confidence: 80,
            evidence: vec![],
            failure_ownership: vec![],
            implementation_ownership: vec![],
            diagnostic_ownership: vec![],
        }
    }

    /// An analysis whose metrics carry the given per-file rows, and whose git
    /// correlation reports the given changed files.
    pub fn analysis_with_files(changed: &[&str], files: &[(&str, &str)]) -> BuildAnalysis {
        let mut analysis = BuildAnalysis {
            git: Some(correlation(changed)),
            ..Default::default()
        };
        let mut metrics = metrics();
        metrics.files = files
            .iter()
            .map(|(path, target)| FileMetric {
                file: (*path).to_owned(),
                seconds: 5.0,
                target: Some((*target).to_owned()),
                step_type: "swiftCompilation".into(),
                architecture: Some("arm64".into()),
                occurrences: 1,
            })
            .collect();
        analysis.metrics = Some(metrics);
        analysis
    }

    /// An analysis whose metrics carry the given target durations.
    pub fn analysis_with_targets(targets: &[(&str, f64)]) -> BuildAnalysis {
        let mut analysis = BuildAnalysis::default();
        let mut metrics = metrics();
        metrics.targets = targets
            .iter()
            .map(|(name, seconds)| target_metric(name, *seconds))
            .collect();
        analysis.metrics = Some(metrics);
        analysis
    }

    /// The end-to-end fixture: App depends on Core, a changed file compiles
    /// into Core, and a CoreTests test failed.
    fn fixture() -> BuildAnalysis {
        let mut analysis = analysis_with_files(
            &["Sources/Core/Thing.swift"],
            &[("/build/repo/Sources/Core/Thing.swift", "Core")],
        );
        analysis.graph.targets = vec![node("App"), node("Core")];
        analysis.graph.dependencies = vec![TargetDependency {
            from: node("App"),
            to: node("Core"),
        }];
        if let Some(metrics) = analysis.metrics.as_mut() {
            metrics.targets = vec![target_metric("Core", 42.0)];
        }
        analysis.tests.tests.push(TestResult {
            suite: "CoreTests".into(),
            test: "testThing".into(),
            status: TestStatus::Failed,
            duration_seconds: Some(0.1),
            message: None,
            fingerprint: None,
        });
        analysis
    }

    fn regression() -> MetricRegression {
        MetricRegression {
            metric_kind: MetricKind::Target,
            name: "Core".into(),
            previous_seconds: 30.0,
            current_seconds: 42.0,
            delta_seconds: 12.0,
            delta_percent: 40.0,
            confidence: RegressionConfidence::High,
            caveats: vec![],
            reason: "same category and comparable baseline".into(),
        }
    }

    fn diagnostic(file: &str) -> DiagnosticAggregate {
        DiagnosticAggregate {
            fingerprint: format!("d:{file}"),
            severity: DiagnosticSeverity::Warning,
            category: DiagnosticCategory::Deprecation,
            occurrences: 1,
            example: DiagnosticExample {
                file: Some(file.to_owned()),
                line: Some(1),
                column: None,
                message: "deprecated".into(),
                target: None,
            },
        }
    }

    #[test]
    fn a_changed_file_maps_to_its_target_and_downstream() {
        let intelligence = analyze(&fixture(), &[]);
        assert_eq!(intelligence.impacts.len(), 1);
        let impact = &intelligence.impacts[0];
        assert_eq!(impact.owning_target.as_deref(), Some("Core"));
        assert_eq!(impact.match_kind, buildlens_core::MatchKind::Suffix);
        assert_eq!(impact.downstream_targets, ["App"]);
    }

    #[test]
    fn an_unmatched_file_has_no_owner() {
        let mut analysis = fixture();
        analysis.git.as_mut().unwrap().changed_files = vec!["README.md".into()];
        let intelligence = analyze(&analysis, &[]);
        assert_eq!(intelligence.impacts[0].owning_target, None);
    }

    #[test]
    fn bottlenecks_are_scored_by_blocked_downstream() {
        let intelligence = analyze(&fixture(), &[]);
        assert_eq!(intelligence.bottlenecks[0].target, "Core");
        assert_eq!(intelligence.bottlenecks[0].blocked_downstream, 1);
        assert!((intelligence.bottlenecks[0].score - 84.0).abs() < 0.001);
    }

    #[test]
    fn an_evidence_chain_combines_signals() {
        let intelligence = analyze(&fixture(), &[regression()]);
        assert_eq!(intelligence.chains.len(), 1);
        let chain = &intelligence.chains[0];
        assert_eq!(chain.subject, "Core");
        // suffix impact (25) + regression high (25) + failing suite (15) = 65
        assert_eq!(chain.confidence, 65);
        assert_eq!(chain.links.len(), 3);
    }

    #[test]
    fn a_suspended_build_reduces_confidence() {
        let mut analysis = fixture();
        analysis
            .metadata
            .entries
            .insert("build.was_suspended".into(), "true".into());
        let intelligence = analyze(&analysis, &[regression()]);
        assert_eq!(intelligence.chains[0].confidence, 40); // 65 - 25
        assert!(
            intelligence.chains[0]
                .links
                .iter()
                .any(|link| link.kind == "counter_suspended")
        );
    }

    /// The environment-shift caveat used to be detected by substring-matching
    /// free prose, so it could never fire. It is now a typed caveat.
    #[test]
    fn an_environment_shift_caveat_reduces_confidence() {
        let mut regression = regression();
        regression.caveats = vec![RegressionCaveat::EnvironmentShifted];
        let intelligence = analyze(&fixture(), &[regression]);
        assert_eq!(intelligence.chains[0].confidence, 45); // 65 - 20
        assert!(
            intelligence.chains[0]
                .links
                .iter()
                .any(|link| link.kind == "counter_environment")
        );
    }

    #[test]
    fn an_unrelated_caveat_does_not_reduce_confidence() {
        let mut regression = regression();
        regression.caveats = vec![RegressionCaveat::ThinBaseline];
        assert_eq!(analyze(&fixture(), &[regression]).chains[0].confidence, 65);
    }

    #[test]
    fn a_low_confidence_regression_is_weighted_less_than_a_high_one() {
        let mut low = regression();
        low.confidence = RegressionConfidence::Low;
        // suffix (25) + low regression (5) + failing suite (15) = 45
        assert_eq!(analyze(&fixture(), &[low]).chains[0].confidence, 45);
    }

    #[test]
    fn a_regression_on_another_metric_kind_is_ignored() {
        let mut phase = regression();
        phase.metric_kind = MetricKind::Phase;
        // Without the regression link: suffix (25) + failing suite (15) = 40
        assert_eq!(analyze(&fixture(), &[phase]).chains[0].confidence, 40);
    }

    #[test]
    fn a_single_weak_signal_is_dropped() {
        let mut analysis = fixture();
        analysis.tests.tests.clear();
        let intelligence = analyze(&analysis, &[]);
        // Only the suffix impact link (25) — below the two-positive-link floor.
        assert!(intelligence.chains.is_empty());
    }

    /// Strong evidence that is strongly contradicted must not be reported.
    #[test]
    fn overwhelming_counter_evidence_drops_the_chain() {
        let mut analysis = fixture();
        analysis
            .metadata
            .entries
            .insert("build.was_suspended".into(), "true".into());
        let mut regression = regression();
        regression.caveats = vec![RegressionCaveat::EnvironmentShifted];
        // 65 - 25 - 20 = 20, below MIN_CONFIDENCE.
        assert!(analyze(&analysis, &[regression]).chains.is_empty());
    }

    #[test]
    fn a_diagnostic_in_a_changed_file_adds_a_link() {
        let mut analysis = fixture();
        analysis.diagnostics.diagnostics = vec![diagnostic("/build/repo/Sources/Core/Thing.swift")];
        let intelligence = analyze(&analysis, &[]);
        // suffix (25) + diagnostic (20) + failing suite (15) = 60
        assert_eq!(intelligence.chains[0].confidence, 60);
        assert!(
            intelligence.chains[0]
                .links
                .iter()
                .any(|link| link.kind == "diagnostic")
        );
    }

    #[test]
    fn a_diagnostic_outside_the_changed_files_is_ignored() {
        let mut analysis = fixture();
        analysis.diagnostics.diagnostics = vec![diagnostic("/build/repo/Vendor/Other.swift")];
        // suffix (25) + failing suite (15) = 40, no diagnostic link.
        assert_eq!(analyze(&analysis, &[]).chains[0].confidence, 40);
    }

    /// A suite named exactly `Tests` used to match every target, inflating
    /// every chain's confidence.
    #[test]
    fn a_generically_named_suite_does_not_attach_to_every_target() {
        let mut analysis = fixture();
        analysis.tests.tests[0].suite = "Tests".into();
        // Without the failing-suite link only the suffix impact (25) remains,
        // which is a single positive link and so drops the chain.
        assert!(analyze(&analysis, &[]).chains.is_empty());
    }

    #[test]
    fn an_empty_analysis_produces_nothing() {
        let intelligence = analyze(&BuildAnalysis::default(), &[]);
        assert!(intelligence.impacts.is_empty());
        assert!(intelligence.bottlenecks.is_empty());
        assert!(intelligence.chains.is_empty());
    }

    #[test]
    fn output_is_deterministic() {
        let first = serde_json::to_string(&analyze(&fixture(), &[regression()])).unwrap();
        let second = serde_json::to_string(&analyze(&fixture(), &[regression()])).unwrap();
        assert_eq!(first, second);
    }

    /// Chain order must not depend on how the underlying maps iterate.
    #[test]
    fn equal_confidence_chains_are_ordered_by_subject() {
        let mut analysis = fixture();
        // Give App its own changed file so both targets have chains.
        analysis.git.as_mut().unwrap().changed_files = vec![
            "Sources/Core/Thing.swift".into(),
            "Sources/App/Main.swift".into(),
        ];
        if let Some(metrics) = analysis.metrics.as_mut() {
            metrics.files.push(FileMetric {
                file: "/build/repo/Sources/App/Main.swift".into(),
                seconds: 5.0,
                target: Some("App".into()),
                step_type: "swiftCompilation".into(),
                architecture: Some("arm64".into()),
                occurrences: 1,
            });
        }
        analysis.tests.tests.push(TestResult {
            suite: "AppTests".into(),
            test: "testApp".into(),
            status: TestStatus::Failed,
            duration_seconds: Some(0.1),
            message: None,
            fingerprint: None,
        });
        let chains = analyze(&analysis, &[]).chains;
        assert_eq!(chains.len(), 2);
        let subjects: Vec<&str> = chains.iter().map(|chain| chain.subject.as_str()).collect();
        assert_eq!(subjects, ["App", "Core"]);
    }
}
