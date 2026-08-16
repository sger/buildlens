use crate::{MetricsPlugin, PluginContext, PluginError, UNKNOWN};
use regex::Regex;
use std::collections::BTreeMap;
use std::sync::OnceLock;

/// Reads the CPU speed limit from macOS power management. 100 means no
/// throttling; anything lower means the machine was thermally limited and its
/// timings are not comparable to an unthrottled run.
pub struct ThermalPlugin;

impl MetricsPlugin for ThermalPlugin {
    fn name(&self) -> &'static str {
        "thermal"
    }

    fn collect(
        &self,
        context: &PluginContext<'_>,
    ) -> Result<BTreeMap<String, String>, PluginError> {
        static RE: OnceLock<Regex> = OnceLock::new();
        let re =
            RE.get_or_init(|| Regex::new(r"CPU_Speed_Limit\s*=\s*(\d+)").expect("valid regex"));
        let value = context
            .probe
            .run("pmset", &["-g", "therm"])
            .ok()
            .and_then(|output| re.captures(&output).map(|captures| captures[1].to_owned()))
            .unwrap_or_else(|| UNKNOWN.to_owned());
        Ok(BTreeMap::from([(
            "thermal.cpu_speed_limit".to_owned(),
            value,
        )]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FakeProbe;
    use buildlens_core::BuildMetadata;
    use std::path::Path;

    fn collect_with(output: Option<&str>) -> BTreeMap<String, String> {
        let probe = match output {
            Some(out) => FakeProbe::with(&[("pmset", &["-g", "therm"], out)]),
            None => FakeProbe(Default::default()),
        };
        let build = BuildMetadata::default();
        let context = PluginContext {
            repo_root: Path::new("."),
            build_start_unix: None,
            build: &build,
            environment: None,
            user_metadata_path: None,
            tags: &Default::default(),
            probe: &probe,
        };
        ThermalPlugin.collect(&context).unwrap()
    }

    #[test]
    fn parses_speed_limit() {
        let entries = collect_with(Some("CPU Power notify\n\tCPU_Speed_Limit \t= 80\n"));
        assert_eq!(entries["thermal.cpu_speed_limit"], "80");
    }

    #[test]
    fn missing_pmset_is_unknown() {
        assert_eq!(collect_with(None)["thermal.cpu_speed_limit"], UNKNOWN);
    }
}
