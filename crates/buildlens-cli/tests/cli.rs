//! End-to-end CLI behavior that unit tests cannot cover: the analyze command's
//! pairing of a text log with a companion activity log.

use std::process::Command;

fn buildlens() -> Command {
    Command::new(env!("CARGO_BIN_EXE_buildlens"))
}

fn fixture(name: &str) -> String {
    format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn run(args: &[&str]) -> (bool, String, String) {
    let output = buildlens().args(args).output().expect("buildlens runs");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn text_log_alone_has_diagnostics_but_no_file_timings() {
    let (ok, stdout, _) = run(&["analyze", &fixture("sample.log"), "--format", "json"]);
    assert!(ok);
    let analysis: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(analysis["diagnostics"]["unique_warnings"].as_u64().unwrap() > 0);
    let files = analysis["metrics"]["files"].as_array().map_or(0, Vec::len);
    assert_eq!(files, 0, "a text log carries no per-file timings");
}

#[test]
fn companion_activity_log_adds_timings_to_text_diagnostics() {
    let (ok, stdout, _) = run(&[
        "analyze",
        &fixture("sample.log"),
        "--activity-log",
        &fixture("packagesbench.xcactivitylog"),
        "--format",
        "json",
    ]);
    assert!(ok);
    let analysis: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    // Diagnostics still come from the text log...
    assert!(analysis["diagnostics"]["unique_warnings"].as_u64().unwrap() > 0);
    // ...while timings now come from the activity log.
    let metrics = &analysis["metrics"];
    assert!(metrics["targets"].as_array().unwrap().len() > 100);
    assert!(!metrics["files"].as_array().unwrap().is_empty());
    assert_eq!(metrics["category"], "clean");
    assert!(metrics["total_seconds"].as_f64().unwrap() > 100.0);
}

#[test]
fn companion_must_be_an_activity_log() {
    let (ok, _, stderr) = run(&[
        "analyze",
        &fixture("sample.log"),
        "--activity-log",
        &fixture("successful-build.log"),
    ]);
    assert!(!ok);
    assert!(
        stderr.contains("expects an .xcactivitylog"),
        "got: {stderr}"
    );
}

#[test]
fn activity_log_primary_rejects_a_companion() {
    let (ok, _, stderr) = run(&[
        "analyze",
        &fixture("packagesbench.xcactivitylog"),
        "--activity-log",
        &fixture("packagesbench.xcactivitylog"),
    ]);
    assert!(!ok);
    assert!(stderr.contains("already an activity log"), "got: {stderr}");
}

#[test]
fn half_written_logs_are_refused_rather_than_stored() {
    let scratch = std::env::temp_dir().join(format!("buildlens-partial-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    let partial = scratch.join("partial.xcactivitylog");
    let full = std::fs::read(fixture("packagesbench.xcactivitylog")).unwrap();
    std::fs::write(&partial, &full[..full.len() / 4]).unwrap();
    // Not a real connection URL: these cases must fail before any database is
    // touched, so an unreachable one proves the log is rejected first.
    let db = "postgres://invalid/nonexistent";

    let (ok, _, stderr) = run(&["history", "save", partial.to_str().unwrap(), "--db", db]);
    assert!(!ok, "a truncated log must not be recorded");
    assert!(
        stderr.contains("did not decode into a usable build"),
        "got: {stderr}"
    );
    assert!(
        stderr.contains("buildlens collect"),
        "message should suggest the collector"
    );

    let empty = scratch.join("empty.xcactivitylog");
    std::fs::write(&empty, b"").unwrap();
    let (ok, _, stderr) = run(&["history", "save", empty.to_str().unwrap(), "--db", db]);
    assert!(!ok);
    assert!(stderr.contains("empty"), "got: {stderr}");
    let _ = std::fs::remove_dir_all(&scratch);
}
