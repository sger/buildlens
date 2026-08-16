//! Evidence chains: per-target links tying changes, regressions, diagnostics
//! and failures together, scored into a confidence.

use crate::language;
use buildlens_core::{
    BuildAnalysis, EvidenceChain, EvidenceLink, MatchKind, MetricKind, MetricRegression,
    RegressionCaveat, RegressionConfidence, TargetImpact, TestStatus, sourcepath,
};
use std::collections::BTreeMap;

/// Chains below this confidence, or with fewer than two positive links, are
/// dropped: a single weak signal is a coincidence, not a finding.
const MIN_CONFIDENCE: i32 = 25;
const MIN_POSITIVE_LINKS: usize = 2;

const WEIGHT_IMPACT_EXACT: i32 = 30;
const WEIGHT_IMPACT_SUFFIX: i32 = 25;
const WEIGHT_DIAGNOSTIC_IN_CHANGE: i32 = 20;
const WEIGHT_FAILING_SUITE: i32 = 15;
const WEIGHT_ENVIRONMENT_SHIFT: i32 = -20;
const WEIGHT_SUSPENDED: i32 = -25;

fn regression_weight(confidence: RegressionConfidence) -> i32 {
    match confidence {
        RegressionConfidence::High => 25,
        RegressionConfidence::Medium => 15,
        RegressionConfidence::Low => 5,
    }
}

pub fn chains(
    analysis: &BuildAnalysis,
    impacts: &[TargetImpact],
    regressions: &[MetricRegression],
) -> Vec<EvidenceChain> {
    let mut per_target: BTreeMap<String, Vec<EvidenceLink>> = BTreeMap::new();
    let mut push = |target: &str, kind: &str, weight: i32, description: String| {
        per_target
            .entry(target.to_owned())
            .or_default()
            .push(EvidenceLink {
                kind: kind.into(),
                weight,
                description,
            });
    };

    for impact in impacts {
        let Some(owning_target) = impact.owning_target.as_deref() else {
            continue;
        };
        let (weight, phrase) = match impact.match_kind {
            MatchKind::Exact => (WEIGHT_IMPACT_EXACT, language::CHANGED_FILE_BUILDS_INTO),
            MatchKind::Suffix => (WEIGHT_IMPACT_SUFFIX, language::CHANGED_FILE_NEAR),
            MatchKind::None => continue,
        };
        push(
            owning_target,
            "changed_file",
            weight,
            format!("{phrase}: {}", impact.changed_file),
        );
    }

    for regression in regressions {
        if regression.metric_kind != MetricKind::Target {
            continue;
        }
        push(
            &regression.name.clone(),
            "regression",
            regression_weight(regression.confidence),
            format!(
                "{}: {:.1}s -> {:.1}s (+{:.1}%)",
                language::TARGET_REGRESSED,
                regression.previous_seconds,
                regression.current_seconds,
                regression.delta_percent
            ),
        );
    }

    if let Some(git) = &analysis.git {
        for diagnostic in &analysis.diagnostics.diagnostics {
            let Some(file) = diagnostic.example.file.as_deref() else {
                continue;
            };
            if !sourcepath::matches_any(file, &git.changed_files) {
                continue;
            }
            let target = diagnostic.example.target.clone().or_else(|| {
                impacts
                    .iter()
                    .find(|impact| sourcepath::same_file(file, &impact.changed_file))
                    .and_then(|impact| impact.owning_target.clone())
            });
            if let Some(target) = target {
                push(
                    &target,
                    "diagnostic",
                    WEIGHT_DIAGNOSTIC_IN_CHANGE,
                    format!(
                        "{}: {}",
                        language::DIAGNOSTIC_IN_CHANGE,
                        sourcepath::normalize_separators(file)
                    ),
                );
            }
        }
    }

    let impacted: Vec<&str> = impacts
        .iter()
        .filter_map(|impact| impact.owning_target.as_deref())
        .chain(
            impacts
                .iter()
                .flat_map(|impact| impact.downstream_targets.iter().map(String::as_str)),
        )
        .collect();
    for test in &analysis.tests.tests {
        if test.status != TestStatus::Failed {
            continue;
        }
        for target in &impacted {
            if suite_matches_target(&test.suite, target) {
                push(
                    target,
                    "failing_suite",
                    WEIGHT_FAILING_SUITE,
                    format!("{}: {}", language::FAILING_SUITE_MATCHES, test.suite),
                );
                break;
            }
        }
    }

    // Counter-evidence applies to every chain.
    let suspended = analysis
        .metadata
        .entries
        .get("build.was_suspended")
        .is_some_and(|value| value == "true");
    let environment_shift = regressions.iter().any(|regression| {
        regression
            .caveats
            .contains(&RegressionCaveat::EnvironmentShifted)
    });

    let mut result: Vec<EvidenceChain> = per_target
        .into_iter()
        .filter_map(|(subject, mut links)| {
            if suspended {
                links.push(EvidenceLink {
                    kind: "counter_suspended".into(),
                    weight: WEIGHT_SUSPENDED,
                    description: language::BUILD_SUSPENDED.into(),
                });
            }
            if environment_shift {
                links.push(EvidenceLink {
                    kind: "counter_environment".into(),
                    weight: WEIGHT_ENVIRONMENT_SHIFT,
                    description: language::ENVIRONMENT_SHIFTED.into(),
                });
            }
            let positive = links.iter().filter(|link| link.weight > 0).count();
            // Sum first, then clamp. Testing the floor against the clamped
            // value made "strong evidence, strongly contradicted" identical to
            // "no evidence at all" — counter-evidence exists to be visible.
            let raw = links.iter().map(|link| link.weight).sum::<i32>();
            if raw < MIN_CONFIDENCE || positive < MIN_POSITIVE_LINKS {
                return None;
            }
            let confidence = raw.clamp(0, 100) as u8;
            Some(EvidenceChain {
                subject: subject.clone(),
                links,
                confidence,
                summary: language::chain_summary(&subject, confidence),
            })
        })
        .collect();
    // Confidence descending, then subject, so equal-confidence chains keep a
    // stable order rather than depending on map iteration.
    result.sort_by(|a, b| {
        b.confidence
            .cmp(&a.confidence)
            .then_with(|| a.subject.cmp(&b.subject))
    });
    result
}

/// True when a failing test suite plausibly belongs to a target.
///
/// Compares the suite's stem — its name with a trailing `Tests` removed —
/// against the target name, and requires both to be non-empty. An earlier
/// version used `target.contains(stem)`, so a suite named exactly `Tests`
/// produced an empty stem and matched *every* target, attaching a failing-suite
/// link to all of them.
fn suite_matches_target(suite: &str, target: &str) -> bool {
    let suite = suite.trim().to_lowercase();
    let target = target.trim().to_lowercase();
    if suite.is_empty() || target.is_empty() {
        return false;
    }
    let stem = suite.strip_suffix("tests").unwrap_or(&suite);
    if stem.is_empty() {
        // A suite named just "Tests" says nothing about which target it covers.
        return false;
    }
    stem == target
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_suite_matches_the_target_it_is_named_after() {
        assert!(suite_matches_target("CoreTests", "Core"));
        assert!(suite_matches_target("coretests", "CORE"));
        assert!(suite_matches_target(" CoreTests ", "Core"));
        // A suite need not carry the suffix at all.
        assert!(suite_matches_target("Core", "Core"));
    }

    /// The headline bug: an empty stem matched every target.
    #[test]
    fn a_suite_named_only_tests_matches_nothing() {
        assert!(!suite_matches_target("Tests", "App"));
        assert!(!suite_matches_target("Tests", "Core"));
        assert!(!suite_matches_target("tests", "Anything"));
    }

    #[test]
    fn an_empty_suite_or_target_matches_nothing() {
        assert!(!suite_matches_target("", "App"));
        assert!(!suite_matches_target("FooTests", ""));
        assert!(!suite_matches_target("", ""));
    }

    /// Substring matching attached failures to targets that merely shared a
    /// prefix.
    #[test]
    fn a_partial_name_does_not_match() {
        assert!(!suite_matches_target("CoreTests", "CoreExtended"));
        assert!(!suite_matches_target("NetworkTests", "Net"));
        assert!(!suite_matches_target("CoreTests", "App"));
    }
}
