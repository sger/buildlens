//! Integration tests against a real Postgres.
//!
//! Skipped unless BUILDLENS_TEST_DATABASE_URL is set, so `cargo test` stays
//! green on machines without a database:
//!
//!   createdb buildlens_test
//!   BUILDLENS_TEST_DATABASE_URL=postgres://$(whoami)@localhost/buildlens_test \
//!     cargo test -p buildlens-server
//!
//! Skipping is reported as `ignored`, not `ok`. A skipped test that prints
//! "passed" is worse than no test: CI without a database showed three green
//! results for code that never ran.
//!
//! Set BUILDLENS_REQUIRE_DATABASE=1 to turn a missing URL into a failure,
//! which is what CI that is supposed to have one should do.

// The store lives in a binary crate, so it is compiled into this test binary
// directly. Not every method is exercised here; they are covered by the unit
// tests in the binary itself.
#![allow(dead_code)]

use buildlens_core::wire::{Attribution, BuildStatus, WIRE_VERSION, WireBuild, WirePhase, WireTarget};
use buildlens_core::BuildCategory;
use postgres::{Client, NoTls};

fn database_url() -> Option<String> {
    match std::env::var("BUILDLENS_TEST_DATABASE_URL") {
        Ok(url) if !url.is_empty() => Some(url),
        _ if std::env::var("BUILDLENS_REQUIRE_DATABASE").is_ok_and(|v| v == "1") => {
            panic!("BUILDLENS_REQUIRE_DATABASE=1 but BUILDLENS_TEST_DATABASE_URL is unset")
        }
        _ => None,
    }
}

/// Tests run in parallel, so each gets its own Postgres schema rather than
/// sharing `public` — concurrent CREATE TABLE in one schema deadlocks on
/// pg_type regardless of IF NOT EXISTS.
fn isolated_url(base: &str, name: &str) -> String {
    let mut client = Client::connect(base, NoTls).expect("connect to test database");
    client
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {name} CASCADE; CREATE SCHEMA {name};"
        ))
        .expect("create test schema");
    let separator = if base.contains('?') { '&' } else { '?' };
    format!("{base}{separator}options=-c%20search_path%3D{name}")
}

fn build(key: &str, seconds: f64, started_at: Option<f64>, machine: Option<&str>) -> WireBuild {
    WireBuild {
        wire_version: WIRE_VERSION,
        build_key: key.into(),
        project: "App".into(),
        category: BuildCategory::Clean,
        total_seconds: seconds,
        compiled_count: 100,
        cache_hit_rate: Some(0.0),
        error_count: 0,
        warning_count: 0,
        status: Some(BuildStatus::Succeeded),
        started_at,
        attribution: Attribution::Pseudonymous,
        machine_id: machine.map(str::to_owned),
        xcode_version: Some("16.2".into()),
        platform: Some("iOS Simulator".into()),
        architecture: Some("arm64".into()),
        targets: vec![WireTarget {
            name: "Core".into(),
            seconds: seconds / 2.0,
            category: BuildCategory::Clean,
            fetched_from_cache: false,
            compiled_count: 50,
        }],
        phases: vec![WirePhase { name: "Prepare build".into(), seconds: 1.0 }],
    }
}

#[test]
fn stores_queries_and_deduplicates_builds() {
    let Some(url) = database_url() else {
        // `ignored` in the summary, rather than a green `ok` for a test that
        // did nothing.
        eprintln!("skipping: BUILDLENS_TEST_DATABASE_URL not set");
        return;
    };
    let url = isolated_url(&url, "t_store");
    let mut store = buildlens_server_store(&url);

    // A fresh build is stored...
    assert!(store.insert(&build("a", 100.0, Some(1_700_000_000.0), Some("m1"))).unwrap());
    // ...and re-sending it is a no-op, so a client retry cannot double-count.
    assert!(!store.insert(&build("a", 100.0, Some(1_700_000_000.0), Some("m1"))).unwrap());

    store.insert(&build("b", 200.0, Some(1_700_000_000.0), Some("m2"))).unwrap();
    store.insert(&build("c", 300.0, Some(1_700_000_000.0), Some("m2"))).unwrap();

    let builds = store.builds(10).unwrap();
    assert_eq!(builds["items"].as_array().unwrap().len(), 3);

    // Children are attached to their build.
    let targets: i64 = Client::connect(&url, NoTls)
        .unwrap()
        .query_one("SELECT COUNT(*)::BIGINT FROM build_targets", &[])
        .unwrap()
        .get(0);
    assert_eq!(targets, 3);
}

#[test]
fn partitions_by_the_build_start_day() {
    let Some(url) = database_url() else {
        // `ignored` in the summary, rather than a green `ok` for a test that
        // did nothing.
        eprintln!("skipping: BUILDLENS_TEST_DATABASE_URL not set");
        return;
    };
    let url = isolated_url(&url, "t_partition");
    let mut store = buildlens_server_store(&url);
    // 2023-11-14 and 2023-11-15 in Unix seconds.
    store.insert(&build("day1", 100.0, Some(1_700_000_000.0), Some("m1"))).unwrap();
    store.insert(&build("day2", 100.0, Some(1_700_090_000.0), Some("m1"))).unwrap();

    let mut client = Client::connect(&url, NoTls).unwrap();
    let days: Vec<String> = client
        .query("SELECT DISTINCT day::TEXT FROM builds ORDER BY 1", &[])
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(days, vec!["2023-11-14", "2023-11-15"]);
}

#[test]
fn stats_report_percentiles_and_machine_counts() {
    let Some(url) = database_url() else {
        // `ignored` in the summary, rather than a green `ok` for a test that
        // did nothing.
        eprintln!("skipping: BUILDLENS_TEST_DATABASE_URL not set");
        return;
    };
    let url = isolated_url(&url, "t_stats");
    let mut store = buildlens_server_store(&url);
    let today = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as f64;
    for (index, seconds) in [100.0, 200.0, 300.0, 400.0, 500.0].iter().enumerate() {
        let machine = if index % 2 == 0 { "m1" } else { "m2" };
        store
            .insert(&build(&format!("s{index}"), *seconds, Some(today), Some(machine)))
            .unwrap();
    }
    let stats = store.stats(30).unwrap();
    let item = &stats["items"].as_array().unwrap()[0];
    assert_eq!(item["project"], "App");
    assert_eq!(item["builds"], 5);
    assert_eq!(item["p50"], 300.0);
    assert_eq!(item["machines"], 2);

    let slowest = store.slowest_targets(30, 5).unwrap();
    let target = &slowest["items"].as_array().unwrap()[0];
    assert_eq!(target["name"], "Core");
    assert_eq!(target["observations"], 5);
}

/// The store lives in the binary crate, so tests reach it through a thin
/// re-export module compiled into the test binary.
fn buildlens_server_store(url: &str) -> store::PostgresStore {
    store::PostgresStore::connect(url).expect("connect and migrate")
}

#[path = "../src/store.rs"]
mod store;

/// The `status` column is TEXT and `BuildStatus` is an enum, so the conversion
/// must round-trip through the same spelling serde writes. `None` must stay
/// NULL: an absent verdict reads as unknown, never as success.
#[test]
fn build_status_round_trips_through_the_text_column() {
    let Some(url) = database_url() else {
        eprintln!("skipping: BUILDLENS_TEST_DATABASE_URL not set");
        return;
    };
    let url = isolated_url(&url, "t_status");
    let mut store = buildlens_server_store(&url);

    let cases = [
        ("ok", Some(BuildStatus::Succeeded), Some("succeeded")),
        ("bad", Some(BuildStatus::Failed), Some("failed")),
        ("stop", Some(BuildStatus::Cancelled), Some("cancelled")),
        ("quiet", None, None),
    ];
    for (key, status, _) in cases {
        let mut wire = build(key, 10.0, Some(1_700_000_000.0), Some("m1"));
        wire.status = status;
        store.insert(&wire).unwrap();
    }

    let items = store.builds(10).unwrap();
    let found: std::collections::BTreeMap<String, Option<String>> = items["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["build_key"].as_str().unwrap().to_owned(),
                item["status"].as_str().map(str::to_owned),
            )
        })
        .collect();
    for (key, _, expected) in cases {
        assert_eq!(
            found[key].as_deref(),
            expected,
            "{key} stored the wrong verdict"
        );
    }
    assert_eq!(found["quiet"], None, "an absent verdict must stay NULL");
}

/// `build_detail` read column 13 as `raw_warnings` while never reading column
/// 12, so the error count was silently dropped and the warning count was
/// reported under the wrong name.
#[test]
fn build_detail_reports_both_diagnostic_counts() {
    let Some(url) = database_url() else {
        eprintln!("skipping: BUILDLENS_TEST_DATABASE_URL not set");
        return;
    };
    let url = isolated_url(&url, "t_detail");
    let mut store = buildlens_server_store(&url);

    let mut wire = build("counts", 10.0, Some(1_700_000_000.0), Some("m1"));
    wire.error_count = 7;
    wire.warning_count = 3;
    store.insert(&wire).unwrap();

    let detail = store.build_detail("counts").unwrap().expect("build exists");
    assert_eq!(detail["error_count"], 7, "the error count was being dropped");
    assert_eq!(detail["warning_count"], 3);
    // The child rows belong in the same call, or the page renders empty.
    assert_eq!(detail["targets"].as_array().unwrap().len(), 1);
    assert_eq!(detail["phases"].as_array().unwrap().len(), 1);
}

#[test]
fn build_detail_is_absent_for_an_unknown_key() {
    let Some(url) = database_url() else {
        eprintln!("skipping: BUILDLENS_TEST_DATABASE_URL not set");
        return;
    };
    let url = isolated_url(&url, "t_detail_missing");
    let mut store = buildlens_server_store(&url);
    assert!(store.build_detail("nope").unwrap().is_none());
}

/// `day > CURRENT_DATE - $1` excluded the boundary day, so days=N covered
/// N-1 days. A build exactly N days old must be included.
#[test]
fn the_day_window_is_inclusive_of_its_boundary() {
    let Some(url) = database_url() else {
        eprintln!("skipping: BUILDLENS_TEST_DATABASE_URL not set");
        return;
    };
    let url = isolated_url(&url, "t_window");
    let mut store = buildlens_server_store(&url);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as f64;
    // Exactly 7 days back, which the old strict comparison dropped.
    let seven_days_ago = now - 7.0 * 86_400.0;
    store
        .insert(&build("edge", 100.0, Some(seven_days_ago), Some("m1")))
        .unwrap();

    let stats = store.stats(7).unwrap();
    assert_eq!(
        stats["items"].as_array().unwrap().len(),
        1,
        "a build exactly 7 days old must fall inside a 7-day window"
    );
    let slowest = store.slowest_targets(7, 5).unwrap();
    assert_eq!(slowest["items"].as_array().unwrap().len(), 1);
}

/// The ranked panels scope to one project; without that, a shared server
/// mixes every team's targets into one list.
#[test]
fn ranked_queries_scope_to_a_project() {
    let Some(url) = database_url() else {
        eprintln!("skipping: BUILDLENS_TEST_DATABASE_URL not set");
        return;
    };
    let url = isolated_url(&url, "t_ranked");
    let mut store = buildlens_server_store(&url);

    let mut ours = build("ours", 100.0, Some(1_700_000_000.0), Some("m1"));
    ours.project = "Ours".into();
    store.insert(&ours).unwrap();

    let mut theirs = build("theirs", 100.0, Some(1_700_000_000.0), Some("m2"));
    theirs.project = "Theirs".into();
    theirs.targets[0].name = "TheirTarget".into();
    store.insert(&theirs).unwrap();

    let scoped = store.ranked_targets(100, 10, Some("Ours")).unwrap();
    let names: Vec<&str> = scoped["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["Core"], "another project's target leaked in");

    // No project means every project.
    let all = store.ranked_targets(100, 10, None).unwrap();
    assert_eq!(all["items"].as_array().unwrap().len(), 2);

    let phases = store.ranked_phases(100, 10, Some("Ours")).unwrap();
    assert_eq!(phases["items"].as_array().unwrap().len(), 1);
}

/// `enough_history` gates the dashboard tile so a percentile is never quoted
/// from a handful of builds.
#[test]
fn percentiles_report_whether_there_is_enough_history() {
    let Some(url) = database_url() else {
        eprintln!("skipping: BUILDLENS_TEST_DATABASE_URL not set");
        return;
    };
    let url = isolated_url(&url, "t_percentiles");
    let mut store = buildlens_server_store(&url);

    for index in 0..3 {
        store
            .insert(&build(
                &format!("p{index}"),
                100.0,
                Some(1_700_000_000.0),
                Some("m1"),
            ))
            .unwrap();
    }
    let thin = store.percentiles(100, None).unwrap();
    assert_eq!(thin["builds"], 3);
    assert_eq!(thin["enough_history"], false);

    for index in 3..8 {
        store
            .insert(&build(
                &format!("p{index}"),
                100.0,
                Some(1_700_000_000.0),
                Some("m1"),
            ))
            .unwrap();
    }
    let full = store.percentiles(100, None).unwrap();
    assert_eq!(full["builds"], 8);
    assert_eq!(full["enough_history"], true);
    assert_eq!(full["p50"], 100.0);
}

/// An empty database must answer every query with an empty result rather than
/// erroring or reporting a percentile over nothing.
#[test]
fn an_empty_database_answers_every_query() {
    let Some(url) = database_url() else {
        eprintln!("skipping: BUILDLENS_TEST_DATABASE_URL not set");
        return;
    };
    let url = isolated_url(&url, "t_empty");
    let mut store = buildlens_server_store(&url);

    assert_eq!(store.builds(10).unwrap()["items"].as_array().unwrap().len(), 0);
    assert_eq!(store.stats(30).unwrap()["items"].as_array().unwrap().len(), 0);
    assert_eq!(
        store.daily(30, None).unwrap()["items"].as_array().unwrap().len(),
        0
    );
    let percentiles = store.percentiles(100, None).unwrap();
    assert_eq!(percentiles["builds"], 0);
    assert_eq!(percentiles["enough_history"], false);
    assert!(percentiles["p50"].is_null(), "no builds means no percentile");
}

/// Migration runs on every connect, so a second connection to a database that
/// already has the schema must be a no-op rather than an error.
#[test]
fn migrating_an_existing_schema_is_idempotent() {
    let Some(url) = database_url() else {
        eprintln!("skipping: BUILDLENS_TEST_DATABASE_URL not set");
        return;
    };
    let url = isolated_url(&url, "t_migrate");
    let mut first = buildlens_server_store(&url);
    first
        .insert(&build("before", 10.0, Some(1_700_000_000.0), Some("m1")))
        .unwrap();
    // Reconnecting re-runs migrate() against a populated schema.
    let mut second = buildlens_server_store(&url);
    assert_eq!(
        second.builds(10).unwrap()["items"].as_array().unwrap().len(),
        1,
        "re-migrating must not disturb existing rows"
    );
}
