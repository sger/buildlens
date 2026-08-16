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

#[derive(Debug, Clone, Serialize)]
pub struct GitCorrelation {
    pub base: String,
    pub head: String,
    pub changed_files: Vec<String>,
    pub likely_related: String,
    pub confidence: u8,
    pub evidence: Vec<String>,
    pub failure_ownership: Vec<GitOwnership>,
    pub implementation_ownership: Vec<GitOwnership>,
    pub diagnostic_ownership: Vec<GitOwnership>,
}
