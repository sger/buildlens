//! Sending metrics to a team server. Nothing here runs unless the user passes
//! `--server`, and the payload is the explicit `WireBuild` document rather than
//! a serialization of the local analysis.

use anyhow::{Context, Result, bail};
use buildlens_core::BuildMetrics;
use buildlens_core::wire::{Attribution, WireBuild};

/// Targets and phases per build; the fleet view needs the slow ones, not all
/// several hundred.
const MAX_TARGETS: usize = 40;

/// A serde-renamed enum as its storage string, matching what the local store
/// writes so the same value arrives whichever path a build took.
fn serde_plain<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Stable, non-reversible id for this machine. Derived from the hardware UUID
/// so it survives reboots and reinstalls, hashed so it cannot be traced back.
pub fn machine_id(probe: &dyn buildlens_plugins::SystemProbe) -> Option<String> {
    let model = probe.run("sysctl", &["-n", "hw.model"]).ok()?;
    let cpu = probe.run("sysctl", &["-n", "machdep.cpu.brand_string"]).unwrap_or_default();
    let memory = probe.run("sysctl", &["-n", "hw.memsize"]).unwrap_or_default();
    let host = probe.run("sysctl", &["-n", "kern.uuid"]).unwrap_or_default();
    let material = format!("{model}|{cpu}|{memory}|{host}");
    Some(buildlens_plugins::pseudonymize_email(&material).replace("user-", "machine-"))
}

pub struct PushOptions<'a> {
    pub server: &'a str,
    pub token: Option<&'a str>,
    pub project: &'a str,
    pub attribution: Attribution,
    pub machine_id: Option<String>,
    pub dry_run: bool,
    /// The analysis the metrics came from, when the caller has one.
    ///
    /// Supplies diagnostics and test results, which `BuildMetrics` does not
    /// carry — they come from the paired text log. Without it the payload is
    /// still valid and the server simply records no diagnostics or tests for
    /// this build, exactly as a wire-version-1 client did.
    pub analysis: Option<&'a buildlens_core::BuildAnalysis>,
}

/// Returns the payload that was sent (or would be sent, for a dry run).
pub fn push(metrics: &BuildMetrics, options: &PushOptions<'_>) -> Result<serde_json::Value> {
    let build = WireBuild::from_metrics(
        metrics,
        options.project,
        options.machine_id.clone(),
        options.attribution,
        MAX_TARGETS,
    )
    .context("metrics did not decode into a usable build, so nothing was sent")?;
    // `serde_plain` renders the two diagnostic enums the way the local store
    // does, so a pushed row and a locally stored one hold the same words.
    let build = match options.analysis {
        Some(analysis) => build.with_analysis(
            &analysis.diagnostics.diagnostics,
            &analysis.tests.tests,
            serde_plain,
            serde_plain,
        ),
        None => build,
    };
    let payload = serde_json::to_value(&build)?;
    if options.dry_run {
        return Ok(payload);
    }
    let url = format!("{}/v1/metrics", options.server.trim_end_matches('/'));
    let mut request = ureq::post(&url).header("Content-Type", "application/json");
    if let Some(token) = options.token {
        request = request.header("Authorization", &format!("Bearer {token}"));
    }
    match request.send_json(&build) {
        Ok(mut response) => {
            let body: serde_json::Value = response
                .body_mut()
                .read_json()
                .unwrap_or_else(|_| serde_json::json!({}));
            Ok(body)
        }
        Err(ureq::Error::StatusCode(401)) => {
            if options.token.is_some() {
                bail!(
                    "{} rejected the token. Check it matches the server's BUILDLENS_TOKEN.",
                    options.server
                )
            }
            bail!(
                "{} requires a token. Pass --token, or set BUILDLENS_TOKEN in your environment.",
                options.server
            )
        }
        Err(ureq::Error::StatusCode(code)) => {
            bail!("server rejected the metrics with HTTP {code}")
        }
        Err(error) => bail!("could not reach {url}: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buildlens_plugins::FakeProbe;

    #[test]
    fn machine_id_is_stable_and_opaque() {
        // Built through `FakeProbe::with`: the key encoding is the probe's own
        // business, and spelling it out here meant these tests silently
        // stopped matching when it changed.
        let probe = FakeProbe::with(&[
            ("sysctl", &["-n", "hw.model"], "Mac14,10\n"),
            ("sysctl", &["-n", "machdep.cpu.brand_string"], "Apple M2 Pro\n"),
            ("sysctl", &["-n", "hw.memsize"], "17179869184\n"),
            ("sysctl", &["-n", "kern.uuid"], "ABC-123\n"),
        ]);
        let first = machine_id(&probe).unwrap();
        let second = machine_id(&probe).unwrap();
        assert_eq!(first, second, "the same machine must hash identically");
        assert!(first.starts_with("machine-"));
        // None of the source material survives into the identifier.
        for secret in ["Mac14", "Apple M2", "ABC-123", "17179869184"] {
            assert!(!first.contains(secret), "{first} leaked {secret}");
        }
    }

    #[test]
    fn machine_id_differs_across_machines() {
        let make = |model: &str| {
            FakeProbe::with(&[
                ("sysctl", &["-n", "hw.model"], model),
                ("sysctl", &["-n", "machdep.cpu.brand_string"], "cpu"),
                ("sysctl", &["-n", "hw.memsize"], "1"),
                ("sysctl", &["-n", "kern.uuid"], "uuid"),
            ])
        };
        assert_ne!(
            machine_id(&make("Mac14,10")).unwrap(),
            machine_id(&make("MacBookPro18,3")).unwrap()
        );
    }
}
