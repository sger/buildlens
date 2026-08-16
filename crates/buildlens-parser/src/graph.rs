//! The target dependency graph, as `xcodebuild` prints it.
//!
//! Dependencies are listed under the target that declares them, so parsing is
//! stateful: an `Explicit dependency on target 'X'` line attaches to whichever
//! `Target 'Y' in project 'Z'` line came last.

use buildlens_core::{TargetDependency, TargetGraphSummary, TargetNode};
use regex::Regex;
use std::sync::OnceLock;

/// `Target dependency graph (12 targets)` — the header stating how many
/// targets Xcode resolved.
fn declared_count_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"Target dependency graph \((\d+) targets?\)").expect("valid regex")
    })
}

/// `Target 'App' in project 'MyApp'`.
fn target_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"Target '([^']+)' in project '([^']+)'").expect("valid regex"))
}

/// `➜ Explicit dependency on target 'Core' in project 'MyApp'`. The project is
/// omitted when it is the same as the declaring target's.
fn dependency_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?:Explicit dependency on target|Target dependency on target) '([^']+)'(?: in project '([^']+)')?",
        )
        .expect("valid regex")
    })
}

/// Parses one graph line into `summary`, returning whether it was consumed.
pub fn parse(line: &str, summary: &mut TargetGraphSummary) -> bool {
    if let Some(captures) = declared_count_re().captures(line) {
        summary.declared_count = captures[1].parse().ok();
        return true;
    }
    if line.trim_start().starts_with("Target '")
        && let Some(captures) = target_re().captures(line)
    {
        summary.targets.push(TargetNode {
            name: captures[1].to_owned(),
            project: captures[2].to_owned(),
        });
        return true;
    }
    if let Some(captures) = dependency_re().captures(line)
        && let Some(from) = summary.targets.last().cloned()
    {
        let to_project = captures
            .get(2)
            .map(|value| value.as_str().to_owned())
            .unwrap_or_else(|| from.project.clone());
        // A target listing itself is not a dependency edge.
        if from.name == captures[1] && from.project == to_project {
            return true;
        }
        summary.dependencies.push(TargetDependency {
            from,
            to: TargetNode {
                name: captures[1].to_owned(),
                project: to_project,
            },
        });
        return true;
    }
    false
}
pub fn finalize(g: &mut TargetGraphSummary) {
    g.targets.sort();
    g.targets.dedup();
    g.dependencies
        .sort_by_key(|d| (d.from.clone(), d.to.clone()));
    g.dependencies
        .dedup_by(|a, b| a.from == b.from && a.to == b.to);
    for d in &g.dependencies {
        *g.fan_out.entry(d.from.name.clone()).or_default() += 1;
        *g.fan_in.entry(d.to.name.clone()).or_default() += 1;
        g.reverse_dependencies
            .entry(d.to.name.clone())
            .or_default()
            .push(d.from.name.clone());
    }
    let mut v: Vec<_> = g.fan_in.iter().collect();
    v.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
    g.hotspots = v.into_iter().take(20).map(|(n, _)| n.clone()).collect();
    for values in g.reverse_dependencies.values_mut() {
        values.sort();
        values.dedup();
    }
    g.cycles = find_cycles(g);
}

/// Strongly connected components of the dependency graph, each reported once
/// as its sorted member list.
///
/// Delegates to [`buildlens_graph::TargetGraph`], which uses Tarjan's
/// algorithm — one linear pass. The hand-rolled predecessor walked the graph
/// from *every* node with no memoization: 400 targets took 105ms, 4000 did not
/// finish in two minutes, and a cycle of N nodes was reported N times, once per
/// rotation.
fn find_cycles(g: &TargetGraphSummary) -> Vec<Vec<String>> {
    let graph = buildlens_graph::TargetGraph::new(g);
    let mut cycles: Vec<Vec<String>> = graph
        .cycles()
        .into_iter()
        .map(|component| {
            let mut members: Vec<String> = component.iter().map(|node| key(node)).collect();
            members.sort();
            members.dedup();
            members
        })
        .collect();
    cycles.sort();
    cycles.dedup();
    cycles
}

fn key(node: &TargetNode) -> String {
    format!("{}::{}", node.project, node.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(lines: &[&str]) -> TargetGraphSummary {
        let mut summary = TargetGraphSummary::default();
        for line in lines {
            parse(line, &mut summary);
        }
        finalize(&mut summary);
        summary
    }

    #[test]
    fn reads_the_declared_target_count() {
        let summary = parse_all(&["Target dependency graph (12 targets)"]);
        assert_eq!(summary.declared_count, Some(12));
        // Singular is also valid wording.
        assert_eq!(
            parse_all(&["Target dependency graph (1 target)"]).declared_count,
            Some(1)
        );
    }

    #[test]
    fn a_dependency_attaches_to_the_preceding_target() {
        let summary = parse_all(&[
            "Target 'App' in project 'MyApp'",
            "    ➜ Explicit dependency on target 'Core' in project 'MyApp'",
        ]);
        assert_eq!(summary.dependencies.len(), 1);
        assert_eq!(summary.dependencies[0].from.name, "App");
        assert_eq!(summary.dependencies[0].to.name, "Core");
    }

    /// The project is omitted when it matches the declaring target's.
    #[test]
    fn a_dependency_without_a_project_inherits_the_declaring_target_s() {
        let summary = parse_all(&[
            "Target 'App' in project 'MyApp'",
            "    ➜ Explicit dependency on target 'Core'",
        ]);
        assert_eq!(summary.dependencies[0].to.project, "MyApp");
    }

    #[test]
    fn a_cross_project_dependency_keeps_its_own_project() {
        let summary = parse_all(&[
            "Target 'App' in project 'MyApp'",
            "    ➜ Explicit dependency on target 'Utils' in project 'Vendor'",
        ]);
        assert_eq!(summary.dependencies[0].to.project, "Vendor");
    }

    #[test]
    fn a_target_listing_itself_is_not_an_edge() {
        let summary = parse_all(&[
            "Target 'App' in project 'MyApp'",
            "    ➜ Explicit dependency on target 'App' in project 'MyApp'",
        ]);
        assert!(summary.dependencies.is_empty());
    }

    /// A dependency line with no preceding target has nothing to attach to.
    #[test]
    fn an_orphan_dependency_line_is_ignored() {
        let summary = parse_all(&["    ➜ Explicit dependency on target 'Core' in project 'P'"]);
        assert!(summary.dependencies.is_empty());
    }

    #[test]
    fn fan_in_and_fan_out_are_counted() {
        let summary = parse_all(&[
            "Target 'App' in project 'P'",
            "    ➜ Explicit dependency on target 'Core' in project 'P'",
            "Target 'Tests' in project 'P'",
            "    ➜ Explicit dependency on target 'Core' in project 'P'",
        ]);
        assert_eq!(summary.fan_in.get("Core"), Some(&2));
        assert_eq!(summary.fan_out.get("App"), Some(&1));
        assert_eq!(
            summary.reverse_dependencies.get("Core").map(Vec::len),
            Some(2)
        );
    }

    #[test]
    fn duplicate_edges_are_collapsed() {
        let summary = parse_all(&[
            "Target 'App' in project 'P'",
            "    ➜ Explicit dependency on target 'Core' in project 'P'",
            "    ➜ Explicit dependency on target 'Core' in project 'P'",
        ]);
        assert_eq!(summary.dependencies.len(), 1);
        assert_eq!(summary.fan_in.get("Core"), Some(&1));
    }

    /// One SCC per cycle, not one per rotation.
    #[test]
    fn a_cycle_is_reported_once_with_all_its_members() {
        let summary = parse_all(&[
            "Target 'A' in project 'P'",
            "    ➜ Explicit dependency on target 'B' in project 'P'",
            "Target 'B' in project 'P'",
            "    ➜ Explicit dependency on target 'C' in project 'P'",
            "Target 'C' in project 'P'",
            "    ➜ Explicit dependency on target 'A' in project 'P'",
        ]);
        assert_eq!(summary.cycles.len(), 1);
        assert_eq!(summary.cycles[0], vec!["P::A", "P::B", "P::C"]);
    }

    #[test]
    fn two_independent_cycles_are_reported_separately() {
        let summary = parse_all(&[
            "Target 'A' in project 'P'",
            "    ➜ Explicit dependency on target 'B' in project 'P'",
            "Target 'B' in project 'P'",
            "    ➜ Explicit dependency on target 'A' in project 'P'",
            "Target 'X' in project 'P'",
            "    ➜ Explicit dependency on target 'Y' in project 'P'",
            "Target 'Y' in project 'P'",
            "    ➜ Explicit dependency on target 'X' in project 'P'",
        ]);
        assert_eq!(summary.cycles.len(), 2);
    }

    #[test]
    fn an_acyclic_graph_has_no_cycles() {
        let summary = parse_all(&[
            "Target 'App' in project 'P'",
            "    ➜ Explicit dependency on target 'Core' in project 'P'",
            "Target 'Core' in project 'P'",
        ]);
        assert!(summary.cycles.is_empty());
    }

    /// Same name in two projects is two targets, so neither inherits the
    /// other's dependents.
    #[test]
    fn same_named_targets_in_different_projects_stay_distinct() {
        let summary = parse_all(&[
            "Target 'App' in project 'One'",
            "    ➜ Explicit dependency on target 'Utils' in project 'One'",
            "Target 'App' in project 'Two'",
            "    ➜ Explicit dependency on target 'Utils' in project 'Two'",
        ]);
        assert_eq!(summary.dependencies.len(), 2);
        assert!(summary.cycles.is_empty());
    }

    /// The regression this crate's worst bug produced: a large cyclic graph
    /// used to walk from every node with no memoization and never finish.
    #[test]
    fn a_large_cyclic_graph_finishes_quickly() {
        let count = 2_000;
        let mut lines = Vec::new();
        for index in 0..count {
            lines.push(format!("Target 'T{index}' in project 'P'"));
            lines.push(format!(
                "    ➜ Explicit dependency on target 'T{}' in project 'P'",
                (index + 1) % count
            ));
        }
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let summary = parse_all(&refs);
        assert_eq!(summary.dependencies.len(), count);
        assert_eq!(summary.cycles.len(), 1);
        assert_eq!(summary.cycles[0].len(), count);
    }

    #[test]
    fn ordinary_lines_are_not_graph_lines() {
        let mut summary = TargetGraphSummary::default();
        assert!(!parse("Compiling Foo.swift", &mut summary));
        assert!(!parse("", &mut summary));
    }

    #[test]
    fn hotspots_rank_the_most_depended_on_targets() {
        let summary = parse_all(&[
            "Target 'A' in project 'P'",
            "    ➜ Explicit dependency on target 'Core' in project 'P'",
            "Target 'B' in project 'P'",
            "    ➜ Explicit dependency on target 'Core' in project 'P'",
            "Target 'C' in project 'P'",
            "    ➜ Explicit dependency on target 'Leaf' in project 'P'",
        ]);
        assert_eq!(summary.hotspots.first().map(String::as_str), Some("Core"));
    }
}
