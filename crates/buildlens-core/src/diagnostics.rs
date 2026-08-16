//! Compiler, linker and toolchain diagnostics, aggregated by fingerprint so a
//! warning emitted a thousand times is reported once with a count.

use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Note,
    Warning,
    Error,
    Fatal,
}

impl DiagnosticSeverity {
    /// Stable string used in fingerprints and storage keys; must match the
    /// serde renaming. See [`DiagnosticCategory::as_str`] for why this is not
    /// `{:?}`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Warning => "warning",
            Self::Error => "error",
            Self::Fatal => "fatal",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
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

impl DiagnosticCategory {
    /// Stable string used in fingerprints and as a `Swift6Summary` key.
    ///
    /// Deliberately not `{:?}`: `Debug` output carries no stability guarantee,
    /// and these values are persisted and compared across runs. Renaming a
    /// variant would silently invalidate every stored fingerprint. Matches the
    /// serde renaming, so the wire and the fingerprint agree.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SwiftCompiler => "swift_compiler",
            Self::SwiftConcurrency => "swift_concurrency",
            Self::SwiftSendable => "swift_sendable",
            Self::SwiftActorIsolation => "swift_actor_isolation",
            Self::Deprecation => "deprecation",
            Self::Linker => "linker",
            Self::CodeSigning => "code_signing",
            Self::Spm => "spm",
            Self::Simulator => "simulator",
            Self::XCTest => "x_c_test",
            Self::MemoryLifecycle => "memory_lifecycle",
            Self::Crash => "crash",
            Self::BuildConfiguration => "build_configuration",
            Self::AppIntents => "app_intents",
            Self::Unknown => "unknown",
        }
    }

    /// True for the categories that block a Swift 6 language-mode migration.
    pub fn is_swift6_blocker(&self) -> bool {
        matches!(
            self,
            Self::SwiftConcurrency | Self::SwiftSendable | Self::SwiftActorIsolation
        )
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `as_str` is the fingerprint/storage spelling and serde is the wire
    /// spelling; they must not drift apart. `XCTest` is the one that would
    /// catch a well-meant "tidy-up" — serde renders it `x_c_test`, so `as_str`
    /// has to say that too, however odd it reads.
    #[test]
    fn category_as_str_matches_the_serde_spelling() {
        let all = [
            DiagnosticCategory::SwiftCompiler,
            DiagnosticCategory::SwiftConcurrency,
            DiagnosticCategory::SwiftSendable,
            DiagnosticCategory::SwiftActorIsolation,
            DiagnosticCategory::Deprecation,
            DiagnosticCategory::Linker,
            DiagnosticCategory::CodeSigning,
            DiagnosticCategory::Spm,
            DiagnosticCategory::Simulator,
            DiagnosticCategory::XCTest,
            DiagnosticCategory::MemoryLifecycle,
            DiagnosticCategory::Crash,
            DiagnosticCategory::BuildConfiguration,
            DiagnosticCategory::AppIntents,
            DiagnosticCategory::Unknown,
        ];
        for category in all {
            let json = serde_json::to_string(&category).unwrap();
            assert_eq!(
                json.trim_matches('"'),
                category.as_str(),
                "serde and as_str disagree for {category:?}"
            );
        }
    }

    #[test]
    fn severity_as_str_matches_the_serde_spelling() {
        for severity in [
            DiagnosticSeverity::Note,
            DiagnosticSeverity::Warning,
            DiagnosticSeverity::Error,
            DiagnosticSeverity::Fatal,
        ] {
            let json = serde_json::to_string(&severity).unwrap();
            assert_eq!(json.trim_matches('"'), severity.as_str());
        }
    }

    #[test]
    fn only_the_concurrency_categories_block_swift6() {
        assert!(DiagnosticCategory::SwiftSendable.is_swift6_blocker());
        assert!(DiagnosticCategory::SwiftActorIsolation.is_swift6_blocker());
        assert!(DiagnosticCategory::SwiftConcurrency.is_swift6_blocker());
        assert!(!DiagnosticCategory::Deprecation.is_swift6_blocker());
        assert!(!DiagnosticCategory::Linker.is_swift6_blocker());
        assert!(!DiagnosticCategory::Unknown.is_swift6_blocker());
    }
}
