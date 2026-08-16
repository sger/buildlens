//! The target dependency graph, and the shapes derived from it.

use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TargetNode {
    pub name: String,
    pub project: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TargetDependency {
    pub from: TargetNode,
    pub to: TargetNode,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct TargetGraphSummary {
    pub declared_count: Option<usize>,
    pub targets: Vec<TargetNode>,
    pub dependencies: Vec<TargetDependency>,
    pub fan_in: BTreeMap<String, usize>,
    pub fan_out: BTreeMap<String, usize>,
    pub reverse_dependencies: BTreeMap<String, Vec<String>>,
    pub hotspots: Vec<String>,
    pub cycles: Vec<Vec<String>>,
}
