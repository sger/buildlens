use crate::{MetricsPlugin, PluginContext, PluginError, UNKNOWN};
use std::collections::BTreeMap;

pub struct HardwarePlugin;

impl MetricsPlugin for HardwarePlugin {
    fn name(&self) -> &'static str {
        "hardware"
    }

    fn collect(
        &self,
        context: &PluginContext<'_>,
    ) -> Result<BTreeMap<String, String>, PluginError> {
        let sysctl = |key: &str| {
            context
                .probe
                .run("sysctl", &["-n", key])
                .map(|out| out.trim().to_owned())
                .filter(|out| !out.is_empty())
                .unwrap_or_else(|_| UNKNOWN.to_owned())
        };
        let mut entries = BTreeMap::from([
            ("hw.model".to_owned(), sysctl("hw.model")),
            ("hw.ncpu".to_owned(), sysctl("hw.ncpu")),
            ("hw.memsize_bytes".to_owned(), sysctl("hw.memsize")),
            (
                "hw.cpu_brand".to_owned(),
                sysctl("machdep.cpu.brand_string"),
            ),
            ("os.version".to_owned(), sysctl("kern.osproductversion")),
            ("os.arch".to_owned(), std::env::consts::ARCH.to_owned()),
        ]);
        // Memory pressure is the usual explanation for a build that is slow
        // without any target being slow: once the machine is swapping, every
        // step pays for it and no per-target timing shows why.
        let free_bytes = context
            .probe
            .run("sysctl", &["-n", "vm.page_free_count"])
            .ok()
            .zip(context.probe.run("sysctl", &["-n", "hw.pagesize"]).ok())
            .and_then(|(pages, size)| parse_u64(&pages)?.checked_mul(parse_u64(&size)?));
        entries.insert(
            "hw.memfree_bytes".to_owned(),
            free_bytes.map_or_else(|| UNKNOWN.to_owned(), |bytes| bytes.to_string()),
        );
        entries.insert("hw.swap_used_bytes".to_owned(), swap_used_bytes(context));
        // Nominal clock speed. Apple Silicon does not publish this sysctl, so
        // it is absent rather than zero on most modern Macs.
        let cpu_hz = sysctl("hw.cpufrequency");
        if cpu_hz != UNKNOWN {
            entries.insert("hw.cpu_frequency_hz".to_owned(), cpu_hz);
        }
        // Uptime separates "first build after a reboot" (cold caches, cold
        // page cache) from a machine that has been warm for days.
        entries.insert("os.uptime_seconds".to_owned(), uptime_seconds(context));
        // A VM's timings are not comparable to bare metal, so aggregates that
        // mix them are misleading. `machdep.cpu.features` advertising VMM is
        // the standard hypervisor tell on Intel; Apple VMs report a model
        // identifier of "VirtualMac".
        let is_virtual = sysctl("machdep.cpu.features").contains("VMM")
            || entries
                .get("hw.model")
                .is_some_and(|model| model.starts_with("VirtualMac"));
        entries.insert("hw.is_virtual".to_owned(), is_virtual.to_string());
        // Timezone is deliberately not collected. Reading it without a new
        // dependency means either shelling out to `date` (not on the probe
        // allowlist) or trusting $TZ, which is unset on a normal macOS
        // machine — a field that reports "unknown" everywhere is worse than
        // an absent one. It also narrows down where a developer is.
        Ok(entries)
    }
}

/// Seconds since boot, from `kern.boottime`, which reports a struct-like
/// `{ sec = 1699999999, usec = 0 } ...` rather than a bare number.
fn uptime_seconds(context: &PluginContext<'_>) -> String {
    let Ok(raw) = context.probe.run("sysctl", &["-n", "kern.boottime"]) else {
        return UNKNOWN.to_owned();
    };
    let Some(boot) = parse_boottime_seconds(&raw) else {
        return UNKNOWN.to_owned();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    now.checked_sub(boot)
        .map_or_else(|| UNKNOWN.to_owned(), |seconds| seconds.to_string())
}

/// Extracts `sec = N` from a `kern.boottime` line.
fn parse_boottime_seconds(raw: &str) -> Option<u64> {
    let after = raw.split("sec =").nth(1)?;
    let digits: String = after
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    parse_u64(&digits)
}

/// `vm.swapusage` is a formatted line, not a number:
/// `total = 2048.00M  used = 512.25M  free = 1535.75M  (encrypted)`.
/// Only the used figure is kept, converted to bytes so it sorts and compares
/// like every other byte-valued fact.
fn swap_used_bytes(context: &PluginContext<'_>) -> String {
    context
        .probe
        .run("sysctl", &["-n", "vm.swapusage"])
        .ok()
        .and_then(|usage| parse_swap_used(&usage))
        .map_or_else(|| UNKNOWN.to_owned(), |bytes| bytes.to_string())
}

fn parse_swap_used(usage: &str) -> Option<u64> {
    let after = usage.split("used =").nth(1)?.trim_start();
    let end = after
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(after.len());
    let (number, rest) = after.split_at(end);
    let value: f64 = number.parse().ok()?;
    let multiplier = match rest.bytes().next() {
        Some(b'K') => 1024.0,
        Some(b'M') => 1024.0 * 1024.0,
        Some(b'G') => 1024.0 * 1024.0 * 1024.0,
        _ => 1.0,
    };
    Some((value * multiplier) as u64)
}

fn parse_u64(value: &str) -> Option<u64> {
    value.trim().parse().ok()
}

trait ResultFilter<T, E> {
    fn filter(self, keep: impl FnOnce(&T) -> bool) -> Result<T, ()>;
}
impl<T, E> ResultFilter<T, E> for Result<T, E> {
    fn filter(self, keep: impl FnOnce(&T) -> bool) -> Result<T, ()> {
        match self {
            Ok(value) if keep(&value) => Ok(value),
            _ => Err(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FakeProbe, MetricsPlugin};
    use buildlens_core::BuildMetadata;
    use std::path::Path;

    fn collect(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        // Fixtures are written as "sysctl -n <name>" for readability; the
        // probe keys on argv, so split them back apart here.
        let probe = FakeProbe(
            pairs
                .iter()
                .map(|(invocation, value)| {
                    let mut parts = invocation.split(' ');
                    let program = parts.next().expect("a program name");
                    let args: Vec<&str> = parts.collect();
                    (FakeProbe::key(program, &args), (*value).to_owned())
                })
                .collect(),
        );
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
        HardwarePlugin
            .collect(&context)
            .expect("hardware plugin never fails")
    }

    #[test]
    fn uptime_is_now_minus_boottime() {
        let entries = collect(&[(
            "sysctl -n kern.boottime",
            "{ sec = 1000, usec = 0 } Thu Jan  1 00:16:40 1970\n",
        )]);
        // Wall clock moves, so assert the shape rather than an exact value:
        // any real "now" minus a 1970 boot time is a large positive number.
        let uptime: u64 = entries["os.uptime_seconds"]
            .parse()
            .expect("uptime parses as a number");
        assert!(uptime > 1_000_000, "expected a large uptime, got {uptime}");
    }

    #[test]
    fn unparseable_boottime_is_unknown() {
        let entries = collect(&[("sysctl -n kern.boottime", "garbage\n")]);
        assert_eq!(entries["os.uptime_seconds"], UNKNOWN);
    }

    /// A bare-metal Mac must not be labelled a VM: `hw.model` on real
    /// hardware starts with "Mac", and the VMM feature flag is absent.
    #[test]
    fn physical_machine_is_not_virtual() {
        let entries = collect(&[
            ("sysctl -n hw.model", "Mac15,3\n"),
            ("sysctl -n machdep.cpu.features", "FPU VME DE PSE TSC\n"),
        ]);
        assert_eq!(entries["hw.is_virtual"], "false");
    }

    #[test]
    fn vmm_feature_flag_marks_a_virtual_machine() {
        let entries = collect(&[
            ("sysctl -n hw.model", "MacPro7,1\n"),
            ("sysctl -n machdep.cpu.features", "FPU VME DE VMM PSE\n"),
        ]);
        assert_eq!(entries["hw.is_virtual"], "true");
    }

    #[test]
    fn apple_virtual_machine_model_marks_a_virtual_machine() {
        let entries = collect(&[("sysctl -n hw.model", "VirtualMac2,1\n")]);
        assert_eq!(entries["hw.is_virtual"], "true");
    }

    /// Apple Silicon has no `hw.cpufrequency`; the key must be omitted rather
    /// than recorded as "unknown" or 0.
    #[test]
    fn absent_cpu_frequency_is_omitted_not_unknown() {
        let entries = collect(&[("sysctl -n hw.model", "Mac15,3\n")]);
        assert!(!entries.contains_key("hw.cpu_frequency_hz"));
    }

    #[test]
    fn cpu_frequency_is_recorded_when_present() {
        let entries = collect(&[("sysctl -n hw.cpufrequency", "2400000000\n")]);
        assert_eq!(entries["hw.cpu_frequency_hz"], "2400000000");
    }

    #[test]
    fn free_memory_is_pages_times_page_size() {
        let entries = collect(&[
            ("sysctl -n vm.page_free_count", "125000\n"),
            ("sysctl -n hw.pagesize", "16384\n"),
        ]);
        assert_eq!(
            entries["hw.memfree_bytes"],
            (125_000u64 * 16_384).to_string()
        );
    }

    #[test]
    fn swap_usage_is_converted_to_bytes() {
        let entries = collect(&[(
            "sysctl -n vm.swapusage",
            "total = 2048.00M  used = 512.50M  free = 1535.50M  (encrypted)\n",
        )]);
        assert_eq!(
            entries["hw.swap_used_bytes"],
            ((512.5 * 1024.0 * 1024.0) as u64).to_string()
        );
    }

    /// A machine with swap disabled reports `used = 0.00M`; that is a real
    /// reading of zero, not a failed probe, and must not become "unknown".
    #[test]
    fn zero_swap_is_recorded_as_zero() {
        let entries = collect(&[(
            "sysctl -n vm.swapusage",
            "total = 0.00M  used = 0.00M  free = 0.00M\n",
        )]);
        assert_eq!(entries["hw.swap_used_bytes"], "0");
    }

    /// Older/virtualized hosts may not expose these sysctls at all. The keys
    /// still appear so a build row never has a hole in it.
    #[test]
    fn missing_sysctls_degrade_to_unknown() {
        let entries = collect(&[]);
        assert_eq!(entries["hw.memfree_bytes"], UNKNOWN);
        assert_eq!(entries["hw.swap_used_bytes"], UNKNOWN);
        assert_eq!(entries["hw.model"], UNKNOWN);
    }

    #[test]
    fn swap_parsing_handles_gigabyte_and_kilobyte_units() {
        assert_eq!(
            parse_swap_used("total = 4G  used = 1.5G  free = 2.5G"),
            Some(1_610_612_736)
        );
        assert_eq!(
            parse_swap_used("total = 4M  used = 256K  free = 3M"),
            Some(262_144)
        );
        assert_eq!(parse_swap_used("nothing useful here"), None);
    }
}
