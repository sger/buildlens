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

/// What kind of type-checking cost an [`Advice`] describes.
///
/// A closed enum rather than a string because advice is persisted and compared
/// across builds, the same reason diagnostic fingerprints are built from
/// `as_str` and never `{:?}`: a derived `Debug` is not a stable contract.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdviceKind {
    /// One expression is expensive to type-check on its own.
    ExpressionHotspot,
    /// A function body costs far more than the expressions inside it, so the
    /// cost is the body's size rather than any single expression.
    LargeFunctionBody,
    /// One file holds most of a target's type-checking time.
    ConcentratedFile,
    /// Type-checking accounts for a large share of a target's whole duration.
    TargetDominatedByTypeChecking,
}

impl AdviceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            AdviceKind::ExpressionHotspot => "expression_hotspot",
            AdviceKind::LargeFunctionBody => "large_function_body",
            AdviceKind::ConcentratedFile => "concentrated_file",
            AdviceKind::TargetDominatedByTypeChecking => "target_dominated_by_type_checking",
        }
    }
}

/// One place worth looking at, with the measurement that identified it.
///
/// `line`/`column` are zero when the compiler message carried no location.
/// Site-level advice always has one; file- and target-level advice does not.
#[derive(Debug, Clone, Serialize)]
pub struct Advice {
    pub kind: AdviceKind,
    pub file: String,
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub line: u32,
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub column: u32,
    pub symbol: Option<String>,
    pub target: Option<String>,
    /// The cost this advice is about: one site's time, or a summed share.
    pub milliseconds: f64,
    /// What the number means, in non-blaming wording. See
    /// `buildlens_intel::language`.
    pub explanation: String,
}

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Intelligence {
    pub impacts: Vec<TargetImpact>,
    pub bottlenecks: Vec<Bottleneck>,
    pub chains: Vec<EvidenceChain>,
    /// Empty unless the build was compiled with the `-warn-long-*` frontend
    /// flags, which is what makes Swift timings exist at all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub advice: Vec<Advice>,
}
