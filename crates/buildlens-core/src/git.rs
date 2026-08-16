//! Correlation between a build's failures and the commits that plausibly
//! caused them.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct GitOwnership {
    pub file: String,
    pub line: Option<u32>,
    pub author: String,
    pub author_email: Option<String>,
    pub authored_at: Option<String>,
    pub committed_at: Option<String>,
    pub commit: String,
    pub subject: String,
}

/// Whether the commit range plausibly explains the build's failures.
///
/// A closed set rather than a `String`: this is a verdict consumers branch on,
/// and "yes"/"no"/"uncertain" spelled as free text invites typos that compare
/// unequal forever.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LikelyRelated {
    /// Evidence directly ties a changed file to a failure.
    Yes,
    /// Some overlap, but nothing decisive.
    Uncertain,
    /// No overlap found between the changes and the build's evidence.
    No,
}

impl LikelyRelated {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::Uncertain => "uncertain",
            Self::No => "no",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GitCorrelation {
    pub base: String,
    pub head: String,
    pub changed_files: Vec<String>,
    pub likely_related: LikelyRelated,
    pub confidence: u8,
    pub evidence: Vec<String>,
    pub failure_ownership: Vec<GitOwnership>,
    pub implementation_ownership: Vec<GitOwnership>,
    pub diagnostic_ownership: Vec<GitOwnership>,
}
