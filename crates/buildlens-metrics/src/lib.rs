mod activity;
mod redact;
mod slf;

pub use redact::{PLACEHOLDER, redact_path, redact_text, redacted};

use buildlens_core::{
    BuildCategory, BuildMetrics, Detail, MetricsSourceKind, PhaseMetric, SwiftTimingKind,
    SwiftTimingMetric, TargetMetric,
};
use flate2::read::GzDecoder;
use regex::Regex;
use std::{
    fs::File,
    io::{Cursor, Read},
    path::Path,
    sync::OnceLock,
};
use thiserror::Error;

pub use slf::SlfError;

/// Refuse to inflate activity logs beyond this size (decompressed).
const MAX_DECOMPRESSED_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum MetricsError {
    #[error("I/O while reading metrics input: {0}")]
    Io(#[from] std::io::Error),
    #[error("activity log could not be decoded: {0}")]
    Slf(#[from] SlfError),
    #[error("unsupported metrics input format")]
    Unsupported,
}

pub fn analyze_file(path: impl AsRef<Path>, detail: Detail) -> Result<BuildMetrics, MetricsError> {
    let path = path.as_ref();
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;
    let mut metrics = analyze_bytes(&bytes, path.extension().and_then(|x| x.to_str()), detail)?;
    metrics.source_log = Some(path.to_string_lossy().into_owned());
    Ok(metrics)
}

pub fn analyze_bytes(
    bytes: &[u8],
    extension: Option<&str>,
    detail: Detail,
) -> Result<BuildMetrics, MetricsError> {
    if extension == Some("xcactivitylog") || bytes.starts_with(&[0x1f, 0x8b]) {
        return Ok(analyze_activity_log(bytes, detail));
    }
    let text = String::from_utf8_lossy(bytes);
    Ok(analyze_text(&text))
}

pub fn analyze_text(text: &str) -> BuildMetrics {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?i)^(.+?)\s*(?:\(([^)]+)\))?\s*\|\s*([0-9]+(?:\.[0-9]+)?)\s*seconds?\s*$")
            .unwrap()
    });
    let mut phases = Vec::new();
    let mut targets: Vec<TargetMetric> = Vec::new();
    let mut swift_timings = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(timing) = parse_swift_timing_line(line) {
            swift_timings.push(timing);
            continue;
        }
        if let Some(c) = re.captures(line) {
            let name = c[1].trim().to_owned();
            let seconds: f64 = c[3].parse().unwrap_or(0.0);
            if seconds <= 0.0 {
                continue;
            }
            let target = c.get(2).map(|x| x.as_str().trim().to_owned());
            phases.push(PhaseMetric {
                name,
                seconds,
                started_at: None,
                ended_at: None,
            });
            if let Some(target) = target {
                targets.push(TargetMetric {
                    fingerprint: format!("target:{target}"),
                    name: target,
                    seconds,
                    started_at: None,
                    ended_at: None,
                    fetched_from_cache: false,
                    category: BuildCategory::Unknown,
                    compiled_count: 0,
                    steps: vec![],
                });
            }
        }
    }
    let total_seconds = phases
        .iter()
        .map(|phase| phase.seconds)
        .reduce(|a, b| a + b);
    BuildMetrics {
        total_seconds,
        phases,
        targets,
        swift_timings,
        ..BuildMetrics::empty(MetricsSourceKind::XcodebuildText, Vec::new())
    }
}

fn analyze_activity_log(bytes: &[u8], detail: Detail) -> BuildMetrics {
    let mut decoded = Vec::new();
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut decoder = GzDecoder::new(Cursor::new(bytes)).take(MAX_DECOMPRESSED_BYTES);
        if let Err(error) = decoder.read_to_end(&mut decoded) {
            return empty_activity_metrics(format!("activity log decompression failed: {error}"));
        }
        if decoded.len() as u64 >= MAX_DECOMPRESSED_BYTES {
            return empty_activity_metrics(format!(
                "activity log exceeds the {MAX_DECOMPRESSED_BYTES}-byte decompression cap"
            ));
        }
    } else {
        decoded.extend_from_slice(bytes);
    }
    let full_detail = detail == Detail::Full;
    let (log, warnings) = slf::parse(
        &decoded,
        &slf::ParseOptions {
            keep_text: full_detail,
        },
    );
    if log.main_section.is_none() && !warnings.is_empty() {
        return empty_activity_metrics(warnings.join("; "));
    }
    let mut metrics = activity::map(&log, warnings, &activity::MapOptions { full_detail });
    if metrics.build_id.is_none() {
        metrics.build_id = Some(format!("sha:{:016x}", content_hash(&decoded)));
    }
    metrics
}

/// Parses one `-warn-long-function-bodies` / `-warn-long-expression-type-checking`
/// warning out of a text build log, e.g.
///
/// ```text
/// /path/File.swift:6:25: warning: expression took 3ms to type-check (limit: 1ms)
/// /path/File.swift:85:18: warning: instance method 'resolve(error:)' took 4ms to type-check (limit: 1ms)
/// ```
///
/// Both flags report "to type-check"; they differ only in whether a symbol is
/// named, which is what distinguishes a function body from a bare expression.
fn parse_swift_timing_line(line: &str) -> Option<SwiftTimingMetric> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r"^(?<file>.+?):(?<line>\d+):(?<column>\d+): warning: (?:(?<kind>.+?) '(?<symbol>[^']+)'|expression) took (?<ms>\d+)ms to type-check",
        )
        .expect("valid regex")
    });
    let captures = re.captures(line)?;
    let symbol = captures.name("symbol").map(|m| m.as_str().to_owned());
    Some(SwiftTimingMetric {
        // A named symbol means -warn-long-function-bodies fired; a bare
        // "expression" is the type-checking flag.
        kind: if symbol.is_some() {
            SwiftTimingKind::FunctionBody
        } else {
            SwiftTimingKind::TypeCheck
        },
        file: captures["file"].to_owned(),
        line: captures["line"].parse().unwrap_or(0),
        column: captures["column"].parse().unwrap_or(0),
        symbol,
        milliseconds: captures["ms"].parse().unwrap_or(0.0),
        target: None,
    })
}

/// Stable identity for logs where Xcode omitted `uniqueIdentifier`.
fn content_hash(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn empty_activity_metrics(warning: String) -> BuildMetrics {
    BuildMetrics::empty(MetricsSourceKind::Xcactivitylog, vec![warning])
}
