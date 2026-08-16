//! Measurements taken from a single build: timings, targets, per-file work,
//! and the environment they were produced in.

use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Default)]
pub struct TimingSummary {
    pub test_operation_seconds: Option<f64>,
    pub build_seconds: Option<f64>,
    pub phases: BTreeMap<String, f64>,
    pub targets: BTreeMap<String, f64>,
    pub slowest_targets: Vec<TargetTiming>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TargetTiming {
    pub target: String,
    pub seconds: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricsSourceKind {
    XcodebuildText,
    Xcactivitylog,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct MetricsEnvironment {
    pub xcode_version: Option<String>,
    pub sdk: Option<String>,
    pub platform: Option<String>,
    pub architecture: Option<String>,
    pub machine: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct CacheMetrics {
    pub status: String,
    pub hit_rate: Option<f64>,
}

#[derive(Debug, Clone, Copy, Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BuildCategory {
    Clean,
    Incremental,
    Noop,
    Unknown,
}

impl BuildCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Incremental => "incremental",
            Self::Noop => "noop",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BuildStepMetric {
    pub fingerprint: String,
    pub step_type: String,
    pub title: String,
    pub file: Option<String>,
    /// Architecture this step compiled for, when the log names one. Xcode
    /// emits a step per architecture, so without this a file built for
    /// arm64 and x86_64 is indistinguishable from one slow compile.
    /// `None` for steps that are genuinely arch-independent (module
    /// emission, most copies) — never guessed.
    pub architecture: Option<String>,
    pub seconds: f64,
    pub started_at: Option<f64>,
    pub ended_at: Option<f64>,
    pub fetched_from_cache: bool,
    pub warning_count: usize,
    pub error_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct TargetMetric {
    pub fingerprint: String,
    pub name: String,
    pub seconds: f64,
    pub started_at: Option<f64>,
    pub ended_at: Option<f64>,
    pub fetched_from_cache: bool,
    pub category: BuildCategory,
    pub compiled_count: usize,
    pub steps: Vec<BuildStepMetric>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PhaseMetric {
    pub name: String,
    pub seconds: f64,
    pub started_at: Option<f64>,
    pub ended_at: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileMetric {
    pub file: String,
    pub seconds: f64,
    pub target: Option<String>,
    pub step_type: String,
    /// See [`BuildStepMetric::architecture`]. Per-file rows are keyed by
    /// architecture, so a multi-arch build reports one row per variant
    /// rather than collapsing to the slowest one.
    pub architecture: Option<String>,
    /// How many compilations produced this row. A file compiled once per
    /// architecture reports `occurrences > 1`, which is what separates
    /// "this file is slow" from "this file compiles four times".
    pub occurrences: u32,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SwiftTimingKind {
    FunctionBody,
    TypeCheck,
}

impl SwiftTimingKind {
    /// Stable string used as a storage key; must match the serde renaming.
    pub fn as_str(&self) -> &'static str {
        match self {
            SwiftTimingKind::FunctionBody => "function_body",
            SwiftTimingKind::TypeCheck => "type_check",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SwiftTimingMetric {
    pub kind: SwiftTimingKind,
    pub file: String,
    pub line: u32,
    pub column: u32,
    pub symbol: Option<String>,
    pub milliseconds: f64,
    pub target: Option<String>,
}

/// A diagnostic recorded by an `.xcactivitylog`, kept as raw parts so the CLI
/// can run it through the same classifier a text log's diagnostics go through.
/// The activity log stores message, severity and location separately, so there
/// is no formatted line to re-parse.
#[derive(Debug, Clone, Serialize)]
pub struct MetricDiagnostic {
    pub message: String,
    /// Xcode's own scale: 1 is a warning, 2 and above an error.
    pub severity: u64,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub target: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BuildMetrics {
    pub metrics_schema_version: u32,
    pub build_id: Option<String>,
    /// Redacted path of the log this was parsed from.
    pub source_log: Option<String>,
    /// Project name supplied explicitly (`--project`, or `$PROJECT_NAME` from
    /// the Xcode post-action). Takes precedence over inferring a name from the
    /// log's path, which is only reliable for logs still sitting in
    /// DerivedData. Deliberately optional rather than required, so `collect`
    /// stays zero-config.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub source_kind: MetricsSourceKind,
    pub category: BuildCategory,
    pub compiled_count: usize,
    pub total_seconds: Option<f64>,
    pub started_at: Option<f64>,
    pub ended_at: Option<f64>,
    pub phases: Vec<PhaseMetric>,
    pub targets: Vec<TargetMetric>,
    pub files: Vec<FileMetric>,
    pub swift_timings: Vec<SwiftTimingMetric>,
    pub environment: MetricsEnvironment,
    pub cache: CacheMetrics,
    pub warnings: Vec<String>,
    /// Diagnostics summed over every step. Reported alongside `status` rather
    /// than used to derive it: a build can fail with zero step-level errors
    /// (a cancelled build, a failure in a script phase), so counting errors
    /// and asking Xcode for its verdict are different questions.
    pub error_count: usize,
    pub warning_count: usize,
    /// Diagnostics as the activity log recorded them. Empty for text logs,
    /// whose diagnostics the text parser handles directly.
    pub diagnostics: Vec<MetricDiagnostic>,
    /// Xcode's own verdict for the build, from the activity log's main section
    /// (`localizedResultString`, e.g. "Build failed"), normalized the same way
    /// reduced to the bare verdict:
    /// "Clean" and trim, leaving "succeeded" / "failed" / "cancelled".
    ///
    /// `None` for text logs, which carry no such statement. Absent means
    /// unknown — never assume success.
    pub status: Option<String>,
}

impl BuildMetrics {
    /// True only when Xcode explicitly said the build did not succeed. An
    /// unknown status is not a failure.
    pub fn failed(&self) -> bool {
        self.status.as_deref().is_some_and(|s| s != "succeeded")
    }

    /// True when the log was decoded well enough to describe a build.
    /// A truncated or empty log yields warnings and no measurements; storing
    /// it would pollute history, baselines, and percentiles with a phantom
    /// build, so callers persist only complete metrics.
    ///
    /// "Measurement" deliberately means more than timings. A text log carries
    /// no phase or target durations unless the build ran with
    /// `-showBuildTimingSummary`, yet one containing 165k Swift type-check
    /// reports is obviously a real build — requiring `total_seconds` here
    /// rejected exactly that case and discarded every timing in it.
    pub fn is_usable(&self) -> bool {
        // Any per-file or per-function measurement proves real work happened,
        // whichever source produced it.
        if !self.files.is_empty() || !self.swift_timings.is_empty() {
            return true;
        }
        if self.timed_no_work() {
            return false;
        }
        self.total_seconds.is_some() && (!self.targets.is_empty() || !self.phases.is_empty())
    }

    /// True for a log that timed something other than a build: `xcodebuild
    /// clean` writes an activity log with a duration and a few build-service
    /// setup phases, but no targets, no files and nothing compiled.
    ///
    /// Recording those as builds put two 21-second entries next to 400-second
    /// clean builds of the same project, which would drag its median down once
    /// it crossed the percentile floor. A real build always compiles something
    /// or names at least one target.
    pub fn timed_no_work(&self) -> bool {
        const SETUP_ONLY_PHASES: &[&str] = &[
            "Send project description to build service",
            "Create build operation",
            "Create build request",
            "Build preparation",
            "Prepare packages",
            "Compute target dependency graph",
            "Cleaning",
        ];
        self.targets.is_empty()
            && self.files.is_empty()
            && self.compiled_count == 0
            && !self.phases.is_empty()
            && self.phases.iter().all(|phase| {
                SETUP_ONLY_PHASES
                    .iter()
                    .any(|known| phase.name.starts_with(known))
            })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricRegression {
    pub metric_kind: String,
    pub name: String,
    pub previous_seconds: f64,
    pub current_seconds: f64,
    pub delta_seconds: f64,
    pub delta_percent: f64,
    pub confidence: String,
    pub reason: String,
}
