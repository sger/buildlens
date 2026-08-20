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
    let cpu = probe
        .run("sysctl", &["-n", "machdep.cpu.brand_string"])
        .unwrap_or_default();
    let memory = probe
        .run("sysctl", &["-n", "hw.memsize"])
        .unwrap_or_default();
    let host = probe
        .run("sysctl", &["-n", "kern.uuid"])
        .unwrap_or_default();
    let material = format!("{model}|{cpu}|{memory}|{host}");
    Some(buildlens_plugins::pseudonymize_email(&material).replace("user-", "machine-"))
}

/// The local user and host, for `--attribution identified`.
///
/// Unlike [`machine_id`] this is deliberately *not* hashed: the entire point
/// of the identified tier is that a team lead can read the name. It is
/// therefore only ever called from a path where that tier was explicitly
/// chosen.
///
/// Read from the environment rather than by spawning `id` and `hostname`:
/// the plugin probe allowlist exists to keep this crate from running arbitrary
/// programs, and widening it for two values libc already exposes would trade a
/// real safety property for nothing. `USER` is set by every login shell;
/// `HOSTNAME` is not, so the hostname falls back to the system call.
pub fn identity(probe: &dyn buildlens_plugins::SystemProbe) -> buildlens_core::wire::Identity {
    let clean = |value: String| {
        let value = value.trim().to_owned();
        if value.is_empty() { None } else { Some(value) }
    };
    let user = std::env::var("USER")
        .ok()
        .or_else(|| std::env::var("LOGNAME").ok())
        .and_then(clean);
    let host = hostname(probe);
    buildlens_core::wire::Identity { user, host }
}

/// The short hostname.
///
/// `scutil --get LocalHostName` is the macOS answer, and goes through the
/// plugin probe like every other system call in this crate — fixed binary,
/// fixed argv, no shell. `HOST` (set by zsh) is tried first so an interactive
/// run answers without spawning anything.
///
/// Truncated at the first dot to match what `hostname -s` reports: a machine
/// on a corporate network answers with a fully-qualified name whose domain is
/// the same for everyone and only makes the value harder to read.
fn hostname(probe: &dyn buildlens_plugins::SystemProbe) -> Option<String> {
    let raw = std::env::var("HOST")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| probe.run("scutil", &["--get", "LocalHostName"]).ok())?;
    let short = raw
        .trim()
        .split('.')
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();
    if short.is_empty() { None } else { Some(short) }
}

pub struct PushOptions<'a> {
    pub server: &'a str,
    pub token: Option<&'a str>,
    pub project: &'a str,
    pub attribution: Attribution,
    pub machine_id: Option<String>,
    /// Only populated for [`Attribution::Identified`]; ignored otherwise.
    pub identity: buildlens_core::wire::Identity,
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
    let build = WireBuild::from_metrics_with_identity(
        metrics,
        options.project,
        options.machine_id.clone(),
        options.attribution,
        options.identity.clone(),
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
            (
                "sysctl",
                &["-n", "machdep.cpu.brand_string"],
                "Apple M2 Pro\n",
            ),
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
