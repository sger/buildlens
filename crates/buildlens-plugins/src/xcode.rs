use crate::{MetricsPlugin, PluginContext, PluginError, UNKNOWN};
use std::collections::BTreeMap;

/// Copies Xcode facts already parsed from the log; never spawns a process.
pub struct XcodeEnvPlugin;

impl MetricsPlugin for XcodeEnvPlugin {
    fn name(&self) -> &'static str {
        "xcode"
    }

    fn collect(
        &self,
        context: &PluginContext<'_>,
    ) -> Result<BTreeMap<String, String>, PluginError> {
        let or_unknown =
            |value: Option<&String>| value.cloned().unwrap_or_else(|| UNKNOWN.to_owned());
        let environment = context.environment;
        Ok(BTreeMap::from([
            (
                "xcode.version".to_owned(),
                or_unknown(
                    context
                        .build
                        .xcode_version
                        .as_ref()
                        .or(environment.and_then(|e| e.xcode_version.as_ref())),
                ),
            ),
            (
                "xcode.sdk".to_owned(),
                or_unknown(
                    context
                        .build
                        .sdk
                        .as_ref()
                        .or(environment.and_then(|e| e.sdk.as_ref())),
                ),
            ),
            (
                "xcode.platform".to_owned(),
                or_unknown(
                    context
                        .build
                        .platform
                        .as_ref()
                        .or(environment.and_then(|e| e.platform.as_ref())),
                ),
            ),
        ]))
    }
}
