mod ci;
mod git;
mod hardware;
mod probe;
mod redact;
mod suspend;
mod tags;
mod thermal;
mod user_file;
mod xcode;

pub use ci::CiPlugin;
pub use git::GitContextPlugin;
pub use hardware::HardwarePlugin;
pub use probe::{FakeProbe, RealProbe, SystemProbe};
pub use redact::{pseudonymize_email, redact_value, scrub_home_paths};
pub use suspend::SuspendPlugin;
pub use tags::{MAX_TAG_KEY, MAX_TAG_VALUE, TagPlugin, parse_tag};
pub use thermal::ThermalPlugin;
pub use user_file::UserMetadataPlugin;
pub use xcode::XcodeEnvPlugin;

use buildlens_core::{BuildMetadata, CollectedMetadata, MetricsEnvironment};
use std::collections::BTreeMap;
use std::path::Path;
use thiserror::Error;

pub const UNKNOWN: &str = "unknown";

#[derive(Debug, Error)]
pub enum PluginError {
    #[error("probe command failed: {0}")]
    Probe(String),
    #[error("invalid user metadata file: {0}")]
    UserFile(String),
}

pub struct PluginContext<'a> {
    pub repo_root: &'a Path,
    /// Unix seconds when the build started (log mtime); None disables suspend detection.
    pub build_start_unix: Option<i64>,
    pub build: &'a BuildMetadata,
    pub environment: Option<&'a MetricsEnvironment>,
    pub user_metadata_path: Option<&'a Path>,
    /// Validated `--tag key=value` pairs; stored under a `tag.` prefix.
    pub tags: &'a BTreeMap<String, String>,
    pub probe: &'a dyn SystemProbe,
}

pub trait MetricsPlugin {
    fn name(&self) -> &'static str;
    fn collect(&self, context: &PluginContext<'_>)
    -> Result<BTreeMap<String, String>, PluginError>;
}

/// Runs each plugin, redacts everything it produced, and folds errors into
/// warnings. A plugin failure never fails the analysis.
///
/// Keys and warning text are redacted alongside values. Built-in plugins use
/// literal keys, but `user-file` builds them from user JSON, and an error
/// message can quote a path — both reach the same output as any value.
pub fn run_plugins(
    plugins: &[&dyn MetricsPlugin],
    context: &PluginContext<'_>,
) -> CollectedMetadata {
    let mut collected = CollectedMetadata::default();
    for plugin in plugins {
        match plugin.collect(context) {
            Ok(entries) => {
                for (key, value) in entries {
                    collected
                        .entries
                        .insert(redact_value(&key), redact_value(&value));
                }
            }
            Err(error) => collected
                .warnings
                .push(redact_value(&format!("{}: {error}", plugin.name()))),
        }
    }
    collected
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    struct LeakyPlugin;
    impl MetricsPlugin for LeakyPlugin {
        fn name(&self) -> &'static str {
            "leaky"
        }
        fn collect(&self, _: &PluginContext<'_>) -> Result<BTreeMap<String, String>, PluginError> {
            Ok(BTreeMap::from([(
                "leaky.value".to_owned(),
                "dev@company.example at /Users/someone/repo".to_owned(),
            )]))
        }
    }

    struct FailingPlugin;
    impl MetricsPlugin for FailingPlugin {
        fn name(&self) -> &'static str {
            "failing"
        }
        fn collect(&self, _: &PluginContext<'_>) -> Result<BTreeMap<String, String>, PluginError> {
            Err(PluginError::Probe("boom".into()))
        }
    }

    fn context<'a>(probe: &'a FakeProbe, build: &'a BuildMetadata) -> PluginContext<'a> {
        static NO_TAGS: OnceLock<BTreeMap<String, String>> = OnceLock::new();
        PluginContext {
            repo_root: Path::new("."),
            build_start_unix: None,
            build,
            environment: None,
            user_metadata_path: None,
            tags: NO_TAGS.get_or_init(BTreeMap::new),
            probe,
        }
    }

    /// A plugin whose *key* carries sensitive text, as `user-file` can.
    struct LeakyKeyPlugin;
    impl MetricsPlugin for LeakyKeyPlugin {
        fn name(&self) -> &'static str {
            "leaky-key"
        }
        fn collect(&self, _: &PluginContext<'_>) -> Result<BTreeMap<String, String>, PluginError> {
            Ok(BTreeMap::from([(
                "user./Users/someone".to_owned(),
                "fine".to_owned(),
            )]))
        }
    }

    /// A plugin whose error message quotes a path, as an I/O failure does.
    struct LeakyErrorPlugin;
    impl MetricsPlugin for LeakyErrorPlugin {
        fn name(&self) -> &'static str {
            "leaky-error"
        }
        fn collect(&self, _: &PluginContext<'_>) -> Result<BTreeMap<String, String>, PluginError> {
            Err(PluginError::UserFile(
                "could not read /Users/someone/secrets.json".to_owned(),
            ))
        }
    }

    #[test]
    fn values_are_redacted_and_failures_become_warnings() {
        let probe = FakeProbe(Default::default());
        let build = BuildMetadata::default();
        let collected = run_plugins(&[&LeakyPlugin, &FailingPlugin], &context(&probe, &build));
        let value = &collected.entries["leaky.value"];
        assert!(!value.contains("dev@company.example"));
        assert!(!value.contains("/Users/someone"));
        assert_eq!(collected.warnings.len(), 1);
        assert!(collected.warnings[0].starts_with("failing:"));
    }

    /// Keys reach the same output as values, and `user-file` builds them from
    /// user JSON.
    #[test]
    fn keys_are_redacted_too() {
        let probe = FakeProbe(Default::default());
        let build = BuildMetadata::default();
        let collected = run_plugins(&[&LeakyKeyPlugin], &context(&probe, &build));
        assert!(
            collected.entries.keys().all(|key| !key.contains("someone")),
            "a key leaked a home directory: {:?}",
            collected.entries.keys().collect::<Vec<_>>()
        );
    }

    /// Warnings are user-visible output, and an I/O error quotes its path.
    #[test]
    fn warning_text_is_redacted() {
        let probe = FakeProbe(Default::default());
        let build = BuildMetadata::default();
        let collected = run_plugins(&[&LeakyErrorPlugin], &context(&probe, &build));
        assert_eq!(collected.warnings.len(), 1);
        assert!(
            !collected.warnings[0].contains("someone"),
            "warning leaked a home directory: {}",
            collected.warnings[0]
        );
    }

    /// A secret in a plugin value must not survive, which is what `tags.rs`
    /// promises when it accepts free-text tag values.
    #[test]
    fn a_secret_in_a_plugin_value_is_redacted() {
        struct SecretPlugin;
        impl MetricsPlugin for SecretPlugin {
            fn name(&self) -> &'static str {
                "secret"
            }
            fn collect(
                &self,
                _: &PluginContext<'_>,
            ) -> Result<BTreeMap<String, String>, PluginError> {
                Ok(BTreeMap::from([(
                    "tag.deploy".to_owned(),
                    "token=abc123".to_owned(),
                )]))
            }
        }
        let probe = FakeProbe(Default::default());
        let build = BuildMetadata::default();
        let collected = run_plugins(&[&SecretPlugin], &context(&probe, &build));
        assert!(
            !collected.entries["tag.deploy"].contains("abc123"),
            "got {}",
            collected.entries["tag.deploy"]
        );
    }

    #[test]
    fn one_plugin_failing_does_not_stop_the_others() {
        let probe = FakeProbe(Default::default());
        let build = BuildMetadata::default();
        let collected = run_plugins(&[&FailingPlugin, &LeakyPlugin], &context(&probe, &build));
        assert!(collected.entries.contains_key("leaky.value"));
        assert_eq!(collected.warnings.len(), 1);
    }

    #[test]
    fn no_plugins_yields_nothing() {
        let probe = FakeProbe(Default::default());
        let build = BuildMetadata::default();
        let collected = run_plugins(&[], &context(&probe, &build));
        assert!(collected.entries.is_empty());
        assert!(collected.warnings.is_empty());
    }
}
