//! The top-level result of analyzing one build, and the comparison of that
//! result against history.

use crate::{
    BuildMetrics, DiagnosticSummary, FailureCluster, FlakyTestSummary, GitCorrelation,
    Intelligence, MetricRegression, TargetGraphSummary, TestCrash, TestDurationRegression,
    TestSummary, TimingSummary,
};
use serde::Serialize;
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Detail {
    Summary,
    #[default]
    Standard,
    Full,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct AnalyzeOptions {
    pub detail: Detail,
    pub no_ai: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct BuildMetadata {
    pub xcode_version: Option<String>,
    pub project: Option<PathBuf>,
    pub workspace: Option<PathBuf>,
    pub scheme: Option<String>,
    pub test_plan: Option<String>,
    pub destination: Option<String>,
    pub result_bundle_path: Option<PathBuf>,
    pub xcconfig_path: Option<PathBuf>,
    pub code_coverage_enabled: Option<bool>,
    pub disable_automatic_package_resolution: bool,
    pub sdk: Option<String>,
    pub platform: Option<String>,
    pub architecture: Option<String>,
    pub deployment_target: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PackageInfo {
    pub name: String,
    pub source: Option<String>,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TimelineEvent {
    pub phase: String,
    pub line: u64,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Investigation {
    pub primary_issue: Option<String>,
    pub next_steps: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct CollectedMetadata {
    pub entries: BTreeMap<String, String>,
    pub warnings: Vec<String>,
}

/// What a parsed log says about whether the build worked.
///
/// A closed set rather than a `String`, for the same reason as
/// [`crate::wire::BuildStatus`]. Note the meaning is weaker than Xcode's own
/// verdict: a text log rarely states one outright, so [`AnalysisStatus::Passed`]
/// means "nothing in this log said otherwise", not "Xcode reported success".
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisStatus {
    /// No failure was observed anywhere in the log.
    #[default]
    Passed,
    /// An error, failing test, or crash was observed.
    Failed,
}

impl AnalysisStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
        }
    }

    /// Records that a failure was seen. Once failed, always failed — a later
    /// passing test does not undo an earlier error.
    pub fn mark_failed(&mut self) {
        *self = Self::Failed;
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BuildAnalysis {
    pub schema_version: String,
    pub status: AnalysisStatus,
    pub build: BuildMetadata,
    pub packages: Vec<PackageInfo>,
    pub graph: TargetGraphSummary,
    pub diagnostics: DiagnosticSummary,
    pub tests: TestSummary,
    pub crashes: Vec<TestCrash>,
    pub failure_clusters: Vec<FailureCluster>,
    pub timings: TimingSummary,
    pub metrics: Option<BuildMetrics>,
    pub timeline: Vec<TimelineEvent>,
    pub investigation: Investigation,
    pub git: Option<GitCorrelation>,
    pub metadata: CollectedMetadata,
    pub intelligence: Option<Intelligence>,
}

impl Default for BuildAnalysis {
    fn default() -> Self {
        Self {
            schema_version: "3".into(),
            status: AnalysisStatus::default(),
            build: Default::default(),
            packages: vec![],
            graph: Default::default(),
            diagnostics: Default::default(),
            tests: Default::default(),
            crashes: vec![],
            failure_clusters: vec![],
            timings: Default::default(),
            metrics: None,
            timeline: vec![],
            investigation: Investigation {
                primary_issue: None,
                next_steps: vec![],
            },
            git: None,
            metadata: Default::default(),
            intelligence: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HistoryComparison {
    pub baseline_build_id: Option<i64>,
    pub new_warnings: Vec<String>,
    pub existing_warnings: usize,
    pub resolved_warnings: Vec<String>,
    pub new_failures: Vec<String>,
    pub known_failures: usize,
    pub test_duration_change_seconds: Option<f64>,
    pub new_packages: Vec<String>,
    pub removed_packages: Vec<String>,
    pub new_dependencies: Vec<String>,
    pub removed_dependencies: Vec<String>,
    pub slower_tests: Vec<TestDurationRegression>,
    pub flaky_tests: Vec<FlakyTestSummary>,
    pub metric_regressions: Vec<MetricRegression>,
    pub build_category_change: Option<(String, String)>,
}
