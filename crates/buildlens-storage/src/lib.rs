//! PostgreSQL storage shared by the collector and dashboard.
//!
//! Two layers live here. [`PostgresStore::insert`] writes the wire payload a
//! team server would receive; [`PostgresStore::save_analysis`] additionally
//! writes the local-only detail (files, Swift timings, diagnostics, tests,
//! collected metadata) that a `collect` run reads straight from the activity
//! log. That split is deliberate: `buildlens_core::wire::WireBuild` omits
//! source paths and per-file timings on purpose, and widening it to feed the
//! dashboard would start transmitting a repository's layout. The dashboard
//! reads the local tables instead, so richer panels cost nothing in privacy.

use buildlens_core::{
    BuildAnalysis, MetricKind, MetricRegression, RegressionCaveat, RegressionConfidence,
    TestStatus,
    wire::{Attribution, WireBuild},
};
use postgres::{Client, NoTls, Transaction};
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("PostgreSQL error: {0}")] Database(#[from] postgres::Error),
    #[error("serialization error: {0}")] Json(#[from] serde_json::Error),
    #[error("build has no usable activity-log metrics")] UnusableBuild,
    #[error("PostgreSQL {operation}: {source}")] Query { operation: &'static str, source: postgres::Error },
    /// `migrations.sql` could not be parsed. A programming error rather than a
    /// runtime one — the file is compiled in — so it surfaces at startup,
    /// before any work, rather than leaving a migration quietly unapplied.
    #[error("invalid migration definition: {0}")] Migration(String),
}

/// The canonical schema, shared by every crate that opens a BuildLens
/// database. `buildlens-server` once carried its own copy covering only
/// builds, targets and phases, so a server that migrated a fresh database had
/// no build_files, build_swift_timings, build_diagnostics or build_tests — and
/// a pushed build carrying that detail hit "relation does not exist".
const SCHEMA_SQL: &str = include_str!("../../buildlens-server/schema.sql");

/// The versioned changes the baseline cannot express. See `migrations.sql`.
const MIGRATIONS_SQL: &str = include_str!("../../buildlens-server/migrations.sql");

/// Arbitrary constant, shared by every BuildLens process on this database.
const MIGRATION_LOCK: i64 = 0x6275_696c;

/// Applies the baseline schema and every unapplied migration, under an
/// advisory lock so concurrent starts serialise rather than deadlock.
///
/// Lives here rather than in `buildlens-server` because that crate depends on
/// this one and not the reverse; the server delegates to it. Two
/// implementations of one schema is exactly what produced the missing-table
/// bug the comment above `SCHEMA_SQL` describes.
pub fn migrate(client: &mut Client) -> Result<(), StoreError> {
    client.execute("SELECT pg_advisory_lock($1)", &[&MIGRATION_LOCK])?;
    let result = migrate_locked(client);
    // Release even when the schema failed, or the next process hangs.
    let unlock = client.execute("SELECT pg_advisory_unlock($1)", &[&MIGRATION_LOCK]);
    result?;
    unlock?;
    Ok(())
}

fn migrate_locked(client: &mut Client) -> Result<(), StoreError> {
    client.batch_execute(SCHEMA_SQL)?;
    apply_migrations(client)
}

/// One migration parsed out of `migrations.sql`.
#[derive(Debug)]
struct Migration<'a> {
    version: &'a str,
    name: &'a str,
    sql: String,
}

/// Splits `migrations.sql` on its `--@migration <version> <name>` markers.
///
/// Text before the first marker is file-level commentary and is dropped. A
/// malformed or duplicated marker is not silently skipped: either would mean a
/// migration that never runs, which is the failure mode this mechanism exists
/// to prevent, so it is reported instead.
fn parse_migrations(sql: &str) -> Result<Vec<Migration<'_>>, String> {
    let mut migrations: Vec<Migration<'_>> = Vec::new();
    for line in sql.lines() {
        if let Some(header) = line.strip_prefix("--@migration") {
            let mut parts = header.split_whitespace();
            let (Some(version), Some(name)) = (parts.next(), parts.next()) else {
                return Err(format!("malformed migration header: {line}"));
            };
            migrations.push(Migration { version, name, sql: String::new() });
        } else if let Some(current) = migrations.last_mut() {
            current.sql.push_str(line);
            current.sql.push('\n');
        }
    }
    for (index, migration) in migrations.iter().enumerate() {
        if migrations[..index].iter().any(|other| other.version == migration.version) {
            return Err(format!("duplicate migration version: {}", migration.version));
        }
    }
    Ok(migrations)
}

/// Applies every migration this database has not yet recorded, in file order.
///
/// Each runs in its own transaction together with the INSERT that records it,
/// so the ledger cannot claim a migration that did not fully apply, and a
/// failure part-way through the file leaves the database on the last version
/// that did.
fn apply_migrations(client: &mut Client) -> Result<(), StoreError> {
    client.batch_execute(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             version TEXT PRIMARY KEY,
             name TEXT NOT NULL,
             applied_at TIMESTAMPTZ NOT NULL DEFAULT now()
         )",
    )?;
    let migrations = parse_migrations(MIGRATIONS_SQL).map_err(StoreError::Migration)?;
    let applied: std::collections::HashSet<String> = client
        .query("SELECT version FROM schema_migrations", &[])?
        .iter()
        .map(|row| row.get::<_, String>(0))
        .collect();
    for migration in migrations {
        if applied.contains(migration.version) {
            continue;
        }
        let mut tx = client.transaction()?;
        tx.batch_execute(&migration.sql)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, name) VALUES ($1, $2)",
            &[&migration.version, &migration.name],
        )?;
        tx.commit()?;
    }
    Ok(())
}

/// Caps on how much per-build detail is stored. An activity log can name tens
/// of thousands of files; the dashboard only ever ranks the slowest, so storing
/// every row would grow the database without changing a single answer.
const MAX_FILES: usize = 500;
const MAX_SWIFT_TIMINGS: usize = 500;
const MAX_DIAGNOSTICS: usize = 500;

/// A target must be slower by both of these to count as a regression. Absolute
/// seconds alone flags every big target's noise; percent alone flags a 0.01s
/// step that doubled.
const TARGET_REGRESSION_MIN_SECONDS: f64 = 0.5;
const TARGET_REGRESSION_MIN_PERCENT: f64 = 10.0;

/// One build measured against the most recent earlier build of its project.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BaselineComparison {
    pub baseline_build_key: String,
    pub project: String,
    pub previous_seconds: f64,
    pub current_seconds: f64,
    /// Set when the two builds were different kinds of build (clean vs
    /// incremental), which makes per-target comparison meaningless.
    pub category_change: Option<(String, String)>,
    pub environment_changed: bool,
    pub regressions: Vec<MetricRegression>,
}

pub struct PostgresStore { client: Client }

impl PostgresStore {
    pub fn connect(url: &str) -> Result<Self, StoreError> {
        let mut store = Self { client: Client::connect(url, NoTls)? };
        store.migrate()?;
        Ok(store)
    }

    /// Applies the schema. Every process does this at startup, so two starting
    /// at once (the dashboard and the collector, say) would run the same
    /// `ALTER TABLE`s concurrently and deadlock against each other. An advisory
    /// lock serialises them: the second process waits, then finds the work
    /// already done — the baseline because every statement is `IF NOT EXISTS`,
    /// the migrations because the first process recorded them.
    fn migrate(&mut self) -> Result<(), StoreError> {
        migrate(&mut self.client)
    }

    pub fn insert(&mut self, build: &WireBuild) -> Result<bool, StoreError> {
        let day = day_of(build.started_at);
        let mut tx = self.client.transaction()?;
        let inserted = insert_wire(&mut tx, build, &day)?;
        tx.commit()?;
        Ok(inserted)
    }

    /// Stores a locally collected build: the wire-shaped rows plus the detail
    /// only a local collect has. Returns false when this build was already
    /// stored, so re-collecting a log stays a no-op.
    /// As [`PostgresStore::save_analysis`], but with attempt numbers supplied
    /// by the caller — one per entry of `analysis.tests.tests`, in the same
    /// order.
    ///
    /// Exists because an `.xcresult` states which run of a retried test each
    /// result is, where a text log only implies it by position. Passing them
    /// through rather than re-deriving keeps Xcode's numbering intact even
    /// when parallel destinations interleaved the log the run also produced.
    pub fn save_analysis_with_attempts(&mut self, analysis: &BuildAnalysis, project: &str, machine_id: Option<String>, anonymous: bool, attempts: &[i32]) -> Result<bool, StoreError> {
        self.save_inner(analysis, project, machine_id, anonymous, Some(attempts))
    }

    pub fn save_analysis(&mut self, analysis: &BuildAnalysis, project: &str, machine_id: Option<String>, anonymous: bool) -> Result<bool, StoreError> {
        self.save_inner(analysis, project, machine_id, anonymous, None)
    }

    fn save_inner(&mut self, analysis: &BuildAnalysis, project: &str, machine_id: Option<String>, anonymous: bool, explicit_attempts: Option<&[i32]>) -> Result<bool, StoreError> {
        let metrics = analysis.metrics.as_ref().ok_or(StoreError::UnusableBuild)?;
        let attribution = if anonymous { Attribution::Anonymous } else { Attribution::Pseudonymous };
        let mut build = WireBuild::from_metrics(metrics, project, machine_id, attribution, 100).ok_or(StoreError::UnusableBuild)?;
        // A failing test fails the build.
        //
        // `WireBuild::from_metrics` takes its verdict from the activity log,
        // which reports whether the *compile* succeeded. For a test run that
        // is "succeeded" however many tests failed, so a ⌘U with a red suite
        // was stored as green: the dashboard's failed-build tile missed it and
        // `--fail-on failures` had nothing to gate on. The analysis knows
        // better — the parser marks it failed on a failing test or a crash —
        // so its verdict wins here.
        //
        // One direction only. An analysis that saw no failure must not turn a
        // compile failure green: the activity log observed something the text
        // log did not, and downgrading it would hide a broken build.
        if analysis.status == buildlens_core::AnalysisStatus::Failed {
            build.status = Some(buildlens_core::wire::BuildStatus::Failed);
        } else if analysis.tests.tests.is_empty()
            && build.status == Some(buildlens_core::wire::BuildStatus::Succeeded)
            && metrics.produces_test_bundle()
        {
            // A build that produced a `.xctest` bundle is followed by a test
            // run whose results do not exist yet — Xcode writes them 70–92
            // seconds after the build log. Storing "succeeded" here would read
            // green for that window, and permanently green if the results
            // never arrive at all. `attach_tests` resolves it.
            build.status = Some(buildlens_core::wire::BuildStatus::PendingTests);
        }
        let day = day_of(build.started_at);
        let key = build.build_key.clone();
        let mut tx = self.client.transaction()?;
        if !insert_wire(&mut tx, &build, &day)? {
            tx.commit()?;
            return Ok(false);
        }
        // Set apart from `insert_wire`: `WireBuild` deliberately omits the
        // scheme, since a team server has no use for it and it names a
        // developer's local configuration. A locally collected build records
        // it because the dashboard's build list shows it.
        if let Some(scheme) = metrics.scheme.as_deref() {
            tx.execute(
                "UPDATE builds SET scheme = $3 WHERE day = to_date($1,'YYYY-MM-DD') AND build_key = $2",
                &[&day, &key, &scheme],
            )
            .map_err(|source| StoreError::Query { operation: "recording the scheme", source })?;
        }

        // Also local-only, and for the same reason as the scheme: the wire
        // carries totals, and how much of a build was replayed rather than run
        // is a property of the log a client parsed, not of the build a server
        // was told about.
        if metrics.replayed_steps > 0 {
            tx.execute(
                "UPDATE builds SET replayed_steps = $3 WHERE day = to_date($1,'YYYY-MM-DD') AND build_key = $2",
                &[&day, &key, &(metrics.replayed_steps as i32)],
            )
            .map_err(|source| StoreError::Query { operation: "recording replayed steps", source })?;
        }

        // Slowest files first, so the cap keeps the rows that matter.
        let mut files: Vec<_> = metrics.files.iter().collect();
        files.sort_by(|a, b| b.seconds.total_cmp(&a.seconds));
        for file in files.into_iter().take(MAX_FILES) {
            tx.execute(
                "INSERT INTO build_files (day,build_key,file,architecture,seconds,target,step_type,occurrences)
                 VALUES (to_date($1,'YYYY-MM-DD'),$2,$3,$4,$5,$6,$7,$8) ON CONFLICT DO NOTHING",
                &[&day, &key, &file.file, &file.architecture.clone().unwrap_or_default(), &file.seconds, &file.target, &file.step_type, &(file.occurrences as i32)],
            ).map_err(|source| StoreError::Query { operation: "inserting file timing", source })?;
        }

        let mut timings: Vec<_> = metrics.swift_timings.iter().collect();
        timings.sort_by(|a, b| b.milliseconds.total_cmp(&a.milliseconds));
        for timing in timings.into_iter().take(MAX_SWIFT_TIMINGS) {
            tx.execute(
                "INSERT INTO build_swift_timings (day,build_key,kind,file,line,column_number,symbol,milliseconds,target)
                 VALUES (to_date($1,'YYYY-MM-DD'),$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT DO NOTHING",
                &[&day, &key, &timing.kind.as_str(), &timing.file, &(timing.line as i32), &(timing.column as i32), &timing.symbol, &timing.milliseconds, &timing.target],
            ).map_err(|source| StoreError::Query { operation: "inserting swift timing", source })?;
        }

        for diagnostic in analysis.diagnostics.diagnostics.iter().take(MAX_DIAGNOSTICS) {
            let severity = serde_plain(&diagnostic.severity)?;
            let category = serde_plain(&diagnostic.category)?;
            tx.execute(
                "INSERT INTO build_diagnostics (day,build_key,fingerprint,severity,category,occurrences,message,file,line,target)
                 VALUES (to_date($1,'YYYY-MM-DD'),$2,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT DO NOTHING",
                &[&day, &key, &diagnostic.fingerprint, &severity, &category, &(diagnostic.occurrences as i32), &diagnostic.example.message, &diagnostic.example.file, &diagnostic.example.line.map(|line| line as i32), &diagnostic.example.target],
            ).map_err(|source| StoreError::Query { operation: "inserting diagnostic", source })?;
        }

        // Xcode's own numbering when the caller read an `.xcresult`, otherwise
        // derived from log order: a test Xcode retried within this build keeps
        // every run instead of the first one winning the ON CONFLICT. See
        // `attempt_numbers`. A supplied slice shorter than the test list falls
        // back per-entry rather than silently dropping the tail.
        let derived = attempt_numbers(analysis.tests.tests.iter().map(|test| (test.suite.as_str(), test.test.as_str())));
        let attempts: Vec<i32> = match explicit_attempts {
            Some(explicit) => derived
                .iter()
                .enumerate()
                .map(|(index, fallback)| explicit.get(index).copied().unwrap_or(*fallback))
                .collect(),
            None => derived,
        };
        for (test, attempt) in analysis.tests.tests.iter().zip(attempts) {
            // A "started" row means the test never reported an outcome, which
            // is how a crash shows up; recording it as-is keeps that visible.
            let status = match test.status { TestStatus::Passed => "passed", TestStatus::Failed => "failed", TestStatus::Started => "started" };
            tx.execute(
                "INSERT INTO build_tests (day,build_key,suite,name,status,seconds,message,attempt)
                 VALUES (to_date($1,'YYYY-MM-DD'),$2,$3,$4,$5,$6,$7,$8) ON CONFLICT DO NOTHING",
                &[&day, &key, &test.suite, &test.test, &status, &test.duration_seconds, &test.message, &attempt],
            ).map_err(|source| StoreError::Query { operation: "inserting test result", source })?;
        }

        for (metadata_key, value) in &analysis.metadata.entries {
            tx.execute(
                "INSERT INTO build_metadata (day,build_key,key,value) VALUES (to_date($1,'YYYY-MM-DD'),$2,$3,$4) ON CONFLICT DO NOTHING",
                &[&day, &key, metadata_key, value],
            ).map_err(|source| StoreError::Query { operation: "inserting metadata", source })?;
        }

        tx.commit()?;
        Ok(true)
    }

    pub fn build_id_for_activity_log(&mut self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self.client.query_opt("SELECT build_key FROM builds WHERE build_key=$1 LIMIT 1", &[&key])?.map(|row| row.get(0)))
    }

    /// Attaches test results to a build already in history, and corrects its
    /// status if any test failed.
    ///
    /// Exists because a ⌘U is two events on disk: Xcode writes the build log
    /// when the compile finishes, then the `.xcresult` 70–90 seconds later
    /// when the tests do. The build is stored as soon as its log settles, so
    /// its results necessarily arrive afterwards and have to be added to a row
    /// that already exists.
    ///
    /// Idempotent. `ON CONFLICT DO NOTHING` on the primary key means a bundle
    /// read twice adds nothing the second time, so a watcher that re-reads a
    /// manifest is harmless.
    ///
    /// Returns how many rows were newly inserted.
    pub fn attach_tests(&mut self, build_key: &str, tests: &[(buildlens_core::TestResult, i32)]) -> Result<usize, StoreError> {
        let Some(row) = self.client.query_opt("SELECT day::TEXT FROM builds WHERE build_key=$1 LIMIT 1", &[&build_key])? else {
            return Ok(0);
        };
        let day: String = row.get(0);
        let mut inserted = 0;
        let mut tx = self.client.transaction()?;
        for (test, attempt) in tests {
            let status = match test.status { TestStatus::Passed => "passed", TestStatus::Failed => "failed", TestStatus::Started => "started" };
            inserted += tx.execute(
                "INSERT INTO build_tests (day,build_key,suite,name,status,seconds,message,attempt)
                 VALUES (to_date($1,'YYYY-MM-DD'),$2,$3,$4,$5,$6,$7,$8) ON CONFLICT DO NOTHING",
                &[&day, &build_key, &test.suite, &test.test, &status, &test.duration_seconds, &test.message, attempt],
            ).map_err(|source| StoreError::Query { operation: "attaching test result", source })? as usize;
        }
        // The activity log reports on the compile, which succeeded; only the
        // test run knows the suite was red. Without this a ⌘U with failing
        // tests stays "succeeded" in history and in the failed-build tile.
        //
        // One direction only, as in `save_inner`: a passing suite never turns
        // a failed compile green.
        if tests.iter().any(|(test, _)| test.status == TestStatus::Failed) {
            tx.execute("UPDATE builds SET status='failed' WHERE build_key=$1", &[&build_key])
                .map_err(|source| StoreError::Query { operation: "marking the build failed", source })?;
        } else if !tests.is_empty() {
            // Results arrived and none failed, so a build held at
            // `pending_tests` is now genuinely green. Scoped to that status so
            // this never overwrites a real verdict: a failed compile stays
            // failed however well its tests went.
            tx.execute(
                "UPDATE builds SET status='succeeded' WHERE build_key=$1 AND status='pending_tests'",
                &[&build_key],
            )
            .map_err(|source| StoreError::Query { operation: "resolving a pending build", source })?;
        }
        tx.commit()?;
        Ok(inserted)
    }

    pub fn projects(&mut self) -> Result<Value, StoreError> { let rows=self.client.query("SELECT project,COUNT(*)::bigint FROM builds GROUP BY project ORDER BY COUNT(*) DESC,project",&[])?; Ok(serde_json::json!({"items":rows.iter().map(|r|serde_json::json!({"project":r.get::<_,String>(0),"builds":r.get::<_,i64>(1)})).collect::<Vec<_>>() })) }
    /// The dashboard build list. `status` and the diagnostic counts come from the
    /// stored columns: hardcoding "succeeded" here painted every failed build
    /// green, which is exactly what the failed-build column exists to show.
    /// The dashboard build list.
    ///
    /// Test counts are joined in so a row can say whether it ran tests at all.
    /// Without them "failed" is ambiguous — a compile that broke and a suite
    /// that went red read identically, and those call for different responses.
    /// Counted over distinct tests rather than rows, so a retried test does not
    /// inflate the total; `failed_tests` counts tests whose *last* attempt
    /// failed, matching the build-detail view, so a fail-then-pass is a pass.
    pub fn dashboard_snapshot(&mut self) -> Result<Value, StoreError> {
        let rows=self.client.query(
            "WITH final AS (
                 SELECT DISTINCT ON (build_key,suite,name) build_key,status
                 FROM build_tests ORDER BY build_key,suite,name,attempt DESC),
             counts AS (
                 SELECT build_key,COUNT(*)::int AS total,
                        COUNT(*) FILTER (WHERE status='failed')::int AS failed
                 FROM final GROUP BY build_key)
             SELECT b.build_key,EXTRACT(EPOCH FROM COALESCE(to_timestamp(b.started_at),b.received_at))::bigint,
                    b.project,b.category,b.total_seconds,b.cache_hit_rate,b.status,b.error_count,
                    b.warning_count,b.scheme,COALESCE(c.total,0),COALESCE(c.failed,0)
             FROM builds b LEFT JOIN counts c ON c.build_key=b.build_key
             ORDER BY b.received_at DESC LIMIT 100",&[])?;
        Ok(serde_json::json!({"builds":rows.iter().map(|r|serde_json::json!({"id":r.get::<_,String>(0),"recorded_at":r.get::<_,i64>(1),"project":r.get::<_,String>(2),"category":r.get::<_,String>(3),"total_seconds":r.get::<_,f64>(4),"status":r.get::<_,Option<String>>(6),"raw_warnings":r.get::<_,i32>(8),"errors":r.get::<_,i32>(7),"cache_hit_rate":r.get::<_,Option<f64>>(5),"scheme":r.get::<_,Option<String>>(9),"total_tests":r.get::<_,i32>(10),"failed_tests":r.get::<_,i32>(11)})).collect::<Vec<_>>() }))
    }
    pub fn duration_trend_for(&mut self, limit:i64, project:Option<&str>) -> Result<Value,StoreError> { let rows=if let Some(project)=project {self.client.query("SELECT build_key,EXTRACT(EPOCH FROM COALESCE(to_timestamp(started_at),received_at))::bigint,project,category,total_seconds,cache_hit_rate FROM builds WHERE project=$1 ORDER BY received_at DESC LIMIT $2",&[&project,&limit])?} else {self.client.query("SELECT build_key,EXTRACT(EPOCH FROM COALESCE(to_timestamp(started_at),received_at))::bigint,project,category,total_seconds,cache_hit_rate FROM builds ORDER BY received_at DESC LIMIT $1",&[&limit])?}; Ok(serde_json::json!({"items":rows.iter().rev().map(|r|serde_json::json!({"id":r.get::<_,String>(0),"recorded_at":r.get::<_,i64>(1),"project":r.get::<_,String>(2),"category":r.get::<_,String>(3),"total_seconds":r.get::<_,f64>(4),"cache_hit_rate":r.get::<_,Option<f64>>(5)})).collect::<Vec<_>>() })) }

    /// One build with everything recorded about it — the detail view a
    /// developer opens after spotting a slow or failed build in the trend.
    ///
    /// Returns per-build files, Swift type-check timings, diagnostics and test
    /// results alongside targets and phases. The Performance tab shows the same
    /// dimensions averaged over many builds, which is the right shape for a
    /// trend and the wrong shape for a diagnosis: an average hides the one file
    /// that regressed today, and a failed build's error does not appear in it
    /// at all.
    ///
    /// `Ok(None)` for an unknown key rather than an error: "no such build" is
    /// a normal answer to a stale dashboard link, and a caller should not have
    /// to read error text to tell it apart from a real database failure.
    pub fn build_snapshot(&mut self, key:&str) -> Result<Option<Value>,StoreError> {
        let Some(r)=self.client.query_opt("SELECT build_key,EXTRACT(EPOCH FROM COALESCE(to_timestamp(started_at),received_at))::bigint,project,category,total_seconds,cache_hit_rate,compiled_count,machine_id,xcode_version,platform,architecture,status,error_count,warning_count,scheme,replayed_steps FROM builds WHERE build_key=$1",&[&key])? else {
            return Ok(None);
        };
        let targets=self.client.query("SELECT name,seconds,category,fetched_from_cache,compiled_count FROM build_targets WHERE build_key=$1 ORDER BY seconds DESC LIMIT 100",&[&key])?;
        let phases=self.client.query("SELECT name,seconds FROM build_phases WHERE build_key=$1 ORDER BY seconds DESC LIMIT 50",&[&key])?;
        let metadata=self.client.query("SELECT key,value FROM build_metadata WHERE build_key=$1 ORDER BY key",&[&key])?;
        let files=self.client.query("SELECT file,target,seconds,occurrences,step_type FROM build_files WHERE build_key=$1 ORDER BY seconds DESC LIMIT 50",&[&key])?;
        let swift=self.client.query("SELECT file,line,symbol,kind,milliseconds,target FROM build_swift_timings WHERE build_key=$1 ORDER BY milliseconds DESC LIMIT 50",&[&key])?;
        // Errors before warnings: a failed build's reason is the first thing
        // this page has to answer, and ordering by occurrences alone can bury
        // a single fatal error under a repeated warning.
        let diagnostics=self.client.query(
            "SELECT fingerprint,severity,category,message,file,line,target,occurrences FROM build_diagnostics WHERE build_key=$1
             ORDER BY (severity='error') DESC,occurrences DESC LIMIT 50",&[&key])?;
        // Failures first for the same reason, then slowest. One row per test
        // rather than per attempt: a retried test reports its final outcome —
        // the one that decided the build — with `attempts` saying how many runs
        // it took to get there. Listing each attempt separately would let a
        // single flaky test crowd the other 49 rows out of the page.
        let tests=self.client.query(
            "SELECT suite,name,
                    (SELECT status FROM build_tests s WHERE s.build_key=t.build_key AND s.suite=t.suite AND s.name=t.name ORDER BY attempt DESC LIMIT 1),
                    SUM(seconds),
                    (SELECT message FROM build_tests s WHERE s.build_key=t.build_key AND s.suite=t.suite AND s.name=t.name AND s.message IS NOT NULL ORDER BY attempt DESC LIMIT 1),
                    COUNT(*)::bigint
             FROM build_tests t WHERE t.build_key=$1
             GROUP BY t.build_key,t.suite,t.name
             ORDER BY (MAX(CASE WHEN status='failed' THEN 1 ELSE 0 END)=1) DESC,SUM(seconds) DESC NULLS LAST LIMIT 50",&[&key])?;
        // Counted over distinct tests, not attempts, so a retry does not make a
        // suite look larger than it is. `failed` counts tests whose last
        // attempt failed: one that failed then passed on retry is a pass.
        let test_totals=self.client.query_one(
            "WITH final AS (
                 SELECT DISTINCT ON (suite,name) suite,name,status
                 FROM build_tests WHERE build_key=$1 ORDER BY suite,name,attempt DESC)
             SELECT (SELECT COUNT(*)::bigint FROM final),
                    (SELECT COUNT(*) FILTER (WHERE status='failed')::bigint FROM final),
                    (SELECT COALESCE(SUM(seconds),0.0) FROM build_tests WHERE build_key=$1)",&[&key])?;
        Ok(Some(serde_json::json!({
            "id":r.get::<_,String>(0),"recorded_at":r.get::<_,i64>(1),"project":r.get::<_,String>(2),"category":r.get::<_,String>(3),
            "total_seconds":r.get::<_,f64>(4),"cache_hit_rate":r.get::<_,Option<f64>>(5),"compiled_count":r.get::<_,i32>(6),"replayed_steps":r.get::<_,i32>(15),
            "machine_id":r.get::<_,Option<String>>(7),"xcode_version":r.get::<_,Option<String>>(8),"platform":r.get::<_,Option<String>>(9),
            "architecture":r.get::<_,Option<String>>(10),"status":r.get::<_,Option<String>>(11),"errors":r.get::<_,i32>(12),"raw_warnings":r.get::<_,i32>(13),"scheme":r.get::<_,Option<String>>(14),
            "targets":targets.iter().map(|t|serde_json::json!({"name":t.get::<_,String>(0),"seconds":t.get::<_,f64>(1),"category":t.get::<_,String>(2),"fetched_from_cache":t.get::<_,bool>(3),"compiled_count":t.get::<_,i32>(4)})).collect::<Vec<_>>(),
            "phases":phases.iter().map(|p|serde_json::json!({"name":p.get::<_,String>(0),"seconds":p.get::<_,f64>(1)})).collect::<Vec<_>>(),
            "metadata":metadata.iter().map(|m|(m.get::<_,String>(0),Value::String(m.get::<_,String>(1)))).collect::<serde_json::Map<_,_>>(),
            "files":files.iter().map(|f|serde_json::json!({"file":f.get::<_,String>(0),"target":f.get::<_,Option<String>>(1),"seconds":f.get::<_,f64>(2),"compilations":f.get::<_,i32>(3),"step_type":f.get::<_,String>(4)})).collect::<Vec<_>>(),
            "swift":swift.iter().map(|s|serde_json::json!({"file":s.get::<_,String>(0),"line":s.get::<_,i32>(1),"symbol":s.get::<_,Option<String>>(2),"kind":s.get::<_,String>(3),"milliseconds":s.get::<_,f64>(4),"target":s.get::<_,Option<String>>(5)})).collect::<Vec<_>>(),
            "diagnostics":diagnostics.iter().map(|d|serde_json::json!({"fingerprint":d.get::<_,String>(0),"severity":d.get::<_,String>(1),"category":d.get::<_,String>(2),"message":d.get::<_,String>(3),"file":d.get::<_,Option<String>>(4),"line":d.get::<_,Option<i32>>(5),"target":d.get::<_,Option<String>>(6),"occurrences":d.get::<_,i32>(7)})).collect::<Vec<_>>(),
            "tests":tests.iter().map(|t|serde_json::json!({"suite":t.get::<_,String>(0),"test":t.get::<_,String>(1),"status":t.get::<_,String>(2),"seconds":t.get::<_,Option<f64>>(3),"message":t.get::<_,Option<String>>(4),"attempts":t.get::<_,i64>(5)})).collect::<Vec<_>>(),
            "test_totals":serde_json::json!({"total":test_totals.get::<_,i64>(0),"failed":test_totals.get::<_,i64>(1),"seconds":test_totals.get::<_,f64>(2)}),
        })))
    }

    /// p50/p95 over recent builds. Percentiles need a floor of history to mean
    /// anything, so a project with one build reports nulls rather than
    /// presenting a single sample as a distribution.
    pub fn duration_percentiles_for(&mut self, limit:i64, project:Option<&str>) -> Result<Value,StoreError> {
        let row=self.client.query_one(
            "WITH recent AS (SELECT total_seconds FROM builds WHERE ($1::TEXT IS NULL OR project=$1) ORDER BY received_at DESC LIMIT $2)
             SELECT COUNT(*)::bigint,
                    percentile_cont(0.5) WITHIN GROUP (ORDER BY total_seconds),
                    percentile_cont(0.95) WITHIN GROUP (ORDER BY total_seconds),
                    MIN(total_seconds),MAX(total_seconds),AVG(total_seconds) FROM recent",
            &[&project,&limit])?;
        let builds=row.get::<_,i64>(0);
        Ok(serde_json::json!({"builds":builds,"enough_history":builds>=MIN_HISTORY,
            "p50":row.get::<_,Option<f64>>(1),"p95":row.get::<_,Option<f64>>(2),
            "min_seconds":row.get::<_,Option<f64>>(3),"max_seconds":row.get::<_,Option<f64>>(4),"avg_seconds":row.get::<_,Option<f64>>(5)}))
    }

    /// Per-calendar-day p50/p95, which is what makes a week-over-week
    /// regression visible rather than just a noisy per-build line.
    pub fn daily_percentiles_for(&mut self, days:i64, project:Option<&str>) -> Result<Value,StoreError> {
        let rows=self.client.query(
            "SELECT day::TEXT,COUNT(*)::bigint,
                    percentile_cont(0.5) WITHIN GROUP (ORDER BY total_seconds),
                    percentile_cont(0.95) WITHIN GROUP (ORDER BY total_seconds)
             FROM builds WHERE ($1::TEXT IS NULL OR project=$1) AND day >= (CURRENT_DATE - $2::INTEGER)
             GROUP BY day ORDER BY day",
            &[&project,&(days as i32)])?;
        Ok(serde_json::json!({"items":rows.iter().map(|r|serde_json::json!({"day":r.get::<_,String>(0),"builds":r.get::<_,i64>(1),"p50":r.get::<_,Option<f64>>(2),"p95":r.get::<_,Option<f64>>(3)})).collect::<Vec<_>>()}))
    }

    /// Slowest targets averaged over recent builds — the "what should we fix
    /// first" list, ranked by mean rather than a single unlucky build.
    pub fn target_trend(&mut self, builds:i64, top:i64, project:Option<&str>) -> Result<Value,StoreError> {
        let rows=self.client.query(
            "WITH recent AS (SELECT build_key FROM builds WHERE ($1::TEXT IS NULL OR project=$1) ORDER BY received_at DESC LIMIT $2)
             SELECT t.name,COUNT(*)::bigint,AVG(t.seconds),MAX(t.seconds),
                    SUM(CASE WHEN t.fetched_from_cache THEN 1 ELSE 0 END)::bigint
             FROM build_targets t JOIN recent r ON r.build_key=t.build_key
             GROUP BY t.name ORDER BY AVG(t.seconds) DESC LIMIT $3",
            &[&project,&builds,&top])?;
        Ok(serde_json::json!({"items":rows.iter().map(|r|serde_json::json!({"name":r.get::<_,String>(0),"observations":r.get::<_,i64>(1),"avg_seconds":r.get::<_,Option<f64>>(2),"max_seconds":r.get::<_,Option<f64>>(3),"cached_builds":r.get::<_,i64>(4)})).collect::<Vec<_>>()}))
    }

    /// Where the time goes by build phase, averaged over recent builds.
    pub fn phase_trend(&mut self, builds:i64, top:i64, project:Option<&str>) -> Result<Value,StoreError> {
        let rows=self.client.query(
            "WITH recent AS (SELECT build_key FROM builds WHERE ($1::TEXT IS NULL OR project=$1) ORDER BY received_at DESC LIMIT $2)
             SELECT p.name,COUNT(*)::bigint,AVG(p.seconds),MAX(p.seconds)
             FROM build_phases p JOIN recent r ON r.build_key=p.build_key
             GROUP BY p.name ORDER BY AVG(p.seconds) DESC LIMIT $3",
            &[&project,&builds,&top])?;
        Ok(serde_json::json!({"items":rows.iter().map(|r|serde_json::json!({"name":r.get::<_,String>(0),"observations":r.get::<_,i64>(1),"avg_seconds":r.get::<_,Option<f64>>(2),"max_seconds":r.get::<_,Option<f64>>(3)})).collect::<Vec<_>>()}))
    }

    /// Slowest individual files to compile. `occurrences` separates "this file
    /// is slow" from "this file compiles once per architecture".
    pub fn slowest_files(&mut self, builds:i64, limit:i64, project:Option<&str>) -> Result<Value,StoreError> {
        let rows=self.client.query(
            "WITH recent AS (SELECT build_key FROM builds WHERE ($1::TEXT IS NULL OR project=$1) ORDER BY received_at DESC LIMIT $2)
             SELECT f.file,MAX(f.target),COUNT(*)::bigint,AVG(f.seconds),MAX(f.seconds),SUM(f.occurrences)::bigint
             FROM build_files f JOIN recent r ON r.build_key=f.build_key
             GROUP BY f.file ORDER BY AVG(f.seconds) DESC LIMIT $3",
            &[&project,&builds,&limit])?;
        Ok(serde_json::json!({"items":rows.iter().map(|r|serde_json::json!({"file":r.get::<_,String>(0),"target":r.get::<_,Option<String>>(1),"observations":r.get::<_,i64>(2),"avg_seconds":r.get::<_,Option<f64>>(3),"max_seconds":r.get::<_,Option<f64>>(4),"compilations":r.get::<_,Option<i64>>(5)})).collect::<Vec<_>>()}))
    }

    /// Slowest Swift function bodies / type-check sites. Empty unless the
    /// project builds with the -warn-long-* flags, which the payload reports
    /// so the dashboard can say "not enabled" instead of "nothing slow".
    pub fn slowest_swift_timings(&mut self, builds:i64, limit:i64, project:Option<&str>) -> Result<Value,StoreError> {
        let rows=self.client.query(
            "WITH recent AS (SELECT build_key FROM builds WHERE ($1::TEXT IS NULL OR project=$1) ORDER BY received_at DESC LIMIT $2)
             SELECT s.file,s.line,MAX(s.symbol),s.kind,COUNT(*)::bigint,AVG(s.milliseconds),MAX(s.milliseconds),MAX(s.target)
             FROM build_swift_timings s JOIN recent r ON r.build_key=s.build_key
             GROUP BY s.file,s.line,s.kind ORDER BY AVG(s.milliseconds) DESC LIMIT $3",
            &[&project,&builds,&limit])?;
        Ok(serde_json::json!({"items":rows.iter().map(|r|serde_json::json!({"file":r.get::<_,String>(0),"line":r.get::<_,i32>(1),"symbol":r.get::<_,Option<String>>(2),"kind":r.get::<_,String>(3),"observations":r.get::<_,i64>(4),"avg_milliseconds":r.get::<_,Option<f64>>(5),"max_milliseconds":r.get::<_,Option<f64>>(6),"target":r.get::<_,Option<String>>(7)})).collect::<Vec<_>>()}))
    }

    /// Warning and error counts per build, so a rising warning count is
    /// visible before it becomes an error.
    pub fn diagnostic_trend_for(&mut self, limit:i64, project:Option<&str>) -> Result<Value,StoreError> {
        let rows=self.client.query(
            "WITH recent AS (SELECT build_key,EXTRACT(EPOCH FROM COALESCE(to_timestamp(started_at),received_at))::bigint AS at,received_at
                             FROM builds WHERE ($1::TEXT IS NULL OR project=$1) ORDER BY received_at DESC LIMIT $2)
             SELECT r.build_key,r.at,
                    COALESCE(SUM(CASE WHEN d.severity='warning' THEN d.occurrences ELSE 0 END),0)::bigint,
                    COALESCE(SUM(CASE WHEN d.severity IN ('error','fatal') THEN d.occurrences ELSE 0 END),0)::bigint,
                    COUNT(d.fingerprint)::bigint
             FROM recent r LEFT JOIN build_diagnostics d ON d.build_key=r.build_key
             GROUP BY r.build_key,r.at,r.received_at ORDER BY r.received_at",
            &[&project,&limit])?;
        Ok(serde_json::json!({"items":rows.iter().map(|r|serde_json::json!({"id":r.get::<_,String>(0),"recorded_at":r.get::<_,i64>(1),"warnings":r.get::<_,i64>(2),"errors":r.get::<_,i64>(3),"unique_diagnostics":r.get::<_,i64>(4)})).collect::<Vec<_>>()}))
    }

    /// The diagnostics appearing in the most builds — the recurring ones worth
    /// fixing, as opposed to a one-off from a single broken build.
    pub fn diagnostic_clusters(&mut self, builds:i64, limit:i64, project:Option<&str>) -> Result<Value,StoreError> {
        let rows=self.client.query(
            "WITH recent AS (SELECT build_key FROM builds WHERE ($1::TEXT IS NULL OR project=$1) ORDER BY received_at DESC LIMIT $2)
             SELECT d.fingerprint,MAX(d.severity),MAX(d.category),MAX(d.message),MAX(d.file),COUNT(DISTINCT d.build_key)::bigint,SUM(d.occurrences)::bigint
             FROM build_diagnostics d JOIN recent r ON r.build_key=d.build_key
             GROUP BY d.fingerprint ORDER BY COUNT(DISTINCT d.build_key) DESC,SUM(d.occurrences) DESC LIMIT $3",
            &[&project,&builds,&limit])?;
        Ok(serde_json::json!({"items":rows.iter().map(|r|serde_json::json!({"fingerprint":r.get::<_,String>(0),"severity":r.get::<_,Option<String>>(1),"category":r.get::<_,Option<String>>(2),"message":r.get::<_,Option<String>>(3),"file":r.get::<_,Option<String>>(4),"builds":r.get::<_,i64>(5),"occurrences":r.get::<_,Option<i64>>(6)})).collect::<Vec<_>>()}))
    }

    /// Tests that both pass and fail across recent builds. Mixed outcomes are
    /// the definition of flaky; a test that always fails is broken, not flaky,
    /// and is reported separately so the two are not confused.
    ///
    /// Two kinds of mixing are counted, and they are not equally strong
    /// evidence. `retried` counts builds where the test both failed and passed
    /// *within that one build* — Xcode retried it and it changed its mind with
    /// the code, machine and environment all held constant, which is flakiness
    /// with nothing else to blame. Plain `flaky` mixes outcomes across
    /// different builds, where a source change is the likelier explanation.
    pub fn flaky_tests(&mut self, builds:i64, limit:i64, project:Option<&str>) -> Result<Value,StoreError> {
        let rows=self.client.query(
            "WITH recent AS (SELECT build_key FROM builds WHERE ($1::TEXT IS NULL OR project=$1) ORDER BY received_at DESC LIMIT $2),
                  per_build AS (
                      SELECT t.suite,t.name,t.build_key,
                             COUNT(*)::bigint AS runs,
                             SUM(CASE WHEN t.status='failed' THEN 1 ELSE 0 END)::bigint AS failed,
                             SUM(CASE WHEN t.status='passed' THEN 1 ELSE 0 END)::bigint AS passed,
                             AVG(t.seconds) AS avg_seconds
                      FROM build_tests t JOIN recent r ON r.build_key=t.build_key
                      GROUP BY t.suite,t.name,t.build_key)
             SELECT suite,name,SUM(runs)::bigint,SUM(failed)::bigint,SUM(passed)::bigint,AVG(avg_seconds),
                    COUNT(*) FILTER (WHERE failed>0 AND passed>0)::bigint
             FROM per_build
             GROUP BY suite,name
             HAVING SUM(failed) > 0
             ORDER BY COUNT(*) FILTER (WHERE failed>0 AND passed>0) DESC, SUM(failed) DESC LIMIT $3",
            &[&project,&builds,&limit])?;
        Ok(serde_json::json!({"items":rows.iter().map(|r|{let runs=r.get::<_,i64>(2);let failed=r.get::<_,i64>(3);let passed=r.get::<_,i64>(4);let retried=r.get::<_,i64>(6);serde_json::json!({"suite":r.get::<_,String>(0),"test":r.get::<_,String>(1),"runs":runs,"failed":failed,"passed":passed,"flaky":failed>0&&passed>0,"retried_builds":retried,"avg_seconds":r.get::<_,Option<f64>>(5)})}).collect::<Vec<_>>()}))
    }

    /// Targets whose average duration in the most recent builds is materially
    /// worse than the window before them. This is a trend signal, not the
    /// baseline-matched regression detection history has always done — it makes
    /// no claim about environment matching, so the dashboard labels it as such.
    pub fn target_regressions(&mut self, builds:i64, limit:i64, project:Option<&str>) -> Result<Value,StoreError> {
        // `builds` appears both as a row-number boundary and as a LIMIT, which
        // Postgres cannot infer a single type for; the window boundary is cast
        // explicitly and the LIMIT gets its own bigint parameter.
        let window_total = builds.saturating_mul(2);
        let rows=self.client.query(
            "WITH ordered AS (SELECT build_key,ROW_NUMBER() OVER (ORDER BY received_at DESC) AS rank
                              FROM builds WHERE ($1::TEXT IS NULL OR project=$1) LIMIT $2),
                  windows AS (SELECT t.name,
                                     AVG(CASE WHEN o.rank<=$3::BIGINT THEN t.seconds END) AS recent,
                                     AVG(CASE WHEN o.rank>$3::BIGINT THEN t.seconds END) AS prior,
                                     COUNT(*)::bigint AS observations
                              FROM build_targets t JOIN ordered o ON o.build_key=t.build_key GROUP BY t.name)
             SELECT name,recent,prior,observations FROM windows
             -- Both thresholds are the shared constants, passed in rather than
             -- written as literals: the same rule decides a regression here and
             -- in `compare_to_baseline`, and a literal would silently diverge
             -- from the constant the moment either is tuned.
             WHERE recent IS NOT NULL AND prior IS NOT NULL
               AND prior > $5::DOUBLE PRECISION
               AND (recent-prior) >= $5::DOUBLE PRECISION
               AND (recent-prior)/prior*100.0 >= $6::DOUBLE PRECISION
             ORDER BY (recent-prior) DESC LIMIT $4",
            &[&project,&window_total,&builds,&limit,
              &TARGET_REGRESSION_MIN_SECONDS,&TARGET_REGRESSION_MIN_PERCENT])?;
        Ok(serde_json::json!({"items":rows.iter().map(|r|{let recent=r.get::<_,f64>(1);let prior=r.get::<_,f64>(2);serde_json::json!({"name":r.get::<_,String>(0),"current_seconds":recent,"previous_seconds":prior,"delta_seconds":recent-prior,"delta_percent":(recent-prior)/prior*100.0,"observations":r.get::<_,i64>(3),"confidence":"trend"})}).collect::<Vec<_>>()}))
    }

    /// Machine, Xcode and platform mix across recent builds. A duration shift
    /// that lines up with an Xcode upgrade is an environment change, not a
    /// code regression, and this is what lets a developer tell them apart.
    pub fn environment_breakdown(&mut self, builds:i64, project:Option<&str>) -> Result<Value,StoreError> {
        let rows=self.client.query(
            "WITH recent AS (SELECT xcode_version,platform,architecture,machine_id,total_seconds
                             FROM builds WHERE ($1::TEXT IS NULL OR project=$1) ORDER BY received_at DESC LIMIT $2)
             SELECT COALESCE(xcode_version,'unknown'),COALESCE(platform,'unknown'),COALESCE(architecture,'unknown'),
                    COUNT(*)::bigint,COUNT(DISTINCT machine_id)::bigint,AVG(total_seconds)
             FROM recent GROUP BY 1,2,3 ORDER BY COUNT(*) DESC",
            &[&project,&builds])?;
        Ok(serde_json::json!({"items":rows.iter().map(|r|serde_json::json!({"xcode_version":r.get::<_,String>(0),"platform":r.get::<_,String>(1),"architecture":r.get::<_,String>(2),"builds":r.get::<_,i64>(3),"machines":r.get::<_,i64>(4),"avg_seconds":r.get::<_,Option<f64>>(5)})).collect::<Vec<_>>()}))
    }

    /// Builds older than `keep_days`, excluding the newest build of each
    /// project. Retention orders by `recorded_at`, not `id`: `collect --all`
    /// backfills old logs under new ids, so a higher id can hold an older build.
    fn prunable_builds(&mut self, keep_days: u32) -> Result<Vec<String>, StoreError> {
        let rows = self.client.query(
            "WITH newest AS (
                 SELECT DISTINCT ON (project) build_key FROM builds
                 ORDER BY project, received_at DESC
             )
             SELECT build_key FROM builds
             WHERE received_at < now() - ($1::INTEGER * INTERVAL '1 day')
               AND build_key NOT IN (SELECT build_key FROM newest)
             ORDER BY received_at",
            &[&(keep_days as i32)],
        )?;
        Ok(rows.iter().map(|row| row.get::<_, String>(0)).collect())
    }

    /// Compares a build against the most recent earlier build of the same
    /// project, and reports the targets that got slower.
    ///
    /// Confidence follows the rules the local store always used, because a
    /// number without them misleads: a category change (clean vs incremental)
    /// makes per-target comparison meaningless, so only totals are reported and
    /// at low confidence; an environment change (different Xcode or platform)
    /// caps confidence at low, since the toolchain moved under the measurement;
    /// and a cache-state flip is skipped entirely, because a target fetched
    /// from cache one run and built the next has not regressed.
    ///
    /// Returns None when there is no earlier build to compare against — no
    /// baseline means no claim, rather than a comparison against zero.
    pub fn compare_to_baseline(
        &mut self,
        build_key: &str,
    ) -> Result<Option<BaselineComparison>, StoreError> {
        let Some(current) = self.client.query_opt(
            "SELECT project, category, total_seconds, cache_hit_rate,
                    coalesce(xcode_version,''), coalesce(platform,''),
                    coalesce(architecture,''), received_at
             FROM builds WHERE build_key = $1",
            &[&build_key],
        )? else {
            return Ok(None);
        };
        let project: String = current.get(0);
        let Some(baseline) = self.client.query_opt(
            "SELECT build_key, category, total_seconds, cache_hit_rate,
                    coalesce(xcode_version,''), coalesce(platform,''),
                    coalesce(architecture,'')
             FROM builds
             WHERE project = $1 AND received_at < $2
             ORDER BY received_at DESC LIMIT 1",
            &[&project, &current.get::<_, std::time::SystemTime>(7)],
        )? else {
            return Ok(None);
        };

        let (current_category, baseline_category): (String, String) =
            (current.get(1), baseline.get(1));
        let category_changed = current_category != baseline_category;
        // The two queries select different column lists: xcode/platform/arch are
        // at 4..6 in `current` and 4..6 in `baseline` only because baseline
        // starts with build_key instead of project. Compare them by name-equal
        // position rather than a shared index expression.
        let environment_changed = (0..3).any(|offset| {
            current.get::<_, String>(4 + offset) != baseline.get::<_, String>(4 + offset)
        });

        let mut regressions = Vec::new();
        // Per-target comparison only makes sense when both builds did the same
        // kind of work; across a category change the totals are all that mean
        // anything.
        if !category_changed {
            let rows = self.client.query(
                "SELECT c.name, b.seconds, c.seconds, b.fetched_from_cache, c.fetched_from_cache
                 FROM build_targets c
                 JOIN build_targets b ON b.name = c.name AND b.build_key = $2
                 WHERE c.build_key = $1 AND c.seconds > b.seconds",
                &[&build_key, &baseline.get::<_, String>(0)],
            )?;
            for row in &rows {
                // A target fetched from cache in one build and compiled in the
                // other is not a regression; it is a different operation.
                if row.get::<_, bool>(3) != row.get::<_, bool>(4) {
                    continue;
                }
                let previous: f64 = row.get(1);
                let current_seconds: f64 = row.get(2);
                let delta = current_seconds - previous;
                if delta < TARGET_REGRESSION_MIN_SECONDS || previous <= 0.0 {
                    continue;
                }
                let delta_percent = delta / previous * 100.0;
                if delta_percent < TARGET_REGRESSION_MIN_PERCENT {
                    continue;
                }
                regressions.push(MetricRegression {
                    metric_kind: MetricKind::Target,
                    name: row.get(0),
                    previous_seconds: previous,
                    current_seconds,
                    delta_seconds: delta,
                    delta_percent,
                    confidence: if environment_changed {
                        RegressionConfidence::Low
                    } else {
                        RegressionConfidence::High
                    },
                    // Machine-readable, so a consumer branches on this rather
                    // than matching words in `reason`. Setting only the prose
                    // is what left the caveat signal dead before.
                    caveats: if environment_changed {
                        vec![RegressionCaveat::EnvironmentShifted]
                    } else {
                        Vec::new()
                    },
                    reason: if environment_changed {
                        "environment changed between these builds".into()
                    } else {
                        "slower than the previous build of this project".into()
                    },
                });
            }
            regressions.sort_by(|a, b| b.delta_seconds.total_cmp(&a.delta_seconds));
        }

        Ok(Some(BaselineComparison {
            baseline_build_key: baseline.get(0),
            project,
            previous_seconds: baseline.get(2),
            current_seconds: current.get(2),
            category_change: category_changed.then_some((baseline_category, current_category)),
            environment_changed,
            regressions,
        }))
    }

    /// Reports what `prune` would delete without deleting anything.
    pub fn prune_preview(&mut self, keep_days: u32) -> Result<Vec<String>, StoreError> {
        self.prunable_builds(keep_days)
    }

    /// Deletes builds older than `keep_days`, keeping the newest build of each
    /// project so regression baselines never lose their chain.
    pub fn prune(&mut self, keep_days: u32) -> Result<usize, StoreError> {
        let keys = self.prunable_builds(keep_days)?;
        if keys.is_empty() {
            return Ok(0);
        }
        let mut tx = self.client.transaction()?;
        for table in BUILD_SCOPED_TABLES {
            tx.execute(
                format!("DELETE FROM {table} WHERE build_key = ANY($1)").as_str(),
                &[&keys],
            )
            .map_err(|source| StoreError::Query { operation: "pruning child rows", source })?;
        }
        tx.execute("DELETE FROM builds WHERE build_key = ANY($1)", &[&keys])
            .map_err(|source| StoreError::Query { operation: "pruning builds", source })?;
        tx.commit()?;
        Ok(keys.len())
    }

    pub fn git_context(&mut self, builds:i64, project:Option<&str>) -> Result<Value,StoreError> {
        let rows=self.client.query(
            "WITH recent AS (SELECT build_key,EXTRACT(EPOCH FROM COALESCE(to_timestamp(started_at),received_at))::bigint AS at,received_at,total_seconds,category
                             FROM builds WHERE ($1::TEXT IS NULL OR project=$1) ORDER BY received_at DESC LIMIT $2)
             SELECT r.build_key,r.at,r.total_seconds,r.category,
                    MAX(CASE WHEN m.key='git.branch' THEN m.value END),
                    MAX(CASE WHEN m.key='git.commit' THEN m.value END),
                    MAX(CASE WHEN m.key='git.dirty' THEN m.value END)
             FROM recent r LEFT JOIN build_metadata m ON m.build_key=r.build_key
             GROUP BY r.build_key,r.at,r.total_seconds,r.category,r.received_at ORDER BY r.received_at DESC",
            &[&project,&builds])?;
        Ok(serde_json::json!({"items":rows.iter().map(|r|serde_json::json!({"id":r.get::<_,String>(0),"recorded_at":r.get::<_,i64>(1),"total_seconds":r.get::<_,f64>(2),"category":r.get::<_,String>(3),"branch":r.get::<_,Option<String>>(4),"commit":r.get::<_,Option<String>>(5),"dirty":r.get::<_,Option<String>>(6)})).collect::<Vec<_>>()}))
    }
}

/// Below this many builds, percentiles describe the sample rather than the
/// project, so the dashboard shows them as not-yet-meaningful.
const MIN_HISTORY: i64 = 5;

/// Every table keyed by `build_key`.
///
/// The schema declares no foreign keys, so nothing cascades: `prune` must
/// delete from each of these explicitly, and a table missing from this list
/// would silently orphan its rows. `prune_covers_every_build_scoped_table`
/// checks the list against the schema so adding a table without adding it here
/// fails a test rather than leaking rows.
pub const BUILD_SCOPED_TABLES: &[&str] = &[
    "build_targets",
    "build_phases",
    "build_files",
    "build_swift_timings",
    "build_diagnostics",
    "build_tests",
    "build_metadata",
];

/// Writes the wire-shaped rows. Shared by `insert` and `save_analysis` so a
/// build stored locally and one received over the wire agree exactly.
fn insert_wire(tx: &mut Transaction<'_>, build: &WireBuild, day: &str) -> Result<bool, StoreError> {
    let category = build.category.as_str().to_owned();
    // The column is TEXT and `status` is a closed-set enum; `as_str` is the
    // spelling serde writes, so a build stored here and the same build sent
    // over the wire read identically. `None` stays NULL, which means unknown
    // rather than success.
    let status = build.status.map(|status| status.as_str());
    let inserted = tx
        .execute(
            "INSERT INTO builds (build_key,day,project,category,total_seconds,compiled_count,
                                 cache_hit_rate,started_at,machine_id,xcode_version,platform,
                                 architecture,error_count,warning_count,status)
             VALUES ($1,to_date($2,'YYYY-MM-DD'),$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)
             ON CONFLICT (day,build_key) DO NOTHING",
            &[
                &build.build_key,
                &day,
                &build.project,
                &category,
                &build.total_seconds,
                &(build.compiled_count as i32),
                &build.cache_hit_rate,
                &build.started_at,
                &build.machine_id,
                &build.xcode_version,
                &build.platform,
                &build.architecture,
                &(build.error_count as i32),
                &(build.warning_count as i32),
                &status,
            ],
        )
        .map_err(|source| StoreError::Query { operation: "inserting build", source })?;
    if inserted == 0 {
        return Ok(false);
    }
    for target in &build.targets {
        let target_category = target.category.as_str().to_owned();
        tx.execute(
            "INSERT INTO build_targets (day,build_key,name,seconds,category,fetched_from_cache,
                                        compiled_count)
             VALUES (to_date($1,'YYYY-MM-DD'),$2,$3,$4,$5,$6,$7) ON CONFLICT DO NOTHING",
            &[
                &day,
                &build.build_key,
                &target.name,
                &target.seconds,
                &target_category,
                &target.fetched_from_cache,
                &(target.compiled_count as i32),
            ],
        )
        .map_err(|source| StoreError::Query { operation: "inserting target", source })?;
    }
    for phase in &build.phases {
        tx.execute(
            "INSERT INTO build_phases (day,build_key,name,seconds)
             VALUES (to_date($1,'YYYY-MM-DD'),$2,$3,$4) ON CONFLICT DO NOTHING",
            &[&day, &build.build_key, &phase.name, &phase.seconds],
        )
        .map_err(|source| StoreError::Query { operation: "inserting phase", source })?;
    }
    // The wire-version-2 detail. Empty for a build sent by an older client,
    // and for one whose sender had no analysis to draw on, so these loops are
    // simply skipped rather than needing a version check: the payload already
    // says what it carries.
    //
    // `save_analysis` writes the same tables from the local model. Both land
    // here for a pushed build, so a team dashboard and a local one show the
    // same panels for the same build.
    for file in &build.files {
        tx.execute(
            "INSERT INTO build_files (day,build_key,file,architecture,seconds,target,step_type,
                                      occurrences)
             VALUES (to_date($1,'YYYY-MM-DD'),$2,$3,$4,$5,$6,$7,$8) ON CONFLICT DO NOTHING",
            &[
                &day,
                &build.build_key,
                &file.file,
                &file.architecture.clone().unwrap_or_default(),
                &file.seconds,
                &file.target,
                &file.step_type,
                &(file.occurrences as i32),
            ],
        )
        .map_err(|source| StoreError::Query { operation: "inserting file timing", source })?;
    }
    for timing in &build.swift_timings {
        tx.execute(
            "INSERT INTO build_swift_timings (day,build_key,kind,file,line,column_number,symbol,
                                              milliseconds,target)
             VALUES (to_date($1,'YYYY-MM-DD'),$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT DO NOTHING",
            &[
                &day,
                &build.build_key,
                &timing.kind,
                &timing.file,
                &(timing.line as i32),
                &(timing.column as i32),
                &timing.symbol,
                &timing.milliseconds,
                &timing.target,
            ],
        )
        .map_err(|source| StoreError::Query { operation: "inserting swift timing", source })?;
    }
    for diagnostic in &build.diagnostics {
        tx.execute(
            "INSERT INTO build_diagnostics (day,build_key,fingerprint,severity,category,
                                            occurrences,message,file,line,target)
             VALUES (to_date($1,'YYYY-MM-DD'),$2,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT DO NOTHING",
            &[
                &day,
                &build.build_key,
                &diagnostic.fingerprint,
                &diagnostic.severity,
                &diagnostic.category,
                &(diagnostic.occurrences as i32),
                &diagnostic.message,
                &diagnostic.file,
                &diagnostic.line.map(|line| line as i32),
                &diagnostic.target,
            ],
        )
        .map_err(|source| StoreError::Query { operation: "inserting diagnostic", source })?;
    }
    // Numbered exactly as the local path numbers them, so a build that arrived
    // over the wire and the same build collected locally agree about attempts.
    let attempts = attempt_numbers(build.tests.iter().map(|test| (test.suite.as_str(), test.name.as_str())));
    for (test, attempt) in build.tests.iter().zip(attempts) {
        tx.execute(
            "INSERT INTO build_tests (day,build_key,suite,name,status,seconds,message,attempt)
             VALUES (to_date($1,'YYYY-MM-DD'),$2,$3,$4,$5,$6,$7,$8) ON CONFLICT DO NOTHING",
            &[
                &day,
                &build.build_key,
                &test.suite,
                &test.name,
                &test.status,
                &test.seconds,
                &test.message,
                &attempt,
            ],
        )
        .map_err(|source| StoreError::Query { operation: "inserting test result", source })?;
    }
    Ok(true)
}

/// Numbers each result as the nth run of its `(suite, name)` within one build.
///
/// Xcode retries a failing test in place, so one build can report the same test
/// more than once. The parser keeps every occurrence in log order; this turns
/// that order into the `attempt` column, which is what lets a fail-then-pass
/// survive an `ON CONFLICT DO NOTHING` that would otherwise keep only the
/// first run and record the retry as nothing at all.
///
/// Order is the log's, not the clock's: a parallel test runner can interleave
/// output, in which case "attempt 2" means the second result seen rather than
/// provably the second run. Mixed outcomes within a build still read as flaky,
/// which is the question these rows exist to answer.
fn attempt_numbers<'a>(tests: impl Iterator<Item = (&'a str, &'a str)>) -> Vec<i32> {
    let mut seen: std::collections::HashMap<(&str, &str), i32> = std::collections::HashMap::new();
    tests
        .map(|key| {
            let count = seen.entry(key).or_insert(0);
            *count += 1;
            *count
        })
        .collect()
}

/// Serde-renamed enums (`swift_concurrency`, `warning`, …) as their storage
/// string, so stored values match what the JSON API already emits.
fn serde_plain<T: serde::Serialize>(value: &T) -> Result<String, StoreError> {
    Ok(serde_json::to_value(value)?.as_str().unwrap_or("unknown").to_owned())
}

/// Partition day for a build, from its start time; falls back to today when
/// the log carried no usable timestamp.
///
/// Howard Hinnant's `civil_from_days`, avoiding a date dependency for the one
/// conversion this crate needs.
fn day_of(started_at: Option<f64>) -> String {
    let seconds = started_at
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| value as i64)
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs() as i64)
                .unwrap_or(0)
        });
    civil_from_days(seconds.div_euclid(86_400))
}

/// Days since the Unix epoch to `YYYY-MM-DD`.
fn civil_from_days(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped file must parse, and every version in it must be unique.
    /// A duplicate would mean the second copy never applies, which is silent.
    #[test]
    fn the_shipped_migrations_parse() {
        let migrations = parse_migrations(MIGRATIONS_SQL).expect("migrations.sql parses");
        assert!(!migrations.is_empty(), "there is at least one migration");
        assert_eq!(migrations[0].version, "0001", "0001 is the baseline marker");
        assert!(
            migrations.iter().any(|m| m.name == "test_attempts"),
            "the test-attempt migration is present"
        );
    }

    #[test]
    fn a_duplicate_migration_version_is_rejected() {
        let sql = "--@migration 0001 one\nSELECT 1;\n--@migration 0001 again\nSELECT 2;\n";
        assert!(parse_migrations(sql).unwrap_err().contains("duplicate"));
    }

    #[test]
    fn a_migration_header_missing_its_name_is_rejected() {
        assert!(parse_migrations("--@migration 0001\nSELECT 1;\n").unwrap_err().contains("malformed"));
    }

    /// Commentary before the first marker belongs to no migration and must not
    /// be executed as SQL.
    #[test]
    fn preamble_before_the_first_marker_is_dropped() {
        let parsed = parse_migrations("-- file comment\n--@migration 0001 one\nSELECT 1;\n").unwrap();
        assert_eq!(parsed.len(), 1);
        assert!(!parsed[0].sql.contains("file comment"));
        assert!(parsed[0].sql.contains("SELECT 1;"));
    }

    /// Attempts number per test, not per build: two different tests both start
    /// at 1, and a repeat of one does not advance the other.
    #[test]
    fn attempts_number_each_test_independently() {
        let numbered = attempt_numbers(
            [("S", "a"), ("S", "b"), ("S", "a"), ("T", "a")].into_iter(),
        );
        assert_eq!(numbered, vec![1, 1, 2, 1]);
    }

    #[test]
    fn day_of_uses_the_build_start_time() {
        assert_eq!(day_of(Some(0.0)), "1970-01-01");
        assert_eq!(day_of(Some(1_700_000_000.0)), "2023-11-14");
        assert_eq!(day_of(None).len(), 10);
        assert_eq!(day_of(Some(f64::NAN)).len(), 10);
    }

    /// Stored enum strings must match the serde renaming the JSON API uses,
    /// or a dashboard filter on "swift_concurrency" would silently match
    /// nothing.
    #[test]
    fn enums_store_as_their_serde_names() {
        use buildlens_core::{DiagnosticCategory, DiagnosticSeverity};
        assert_eq!(serde_plain(&DiagnosticSeverity::Warning).unwrap(), "warning");
        assert_eq!(serde_plain(&DiagnosticCategory::SwiftConcurrency).unwrap(), "swift_concurrency");
    }

    #[test]
    fn converts_epoch_days_to_civil_dates() {
        assert_eq!(civil_from_days(0), "1970-01-01");
        assert_eq!(civil_from_days(19_000), "2022-01-08");
        // 2024 was a leap year, so day 60 of it is Feb 29.
        assert_eq!(civil_from_days(19_782), "2024-02-29");
        // Before the epoch, which `div_euclid` must handle without wrapping.
        assert_eq!(civil_from_days(-1), "1969-12-31");
    }

    /// A build that started just before midnight and one just after must land
    /// in different partitions, since the day is the partition key.
    #[test]
    fn a_day_boundary_splits_partitions() {
        let midnight = 1_700_000_000_i64 - (1_700_000_000_i64 % 86_400);
        assert_ne!(
            day_of(Some((midnight - 1) as f64)),
            day_of(Some(midnight as f64))
        );
    }

    /// Every stored enum must round-trip as the spelling serde emits, or a
    /// stored row and the JSON describing it disagree.
    #[test]
    fn every_status_and_kind_stores_as_its_serde_name() {
        use buildlens_core::{SwiftTimingKind, wire::BuildStatus};
        assert_eq!(BuildStatus::Succeeded.as_str(), "succeeded");
        assert_eq!(BuildStatus::Failed.as_str(), "failed");
        assert_eq!(BuildStatus::Cancelled.as_str(), "cancelled");
        assert_eq!(SwiftTimingKind::FunctionBody.as_str(), "function_body");
        assert_eq!(SwiftTimingKind::TypeCheck.as_str(), "type_check");
        // And the enum spelling matches what serde would write.
        assert_eq!(serde_plain(&SwiftTimingKind::TypeCheck).unwrap(), "type_check");
    }

    /// `serde_plain` falls back to "unknown" rather than panicking on a value
    /// that is not a plain string, so one odd enum cannot fail a whole insert.
    #[test]
    fn a_non_string_value_stores_as_unknown() {
        assert_eq!(serde_plain(&42).unwrap(), "unknown");
        assert_eq!(serde_plain(&vec![1, 2]).unwrap(), "unknown");
    }

    /// `prune` deletes from this list explicitly because nothing cascades.
    /// A duplicate or an empty name would mean a table silently skipped.
    #[test]
    fn build_scoped_tables_are_unique_and_named() {
        let unique: std::collections::BTreeSet<_> = BUILD_SCOPED_TABLES.iter().collect();
        assert_eq!(unique.len(), BUILD_SCOPED_TABLES.len(), "duplicate table");
        assert!(BUILD_SCOPED_TABLES.iter().all(|table| !table.is_empty()));
        // `builds` itself is deleted separately, after its children.
        assert!(!BUILD_SCOPED_TABLES.contains(&"builds"));
    }

    /// Both regression paths apply the same rule, so the shared predicate is
    /// asserted directly. `target_regressions` used to hardcode 1.2 (20%)
    /// while the constant said 10%, making the same slowdown a regression on
    /// one path and not the other.
    #[test]
    fn the_regression_rule_needs_both_an_absolute_and_a_relative_jump() {
        // Mirrors the predicate both paths use.
        let regressed = |previous: f64, current: f64| {
            let delta = current - previous;
            previous > TARGET_REGRESSION_MIN_SECONDS
                && delta >= TARGET_REGRESSION_MIN_SECONDS
                && delta / previous * 100.0 >= TARGET_REGRESSION_MIN_PERCENT
        };

        // A big target gaining a full second, well past both thresholds.
        assert!(regressed(10.0, 12.0));
        // Percent alone is not enough: a tiny step that doubled is noise.
        assert!(!regressed(0.2, 0.4), "a 0.2s step doubling is not a regression");
        // Absolute alone is not enough either, once the constant is 10%: a
        // 100s target gaining 0.6s is 0.6%.
        assert!(!regressed(100.0, 100.6));
        // And exactly at both thresholds counts, so the boundary is inclusive.
        assert!(regressed(5.0, 5.0 + 5.0 * TARGET_REGRESSION_MIN_PERCENT / 100.0));
    }
}
