//! Queries over the target dependency graph.
//!
//! Build the graph once with [`TargetGraph::new`], then query it. Every query
//! used to rebuild the whole petgraph from scratch, which made per-target
//! analysis quadratic in the number of targets.
//!
//! # Edge direction
//!
//! An edge points **from a dependent to its dependency**: `App -> Core` means
//! App needs Core. So Core's *dependents* sit upstream (incoming edges) and
//! its *dependencies* sit downstream (outgoing).
//!
//! # Identity
//!
//! Targets are keyed by name *and* project. Two packages may each define a
//! `Utils` target, and collapsing them merges two unrelated subgraphs — which
//! silently corrupts dependents, fan-in, and everything derived from them.

use buildlens_core::{TargetGraphSummary, TargetNode};
use petgraph::graph::NodeIndex;
use petgraph::{Direction, Graph};
use std::collections::{HashMap, HashSet, VecDeque};

/// A target dependency graph, built once and queried many times.
pub struct TargetGraph {
    graph: Graph<TargetNode, ()>,
    /// Every declared target, including ones with no edges.
    index: HashMap<TargetNode, NodeIndex>,
    /// Edges whose endpoints were not declared in `targets`, kept so callers
    /// can report an incomplete graph rather than silently trusting it.
    dangling_edges: Vec<(TargetNode, TargetNode)>,
    /// Targets declared as depending on themselves.
    self_dependencies: Vec<TargetNode>,
}

impl TargetGraph {
    /// Builds the graph from a parsed summary.
    ///
    /// Duplicate edges are collapsed: petgraph's `add_edge` happily creates
    /// parallel edges, which would double-count fan-in and yield a neighbour
    /// twice during traversal.
    pub fn new(summary: &TargetGraphSummary) -> Self {
        let mut graph = Graph::<TargetNode, ()>::new();
        let mut index = HashMap::new();
        for target in &summary.targets {
            index
                .entry(target.clone())
                .or_insert_with(|| graph.add_node(target.clone()));
        }

        let mut dangling_edges = Vec::new();
        let mut self_dependencies = Vec::new();
        let mut seen_edges = HashSet::new();
        for dependency in &summary.dependencies {
            let (from, to) = (&dependency.from, &dependency.to);
            if from == to {
                self_dependencies.push(from.clone());
                continue;
            }
            match (index.get(from), index.get(to)) {
                (Some(&a), Some(&b)) => {
                    if seen_edges.insert((a, b)) {
                        graph.add_edge(a, b, ());
                    }
                }
                _ => dangling_edges.push((from.clone(), to.clone())),
            }
        }
        self_dependencies.sort();
        self_dependencies.dedup();

        Self {
            graph,
            index,
            dangling_edges,
            self_dependencies,
        }
    }

    /// Number of targets in the graph.
    pub fn len(&self) -> usize {
        self.graph.node_count()
    }

    pub fn is_empty(&self) -> bool {
        self.graph.node_count() == 0
    }

    /// True when the graph is a faithful picture of the summary it came from.
    ///
    /// Dropping an edge that names an undeclared target is the safe thing to
    /// do, but doing it silently means every downstream answer is subtly wrong
    /// with no indication. Callers should check this before trusting results.
    pub fn is_complete(&self) -> bool {
        self.dangling_edges.is_empty()
    }

    /// Edges referring to targets that were never declared.
    pub fn dangling_edges(&self) -> &[(TargetNode, TargetNode)] {
        &self.dangling_edges
    }

    /// Targets declared as depending on themselves — the smallest possible
    /// cycle, and always a modelling error.
    pub fn self_dependencies(&self) -> &[TargetNode] {
        &self.self_dependencies
    }

    /// Looks up a target by name alone.
    ///
    /// Returns `None` when the name is ambiguous across projects, because
    /// picking one arbitrarily is what the name-keyed version did wrong. Use
    /// [`TargetGraph::find_all`] to see the candidates, or query by
    /// [`TargetNode`] when the project is known.
    pub fn find(&self, name: &str) -> Option<&TargetNode> {
        let mut matches = self.find_all(name);
        match matches.len() {
            1 => Some(matches.remove(0)),
            _ => None,
        }
    }

    /// Every target with this name, across all projects.
    pub fn find_all(&self, name: &str) -> Vec<&TargetNode> {
        let mut found: Vec<&TargetNode> = self
            .index
            .keys()
            .filter(|target| target.name == name)
            .collect();
        found.sort();
        found
    }

    /// All targets that transitively depend on `target` — everything that must
    /// rebuild when it changes.
    pub fn dependents(&self, target: &TargetNode) -> Vec<&TargetNode> {
        self.walk(target, Direction::Incoming)
    }

    /// All targets `target` transitively depends on.
    pub fn dependencies(&self, target: &TargetNode) -> Vec<&TargetNode> {
        self.walk(target, Direction::Outgoing)
    }

    /// Breadth-first traversal, cycle-safe: the seed is marked seen up front,
    /// so a cycle back to it terminates and never reports a target as its own
    /// dependent.
    fn walk(&self, target: &TargetNode, direction: Direction) -> Vec<&TargetNode> {
        let Some(&start) = self.index.get(target) else {
            return Vec::new();
        };
        let mut seen: HashSet<NodeIndex> = HashSet::from([start]);
        let mut queue = VecDeque::from([start]);
        let mut found = Vec::new();
        while let Some(node) = queue.pop_front() {
            for next in self.graph.neighbors_directed(node, direction) {
                if seen.insert(next) {
                    found.push(&self.graph[next]);
                    queue.push_back(next);
                }
            }
        }
        found.sort();
        found
    }

    /// The shortest dependency chain from any root to `target`.
    ///
    /// A *root* is a target nothing depends on — an app or a top-level test
    /// bundle. The returned path starts at that root and ends at `target`, so
    /// it answers "what is the shortest way this target gets pulled into a
    /// build".
    ///
    /// Returns `None` when `target` is unknown or is itself a root, since a
    /// one-element path is not a chain and reads as a real answer when it is
    /// not. Ties are broken by node order so the result never depends on the
    /// order targets happened to be declared in.
    pub fn shortest_path_from_root(&self, target: &TargetNode) -> Option<Vec<&TargetNode>> {
        let &destination = self.index.get(target)?;
        // Walk *up* from the target: the first root reached by breadth-first
        // search is the closest one. Searching down from every root instead
        // returned whichever root happened to be visited first, which was
        // neither shortest nor stable across declaration order.
        let mut previous: HashMap<NodeIndex, NodeIndex> = HashMap::new();
        let mut seen: HashSet<NodeIndex> = HashSet::from([destination]);
        let mut queue = VecDeque::from([destination]);
        let mut best_root: Option<NodeIndex> = None;

        while let Some(node) = queue.pop_front() {
            let mut parents: Vec<NodeIndex> = self
                .graph
                .neighbors_directed(node, Direction::Incoming)
                .collect();
            if parents.is_empty() && node != destination {
                // A root, found at the current BFS depth. Ties are broken by
                // the target itself, never by node index: indices follow
                // declaration order, so using them would make the answer
                // depend on the order targets happened to be listed in.
                best_root = Some(match best_root {
                    Some(existing) if self.graph[existing] <= self.graph[node] => existing,
                    Some(_) | None => node,
                });
                continue;
            }
            if best_root.is_some() {
                // Already found a root at a shallower depth; anything deeper
                // cannot be shorter.
                continue;
            }
            // Sorted by target, not by index, so the recorded parent — and
            // therefore the reconstructed path body — is the same regardless
            // of the order targets were declared in.
            parents.sort_by(|&a, &b| self.graph[a].cmp(&self.graph[b]));
            for parent in parents {
                if seen.insert(parent) {
                    previous.insert(parent, node);
                    queue.push_back(parent);
                }
            }
        }

        let mut node = best_root?;
        let mut path = vec![&self.graph[node]];
        while let Some(&next) = previous.get(&node) {
            path.push(&self.graph[next]);
            node = next;
        }
        Some(path)
    }

    /// Targets nothing depends on — the entry points of the build.
    pub fn roots(&self) -> Vec<&TargetNode> {
        let mut found: Vec<&TargetNode> = self
            .graph
            .node_indices()
            .filter(|&node| {
                self.graph
                    .neighbors_directed(node, Direction::Incoming)
                    .next()
                    .is_none()
            })
            .map(|node| &self.graph[node])
            .collect();
        found.sort();
        found
    }

    /// Distinct cycles in the graph, each as a sorted set of member targets.
    ///
    /// Reported because a cycle in a build graph is always a defect, and
    /// because traversals silently tolerate cycles — so nothing else would
    /// surface them.
    pub fn cycles(&self) -> Vec<Vec<&TargetNode>> {
        let mut found: Vec<Vec<&TargetNode>> = petgraph::algo::tarjan_scc(&self.graph)
            .into_iter()
            .filter(|component| component.len() > 1)
            .map(|component| {
                let mut members: Vec<&TargetNode> = component
                    .into_iter()
                    .map(|node| &self.graph[node])
                    .collect();
                members.sort();
                members
            })
            .collect();
        found.sort();
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buildlens_core::TargetDependency;

    fn node(name: &str) -> TargetNode {
        TargetNode {
            name: name.into(),
            project: "P".into(),
        }
    }

    fn in_project(name: &str, project: &str) -> TargetNode {
        TargetNode {
            name: name.into(),
            project: project.into(),
        }
    }

    /// Builds a summary from edges, declaring every mentioned target.
    fn graph(edges: &[(&str, &str)]) -> TargetGraphSummary {
        let mut names: Vec<&str> = edges.iter().flat_map(|(a, b)| [*a, *b]).collect();
        names.sort();
        names.dedup();
        summary(&names, edges)
    }

    /// Builds a summary with an explicit target list, so declaration order and
    /// isolated targets can be controlled.
    fn summary(targets: &[&str], edges: &[(&str, &str)]) -> TargetGraphSummary {
        TargetGraphSummary {
            targets: targets.iter().map(|name| node(name)).collect(),
            dependencies: edges
                .iter()
                .map(|(from, to)| TargetDependency {
                    from: node(from),
                    to: node(to),
                })
                .collect(),
            ..Default::default()
        }
    }

    fn names(targets: Vec<&TargetNode>) -> Vec<String> {
        targets.iter().map(|target| target.name.clone()).collect()
    }

    #[test]
    fn a_diamond_reports_all_dependents_and_dependencies() {
        // App -> UI -> Core, App -> Net -> Core
        let graph = TargetGraph::new(&graph(&[
            ("App", "UI"),
            ("App", "Net"),
            ("UI", "Core"),
            ("Net", "Core"),
        ]));
        assert_eq!(names(graph.dependents(&node("Core"))), ["App", "Net", "UI"]);
        assert_eq!(
            names(graph.dependencies(&node("App"))),
            ["Core", "Net", "UI"]
        );
        assert!(graph.dependents(&node("App")).is_empty());
    }

    #[test]
    fn a_cycle_terminates_and_excludes_the_seed() {
        let graph = TargetGraph::new(&graph(&[("A", "B"), ("B", "C"), ("C", "A")]));
        assert_eq!(names(graph.dependents(&node("A"))), ["B", "C"]);
        assert_eq!(names(graph.dependencies(&node("A"))), ["B", "C"]);
    }

    #[test]
    fn an_unknown_target_has_no_relatives() {
        let graph = TargetGraph::new(&graph(&[("A", "B")]));
        assert!(graph.dependents(&node("Nope")).is_empty());
        assert!(graph.dependencies(&node("Nope")).is_empty());
    }

    #[test]
    fn an_empty_graph_answers_without_panicking() {
        let graph = TargetGraph::new(&TargetGraphSummary::default());
        assert!(graph.is_empty());
        assert_eq!(graph.len(), 0);
        assert!(graph.roots().is_empty());
        assert!(graph.cycles().is_empty());
        assert!(graph.shortest_path_from_root(&node("A")).is_none());
    }

    /// Two packages may each define a `Utils`. Keying by name alone merged
    /// them, so a dependency on one reported as a dependency on both.
    #[test]
    fn same_named_targets_in_different_projects_stay_distinct() {
        let summary = TargetGraphSummary {
            targets: vec![
                in_project("Utils", "P1"),
                in_project("Utils", "P2"),
                node("App"),
            ],
            dependencies: vec![TargetDependency {
                from: node("App"),
                to: in_project("Utils", "P1"),
            }],
            ..Default::default()
        };
        let graph = TargetGraph::new(&summary);
        assert_eq!(graph.len(), 3);
        // Only the P1 Utils is depended upon.
        assert_eq!(names(graph.dependents(&in_project("Utils", "P1"))), ["App"]);
        assert!(graph.dependents(&in_project("Utils", "P2")).is_empty());
    }

    #[test]
    fn an_ambiguous_name_does_not_resolve_to_an_arbitrary_target() {
        let summary = TargetGraphSummary {
            targets: vec![in_project("Utils", "P1"), in_project("Utils", "P2")],
            ..Default::default()
        };
        let graph = TargetGraph::new(&summary);
        assert!(
            graph.find("Utils").is_none(),
            "ambiguous name must not resolve"
        );
        assert_eq!(graph.find_all("Utils").len(), 2);
    }

    #[test]
    fn an_unambiguous_name_resolves() {
        let graph = TargetGraph::new(&graph(&[("App", "Core")]));
        assert_eq!(graph.find("Core"), Some(&node("Core")));
        assert!(graph.find("Missing").is_none());
    }

    /// The headline bug: the old implementation returned the first root it
    /// happened to reach, which could be far longer than the best one.
    #[test]
    fn shortest_path_is_actually_the_shortest() {
        // Deep -> M1 -> M2 -> Core  (4 nodes) vs  Direct -> Core  (2 nodes)
        let graph = TargetGraph::new(&summary(
            &["Deep", "M1", "M2", "Direct", "Core"],
            &[
                ("Deep", "M1"),
                ("M1", "M2"),
                ("M2", "Core"),
                ("Direct", "Core"),
            ],
        ));
        let path = graph.shortest_path_from_root(&node("Core")).unwrap();
        assert_eq!(names(path), ["Direct", "Core"]);
    }

    /// The answer must not depend on the order targets were declared in.
    #[test]
    fn shortest_path_does_not_depend_on_declaration_order() {
        let edges = [("App", "Core"), ("Tool", "Core")];
        let orders = [
            ["App", "Tool", "Core"],
            ["Tool", "App", "Core"],
            ["Core", "Tool", "App"],
        ];
        let paths: Vec<Vec<String>> = orders
            .iter()
            .map(|order| {
                let graph = TargetGraph::new(&summary(order, &edges));
                names(graph.shortest_path_from_root(&node("Core")).unwrap())
            })
            .collect();
        assert_eq!(paths[0], paths[1], "declaration order changed the answer");
        assert_eq!(paths[1], paths[2], "declaration order changed the answer");
        assert_eq!(paths[0].last().unwrap(), "Core");
    }

    /// Not just the endpoints: when two equal-length paths exist through
    /// different intermediates, the *body* of the path must be stable too.
    /// Sorting only the root tie-break left this varying, because the
    /// breadth-first search recorded whichever parent it happened to reach
    /// first, and neighbour order follows declaration order.
    #[test]
    fn an_equal_length_tie_picks_the_same_intermediates_every_time() {
        let edges = [
            ("Root", "M1"),
            ("Root", "M2"),
            ("M1", "Core"),
            ("M2", "Core"),
        ];
        let orders = [
            ["Root", "M1", "M2", "Core"],
            ["Root", "M2", "M1", "Core"],
            ["Core", "M2", "M1", "Root"],
            ["M2", "Core", "Root", "M1"],
        ];
        for order in orders {
            let graph = TargetGraph::new(&summary(&order, &edges));
            let path = graph.shortest_path_from_root(&node("Core")).unwrap();
            assert_eq!(
                names(path),
                ["Root", "M1", "Core"],
                "declaration order {order:?} changed the path body"
            );
        }
    }

    /// A root reaches itself trivially, so the old version returned a
    /// one-element "path" that read like a real chain.
    #[test]
    fn a_root_has_no_path_to_itself() {
        let graph = TargetGraph::new(&graph(&[("App", "Core")]));
        assert!(graph.shortest_path_from_root(&node("App")).is_none());
    }

    #[test]
    fn an_isolated_target_has_no_path() {
        let graph = TargetGraph::new(&summary(&["A", "B", "Lonely"], &[("A", "B")]));
        assert!(graph.shortest_path_from_root(&node("Lonely")).is_none());
        assert!(graph.dependents(&node("Lonely")).is_empty());
    }

    #[test]
    fn a_target_reachable_only_through_a_cycle_has_no_root_path() {
        let graph = TargetGraph::new(&graph(&[("A", "B"), ("B", "A")]));
        assert!(graph.shortest_path_from_root(&node("A")).is_none());
    }

    #[test]
    fn a_long_chain_reports_the_whole_path_in_order() {
        let graph = TargetGraph::new(&graph(&[("App", "UI"), ("UI", "Mid"), ("Mid", "Core")]));
        let path = graph.shortest_path_from_root(&node("Core")).unwrap();
        assert_eq!(names(path), ["App", "UI", "Mid", "Core"]);
    }

    #[test]
    fn roots_are_the_targets_nothing_depends_on() {
        let graph = TargetGraph::new(&graph(&[("App", "Core"), ("Tool", "Core")]));
        assert_eq!(names(graph.roots()), ["App", "Tool"]);
    }

    /// Edges pointing at undeclared targets are dropped, but the graph says so
    /// rather than presenting itself as complete.
    #[test]
    fn an_edge_naming_an_undeclared_target_is_reported() {
        let mut summary = graph(&[("A", "B")]);
        summary.dependencies.push(TargetDependency {
            from: node("Ghost"),
            to: node("B"),
        });
        let graph = TargetGraph::new(&summary);
        assert!(!graph.is_complete());
        assert_eq!(graph.dangling_edges().len(), 1);
        assert_eq!(graph.dangling_edges()[0].0.name, "Ghost");
        // The dropped edge does not leak into results.
        assert_eq!(names(graph.dependents(&node("B"))), ["A"]);
    }

    #[test]
    fn a_graph_with_every_edge_declared_is_complete() {
        let graph = TargetGraph::new(&graph(&[("A", "B")]));
        assert!(graph.is_complete());
        assert!(graph.dangling_edges().is_empty());
    }

    /// A self-edge is the smallest possible cycle and always a modelling
    /// error, so it is surfaced rather than silently ignored.
    #[test]
    fn a_self_dependency_is_reported_and_not_traversed() {
        let summary = summary(&["A", "B"], &[("A", "A"), ("A", "B")]);
        let graph = TargetGraph::new(&summary);
        assert_eq!(graph.self_dependencies(), [node("A")]);
        // It does not make A its own dependency.
        assert_eq!(names(graph.dependencies(&node("A"))), ["B"]);
        assert!(graph.dependents(&node("A")).is_empty());
    }

    /// Parallel edges would double-count fan-in and yield a neighbour twice.
    #[test]
    fn duplicate_edges_are_collapsed() {
        let graph = TargetGraph::new(&graph(&[("A", "B"), ("A", "B"), ("A", "B")]));
        assert_eq!(names(graph.dependents(&node("B"))), ["A"]);
        assert_eq!(graph.graph.edge_count(), 1);
    }

    #[test]
    fn cycles_are_reported_with_their_members() {
        let graph = TargetGraph::new(&graph(&[("A", "B"), ("B", "C"), ("C", "A"), ("Solo", "A")]));
        let cycles = graph.cycles();
        assert_eq!(cycles.len(), 1);
        assert_eq!(names(cycles[0].clone()), ["A", "B", "C"]);
    }

    #[test]
    fn an_acyclic_graph_reports_no_cycles() {
        let graph = TargetGraph::new(&graph(&[("App", "UI"), ("UI", "Core")]));
        assert!(graph.cycles().is_empty());
    }

    /// Building once and querying many times is the reason this is a struct.
    #[test]
    fn one_build_serves_many_queries() {
        let names: Vec<String> = (0..64).map(|index| format!("T{index}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let owned: Vec<(String, String)> = (0..63)
            .map(|index| (format!("T{index}"), format!("T{}", index + 1)))
            .collect();
        let edges: Vec<(&str, &str)> = owned
            .iter()
            .map(|(from, to)| (from.as_str(), to.as_str()))
            .collect();

        let graph = TargetGraph::new(&summary(&refs, &edges));
        // The last target is reachable from the single root through the chain.
        let path = graph.shortest_path_from_root(&node("T63")).unwrap();
        assert_eq!(path.len(), 64);
        // Every earlier target is a dependent of the last one.
        assert_eq!(graph.dependents(&node("T63")).len(), 63);
        assert_eq!(graph.dependencies(&node("T0")).len(), 63);
    }

    #[test]
    fn find_all_is_sorted_and_empty_for_unknown_names() {
        let summary = TargetGraphSummary {
            targets: vec![in_project("Utils", "P2"), in_project("Utils", "P1")],
            ..Default::default()
        };
        let graph = TargetGraph::new(&summary);
        let found: Vec<String> = graph
            .find_all("Utils")
            .iter()
            .map(|target| target.project.clone())
            .collect();
        assert_eq!(found, ["P1", "P2"]);
        assert!(graph.find_all("Nope").is_empty());
    }

    #[test]
    fn a_target_declared_twice_is_one_node() {
        let summary = TargetGraphSummary {
            targets: vec![node("A"), node("A"), node("B")],
            dependencies: vec![TargetDependency {
                from: node("A"),
                to: node("B"),
            }],
            ..Default::default()
        };
        let graph = TargetGraph::new(&summary);
        assert_eq!(graph.len(), 2);
        assert_eq!(names(graph.dependents(&node("B"))), ["A"]);
    }

    /// A dependent nearer the target must win even when a longer branch is
    /// explored first.
    #[test]
    fn the_nearest_root_wins_across_branches_of_different_depths() {
        let graph = TargetGraph::new(&summary(
            &["Far", "F1", "F2", "F3", "Near", "Shared"],
            &[
                ("Far", "F1"),
                ("F1", "F2"),
                ("F2", "F3"),
                ("F3", "Shared"),
                ("Near", "Shared"),
            ],
        ));
        let path = graph.shortest_path_from_root(&node("Shared")).unwrap();
        assert_eq!(names(path), ["Near", "Shared"]);
    }
}
