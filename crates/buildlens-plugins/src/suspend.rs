use crate::{MetricsPlugin, PluginContext, PluginError, UNKNOWN};
use regex::Regex;
use std::collections::BTreeMap;
use std::sync::OnceLock;

/// Flags builds interrupted by machine sleep: the last wake time
/// (kern.sleeptime) falling after the build start means the machine slept
/// mid-build and its timings are unreliable.
pub struct SuspendPlugin;

impl MetricsPlugin for SuspendPlugin {
    fn name(&self) -> &'static str {
        "suspend"
    }

    fn collect(
        &self,
        context: &PluginContext<'_>,
    ) -> Result<BTreeMap<String, String>, PluginError> {
        static RE: OnceLock<Regex> = OnceLock::new();
        let re = RE.get_or_init(|| Regex::new(r"sec\s*=\s*(\d+)").expect("valid regex"));
        let value = match (
            context.build_start_unix,
            context.probe.run("sysctl", &["-n", "kern.sleeptime"]),
        ) {
            (Some(build_start), Ok(output)) => re
                .captures(&output)
                .and_then(|captures| captures[1].parse::<i64>().ok())
                .map(|sleep_epoch| (sleep_epoch > build_start).to_string())
                .unwrap_or_else(|| UNKNOWN.to_owned()),
            _ => UNKNOWN.to_owned(),
        };
        Ok(BTreeMap::from([("build.was_suspended".to_owned(), value)]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FakeProbe;
    use buildlens_core::BuildMetadata;
    use std::path::Path;

    fn collect_with(build_start: Option<i64>, sleep_epoch: i64) -> String {
        let sleeptime = format!("{{ sec = {sleep_epoch}, usec = 0 }} Tue Aug 12 08:00:00 2025");
        let probe = FakeProbe::with(&[("sysctl", &["-n", "kern.sleeptime"], &sleeptime)]);
        let build = BuildMetadata::default();
        let context = PluginContext {
            repo_root: Path::new("."),
            build_start_unix: build_start,
            build: &build,
            environment: None,
            user_metadata_path: None,
            tags: &Default::default(),
            probe: &probe,
        };
        SuspendPlugin.collect(&context).unwrap()["build.was_suspended"].clone()
    }

    #[test]
    fn sleep_after_build_start_means_suspended() {
        assert_eq!(collect_with(Some(1_000), 2_000), "true");
    }

    #[test]
    fn sleep_before_build_start_means_not_suspended() {
        assert_eq!(collect_with(Some(2_000), 1_000), "false");
    }

    #[test]
    fn missing_build_start_is_unknown() {
        assert_eq!(collect_with(None, 1_000), UNKNOWN);
    }
}
