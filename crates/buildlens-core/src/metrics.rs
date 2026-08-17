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

/// Which kind of log the metrics were decoded from.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MetricsSourceKind {
    XcodebuildText,
    Xcactivitylog,
}

impl MetricsSourceKind {
    /// Stable spelling for display; must match the serde renaming so a report
    /// and the JSON of the same run do not disagree on the source's name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::XcodebuildText => "xcodebuild_text",
            Self::Xcactivitylog => "xcactivitylog",
        }
    }
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
    /// Whether this step ran during the build being measured, rather than
    /// being replayed from an earlier one.
    ///
    /// Xcode re-emits the whole build graph in an incremental build's log,
    /// including steps it did not run, carrying their **original durations and
    /// timestamps**. Measured on a real project: a 21.9-second rebuild logged
    /// 3,888 steps of which 3,850 had started before that build began, and
    /// claimed 2,007 Swift compilations totalling 1,723 seconds.
    ///
    /// Nothing else distinguishes them — `fetched_from_cache` is false on a
    /// replayed step, because Xcode did not fetch it from anywhere. Treating
    /// "not fetched from cache" as "compiled" is what made every incremental
    /// build report as `clean` with its full clean-build file count. XCMetrics
    /// classifies builds the same way and has the same blind spot.
    ///
    /// `true` for a step with no usable timestamps: a step that cannot be
    /// placed is counted as real, so an unreadable log under-reports nothing.
    pub executed: bool,
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

fn is_zero(value: &usize) -> bool { *value == 0 }

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
    /// The scheme this build ran, from the activity log's preparation section
    /// (`Workspace X | Scheme Y | Destination Z`). `None` for text logs and
    /// for logs that never named one.
    pub scheme: Option<String>,
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
    /// Problems encountered while decoding. A non-empty list means the result
    /// may be wrong or incomplete through no choice of ours.
    pub warnings: Vec<String>,
    /// Ranked lists that were deliberately capped, and by how much.
    ///
    /// Kept apart from `warnings` because a cap is normal operation, not a
    /// problem: callers check `warnings.is_empty()` to mean "this log decoded
    /// cleanly", and folding truncation in there would make every large build
    /// look broken. Recorded rather than silent so a reader of "50 files"
    /// knows the build compiled nine thousand.
    pub truncations: Vec<String>,
    /// How many steps this log restated from an earlier build without re-running
    /// them. See [`BuildStepMetric::executed`].
    ///
    /// Its own field rather than a `truncations` entry: nothing was capped, and
    /// rather than `warnings`, because the log decoded perfectly — this is what
    /// Xcode wrote. It is a fact about the build worth surfacing, since a
    /// rebuild that replayed 3,850 of 3,888 steps did almost nothing, and that
    /// is the interesting part.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub replayed_steps: usize,
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
    /// (`localizedResultString`, e.g. "Build failed"), reduced to the bare
    /// verdict: "succeeded" / "failed" / "cancelled".
    ///
    /// `None` for text logs, which carry no such statement. Absent means
    /// unknown — never assume success.
    pub status: Option<String>,
}

/// The schema version [`BuildMetrics`] is currently written at.
///
/// Producers must use this rather than a literal: the value is also what
/// [`crate::wire::SUPPORTED_METRICS_SCHEMA`] is checked against, so a
/// hand-written `2` in one constructor and a bump here would silently diverge.
pub const METRICS_SCHEMA_VERSION: u32 = 2;

impl BuildMetrics {
    /// Whether this build produced a test bundle, and so is a build Xcode will
    /// follow with a test run.
    ///
    /// The activity log names no action: ⌘B and ⌘U produce the same kind of
    /// log, and nothing in it says "this was Test". What does distinguish them
    /// is the product — a test build signs, touches and copies `.xctest`
    /// bundles, and a plain build never mentions one. Measured on a real
    /// project: 84 such steps for ⌘U, zero for ⌘B.
    ///
    /// This exists for the collector's timing problem. Xcode writes the build
    /// log when the build finishes and the `.xcresult` only when the tests
    /// finish afterwards — a minute later in one observed run — so a collector
    /// that saves as soon as the log settles stores a test run with no tests.
    /// Knowing a test run is coming is what lets it wait, and only then.
    ///
    /// Matched on the step title rather than a target's product type, which
    /// the log does not carry. `.xctest` appears in titles like
    /// `Sign UnitTestsAppTests.xctest`.
    pub fn produces_test_bundle(&self) -> bool {
        self.targets
            .iter()
            .flat_map(|target| target.steps.iter())
            .any(|step| step.title.contains(".xctest"))
    }

    /// An empty result of the given kind, carrying any warnings explaining why
    /// it is empty.
    ///
    /// Exists so producers do not each spell out all twenty-odd fields; two
    /// such constructors had already drifted into near-copies.
    pub fn empty(source_kind: MetricsSourceKind, warnings: Vec<String>) -> Self {
        Self {
            metrics_schema_version: METRICS_SCHEMA_VERSION,
            build_id: None,
            source_log: None,
            project: None,
            scheme: None,
            source_kind,
            category: BuildCategory::Unknown,
            compiled_count: 0,
            total_seconds: None,
            started_at: None,
            ended_at: None,
            phases: Vec::new(),
            targets: Vec::new(),
            files: Vec::new(),
            swift_timings: Vec::new(),
            environment: MetricsEnvironment::default(),
            cache: CacheMetrics {
                status: "unknown".to_owned(),
                hit_rate: None,
            },
            warnings,
            truncations: Vec::new(),
            replayed_steps: 0,
            error_count: 0,
            warning_count: 0,
            diagnostics: Vec::new(),
            status: None,
        }
    }

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
        if self.timed_no_work() || self.decoded_partially() {
            return false;
        }
        self.total_seconds.is_some() && (!self.targets.is_empty() || !self.phases.is_empty())
    }

    /// True for a log whose decode aborted partway and left only a fragment.
    ///
    /// An activity log is parsed sequentially, so an unrecognized record stops
    /// the parse and keeps whatever preceded it. That fragment can still look
    /// plausible: an `IDELogDocumentLocation` 478 bytes into a 52KB log left a
    /// real build reporting one phase, no targets, no files and a duration —
    /// enough to satisfy every other check here. It was stored keyed by a
    /// content hash (no id had been read yet) and appeared in the dashboard as
    /// a second, near-empty row beside the same build collected from a log
    /// Xcode wrote differently.
    ///
    /// The parser records the abort in `warnings`, so a partial decode is
    /// detectable rather than a matter of guessing from the shape. Requiring
    /// evidence of compilation is what separates a fragment from a genuine
    /// phase-only log, which `-showBuildTimingSummary` text logs legitimately
    /// are.
    pub fn decoded_partially(&self) -> bool {
        const ABORTED: &[&str] = &["keeping partial result", "truncated", "unknown SLF"];
        self.targets.is_empty()
            && self.files.is_empty()
            && self.compiled_count == 0
            && self.warnings.iter().any(|warning| {
                ABORTED.iter().any(|marker| warning.contains(marker))
            })
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
            // Xcode 26's spelling for the same thing. A ⇧⌘K writes a log whose
            // only substantial phase is this one — 7.6 seconds of deleting, no
            // targets, no files — and without it here that log passed
            // `is_usable` on its phases alone and appeared beside real builds
            // as an `unknown` row that compiled nothing.
            "Prepare clean",
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

/// How much to trust a regression measurement.
///
/// A closed set rather than a `String`: consumers weight regressions by this,
/// and an unrecognized spelling silently fell through to the lowest weight,
/// so a typo would quietly discount a high-confidence regression.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RegressionConfidence {
    /// Same build category, comparable baseline, stable environment.
    High,
    /// Comparable, but with a caveat that widens the error bars.
    Medium,
    /// Too little history, or a baseline that is not really comparable.
    Low,
}

impl RegressionConfidence {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
}

/// What a regression was measured over.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
    Target,
    Phase,
    File,
    Build,
}

impl MetricKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Target => "target",
            Self::Phase => "phase",
            Self::File => "file",
            Self::Build => "build",
        }
    }
}

/// A caveat that undercuts a regression rather than explaining it.
///
/// These were previously detected by substring-matching the free-text
/// `reason`, a contract with no producer — so the signal was dead, and would
/// have silently stayed dead if the wording ever drifted.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum RegressionCaveat {
    /// The machine, Xcode version, or SDK differs from the baseline's, so the
    /// timing difference may not be attributable to the code.
    EnvironmentShifted,
    /// The baseline is a different build category, so the comparison is weak.
    CategoryMismatch,
    /// Too few historical samples for the baseline to mean much.
    ThinBaseline,
}

impl RegressionCaveat {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::EnvironmentShifted => "environment_shifted",
            Self::CategoryMismatch => "category_mismatch",
            Self::ThinBaseline => "thin_baseline",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricRegression {
    pub metric_kind: MetricKind,
    pub name: String,
    pub previous_seconds: f64,
    pub current_seconds: f64,
    pub delta_seconds: f64,
    pub delta_percent: f64,
    pub confidence: RegressionConfidence,
    /// Machine-readable caveats. Consumers must branch on these rather than
    /// on `reason`, which is prose for humans and free to be reworded.
    pub caveats: Vec<RegressionCaveat>,
    /// Human-readable explanation. Never parsed.
    pub reason: String,
}

#[cfg(test)]
mod usability_tests {
    use super::*;

    /// The shape a build has after its parse aborted: a duration, whatever
    /// phase happened to precede the failure, and nothing else.
    fn partial(warning: &str) -> BuildMetrics {
        let mut metrics =
            BuildMetrics::empty(MetricsSourceKind::Xcactivitylog, vec![warning.to_owned()]);
        metrics.total_seconds = Some(5.4);
        metrics.phases = vec![PhaseMetric {
            name: "Resolving package dependencies".into(),
            seconds: 5.4,
            started_at: None,
            ended_at: None,
        }];
        metrics
    }

    /// Xcode's Clean Build Folder writes a log like any build's, and Xcode 26
    /// names its work "Prepare clean" where earlier versions said "Cleaning".
    /// With only the older spelling recognised, a ⇧⌘K appeared in the
    /// dashboard as a 7.6-second `unknown` build that compiled nothing.
    #[test]
    fn an_xcode_26_clean_action_is_not_a_build() {
        let mut metrics = BuildMetrics::empty(MetricsSourceKind::Xcactivitylog, Vec::new());
        metrics.total_seconds = Some(7.63);
        // The exact phases observed, in order.
        metrics.phases = ["Prepare clean", "Create build operation",
                          "Send project description to build service", "Create build request"]
            .iter()
            .map(|name| PhaseMetric {
                name: (*name).into(),
                seconds: 0.1,
                started_at: None,
                ended_at: None,
            })
            .collect();
        assert!(metrics.timed_no_work(), "a clean action compiles nothing");
        assert!(!metrics.is_usable(), "and so is not stored as a build");
    }

    /// The bug this guards: an `IDELogDocumentLocation` 478 bytes into a real
    /// 52KB log left exactly this shape. It has a duration and a phase, so
    /// every other check passed, and it reached the dashboard as a near-empty
    /// row beside the same build read from a log Xcode wrote differently.
    #[test]
    fn a_fragment_from_an_aborted_parse_is_not_a_usable_build() {
        let metrics = partial("unknown SLF location class 'IDELogDocumentLocation' at byte 478; keeping partial result");
        assert!(metrics.decoded_partially());
        assert!(!metrics.is_usable(), "a one-phase fragment is not a build");
        // Not the clean-log case: that message would misdirect the reader.
        assert!(!metrics.timed_no_work());
    }

    /// A complete parse that merely warned about something must still count.
    /// Warnings are common and mostly benign; only the abort markers mean the
    /// decode stopped early.
    #[test]
    fn an_unrelated_warning_does_not_condemn_a_build() {
        let mut metrics = partial("redacted 3 absolute paths");
        metrics.targets = vec![TargetMetric {
            fingerprint: "f".into(),
            name: "App".into(),
            seconds: 5.0,
            started_at: None,
            ended_at: None,
            fetched_from_cache: false,
            category: BuildCategory::Clean,
            compiled_count: 12,
            steps: Vec::new(),
        }];
        metrics.compiled_count = 12;
        assert!(!metrics.decoded_partially());
        assert!(metrics.is_usable());
    }

    fn with_step_titles(titles: &[&str]) -> BuildMetrics {
        let mut metrics = BuildMetrics::empty(MetricsSourceKind::Xcactivitylog, Vec::new());
        metrics.targets = vec![TargetMetric {
            fingerprint: "f".into(),
            name: "App".into(),
            seconds: 1.0,
            started_at: None,
            ended_at: None,
            fetched_from_cache: false,
            category: BuildCategory::Clean,
            compiled_count: 1,
            steps: titles
                .iter()
                .map(|title| BuildStepMetric {
                    fingerprint: "s".into(),
                    step_type: "other".into(),
                    title: (*title).into(),
                    file: None,
                    architecture: None,
                    seconds: 0.1,
                    started_at: None,
                    ended_at: None,
                    fetched_from_cache: false,
                    executed: true,
                    warning_count: 0,
                    error_count: 0,
                })
                .collect(),
        }];
        metrics
    }

    /// ⌘U and ⌘B produce the same kind of log and nothing in it names the
    /// action, so the product is the signal: only a test build makes a
    /// `.xctest` bundle. Titles here are the real ones from an Xcode 26 run.
    #[test]
    fn a_build_that_signs_a_test_bundle_expects_tests() {
        let metrics = with_step_titles(&[
            "Compile App.swift",
            "Sign UnitTestsAppTests.xctest",
            "Touch UnitTestsAppTests.xctest",
        ]);
        assert!(metrics.produces_test_bundle());
    }

    /// A plain build must not wait for results that will never arrive.
    #[test]
    fn an_ordinary_build_expects_no_tests() {
        let metrics = with_step_titles(&["Compile App.swift", "Link App", "Copy Assets.car"]);
        assert!(!metrics.produces_test_bundle());
        assert!(
            !BuildMetrics::empty(MetricsSourceKind::Xcactivitylog, Vec::new())
                .produces_test_bundle(),
            "a build with no steps at all expects nothing"
        );
    }

    /// Evidence of compilation outranks the warning: a log that aborted near
    /// its end still measured real work, and discarding it would lose a build
    /// that was almost entirely readable.
    #[test]
    fn an_abort_after_real_work_still_yields_a_build() {
        let mut metrics = partial("log truncated; keeping partial result");
        metrics.compiled_count = 274;
        metrics.files = vec![FileMetric {
            file: "App.swift".into(),
            seconds: 1.0,
            target: Some("App".into()),
            step_type: "swiftCompilation".into(),
            architecture: None,
            occurrences: 1,
        }];
        assert!(!metrics.decoded_partially(), "real measurements were recorded");
        assert!(metrics.is_usable());
    }

    /// A phase-only text log is legitimate: `-showBuildTimingSummary` reports
    /// phases without targets, and that must not be mistaken for a fragment.
    #[test]
    fn a_phase_only_text_log_is_still_usable() {
        let mut metrics = BuildMetrics::empty(MetricsSourceKind::XcodebuildText, Vec::new());
        metrics.total_seconds = Some(42.0);
        metrics.phases = vec![PhaseMetric {
            name: "Compile Swift source files".into(),
            seconds: 42.0,
            started_at: None,
            ended_at: None,
        }];
        assert!(!metrics.decoded_partially());
        assert!(metrics.is_usable());
    }
}
