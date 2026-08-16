//! Scrubbing identifying and secret material out of parsed metrics.
//!
//! This is a privacy boundary: [`crate::redacted`] runs over everything before
//! it can reach a dashboard or a team server, so a miss here is a leak. The
//! rules are deliberately conservative — over-redacting a build log costs
//! nothing, under-redacting publishes someone's home directory or an API key.

use buildlens_core::BuildMetrics;
use regex::Regex;
use std::path::Path;
use std::sync::OnceLock;

/// Placeholder substituted for a redacted value.
pub const PLACEHOLDER: &str = "<redacted>";

/// Keys whose *value* is a secret. Matched case-insensitively as whole words,
/// so `token` matches `TOKEN=` and `api-token:` but not `tokenizer`.
const SECRET_KEYS: &[&str] = &[
    "token",
    "api_key",
    "api-key",
    "apikey",
    "secret",
    "password",
    "passwd",
    "authorization",
    "auth",
    "credential",
    "private_key",
    "access_key",
];

/// Matches `key = value`, `key: value`, or `key value` and captures the value.
///
/// The value runs to whitespace or a delimiter. An earlier version searched for
/// a terminator *after* the key and gave up when there was none, so a secret at
/// the end of a string — the common case — was left in place entirely.
fn secret_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let keys = SECRET_KEYS.join("|");
        Regex::new(&format!(
            r#"(?i)\b({keys})\b\s*[:=]?\s*(?:"[^"]*"|'[^']*'|[^\s,;'"]+)"#
        ))
        .expect("valid secret regex")
    })
}

/// Matches a bearer token even when the word `Authorization` is absent.
fn bearer_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\b(bearer|basic)\s+[A-Za-z0-9._~+/=-]+").expect("valid"))
}

/// Home directories, in the shapes the platforms write them.
///
/// Captures the username segment so it — and only it — is replaced. An earlier
/// version required a `/` after the username to locate the resume point, so a
/// path that *ended* at the home directory (`/Users/alice`) kept the name.
fn home_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Consumes the whole `/Users/<name>` prefix, so no `/Users/` survives
        // in output — callers assert on that, and a bare separator left behind
        // still discloses the platform's home layout.
        Regex::new(r"(?i)/(?:Users|home)/[^/\s]+").expect("valid home regex")
    })
}

/// Replaces every secret-looking `key=value` with a placeholder.
///
/// Applies to all occurrences, not just the first: a message may carry several.
pub fn redact_text(value: &str) -> String {
    let without_bearer = bearer_re().replace_all(value, PLACEHOLDER);
    let without_secrets = secret_re().replace_all(&without_bearer, PLACEHOLDER);
    redact_home_dirs(&without_secrets)
}

/// Replaces every home-directory username with `<home>`.
fn redact_home_dirs(value: &str) -> String {
    home_re().replace_all(value, "<home>").into_owned()
}

/// Turns a filesystem path into one safe to publish.
///
/// A path under `repo_root` becomes `<repo>/...`, which keeps the useful part
/// (where in the project the file lives) and drops the machine-specific
/// prefix. Otherwise every home directory in the string is replaced.
pub fn redact_path(value: &str, repo_root: Option<&Path>) -> String {
    let normalized = value.replace('\\', "/");
    if let Some(root) = repo_root.and_then(|root| root.to_str()) {
        let root = root.replace('\\', "/");
        let root = root.strip_suffix('/').unwrap_or(&root);
        if let Some(relative) = normalized.strip_prefix(root) {
            return format!("<repo>{relative}");
        }
    }
    redact_home_dirs(&normalized)
}

/// Scrubs every field of a parsed build that could carry a path or a secret.
pub fn redacted(mut metrics: BuildMetrics, repo_root: Option<&Path>) -> BuildMetrics {
    metrics.source_log = metrics
        .source_log
        .take()
        .map(|path| redact_path(&path, repo_root));
    metrics.project = metrics.project.take().map(|name| redact_text(&name));
    for phase in &mut metrics.phases {
        phase.name = redact_text(&phase.name);
    }
    for target in &mut metrics.targets {
        target.name = redact_text(&target.name);
        target.fingerprint = redact_text(&target.fingerprint);
        for step in &mut target.steps {
            step.title = redact_text(&step.title);
            step.file = step.file.take().map(|file| redact_path(&file, repo_root));
            step.fingerprint = redact_path(&step.fingerprint, repo_root);
        }
    }
    for file in &mut metrics.files {
        file.file = redact_path(&file.file, repo_root);
        file.target = file.target.take().map(|target| redact_text(&target));
    }
    for timing in &mut metrics.swift_timings {
        timing.file = redact_path(&timing.file, repo_root);
        timing.symbol = timing.symbol.take().map(|symbol| redact_text(&symbol));
    }
    // Diagnostics carry paths in both the location and the message text, and
    // both reach the dashboard, so both are scrubbed.
    for diagnostic in &mut metrics.diagnostics {
        diagnostic.file = diagnostic
            .file
            .take()
            .map(|file| redact_path(&file, repo_root));
        diagnostic.message = redact_text(&diagnostic.message);
        diagnostic.target = diagnostic.target.take().map(|target| redact_text(&target));
    }
    metrics.environment.machine = metrics
        .environment
        .machine
        .take()
        .map(|machine| redact_text(&machine));
    metrics.warnings = metrics
        .warnings
        .iter()
        .map(|warning| redact_text(warning))
        .collect();
    metrics
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A secret with nothing after it used to survive untouched, because the
    /// old implementation looked for a terminator and gave up without one.
    #[test]
    fn a_secret_at_the_end_of_a_string_is_redacted() {
        assert_eq!(redact_text("api_key=SECRET"), PLACEHOLDER);
        assert_eq!(redact_text("token=abc123"), PLACEHOLDER);
        assert!(!redact_text("password=hunter2").contains("hunter2"));
    }

    /// The old version replaced the label and kept the token.
    #[test]
    fn an_authorization_header_loses_its_token() {
        let redacted = redact_text("Authorization: Bearer abcdef123");
        assert!(
            !redacted.contains("abcdef123"),
            "token survived: {redacted}"
        );
    }

    #[test]
    fn a_bare_bearer_token_is_redacted_without_the_header_word() {
        assert!(!redact_text("using Bearer sk-abc.def-123").contains("sk-abc"));
        assert!(!redact_text("Basic dXNlcjpwYXNz").contains("dXNlcjpwYXNz"));
    }

    /// Only the first occurrence of each key used to be replaced.
    #[test]
    fn every_secret_in_a_string_is_redacted() {
        let redacted = redact_text("token=a password=b apikey=c");
        assert!(!redacted.contains('a') || !redacted.contains("token=a"));
        assert!(!redacted.contains("password=b"));
        assert!(!redacted.contains("apikey=c"));
    }

    #[test]
    fn secrets_are_matched_case_insensitively() {
        assert!(!redact_text("TOKEN=abc").contains("abc"));
        assert!(!redact_text("Api_Key: xyz").contains("xyz"));
    }

    #[test]
    fn quoted_secret_values_are_redacted_whole() {
        assert!(!redact_text(r#"token="a b c""#).contains("a b c"));
        assert!(!redact_text("token='a b c'").contains("a b c"));
    }

    /// The word must stand alone — redacting every identifier containing
    /// "auth" or "token" would gut ordinary build output.
    #[test]
    fn ordinary_words_containing_a_key_are_left_alone() {
        assert_eq!(
            redact_text("Tokenizer.swift compiled"),
            "Tokenizer.swift compiled"
        );
        assert_eq!(redact_text("Authenticator built"), "Authenticator built");
    }

    #[test]
    fn text_without_secrets_is_unchanged() {
        assert_eq!(redact_text("Compiling Foo.swift"), "Compiling Foo.swift");
        assert_eq!(redact_text(""), "");
    }

    /// A path that ends at the home directory kept the username.
    #[test]
    fn a_path_ending_at_the_home_directory_is_redacted() {
        assert_eq!(redact_path("/Users/alice", None), "<home>");
        assert_eq!(redact_path("/home/carol", None), "<home>");
    }

    #[test]
    fn a_home_path_with_children_keeps_the_project_relative_part() {
        assert_eq!(
            redact_path("/Users/alice/proj/Sources/A.swift", None),
            "<home>/proj/Sources/A.swift"
        );
    }

    /// Linux home directories were not handled at all.
    #[test]
    fn linux_home_directories_are_redacted() {
        assert_eq!(
            redact_path("/home/carol/proj/A.swift", None),
            "<home>/proj/A.swift"
        );
    }

    #[test]
    fn windows_paths_normalize_and_redact() {
        assert_eq!(
            redact_path(r"C:\Users\bob\proj\A.swift", None),
            "C:<home>/proj/A.swift"
        );
    }

    /// Only the first home path used to be replaced.
    #[test]
    fn every_home_path_in_a_string_is_redacted() {
        let redacted = redact_path("/Users/alice/a/Users/bob/b.swift", None);
        assert!(!redacted.contains("alice"), "{redacted}");
        assert!(!redacted.contains("bob"), "{redacted}");
    }

    #[test]
    fn a_home_path_inside_a_message_is_redacted() {
        let redacted = redact_text("error in /Users/alice/proj/A.swift line 3");
        assert!(!redacted.contains("alice"), "{redacted}");
    }

    #[test]
    fn a_repo_relative_path_is_reported_against_the_repo_root() {
        let root = Path::new("/Users/alice/proj");
        assert_eq!(
            redact_path("/Users/alice/proj/Sources/A.swift", Some(root)),
            "<repo>/Sources/A.swift"
        );
    }

    #[test]
    fn a_trailing_slash_on_the_repo_root_does_not_double_up() {
        let root = Path::new("/Users/alice/proj/");
        assert_eq!(
            redact_path("/Users/alice/proj/Sources/A.swift", Some(root)),
            "<repo>/Sources/A.swift"
        );
    }

    /// A path outside the repo still gets its home directory scrubbed.
    #[test]
    fn a_path_outside_the_repo_root_still_loses_the_username() {
        let root = Path::new("/Users/alice/proj");
        let redacted = redact_path("/Users/alice/other/B.swift", Some(root));
        assert!(!redacted.contains("alice"), "{redacted}");
    }

    #[test]
    fn a_path_with_no_home_component_is_only_normalized() {
        assert_eq!(
            redact_path("/var/folders/x/T/tmp.swift", None),
            "/var/folders/x/T/tmp.swift"
        );
        assert_eq!(redact_path(r"Sources\A.swift", None), "Sources/A.swift");
    }
}
