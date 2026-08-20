//! The shared vocabulary every BuildLens crate speaks.
//!
//! This crate holds data and the invariants of that data — nothing that
//! *interprets* a build. Analysis lives in the crate named for it
//! (`buildlens-graph`, `buildlens-intel`, …), which keeps the dependency
//! arrows pointing one way: everything depends on core, core depends on
//! nothing but serde.
//!
//! Types are grouped into modules by domain and re-exported flat, so callers
//! write `buildlens_core::BuildMetrics` without tracking which module it
//! lives in.

pub mod analysis;
pub mod diagnostics;
pub mod git;
pub mod graph;
pub mod intel;
pub mod metrics;
pub mod sourcepath;
/// Named `testing` rather than `tests` so it is never mistaken for a
/// `#[cfg(test)]` module — these are the shipped types describing test runs.
pub mod testing;
pub mod wire;

pub use analysis::{
    AnalysisStatus, AnalyzeOptions, BuildAnalysis, BuildMetadata, CollectedMetadata, Detail,
    HistoryComparison, Investigation, PackageInfo, TimelineEvent,
};
pub use diagnostics::{
    DiagnosticAggregate, DiagnosticCategory, DiagnosticExample, DiagnosticSeverity,
    DiagnosticSummary, Swift6Summary,
};
pub use git::{GitCorrelation, GitOwnership, LikelyRelated};
pub use graph::{TargetDependency, TargetGraphSummary, TargetNode};
pub use intel::{
    Advice, AdviceKind, Bottleneck, EvidenceChain, EvidenceLink, Intelligence, MatchKind,
    TargetImpact,
};
pub use metrics::{
    BuildCategory, BuildMetrics, BuildStepMetric, CacheMetrics, FileMetric, METRICS_SCHEMA_VERSION,
    MetricDiagnostic, MetricKind, MetricRegression, MetricsEnvironment, MetricsSourceKind,
    PhaseMetric, RegressionCaveat, RegressionConfidence, SwiftTimingKind, SwiftTimingMetric,
    TargetMetric, TargetTiming, TimingSummary,
};
pub use sourcepath::{matches_any, normalize_separators, same_file};
pub use testing::{
    CrashType, FailureCluster, FlakyTestSummary, TestCrash, TestDurationRegression, TestResult,
    TestStatus, TestSummary,
};
