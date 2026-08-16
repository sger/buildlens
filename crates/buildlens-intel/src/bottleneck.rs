//! Which slow targets hold up the most downstream work.

use crate::impact::downstream_of;
use buildlens_core::{Bottleneck, BuildAnalysis};
use buildlens_graph::TargetGraph;
use std::collections::BTreeMap;

/// Targets faster than this are never bottlenecks: below about a second, the
/// measurement is mostly scheduling noise and shaving it saves nothing a
/// developer would notice.
const MIN_SECONDS: f64 = 1.0;

/// Only the worst few are worth reporting — a ranked list nobody reads to the
/// end is the same as no list.
const MAX_BOTTLENECKS: usize = 5;

/// Ranks slow targets by how much downstream work they block.
///
/// `score = seconds * (1 + blocked)`. Linear in both, because a target that
/// takes twice as long and one that blocks twice as many targets are equally
/// worth attention. The `1 +` keeps a slow target with nothing downstream on
/// the list — it still costs its own time — rather than zeroing it out.
pub fn bottlenecks(analysis: &BuildAnalysis, graph: &TargetGraph) -> Vec<Bottleneck> {
    let Some(metrics) = &analysis.metrics else {
        return Vec::new();
    };

    // Metrics may record the same target name more than once (one row per
    // project, or per architecture). Sum their durations so a split target is
    // not ranked as several small ones.
    let mut seconds_by_target: BTreeMap<&str, f64> = BTreeMap::new();
    for target in &metrics.targets {
        *seconds_by_target.entry(target.name.as_str()).or_default() += target.seconds;
    }

    let mut result: Vec<Bottleneck> = seconds_by_target
        .into_iter()
        .filter(|&(_, seconds)| seconds >= MIN_SECONDS)
        .map(|(name, seconds)| {
            let blocked = downstream_of(graph, name).len();
            Bottleneck {
                target: name.to_owned(),
                seconds,
                // From the graph, not the precomputed summary map: the map is
                // keyed by bare name and so collapses same-named targets in
                // different projects, which is the collision the graph fixes.
                fan_in: direct_dependents(graph, name),
                blocked_downstream: blocked,
                score: seconds * (1.0 + blocked as f64),
            }
        })
        .collect();
    // Score descending, then name, so equal scores keep a stable order.
    result.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.target.cmp(&b.target))
    });
    result.truncate(MAX_BOTTLENECKS);
    result
}

/// How many targets depend on this one directly.
fn direct_dependents(graph: &TargetGraph, name: &str) -> usize {
    let mut direct: Vec<&str> = graph
        .find_all(name)
        .into_iter()
        .flat_map(|target| graph.direct_dependents(target))
        .map(|target| target.name.as_str())
        .collect();
    direct.sort();
    direct.dedup();
    direct.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{analysis_with_targets, node};
    use buildlens_core::{TargetDependency, TargetGraphSummary};

    fn graph_of(edges: &[(&str, &str)]) -> TargetGraph {
        let mut names: Vec<&str> = edges.iter().flat_map(|(a, b)| [*a, *b]).collect();
        names.sort();
        names.dedup();
        TargetGraph::new(&TargetGraphSummary {
            targets: names.iter().map(|name| node(name)).collect(),
            dependencies: edges
                .iter()
                .map(|(from, to)| TargetDependency {
                    from: node(from),
                    to: node(to),
                })
                .collect(),
            ..Default::default()
        })
    }

    #[test]
    fn a_target_blocking_more_work_outranks_a_slower_one() {
        // Core is slower in aggregate effect: 10s blocking 2 targets (30)
        // beats Slow's 20s blocking nothing (20).
        let analysis = analysis_with_targets(&[("Core", 10.0), ("Slow", 20.0)]);
        let graph = graph_of(&[("App", "Core"), ("UI", "Core")]);
        let ranked = bottlenecks(&analysis, &graph);
        assert_eq!(ranked[0].target, "Core");
        assert_eq!(ranked[0].blocked_downstream, 2);
        assert!((ranked[0].score - 30.0).abs() < f64::EPSILON);
        assert_eq!(ranked[1].target, "Slow");
        assert!((ranked[1].score - 20.0).abs() < f64::EPSILON);
    }

    #[test]
    fn targets_below_the_floor_are_not_bottlenecks() {
        let analysis = analysis_with_targets(&[("Tiny", 0.5), ("Real", 4.0)]);
        let ranked = bottlenecks(&analysis, &graph_of(&[("App", "Real")]));
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].target, "Real");
    }

    #[test]
    fn at_most_five_bottlenecks_are_reported() {
        let targets: Vec<(String, f64)> = (0..9)
            .map(|index| (format!("T{index}"), 10.0 + index as f64))
            .collect();
        let refs: Vec<(&str, f64)> = targets
            .iter()
            .map(|(name, seconds)| (name.as_str(), *seconds))
            .collect();
        let ranked = bottlenecks(&analysis_with_targets(&refs), &graph_of(&[("A", "B")]));
        assert_eq!(ranked.len(), MAX_BOTTLENECKS);
        // The slowest survive.
        assert_eq!(ranked[0].target, "T8");
    }

    /// Two metric rows for one name (per-project or per-architecture) are one
    /// target, not two small ones.
    #[test]
    fn duplicate_target_rows_are_summed() {
        let analysis = analysis_with_targets(&[("Core", 6.0), ("Core", 4.0)]);
        let ranked = bottlenecks(&analysis, &graph_of(&[("App", "Core")]));
        assert_eq!(ranked.len(), 1);
        assert!((ranked[0].seconds - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn fan_in_counts_only_direct_dependents() {
        // App -> UI -> Core: Core's fan-in is 1 (UI), blocked is 2.
        let analysis = analysis_with_targets(&[("Core", 5.0)]);
        let ranked = bottlenecks(&analysis, &graph_of(&[("App", "UI"), ("UI", "Core")]));
        assert_eq!(ranked[0].fan_in, 1);
        assert_eq!(ranked[0].blocked_downstream, 2);
    }

    #[test]
    fn equal_scores_are_ordered_by_name() {
        let analysis = analysis_with_targets(&[("Zeta", 5.0), ("Alpha", 5.0)]);
        let ranked = bottlenecks(&analysis, &graph_of(&[("App", "Other")]));
        assert_eq!(ranked[0].target, "Alpha");
        assert_eq!(ranked[1].target, "Zeta");
    }

    #[test]
    fn no_metrics_yields_no_bottlenecks() {
        let mut analysis = analysis_with_targets(&[("Core", 5.0)]);
        analysis.metrics = None;
        assert!(bottlenecks(&analysis, &graph_of(&[("App", "Core")])).is_empty());
    }
}
