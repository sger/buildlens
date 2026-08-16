//! Which target compiles each changed file, and what sits downstream of it.

use buildlens_core::{BuildAnalysis, MatchKind, TargetImpact, sourcepath};
use buildlens_graph::TargetGraph;

/// Maps each git-changed file to the target that compiles it (via per-file
/// metrics) and the targets blocked downstream of that target.
pub fn impacts(analysis: &BuildAnalysis, graph: &TargetGraph) -> Vec<TargetImpact> {
    let Some(git) = &analysis.git else {
        return Vec::new();
    };
    let Some(metrics) = &analysis.metrics else {
        return Vec::new();
    };
    git.changed_files
        .iter()
        .map(|changed| {
            let matched = metrics.files.iter().find_map(|file| {
                if sourcepath::normalize_separators(&file.file)
                    == sourcepath::normalize_separators(changed)
                {
                    Some((file, MatchKind::Exact))
                } else if sourcepath::same_file(&file.file, changed) {
                    Some((file, MatchKind::Suffix))
                } else {
                    None
                }
            });
            match matched {
                Some((file, match_kind)) => {
                    let owning_target = file.target.clone();
                    let downstream_targets = owning_target
                        .as_deref()
                        .map(|name| downstream_of(graph, name))
                        .unwrap_or_default();
                    TargetImpact {
                        changed_file: changed.clone(),
                        owning_target,
                        downstream_targets,
                        match_kind,
                    }
                }
                None => TargetImpact {
                    changed_file: changed.clone(),
                    owning_target: None,
                    downstream_targets: Vec::new(),
                    match_kind: MatchKind::None,
                },
            }
        })
        .collect()
}

/// Targets blocked downstream of `name`.
///
/// Build metrics identify a target by bare name, while the graph keys on name
/// *and* project. When one name belongs to several projects the answer is the
/// union of their dependents: the metrics cannot say which project compiled
/// the file, and reporting just one would understate the blast radius.
pub(crate) fn downstream_of(graph: &TargetGraph, name: &str) -> Vec<String> {
    let mut downstream: Vec<String> = graph
        .find_all(name)
        .into_iter()
        .flat_map(|target| graph.dependents(target))
        .map(|target| target.name.clone())
        .collect();
    downstream.sort();
    downstream.dedup();
    downstream
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{analysis_with_files, node};
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
    fn an_exactly_matching_path_is_reported_as_exact() {
        let analysis = analysis_with_files(
            &["Sources/Core/Thing.swift"],
            &[("Sources/Core/Thing.swift", "Core")],
        );
        let impacts = impacts(&analysis, &graph_of(&[("App", "Core")]));
        assert_eq!(impacts[0].match_kind, MatchKind::Exact);
        assert_eq!(impacts[0].owning_target.as_deref(), Some("Core"));
        assert_eq!(impacts[0].downstream_targets, ["App"]);
    }

    #[test]
    fn an_absolute_metric_path_matches_a_relative_change_as_suffix() {
        let analysis = analysis_with_files(
            &["Sources/Core/Thing.swift"],
            &[("/build/repo/Sources/Core/Thing.swift", "Core")],
        );
        let impacts = impacts(&analysis, &graph_of(&[("App", "Core")]));
        assert_eq!(impacts[0].match_kind, MatchKind::Suffix);
        assert_eq!(impacts[0].owning_target.as_deref(), Some("Core"));
    }

    /// The boundary bug: `a.swift` must not match `xa.swift`.
    #[test]
    fn a_partial_filename_does_not_match_a_compiled_file() {
        let analysis =
            analysis_with_files(&["a.swift"], &[("/build/repo/Sources/xa.swift", "Core")]);
        let impacts = impacts(&analysis, &graph_of(&[("App", "Core")]));
        assert_eq!(impacts[0].match_kind, MatchKind::None);
        assert_eq!(impacts[0].owning_target, None);
    }

    #[test]
    fn the_same_filename_in_another_directory_does_not_match() {
        let analysis = analysis_with_files(
            &["Sources/Thing.swift"],
            &[("/build/repo/Vendor/Thing.swift", "Vendor")],
        );
        assert_eq!(
            impacts(&analysis, &graph_of(&[("App", "Core")]))[0].match_kind,
            MatchKind::None
        );
    }

    #[test]
    fn an_unmatched_file_has_no_owning_target() {
        let analysis = analysis_with_files(&["README.md"], &[("Sources/Core/Thing.swift", "Core")]);
        let impacts = impacts(&analysis, &graph_of(&[("App", "Core")]));
        assert_eq!(impacts[0].owning_target, None);
        assert_eq!(impacts[0].match_kind, MatchKind::None);
        assert!(impacts[0].downstream_targets.is_empty());
    }

    /// A target legitimately named `unknown` used to be indistinguishable from
    /// "no target found".
    #[test]
    fn a_target_actually_named_unknown_is_not_a_sentinel() {
        let analysis = analysis_with_files(
            &["Sources/Odd/Thing.swift"],
            &[("Sources/Odd/Thing.swift", "unknown")],
        );
        let impacts = impacts(&analysis, &graph_of(&[("App", "unknown")]));
        assert_eq!(impacts[0].owning_target.as_deref(), Some("unknown"));
        assert_eq!(
            impacts[0].downstream_targets,
            ["App"],
            "a real target named `unknown` must still resolve downstream"
        );
    }

    /// Metrics name a target without a project, so a name in two projects must
    /// contribute both sets of dependents.
    #[test]
    fn downstream_of_an_ambiguous_name_unions_every_project() {
        let summary = TargetGraphSummary {
            targets: vec![
                buildlens_core::TargetNode {
                    name: "Utils".into(),
                    project: "P1".into(),
                },
                buildlens_core::TargetNode {
                    name: "Utils".into(),
                    project: "P2".into(),
                },
                node("AppOne"),
                node("AppTwo"),
            ],
            dependencies: vec![
                TargetDependency {
                    from: node("AppOne"),
                    to: buildlens_core::TargetNode {
                        name: "Utils".into(),
                        project: "P1".into(),
                    },
                },
                TargetDependency {
                    from: node("AppTwo"),
                    to: buildlens_core::TargetNode {
                        name: "Utils".into(),
                        project: "P2".into(),
                    },
                },
            ],
            ..Default::default()
        };
        let graph = TargetGraph::new(&summary);
        assert_eq!(downstream_of(&graph, "Utils"), ["AppOne", "AppTwo"]);
    }

    #[test]
    fn a_target_absent_from_the_graph_has_no_downstream() {
        assert!(downstream_of(&graph_of(&[("App", "Core")]), "Ghost").is_empty());
    }

    #[test]
    fn no_git_correlation_yields_no_impacts() {
        let mut analysis = analysis_with_files(
            &["Sources/Core/Thing.swift"],
            &[("Sources/Core/Thing.swift", "Core")],
        );
        analysis.git = None;
        assert!(impacts(&analysis, &graph_of(&[("App", "Core")])).is_empty());
    }

    #[test]
    fn no_metrics_yields_no_impacts() {
        let mut analysis = analysis_with_files(
            &["Sources/Core/Thing.swift"],
            &[("Sources/Core/Thing.swift", "Core")],
        );
        analysis.metrics = None;
        assert!(impacts(&analysis, &graph_of(&[("App", "Core")])).is_empty());
    }
}
