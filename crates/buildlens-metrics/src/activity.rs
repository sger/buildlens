use crate::slf::{IdeActivityLog, IdeSection, cf_to_unix};
use buildlens_core::{
    BuildCategory, BuildMetrics, BuildStepMetric, CacheMetrics, FileMetric, MetricDiagnostic,
    MetricsSourceKind, PhaseMetric, SwiftTimingKind, SwiftTimingMetric, TargetMetric,
};
use regex::Regex;
use std::{collections::HashMap, sync::OnceLock};

/// Caps applied unless the caller asked for full detail.
const MAX_STEPS_PER_TARGET: usize = 20;
const MAX_FILES: usize = 50;
const MAX_SWIFT_TIMINGS: usize = 50;
const MAX_PHASES: usize = 100;

/// Step types excluded when computing the clean/incremental/noop category:
/// they run regardless of whether anything was recompiled, so counting them
/// would make every build look incremental.
const NON_COMPILATION_STEP_TYPES: &[&str] = &["other", "scriptExecution", "copySwiftLibs"];

pub struct MapOptions {
    pub full_detail: bool,
}

pub fn map(log: &IdeActivityLog, warnings: Vec<String>, options: &MapOptions) -> BuildMetrics {
    let mut metrics = BuildMetrics::empty(MetricsSourceKind::Xcactivitylog, warnings);
    let Some(main) = &log.main_section else {
        metrics
            .warnings
            .push("activity log contained no build section".into());
        return metrics;
    };
    // Xcode leaves uniqueIdentifier empty in some logs; the caller substitutes
    // a content hash so builds stay individually identifiable.
    metrics.status = build_status(&main.localized_result_string);
    metrics.build_id = Some(main.unique_identifier.clone()).filter(|id| !id.is_empty());
    metrics.started_at = main.time_started.map(cf_to_unix);
    metrics.ended_at = main.time_stopped.map(cf_to_unix);
    metrics.total_seconds = duration(main);
    (metrics.scheme, metrics.environment.platform) = scheme_and_destination(main);
    // Scraped from compiler invocations during the parse; see
    // `slf::Parser::note_toolchain`.
    metrics.environment.xcode_version = log.toolchain.xcode_version.clone();
    metrics.environment.sdk = log.toolchain.sdk.clone();
    metrics.environment.architecture = log.toolchain.architecture.clone();
    collect(main, None, &mut metrics);
    // Xcode 26 stopped wrapping steps in a section per target: the steps are
    // flat siblings of the root, so the walk above finds no targets at all and
    // every per-target and per-file number comes out empty. Rebuilding the
    // targets from the steps themselves recovers them without assuming any
    // particular nesting.
    if metrics.targets.is_empty() {
        collect_targets_from_steps(main, &mut metrics);
    }
    finish(&mut metrics, options);
    metrics
}

/// Groups steps into targets by the target each step names.
///
/// The fallback for logs with no target wrapper sections. Targets keep the
/// order in which their first step appears, so the output is deterministic
/// rather than dependent on hash iteration order, and each target's span is
/// derived from its steps: the earliest start to the latest end.
fn collect_targets_from_steps(main: &IdeSection, metrics: &mut BuildMetrics) {
    let mut steps: Vec<(String, &IdeSection)> = Vec::new();
    tagged_steps(main, &mut steps);
    let mut order: Vec<String> = Vec::new();
    let mut grouped: HashMap<String, Vec<&IdeSection>> = HashMap::new();
    for (target, section) in steps {
        if !grouped.contains_key(&target) {
            order.push(target.clone());
        }
        grouped.entry(target).or_default().push(section);
    }
    for name in order {
        let sections = &grouped[&name];
        let mut target = TargetMetric {
            fingerprint: format!("target:{name}"),
            name: name.clone(),
            seconds: 0.0,
            started_at: None,
            ended_at: None,
            // A target is only "from cache" if every one of its steps was, which
            // matches how the nested path reads the target section's own flag.
            fetched_from_cache: !sections.is_empty()
                && sections.iter().all(|step| step.was_fetched_from_cache),
            category: BuildCategory::Unknown,
            compiled_count: 0,
            steps: vec![],
        };
        // Without a wrapper section there is no recorded target duration, so it
        // is spanned from the steps. Wall-clock start-to-end, not the sum: steps
        // run concurrently, and summing them reports a target as taking several
        // times longer than the build that contains it.
        let starts = sections.iter().filter_map(|step| step.time_started);
        let ends = sections.iter().filter_map(|step| step.time_stopped);
        let start = starts.fold(f64::INFINITY, f64::min);
        let end = ends.fold(f64::NEG_INFINITY, f64::max);
        if start.is_finite() && end.is_finite() && end >= start {
            target.seconds = end - start;
            target.started_at = Some(cf_to_unix(start));
            target.ended_at = Some(cf_to_unix(end));
        }
        for section in sections {
            collect_step(section, &name, &mut target, metrics);
        }
        metrics.targets.push(target);
    }
}

/// The scheme and destination Xcode recorded for this build.
///
/// Both live in the subtitle of the XCBuild preparation section, formatted as
/// `Workspace X | Scheme Y | Destination Z`. Nothing else in the log states the
/// scheme, so without this the dashboard's scheme column is permanently empty.
///
/// Parsed by splitting on `|` and matching the label rather than by position:
/// a build with no workspace omits that segment entirely, and a positional
/// read would then report the scheme as the workspace.
fn scheme_and_destination(main: &IdeSection) -> (Option<String>, Option<String>) {
    fn labelled(subtitle: &str, label: &str) -> Option<String> {
        subtitle
            .split('|')
            .filter_map(|part| part.trim().strip_prefix(label))
            .map(|value| value.trim().to_owned())
            .find(|value| !value.is_empty())
    }
    for section in &main.sub_sections {
        if section.subtitle.contains("Scheme ") {
            return (
                labelled(&section.subtitle, "Scheme "),
                labelled(&section.subtitle, "Destination "),
            );
        }
    }
    (None, None)
}

/// Xcode's verdict for the build, reduced to the bare word: "Build succeeded"
/// and "Cleanup failed" become "succeeded" and "failed", matching what
/// [`buildlens_core::wire::BuildStatus`] accepts.
///
/// Returns `None` when the log states no result — never "succeeded".
fn build_status(localized_result_string: &str) -> Option<String> {
    // Recognize the verdict itself rather than the noun in front of it. Which
    // nouns Xcode uses is open-ended ("Build", "Cleanup", "Rebuild", localized
    // variants); the set of verdicts is not. Stripping known prefixes instead
    // dropped "Cleanup failed" entirely, and a global `replace` turned it into
    // "up failed" — both reaching the wire as "unknown".
    //
    // Anything else yields `None`: absent means unknown, and a guess would be
    // indistinguishable from a real result.
    const VERDICTS: &[&str] = &["succeeded", "failed", "cancelled"];
    localized_result_string.split_whitespace().find_map(|word| {
        let lowered = word
            .trim_matches(|c: char| !c.is_alphabetic())
            .to_lowercase();
        VERDICTS.contains(&lowered.as_str()).then_some(lowered)
    })
}

fn duration(section: &IdeSection) -> Option<f64> {
    match (section.time_started, section.time_stopped) {
        (Some(start), Some(stop)) if stop >= start => Some(stop - start),
        _ => None,
    }
}

/// The target a *wrapper section* introduces, for logs that nest steps under
/// one section per target.
///
/// Only Xcode's own section titles are matched here; the target a build *step*
/// belongs to comes from [`step_target`] instead.
fn target_name(section: &IdeSection) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE
        .get_or_init(|| Regex::new(r"^Build target (\S+)|^(\S+) \(project ").expect("valid regex"));
    let captures = re.captures(&section.title)?;
    captures
        .get(1)
        .or_else(|| captures.get(2))
        .map(|name| name.as_str().to_owned())
}

/// The target a build step declares in its signature.
///
/// Xcode tags task signatures with `(in target 'Name' from project 'Other')`,
/// optionally followed by ` at path '...'`. This is what makes target
/// attribution independent of how the log is *shaped*: Xcode 16 and earlier
/// nest steps under a "Build target X" section, Xcode 26 emits them as flat
/// siblings of the root with no wrapper at all, and both spell the target the
/// same way here. Xcode 16 logs carry the tag on every step as well, so one
/// rule covers both rather than one rule per Xcode release.
///
/// The name is quoted, so it is read up to the closing quote rather than to
/// whitespace: target names may contain spaces.
fn step_target(section: &IdeSection) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\(in target '([^']+)'").expect("valid regex"));
    // The signature is where Xcode puts the tag; the title is checked too
    // because a few step kinds carry it there and nowhere else.
    re.captures(&section.signature)
        .or_else(|| re.captures(&section.title))
        .map(|captures| captures[1].to_owned())
}

/// Every step under `section`, paired with the target it names.
///
/// Used for logs that have no target wrapper sections: the tree is walked in
/// full and each step is attributed to the target its signature declares.
/// Descent continues through untagged sections so a tagged step nested under
/// one is still found.
fn tagged_steps<'log>(section: &'log IdeSection, found: &mut Vec<(String, &'log IdeSection)>) {
    for sub in &section.sub_sections {
        match step_target(sub) {
            // A tagged step owns its whole subtree: `collect_steps` recurses
            // into children itself, so descending here as well would record
            // every nested step twice.
            Some(target) => found.push((target, sub)),
            None => tagged_steps(sub, found),
        }
    }
}

fn collect(section: &IdeSection, current_target: Option<&str>, metrics: &mut BuildMetrics) {
    for sub in &section.sub_sections {
        if let Some(name) = target_name(sub) {
            let mut target = TargetMetric {
                fingerprint: format!("target:{name}"),
                name,
                seconds: duration(sub).unwrap_or(0.0),
                started_at: sub.time_started.map(cf_to_unix),
                ended_at: sub.time_stopped.map(cf_to_unix),
                fetched_from_cache: sub.was_fetched_from_cache,
                category: BuildCategory::Unknown,
                compiled_count: 0,
                steps: vec![],
            };
            collect_steps(sub, &target.name.clone(), &mut target, metrics);
            metrics.targets.push(target);
        } else if current_target.is_none() {
            // Non-target child of the main build section: a build phase.
            if let Some(seconds) = duration(sub)
                && seconds > 0.0
                && !sub.title.is_empty()
            {
                metrics.phases.push(PhaseMetric {
                    name: sub.title.clone(),
                    seconds,
                    started_at: sub.time_started.map(cf_to_unix),
                    ended_at: sub.time_stopped.map(cf_to_unix),
                });
            }
            collect(sub, None, metrics);
        }
        collect_swift_timings(sub, current_target, metrics);
    }
}

fn collect_steps(
    section: &IdeSection,
    target: &str,
    target_metric: &mut TargetMetric,
    metrics: &mut BuildMetrics,
) {
    for sub in &section.sub_sections {
        collect_step(sub, target, target_metric, metrics);
    }
}

/// Records one build step against `target`, then recurses into its children.
///
/// Split out of [`collect_steps`] so the flat-log path can record a step it
/// located itself. Both paths must produce identical steps, files and
/// diagnostics for the same section — sharing this function is what guarantees
/// that, rather than two copies drifting apart.
fn collect_step(
    sub: &IdeSection,
    target: &str,
    target_metric: &mut TargetMetric,
    metrics: &mut BuildMetrics,
) {
    {
        let step_type = classify_step(sub);
        let file = step_file(sub);
        let seconds = duration(sub).unwrap_or(0.0);
        let identity = file.clone().unwrap_or_else(|| sub.signature.clone());
        let architecture = step_architecture(sub);
        // Architecture belongs in the fingerprint: Xcode emits one step per
        // arch, so without it the arm64 and x86_64 compiles of one file
        // collide and the second silently replaces the first on save.
        let arch_key = architecture.as_deref().unwrap_or("-");
        target_metric.steps.push(BuildStepMetric {
            fingerprint: format!("step:{step_type}:{target}:{arch_key}:{identity}"),
            step_type: step_type.to_owned(),
            title: sub.title.clone(),
            file: file.clone(),
            architecture: architecture.clone(),
            seconds,
            started_at: sub.time_started.map(cf_to_unix),
            ended_at: sub.time_stopped.map(cf_to_unix),
            fetched_from_cache: sub.was_fetched_from_cache,
            warning_count: sub
                .messages
                .iter()
                .filter(|message| message.severity == 1)
                .count(),
            error_count: sub
                .messages
                .iter()
                .filter(|message| message.severity >= 2)
                .count(),
        });
        // Keep the messages themselves, not just how many there were. Counts
        // answer "did this build have warnings"; the dashboard's diagnostics
        // panels need the message, file and line to group recurring ones.
        for message in &sub.messages {
            if message.severity == 0 {
                continue; // notes are not diagnostics
            }
            metrics.diagnostics.push(MetricDiagnostic {
                message: message.title.clone(),
                severity: message.severity,
                file: message
                    .location
                    .as_ref()
                    .map(|location| strip_file_url(&location.document_url)),
                line: message
                    .location
                    .as_ref()
                    .and_then(|location| location.starting_line)
                    .map(|line| line as u32),
                column: message
                    .location
                    .as_ref()
                    .and_then(|location| location.starting_column)
                    .map(|column| column as u32),
                target: Some(target.to_owned()),
            });
        }
        if let Some(file) = file
            && seconds > 0.0
        {
            metrics.files.push(FileMetric {
                file,
                seconds,
                target: Some(target.to_owned()),
                step_type: step_type.to_owned(),
                architecture,
                occurrences: 1,
            });
        }
        collect_swift_timings(sub, Some(target), metrics);
        collect_steps(sub, target, target_metric, metrics);
    }
}

fn classify_step(section: &IdeSection) -> &'static str {
    let signature = if section.signature.is_empty() {
        &section.title
    } else {
        &section.signature
    };
    let rules: &[(&str, &str)] = &[
        ("SwiftCompile", "swiftCompilation"),
        ("CompileSwift", "swiftCompilation"),
        ("SwiftDriver", "swiftCompilation"),
        ("SwiftEmitModule", "swiftCompilation"),
        ("CompileC", "cCompilation"),
        ("CompileAssetCatalog", "compileAssetCatalog"),
        ("CompileStoryboard", "compileStoryboard"),
        ("CompileXIB", "compileXIB"),
        ("PrecompileModule", "cCompilation"),
        ("ScanDependencies", "scanDependencies"),
        ("Ld ", "linker"),
        ("Libtool", "linker"),
        ("PhaseScriptExecution", "scriptExecution"),
        ("CopySwiftLibs", "copySwiftLibs"),
        ("CpResource", "copy"),
        ("Copy ", "copy"),
        ("CpHeader", "copy"),
        ("PBXCp", "copy"),
        ("CodeSign", "codeSign"),
        ("ProcessInfoPlistFile", "processInfoPlist"),
        ("Touch", "touch"),
        ("WriteAuxiliaryFile", "writeAuxiliaryFile"),
        ("CreateBuildDirectory", "createBuildDirectory"),
        ("RegisterExecutionPolicyException", "other"),
        ("Validate", "validate"),
    ];
    for (needle, step_type) in rules {
        if signature.starts_with(needle) || section.title.starts_with(needle) {
            return step_type;
        }
    }
    "other"
}

fn step_file(section: &IdeSection) -> Option<String> {
    if let Some(location) = &section.location
        && !location.document_url.is_empty()
    {
        return Some(strip_file_url(&location.document_url));
    }
    // Signatures like "CompileSwift normal arm64 /path/File.swift ..." carry the path.
    section
        .signature
        .split_whitespace()
        .find(|part| part.starts_with('/') && part.contains('.'))
        .map(str::to_owned)
}

fn strip_file_url(url: &str) -> String {
    url.strip_prefix("file://").unwrap_or(url).to_owned()
}

/// Architectures Xcode actually emits. Matching an allowlist rather than
/// "any trailing parenthesised word" keeps titles like
/// `Copy Foo.h (bridging)` from being recorded as an architecture.
const KNOWN_ARCHITECTURES: &[&str] = &[
    "arm64", "arm64e", "arm64_32", "armv7", "armv7k", "armv7s", "x86_64", "x86_64h", "i386",
];

/// Architecture for a step, or `None` when the log does not name one.
///
/// Two shapes carry it: a trailing `(arm64)` on the title (`Compile
/// Exports.swift (arm64)`) and a bare token in the signature
/// (`CompileSwift normal arm64 /path/File.swift`). Arch-independent steps
/// — module emission, most copies — legitimately have neither, and are
/// left `None` rather than being attributed to a guessed architecture.
fn step_architecture(section: &IdeSection) -> Option<String> {
    if let Some(rest) = section.title.rsplit_once('(') {
        let token = rest.1.trim_end().trim_end_matches(')');
        if KNOWN_ARCHITECTURES.contains(&token) {
            return Some(token.to_owned());
        }
    }
    section
        .signature
        .split_whitespace()
        .find(|part| KNOWN_ARCHITECTURES.contains(part))
        .map(str::to_owned)
}

fn collect_swift_timings(section: &IdeSection, target: Option<&str>, metrics: &mut BuildMetrics) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?:(.+?) '([^']+)' )?took (\d+)ms to type-check").expect("valid regex")
    });
    for message in &section.messages {
        let Some(captures) = re.captures(&message.title) else {
            continue;
        };
        let milliseconds: f64 = captures[3].parse().unwrap_or(0.0);
        let symbol = captures.get(2).map(|symbol| symbol.as_str().to_owned());
        let kind = if symbol.is_some() {
            SwiftTimingKind::FunctionBody
        } else {
            SwiftTimingKind::TypeCheck
        };
        let (file, line, column) = match &message.location {
            Some(location) => (
                strip_file_url(&location.document_url),
                location.starting_line.unwrap_or(0) as u32,
                location.starting_column.unwrap_or(0) as u32,
            ),
            None => (String::new(), 0, 0),
        };
        metrics.swift_timings.push(SwiftTimingMetric {
            kind,
            file,
            line,
            column,
            symbol,
            milliseconds,
            target: target.map(str::to_owned),
        });
    }
}

fn finish(metrics: &mut BuildMetrics, options: &MapOptions) {
    // Roll step diagnostics up to the build. Xcode records errors on the steps
    // that produced them and never states an overall verdict, so this sum is
    // what makes a failed build recognisable downstream.
    metrics.error_count = metrics
        .targets
        .iter()
        .flat_map(|target| &target.steps)
        .map(|step| step.error_count)
        .sum();
    metrics.warning_count = metrics
        .targets
        .iter()
        .flat_map(|target| &target.steps)
        .map(|step| step.warning_count)
        .sum();
    let mut total_steps = 0usize;
    let mut cached_steps = 0usize;
    let mut clean_targets = 0usize;
    let mut noop_targets = 0usize;
    for target in &mut metrics.targets {
        // Only compilation-relevant steps decide the category.
        let relevant: Vec<&BuildStepMetric> = target
            .steps
            .iter()
            .filter(|step| !NON_COMPILATION_STEP_TYPES.contains(&step.step_type.as_str()))
            .collect();
        let compiled = relevant
            .iter()
            .filter(|step| !step.fetched_from_cache)
            .count();
        target.compiled_count = compiled;
        target.category = if relevant.is_empty() || compiled == 0 {
            BuildCategory::Noop
        } else if compiled == relevant.len() {
            BuildCategory::Clean
        } else {
            BuildCategory::Incremental
        };
        target.fetched_from_cache =
            !target.steps.is_empty() && target.steps.iter().all(|step| step.fetched_from_cache);
        total_steps += target.steps.len();
        cached_steps += target
            .steps
            .iter()
            .filter(|step| step.fetched_from_cache)
            .count();
        match target.category {
            BuildCategory::Clean => clean_targets += 1,
            BuildCategory::Noop => noop_targets += 1,
            _ => {}
        }
        metrics.compiled_count += compiled;
    }
    let total_targets = metrics.targets.len();
    metrics.category = if total_targets == 0 {
        BuildCategory::Unknown
    } else if noop_targets == total_targets {
        BuildCategory::Noop
    } else if clean_targets > total_targets / 2 {
        BuildCategory::Clean
    } else {
        BuildCategory::Incremental
    };
    if total_steps > 0 {
        let hit_rate = cached_steps as f64 / total_steps as f64;
        metrics.cache = CacheMetrics {
            status: if cached_steps > 0 {
                "partial".into()
            } else {
                "cold".into()
            },
            hit_rate: Some(hit_rate),
        };
        if cached_steps == total_steps {
            metrics.cache.status = "warm".into();
        }
    }
    // Repeated section titles (one per package/platform pass) merge into one phase entry.
    let mut merged: std::collections::BTreeMap<String, PhaseMetric> = Default::default();
    for phase in metrics.phases.drain(..) {
        merged
            .entry(phase.name.clone())
            .and_modify(|existing| {
                existing.seconds += phase.seconds;
                existing.started_at = match (existing.started_at, phase.started_at) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                existing.ended_at = match (existing.ended_at, phase.ended_at) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (a, b) => a.or(b),
                };
            })
            .or_insert(phase);
    }
    metrics.phases = merged.into_values().collect();
    metrics
        .phases
        .sort_by(|a, b| b.seconds.total_cmp(&a.seconds));
    if !options.full_detail {
        note_truncation(metrics, "build phases", metrics.phases.len(), MAX_PHASES);
        metrics.phases.truncate(MAX_PHASES);
    }
    // The same file compiles in several variants; key on architecture too so
    // arm64 and x86_64 stay separate rows. Collapsing them to the slowest
    // observation (as this once did) made "this file is slow" and "this file
    // compiles four times" indistinguishable. Variants that repeat *within*
    // one architecture — module passes — still merge, keeping the slowest and
    // counting occurrences.
    let mut unique_files: std::collections::BTreeMap<(String, String, Option<String>), FileMetric> =
        Default::default();
    for file in metrics.files.drain(..) {
        let key = (
            file.file.clone(),
            file.step_type.clone(),
            file.architecture.clone(),
        );
        unique_files
            .entry(key)
            .and_modify(|existing| {
                existing.occurrences += 1;
                if file.seconds > existing.seconds {
                    existing.seconds = file.seconds;
                    existing.target = file.target.clone();
                }
            })
            .or_insert(file);
    }
    metrics.files = unique_files.into_values().collect();
    metrics
        .files
        .sort_by(|a, b| b.seconds.total_cmp(&a.seconds));
    metrics
        .swift_timings
        .sort_by(|a, b| b.milliseconds.total_cmp(&a.milliseconds));
    metrics
        .targets
        .sort_by(|a, b| b.seconds.total_cmp(&a.seconds));
    if !options.full_detail {
        // Truncation is recorded, not silent: a capped result would otherwise
        // be indistinguishable from a complete one, and someone reading "50
        // files" has no way to know the build compiled four thousand.
        note_truncation(metrics, "per-file timings", metrics.files.len(), MAX_FILES);
        metrics.files.truncate(MAX_FILES);
        note_truncation(
            metrics,
            "Swift type-check timings",
            metrics.swift_timings.len(),
            MAX_SWIFT_TIMINGS,
        );
        metrics.swift_timings.truncate(MAX_SWIFT_TIMINGS);
        let mut dropped_steps = 0usize;
        for target in &mut metrics.targets {
            target.steps.sort_by(|a, b| b.seconds.total_cmp(&a.seconds));
            dropped_steps += target.steps.len().saturating_sub(MAX_STEPS_PER_TARGET);
            target.steps.truncate(MAX_STEPS_PER_TARGET);
        }
        if dropped_steps > 0 {
            metrics.truncations.push(format!(
                "kept the slowest {MAX_STEPS_PER_TARGET} steps per target; \
                 {dropped_steps} slower-to-report steps omitted (use --detail full for all)"
            ));
        }
    }
}

/// Records that a ranked list was capped, naming what was dropped.
fn note_truncation(metrics: &mut BuildMetrics, what: &str, actual: usize, cap: usize) {
    if actual > cap {
        metrics.truncations.push(format!(
            "kept the slowest {cap} of {actual} {what} (use --detail full for all)"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slf::IdeSection;

    /// The exact strings Xcode writes, verified against real logs: a failing
    /// sample-app build and the packagesbench fixture.
    /// The scheme lives only in the preparation section's subtitle. Nothing
    /// else in the log names it, so without this the dashboard's scheme column
    /// is permanently empty.
    #[test]
    fn the_scheme_and_destination_come_from_the_preparation_subtitle() {
        let mut main = IdeSection::default();
        main.sub_sections.push(IdeSection {
            subtitle: "Workspace TVToday | Scheme MyScheme | Destination iPhone 17 Pro".into(),
            ..Default::default()
        });
        let (scheme, destination) = scheme_and_destination(&main);
        assert_eq!(scheme.as_deref(), Some("MyScheme"));
        assert_eq!(destination.as_deref(), Some("iPhone 17 Pro"));
    }

    /// A build with no workspace omits that segment, so reading by position
    /// would report the scheme as the workspace.
    #[test]
    fn a_missing_workspace_segment_does_not_shift_the_scheme() {
        let mut main = IdeSection::default();
        main.sub_sections.push(IdeSection {
            subtitle: "Scheme OnlyScheme | Destination My Mac".into(),
            ..Default::default()
        });
        assert_eq!(
            scheme_and_destination(&main).0.as_deref(),
            Some("OnlyScheme")
        );
    }

    /// A log that never names a scheme reports none rather than an empty
    /// string, which would render as a blank column that looks like data.
    #[test]
    fn a_log_without_a_scheme_reports_none() {
        assert_eq!(scheme_and_destination(&IdeSection::default()), (None, None));
        let mut main = IdeSection::default();
        main.sub_sections.push(IdeSection {
            subtitle: "Prepare build".into(),
            ..Default::default()
        });
        assert_eq!(scheme_and_destination(&main), (None, None));
    }

    #[test]
    fn build_status_matches_xcodes_own_verdict() {
        assert_eq!(build_status("Build failed").as_deref(), Some("failed"));
        assert_eq!(
            build_status("Build succeeded").as_deref(),
            Some("succeeded")
        );
        assert_eq!(
            build_status("Clean succeeded").as_deref(),
            Some("succeeded")
        );
        // A log that states no result must stay unknown, never "succeeded".
        assert_eq!(build_status(""), None);
        assert_eq!(build_status("Build"), None);
    }

    /// A global `replace` mangled any verdict whose noun appeared mid-word.
    /// `BuildStatus::parse` accepts only the three known verdicts, so each of
    /// these silently became "unknown".
    #[test]
    fn a_noun_inside_another_word_is_not_stripped() {
        assert_eq!(build_status("Cleanup failed").as_deref(), Some("failed"));
        assert_eq!(
            build_status("Rebuild succeeded").as_deref(),
            Some("succeeded")
        );
        assert_eq!(build_status("Prebuild failed").as_deref(), Some("failed"));
    }

    #[test]
    fn detail_after_the_verdict_is_dropped() {
        assert_eq!(
            build_status("Build succeeded with 3 warnings").as_deref(),
            Some("succeeded")
        );
    }

    #[test]
    fn other_xcode_actions_are_normalized_too() {
        assert_eq!(build_status("Test failed").as_deref(), Some("failed"));
        assert_eq!(
            build_status("Archive succeeded").as_deref(),
            Some("succeeded")
        );
        assert_eq!(
            build_status("Build cancelled").as_deref(),
            Some("cancelled")
        );
    }

    /// Every verdict this produces must round-trip through the wire type;
    /// otherwise a real result is transmitted as "unknown".
    #[test]
    fn every_verdict_is_understood_by_the_wire_type() {
        use buildlens_core::wire::BuildStatus;
        for input in [
            "Build succeeded",
            "Build failed",
            "Clean succeeded",
            "Build cancelled",
            "Cleanup failed",
            "Rebuild succeeded",
            "Build succeeded with 3 warnings",
        ] {
            let status = build_status(input).expect("a verdict");
            assert!(
                BuildStatus::parse(&status).is_some(),
                "wire type rejected {status:?} from {input:?}"
            );
        }
    }

    #[test]
    fn unrecognized_wording_yields_no_verdict_rather_than_a_guess() {
        assert_eq!(build_status("Something entirely else"), None);
        assert_eq!(build_status("   "), None);
    }

    fn step(step_type_title: &str, cached: bool) -> IdeSection {
        IdeSection {
            title: step_type_title.to_owned(),
            signature: step_type_title.to_owned(),
            time_started: Some(0.0),
            time_stopped: Some(1.0),
            was_fetched_from_cache: cached,
            ..Default::default()
        }
    }

    fn target(name: &str, steps: Vec<IdeSection>) -> IdeSection {
        IdeSection {
            title: format!("Build target {name}"),
            time_started: Some(0.0),
            time_stopped: Some(10.0),
            sub_sections: steps,
            ..Default::default()
        }
    }

    fn build(targets: Vec<IdeSection>) -> IdeActivityLog {
        IdeActivityLog {
            toolchain: Default::default(),
            version: 12,
            main_section: Some(IdeSection {
                title: "Build App".into(),
                unique_identifier: "UID".into(),
                time_started: Some(0.0),
                time_stopped: Some(100.0),
                sub_sections: targets,
                ..Default::default()
            }),
        }
    }

    fn category_of(log: &IdeActivityLog) -> BuildCategory {
        map(log, vec![], &MapOptions { full_detail: false }).category
    }

    /// A step as Xcode 26 emits it: no target wrapper, the target named in the
    /// signature's `(in target '...')` tag.
    fn flat_step(title: &str, signature: &str, target: &str, start: f64, stop: f64) -> IdeSection {
        IdeSection {
            title: title.to_owned(),
            signature: format!("{signature} (in target '{target}' from project 'P')"),
            time_started: Some(start),
            time_stopped: Some(stop),
            ..Default::default()
        }
    }

    /// Xcode 26 stopped wrapping build steps in a section per target: they are
    /// flat siblings of the root. The whole per-target and per-file half of the
    /// dashboard read as empty, and the build's category fell back to
    /// "unknown", because the walk found no target sections to descend into.
    #[test]
    fn targets_are_recovered_from_flat_steps_without_wrapper_sections() {
        let log = build(vec![
            flat_step(
                "Compile A.swift (arm64)",
                "SwiftCompile normal arm64 /src/A.swift",
                "Alpha",
                0.0,
                3.0,
            ),
            flat_step(
                "Compile B.swift (arm64)",
                "SwiftCompile normal arm64 /src/B.swift",
                "Beta",
                1.0,
                2.0,
            ),
            flat_step(
                "Compile C.swift (arm64)",
                "SwiftCompile normal arm64 /src/C.swift",
                "Alpha",
                3.0,
                7.0,
            ),
        ]);
        let metrics = map(&log, vec![], &MapOptions { full_detail: true });
        let names: Vec<&str> = metrics
            .targets
            .iter()
            .map(|target| target.name.as_str())
            .collect();
        // Grouped by name, in the order each target's first step appeared.
        assert_eq!(names, vec!["Alpha", "Beta"]);
        assert_eq!(metrics.targets[0].steps.len(), 2);
        assert_eq!(metrics.targets[1].steps.len(), 1);
        // Files must be attributed to the target that compiled them. Sorted
        // here because the file list is ranked by duration, not input order.
        let mut alpha_files: Vec<&str> = metrics
            .files
            .iter()
            .filter(|file| file.target.as_deref() == Some("Alpha"))
            .map(|file| file.file.as_str())
            .collect();
        alpha_files.sort_unstable();
        assert_eq!(alpha_files, vec!["/src/A.swift", "/src/C.swift"]);
        assert_eq!(
            metrics
                .files
                .iter()
                .filter(|file| file.target.as_deref() == Some("Beta"))
                .count(),
            1
        );
        // And a build with real targets is no longer "unknown".
        assert_eq!(metrics.category, BuildCategory::Clean);
    }

    /// A target's duration spans its steps rather than summing them: steps run
    /// concurrently, and a sum reports a target as taking longer than the
    /// build that contains it.
    #[test]
    fn a_flat_targets_duration_spans_its_steps_rather_than_summing_them() {
        let log = build(vec![
            flat_step(
                "Compile A",
                "SwiftCompile normal arm64 /src/A.swift",
                "T",
                2.0,
                6.0,
            ),
            flat_step(
                "Compile B",
                "SwiftCompile normal arm64 /src/B.swift",
                "T",
                3.0,
                5.0,
            ),
        ]);
        let metrics = map(&log, vec![], &MapOptions { full_detail: true });
        assert_eq!(metrics.targets.len(), 1);
        // 2.0 -> 6.0 wall clock, not 4.0 + 2.0 summed.
        assert!((metrics.targets[0].seconds - 4.0).abs() < f64::EPSILON);
    }

    /// Xcode 16 logs carry the `(in target '...')` tag on their steps *and*
    /// nest them under "Build target X" sections. The wrapper must keep
    /// winning, or every existing log would regroup and change shape.
    #[test]
    fn wrapper_sections_take_precedence_over_step_tags() {
        let mut nested = step("Compile A.swift", false);
        nested.signature =
            "SwiftCompile normal arm64 /src/A.swift (in target 'Inner' from project 'P')".into();
        let log = build(vec![target("Outer", vec![nested])]);
        let metrics = map(&log, vec![], &MapOptions { full_detail: true });
        assert_eq!(metrics.targets.len(), 1);
        assert_eq!(metrics.targets[0].name, "Outer");
    }

    /// A tagged step owns its subtree. Descending into it as well as recording
    /// it would count every nested step twice.
    #[test]
    fn steps_nested_under_a_tagged_step_are_not_counted_twice() {
        let mut parent = flat_step(
            "Compiling A.swift",
            "SwiftCompile normal arm64 Compiling A.swift",
            "T",
            0.0,
            4.0,
        );
        let mut child = flat_step(
            "Compile A.swift (arm64)",
            "SwiftCompile normal arm64 /src/A.swift",
            "T",
            0.0,
            4.0,
        );
        child.sub_sections = vec![];
        parent.sub_sections = vec![child];
        let log = build(vec![parent]);
        let metrics = map(&log, vec![], &MapOptions { full_detail: true });
        assert_eq!(metrics.targets.len(), 1);
        // Parent plus its one child, recorded once each.
        assert_eq!(metrics.targets[0].steps.len(), 2);
        assert_eq!(
            metrics
                .files
                .iter()
                .filter(|file| file.file == "/src/A.swift")
                .count(),
            1
        );
    }

    /// Target names may contain spaces, so the quoted name is read to its
    /// closing quote rather than to the next whitespace.
    #[test]
    fn a_target_name_containing_spaces_is_read_whole() {
        let section = flat_step(
            "Compile A.swift",
            "SwiftCompile normal arm64 /src/A.swift",
            "My App Extension",
            0.0,
            1.0,
        );
        assert_eq!(step_target(&section).as_deref(), Some("My App Extension"));
    }

    /// Steps that name no target must not invent one, and must not stop the
    /// walk from reaching tagged steps nested beneath them.
    #[test]
    fn untagged_steps_are_skipped_but_still_descended_into() {
        assert_eq!(step_target(&step("CreateBuildDirectory", false)), None);
        let mut untagged = step("Create build description", false);
        untagged.sub_sections = vec![flat_step(
            "Compile A.swift",
            "SwiftCompile normal arm64 /src/A.swift",
            "Buried",
            0.0,
            1.0,
        )];
        let log = build(vec![untagged]);
        let metrics = map(&log, vec![], &MapOptions { full_detail: true });
        assert_eq!(metrics.targets.len(), 1);
        assert_eq!(metrics.targets[0].name, "Buried");
    }

    /// A flat target is only "from cache" when every one of its steps was,
    /// matching how the nested path reads the target section's own flag.
    #[test]
    fn a_flat_target_is_cached_only_when_all_of_its_steps_are() {
        let cached = |target: &str, cached: bool| {
            let mut section = flat_step(
                "Compile X.swift",
                "SwiftCompile normal arm64 /src/X.swift",
                target,
                0.0,
                1.0,
            );
            section.was_fetched_from_cache = cached;
            section
        };
        let log = build(vec![
            cached("AllCached", true),
            cached("Mixed", true),
            cached("Mixed", false),
        ]);
        let metrics = map(&log, vec![], &MapOptions { full_detail: true });
        let by_name = |name: &str| {
            metrics
                .targets
                .iter()
                .find(|target| target.name == name)
                .expect("target")
        };
        assert!(by_name("AllCached").fetched_from_cache);
        assert!(!by_name("Mixed").fetched_from_cache);
    }

    /// A file compiled for two architectures must stay two rows. Collapsing
    /// them (as this once did, keeping only the slowest) made a file that
    /// compiles twice look identical to one slow compile.
    #[test]
    fn multi_arch_file_compiles_are_not_collapsed() {
        let mut arm = step("Compile Thing.swift (arm64)", false);
        arm.signature = "CompileSwift normal arm64 /src/Thing.swift".into();
        arm.time_stopped = Some(4.0);
        let mut intel = step("Compile Thing.swift (x86_64)", false);
        intel.signature = "CompileSwift normal x86_64 /src/Thing.swift".into();
        intel.time_stopped = Some(1.0);
        let log = build(vec![target("App", vec![arm, intel])]);
        let metrics = map(&log, vec![], &MapOptions { full_detail: false });

        let rows: Vec<_> = metrics
            .files
            .iter()
            .filter(|f| f.file == "/src/Thing.swift")
            .collect();
        assert_eq!(
            rows.len(),
            2,
            "expected one row per architecture: {rows:#?}"
        );
        let mut architectures: Vec<_> = rows
            .iter()
            .filter_map(|f| f.architecture.as_deref())
            .collect();
        architectures.sort_unstable();
        assert_eq!(architectures, vec!["arm64", "x86_64"]);
        // The slow arch keeps its own time rather than masking the fast one.
        let arm_row = rows
            .iter()
            .find(|f| f.architecture.as_deref() == Some("arm64"))
            .expect("arm64 row present");
        assert!((arm_row.seconds - 4.0).abs() < f64::EPSILON);
        // Distinct architectures must not collide in the step fingerprint.
        let fingerprints: std::collections::BTreeSet<_> = metrics.targets[0]
            .steps
            .iter()
            .map(|s| s.fingerprint.as_str())
            .collect();
        assert_eq!(fingerprints.len(), metrics.targets[0].steps.len());
    }

    /// Repeats within one architecture (module passes) still merge, but the
    /// row records that more than one compilation happened.
    #[test]
    fn repeats_within_one_arch_merge_and_count_occurrences() {
        let mut first = step("Compile Thing.swift (arm64)", false);
        first.signature = "CompileSwift normal arm64 /src/Thing.swift".into();
        first.time_stopped = Some(2.0);
        let mut second = step("Compile Thing.swift (arm64)", false);
        second.signature = "CompileSwift normal arm64 /src/Thing.swift".into();
        second.time_stopped = Some(5.0);
        let log = build(vec![target("App", vec![first, second])]);
        let metrics = map(&log, vec![], &MapOptions { full_detail: false });

        let rows: Vec<_> = metrics
            .files
            .iter()
            .filter(|f| f.file == "/src/Thing.swift")
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].occurrences, 2);
        assert!(
            (rows[0].seconds - 5.0).abs() < f64::EPSILON,
            "keeps the slowest"
        );
    }

    /// Steps that are genuinely architecture-independent stay `None`; a
    /// guessed architecture would be worse than an absent one.
    #[test]
    fn arch_independent_steps_have_no_architecture() {
        let mut emit = step("Emitting module for App", false);
        emit.signature = "EmitSwiftModule normal /src/App.swiftmodule".into();
        let log = build(vec![target("App", vec![emit])]);
        let metrics = map(&log, vec![], &MapOptions { full_detail: false });
        assert!(
            metrics.targets[0]
                .steps
                .iter()
                .all(|s| s.architecture.is_none())
        );
    }

    /// A trailing parenthesised word is only an architecture if it is one.
    #[test]
    fn non_arch_parenthetical_is_not_read_as_architecture() {
        let mut copy = step("Copy Bridging.h (bridging)", false);
        copy.signature = "PBXCp /src/Bridging.h".into();
        let log = build(vec![target("App", vec![copy])]);
        let metrics = map(&log, vec![], &MapOptions { full_detail: false });
        assert!(
            metrics.targets[0]
                .steps
                .iter()
                .all(|s| s.architecture.is_none())
        );
    }

    #[test]
    fn all_cached_targets_are_noop() {
        let log = build(vec![
            target("A", vec![step("CompileSwift a", true)]),
            target("B", vec![step("CompileSwift b", true)]),
        ]);
        assert_eq!(category_of(&log), BuildCategory::Noop);
    }

    #[test]
    fn majority_clean_targets_make_clean_build() {
        let log = build(vec![
            target("A", vec![step("CompileSwift a", false)]),
            target("B", vec![step("CompileSwift b", false)]),
            target(
                "C",
                vec![
                    step("CompileSwift c1", false),
                    step("CompileSwift c2", true),
                ],
            ),
        ]);
        assert_eq!(category_of(&log), BuildCategory::Clean);
    }

    #[test]
    fn mixed_targets_make_incremental_build() {
        let log = build(vec![
            target("A", vec![step("CompileSwift a", false)]),
            target("B", vec![step("CompileSwift b", true)]),
        ]);
        assert_eq!(category_of(&log), BuildCategory::Incremental);
    }

    #[test]
    fn script_only_targets_do_not_count_as_compiled() {
        // scriptExecution steps are excluded from categorization, so this target is noop.
        let log = build(vec![target(
            "A",
            vec![step("PhaseScriptExecution lint", false)],
        )]);
        assert_eq!(category_of(&log), BuildCategory::Noop);
    }

    #[test]
    fn swift_timing_messages_are_extracted() {
        use crate::slf::{DvtLocation, IdeMessage};
        let mut section = target("A", vec![]);
        section.sub_sections.push(IdeSection {
            title: "CompileSwift normal arm64 /tmp/File.swift".into(),
            signature: "CompileSwift normal arm64 /tmp/File.swift".into(),
            time_started: Some(0.0),
            time_stopped: Some(2.0),
            messages: vec![IdeMessage {
                title: "instance method 'slow()' took 240ms to type-check (limit: 100ms)".into(),
                location: Some(DvtLocation {
                    document_url: "file:///tmp/File.swift".into(),
                    starting_line: Some(41),
                    starting_column: Some(9),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        });
        let log = build(vec![section]);
        let metrics = map(&log, vec![], &MapOptions { full_detail: false });
        assert_eq!(metrics.swift_timings.len(), 1);
        let timing = &metrics.swift_timings[0];
        assert_eq!(timing.kind, SwiftTimingKind::FunctionBody);
        assert_eq!(timing.symbol.as_deref(), Some("slow()"));
        assert_eq!(timing.milliseconds, 240.0);
        assert_eq!(timing.file, "/tmp/File.swift");
        assert_eq!(timing.line, 41);
    }
}

#[cfg(test)]
mod swift_timing_tests {
    use super::*;
    use crate::slf::{DvtLocation, IdeMessage, IdeSection};

    /// Verbatim warning titles emitted by Xcode 26.3 with
    /// `-warn-long-function-bodies=10 -warn-long-expression-type-checking=10`.
    const REAL_WARNINGS: &[(&str, u32, u32, Option<&str>, f64)] = &[
        (
            "expression took 18ms to type-check (limit: 10ms)",
            8,
            17,
            None,
            18.0,
        ),
        (
            "static method 'gnarly()' took 34ms to type-check (limit: 10ms)",
            6,
            24,
            Some("gnarly()"),
            34.0,
        ),
        (
            "expression took 74ms to type-check (limit: 10ms)",
            15,
            22,
            None,
            74.0,
        ),
        (
            "static method 'nested()' took 75ms to type-check (limit: 10ms)",
            14,
            24,
            Some("nested()"),
            75.0,
        ),
    ];

    #[test]
    fn extracts_real_xcode_swift_timing_warnings() {
        let messages = REAL_WARNINGS
            .iter()
            .map(|(title, line, column, _, _)| IdeMessage {
                title: (*title).into(),
                location: Some(DvtLocation {
                    document_url: "file:///tmp/SlowApp/Slow.swift".into(),
                    starting_line: Some(u64::from(*line)),
                    starting_column: Some(u64::from(*column)),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .collect();
        let log = IdeActivityLog {
            toolchain: Default::default(),
            version: 12,
            main_section: Some(IdeSection {
                title: "Build SlowApp".into(),
                time_started: Some(0.0),
                time_stopped: Some(10.0),
                sub_sections: vec![IdeSection {
                    title: "Build target SlowApp".into(),
                    time_started: Some(0.0),
                    time_stopped: Some(9.0),
                    sub_sections: vec![IdeSection {
                        title: "CompileSwift normal arm64 /tmp/SlowApp/Slow.swift".into(),
                        signature: "CompileSwift normal arm64 /tmp/SlowApp/Slow.swift".into(),
                        time_started: Some(0.0),
                        time_stopped: Some(8.0),
                        messages,
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }),
        };
        let metrics = map(&log, vec![], &MapOptions { full_detail: true });
        assert_eq!(metrics.swift_timings.len(), REAL_WARNINGS.len());
        for (timing, (_, line, column, symbol, milliseconds)) in metrics.swift_timings.iter().zip(
            // map() sorts by duration descending; compare against the same order.
            {
                let mut expected = REAL_WARNINGS.to_vec();
                expected.sort_by(|a, b| b.4.total_cmp(&a.4));
                expected
            },
        ) {
            assert_eq!(timing.milliseconds, milliseconds);
            assert_eq!(timing.symbol.as_deref(), symbol);
            assert_eq!(timing.line, line);
            assert_eq!(timing.column, column);
            assert_eq!(timing.file, "/tmp/SlowApp/Slow.swift");
            assert_eq!(timing.target.as_deref(), Some("SlowApp"));
            assert_eq!(
                timing.kind,
                if symbol.is_some() {
                    SwiftTimingKind::FunctionBody
                } else {
                    SwiftTimingKind::TypeCheck
                }
            );
        }
    }
}
