//! Rendering a [`BuildAnalysis`] for humans.
//!
//! Three formats over one analysis: JSON for machines, a terminal summary, and
//! markdown for pasting into a pull request. The section builders are separate
//! functions rather than one large format string so each can be asserted on
//! its own, and so a section with nothing to say can return `""` and disappear
//! instead of printing an empty heading.

use buildlens_core::{BuildAnalysis, BuildMetrics, GitCorrelation, GitOwnership, Intelligence};

/// Caps on list rendering.
///
/// A report is read, not queried: past a handful of rows the reader stops
/// counting, and the full data is in the JSON. Every ranked list is capped —
/// bottlenecks and ownership were previously unbounded, so a build with two
/// hundred diagnostics produced two hundred bullets.
const MAX_IMPACTS: usize = 5;
const MAX_BOTTLENECKS: usize = 5;
const MAX_CHAINS: usize = 3;
const MAX_OWNERSHIP: usize = 5;
const MAX_SWIFT_TIMINGS: usize = 10;
const MAX_SLOWEST_TARGETS: usize = 5;

/// Serializes the analysis as pretty-printed JSON.
///
/// Returns `Err` rather than falling back to `"{}"`. An empty object is a
/// *valid, clean* build report, so a caller writing it to storage would record
/// "nothing found" for a run that actually failed to serialize — the worst
/// possible value to fail to.
///
/// Note that non-finite `f64` durations are not an error: JSON has no `NaN`,
/// so `serde_json` writes `null`. That is the honest encoding, but it means a
/// bad duration is absorbed silently rather than reported.
pub fn json(analysis: &BuildAnalysis) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(analysis)
}

/// Notes multi-architecture builds next to the file count.
///
/// Without it a jump from 400 to 800 measured files reads as "the build got
/// bigger" when it only means a second architecture was added.
fn architecture_note(metrics: &BuildMetrics) -> String {
    let architectures: std::collections::BTreeSet<&str> = metrics
        .files
        .iter()
        .filter_map(|file| file.architecture.as_deref())
        .collect();
    if architectures.len() < 2 {
        return String::new();
    }
    format!(
        " (across {} architectures: {})",
        architectures.len(),
        architectures.into_iter().collect::<Vec<_>>().join(", ")
    )
}

/// Slowest Swift function bodies and type-check expressions.
///
/// Only produced when the build recorded them, which requires
/// `-warn-long-function-bodies` or `-warn-long-expression-type-checking`.
/// Repeated locations are summed because one function is reported once per
/// compilation, so a file built for two architectures reports twice.
fn swift_timings_terminal(metrics: &BuildMetrics) -> String {
    use std::collections::BTreeMap;
    if metrics.swift_timings.is_empty() {
        return String::new();
    }
    /// One source location's summed cost across every compilation of it.
    #[derive(Default)]
    struct Total<'a> {
        milliseconds: f64,
        occurrences: usize,
        symbol: Option<&'a str>,
    }
    let mut totals: BTreeMap<(&str, u32, u32), Total<'_>> = BTreeMap::new();
    for timing in &metrics.swift_timings {
        let entry = totals
            .entry((timing.file.as_str(), timing.line, timing.column))
            .or_default();
        entry.milliseconds += timing.milliseconds;
        entry.occurrences += 1;
        entry.symbol = entry.symbol.or(timing.symbol.as_deref());
    }
    let mut ranked: Vec<_> = totals.into_iter().collect();
    // Ties break on the location key. The `BTreeMap` already yields keys in
    // order and `sort_by` is stable, so this is belt-and-braces rather than
    // load-bearing — stated explicitly so the ordering does not silently
    // depend on which container this happens to be built from.
    ranked.sort_by(|a, b| {
        b.1.milliseconds
            .total_cmp(&a.1.milliseconds)
            .then_with(|| a.0.cmp(&b.0))
    });
    let rows = ranked
        .iter()
        .take(MAX_SWIFT_TIMINGS)
        .map(|((file, line, _), total)| {
            let repeats = if total.occurrences > 1 {
                format!(" x{}", total.occurrences)
            } else {
                String::new()
            };
            format!(
                "\n{:.0}ms{repeats} {file}:{line}{}",
                total.milliseconds,
                total
                    .symbol
                    .map(|symbol| format!(" - {symbol}"))
                    .unwrap_or_default()
            )
        })
        .collect::<String>();
    format!(
        "\nSWIFT TIMINGS\nSlow to compile: {} location(s){}\n",
        ranked.len(),
        rows
    )
}

/// Renders one ownership record. Shared by both formats so the two cannot
/// drift into disagreeing about what a blame record contains.
fn ownership_line(owner: &GitOwnership) -> String {
    format!(
        "{} <{}> ({}) - {}:{} - authored {} - committed {} - {}",
        owner.author,
        owner.author_email.as_deref().unwrap_or("unknown"),
        owner.commit,
        owner.file,
        owner
            .line
            .map(|line| line.to_string())
            .unwrap_or_else(|| "?".to_owned()),
        owner.authored_at.as_deref().unwrap_or("unknown"),
        owner.committed_at.as_deref().unwrap_or("unknown"),
        owner.subject,
    )
}

/// A capped block of ownership records, or `""` when there are none.
fn ownership_block(heading: &str, owners: &[GitOwnership], prefix: &str) -> String {
    if owners.is_empty() {
        return String::new();
    }
    let rows = owners
        .iter()
        .take(MAX_OWNERSHIP)
        .map(|owner| format!("{prefix}{}\n", ownership_line(owner)))
        .collect::<String>();
    format!("\n{heading}\n{rows}")
}

fn git_terminal(git: &GitCorrelation) -> String {
    format!(
        "\nGIT CORRELATION\nLikely PR related: {}\nConfidence: {}%\nEvidence: {}\n",
        git.likely_related.as_str().to_uppercase(),
        git.confidence,
        git.evidence.join("; ")
    ) + &ownership_block(
        "FAILING TEST LOCATION",
        &git.failure_ownership,
        "Test location author: ",
    ) + &ownership_block(
        "RELEVANT IMPLEMENTATION CHANGE",
        &git.implementation_ownership,
        "Last changed by: ",
    ) + &ownership_block(
        "DIAGNOSTIC OWNERSHIP",
        &git.diagnostic_ownership,
        "Diagnostic owner: ",
    )
}

fn intelligence_terminal(intelligence: &Intelligence) -> String {
    // An empty `Intelligence` has nothing to report; printing the heading with
    // `0/0` under it reads as a finding rather than an absence.
    if intelligence.impacts.is_empty()
        && intelligence.bottlenecks.is_empty()
        && intelligence.chains.is_empty()
    {
        return String::new();
    }
    let mut text = String::from("\nINTELLIGENCE\n");
    // `owning_target` is `Option`: `None` means no target claims the file, so
    // it is filtered out rather than compared against a sentinel string.
    let mapped: Vec<_> = intelligence
        .impacts
        .iter()
        .filter_map(|impact| {
            impact
                .owning_target
                .as_deref()
                .map(|target| (impact, target))
        })
        .collect();
    text.push_str(&format!(
        "Changed files mapped to targets: {}/{}\n",
        mapped.len(),
        intelligence.impacts.len()
    ));
    for (impact, target) in mapped.iter().take(MAX_IMPACTS) {
        text.push_str(&format!(
            "  {} -> {} (blocks {} downstream)\n",
            impact.changed_file,
            target,
            impact.downstream_targets.len()
        ));
    }
    if !intelligence.bottlenecks.is_empty() {
        text.push_str("Build bottlenecks (duration x blocked targets):\n");
        for bottleneck in intelligence.bottlenecks.iter().take(MAX_BOTTLENECKS) {
            text.push_str(&format!(
                "  {}: {:.1}s, blocks {} targets (score {:.0})\n",
                bottleneck.target,
                bottleneck.seconds,
                bottleneck.blocked_downstream,
                bottleneck.score
            ));
        }
    }
    for chain in intelligence.chains.iter().take(MAX_CHAINS) {
        text.push_str(&format!(
            "Evidence for {} ({}% confidence):\n",
            chain.subject, chain.confidence
        ));
        for link in &chain.links {
            let sign = if link.weight >= 0 { "+" } else { "" };
            text.push_str(&format!("  [{sign}{}] {}\n", link.weight, link.description));
        }
    }
    text
}

/// Metrics section, shared shape across both formats.
///
/// `warnings` and `truncations` are both surfaced: a non-empty `warnings`
/// means the decode may be wrong, and a truncation means a headline count is
/// smaller than what the build actually did. Markdown previously showed
/// neither, which is the format most likely to be pasted into a PR.
fn metrics_terminal(metrics: &BuildMetrics) -> String {
    let truncated = if metrics.truncations.is_empty() {
        String::new()
    } else {
        format!("\nTruncated lists: {}", metrics.truncations.join("; "))
    };
    format!(
        "\n\nMETRICS\nSource: {}\nMeasured phases: {}\nMeasured targets: {}\nMeasured files: {}{}\nWarnings: {}{}\n",
        metrics.source_kind.as_str(),
        metrics.phases.len(),
        metrics.targets.len(),
        metrics.files.len(),
        architecture_note(metrics),
        metrics.warnings.len(),
        truncated,
    )
}

pub fn terminal(analysis: &BuildAnalysis) -> String {
    let build_timing = if analysis.timings.phases.is_empty() {
        "\n\nBUILD TIMING\nXcode build timing summary: not found\nUse -showBuildTimingSummary when running xcodebuild to collect phase and target timings.\n".to_owned()
    } else {
        let targets = analysis
            .timings
            .slowest_targets
            .iter()
            .take(MAX_SLOWEST_TARGETS)
            .map(|timing| format!("\n{}: {:.3}s", timing.target, timing.seconds))
            .collect::<String>();
        format!(
            "\n\nBUILD TIMING\nPhases: {}\nSlowest targets:{}\n",
            analysis.timings.phases.len(),
            targets
        )
    };
    let metrics = analysis
        .metrics
        .as_ref()
        .map(metrics_terminal)
        .unwrap_or_default();
    let swift_timings = analysis
        .metrics
        .as_ref()
        .map(swift_timings_terminal)
        .unwrap_or_default();
    // Test counts appear once, under TESTS. The old FAILURES block repeated
    // `tests.failed` and `tests.crashed` under different names ("assertion
    // failures", "test process crashes"), so three problems read as six.
    format!(
        "BuildLens\n\nBUILD\nStatus: {}\nScheme: {}\nTargets in build graph: {}\n\nFAILURES\nRoot cause clusters: {}\n\nWARNINGS\nRaw occurrences: {}\nUnique issues: {}\n\nTESTS\nTotal: {}\nFailed: {}\nCrashed: {}\nTest operation: {}\n\nSWIFT 6\nUnique blockers: {}\n",
        analysis.status.as_str(),
        analysis.build.scheme.as_deref().unwrap_or("unknown"),
        analysis
            .graph
            .declared_count
            .unwrap_or(analysis.graph.targets.len()),
        analysis.failure_clusters.len(),
        analysis.diagnostics.raw_warnings,
        analysis.diagnostics.unique_warnings,
        analysis.tests.total,
        analysis.tests.failed,
        analysis.tests.crashed,
        analysis
            .timings
            .test_operation_seconds
            .map(|seconds| format!("{seconds:.3}s"))
            .unwrap_or_else(|| "unavailable".to_owned()),
        analysis.diagnostics.swift6.unique_blockers
    ) + &build_timing
        + &metrics
        + &swift_timings
        + &analysis.git.as_ref().map(git_terminal).unwrap_or_default()
        + &analysis
            .intelligence
            .as_ref()
            .map(intelligence_terminal)
            .unwrap_or_default()
}

fn metrics_markdown(metrics: &BuildMetrics) -> String {
    let swift = if metrics.swift_timings.is_empty() {
        String::new()
    } else {
        format!(
            "- **Slow Swift locations:** {}\n",
            metrics.swift_timings.len()
        )
    };
    // A non-empty `warnings` means the decode may be wrong or incomplete, and
    // markdown is the format that ends up in a pull request. Listing them is
    // the point of the section.
    let warnings = if metrics.warnings.is_empty() {
        String::new()
    } else {
        format!(
            "- **Warnings ({}):** {}\n",
            metrics.warnings.len(),
            metrics.warnings.join("; ")
        )
    };
    let truncations = if metrics.truncations.is_empty() {
        String::new()
    } else {
        format!("- **Truncated:** {}\n", metrics.truncations.join("; "))
    };
    format!(
        "\n### Metrics\n\n- **Source:** {}\n- **Phases:** {}\n- **Targets:** {}\n- **Files:** {}\n{swift}{warnings}{truncations}",
        metrics.source_kind.as_str(),
        metrics.phases.len(),
        metrics.targets.len(),
        metrics.files.len(),
    )
}

/// A capped markdown bullet list of ownership records, or `""` when empty —
/// so a heading is never emitted with nothing beneath it.
fn ownership_markdown(heading: &str, owners: &[GitOwnership], label: &str) -> String {
    if owners.is_empty() {
        return String::new();
    }
    let rows = owners
        .iter()
        .take(MAX_OWNERSHIP)
        .map(|owner| format!("\n- **{label}:** {}", ownership_line(owner)))
        .collect::<String>();
    format!("\n### {heading}\n{rows}\n")
}

fn git_markdown(git: &GitCorrelation) -> String {
    format!(
        "\n### Git correlation\n\n- **Likely PR related:** {}\n- **Confidence:** {}%\n- **Evidence:** {}\n",
        git.likely_related.as_str(),
        git.confidence,
        git.evidence.join("; ")
    ) + &ownership_markdown(
        "Failure ownership",
        &git.failure_ownership,
        "Test location author",
    ) + &ownership_markdown(
        "Implementation ownership",
        &git.implementation_ownership,
        "Last relevant implementation change",
    ) + &ownership_markdown(
        "Diagnostic ownership",
        &git.diagnostic_ownership,
        "Diagnostic owner",
    )
}

fn intelligence_markdown(intelligence: &Intelligence) -> String {
    if intelligence.impacts.is_empty()
        && intelligence.bottlenecks.is_empty()
        && intelligence.chains.is_empty()
    {
        return String::new();
    }
    let mut text = String::from("\n### Intelligence\n");
    for impact in intelligence.impacts.iter().take(MAX_IMPACTS) {
        let Some(target) = impact.owning_target.as_deref() else {
            continue;
        };
        text.push_str(&format!(
            "\n- **Impact:** `{}` -> **{}** (blocks {} downstream)",
            impact.changed_file,
            target,
            impact.downstream_targets.len()
        ));
    }
    for bottleneck in intelligence.bottlenecks.iter().take(MAX_BOTTLENECKS) {
        text.push_str(&format!(
            "\n- **Bottleneck:** **{}** {:.1}s, blocks {} targets",
            bottleneck.target, bottleneck.seconds, bottleneck.blocked_downstream
        ));
    }
    for chain in intelligence.chains.iter().take(MAX_CHAINS) {
        text.push_str(&format!(
            "\n- **Evidence ({}% confidence):** {}",
            chain.confidence, chain.summary
        ));
        for link in &chain.links {
            text.push_str(&format!("\n  - {}", link.description));
        }
    }
    text.push('\n');
    text
}

pub fn markdown(analysis: &BuildAnalysis) -> String {
    format!(
        "## BuildLens\n\n- **Status:** {}\n- **Scheme:** {}\n- **Targets:** {}\n- **Failures:** {}\n- **Crashes:** {}\n- **Unique warnings:** {}\n",
        analysis.status.as_str(),
        analysis.build.scheme.as_deref().unwrap_or("unknown"),
        analysis
            .graph
            .declared_count
            .unwrap_or(analysis.graph.targets.len()),
        analysis.tests.failed,
        analysis.tests.crashed,
        analysis.diagnostics.unique_warnings
    ) + &analysis
        .metrics
        .as_ref()
        .map(metrics_markdown)
        .unwrap_or_default()
        + &analysis.git.as_ref().map(git_markdown).unwrap_or_default()
        + &analysis
            .intelligence
            .as_ref()
            .map(intelligence_markdown)
            .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use buildlens_core::{
        AnalysisStatus, Bottleneck, EvidenceChain, EvidenceLink, FileMetric, GitOwnership,
        LikelyRelated, MatchKind, MetricsSourceKind, SwiftTimingKind, SwiftTimingMetric,
        TargetGraphSummary, TargetImpact, TestSummary, TimingSummary,
    };

    /// A default analysis carrying just these metrics.
    fn with_metrics(metrics: BuildMetrics) -> BuildAnalysis {
        BuildAnalysis {
            metrics: Some(metrics),
            ..Default::default()
        }
    }

    fn metrics() -> BuildMetrics {
        BuildMetrics::empty(MetricsSourceKind::Xcactivitylog, vec![])
    }

    fn owner(file: &str) -> GitOwnership {
        GitOwnership {
            file: file.to_owned(),
            line: Some(12),
            author: "Ada".to_owned(),
            author_email: Some("ada@example.com".to_owned()),
            authored_at: Some("2026-01-01".to_owned()),
            committed_at: Some("2026-01-02".to_owned()),
            commit: "abc1234".to_owned(),
            subject: "Fix the thing".to_owned(),
        }
    }

    fn correlation() -> GitCorrelation {
        GitCorrelation {
            base: "main".to_owned(),
            head: "topic".to_owned(),
            changed_files: vec![],
            likely_related: LikelyRelated::Yes,
            confidence: 80,
            evidence: vec!["changed file matches failure".to_owned()],
            failure_ownership: vec![],
            implementation_ownership: vec![],
            diagnostic_ownership: vec![],
        }
    }

    fn impact(file: &str, target: Option<&str>) -> TargetImpact {
        TargetImpact {
            changed_file: file.to_owned(),
            owning_target: target.map(str::to_owned),
            downstream_targets: vec!["Downstream".to_owned()],
            match_kind: MatchKind::Exact,
        }
    }

    fn timing(file: &str, ms: f64, symbol: Option<&str>) -> SwiftTimingMetric {
        SwiftTimingMetric {
            kind: SwiftTimingKind::FunctionBody,
            file: file.to_owned(),
            line: 10,
            column: 5,
            symbol: symbol.map(str::to_owned),
            milliseconds: ms,
            target: None,
        }
    }

    // --- json ---

    /// JSON has no NaN or infinity, so `serde_json` writes `null` rather than
    /// failing. That is the honest encoding — a duration that is not a number
    /// is absent — but it means a bad `f64` is silently absorbed rather than
    /// reported, so the fact is pinned here.
    #[test]
    fn a_non_finite_duration_serializes_as_null() {
        let analysis = BuildAnalysis {
            timings: TimingSummary {
                test_operation_seconds: Some(f64::NAN),
                ..Default::default()
            },
            ..Default::default()
        };
        let text = json(&analysis).expect("serde_json encodes NaN as null");
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(parsed["timings"]["test_operation_seconds"].is_null());
    }

    /// `"{}"` is a valid, *clean* build report. Falling back to it on failure
    /// would record "nothing found" for a run that never serialized, so the
    /// error is returned to the caller instead.
    #[test]
    fn json_returns_the_error_rather_than_an_empty_report() {
        // A map with non-string keys is the reachable failure shape: JSON
        // objects can only be keyed by strings.
        let mut unserializable = std::collections::BTreeMap::new();
        unserializable.insert(vec![1u8, 2], "value");
        let error = serde_json::to_string(&unserializable).unwrap_err();
        assert!(error.to_string().contains("key must be a string"));
        // The signature is what enforces this: there is no `unwrap_or_else`
        // fallback left for a caller to mistake for a clean result.
        let _: Result<String, serde_json::Error> = json(&BuildAnalysis::default());
    }

    #[test]
    fn json_round_trips_a_normal_analysis() {
        let text = json(&BuildAnalysis::default()).expect("plain analysis serializes");
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["status"], "passed");
    }

    /// The rendered status must match the JSON spelling of the same run;
    /// `{:?}` would print `Passed` beside a JSON `"passed"`.
    #[test]
    fn status_renders_with_the_same_spelling_as_json() {
        let analysis = BuildAnalysis {
            status: AnalysisStatus::Failed,
            ..Default::default()
        };
        assert!(terminal(&analysis).contains("Status: failed"));
        assert!(markdown(&analysis).contains("**Status:** failed"));
    }

    #[test]
    fn metrics_source_renders_with_the_serde_spelling() {
        let analysis = with_metrics(metrics());
        assert!(terminal(&analysis).contains("Source: xcactivitylog"));
        assert!(markdown(&analysis).contains("**Source:** xcactivitylog"));
        assert!(
            !terminal(&analysis).contains("Xcactivitylog"),
            "must not leak the Debug spelling"
        );
    }

    // --- failure counts ---

    /// `tests.failed` was printed twice, once as "Assertion failures" and
    /// again under TESTS, so three problems read as six.
    #[test]
    fn test_failures_are_reported_once() {
        let analysis = BuildAnalysis {
            tests: TestSummary {
                failed: 3,
                crashed: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let text = terminal(&analysis);
        assert_eq!(text.matches("Failed: 3").count(), 1, "{text}");
        assert!(!text.contains("Assertion failures"), "{text}");
        assert!(!text.contains("Test process crashes"), "{text}");
    }

    // --- intelligence: Option<String> owning_target ---

    /// `owning_target` is `Option`; `None` means no target claims the file.
    /// The old code compared against a `"unknown"` sentinel, which a target
    /// legitimately named `unknown` would have tripped.
    #[test]
    fn unmapped_files_are_excluded_from_the_mapped_count() {
        let intelligence = Intelligence {
            impacts: vec![
                impact("A.swift", Some("App")),
                impact("B.swift", None),
                impact("C.swift", None),
            ],
            ..Default::default()
        };
        assert!(intelligence_terminal(&intelligence).contains("mapped to targets: 1/3"));
    }

    /// A target really named `unknown` is a real mapping and must be reported.
    #[test]
    fn a_target_named_unknown_is_still_a_mapping() {
        let intelligence = Intelligence {
            impacts: vec![impact("A.swift", Some("unknown"))],
            ..Default::default()
        };
        let text = intelligence_terminal(&intelligence);
        assert!(text.contains("mapped to targets: 1/1"), "{text}");
        assert!(text.contains("A.swift -> unknown"), "{text}");
    }

    #[test]
    fn markdown_skips_unmapped_impacts() {
        let intelligence = Intelligence {
            impacts: vec![impact("A.swift", None), impact("B.swift", Some("App"))],
            ..Default::default()
        };
        let text = intelligence_markdown(&intelligence);
        assert!(!text.contains("A.swift"), "{text}");
        assert!(text.contains("B.swift"), "{text}");
    }

    // --- empty sections ---

    /// An empty `Intelligence` printed a heading with `0/0` beneath it, which
    /// reads as a finding rather than an absence.
    #[test]
    fn empty_intelligence_renders_nothing() {
        assert_eq!(intelligence_terminal(&Intelligence::default()), "");
        assert_eq!(intelligence_markdown(&Intelligence::default()), "");
    }

    #[test]
    fn empty_ownership_lists_render_no_heading() {
        let text = git_markdown(&correlation());
        assert!(!text.contains("Failure ownership"), "{text}");
        assert!(!text.contains("Diagnostic ownership"), "{text}");
        assert!(text.contains("Git correlation"), "{text}");
    }

    #[test]
    fn absent_metrics_render_no_metrics_section() {
        let analysis = BuildAnalysis::default();
        assert!(!markdown(&analysis).contains("### Metrics"));
        assert!(!terminal(&analysis).contains("METRICS"));
    }

    #[test]
    fn metrics_without_swift_timings_render_no_timings_section() {
        assert_eq!(swift_timings_terminal(&metrics()), "");
    }

    // --- caps ---

    #[test]
    fn ownership_lists_are_capped() {
        let mut git = correlation();
        git.diagnostic_ownership = (0..20).map(|i| owner(&format!("F{i}.swift"))).collect();
        // Counts the rendered records, not the heading — "DIAGNOSTIC
        // OWNERSHIP" contains the label as a substring.
        assert_eq!(
            git_markdown(&git).matches("**Diagnostic owner:**").count(),
            MAX_OWNERSHIP
        );
        assert_eq!(
            git_terminal(&git).matches("Diagnostic owner: ").count(),
            MAX_OWNERSHIP
        );
    }

    #[test]
    fn bottlenecks_are_capped() {
        let intelligence = Intelligence {
            bottlenecks: (0..20)
                .map(|i| Bottleneck {
                    target: format!("T{i}"),
                    seconds: 1.0,
                    fan_in: 0,
                    blocked_downstream: 0,
                    score: 1.0,
                })
                .collect(),
            ..Default::default()
        };
        assert_eq!(
            intelligence_terminal(&intelligence)
                .matches("blocks 0 targets")
                .count(),
            MAX_BOTTLENECKS
        );
        assert_eq!(
            intelligence_markdown(&intelligence)
                .matches("**Bottleneck:**")
                .count(),
            MAX_BOTTLENECKS
        );
    }

    #[test]
    fn swift_timings_are_capped_but_the_total_count_is_honest() {
        let mut m = metrics();
        m.swift_timings = (0..30)
            .map(|i| timing(&format!("F{i}.swift"), f64::from(i), None))
            .collect();
        let text = swift_timings_terminal(&m);
        assert!(text.contains("Slow to compile: 30 location(s)"), "{text}");
        assert_eq!(text.matches("ms ").count(), MAX_SWIFT_TIMINGS, "{text}");
    }

    // --- metrics warnings and truncations ---

    /// A non-empty `warnings` means the decode may be wrong. Markdown showed
    /// none of them, and markdown is what gets pasted into a pull request.
    #[test]
    fn markdown_surfaces_metrics_warnings() {
        let analysis = with_metrics(BuildMetrics::empty(
            MetricsSourceKind::Xcactivitylog,
            vec!["truncated section".to_owned()],
        ));
        let text = markdown(&analysis);
        assert!(text.contains("Warnings (1)"), "{text}");
        assert!(text.contains("truncated section"), "{text}");
    }

    /// "Files: 50" must not hide that the build compiled nine thousand.
    #[test]
    fn truncations_are_surfaced_in_both_formats() {
        let mut m = metrics();
        m.truncations = vec!["files: kept 50 of 9000".to_owned()];
        let analysis = with_metrics(m);
        assert!(terminal(&analysis).contains("kept 50 of 9000"));
        assert!(markdown(&analysis).contains("kept 50 of 9000"));
    }

    #[test]
    fn clean_metrics_render_no_warning_or_truncation_lines() {
        let analysis = with_metrics(metrics());
        let text = markdown(&analysis);
        assert!(!text.contains("**Warnings"), "{text}");
        assert!(!text.contains("**Truncated"), "{text}");
    }

    // --- swift timing aggregation ---

    /// One function is reported once per compilation, so a file built for two
    /// architectures reports twice; the costs are summed, not listed apart.
    #[test]
    fn repeated_locations_are_summed_and_counted() {
        let mut m = metrics();
        m.swift_timings = vec![
            timing("A.swift", 100.0, Some("slowFunc")),
            timing("A.swift", 150.0, Some("slowFunc")),
        ];
        let text = swift_timings_terminal(&m);
        assert!(text.contains("Slow to compile: 1 location(s)"), "{text}");
        assert!(text.contains("250ms x2"), "{text}");
        assert!(text.contains("slowFunc"), "{text}");
    }

    /// The symbol must survive even when the first record for a location
    /// lacked one — `or_insert` previously locked in whichever came first.
    #[test]
    fn a_symbol_is_kept_even_when_the_first_record_lacks_one() {
        let mut m = metrics();
        m.swift_timings = vec![
            timing("A.swift", 10.0, None),
            timing("A.swift", 10.0, Some("namedFunc")),
        ];
        assert!(swift_timings_terminal(&m).contains("namedFunc"));
    }

    /// Equal durations must fall back to location order, not input order, so
    /// two runs over the same build render an identical report.
    #[test]
    fn equal_durations_render_in_a_stable_order() {
        let mut m = metrics();
        m.swift_timings = vec![
            timing("B.swift", 50.0, None),
            timing("A.swift", 50.0, None),
            timing("C.swift", 50.0, None),
        ];
        let first = swift_timings_terminal(&m);
        m.swift_timings.reverse();
        assert_eq!(first, swift_timings_terminal(&m));
        assert!(
            first.find("A.swift").unwrap() < first.find("B.swift").unwrap(),
            "{first}"
        );
    }

    #[test]
    fn slower_locations_rank_first() {
        let mut m = metrics();
        m.swift_timings = vec![
            timing("Fast.swift", 10.0, None),
            timing("Slow.swift", 900.0, None),
        ];
        let text = swift_timings_terminal(&m);
        assert!(
            text.find("Slow.swift").unwrap() < text.find("Fast.swift").unwrap(),
            "{text}"
        );
    }

    // --- architecture note ---

    #[test]
    fn a_single_architecture_adds_no_note() {
        let mut m = metrics();
        m.files = vec![FileMetric {
            file: "A.swift".to_owned(),
            seconds: 1.0,
            target: None,
            step_type: "swift".to_owned(),
            architecture: Some("arm64".to_owned()),
            occurrences: 1,
        }];
        assert_eq!(architecture_note(&m), "");
    }

    /// Without the note, 400 files becoming 800 reads as a bigger build when
    /// it only means a second architecture was added.
    #[test]
    fn multiple_architectures_are_noted() {
        let mut m = metrics();
        m.files = ["arm64", "x86_64"]
            .iter()
            .map(|arch| FileMetric {
                file: "A.swift".to_owned(),
                seconds: 1.0,
                target: None,
                step_type: "swift".to_owned(),
                architecture: Some((*arch).to_owned()),
                occurrences: 1,
            })
            .collect();
        let note = architecture_note(&m);
        assert!(note.contains("2 architectures"), "{note}");
        assert!(note.contains("arm64, x86_64"), "{note}");
    }

    // --- git ---

    #[test]
    fn likely_related_renders_with_the_serde_spelling() {
        let mut git = correlation();
        git.likely_related = LikelyRelated::Uncertain;
        assert!(git_terminal(&git).contains("Likely PR related: UNCERTAIN"));
        assert!(git_markdown(&git).contains("**Likely PR related:** uncertain"));
    }

    #[test]
    fn a_missing_blame_line_renders_as_a_question_mark() {
        let mut owner = owner("A.swift");
        owner.line = None;
        assert!(ownership_line(&owner).contains("A.swift:?"));
    }

    #[test]
    fn missing_ownership_fields_render_as_unknown() {
        let mut owner = owner("A.swift");
        owner.author_email = None;
        owner.authored_at = None;
        owner.committed_at = None;
        let line = ownership_line(&owner);
        assert!(line.contains("<unknown>"), "{line}");
        assert!(line.contains("authored unknown"), "{line}");
        assert!(line.contains("committed unknown"), "{line}");
    }

    /// Both formats read the same record, so they must not drift into
    /// disagreeing about what a blame entry contains.
    #[test]
    fn both_formats_render_the_same_ownership_facts() {
        let mut git = correlation();
        git.failure_ownership = vec![owner("Tests/AppTests.swift")];
        for text in [git_terminal(&git), git_markdown(&git)] {
            assert!(text.contains("Ada"), "{text}");
            assert!(text.contains("abc1234"), "{text}");
            assert!(text.contains("Tests/AppTests.swift:12"), "{text}");
            assert!(text.contains("Fix the thing"), "{text}");
        }
    }

    // --- headline fields ---

    #[test]
    fn a_missing_scheme_renders_as_unknown() {
        let analysis = BuildAnalysis::default();
        assert!(terminal(&analysis).contains("Scheme: unknown"));
        assert!(markdown(&analysis).contains("**Scheme:** unknown"));
    }

    /// `declared_count` is what the project declares; the parsed `targets`
    /// list is only what the log happened to mention.
    #[test]
    fn declared_target_count_wins_over_the_observed_list() {
        let analysis = BuildAnalysis {
            graph: TargetGraphSummary {
                declared_count: Some(42),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(terminal(&analysis).contains("Targets in build graph: 42"));
    }

    #[test]
    fn absent_build_timing_says_how_to_collect_it() {
        let text = terminal(&BuildAnalysis::default());
        assert!(text.contains("not found"), "{text}");
        assert!(text.contains("-showBuildTimingSummary"), "{text}");
    }

    #[test]
    fn an_unavailable_test_operation_time_is_labelled() {
        assert!(terminal(&BuildAnalysis::default()).contains("Test operation: unavailable"));
    }

    // --- evidence chains ---

    #[test]
    fn negative_evidence_weights_keep_their_sign() {
        let intelligence = Intelligence {
            chains: vec![EvidenceChain {
                subject: "AppTests".to_owned(),
                links: vec![
                    EvidenceLink {
                        kind: "overlap".to_owned(),
                        weight: 3,
                        description: "touched the failing file".to_owned(),
                    },
                    EvidenceLink {
                        kind: "age".to_owned(),
                        weight: -2,
                        description: "failure predates the change".to_owned(),
                    },
                ],
                confidence: 60,
                summary: "partial overlap".to_owned(),
            }],
            ..Default::default()
        };
        let text = intelligence_terminal(&intelligence);
        assert!(text.contains("[+3]"), "{text}");
        assert!(text.contains("[-2]"), "{text}");
    }

    /// A report is rendered, not queried — every heading must be reachable in
    /// one pass over a fully-populated analysis.
    #[test]
    fn a_fully_populated_analysis_renders_every_section() {
        let mut m = metrics();
        m.swift_timings = vec![timing("A.swift", 500.0, Some("f"))];
        let mut timings = TimingSummary::default();
        timings.phases.insert("Compile".to_owned(), 1.0);
        let analysis = BuildAnalysis {
            metrics: Some(m),
            timings,
            git: Some(correlation()),
            intelligence: Some(Intelligence {
                impacts: vec![impact("A.swift", Some("App"))],
                ..Default::default()
            }),
            ..Default::default()
        };
        let text = terminal(&analysis);
        for heading in [
            "BUILD",
            "FAILURES",
            "WARNINGS",
            "TESTS",
            "SWIFT 6",
            "BUILD TIMING",
            "METRICS",
            "SWIFT TIMINGS",
            "GIT CORRELATION",
            "INTELLIGENCE",
        ] {
            assert!(text.contains(heading), "missing {heading} in:\n{text}");
        }
        assert!(json(&analysis).is_ok());
    }
}
