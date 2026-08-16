use buildlens_core::TimingSummary;
use regex::Regex;
use std::sync::OnceLock;

fn timing_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^(.+?)\s*(?:\(([^)]+)\))?\s*\|\s*([0-9]+(?:\.[0-9]+)?)\s*seconds?\s*$")
            .unwrap()
    })
}

pub fn parse(line: &str, timings: &mut TimingSummary) -> bool {
    let Some(captures) = timing_re().captures(line.trim()) else {
        return false;
    };
    let phase = captures[1].trim().to_owned();
    let seconds: f64 = captures[3].parse().ok().unwrap_or_default();
    if phase.is_empty() || seconds == 0.0 {
        return false;
    }
    *timings.phases.entry(phase).or_default() += seconds;
    if let Some(target) = captures
        .get(2)
        .map(|x| x.as_str().trim())
        .filter(|x| !x.is_empty())
    {
        *timings.targets.entry(target.to_owned()).or_default() += seconds;
    }
    true
}

pub fn finalize(timings: &mut TimingSummary) {
    timings.slowest_targets = timings
        .targets
        .iter()
        .map(|(target, seconds)| buildlens_core::TargetTiming {
            target: target.clone(),
            seconds: *seconds,
        })
        .collect();
    timings.slowest_targets.sort_by(|a, b| {
        b.seconds
            .partial_cmp(&a.seconds)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    timings.slowest_targets.truncate(10);
}
