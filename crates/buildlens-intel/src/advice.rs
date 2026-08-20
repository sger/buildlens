//! Which Swift type-checking costs are worth acting on.
//!
//! Ranking a list of slow expressions is a measurement; this turns it into a
//! short list of places with a stated reason for each. Everything here is
//! derived from `BuildMetrics` alone — no source is read, which is what keeps
//! this in `buildlens-intel` rather than behind an opt-in. Advice that needs
//! to see the expression text is a separate, local-only concern.

use crate::language;
use buildlens_core::{
    Advice, AdviceKind, BuildAnalysis, BuildCategory, SwiftTimingKind, SwiftTimingMetric,
};
use std::collections::BTreeMap;

/// A single site below this is not worth a line of output.
///
/// Calibrated against a real SwiftUI app rather than a synthetic one: across
/// 91 measured sites the median was 17ms, p90 38ms and the slowest 93ms. The
/// first version of this used 250ms, taken from a package written to be
/// pathologically slow, and so reported nothing on healthy code — which looked
/// like a broken panel rather than the good news it was.
///
/// 150ms sits above p99 on that sample, so a site clearing it is a genuine
/// outlier rather than the top of a normal distribution.
const MIN_SITE_MS: f64 = 150.0;

/// A file must hold at least this share of its target's type-checking time,
/// *and* [`MIN_FILE_MS`] in absolute terms, before it is called concentrated.
/// A share alone would flag the only file in a target that barely type-checks
/// at all.
///
/// The same recalibration: the worst file in that app held 12% of its target,
/// so a 40% bar could only ever fire on a project with one dominant file.
const FILE_SHARE: f64 = 0.25;
const MIN_FILE_MS: f64 = 400.0;

/// Likewise for a whole target: a share of a build that takes no time is not
/// a finding.
///
/// That app spent 1.95s type-checking across a 27s target — 7%, which is
/// healthy and should stay silent. 15% is the point where type-checking is a
/// large enough slice to be worth naming.
const TARGET_SHARE: f64 = 0.15;
const MIN_TARGET_MS: f64 = 1_500.0;

/// Site-level advice past this point is a list nobody reads to the end — the
/// same reasoning as `bottleneck::MAX_BOTTLENECKS`.
const MAX_SITES: usize = 5;

/// Ranks Swift type-checking costs into advice.
///
/// Returns empty when the build carries no Swift timings at all, which is the
/// normal case: they only exist when the project is compiled with
/// `-warn-long-function-bodies` / `-warn-long-expression-type-checking`. An
/// empty result here means "not measured", not "nothing to fix", and the
/// caller is responsible for not presenting it as the latter.
pub fn advice(analysis: &BuildAnalysis) -> Vec<Advice> {
    let Some(metrics) = &analysis.metrics else {
        return Vec::new();
    };
    let seconds_by_target: Vec<(&str, f64)> = metrics
        .targets
        .iter()
        .map(|target| (target.name.as_str(), target.seconds))
        .collect();
    from_timings(&metrics.swift_timings, &seconds_by_target, metrics.category)
}

/// The same analysis, over timings that did not come from a parsed log.
///
/// The dashboard reads builds back from Postgres and never holds a
/// `BuildAnalysis`, so without this it would need its own copy of the
/// thresholds — and a threshold that differs between the CLI and the dashboard
/// is a bug that shows as two tools disagreeing about the same build.
///
/// `seconds_by_target` may repeat a name (one row per project or
/// architecture); the durations are summed, as `bottleneck` does.
pub fn from_timings(
    timings: &[SwiftTimingMetric],
    seconds_by_target: &[(&str, f64)],
    category: BuildCategory,
) -> Vec<Advice> {
    // A noop build compiled nothing: Xcode replayed a previous build's log, so
    // these timings record type-checking that happened in *that* build, beside
    // per-target seconds inherited from it. Advising on them would attribute
    // another build's cost to this one, and the share rules would divide this
    // build's milliseconds by a duration it never spent — one real noop here
    // reported 0.59s total against a 27.22s target.
    //
    // Silence rather than a caveat: there is no version of "this expression is
    // slow" that is true of a build which ran no compiler.
    if category == BuildCategory::Noop {
        return Vec::new();
    }
    if timings.is_empty() {
        return Vec::new();
    }
    let mut result = Vec::new();
    result.extend(site_advice(timings));
    result.extend(file_advice(timings));
    result.extend(target_advice(timings, seconds_by_target));
    result
}

/// The individual expressions and function bodies worth naming.
///
/// `map()` already sorts timings by duration descending, so this takes the
/// front of the list rather than sorting again — but it filters first, because
/// a build may have many sites over the threshold and only the worst few are
/// reported.
fn site_advice(timings: &[SwiftTimingMetric]) -> Vec<Advice> {
    // A function body's cost includes the expressions inside it, so a body and
    // one of its own expressions both appearing would report the same
    // milliseconds twice under two different headings. Bodies win: the
    // expression is visible underneath the body's advice, and the body is the
    // thing to split.
    let bodies: Vec<&SwiftTimingMetric> = timings
        .iter()
        .filter(|timing| timing.kind == SwiftTimingKind::FunctionBody)
        .collect();

    timings
        .iter()
        .filter(|timing| timing.milliseconds >= MIN_SITE_MS)
        .filter(|timing| {
            timing.kind == SwiftTimingKind::FunctionBody || !inside_reported_body(timing, &bodies)
        })
        .take(MAX_SITES)
        .map(|timing| match (&timing.symbol, timing.kind) {
            (Some(symbol), SwiftTimingKind::FunctionBody) => Advice {
                kind: AdviceKind::LargeFunctionBody,
                explanation: language::large_function_body(symbol, timing.milliseconds),
                ..site(timing)
            },
            _ => Advice {
                kind: AdviceKind::ExpressionHotspot,
                explanation: language::expression_hotspot(timing.milliseconds),
                ..site(timing)
            },
        })
        .collect()
}

/// Whether an expression sits inside a function body that is itself reported.
///
/// The activity log gives no containment relation, only locations, so this is
/// decided by file and a line window: a body reported at line N covers the
/// expressions the compiler attributed to lines at or after it. Without an end
/// line this can only be approximate, so the window is bounded rather than
/// open-ended — an expression 400 lines below a body is far more likely to be
/// in a later function than in a very long one.
fn inside_reported_body(expression: &SwiftTimingMetric, bodies: &[&SwiftTimingMetric]) -> bool {
    const BODY_LINE_WINDOW: u32 = 200;
    bodies.iter().any(|body| {
        body.milliseconds >= MIN_SITE_MS
            && body.file == expression.file
            && expression.line >= body.line
            && expression.line.saturating_sub(body.line) <= BODY_LINE_WINDOW
    })
}

/// The `Advice` fields that come straight off a timing, so the two arms above
/// differ only in kind and wording.
fn site(timing: &SwiftTimingMetric) -> Advice {
    Advice {
        kind: AdviceKind::ExpressionHotspot,
        file: timing.file.clone(),
        line: timing.line,
        column: timing.column,
        symbol: timing.symbol.clone(),
        target: timing.target.clone(),
        milliseconds: timing.milliseconds,
        explanation: String::new(),
    }
}

/// Files holding a large share of their own target's type-checking time.
///
/// Per target, not per build: a monorepo's slowest file would otherwise be
/// compared against every other target's total and never clear the threshold.
fn file_advice(timings: &[SwiftTimingMetric]) -> Vec<Advice> {
    // (target, file) -> summed milliseconds. BTreeMap so equal shares come out
    // in a stable order rather than a hash-random one.
    let mut per_file: BTreeMap<(&str, &str), f64> = BTreeMap::new();
    let mut per_target: BTreeMap<&str, f64> = BTreeMap::new();
    for timing in timings {
        if timing.file.is_empty() {
            continue;
        }
        let target = timing.target.as_deref().unwrap_or("");
        *per_file.entry((target, timing.file.as_str())).or_default() += timing.milliseconds;
        *per_target.entry(target).or_default() += timing.milliseconds;
    }

    let mut result: Vec<Advice> = per_file
        .into_iter()
        .filter_map(|((target, file), milliseconds)| {
            let total = per_target.get(target).copied().unwrap_or(0.0);
            let share = if total > 0.0 {
                milliseconds / total
            } else {
                0.0
            };
            // A file that is the only one measured in its target is 100% of it
            // by arithmetic, not by being concentrated; the absolute floor is
            // what stops that being reported as a finding.
            (share >= FILE_SHARE && milliseconds >= MIN_FILE_MS).then(|| Advice {
                kind: AdviceKind::ConcentratedFile,
                file: file.to_owned(),
                line: 0,
                column: 0,
                symbol: None,
                target: (!target.is_empty()).then(|| target.to_owned()),
                milliseconds,
                explanation: language::concentrated_file(milliseconds, percent(share)),
            })
        })
        .collect();
    result.sort_by(|a, b| b.milliseconds.total_cmp(&a.milliseconds));
    result
}

/// Targets where type-checking is a large share of the whole build step.
fn target_advice(timings: &[SwiftTimingMetric], seconds_by_target: &[(&str, f64)]) -> Vec<Advice> {
    let mut per_target: BTreeMap<&str, f64> = BTreeMap::new();
    for timing in timings {
        if let Some(target) = timing.target.as_deref() {
            *per_target.entry(target).or_default() += timing.milliseconds;
        }
    }

    let mut result: Vec<Advice> = per_target
        .into_iter()
        .filter_map(|(target, milliseconds)| {
            // Same-named targets in different projects are summed, matching
            // what `bottleneck` does for the same reason.
            let seconds: f64 = seconds_by_target
                .iter()
                .filter(|(name, _)| *name == target)
                .map(|(_, seconds)| *seconds)
                .sum();
            let total_ms = seconds * 1_000.0;
            let share = if total_ms > 0.0 {
                milliseconds / total_ms
            } else {
                0.0
            };
            (share >= TARGET_SHARE && milliseconds >= MIN_TARGET_MS).then(|| Advice {
                kind: AdviceKind::TargetDominatedByTypeChecking,
                file: String::new(),
                line: 0,
                column: 0,
                symbol: None,
                target: Some(target.to_owned()),
                milliseconds,
                explanation: language::target_dominated(percent(share), milliseconds),
            })
        })
        .collect();
    result.sort_by(|a, b| b.milliseconds.total_cmp(&a.milliseconds));
    result
}

/// A share as whole percent, clamped: a rounding artefact reporting 101% would
/// read as a bug in the measurement rather than in the rounding.
fn percent(share: f64) -> u8 {
    (share * 100.0).round().clamp(0.0, 100.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::analysis_with_targets;
    use buildlens_core::BuildAnalysis;

    fn timing(
        kind: SwiftTimingKind,
        file: &str,
        line: u32,
        symbol: Option<&str>,
        milliseconds: f64,
        target: &str,
    ) -> SwiftTimingMetric {
        SwiftTimingMetric {
            kind,
            file: file.into(),
            line,
            column: 1,
            symbol: symbol.map(str::to_owned),
            milliseconds,
            target: Some(target.into()),
        }
    }

    /// Sorted descending, as `map()` leaves them.
    fn analysis_with_timings(timings: Vec<SwiftTimingMetric>) -> BuildAnalysis {
        let mut analysis = analysis_with_targets(&[("App", 10.0)]);
        let mut timings = timings;
        timings.sort_by(|a, b| b.milliseconds.total_cmp(&a.milliseconds));
        if let Some(metrics) = analysis.metrics.as_mut() {
            metrics.swift_timings = timings;
        }
        analysis
    }

    #[test]
    fn no_timings_yields_no_advice() {
        let analysis = analysis_with_timings(vec![]);
        assert!(advice(&analysis).is_empty());
    }

    #[test]
    fn sites_below_the_floor_are_ignored() {
        let analysis = analysis_with_timings(vec![timing(
            SwiftTimingKind::TypeCheck,
            "/a/Slow.swift",
            10,
            None,
            120.0,
            "App",
        )]);
        assert!(
            advice(&analysis)
                .iter()
                .all(|item| item.kind != AdviceKind::ExpressionHotspot)
        );
    }

    #[test]
    fn an_expensive_expression_is_reported_with_its_location() {
        let analysis = analysis_with_timings(vec![timing(
            SwiftTimingKind::TypeCheck,
            "/a/Slow.swift",
            47,
            None,
            3_800.0,
            "App",
        )]);
        let found = advice(&analysis);
        let hotspot = found
            .iter()
            .find(|item| item.kind == AdviceKind::ExpressionHotspot)
            .expect("expression hotspot");
        assert_eq!(hotspot.line, 47);
        assert_eq!(hotspot.file, "/a/Slow.swift");
        assert!(hotspot.explanation.contains("3800ms"));
    }

    #[test]
    fn a_function_body_is_reported_as_a_body_not_an_expression() {
        let analysis = analysis_with_timings(vec![timing(
            SwiftTimingKind::FunctionBody,
            "/a/Slow.swift",
            6,
            Some("gnarly()"),
            2_400.0,
            "App",
        )]);
        let found = advice(&analysis);
        let body = found
            .iter()
            .find(|item| item.kind == AdviceKind::LargeFunctionBody)
            .expect("function body advice");
        assert_eq!(body.symbol.as_deref(), Some("gnarly()"));
        assert!(body.explanation.contains("gnarly()"));
    }

    /// A body's milliseconds already include the expressions inside it, so
    /// reporting both counts the same time twice under two headings.
    #[test]
    fn an_expression_inside_a_reported_body_is_not_reported_again() {
        let analysis = analysis_with_timings(vec![
            timing(
                SwiftTimingKind::FunctionBody,
                "/a/Slow.swift",
                40,
                Some("body"),
                3_000.0,
                "App",
            ),
            timing(
                SwiftTimingKind::TypeCheck,
                "/a/Slow.swift",
                47,
                None,
                2_800.0,
                "App",
            ),
        ]);
        let found = advice(&analysis);
        assert_eq!(
            found
                .iter()
                .filter(|item| {
                    matches!(
                        item.kind,
                        AdviceKind::ExpressionHotspot | AdviceKind::LargeFunctionBody
                    )
                })
                .count(),
            1,
            "the body subsumes the expression inside it"
        );
    }

    /// The same line number in a different file is a different place.
    #[test]
    fn an_expression_in_another_file_is_still_reported() {
        let analysis = analysis_with_timings(vec![
            timing(
                SwiftTimingKind::FunctionBody,
                "/a/One.swift",
                40,
                Some("body"),
                3_000.0,
                "App",
            ),
            timing(
                SwiftTimingKind::TypeCheck,
                "/a/Two.swift",
                47,
                None,
                2_800.0,
                "App",
            ),
        ]);
        let found = advice(&analysis);
        assert_eq!(
            found
                .iter()
                .filter(|item| {
                    matches!(
                        item.kind,
                        AdviceKind::ExpressionHotspot | AdviceKind::LargeFunctionBody
                    )
                })
                .count(),
            2
        );
    }

    /// A single measured file is 100% of its target by arithmetic. Without an
    /// absolute floor every such build would report a "concentrated" file.
    #[test]
    fn a_small_single_file_is_not_concentrated() {
        let analysis = analysis_with_timings(vec![timing(
            SwiftTimingKind::TypeCheck,
            "/a/Slow.swift",
            10,
            None,
            300.0,
            "App",
        )]);
        assert!(
            advice(&analysis)
                .iter()
                .all(|item| item.kind != AdviceKind::ConcentratedFile)
        );
    }

    #[test]
    fn a_file_holding_most_of_a_targets_time_is_concentrated() {
        let analysis = analysis_with_timings(vec![
            timing(
                SwiftTimingKind::TypeCheck,
                "/a/Hot.swift",
                10,
                None,
                4_000.0,
                "App",
            ),
            timing(
                SwiftTimingKind::TypeCheck,
                "/a/Cool.swift",
                10,
                None,
                500.0,
                "App",
            ),
        ]);
        let found = advice(&analysis);
        let file = found
            .iter()
            .find(|item| item.kind == AdviceKind::ConcentratedFile)
            .expect("concentrated file");
        assert_eq!(file.file, "/a/Hot.swift");
        assert!(file.explanation.contains("89%"), "{}", file.explanation);
    }

    /// 10s target, 4.5s of type-checking: 45%, over the 25% threshold.
    #[test]
    fn a_target_dominated_by_type_checking_is_reported() {
        let analysis = analysis_with_timings(vec![timing(
            SwiftTimingKind::TypeCheck,
            "/a/Hot.swift",
            10,
            None,
            4_500.0,
            "App",
        )]);
        let found = advice(&analysis);
        let target = found
            .iter()
            .find(|item| item.kind == AdviceKind::TargetDominatedByTypeChecking)
            .expect("target advice");
        assert_eq!(target.target.as_deref(), Some("App"));
        assert!(target.explanation.contains("45%"), "{}", target.explanation);
    }

    #[test]
    fn only_the_worst_few_sites_are_reported() {
        let timings = (0..20)
            .map(|index| {
                timing(
                    SwiftTimingKind::TypeCheck,
                    &format!("/a/File{index}.swift"),
                    10,
                    None,
                    1_000.0 + index as f64,
                    "App",
                )
            })
            .collect();
        let found = advice(&analysis_with_timings(timings));
        assert_eq!(
            found
                .iter()
                .filter(|item| item.kind == AdviceKind::ExpressionHotspot)
                .count(),
            MAX_SITES
        );
    }

    /// Advice is persisted and compared across builds, so the discriminant
    /// must not move with a `Debug` reformat.
    /// Regression guard for the calibration, taken from a real SwiftUI app:
    /// 91 sites, median 17ms, slowest 93ms, 1.95s of type-checking in a 27s
    /// target. That is a healthy build and every rule must stay silent on it.
    /// An earlier threshold set tuned on a deliberately pathological package
    /// also reported nothing here, but for the wrong reason — it would have
    /// stayed silent on a genuinely slow project too.
    #[test]
    fn a_healthy_real_world_build_produces_no_advice() {
        let timings = [93.0, 71.0, 64.0, 55.0, 55.0, 52.0, 46.0, 44.0, 42.0, 38.0]
            .iter()
            .enumerate()
            .map(|(index, milliseconds)| {
                timing(
                    SwiftTimingKind::FunctionBody,
                    &format!("/app/View{index}.swift"),
                    10,
                    Some("body"),
                    *milliseconds,
                    "TestRouterApp",
                )
            })
            .collect();
        let mut analysis = analysis_with_timings(timings);
        // The 27.2s target the 1.95s of type-checking sat inside.
        if let Some(metrics) = analysis.metrics.as_mut() {
            metrics.targets[0].name = "TestRouterApp".into();
            metrics.targets[0].seconds = 27.2;
        }
        assert!(
            advice(&analysis).is_empty(),
            "a build spending 7% of a target on type-checking has nothing to advise"
        );
    }

    /// A noop build compiled nothing — Xcode replayed a previous build's log,
    /// so its timings and its per-target seconds both belong to that earlier
    /// build. A real one recorded 0.59s total beside a 27.22s target, which is
    /// the denominator the share rules would otherwise divide by.
    #[test]
    fn a_noop_build_gets_no_advice_however_slow_its_replayed_timings() {
        let timings = vec![timing(
            SwiftTimingKind::TypeCheck,
            "/app/Slow.swift",
            47,
            None,
            3_800.0,
            "App",
        )];
        // The same timings on a real build do produce advice, so this is the
        // category deciding it and not the threshold.
        assert!(!advice(&analysis_with_timings(timings.clone())).is_empty());

        let mut analysis = analysis_with_timings(timings);
        if let Some(metrics) = analysis.metrics.as_mut() {
            metrics.category = BuildCategory::Noop;
        }
        assert!(
            advice(&analysis).is_empty(),
            "a build that ran no compiler has no type-checking to advise on"
        );
    }

    /// The other half of the calibration: a site that is a real outlier still
    /// clears the floor, so lowering it did not make the rule unfireable.
    #[test]
    fn a_genuine_outlier_still_clears_the_floor() {
        let analysis = analysis_with_timings(vec![timing(
            SwiftTimingKind::TypeCheck,
            "/app/Slow.swift",
            47,
            None,
            354.0,
            "App",
        )]);
        assert!(
            advice(&analysis)
                .iter()
                .any(|item| item.kind == AdviceKind::ExpressionHotspot)
        );
    }

    #[test]
    fn kind_strings_are_stable() {
        assert_eq!(AdviceKind::ExpressionHotspot.as_str(), "expression_hotspot");
        assert_eq!(
            AdviceKind::LargeFunctionBody.as_str(),
            "large_function_body"
        );
        assert_eq!(AdviceKind::ConcentratedFile.as_str(), "concentrated_file");
        assert_eq!(
            AdviceKind::TargetDominatedByTypeChecking.as_str(),
            "target_dominated_by_type_checking"
        );
    }
}
