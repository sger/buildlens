//! Compiler, linker and toolchain diagnostics, aggregated by fingerprint so a
//! warning emitted a thousand times is reported once with a count.

use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Note,
    Warning,
    Error,
    Fatal,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCategory {
    SwiftCompiler,
    SwiftConcurrency,
    SwiftSendable,
    SwiftActorIsolation,
    Deprecation,
    Linker,
    CodeSigning,
    Spm,
    Simulator,
    XCTest,
    MemoryLifecycle,
    Crash,
    BuildConfiguration,
    AppIntents,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticExample {
    pub file: Option<String>,
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub message: String,
    pub target: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticAggregate {
    pub fingerprint: String,
    pub severity: DiagnosticSeverity,
    pub category: DiagnosticCategory,
    pub occurrences: usize,
    pub example: DiagnosticExample,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Swift6Summary {
    pub unique_blockers: usize,
    pub by_category: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DiagnosticSummary {
    pub raw_warnings: usize,
    pub unique_warnings: usize,
    pub raw_errors: usize,
    pub unique_errors: usize,
    pub diagnostics: Vec<DiagnosticAggregate>,
    pub swift6: Swift6Summary,
}
