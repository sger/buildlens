//! Scrubbing plugin values before they leave the crate.
//!
//! Secrets and home directories are handled by
//! [`buildlens_metrics::redact_text`], the same implementation that scrubs
//! parsed build metrics — one set of rules rather than a weaker second copy.
//! What this module adds is *pseudonymization*: a git author's email becomes a
//! stable `user-<hash>`, so recurring per-developer patterns stay visible
//! without naming anyone.

use regex::Regex;
use std::sync::OnceLock;

/// Deterministic, non-reversible pseudonym for an email address.
///
/// Stable across runs and machines so the same person is the same pseudonym
/// everywhere, which is what makes "this failure keeps happening to one
/// developer" answerable without identifying them.
pub fn pseudonymize_email(email: &str) -> String {
    format!(
        "user-{:016x}",
        fnv1a64(email.trim().to_lowercase().as_bytes())
    )
}

/// Replaces any email address inside a value with its pseudonym.
pub fn redact_emails(value: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}").expect("valid regex")
    });
    re.replace_all(value, |captures: &regex::Captures<'_>| {
        pseudonymize_email(&captures[0])
    })
    .into_owned()
}

/// Replaces home-directory paths with a fixed marker.
///
/// Delegates so macOS, Linux and Windows homes are all covered; the previous
/// local implementation matched only `/Users/`, which missed every Linux CI
/// runner — the environment this crate exists to describe.
pub fn scrub_home_paths(value: &str) -> String {
    buildlens_metrics::redact_text(value)
}

/// Applied to every plugin value, and to every user-supplied key, before it
/// leaves the crate.
///
/// Order matters: emails are pseudonymized first, because the generic secret
/// scrubber would otherwise replace an `author: name@host` value wholesale and
/// lose the stable pseudonym that makes per-developer patterns visible.
pub fn redact_value(value: &str) -> String {
    buildlens_metrics::redact_text(&redact_emails(value))
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_pseudonyms_are_deterministic_and_masked() {
        let upper = pseudonymize_email("Jane.Doe@Example.com");
        let lower = pseudonymize_email("jane.doe@example.com");
        assert_eq!(upper, lower, "case must not change the pseudonym");
        assert!(upper.starts_with("user-"));
        assert!(!upper.contains("jane"));
    }

    #[test]
    fn different_people_get_different_pseudonyms() {
        assert_ne!(
            pseudonymize_email("a@example.com"),
            pseudonymize_email("b@example.com")
        );
    }

    #[test]
    fn emails_inside_values_are_replaced() {
        let redacted = redact_value("author <dev@company.example> committed");
        assert!(!redacted.contains("dev@company.example"));
        assert!(redacted.contains("user-"));
    }

    #[test]
    fn several_emails_in_one_value_are_all_replaced() {
        let redacted = redact_value("a@x.com and b@y.com");
        assert!(!redacted.contains("a@x.com"));
        assert!(!redacted.contains("b@y.com"));
    }

    /// `tags.rs` promises that plugin values are scrubbed on the way out, so a
    /// user passing `--tag deploy_key=...` must not have it stored verbatim.
    /// The previous implementation handled only emails and `/Users/`.
    #[test]
    fn secrets_in_values_are_redacted() {
        for value in [
            "token=abc123",
            "api_key=SECRET",
            "password=hunter2",
            "Authorization: Bearer xyz789",
        ] {
            let redacted = redact_value(value);
            assert!(
                redacted.contains("<redacted>"),
                "secret survived in {value:?}: {redacted}"
            );
        }
        assert!(!redact_value("token=abc123").contains("abc123"));
        assert!(!redact_value("Authorization: Bearer xyz789").contains("xyz789"));
    }

    #[test]
    fn macos_home_paths_are_scrubbed() {
        let redacted = redact_value("/Users/someone/Library/Logs");
        assert!(!redacted.contains("someone"), "{redacted}");
        assert!(redacted.contains("<home>"), "{redacted}");
    }

    /// CI runners are frequently Linux, and this crate exists to describe CI
    /// environments — the old `/Users/`-only pattern missed them entirely.
    #[test]
    fn linux_home_paths_are_scrubbed() {
        let redacted = redact_value("/home/runner/work/project");
        assert!(!redacted.contains("runner"), "{redacted}");
    }

    #[test]
    fn a_value_with_nothing_sensitive_is_unchanged() {
        assert_eq!(redact_value("arm64"), "arm64");
        assert_eq!(redact_value("16.2"), "16.2");
        assert_eq!(redact_value(""), "");
    }

    /// Both an email and a path in one value must each be handled.
    #[test]
    fn mixed_sensitive_content_is_fully_scrubbed() {
        let redacted = redact_value("dev@company.example at /Users/someone/repo");
        assert!(!redacted.contains("dev@company.example"), "{redacted}");
        assert!(!redacted.contains("someone"), "{redacted}");
        assert!(redacted.contains("user-"), "{redacted}");
    }
}
