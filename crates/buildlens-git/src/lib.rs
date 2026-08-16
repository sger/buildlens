//! Correlates a build's failures with the commits that plausibly caused them.
//!
//! The crate answers two questions: does this commit range explain the
//! failures (a scored verdict), and who last touched the lines involved
//! (blame). Both are best-effort — a diagnostic may name a file that is not in
//! the repo, and blame may fail on an untracked path. Those cases yield no
//! attribution rather than a guessed one: naming the wrong person as the
//! author of a failure is worse than naming nobody.

pub mod blame;
pub mod paths;
pub mod scoring;

use buildlens_core::{BuildAnalysis, GitCorrelation, GitOwnership};
use std::path::{Path, PathBuf};
use thiserror::Error;

pub use scoring::{Signals, Verdict};

#[derive(Debug, Error)]
pub enum GitError {
    #[error("git could not be run: {0}")]
    Spawn(String),
    #[error("git command failed: {0}")]
    Command(String),
    #[error("git output was not valid UTF-8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("repository path is not valid UTF-8: {0}")]
    NonUtf8Path(PathBuf),
}

/// A source location a diagnostic or failure pointed at.
type Location = (String, Option<u32>);

/// Correlates the commit range `base...head` against a build's evidence.
pub fn correlate(
    repo: impl AsRef<Path>,
    base: &str,
    head: &str,
    analysis: &BuildAnalysis,
) -> Result<GitCorrelation, GitError> {
    let repo = repo.as_ref();
    let changed_files = blame::changed_files(repo, base, head)?;

    let diagnostic_locations = diagnostic_locations(analysis, &changed_files);
    let failure_locations: Vec<Location> = analysis
        .failure_clusters
        .iter()
        .filter_map(|cluster| paths::location_from_message(&cluster.message))
        .collect();

    let signals = Signals {
        diagnostic_file_changed: !diagnostic_locations.is_empty(),
        package_manifest_changed: changed_files
            .iter()
            .any(|file| paths::is_package_manifest(file)),
        deterministic_failures_with_changes: !analysis.failure_clusters.is_empty()
            && !changed_files.is_empty(),
        corroborated_crash: !analysis.crashes.is_empty() && !diagnostic_locations.is_empty(),
    };
    let verdict = scoring::weigh(signals);

    Ok(GitCorrelation {
        base: base.to_owned(),
        head: head.to_owned(),
        changed_files,
        likely_related: verdict.likely_related,
        confidence: verdict.confidence,
        evidence: verdict.evidence,
        failure_ownership: own(repo, failure_locations)?,
        implementation_ownership: Vec::new(),
        diagnostic_ownership: own(repo, diagnostic_locations)?,
    })
}

/// The locations of diagnostics whose file the commit range touched.
fn diagnostic_locations(analysis: &BuildAnalysis, changed_files: &[String]) -> Vec<Location> {
    analysis
        .diagnostics
        .diagnostics
        .iter()
        .filter_map(|diagnostic| {
            let file = diagnostic.example.file.as_ref()?;
            paths::matches_any(file, changed_files).then(|| (file.clone(), diagnostic.example.line))
        })
        .collect()
}

/// Blames each location, dropping the ones git cannot speak to.
fn own(repo: &Path, mut locations: Vec<Location>) -> Result<Vec<GitOwnership>, GitError> {
    locations.sort();
    locations.dedup();
    let mut owners = Vec::new();
    for (file, line) in locations {
        let Some(repo_file) = paths::resolve_repo_file(repo, &file) else {
            continue;
        };
        let Some(record) = blame::blame(repo, &repo_file, line)? else {
            continue;
        };
        owners.push(GitOwnership {
            file: repo_file,
            line,
            author: record.author,
            author_email: record.author_email,
            authored_at: record.authored_at,
            committed_at: record.committed_at,
            commit: record.commit,
            subject: record.subject,
        });
    }
    Ok(owners)
}

#[cfg(test)]
mod tests {
    use super::*;
    use buildlens_core::{
        DiagnosticAggregate, DiagnosticCategory, DiagnosticExample, DiagnosticSeverity,
        FailureCluster, LikelyRelated,
    };
    use std::process::Command;

    /// Builds a throwaway repository so correlation runs against real git
    /// rather than a mock, without depending on this repo's own history.
    fn repo_with_commit(dir: &Path, file: &str, contents: &str) {
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .expect("git must be available");
            assert!(status.status.success(), "git {args:?} failed");
        };
        std::fs::create_dir_all(dir).unwrap();
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test Person"]);
        let path = dir.join(file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, "line one\nline two\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "first"]);
        std::fs::write(&path, contents).unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "second"]);
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("buildlens-git-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn diagnostic(file: &str, line: u32) -> DiagnosticAggregate {
        DiagnosticAggregate {
            fingerprint: format!("f:{file}:{line}"),
            severity: DiagnosticSeverity::Error,
            category: DiagnosticCategory::Unknown,
            occurrences: 1,
            example: DiagnosticExample {
                file: Some(file.to_owned()),
                line: Some(line),
                column: None,
                message: "boom".to_owned(),
                target: None,
            },
        }
    }

    #[test]
    fn a_build_with_no_evidence_is_unrelated() {
        let dir = scratch("no-evidence");
        repo_with_commit(&dir, "Sources/App.swift", "line one\nchanged\n");
        let correlation = correlate(&dir, "HEAD~1", "HEAD", &BuildAnalysis::default()).unwrap();
        assert_eq!(correlation.likely_related, LikelyRelated::No);
        assert_eq!(correlation.confidence, 0);
        assert_eq!(correlation.changed_files, vec!["Sources/App.swift"]);
        assert!(correlation.failure_ownership.is_empty());
        assert!(correlation.diagnostic_ownership.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_diagnostic_in_a_changed_file_is_blamed_to_its_author() {
        let dir = scratch("diagnostic-blame");
        repo_with_commit(&dir, "Sources/App.swift", "line one\nchanged\n");
        let mut analysis = BuildAnalysis::default();
        analysis.diagnostics.diagnostics = vec![diagnostic("Sources/App.swift", 2)];

        let correlation = correlate(&dir, "HEAD~1", "HEAD", &analysis).unwrap();
        assert_eq!(correlation.likely_related, LikelyRelated::Yes);
        assert_eq!(correlation.confidence, 60);
        let owner = correlation
            .diagnostic_ownership
            .first()
            .expect("the changed line should have an owner");
        assert_eq!(owner.author, "Test Person");
        assert_eq!(owner.file, "Sources/App.swift");
        assert_eq!(owner.line, Some(2));
        assert_eq!(owner.subject, "second");
        // `git show` supplies a real ISO-8601 date rather than epoch seconds.
        assert!(
            owner
                .authored_at
                .as_deref()
                .is_some_and(|d| d.contains('T')),
            "expected ISO-8601, got {:?}",
            owner.authored_at
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A diagnostic pointing outside the repository must not be attributed to
    /// anyone — the file is not ours to blame.
    #[test]
    fn a_diagnostic_outside_the_repo_yields_no_owner() {
        let dir = scratch("outside-repo");
        repo_with_commit(&dir, "Sources/App.swift", "line one\nchanged\n");
        let mut analysis = BuildAnalysis::default();
        analysis.diagnostics.diagnostics = vec![diagnostic("/elsewhere/Other.swift", 1)];

        let correlation = correlate(&dir, "HEAD~1", "HEAD", &analysis).unwrap();
        assert!(correlation.diagnostic_ownership.is_empty());
        assert_eq!(correlation.likely_related, LikelyRelated::No);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The crate previously guessed a line by grepping for identifiers from
    /// one specific codebase, falling back to blaming line 1. A failure
    /// message we cannot resolve must produce no attribution at all.
    #[test]
    fn an_unresolvable_failure_blames_nobody() {
        let dir = scratch("unresolvable");
        repo_with_commit(&dir, "Sources/App.swift", "line one\nchanged\n");
        let analysis = BuildAnalysis {
            failure_clusters: vec![FailureCluster {
                fingerprint: "c1".to_owned(),
                category: "assertion".to_owned(),
                tests: vec!["testThing".to_owned()],
                message: "XCTAssertEqual failed at /nowhere/Ghost.swift:9".to_owned(),
            }],
            ..Default::default()
        };

        let correlation = correlate(&dir, "HEAD~1", "HEAD", &analysis).unwrap();
        assert!(
            correlation.failure_ownership.is_empty(),
            "an unresolvable location must not be attributed to anyone"
        );
        assert!(correlation.implementation_ownership.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_changed_package_manifest_is_recorded_as_evidence() {
        let dir = scratch("manifest");
        repo_with_commit(&dir, "Package.swift", "line one\nchanged\n");
        let correlation = correlate(&dir, "HEAD~1", "HEAD", &BuildAnalysis::default()).unwrap();
        assert_eq!(correlation.confidence, 20);
        assert!(
            correlation
                .evidence
                .iter()
                .any(|reason| reason.contains("Package.swift"))
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unknown_revision_is_an_error_not_an_empty_result() {
        let dir = scratch("bad-rev");
        repo_with_commit(&dir, "Sources/App.swift", "line one\nchanged\n");
        let result = correlate(&dir, "does-not-exist", "HEAD", &BuildAnalysis::default());
        assert!(matches!(result, Err(GitError::Command(_))));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn duplicate_diagnostics_are_blamed_once() {
        let dir = scratch("dedup");
        repo_with_commit(&dir, "Sources/App.swift", "line one\nchanged\n");
        let mut analysis = BuildAnalysis::default();
        analysis.diagnostics.diagnostics = vec![
            diagnostic("Sources/App.swift", 2),
            diagnostic("Sources/App.swift", 2),
        ];
        let correlation = correlate(&dir, "HEAD~1", "HEAD", &analysis).unwrap();
        assert_eq!(correlation.diagnostic_ownership.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
