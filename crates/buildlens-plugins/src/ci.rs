use crate::{MetricsPlugin, PluginContext, PluginError};
use std::collections::BTreeMap;

/// CI providers recognised by the presence of a marker variable.
///
/// The variable's *value* is never read. CI environments routinely hold
/// tokens, signing material, and internal URLs in exactly these variables, so
/// this plugin is deliberately limited to asking whether a name is set. That
/// makes the allowlist safe to extend without re-reviewing what each provider
/// happens to put in its variables.
const PROVIDERS: &[(&str, &str)] = &[
    ("GITHUB_ACTIONS", "github_actions"),
    ("GITLAB_CI", "gitlab_ci"),
    ("BITRISE_IO", "bitrise"),
    ("CIRCLECI", "circleci"),
    ("BUILDKITE", "buildkite"),
    ("TEAMCITY_VERSION", "teamcity"),
    ("JENKINS_URL", "jenkins"),
    ("TF_BUILD", "azure_pipelines"),
    ("APPCENTER_BUILD_ID", "app_center"),
    ("XCS", "xcode_server"),
    // Checked last: several providers set the generic marker in addition to
    // their own, so a specific match should win.
    ("CI", "unknown"),
];

/// Records whether the build ran on CI, and under which provider.
pub struct CiPlugin;

impl MetricsPlugin for CiPlugin {
    fn name(&self) -> &'static str {
        "ci"
    }

    fn collect(&self, _: &PluginContext<'_>) -> Result<BTreeMap<String, String>, PluginError> {
        Ok(entries(|name| std::env::var_os(name).is_some()))
    }
}

/// Split out from the environment lookup so tests can drive detection without
/// mutating process-wide state.
fn entries(is_set: impl Fn(&str) -> bool) -> BTreeMap<String, String> {
    let provider = PROVIDERS
        .iter()
        .find(|(name, _)| is_set(name))
        .map(|(_, provider)| *provider);
    BTreeMap::from([
        ("ci.is_ci".to_owned(), provider.is_some().to_string()),
        (
            "ci.provider".to_owned(),
            provider.unwrap_or("none").to_owned(),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detect(set: &[&str]) -> BTreeMap<String, String> {
        entries(|name| set.contains(&name))
    }

    #[test]
    fn a_local_build_is_not_ci() {
        let entries = detect(&[]);
        assert_eq!(entries["ci.is_ci"], "false");
        assert_eq!(entries["ci.provider"], "none");
    }

    #[test]
    fn a_known_provider_is_named() {
        let entries = detect(&["CI", "GITHUB_ACTIONS"]);
        assert_eq!(entries["ci.is_ci"], "true");
        assert_eq!(entries["ci.provider"], "github_actions");
    }

    /// A provider we do not recognise still sets the generic marker, which is
    /// enough to keep its builds out of local-machine percentiles.
    #[test]
    fn a_bare_ci_marker_is_still_ci() {
        let entries = detect(&["CI"]);
        assert_eq!(entries["ci.is_ci"], "true");
        assert_eq!(entries["ci.provider"], "unknown");
    }

    #[test]
    fn a_specific_provider_wins_over_the_generic_marker() {
        for (name, expected) in PROVIDERS.iter().filter(|(name, _)| *name != "CI") {
            let entries = detect(&["CI", name]);
            assert_eq!(entries["ci.provider"], *expected, "for {name}");
        }
    }

    /// The guarantee that makes this plugin safe: detection depends only on
    /// whether a name is set, so a variable holding a token cannot leak.
    #[test]
    fn provider_values_are_never_read() {
        let entries = entries(|name| {
            assert!(
                PROVIDERS.iter().any(|(known, _)| known == &name),
                "looked up a variable outside the allowlist: {name}"
            );
            name == "BUILDKITE"
        });
        assert_eq!(entries["ci.provider"], "buildkite");
        assert!(entries.values().all(|value| !value.contains("secret")));
    }
}
