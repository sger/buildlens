use crate::PluginError;
use std::collections::BTreeMap;
use std::process::Command;

/// Programs plugins are allowed to invoke. Nothing else can be spawned.
const ALLOWED: &[&str] = &["git", "sysctl", "pmset", "scutil"];

/// The only place in this crate that spawns a process: fixed binary, fixed argv, no shell.
pub trait SystemProbe {
    fn run(&self, program: &'static str, args: &[&str]) -> Result<String, PluginError>;
}

pub struct RealProbe;

impl SystemProbe for RealProbe {
    fn run(&self, program: &'static str, args: &[&str]) -> Result<String, PluginError> {
        if !ALLOWED.contains(&program) {
            return Err(PluginError::Probe(format!(
                "program '{program}' is not on the plugin allowlist"
            )));
        }
        let output = Command::new(program)
            .args(args)
            .output()
            .map_err(|error| PluginError::Probe(format!("{program} failed to start: {error}")))?;
        if !output.status.success() {
            return Err(PluginError::Probe(format!(
                "{program} exited with {}",
                output.status
            )));
        }
        String::from_utf8(output.stdout)
            .map_err(|_| PluginError::Probe(format!("{program} produced non-UTF-8 output")))
    }
}

/// Test double keyed by [`FakeProbe::key`].
pub struct FakeProbe(pub BTreeMap<String, String>);

impl FakeProbe {
    /// The lookup key for one invocation.
    ///
    /// Arguments are joined with `\u{1}` rather than a space so that an
    /// argument *containing* a space cannot produce the same key as two
    /// separate arguments — otherwise a test could pass against a call the
    /// real probe would make differently.
    pub fn key(program: &str, args: &[&str]) -> String {
        std::iter::once(program)
            .chain(args.iter().copied())
            .collect::<Vec<_>>()
            .join("\u{1}")
    }

    /// Builds a probe from `(program, args, output)` triples.
    pub fn with(entries: &[(&str, &[&str], &str)]) -> Self {
        Self(
            entries
                .iter()
                .map(|(program, args, output)| (Self::key(program, args), (*output).to_owned()))
                .collect(),
        )
    }
}

impl SystemProbe for FakeProbe {
    fn run(&self, program: &'static str, args: &[&str]) -> Result<String, PluginError> {
        let key = Self::key(program, args);
        self.0.get(&key).cloned().ok_or_else(|| {
            PluginError::Probe(format!(
                "no fake output for '{}'",
                key.replace('\u{1}', " ")
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_probe_rejects_programs_off_the_allowlist() {
        let error = RealProbe.run("curl", &["https://example.com"]).unwrap_err();
        assert!(error.to_string().contains("not on the plugin allowlist"));
    }
}
