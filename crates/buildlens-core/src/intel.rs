//! Derived findings: which change hit which target, where the critical path
//! bottlenecks, and the evidence backing each claim.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct TargetImpact {
    pub changed_file: String,
    pub owning_target: String,
    pub downstream_targets: Vec<String>,
    pub match_kind: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Bottleneck {
    pub target: String,
    pub seconds: f64,
    pub fan_in: usize,
    pub blocked_downstream: usize,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct EvidenceLink {
    pub kind: String,
    pub weight: i32,
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EvidenceChain {
    pub subject: String,
    pub links: Vec<EvidenceLink>,
    pub confidence: u8,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Intelligence {
    pub impacts: Vec<TargetImpact>,
    pub bottlenecks: Vec<Bottleneck>,
    pub chains: Vec<EvidenceChain>,
}
