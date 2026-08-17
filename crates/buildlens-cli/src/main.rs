mod collect;
mod push;

use anyhow::{Context, Result, bail};
use buildlens_core::{AnalyzeOptions, Detail};
use buildlens_graph::TargetGraph;
use buildlens_parser::analyze_file;
use buildlens_storage::PostgresStore;
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
#[derive(Parser)]
#[command(
    name = "buildlens",
    about = "Deterministic intelligence for xcodebuild logs",
    // Lets `buildlens --version` report what is installed, which the installer
    // echoes back and bug reports need.
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Analyze {
        log: Option<PathBuf>,
        #[arg(long,value_enum,default_value_t=Format::Terminal)]
        format: Format,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long,value_enum,default_value_t=DetailArg::Standard)]
        detail: DetailArg,
        #[arg(long)]
        no_ai: bool,
        #[arg(long)]
        fail_on: Option<Policy>,
        #[arg(long, requires = "head")]
        base: Option<String>,
        #[arg(long, requires = "base")]
        head: Option<String>,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Companion .xcactivitylog for the same build: adds precise target,
        /// phase, and per-file timings to a text log's diagnostics
        #[arg(long)]
        activity_log: Option<PathBuf>,
        /// Force the intelligence section (auto-enabled when metrics and git correlation are both present)
        #[arg(long)]
        intel: bool,
        /// History database used to load metric regressions for evidence chains
        #[arg(long)]
        db: Option<PathBuf>,
        #[command(flatten)]
        collect: CollectArgs,
    },
    Metrics {
        input: PathBuf,
        #[arg(long, value_enum, default_value_t=Format::Terminal)]
        format: Format,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long)]
        raw: bool,
        #[arg(long,value_enum,default_value_t=DetailArg::Standard)]
        detail: DetailArg,
    },
    Dashboard {
        /// PostgreSQL connection URL. A URL, not a path: a `PathBuf` here
        /// would pass non-UTF-8 bytes through `to_string_lossy` and silently
        /// connect with a string the user never typed.
        #[arg(long, default_value = "postgres://localhost/buildlens")]
        db: String,
        #[arg(long, default_value_t = 8787)]
        port: u16,
    },
    Why {
        target: String,
        log: PathBuf,
    },
    Warnings {
        log: PathBuf,
    },
    Failures {
        log: PathBuf,
    },
    Tests {
        log: PathBuf,
    },
    Graph {
        log: PathBuf,
    },
    History {
        #[command(subcommand)]
        command: HistoryCommand,
    },
    /// Find the newest Xcode activity log, wait until it is fully written, and save it
    Collect {
        /// DerivedData root, a project's DerivedData directory, or a Logs/Build directory.
        /// Defaults to `$BUILD_DIR`'s logs when Xcode set it — so a scheme post-action
        /// collects the build Xcode just ran — and to the DerivedData root otherwise.
        #[arg(long)]
        build_dir: Option<String>,
        /// Only consider DerivedData entries whose name starts with this prefix.
        /// Distinct from `--project`, which names the build recorded in
        /// history: this one chooses *which log to read*.
        #[arg(long, value_name = "PREFIX")]
        match_project: Option<String>,
        /// PostgreSQL connection URL. A URL, not a path: a `PathBuf` here
        /// would pass non-UTF-8 bytes through `to_string_lossy` and silently
        /// connect with a string the user never typed.
        #[arg(long, default_value = "postgres://localhost/buildlens")]
        db: String,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Seconds to wait for Xcode to finish writing the log
        #[arg(long, default_value_t = 30)]
        timeout: u64,
        /// Print the log that would be collected without saving it
        #[arg(long)]
        dry_run: bool,
        /// Import every activity log found, not just the newest — use once to
        /// backfill history from builds made before BuildLens was set up
        #[arg(long)]
        all: bool,
        /// Keep running and import each new build as Xcode writes it. Leave
        /// this in a terminal (or a launchd agent) instead of running collect
        /// after every build; re-seeing a log is a no-op, so nothing is
        /// double-counted.
        #[arg(long, conflicts_with_all = ["all", "dry_run"])]
        watch: bool,
        /// Seconds between scans in --watch mode
        #[arg(long, default_value_t = 5, value_name = "SECONDS")]
        watch_interval: u64,
        /// Team server base URL; without it nothing ever leaves this machine
        #[arg(long)]
        server: Option<String>,
        /// Bearer token for the team server.
        ///
        /// Prefer the BUILDLENS_TOKEN environment variable: an argument is
        /// visible in `ps` to every user on the machine and lands in shell
        /// history. Passing --token warns for that reason.
        #[arg(long, env = "BUILDLENS_TOKEN")]
        token: Option<String>,
        /// Send anonymously instead of with a pseudonymous machine id
        #[arg(long)]
        anonymous: bool,
        /// Print the payload that would be sent without sending it
        #[arg(long)]
        dry_run_push: bool,
        #[command(flatten)]
        collect: CollectArgs,
    },
}
#[derive(Subcommand)]
enum HistoryCommand {
    Save {
        log: PathBuf,
        /// Companion .xcactivitylog for the same build. The text log carries
        /// diagnostics and Swift timings, the activity log the precise
        /// measurements; only together do both reach history.
        #[arg(long)]
        activity_log: Option<PathBuf>,
        /// Companion .xcresult for the same run, as the source of test
        /// results. Xcode states the suite, the verdict and the retry number
        /// outright, where a text log only prints them for humans to read; see
        /// `buildlens-xcresult`. Optional, and only tests come from it.
        #[arg(long)]
        xcresult: Option<PathBuf>,
        /// PostgreSQL connection URL. A URL, not a path: a `PathBuf` here
        /// would pass non-UTF-8 bytes through `to_string_lossy` and silently
        /// connect with a string the user never typed.
        #[arg(long, default_value = "postgres://localhost/buildlens")]
        db: String,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[command(flatten)]
        collect: CollectArgs,
    },
    Compare {
        log: PathBuf,
        /// PostgreSQL connection URL. A URL, not a path: a `PathBuf` here
        /// would pass non-UTF-8 bytes through `to_string_lossy` and silently
        /// connect with a string the user never typed.
        #[arg(long, default_value = "postgres://localhost/buildlens")]
        db: String,
        #[arg(long, value_enum, default_value_t = Format::Terminal)]
        format: Format,
    },
    /// Delete builds older than N days. Previews by default; pass --confirm to
    /// actually delete. The newest build of each project is always kept so
    /// regression baselines survive.
    Prune {
        /// Keep builds recorded within this many days
        #[arg(long, value_name = "N")]
        keep_days: u32,
        /// PostgreSQL connection URL. A URL, not a path: a `PathBuf` here
        /// would pass non-UTF-8 bytes through `to_string_lossy` and silently
        /// connect with a string the user never typed.
        #[arg(long, default_value = "postgres://localhost/buildlens")]
        db: String,
        /// Actually delete. Without this the command only reports what would go.
        #[arg(long)]
        confirm: bool,
    },
}
#[derive(Clone, ValueEnum)]
enum Format {
    Terminal,
    Json,
    Markdown,
}
#[derive(Clone, ValueEnum)]
enum DetailArg {
    Summary,
    Standard,
    Full,
}
impl From<DetailArg> for Detail {
    fn from(value: DetailArg) -> Self {
        match value {
            DetailArg::Summary => Detail::Summary,
            DetailArg::Standard => Detail::Standard,
            DetailArg::Full => Detail::Full,
        }
    }
}
#[derive(Clone, ValueEnum)]
enum Policy {
    /// Any compile or link error.
    Errors,
    /// Any warning. The strictest gate.
    Warnings,
    /// A failing or crashed test.
    ///
    /// Deliberately narrower than `Errors`: a build that failed to compile has
    /// no test results at all, so a job that wants "the build is broken" wants
    /// `--fail-on errors`. `Any` covers both.
    Failures,
    /// Errors, failing tests, or crashes — "this build is not good".
    Any,
}

impl Policy {
    /// Whether this analysis violates the policy.
    ///
    /// A method rather than an inline match so the rules are testable without
    /// running the binary — `--fail-on` is a CI gate, and a gate that silently
    /// stops gating is worse than none.
    fn is_violated_by(&self, analysis: &buildlens_core::BuildAnalysis) -> bool {
        let errors = analysis.diagnostics.raw_errors > 0;
        let failures = analysis.tests.failed > 0 || !analysis.crashes.is_empty();
        match self {
            Self::Errors => errors,
            Self::Warnings => analysis.diagnostics.raw_warnings > 0,
            Self::Failures => failures,
            Self::Any => errors || failures,
        }
    }
}
#[derive(clap::Args, Clone, Default)]
struct CollectArgs {
    /// Project name to record this build under. Overrides the name inferred
    /// from the log's path, which is only reliable for logs inside DerivedData.
    /// Pass "$PROJECT_NAME" from an Xcode scheme post-action.
    #[arg(long, value_name = "NAME")]
    project: Option<String>,
    /// Collect git branch, commit, and dirty state
    #[arg(long)]
    collect_git: bool,
    /// Collect hardware and OS facts via sysctl
    #[arg(long)]
    collect_hardware: bool,
    /// Collect CPU thermal throttling state via pmset
    #[arg(long)]
    collect_thermal: bool,
    /// Detect whether the machine slept during the build
    #[arg(long)]
    collect_suspend: bool,
    /// Detect whether this build ran on CI, and under which provider. Only the
    /// presence of a provider's marker variable is checked, never its value.
    #[arg(long)]
    collect_ci: bool,
    /// Enable all built-in metadata collectors
    #[arg(long)]
    collect_all: bool,
    /// Label this build, repeatable: --tag env=staging --tag runner=m2.
    /// Stored as tag.<key> and filterable in the dashboard.
    #[arg(long = "tag", value_name = "KEY=VALUE", value_parser = parse_tag_arg)]
    tags: Vec<(String, String)>,
    /// Flat string-valued JSON file merged in as user.<key> metadata
    #[arg(long)]
    metadata_file: Option<PathBuf>,
}
impl CollectArgs {
    fn is_enabled(&self) -> bool {
        self.collect_git
            || self.collect_hardware
            || self.collect_thermal
            || self.collect_suspend
            || self.collect_ci
            || self.collect_all
            || !self.tags.is_empty()
            || self.metadata_file.is_some()
    }

    /// Later `--tag` wins on a repeated key, matching how a shell treats a
    /// repeated flag and keeping the map one-value-per-key.
    fn tag_map(&self) -> std::collections::BTreeMap<String, String> {
        self.tags.iter().cloned().collect()
    }
}

/// clap rejects a malformed `--tag` at parse time, so a typo fails before any
/// log is read rather than being silently recorded.
fn parse_tag_arg(argument: &str) -> Result<(String, String), String> {
    buildlens_plugins::parse_tag(argument)
}
fn is_activity_log(path: &std::path::Path) -> bool {
    path.extension().is_some_and(|extension| extension == "xcactivitylog")
}
/// Activity logs carry no parseable text diagnostics; the analysis is the
/// metrics attached to an otherwise empty report.
fn analysis_for(
    path: std::path::PathBuf,
    options: AnalyzeOptions,
    detail: Detail,
) -> Result<buildlens_core::BuildAnalysis> {
    analysis_for_pair(path, None, options, detail)
}

/// The `.xcresult` belonging to this build, if its test run has finished.
///
/// Matched through `Logs/Test/LogStoreManifest.plist`, where Xcode records
/// each result bundle against the `.xcactivitylog` it built from. That pairing
/// is what makes this exact: a build's log and its results are written 70–92
/// seconds apart, so "the newest bundle" is frequently the *previous* run's,
/// and attaching those results would misreport a build rather than merely miss
/// one.
///
/// `None` when the tests have not finished, which for a watcher is the normal
/// case at collect time — the manifest scan attaches them later.
fn bundle_for_build(
    root: &std::path::Path,
    log: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let build_key = log.file_stem()?.to_str()?;
    let entry = buildlens_xcresult::manifest_entries(root)
        .into_iter()
        .find(|entry| entry.activity_log_id.as_deref() == Some(build_key))?;
    let bundle = root.join("Logs/Test").join(&entry.file_name);
    bundle.exists().then_some(bundle)
}

/// The `<Project>-<hash>` DerivedData directory an activity log sits inside.
///
/// Found by walking up to the `Logs` directory and taking its parent, rather
/// than by counting path components: a normal build's log is at
/// `<root>/Logs/Build/x.xcactivitylog`, but an archive build's sits several
/// levels deeper under `Build/Intermediates.noindex/ArchiveIntermediates`.
/// Both have exactly one `Logs` ancestor, so this handles them alike.
///
/// `None` for a log kept anywhere else — a copy in `/tmp`, say — which is a
/// normal thing to analyse and simply has no bundle to find.
fn derived_data_root(log: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut current = log.parent()?;
    loop {
        if current.file_name().is_some_and(|name| name == "Logs") {
            return current.parent().map(std::path::Path::to_path_buf);
        }
        current = current.parent()?;
    }
}

/// Replaces the analysis's test results with the ones an `.xcresult` states,
/// returning the attempt number of each.
///
/// Replaces rather than merges. The bundle and the text log describe the same
/// run, so keeping both would double every test; and where they disagree the
/// bundle is right, because Xcode wrote it as data rather than as a line for a
/// human to read. The text-log results stand only when no bundle is given.
///
/// Everything else on the analysis — timings, diagnostics, the build graph —
/// is untouched. Only tests come from here.
fn apply_xcresult_tests(
    analysis: &mut buildlens_core::BuildAnalysis,
    bundle: Option<&std::path::Path>,
) -> Result<Vec<i32>> {
    let Some(bundle) = bundle else {
        return Ok(Vec::new());
    };
    let runs = buildlens_xcresult::test_runs(bundle)
        .with_context(|| format!("reading test results from {}", bundle.display()))?;
    let attempts: Vec<i32> = runs.iter().map(|run| run.attempt as i32).collect();
    let results: Vec<_> = runs.into_iter().map(|run| run.into_result()).collect();
    // The summary counts distinct runs, matching what the text-log path
    // produces, so a retried test is not counted as two tests.
    analysis.tests.total = results.len();
    analysis.tests.passed = results.iter().filter(|r| r.status == buildlens_core::TestStatus::Passed).count();
    analysis.tests.failed = results.iter().filter(|r| r.status == buildlens_core::TestStatus::Failed).count();
    analysis.tests.slowest = results.clone();
    analysis.tests.slowest.sort_by(|left, right| {
        right.duration_seconds.partial_cmp(&left.duration_seconds).unwrap_or(std::cmp::Ordering::Equal)
    });
    analysis.tests.slowest.truncate(20);
    // These results replace the text log's, so the verdict has to be restated
    // here: a bundle read from a run whose log was never parsed would
    // otherwise leave the analysis at its default "passed" and store a red
    // suite as a green build.
    if analysis.tests.failed > 0 {
        analysis.status.mark_failed();
    }
    analysis.tests.tests = results;
    Ok(attempts)
}

/// Records an explicitly supplied project name on the analysis. Without one,
/// storage falls back to inferring a name from the log's path, which only
/// works for logs still inside `DerivedData/<Project>-<hash>/`.
fn apply_project_name(analysis: &mut buildlens_core::BuildAnalysis, project: Option<&str>) {
    let Some(name) = project.map(str::trim).filter(|name| !name.is_empty()) else {
        return;
    };
    if let Some(metrics) = analysis.metrics.as_mut() {
        metrics.project = Some(name.to_owned());
    }
}

/// Text logs and activity logs describe the same build from different angles:
/// the text log carries diagnostics, tests, and the dependency graph; the
/// activity log carries accurate timings. When both are supplied, the activity
/// log's metrics replace whatever the text log could infer.
fn analysis_for_pair(
    path: std::path::PathBuf,
    activity_log: Option<std::path::PathBuf>,
    options: AnalyzeOptions,
    detail: Detail,
) -> Result<buildlens_core::BuildAnalysis> {
    // Metrics are always stored and reported with redacted paths; only the
    // `metrics --raw` subcommand ever surfaces raw paths.
    let metrics_source = activity_log.clone().unwrap_or_else(|| path.clone());
    if let Some(companion) = &activity_log {
        if !is_activity_log(companion) {
            bail!(
                "--activity-log expects an .xcactivitylog file, got {}",
                companion.display()
            );
        }
        if is_activity_log(&path) {
            bail!(
                "{} is already an activity log; --activity-log is for pairing a text log with one",
                path.display()
            );
        }
    }
    let mut metrics = buildlens_metrics::analyze_file(&metrics_source, detail)
        .ok()
        .map(|metrics| buildlens_metrics::redacted(metrics, None));
    // Only when a companion was supplied is `path` a text log whose Swift
    // timings would otherwise be dropped; kept before `path` is consumed.
    let text_log_for_timings = activity_log.as_ref().map(|_| path.clone());
    let mut analysis = if is_activity_log(&path) {
        buildlens_core::BuildAnalysis::default()
    } else {
        analyze_file(path, options)?
    };
    if let Some(metrics) = &metrics {
        for warning in &metrics.warnings {
            analysis
                .investigation
                .next_steps
                .push(format!("Activity log: {warning}"));
        }
    }
    // Swift function-body / type-check timings only ever appear as warning
    // lines in the *text* log — the activity log has no equivalent, so pairing
    // the two would otherwise lose them. The text log has to be parsed for
    // metrics separately: `analyze_file` above is the diagnostics parser and
    // leaves `analysis.metrics` unset.
    if let (Some(metrics), Some(text)) = (metrics.as_mut(), text_log_for_timings.as_ref())
        && metrics.swift_timings.is_empty()
        && let Ok(text_metrics) = buildlens_metrics::analyze_file(text, detail)
    {
        let timings = buildlens_metrics::redacted(text_metrics, None).swift_timings;
        if !timings.is_empty() {
            metrics.swift_timings = timings;
        }
    }
    // An activity log records diagnostics too, but as separate fields rather
    // than formatted lines, so the text parser never sees them. Run them
    // through the same classifier here: only when the text parser found none,
    // since its output is richer where both exist.
    if analysis.diagnostics.diagnostics.is_empty()
        && let Some(metrics) = &metrics
    {
        analysis.diagnostics = diagnostics_from_metrics(&metrics.diagnostics);
    }
    analysis.metrics = metrics;
    Ok(analysis)
}
/// Classifies and deduplicates an activity log's diagnostics with the same
/// fingerprinting a text log's go through, so the same warning found either way
/// groups together in the dashboard.
fn diagnostics_from_metrics(
    raw: &[buildlens_core::MetricDiagnostic],
) -> buildlens_core::DiagnosticSummary {
    use buildlens_core::DiagnosticSeverity;
    use std::collections::BTreeMap;
    let mut by_fingerprint: BTreeMap<String, buildlens_core::DiagnosticAggregate> =
        BTreeMap::new();
    let (mut raw_warnings, mut raw_errors) = (0usize, 0usize);
    for item in raw {
        // Xcode's scale: 1 is a warning, 2 and above an error.
        let severity = if item.severity >= 2 {
            raw_errors += 1;
            DiagnosticSeverity::Error
        } else {
            raw_warnings += 1;
            DiagnosticSeverity::Warning
        };
        let mut aggregate = buildlens_diagnostics::from_parts(
            severity,
            item.file.clone(),
            item.line,
            item.column,
            item.message.clone(),
        );
        aggregate.example.target = item.target.clone();
        by_fingerprint
            .entry(aggregate.fingerprint.clone())
            .and_modify(|existing| existing.occurrences += 1)
            .or_insert(aggregate);
    }
    let diagnostics: Vec<_> = by_fingerprint.into_values().collect();
    let unique_warnings = diagnostics
        .iter()
        .filter(|d| d.severity == DiagnosticSeverity::Warning)
        .count();
    let unique_errors = diagnostics.len() - unique_warnings;
    buildlens_core::DiagnosticSummary {
        raw_warnings,
        unique_warnings,
        raw_errors,
        unique_errors,
        diagnostics,
        swift6: Default::default(),
    }
}

/// Warns when the token was typed as an argument rather than read from the
/// environment.
///
/// clap fills `token` from either source, so the value alone cannot say which.
/// The raw arguments can: a `--token` on the command line is in `ps` output for
/// every user on the machine, and in shell history.
fn warn_if_token_came_from_the_command_line(token: Option<&str>) {
    if token.is_some() && token_passed_as_argument(std::env::args()) {
        eprintln!(
            "warning: --token was passed on the command line, where it is visible \
             in `ps` and recorded in shell history. Set BUILDLENS_TOKEN instead."
        );
    }
}

/// Whether these arguments carry the token, in either `--token X` or
/// `--token=X` form.
fn token_passed_as_argument(mut args: impl Iterator<Item = String>) -> bool {
    args.any(|argument| argument == "--token" || argument.starts_with("--token="))
}

/// Terminal rendering for `history compare`. States the confidence caveats
/// inline rather than printing a bare number: a comparison across a category
/// change or an environment shift means something different, and hiding that
/// makes the number look more certain than it is.
fn baseline_comparison_text(comparison: &buildlens_storage::BaselineComparison) -> String {
    let mut text = String::new();
    let delta = comparison.current_seconds - comparison.previous_seconds;
    text.push_str(&format!(
        "Compared against build {} of {}\n  {:.1}s -> {:.1}s ({}{:.1}s)\n",
        comparison.baseline_build_key,
        comparison.project,
        comparison.previous_seconds,
        comparison.current_seconds,
        if delta >= 0.0 { "+" } else { "" },
        delta
    ));
    if let Some((from, to)) = &comparison.category_change {
        text.push_str(&format!(
            "  Build category changed {from} -> {to}: only the totals compare, \
             and they compare weakly.\n"
        ));
    }
    if comparison.environment_changed {
        text.push_str(
            "  Environment changed between these builds, so this is low confidence.\n",
        );
    }
    if comparison.regressions.is_empty() {
        text.push_str("  No target got materially slower.\n");
        return text;
    }
    text.push_str(&format!("  {} slower target(s):\n", comparison.regressions.len()));
    for regression in comparison.regressions.iter().take(10) {
        text.push_str(&format!(
            "    {}: {:.2}s -> {:.2}s (+{:.2}s, +{:.1}%) [{}]\n",
            regression.name,
            regression.previous_seconds,
            regression.current_seconds,
            regression.delta_seconds,
            regression.delta_percent,
            regression.confidence.as_str()
        ));
    }
    text
}

fn collect_metadata(
    collect: &CollectArgs,
    repo: &std::path::Path,
    log_path: Option<&std::path::Path>,
    analysis: &buildlens_core::BuildAnalysis,
) -> buildlens_core::CollectedMetadata {
    use buildlens_plugins::{
        CiPlugin, GitContextPlugin, HardwarePlugin, MetricsPlugin, PluginContext, RealProbe,
        SuspendPlugin, TagPlugin, ThermalPlugin, UserMetadataPlugin, XcodeEnvPlugin,
    };
    if !collect.is_enabled() {
        return Default::default();
    }
    let mut plugins: Vec<&dyn MetricsPlugin> = vec![&XcodeEnvPlugin];
    if collect.collect_git || collect.collect_all {
        plugins.push(&GitContextPlugin);
    }
    if collect.collect_hardware || collect.collect_all {
        plugins.push(&HardwarePlugin);
    }
    if collect.collect_thermal || collect.collect_all {
        plugins.push(&ThermalPlugin);
    }
    if collect.collect_suspend || collect.collect_all {
        plugins.push(&SuspendPlugin);
    }
    if collect.collect_ci || collect.collect_all {
        plugins.push(&CiPlugin);
    }
    if !collect.tags.is_empty() {
        plugins.push(&TagPlugin);
    }
    if collect.metadata_file.is_some() {
        plugins.push(&UserMetadataPlugin);
    }
    let build_start_unix = log_path
        .and_then(|path| std::fs::metadata(path).ok())
        .and_then(|meta| meta.modified().ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64);
    let probe = RealProbe;
    let tags = collect.tag_map();
    let context = PluginContext {
        repo_root: repo,
        build_start_unix,
        build: &analysis.build,
        environment: analysis.metrics.as_ref().map(|metrics| &metrics.environment),
        user_metadata_path: collect.metadata_file.as_deref(),
        tags: &tags,
        probe: &probe,
    };
    buildlens_plugins::run_plugins(&plugins, &context)
}
/// Exit code used when `--fail-on` matches. Distinct from 1, which anyhow
/// uses for an actual error, so a CI job can tell "the analysis ran and the
/// build is bad" from "the analysis itself failed".
const POLICY_EXIT_CODE: u8 = 2;

fn main() -> std::process::ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Error: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<std::process::ExitCode> {
    let c = Cli::parse();
    match c.command {
        Command::Metrics { input, format, output, raw, detail } => {
            let metrics = buildlens_metrics::analyze_file(&input, detail.into())?;
            let metrics = if raw { metrics } else { buildlens_metrics::redacted(metrics, None) };
            let total = metrics.total_seconds.map(|x| format!("{x:.3}s")).unwrap_or_else(|| "unknown".into());
            let cache = metrics.cache.hit_rate.map(|x| format!("{:.0}% ({})", x * 100.0, metrics.cache.status)).unwrap_or_else(|| metrics.cache.status.clone());
            if !metrics.warnings.is_empty() {
                for warning in &metrics.warnings {
                    eprintln!("warning: {warning}");
                }
                if !metrics.is_usable() {
                    eprintln!(
                        "warning: this log did not decode into a usable build; if Xcode is still writing it, retry or use `buildlens collect`"
                    );
                }
            }
            let text = match format {
                Format::Json => serde_json::to_string_pretty(&metrics)?,
                Format::Markdown => format!("## BuildLens Metrics\n\n- **Source:** {:?}\n- **Category:** {}\n- **Total:** {}\n- **Cache hits:** {}\n- **Phases:** {}\n- **Targets:** {}\n- **Files:** {}\n- **Warnings:** {}\n", metrics.source_kind, metrics.category.as_str(), total, cache, metrics.phases.len(), metrics.targets.len(), metrics.files.len(), metrics.warnings.len()),
                Format::Terminal => {
                    let targets = metrics.targets.iter().take(10).map(|x| format!("\n{}: {:.3}s [{}]", x.name, x.seconds, x.category.as_str())).collect::<String>();
                    let phases = metrics.phases.iter().take(10).map(|x| format!("\n{}: {:.3}s", x.name, x.seconds)).collect::<String>();
                    let files = metrics.files.iter().take(10).map(|x| format!("\n{}: {:.3}s", x.file, x.seconds)).collect::<String>();
                    format!("BuildLens Metrics\n\nSource: {:?}\nCategory: {}\nTotal: {}\nCache hits: {}\nPhases: {}\nTargets: {}\nFiles: {}\n\nSlowest targets:{}\n\nSlowest phases:{}\n\nSlowest files:{}\n", metrics.source_kind, metrics.category.as_str(), total, cache, metrics.phases.len(), metrics.targets.len(), metrics.files.len(), targets, phases, files)
                }
            };
            if let Some(path) = output { std::fs::write(path, text)?; } else { println!("{text}"); }
        }
        Command::Dashboard { db, port } => {
            // The same server the container runs, so there is one
            // implementation rather than two that drift. Loopback and
            // unauthenticated: this is the local dashboard, and a token would
            // be a password you set for yourself. `buildlens-server` reads its
            // settings from the environment instead and refuses to start
            // without one.
            buildlens_server::run(buildlens_server::Config {
                database_url: db,
                bind: format!("127.0.0.1:{port}"),
                token: None,
                pool_size: 4,
                threads: 4,
            })?;
        }
        Command::Analyze {
            log,
            format,
            output,
            detail,
            no_ai,
            fail_on,
            base,
            head,
            repo,
            activity_log,
            intel,
            db,
            collect,
        } => {
            let log_path_for_metadata = log.clone().filter(|path| path.as_os_str() != "-");
            let mut a = if let Some(path) = log.filter(|path| path.as_os_str() != "-") {
                analysis_for_pair(
                    path,
                    activity_log.clone(),
                    AnalyzeOptions { detail: detail.clone().into(), no_ai },
                    detail.clone().into(),
                )?
            } else {
                let stdin = std::io::stdin();
                let mut analysis = buildlens_parser::analyze_reader(
                    stdin.lock(),
                    AnalyzeOptions { detail: detail.clone().into(), no_ai },
                )?;
                if let Some(companion) = &activity_log {
                    analysis.metrics = buildlens_metrics::analyze_file(companion, detail.clone().into())
                        .ok()
                        .map(|metrics| buildlens_metrics::redacted(metrics, None));
                }
                analysis
            };
            apply_project_name(&mut a, collect.project.as_deref());
            a.metadata = collect_metadata(&collect, &repo, log_path_for_metadata.as_deref(), &a);
            if let (Some(base), Some(head)) = (base.as_deref(), head.as_deref()) {
                a.git = Some(buildlens_git::correlate(&repo, base, head, &a)?);
            }
            if intel || (a.metrics.is_some() && a.git.is_some()) {
                let regressions = match (&db, &a.metrics) {
                    (Some(db), Some(metrics)) => {
                        // Regression baselines now live in PostgreSQL; the
                        // standalone analyzer does not query storage here.
                        let _ = (db, metrics);
                        vec![]
                    }
                    _ => vec![],
                };
                let intelligence = buildlens_intel::analyze(&a, &regressions);
                a.investigation.next_steps.extend(
                    intelligence.chains.iter().take(3).map(|chain| chain.summary.clone()),
                );
                a.intelligence = Some(intelligence);
            }
            // `json` reports serialization failure rather than falling back to
            // an empty document: `{}` is a valid, *clean* report, so writing it
            // to --output would record "nothing found" for a run that never
            // serialized.
            let text = match format {
                Format::Json => {
                    buildlens_report::json(&a).context("rendering the analysis as JSON")?
                }
                Format::Markdown => buildlens_report::markdown(&a),
                Format::Terminal => buildlens_report::terminal(&a),
            };
            if let Some(path) = output {
                std::fs::write(path, text)?
            } else {
                println!("{text}")
            }
            // Returned rather than `process::exit`, which skips destructors:
            // a buffered --output write would be lost in exactly the case a CI
            // job wants both the report file and the failing status.
            if fail_on.is_some_and(|policy| policy.is_violated_by(&a)) {
                return Ok(std::process::ExitCode::from(POLICY_EXIT_CODE));
            }
        }
        Command::Why { target, log } => {
            let analysis = analyze_file(log, AnalyzeOptions::default())?;
            let graph = TargetGraph::new(&analysis.graph);
            // A name can belong to more than one project, and answering for
            // whichever happened to be declared first would be a guess. Say so
            // and let the caller disambiguate.
            let matches = graph.find_all(&target);
            let node = match matches.as_slice() {
                [] => bail!("no target named '{target}' is in this build's graph"),
                [only] => *only,
                many => bail!(
                    "'{target}' names {} targets ({}); the graph cannot say which was meant",
                    many.len(),
                    many.iter()
                        .map(|node| node.project.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            };
            match graph.shortest_path_from_root(node) {
                Some(path) => println!(
                    "Why was {target} built?\n\n{}",
                    path.iter()
                        .map(|node| node.name.as_str())
                        .collect::<Vec<_>>()
                        .join("\n    ↓\n")
                ),
                // A root is built because it was asked for, which is a real
                // answer rather than a missing path.
                None if graph.roots().contains(&node) => {
                    println!("{target} is a root target: it was built because it was requested.")
                }
                None => bail!("no dependency path to target '{target}' found"),
            }
        }
        Command::Warnings { log } => {
            let a = analyze_file(log, AnalyzeOptions::default())?;
            println!(
                "Warnings\nRaw occurrences: {}\nUnique issues: {}",
                a.diagnostics.raw_warnings, a.diagnostics.unique_warnings
            )
        }
        Command::Failures { log } => {
            let a = analyze_file(log, AnalyzeOptions::default())?;
            println!(
                "Failures\n{} test failures, {} crashes",
                a.tests.failed, a.tests.crashed
            )
        }
        Command::Tests { log } => {
            let a = analyze_file(log, AnalyzeOptions::default())?;
            println!(
                "Tests\nTotal: {}  Passed: {}  Failed: {}  Crashed: {}",
                a.tests.total, a.tests.passed, a.tests.failed, a.tests.crashed
            )
        }
        Command::Graph { log } => {
            let a = analyze_file(log, AnalyzeOptions::default())?;
            println!(
                "Target graph\nTargets: {}\nDeclared: {:?}\nDependencies: {}\nHotspots: {}",
                a.graph.targets.len(),
                a.graph.declared_count,
                a.graph.dependencies.len(),
                a.graph.hotspots.join(", ")
            )
        }
        Command::Collect {
            build_dir,
            match_project,
            db,
            repo,
            timeout,
            dry_run,
            all,
            watch,
            watch_interval,
            server,
            token,
            anonymous,
            dry_run_push,
            collect,
        } => {
            warn_if_token_came_from_the_command_line(token.as_deref());
            let explicit = build_dir.as_deref().map(expand_home);
            let root = collect::search_root(
                explicit.as_deref(),
                &expand_home(DEFAULT_DERIVED_DATA),
            );
            // Before the lookup below: a watcher must survive an empty
            // DerivedData and wait for the first build, not exit.
            if watch {
                watch_loop(
                    &root,
                    match_project.as_deref(),
                    &db,
                    &repo,
                    &collect,
                    timeout,
                    watch_interval,
                )?;
                return Ok(std::process::ExitCode::SUCCESS);
            }
            let logs = if all {
                collect::find_activity_logs(&root, match_project.as_deref())?
            } else {
                vec![collect::find_newest_activity_log(&root, match_project.as_deref())?]
            };
            if dry_run {
                for log in &logs {
                    println!("Would collect {}", log.display());
                }
                return Ok(std::process::ExitCode::SUCCESS);
            }
            if all {
                // Backfill: import every log, reporting per-log outcomes rather
                // than failing the run on one bad file.
                let mut store = PostgresStore::connect(&db)?;
                let (mut imported, mut skipped) = (0usize, 0usize);
                for log in &logs {
                    match import_one(log, &mut store, &repo, &collect, timeout) {
                        Ok(true) => imported += 1,
                        Ok(false) => skipped += 1,
                        Err(error) => {
                            skipped += 1;
                            eprintln!("skipped {}: {error}", log.display());
                        }
                    }
                }
                println!(
                    "Imported {imported} build(s), skipped {skipped} of {} log(s)",
                    logs.len()
                );
                return Ok(std::process::ExitCode::SUCCESS);
            }
            let log = logs.into_iter().next().expect("at least one log");
            collect::wait_until_stable(&log, std::time::Duration::from_secs(timeout))?;
            let mut analysis = analysis_for(
                log.clone(),
                AnalyzeOptions { detail: Detail::Full, no_ai: true },
                Detail::Full,
            )?;
            // Xcode.app writes an `.xcresult` for every ⌘U run into the same
            // DerivedData directory as the activity log, without being asked.
            // Finding it here is what makes a UI test run record its tests
            // with correct suites and Xcode's own retry numbers, the same as a
            // terminal run passing --xcresult. A build-only run writes none,
            // which is why a missing bundle is silence rather than a warning.
            // Results already on disk for *this* build, and only this build.
            //
            // Nothing waits here. Xcode writes the build log when the compile
            // finishes and the `.xcresult` only when the tests do, 70–92
            // seconds later on a measured project, so blocking for it would
            // stall the watcher for every test build and still race. A
            // one-shot `collect` run after the tests finish finds them here; a
            // watcher gets them from the manifest scan instead, once the run
            // completes. See `attach_new_test_results`.
            //
            // Matched through the manifest rather than by taking the newest
            // bundle: the newest may belong to a previous run, and attaching
            // last run's results to this build looks right while being wrong.
            let attempts = match derived_data_root(&log) {
                Some(root) => {
                    let bundle = bundle_for_build(&root, &log);
                    apply_xcresult_tests(&mut analysis, bundle.as_deref())?
                }
                None => Vec::new(),
            };
            // `--project` names the build in history. `--match-project` only
            // picks which DerivedData entry to read, but when it is the sole
            // hint it is a better name than one inferred from the log path.
            apply_project_name(
                &mut analysis,
                collect.project.as_deref().or(match_project.as_deref()),
            );
            analysis.metadata = collect_metadata(&collect, &repo, Some(&log), &analysis);
            reject_unusable_metrics(&analysis, &log)?;
            let mut store = PostgresStore::connect(&db)?;
            let activity_id = analysis
                .metrics
                .as_ref()
                .and_then(|metrics| metrics.build_id.clone())
                .filter(|id| !id.is_empty());
            if let Some(activity_id) = &activity_id
                && let Some(existing) = store.build_id_for_activity_log(activity_id)?
            {
                {
                    println!(
                        "Already collected {} as BuildLens build {existing}",
                        log.display()
                    );
                    return Ok(std::process::ExitCode::SUCCESS);
                }
            }
            let project = project_name_for_storage(&analysis);
            let inserted = if attempts.is_empty() {
                store.save_analysis(&analysis, &project, local_machine_id(anonymous), anonymous)?
            } else {
                store.save_analysis_with_attempts(&analysis, &project, local_machine_id(anonymous), anonymous, &attempts)?
            };
            let summary = analysis
                .metrics
                .as_ref()
                .map(|metrics| {
                    format!(
                        " ({}, {})",
                        metrics.category.as_str(),
                        metrics
                            .total_seconds
                            .map(|seconds| format!("{seconds:.1}s"))
                            .unwrap_or_else(|| "duration unknown".into())
                    )
                })
                .unwrap_or_default();
            println!("{} {} in PostgreSQL{summary}", if inserted { "Collected" } else { "Skipped duplicate" }, log.display());
            if let Some(server) = &server {
                push_metrics(
                    &analysis,
                    server,
                    token.as_deref(),
                    anonymous,
                    dry_run_push,
                    &log,
                )?;
            }
        }
        Command::History { command } => match command {
            HistoryCommand::Save { log, activity_log, xcresult, db, repo, collect } => {
                let log_for_metadata = log.clone();
                let mut analysis = analysis_for_pair(
                    log,
                    activity_log,
                    AnalyzeOptions { detail: Detail::Full, no_ai: true },
                    Detail::Full,
                )?;
                let attempts = apply_xcresult_tests(&mut analysis, xcresult.as_deref())?;
                apply_project_name(&mut analysis, collect.project.as_deref());
                analysis.metadata = collect_metadata(&collect, &repo, Some(&log_for_metadata), &analysis);
                reject_unusable_metrics(&analysis, &log_for_metadata)?;
                let mut store = PostgresStore::connect(&db)?;
                let project = project_name_for_storage(&analysis);
                let inserted = if attempts.is_empty() {
                    store.save_analysis(&analysis, &project, local_machine_id(false), false)?
                } else {
                    store.save_analysis_with_attempts(&analysis, &project, local_machine_id(false), false, &attempts)?
                };
                println!("{} build in PostgreSQL", if inserted { "Saved" } else { "Skipped duplicate" });
            }
            HistoryCommand::Prune { keep_days, db, confirm } => {
                if keep_days == 0 {
                    bail!("--keep-days must be at least 1; 0 would delete the entire history");
                }
                let mut store = PostgresStore::connect(&db)?;
                // Previews by default: deleting history is not something to do
                // as a side effect of a mistyped number.
                let doomed = store.prune_preview(keep_days)?;
                if doomed.is_empty() {
                    println!("Nothing to prune: no build is older than {keep_days} days.");
                    return Ok(std::process::ExitCode::SUCCESS);
                }
                if !confirm {
                    println!(
                        "Would delete {} build(s) older than {keep_days} days. \
                         The newest build of each project is always kept.",
                        doomed.len()
                    );
                    for key in doomed.iter().take(10) {
                        println!("  {key}");
                    }
                    if doomed.len() > 10 {
                        println!("  ... and {} more", doomed.len() - 10);
                    }
                    println!("Re-run with --confirm to delete.");
                    return Ok(std::process::ExitCode::SUCCESS);
                }
                let deleted = store.prune(keep_days)?;
                println!("Deleted {deleted} build(s) older than {keep_days} days.");
            }
            HistoryCommand::Compare { log, db, format } => {
                let analysis = analysis_for(
                    log.clone(),
                    AnalyzeOptions { detail: Detail::Full, no_ai: true },
                    Detail::Full,
                )?;
                let key = analysis
                    .metrics
                    .as_ref()
                    .and_then(|metrics| metrics.build_id.clone())
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("{} has no build id to compare", log.display()))?;
                let mut store = PostgresStore::connect(&db)?;
                let Some(comparison) = store.compare_to_baseline(&key)? else {
                    // No baseline means no claim. Saying "no regressions" here
                    // would read as a verdict rather than an absence of data.
                    println!(
                        "No earlier build of this project to compare against. \
                         Save this build, then compare the next one."
                    );
                    return Ok(std::process::ExitCode::SUCCESS);
                };
                match format {
                    Format::Json => println!("{}", serde_json::to_string_pretty(&comparison)?),
                    _ => print!("{}", baseline_comparison_text(&comparison)),
                }
            }
        },
    }
    Ok(std::process::ExitCode::SUCCESS)
}

/// Terminal rendering for `history compare`, which is not yet reimplemented on
/// PostgreSQL — its only callers are in the commented-out block above. Kept as
/// the reference for restoring that command rather than rewritten from scratch.
#[allow(dead_code)]
fn metric_regression_summary(comparison: &buildlens_core::HistoryComparison) -> String {
    let mut text = String::new();
    if let Some((from, to)) = &comparison.build_category_change {
        text.push_str(&format!("Build category change: {from} -> {to}\n"));
    }
    text.push_str(&format!(
        "Metric regressions: {}\n",
        comparison.metric_regressions.len()
    ));
    for regression in comparison.metric_regressions.iter().take(5) {
        text.push_str(&format!(
            "  {} {}: {:.3}s -> {:.3}s (+{:.3}s, +{:.1}%) [{}] {}\n",
            regression.metric_kind.as_str(),
            regression.name,
            regression.previous_seconds,
            regression.current_seconds,
            regression.delta_seconds,
            regression.delta_percent,
            regression.confidence.as_str(),
            regression.reason
        ));
    }
    text
}

/// Where Xcode.app keeps DerivedData, and so where a scan starts when neither
/// `--build-dir` nor `$BUILD_DIR` says otherwise.
const DEFAULT_DERIVED_DATA: &str = "~/Library/Developer/Xcode/DerivedData";

/// Unix seconds `keep_days` before now. Builds recorded before this go.
///
/// `--keep-days 0` would delete everything except the per-project baselines,
/// which is more likely a typo than an intent, so it is refused.
fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => PathBuf::from(path),
        },
        None => PathBuf::from(path),
    }
}

/// Refuses to record a build whose log could not be decoded. Xcode creates an
/// activity log before it finishes writing it, so a truncated or empty file is
/// a routine timing condition, not a corrupt install.
fn reject_unusable_metrics(
    analysis: &buildlens_core::BuildAnalysis,
    log: &std::path::Path,
) -> Result<()> {
    let Some(metrics) = &analysis.metrics else {
        return Ok(());
    };
    if metrics.is_usable() {
        return Ok(());
    }
    // A clean-action log decodes perfectly and still is not a build, so the
    // "still being written" advice would send the reader down the wrong path.
    if metrics.timed_no_work() {
        bail!(
            "{} is an `xcodebuild clean` log, not a build: it timed {} but compiled nothing \
             and names no targets. Recording it would put a ~20s entry next to real builds of \
             the same project. Build first, then collect.",
            log.display(),
            metrics
                .total_seconds
                .map(|s| format!("{s:.1}s"))
                .unwrap_or_else(|| "no work".into())
        );
    }
    // A partial decode is not a timing problem, so the "still being written"
    // advice would send the reader down the wrong path: the file is complete
    // and BuildLens could not read all of it. Say so, and say what survived,
    // because the fragment is the evidence for a bug report.
    if metrics.decoded_partially() {
        bail!(
            "{} decoded only partially ({}), leaving {} phase(s), no targets and nothing \
             compiled. That fragment is not a build, and recording it would put a near-empty \
             row next to the same build collected from a complete log. This is a parser gap \
             rather than a problem with your build — please report the log.",
            log.display(),
            metrics.warnings.join("; "),
            metrics.phases.len()
        );
    }
    let detail = if metrics.warnings.is_empty() {
        "no build sections were found".to_owned()
    } else {
        metrics.warnings.join("; ")
    };
    bail!(
        "{} did not decode into a usable build ({detail}). \
         If Xcode is still writing it, retry or use `buildlens collect`, which waits for the log to settle.",
        log.display()
    )
}

/// Sends already-parsed metrics to a team server. Only ever called when the
/// user passed --server.
fn push_metrics(
    analysis: &buildlens_core::BuildAnalysis,
    server: &str,
    token: Option<&str>,
    anonymous: bool,
    dry_run: bool,
    log: &std::path::Path,
) -> Result<()> {
    use buildlens_core::wire::Attribution;
    let Some(metrics) = &analysis.metrics else {
        bail!("no metrics to send for {}", log.display());
    };
    let attribution = if anonymous { Attribution::Anonymous } else { Attribution::Pseudonymous };
    let machine = match attribution {
        Attribution::Anonymous => None,
        Attribution::Pseudonymous => push::machine_id(&buildlens_plugins::RealProbe),
    };
    // An explicit --project wins here too, so the team server groups builds
    // under the same name the local dashboard shows.
    let project = metrics
        .project
        .clone()
        .or_else(|| metrics.source_log.as_deref().map(project_name_from_log))
        .unwrap_or_else(|| "unknown".to_owned());
    let options = push::PushOptions {
        server,
        token,
        project: &project,
        attribution,
        machine_id: machine,
        dry_run,
        // The caller already holds the analysis these metrics came from, so a
        // pushed build carries the same diagnostics and tests a locally
        // collected one records.
        analysis: Some(analysis),
    };
    let result = push::push(metrics, &options)?;
    if dry_run {
        println!("Would send to {server}:\n{}", serde_json::to_string_pretty(&result)?);
    } else if result.get("duplicate").and_then(serde_json::Value::as_bool) == Some(true) {
        println!("Server already had this build; nothing re-sent");
    } else {
        println!("Sent metrics for {project} to {server}");
    }
    Ok(())
}

/// The machine id recorded with a locally stored build.
///
/// The same pseudonymous id the `--server` push path sends, so a build stored
/// locally and the same build pushed to a team server identify the machine
/// identically. Without this every local build recorded an empty `machine_id`,
/// leaving the dashboard's "Machine" field permanently blank and
/// `/api/environment` unable to count distinct machines.
///
/// `anonymous` suppresses it, matching the push path: opting out of
/// attribution must mean the same thing wherever a build is written.
fn local_machine_id(anonymous: bool) -> Option<String> {
    if anonymous {
        return None;
    }
    push::machine_id(&buildlens_plugins::RealProbe)
}

/// The name a build is stored under. `--project` (already folded into
/// `metrics.project` by `apply_project_name`) wins; otherwise the name is
/// inferred from the log's DerivedData path, exactly as the `--server` push
/// path does, so the same build never lands as "Kaizen" in one place and
/// "unknown" in another. Only a log outside DerivedData falls back to
/// "unknown".
fn project_name_for_storage(analysis: &buildlens_core::BuildAnalysis) -> String {
    analysis
        .metrics
        .as_ref()
        .and_then(|metrics| {
            metrics
                .project
                .clone()
                .or_else(|| metrics.source_log.as_deref().map(project_name_from_log))
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

/// DerivedData stores logs under `<Project>-<hash>/Logs/Build`; the project is
/// the directory name with that hash removed.
fn project_name_from_log(source_log: &str) -> String {
    let directory = source_log
        .split("/Logs/")
        .next()
        .unwrap_or(source_log)
        .rsplit('/')
        .next()
        .unwrap_or("unknown");
    match directory.rsplit_once('-') {
        Some((name, suffix))
            if !name.is_empty()
                && suffix.len() >= 3
                && suffix.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()) =>
        {
            name.to_owned()
        }
        _ => directory.to_owned(),
    }
}

/// Imports one log for `--all`. Returns whether it was newly stored; an
/// already-collected or undecodable log is a skip, not a failure, so one bad
/// file never aborts a backfill.
/// Import every build Xcode writes, for as long as this runs.
///
/// Polls rather than using FSEvents: a build log is written over seconds and
/// the interesting moment is when it stops changing, not when it first
/// appears, so `wait_until_stable` does the real work either way. Polling a
/// directory listing every few seconds costs nothing next to a build.
///
/// Safety comes from `import_one` being idempotent — it returns `Ok(false)`
/// for a log already in history — so a log seen on every scan is imported
/// once. Errors are reported and skipped: a watcher that exits on one bad log
/// stops collecting the builds after it.
fn watch_loop(
    root: &std::path::Path,
    match_project: Option<&str>,
    db: &str,
    repo: &std::path::Path,
    collect: &CollectArgs,
    timeout: u64,
    interval: u64,
) -> Result<()> {
    let mut store = PostgresStore::connect(db)?;
    // Logs already handled this session, keyed by path *and* write identity.
    // Xcode reuses one file for successive builds in the same DerivedData, so
    // keying on the path alone makes every build after the first invisible.
    // History is still the real dedup; this only avoids re-parsing unchanged
    // logs on every scan.
    let mut seen: std::collections::HashSet<(PathBuf, u64, i64)> =
        std::collections::HashSet::new();
    let mut first_scan = true;
    // Bundles whose results are already in history, by bundle name.
    let mut attached: std::collections::HashSet<String> = std::collections::HashSet::new();
    println!("Watching {} — press Ctrl-C to stop", root.display());
    // Said once, at startup, rather than on every scan: a watcher pointed at a
    // DerivedData that `xcodebuild` writes no logs into would otherwise sit
    // silent forever, which is indistinguishable from "no builds yet" and is
    // exactly the state that looks like a bug in the watcher.
    if let Some(hint) = collect::shared_derived_data_hint(root) {
        eprintln!("warning: {hint}");
    }
    loop {
        match collect::list_activity_logs(root, match_project) {
            Ok(logs) => {
                for log in logs {
                    // Size and mtime change whenever Xcode rewrites the file,
                    // so a reused path is treated as the new build it is.
                    let stamp = std::fs::metadata(&log)
                        .and_then(|meta| {
                            let modified = meta
                                .modified()?
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs() as i64)
                                .unwrap_or_default();
                            Ok((meta.len(), modified))
                        })
                        .unwrap_or((0, 0));
                    if !seen.insert((log.clone(), stamp.0, stamp.1)) {
                        continue;
                    }
                    // The first scan adopts whatever already exists without
                    // importing it: a watcher started months in should not
                    // silently backfill. `collect --all` is the explicit way.
                    if first_scan {
                        continue;
                    }
                    // Xcode creates the file before it writes it, so a log seen
                    // at 0 bytes is normal. Leave it for the next scan rather
                    // than blocking this one waiting for a build in progress.
                    if std::fs::metadata(&log).map(|meta| meta.len()).unwrap_or(0) == 0 {
                        seen.remove(&(log.clone(), stamp.0, stamp.1));
                        continue;
                    }
                    match import_one(&log, &mut store, repo, collect, timeout) {
                        Ok(true) => println!("Collected {}", log.display()),
                        Ok(false) => {}
                        Err(error) => {
                            // Only a log still being written is worth another
                            // look. Every other rejection is a verdict about
                            // this log's contents — a clean log never becomes
                            // a build log — so retrying would report the same
                            // failure on every scan, forever.
                            let transient = error.to_string().contains("did not become stable");
                            if transient {
                                seen.remove(&(log.clone(), stamp.0, stamp.1));
                            }
                            eprintln!("skipped {}: {error}", log.display());
                        }
                    }
                }
            }
            // A transient failure (DerivedData being cleaned mid-scan) must not
            // end the watch.
            Err(error) => eprintln!("scan failed: {error}"),
        }
        // Test results are a second event, not part of the scan above. Xcode
        // writes a build's log when the compile finishes and its `.xcresult`
        // only when the tests do — 70 to 92 seconds later on a measured
        // project — so by the time results exist their build has long been
        // stored. This attaches them to it.
        //
        // Driven by the manifest rather than by the bundle appearing: the
        // bundle *directory* is created when tests start and filled as they
        // run, so a watcher triggering on its existence reads a half-written
        // result. The manifest entry is written when the run completes.
        if !first_scan {
            attach_new_test_results(root, match_project, &mut store, &mut attached);
        }
        first_scan = false;
        std::thread::sleep(std::time::Duration::from_secs(interval));
    }
}

/// Attaches results from any completed test run not yet seen this session.
///
/// `attached` keys on the bundle name, so a manifest re-read across scans does
/// no repeated work; `attach_tests` is idempotent regardless, which is what
/// makes a restarted watcher safe rather than merely cheap.
///
/// Failures are reported and skipped for the same reason the build scan skips
/// them: a watcher that exits on one unreadable bundle stops collecting
/// everything after it.
fn attach_new_test_results(
    root: &std::path::Path,
    match_project: Option<&str>,
    store: &mut PostgresStore,
    attached: &mut std::collections::HashSet<String>,
) {
    let Ok(projects) = collect::list_project_dirs(root, match_project) else {
        return;
    };
    for project_dir in projects {
        for entry in buildlens_xcresult::manifest_entries(&project_dir) {
            if attached.contains(&entry.file_name) {
                continue;
            }
            // The build log this run compiled from, named by Xcode rather than
            // matched by timestamp against a build that finished a minute and
            // a half earlier.
            let Some(build_key) = entry.activity_log_id.clone() else {
                continue;
            };
            // Only for a build already in history. A test run whose build was
            // never collected — the watcher started after it — is left for a
            // later `collect`, not attached to nothing.
            match store.build_id_for_activity_log(&build_key) {
                Ok(Some(_)) => {}
                Ok(None) => continue,
                Err(error) => {
                    eprintln!("could not look up build {build_key}: {error}");
                    continue;
                }
            }
            let bundle = project_dir.join("Logs/Test").join(&entry.file_name);
            let runs = match buildlens_xcresult::test_runs(&bundle) {
                Ok(runs) => runs,
                Err(error) => {
                    // Not marked attached: a bundle Xcode is still finishing
                    // becomes readable on a later scan.
                    eprintln!("skipped {}: {error}", bundle.display());
                    continue;
                }
            };
            let tests: Vec<_> = runs
                .into_iter()
                .map(|run| {
                    let attempt = run.attempt as i32;
                    (run.into_result(), attempt)
                })
                .collect();
            match store.attach_tests(&build_key, &tests) {
                Ok(count) => {
                    attached.insert(entry.file_name.clone());
                    if count > 0 {
                        println!(
                            "Attached {count} test results to build {build_key}{}",
                            match entry.test_failures {
                                Some(failures) if failures > 0 => format!(" ({failures} failed)"),
                                _ => String::new(),
                            }
                        );
                    }
                }
                Err(error) => eprintln!("could not attach tests for {build_key}: {error}"),
            }
        }
    }
}

fn import_one(
    log: &std::path::Path,
    store: &mut PostgresStore,
    repo: &std::path::Path,
    collect: &CollectArgs,
    timeout: u64,
) -> Result<bool> {
    collect::wait_until_stable(log, std::time::Duration::from_secs(timeout))?;
    let mut analysis = analysis_for(
        log.to_path_buf(),
        AnalyzeOptions { detail: Detail::Full, no_ai: true },
        Detail::Full,
    )?;
    // Deliberately not defaulting to the directory selector here: `--all`
    // spans several projects, so one name would mislabel all but one of them.
    // Path inference names each log correctly.
    apply_project_name(&mut analysis, collect.project.as_deref());
    analysis.metadata = collect_metadata(collect, repo, Some(log), &analysis);
    reject_unusable_metrics(&analysis, log)?;
    let activity_id = analysis
        .metrics
        .as_ref()
        .and_then(|metrics| metrics.build_id.clone())
        .filter(|id| !id.is_empty());
    if let Some(activity_id) = &activity_id
        && store.build_id_for_activity_log(activity_id)?.is_some()
    {
        return Ok(false);
    }
    store.save_analysis(
        &analysis,
        &project_name_for_storage(&analysis),
        local_machine_id(false),
        false,
    )?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn analysis_with(errors: usize, warnings: usize, failed: usize) -> buildlens_core::BuildAnalysis {
        let mut analysis = buildlens_core::BuildAnalysis::default();
        analysis.diagnostics.raw_errors = errors;
        analysis.diagnostics.raw_warnings = warnings;
        analysis.tests.failed = failed;
        analysis
    }

    /// `--fail-on` is a CI gate, so each policy must check exactly what its
    /// name says. `Failures` is deliberately narrower than `Errors`: a build
    /// that failed to compile produces no test results at all.
    #[test]
    fn each_fail_on_policy_checks_what_it_names() {
        let clean = analysis_with(0, 0, 0);
        let only_errors = analysis_with(3, 0, 0);
        let only_warnings = analysis_with(0, 5, 0);
        let only_failures = analysis_with(0, 0, 2);

        assert!(!Policy::Errors.is_violated_by(&clean));
        assert!(Policy::Errors.is_violated_by(&only_errors));
        assert!(!Policy::Errors.is_violated_by(&only_warnings));
        assert!(!Policy::Errors.is_violated_by(&only_failures));

        assert!(Policy::Warnings.is_violated_by(&only_warnings));
        assert!(!Policy::Warnings.is_violated_by(&only_errors));

        assert!(Policy::Failures.is_violated_by(&only_failures));
        assert!(
            !Policy::Failures.is_violated_by(&only_errors),
            "a compile error produces no test results, so it is not a test failure"
        );
    }

    /// `Any` exists because "the build is broken" spans both a compile error
    /// and a failing test, and neither narrower policy covers both.
    #[test]
    fn the_any_policy_covers_errors_and_failures_together() {
        assert!(Policy::Any.is_violated_by(&analysis_with(1, 0, 0)));
        assert!(Policy::Any.is_violated_by(&analysis_with(0, 0, 1)));
        assert!(!Policy::Any.is_violated_by(&analysis_with(0, 0, 0)));
        assert!(
            !Policy::Any.is_violated_by(&analysis_with(0, 9, 0)),
            "warnings alone are not a broken build"
        );
    }

    /// A crash with no failing test still fails the build.
    #[test]
    fn a_crash_alone_violates_the_failure_policies() {
        let mut analysis = buildlens_core::BuildAnalysis::default();
        analysis.crashes.push(buildlens_core::TestCrash {
            test: Some("CoreTests:testThing".into()),
            crash_type: buildlens_core::CrashType::Signal,
            message: "signal SIGABRT".into(),
            file: None,
            line: None,
        });
        assert!(Policy::Failures.is_violated_by(&analysis));
        assert!(Policy::Any.is_violated_by(&analysis));
        assert!(!Policy::Errors.is_violated_by(&analysis));
    }

    /// The exit code must be distinct from 1, which anyhow uses for a real
    /// error: a CI job needs to tell "the analysis ran and the build is bad"
    /// from "the analysis itself failed".
    #[test]
    fn the_policy_exit_code_is_distinct_from_a_hard_failure() {
        assert_eq!(POLICY_EXIT_CODE, 2);
    }

    /// clap fills `token` from either the flag or the environment, so only the
    /// raw arguments can say which. A token on the command line is visible in
    /// `ps` to every user on the machine.
    #[test]
    fn a_token_argument_is_detected_in_either_form() {
        let args = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert!(token_passed_as_argument(args(&["buildlens", "--token", "secret"]).into_iter()));
        assert!(token_passed_as_argument(args(&["buildlens", "--token=secret"]).into_iter()));
        assert!(!token_passed_as_argument(args(&["buildlens", "collect"]).into_iter()));
        // The environment variable is the safe path and must not warn.
        assert!(!token_passed_as_argument(args(&["buildlens", "--server", "http://x"]).into_iter()));
        // A flag that merely starts with the same letters is not --token.
        assert!(!token_passed_as_argument(args(&["buildlens", "--tokens"]).into_iter()));
    }

    /// `--db` carries a connection URL. As a `PathBuf` it went through
    /// `to_string_lossy`, which silently substitutes replacement characters
    /// and would connect with a string the user never typed.
    #[test]
    fn the_db_argument_is_a_url_not_a_path() {
        let cli = Cli::try_parse_from([
            "buildlens",
            "dashboard",
            "--db",
            "postgres://user:pw@host:5432/db?sslmode=require",
        ])
        .expect("parses");
        match cli.command {
            Command::Dashboard { db, .. } => {
                assert_eq!(db, "postgres://user:pw@host:5432/db?sslmode=require");
            }
            _ => panic!("expected the dashboard command"),
        }
    }

    #[test]
    fn the_db_argument_defaults_to_a_local_postgres() {
        let cli = Cli::try_parse_from(["buildlens", "dashboard"]).expect("parses");
        match cli.command {
            Command::Dashboard { db, port } => {
                assert_eq!(db, "postgres://localhost/buildlens");
                assert_eq!(port, 8787);
            }
            _ => panic!("expected the dashboard command"),
        }
    }

    /// `collect` once declared `project` twice — its DerivedData selector and
    /// the flattened `CollectArgs` name — which made clap's uniqueness
    /// assertion fire on *every* invocation, including `--help`. This check
    /// covers every subcommand, so the next flattened flag that collides is
    /// caught here rather than by a user running the command.
    #[test]
    fn every_command_has_unique_argument_names() {
        Cli::command().debug_assert();
    }

    /// The two names mean different things and must both stay reachable:
    /// `--match-project` picks which log to read, `--project` names the build.
    #[test]
    fn collect_separates_log_selection_from_recorded_name() {
        let cli = Cli::parse_from([
            "buildlens",
            "collect",
            "--match-project",
            "Kaizen",
            "--project",
            "KaizenApp",
        ]);
        let Command::Collect { match_project, collect, .. } = cli.command else {
            panic!("expected the collect subcommand");
        };
        assert_eq!(match_project.as_deref(), Some("Kaizen"));
        assert_eq!(collect.project.as_deref(), Some("KaizenApp"));
    }

    /// With only the selector given, it doubles as the recorded name — better
    /// than a name inferred from a DerivedData path.
    #[test]
    fn match_project_falls_back_to_the_recorded_name() {
        let cli = Cli::parse_from(["buildlens", "collect", "--match-project", "Kaizen"]);
        let Command::Collect { match_project, collect, .. } = cli.command else {
            panic!("expected the collect subcommand");
        };
        assert_eq!(collect.project.as_deref().or(match_project.as_deref()), Some("Kaizen"));
    }

    /// Built through `BuildMetrics::empty` rather than by listing every field,
    /// so adding a field to core does not break this helper.
    fn analysis_from(source_log: Option<&str>, project: Option<&str>) -> buildlens_core::BuildAnalysis {
        let mut metrics = buildlens_core::BuildMetrics::empty(
            buildlens_core::MetricsSourceKind::Xcactivitylog,
            Vec::new(),
        );
        metrics.source_log = source_log.map(str::to_owned);
        metrics.project = project.map(str::to_owned);
        buildlens_core::BuildAnalysis {
            metrics: Some(metrics),
            ..Default::default()
        }
    }

    /// The Postgres save path once hardcoded `"unknown"` whenever `--project`
    /// was absent, so every `collect --all` build landed under one name and the
    /// dashboard's project filter had nothing to separate. Storage must infer
    /// from the DerivedData path, the same rule the `--server` push uses.
    #[test]
    fn storage_infers_the_project_from_a_derived_data_path() {
        let analysis = analysis_from(
            Some("/Users/x/Library/Developer/Xcode/DerivedData/Kaizen-btrrdvosubixawcn/Logs/Build/A.xcactivitylog"),
            None,
        );
        assert_eq!(project_name_for_storage(&analysis), "Kaizen");
    }

    /// An explicit `--project` (already folded into the metrics) still wins.
    #[test]
    fn an_explicit_project_name_beats_path_inference() {
        let analysis = analysis_from(
            Some("/Users/x/Library/Developer/Xcode/DerivedData/Kaizen-btrrdvosubixawcn/Logs/Build/A.xcactivitylog"),
            Some("KaizenApp"),
        );
        assert_eq!(project_name_for_storage(&analysis), "KaizenApp");
    }

    /// Only a log with nothing to infer from falls back to "unknown".
    #[test]
    fn storage_falls_back_to_unknown_without_a_usable_path() {
        assert_eq!(project_name_for_storage(&analysis_from(None, None)), "unknown");
    }
}
