//! Derived findings: which change hit which target, where the critical path
//! bottlenecks, and the evidence backing each claim.

use serde::Serialize;

/// How confidently a changed file was tied to the target that compiles it.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    /// The recorded compile path and the changed path are identical.
    Exact,
    /// One path is a suffix of the other on a path boundary — the usual case,
    /// since git reports repo-relative paths and metrics record absolute ones.
    Suffix,
    /// No per-file metric matched this changed file.
    None,
}

impl MatchKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Suffix => "suffix",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TargetImpact {
    pub changed_file: String,
    /// The target that compiles this file, when one is known.
    ///
    /// `None` rather than a `"unknown"` sentinel: a real target may legally be
    /// named `unknown`, and every consumer had to remember to compare against
    /// the magic string.
    pub owning_target: Option<String>,
    pub downstream_targets: Vec<String>,
    pub match_kind: MatchKind,
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
