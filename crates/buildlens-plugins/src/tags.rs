use crate::{MetricsPlugin, PluginContext, PluginError};
use std::collections::BTreeMap;

/// Longest accepted key and value. Tags are labels for slicing history, not a
/// place to park payloads; a cap keeps one mistyped `--tag` from bloating every
/// row of `build_metadata`.
pub const MAX_TAG_KEY: usize = 64;
pub const MAX_TAG_VALUE: usize = 256;

/// User-supplied `--tag key=value` pairs, stored as `tag.<key>`.
pub struct TagPlugin;

impl MetricsPlugin for TagPlugin {
    fn name(&self) -> &'static str {
        "tags"
    }

    fn collect(
        &self,
        context: &PluginContext<'_>,
    ) -> Result<BTreeMap<String, String>, PluginError> {
        Ok(context
            .tags
            .iter()
            .map(|(key, value)| (format!("tag.{key}"), value.clone()))
            .collect())
    }
}

/// Parses one `--tag key=value` argument.
///
/// Keys are restricted so a tag can be addressed as a dashboard filter and
/// cannot collide with a built-in metadata namespace like `hw.` or `ci.`.
/// Values are free text — `run_plugins` redacts them like every other plugin
/// value, so a value carrying a home path or an email is scrubbed on the way
/// out rather than rejected here.
pub fn parse_tag(argument: &str) -> Result<(String, String), String> {
    let Some((key, value)) = argument.split_once('=') else {
        return Err(format!("expected key=value, got '{argument}'"));
    };
    let (key, value) = (key.trim(), value.trim());
    if key.is_empty() {
        return Err(format!("tag key is empty in '{argument}'"));
    }
    if value.is_empty() {
        return Err(format!("tag '{key}' has an empty value"));
    }
    if key.len() > MAX_TAG_KEY {
        return Err(format!("tag key '{key}' exceeds {MAX_TAG_KEY} characters"));
    }
    if value.len() > MAX_TAG_VALUE {
        return Err(format!(
            "value for tag '{key}' exceeds {MAX_TAG_VALUE} characters"
        ));
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(format!(
            "tag key '{key}' may only contain letters, digits, '_' and '-'"
        ));
    }
    Ok((key.to_owned(), value.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FakeProbe, run_plugins};
    use buildlens_core::BuildMetadata;
    use std::path::Path;

    #[test]
    fn parses_a_well_formed_pair() {
        assert_eq!(
            parse_tag("env=staging"),
            Ok(("env".into(), "staging".into()))
        );
        assert_eq!(
            parse_tag("  runner = m2-pro  "),
            Ok(("runner".into(), "m2-pro".into()))
        );
    }

    /// Values may contain '=' — a tag like `branch=feature/a=b` should keep
    /// everything after the first separator.
    #[test]
    fn only_the_first_separator_splits() {
        assert_eq!(parse_tag("query=a=b"), Ok(("query".into(), "a=b".into())));
    }

    #[test]
    fn rejects_malformed_tags() {
        for bad in ["novalue", "=orphan", "empty=", "bad key=x", "dot.ted=x"] {
            assert!(parse_tag(bad).is_err(), "expected '{bad}' to be rejected");
        }
        assert!(parse_tag(&format!("{}=x", "k".repeat(MAX_TAG_KEY + 1))).is_err());
        assert!(parse_tag(&format!("k={}", "v".repeat(MAX_TAG_VALUE + 1))).is_err());
    }

    /// Tag keys cannot shadow a built-in namespace, because they are stored
    /// under a `tag.` prefix of their own.
    #[test]
    fn tags_are_namespaced_and_redacted() {
        let probe = FakeProbe(Default::default());
        let build = BuildMetadata::default();
        let tags = BTreeMap::from([
            ("env".to_owned(), "staging".to_owned()),
            ("who".to_owned(), "dev@company.example".to_owned()),
        ]);
        let context = PluginContext {
            repo_root: Path::new("."),
            build_start_unix: None,
            build: &build,
            environment: None,
            user_metadata_path: None,
            tags: &tags,
            probe: &probe,
        };
        let collected = run_plugins(&[&TagPlugin], &context);
        assert_eq!(collected.entries["tag.env"], "staging");
        assert!(!collected.entries["tag.who"].contains("dev@company.example"));
        assert!(collected.entries.keys().all(|key| key.starts_with("tag.")));
    }
}
