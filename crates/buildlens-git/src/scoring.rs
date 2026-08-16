//! How confident we are that a commit range explains a build's failures.
//!
//! Pure scoring, separated from the git calls so the weights can be tested
//! and argued about directly.

use buildlens_core::LikelyRelated;

/// A diagnostic names a file the commit range touched. The strongest signal
/// available: the compiler pointed at a line, and that line changed.
pub const WEIGHT_DIAGNOSTIC_FILE_CHANGED: u16 = 60;
/// Dependency resolution changed, which can break a build with no source edit.
pub const WEIGHT_PACKAGE_MANIFEST_CHANGED: u16 = 20;
/// Reproducible test failures alongside source changes — suggestive, but the
/// failures are not tied to any particular change.
pub const WEIGHT_DETERMINISTIC_FAILURES: u16 = 10;
/// A crash, with a diagnostic already pointing into changed code.
pub const WEIGHT_CORROBORATED_CRASH: u16 = 10;

/// At or above this, the changes are reported as the likely cause. Set to the
/// diagnostic weight: a diagnostic in a changed file is on its own sufficient,
/// and no combination of weaker signals reaches it.
pub const THRESHOLD_LIKELY: u16 = WEIGHT_DIAGNOSTIC_FILE_CHANGED;
/// At or below this, no meaningful overlap was found. A single weak signal
/// does not clear it; two weak signals do, becoming `Uncertain`.
pub const THRESHOLD_UNRELATED: u16 = WEIGHT_PACKAGE_MANIFEST_CHANGED;

/// The signals gathered about one commit range.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Signals {
    pub diagnostic_file_changed: bool,
    pub package_manifest_changed: bool,
    pub deterministic_failures_with_changes: bool,
    pub corroborated_crash: bool,
}

/// The verdict, with the human-readable reasons behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub confidence: u8,
    pub likely_related: LikelyRelated,
    pub evidence: Vec<String>,
}

impl Signals {
    fn score(&self) -> u16 {
        let mut score = 0;
        if self.diagnostic_file_changed {
            score += WEIGHT_DIAGNOSTIC_FILE_CHANGED;
        }
        if self.package_manifest_changed {
            score += WEIGHT_PACKAGE_MANIFEST_CHANGED;
        }
        if self.deterministic_failures_with_changes {
            score += WEIGHT_DETERMINISTIC_FAILURES;
        }
        if self.corroborated_crash {
            score += WEIGHT_CORROBORATED_CRASH;
        }
        score
    }

    fn reasons(&self) -> Vec<String> {
        let mut evidence = Vec::new();
        if self.diagnostic_file_changed {
            evidence.push("a diagnostic's source file changed in this commit range".to_owned());
        }
        if self.package_manifest_changed {
            evidence.push("Package.swift or Package.resolved changed".to_owned());
        }
        if self.deterministic_failures_with_changes {
            evidence.push(
                "the build has reproducible test failures and the range changes source files"
                    .to_owned(),
            );
        }
        if self.corroborated_crash {
            evidence.push("a crash is corroborated by a diagnostic in changed code".to_owned());
        }
        evidence
    }
}

/// Scores the signals into a verdict.
pub fn weigh(signals: Signals) -> Verdict {
    let score = signals.score();
    let likely_related = if score >= THRESHOLD_LIKELY {
        LikelyRelated::Yes
    } else if score <= THRESHOLD_UNRELATED {
        LikelyRelated::No
    } else {
        LikelyRelated::Uncertain
    };
    let mut evidence = signals.reasons();
    if evidence.is_empty() {
        evidence.push("no overlap between changed files and the build's evidence".to_owned());
    }
    Verdict {
        // Saturates: the weights can sum past 100 and confidence is a percentage.
        confidence: score.min(100) as u8,
        likely_related,
        evidence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_signals_is_unrelated_with_a_stated_reason() {
        let verdict = weigh(Signals::default());
        assert_eq!(verdict.confidence, 0);
        assert_eq!(verdict.likely_related, LikelyRelated::No);
        // Never silently empty — a reader needs to know why the answer is no.
        assert_eq!(verdict.evidence.len(), 1);
        assert!(verdict.evidence[0].contains("no overlap"));
    }

    /// The rule the threshold encodes: a diagnostic in a changed file is on
    /// its own enough to report the range as the likely cause.
    #[test]
    fn a_changed_diagnostic_file_alone_is_likely_related() {
        let verdict = weigh(Signals {
            diagnostic_file_changed: true,
            ..Default::default()
        });
        assert_eq!(verdict.likely_related, LikelyRelated::Yes);
        assert_eq!(verdict.confidence, 60);
    }

    #[test]
    fn a_manifest_change_alone_is_not_enough() {
        let verdict = weigh(Signals {
            package_manifest_changed: true,
            ..Default::default()
        });
        assert_eq!(verdict.likely_related, LikelyRelated::No);
        assert_eq!(verdict.confidence, 20);
    }

    /// The band between the thresholds is reachable, and only by combining
    /// weak signals. If a weight changes and this collapses, the three-way
    /// verdict has quietly become two-way.
    #[test]
    fn two_weak_signals_are_uncertain() {
        let verdict = weigh(Signals {
            package_manifest_changed: true,
            deterministic_failures_with_changes: true,
            ..Default::default()
        });
        assert_eq!(verdict.likely_related, LikelyRelated::Uncertain);
        assert_eq!(verdict.confidence, 30);
    }

    #[test]
    fn a_single_weak_signal_stays_unrelated() {
        let verdict = weigh(Signals {
            deterministic_failures_with_changes: true,
            ..Default::default()
        });
        assert_eq!(verdict.likely_related, LikelyRelated::No);
        assert_eq!(verdict.confidence, 10);
    }

    #[test]
    fn confidence_never_exceeds_one_hundred() {
        let verdict = weigh(Signals {
            diagnostic_file_changed: true,
            package_manifest_changed: true,
            deterministic_failures_with_changes: true,
            corroborated_crash: true,
        });
        assert_eq!(verdict.confidence, 100);
        assert_eq!(verdict.likely_related, LikelyRelated::Yes);
        assert_eq!(verdict.evidence.len(), 4);
    }

    #[test]
    fn every_signal_contributes_a_distinct_reason() {
        let all = Signals {
            diagnostic_file_changed: true,
            package_manifest_changed: true,
            deterministic_failures_with_changes: true,
            corroborated_crash: true,
        };
        let evidence = weigh(all).evidence;
        let unique: std::collections::BTreeSet<_> = evidence.iter().collect();
        assert_eq!(unique.len(), evidence.len());
    }
}
