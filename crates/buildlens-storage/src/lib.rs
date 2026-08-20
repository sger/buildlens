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
    #[error("PostgreSQL error: {0}")]
    Database(#[from] postgres::Error),
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("build has no usable activity-log metrics")]
    UnusableBuild,
    #[error("PostgreSQL {operation}: {source}")]
    Query {
        operation: &'static str,
        source: postgres::Error,
    },
    /// `migrations.sql` could not be parsed. A programming error rather than a
    /// runtime one — the file is compiled in — so it surfaces at startup,
    /// before any work, rather than leaving a migration quietly unapplied.
    #[error("invalid migration definition: {0}")]
    Migration(String),
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

/// The Postgres channel a write is announced on, and that a dashboard LISTENs
/// to so it learns about builds it did not store itself.
///
/// The local setup is two processes — `buildlens collect --watch` writes, and
/// the dashboard reads — so the dashboard cannot know a build arrived by any
/// in-process means. It used to find out only when its response cache expired,
/// which is why a freshly finished build did not appear until the page was
/// reloaded. Postgres is the one thing both processes already share, so the
/// announcement goes through it.
pub const BUILD_CHANNEL: &str = "buildlens_builds";

/// Advisory-lock class for per-day partition DDL, distinct from
/// [`MIGRATION_LOCK`] so a long migration on one database does not block an
/// ingest that only needs today's partition. The second key is the day, so two
/// processes creating partitions for *different* days never wait on each other.
const PARTITION_LOCK: i32 = 0x6275_6970_u32 as i32;

/// How far either side of today [`migrate`] pre-creates partitions.
///
/// One day each way, not a week. Every `connect` runs this under
/// `MIGRATION_LOCK`, and each day costs eight `CREATE`/`ATTACH` pairs, so a wide
/// window turns every CLI invocation and every test into hundreds of DDL
/// statements serialised against each other — measured at 3.5x slower on a
/// 35-test suite for partitions nothing was going to write to.
///
/// The window only has to cover the days a *running* process will be asked for
/// without reconnecting: today, tomorrow for a clock a little fast or a process
/// alive over midnight, and yesterday for a build that finished just before it.
/// Anything further out — a `collect --all` backfill dated last month — is
/// handled on the write path by [`PostgresStore::ensure_partitions_for`], which
/// pays one short DDL for the first build of that day and nothing after.
const PARTITION_WINDOW_DAYS: i64 = 1;

/// Every table partitioned by `day`.
///
/// The sibling of [`BUILD_SCOPED_TABLES`], and deliberately a separate list:
/// that one names the tables `prune` must delete child rows from, which excludes
/// `builds` itself, while this one includes it because `builds` is partitioned
/// too. `every_day_partitioned_table_is_declared_partitioned_in_the_schema`
/// checks this list against the schema, so a ninth partitioned table cannot be
/// added without appearing here.
pub const DAY_PARTITIONED_TABLES: &[&str] = &[
    "builds",
    "build_targets",
    "build_phases",
    "build_files",
    "build_swift_timings",
    "build_diagnostics",
    "build_tests",
    "build_steps",
    "build_metadata",
];

/// Applies the baseline schema and every unapplied migration, under an
/// advisory lock so concurrent starts serialise rather than deadlock.
///
/// Lives here rather than in `buildlens-server` because that crate depends on
/// this one and not the reverse; the server delegates to it. Two
/// implementations of one schema is exactly what produced the missing-table
/// bug the comment above `SCHEMA_SQL` describes.
/// Announces that a build landed, so a dashboard in another process can drop
/// its cached answers now rather than when they expire.
///
/// Sent after the transaction commits, never inside it. A NOTIFY issued in a
/// transaction is queued until commit anyway, but sending it after also means a
/// listener cannot be woken for a build that then rolls back.
///
/// Failure is logged and swallowed. This is a latency optimisation on top of
/// polling that still works: a dashboard that misses the announcement refreshes
/// on its next tick as it always did, and refusing to store a build because the
/// notification failed would trade a real feature for a cosmetic one.
fn notify_build_stored(client: &mut Client, build_key: &str) {
    // `pg_notify` rather than `NOTIFY`, whose channel and payload are literals
    // that cannot be parameterised — a build key is Xcode's string, not ours.
    if let Err(error) = client.execute("SELECT pg_notify($1, $2)", &[&BUILD_CHANNEL, &build_key]) {
        // Once per process, as with the partition warning above: a database
        // that refuses NOTIFY refuses every one of them, and the fact worth
        // seeing is that announcements are not arriving at all.
        static WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        WARNED.get_or_init(|| {
            eprintln!(
                "buildlens: could not announce a stored build ({error}); dashboards will \
                 update on their next refresh instead of immediately"
            );
        });
    }
}

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
    apply_migrations(client)?;
    // After the migrations, never before. Migration 0002 rewrites
    // `build_tests`' primary key on the parent, and a child created ahead of
    // that would be born with the old key. Creating partitions last means every
    // child inherits the current one — which is also why they are created from
    // here rather than from `schema.sql`, which runs first.
    ensure_partition_window(client, PARTITION_WINDOW_DAYS, PARTITION_WINDOW_DAYS)?;
    Ok(())
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
            migrations.push(Migration {
                version,
                name,
                sql: String::new(),
            });
        } else if let Some(current) = migrations.last_mut() {
            current.sql.push_str(line);
            current.sql.push('\n');
        }
    }
    for (index, migration) in migrations.iter().enumerate() {
        if migrations[..index]
            .iter()
            .any(|other| other.version == migration.version)
        {
            return Err(format!(
                "duplicate migration version: {}",
                migration.version
            ));
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

/// `builds` + `2023-11-14` -> `builds_p20231114`.
///
/// Not `builds_2023_11_14`: a child whose name reads like a parent is a trap for
/// anything that derives table lists from names, and
/// `prune_covers_every_build_scoped_table` does exactly that. The `_p` prefix on
/// the digits keeps children out of every such filter.
fn partition_name(table: &str, day: &str) -> String {
    let mut name = String::with_capacity(table.len() + 10);
    name.push_str(table);
    name.push_str("_p");
    name.extend(day.chars().filter(char::is_ascii_digit));
    name
}

/// True when every one of `day`'s partitions already exists in the current
/// `search_path`.
///
/// `to_regclass` on the unqualified names rather than a `pg_inherits` join: it
/// resolves exactly as the following DDL would, which is what makes this correct
/// under the per-test schema isolation the integration tests use.
///
/// Checks all eight rather than `builds` alone. They are created together, but
/// they do not always disappear together: `prune` drops a day per table, and a
/// day left half-partitioned by an interrupted prune — or by a hand-run
/// `DROP TABLE` — would otherwise report as present and quietly send the missing
/// tables' rows to their defaults.
fn day_partition_exists(client: &mut Client, day: &str) -> Result<bool, StoreError> {
    let names: Vec<String> = DAY_PARTITIONED_TABLES
        .iter()
        .map(|table| partition_name(table, day))
        .collect();
    let row = client
        .query_one(
            "SELECT count(*) = cardinality($1::TEXT[])
             FROM unnest($1::TEXT[]) AS name
             WHERE to_regclass(name) IS NOT NULL",
            &[&names],
        )
        .map_err(|source| StoreError::Query {
            operation: "checking for a partition",
            source,
        })?;
    Ok(row.get(0))
}

/// Creates `day`'s partition on every table in [`DAY_PARTITIONED_TABLES`],
/// moving any rows already sitting in the default partition into it.
///
/// Always takes the create-move-attach path rather than `CREATE TABLE ...
/// PARTITION OF`, because Postgres scans the default partition on that form and
/// refuses when it holds rows in the new range — which is every row written
/// before per-day partitions existed. Using one path unconditionally means the
/// populated case is exercised by every test rather than by a branch that only
/// fires during an upgrade.
///
/// Idempotent and race-safe in two independent ways, because they fail
/// differently: an advisory lock keyed on the day serialises two BuildLens
/// processes that both find the partition missing, and a `duplicate_table` from
/// someone who ran the DDL by hand outside that lock is treated as success.
///
/// `day` is `YYYY-MM-DD`, the spelling [`day_of`] produces, so no caller
/// converts between two date representations.
pub fn ensure_day_partitions(client: &mut Client, day: &str) -> Result<(), StoreError> {
    let upper = next_day(day)?;
    // Identifiers cannot be parameterised, and neither can partition bounds.
    // Every value interpolated below is either a compile-time constant from
    // `DAY_PARTITIONED_TABLES` or a `YYYY-MM-DD` string that `next_day` has
    // already validated as digits, so there is nothing a caller could inject.
    //
    // All eight tables in one `batch_execute`, and so one round trip: this runs
    // under `MIGRATION_LOCK` on every connect, where eight separate round trips
    // per day is latency every other starting process waits on.
    let mut statements = String::new();
    // Transaction-scoped, unlike `MIGRATION_LOCK`: it releases on commit or
    // rollback, so it cannot leak the way a session lock does on an error path.
    statements.push_str(&format!(
        "SELECT pg_advisory_xact_lock({PARTITION_LOCK}, {});",
        day_lock_key(day)
    ));
    for table in DAY_PARTITIONED_TABLES {
        let child = partition_name(table, day);
        let default = format!("{table}_default");
        // Per table rather than per day, because a day can be half-partitioned:
        // `prune` drops each table's partition separately, so an interrupted one
        // leaves some present and some not. Creating only what is missing makes
        // this a repair as well as a create.
        //
        // `to_regclass` is checked inside the same statement batch, and so under
        // the advisory lock, rather than by the caller's earlier existence check:
        // between that check and this batch another process may have created it.
        //
        // The CHECK matching the bounds exactly lets ATTACH skip its validation
        // scan of the new child, turning an O(rows) read into a catalog check.
        statements.push_str(&format!(
            "DO $do$ BEGIN
                 IF to_regclass('{child}') IS NULL THEN
                     CREATE TABLE {child} (LIKE {table} INCLUDING DEFAULTS INCLUDING CONSTRAINTS);
                     ALTER TABLE {child} ADD CONSTRAINT {child}_day
                         CHECK (day >= DATE '{day}' AND day < DATE '{upper}');
                     INSERT INTO {child} SELECT * FROM {default} WHERE day = DATE '{day}';
                     DELETE FROM {default} WHERE day = DATE '{day}';
                     ALTER TABLE {table} ATTACH PARTITION {child}
                         FOR VALUES FROM ('{day}') TO ('{upper}');
                 END IF;
             END $do$;"
        ));
    }
    // `batch_execute` is implicitly one transaction, so a failure part-way
    // leaves no half-partitioned day behind.
    if let Err(error) = client.batch_execute(&statements) {
        // 42P07 is duplicate_table. The advisory lock serialises BuildLens
        // processes and the `to_regclass` guard skips what already exists, so
        // reaching this means someone outside both created the partition — a
        // hand-run `CREATE TABLE`, or another tool holding no lock of ours.
        // The partition exists either way, which is all the caller wanted.
        if error.code().map(|code| code.code()) == Some("42P07") {
            return Ok(());
        }
        return Err(StoreError::Query {
            operation: "creating a day partition",
            source: error,
        });
    }
    Ok(())
}

/// Pre-creates partitions for a window around today, so steady-state ingest
/// does no DDL at all.
///
/// Returns how many days it created. Zero on the second start of the day is the
/// signal that the window is warm, which is the number worth logging.
pub fn ensure_partition_window(
    client: &mut Client,
    back: i64,
    ahead: i64,
) -> Result<usize, StoreError> {
    let days = window_days(&day_of(None), back, ahead);
    // One round trip to find the days that need work, rather than one existence
    // check per day per table. On a warm database this is the only statement this
    // function runs, which is the whole point of the window.
    //
    // Screens on `builds` alone, unlike `day_partition_exists`: this only decides
    // which days to hand to `ensure_day_partitions`, which re-checks every table
    // under the lock and creates whatever is missing. A day that is half
    // partitioned is repaired there, on the write path, rather than by widening
    // this into eight lookups per day on every single connect.
    let missing: Vec<String> = client
        .query(
            "SELECT day FROM unnest($1::TEXT[]) AS day
             WHERE to_regclass('builds_p' || replace(day, '-', '')) IS NULL",
            &[&days],
        )
        .map_err(|source| StoreError::Query {
            operation: "listing missing partitions",
            source,
        })?
        .iter()
        .map(|row| row.get(0))
        .collect();
    for day in &missing {
        ensure_day_partitions(client, day)?;
    }
    Ok(missing.len())
}

/// The days a window covers, oldest first, inclusive of both ends.
fn window_days(anchor: &str, back: i64, ahead: i64) -> Vec<String> {
    let Ok(center) = days_from_civil(anchor) else {
        return Vec::new();
    };
    ((center - back)..=(center + ahead))
        .map(civil_from_days)
        .collect()
}

/// A stable per-day key for the advisory lock's second argument.
///
/// The epoch day itself, so two processes on the same day collide and two on
/// different days do not. Falls back to 0 for an unparseable day, which only
/// costs a little serialisation.
fn day_lock_key(day: &str) -> i32 {
    days_from_civil(day).unwrap_or(0) as i32
}

/// The day after `day`, as the exclusive upper bound of its partition.
///
/// Computed here rather than as `$1::date + 1` in SQL because partition bounds
/// cannot be parameterised — the DDL is formatted as text either way, so asking
/// Postgres for the date would cost a round trip and buy nothing.
fn next_day(day: &str) -> Result<String, StoreError> {
    Ok(civil_from_days(days_from_civil(day)? + 1))
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

/// Deliberately derives nothing: `url` is a connection string that may carry a
/// password, and a `Debug` impl would put it in every error report that formats
/// a struct containing one.
pub struct PostgresStore {
    client: Client,
    /// The URL this store connected with, kept so [`PostgresStore::ensure_partitions_for`]
    /// can open a second, short-lived connection for its DDL.
    ///
    /// Creating a partition takes ACCESS EXCLUSIVE on the parent and holds it
    /// until commit. Run on `client`, that lock would be held for the whole
    /// ingest transaction, and every concurrent reader and writer would queue
    /// behind one build's insert. A throwaway connection commits the DDL in
    /// milliseconds and disconnects before the ingest transaction opens.
    url: String,
}

impl PostgresStore {
    pub fn connect(url: &str) -> Result<Self, StoreError> {
        let mut store = Self {
            client: Client::connect(url, NoTls)?,
            url: url.to_owned(),
        };
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

    /// Makes sure `day`'s partitions exist before a write that targets them,
    /// using a throwaway connection so the DDL's ACCESS EXCLUSIVE lock on eight
    /// parents is not held for the length of the ingest transaction.
    ///
    /// The existence check runs on `self.client` because it is on the hot path
    /// and costs one catalog lookup; a connection is only opened on the rare
    /// miss, which after `migrate`'s pre-created window means the first build of
    /// a day outside it.
    ///
    /// A connection failure here is logged and swallowed rather than propagated.
    /// The insert that follows still succeeds — it lands in the default
    /// partition, which exists for exactly this reason — and refusing a build
    /// because its storage could not be optimised would be the wrong trade. A
    /// failure of the DDL itself does propagate: that is a real schema problem,
    /// not a degraded path.
    fn ensure_partitions_for(&mut self, day: &str) -> Result<(), StoreError> {
        if day_partition_exists(&mut self.client, day)? {
            return Ok(());
        }
        match Client::connect(&self.url, NoTls) {
            Ok(mut ddl) => ensure_day_partitions(&mut ddl, day),
            Err(error) => {
                // Once per process: a broken DDL path would otherwise log on
                // every insert, and the symptom worth seeing is that it happened
                // at all, not how many times.
                static WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
                WARNED.get_or_init(|| {
                    eprintln!(
                        "buildlens: could not open a connection to create day partitions \
                         ({error}); builds will be stored in the default partition"
                    );
                });
                Ok(())
            }
        }
    }

    pub fn insert(&mut self, build: &WireBuild) -> Result<bool, StoreError> {
        let day = day_of(build.started_at);
        self.ensure_partitions_for(&day)?;
        let mut tx = self.client.transaction()?;
        let inserted = insert_wire(&mut tx, build, &day)?;
        tx.commit()?;
        // Only a real insert. A duplicate push changed nothing, and announcing
        // it would wake every dashboard for a build they already show.
        if inserted {
            notify_build_stored(&mut self.client, &build.build_key);
        }
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
    pub fn save_analysis_with_attempts(
        &mut self,
        analysis: &BuildAnalysis,
        project: &str,
        machine_id: Option<String>,
        anonymous: bool,
        attempts: &[i32],
    ) -> Result<bool, StoreError> {
        self.save_inner(analysis, project, machine_id, anonymous, Some(attempts))
    }

    pub fn save_analysis(
        &mut self,
        analysis: &BuildAnalysis,
        project: &str,
        machine_id: Option<String>,
        anonymous: bool,
    ) -> Result<bool, StoreError> {
        self.save_inner(analysis, project, machine_id, anonymous, None)
    }

    /// As [`Self::save_analysis`], with an explicit tier and the identity the
    /// identified tier records.
    ///
    /// A local collect stores through the same `WireBuild` a push sends, so
    /// without this the two paths disagreed: `--attribution identified`
    /// reached the server but was flattened back to pseudonymous on the way
    /// into the local database, which then had no name to break down by.
    pub fn save_analysis_attributed(
        &mut self,
        analysis: &BuildAnalysis,
        project: &str,
        machine_id: Option<String>,
        attribution: Attribution,
        identity: buildlens_core::wire::Identity,
        attempts: Option<&[i32]>,
    ) -> Result<bool, StoreError> {
        self.save_with(
            analysis,
            project,
            machine_id,
            attribution,
            identity,
            attempts,
        )
    }

    fn save_inner(
        &mut self,
        analysis: &BuildAnalysis,
        project: &str,
        machine_id: Option<String>,
        anonymous: bool,
        explicit_attempts: Option<&[i32]>,
    ) -> Result<bool, StoreError> {
        let attribution = if anonymous {
            Attribution::Anonymous
        } else {
            Attribution::Pseudonymous
        };
        self.save_with(
            analysis,
            project,
            machine_id,
            attribution,
            buildlens_core::wire::Identity::default(),
            explicit_attempts,
        )
    }

    fn save_with(
        &mut self,
        analysis: &BuildAnalysis,
        project: &str,
        machine_id: Option<String>,
        attribution: Attribution,
        identity: buildlens_core::wire::Identity,
        explicit_attempts: Option<&[i32]>,
    ) -> Result<bool, StoreError> {
        let metrics = analysis.metrics.as_ref().ok_or(StoreError::UnusableBuild)?;
        let mut build = WireBuild::from_metrics_with_identity(
            metrics,
            project,
            machine_id,
            attribution,
            identity,
            100,
        )
        .ok_or(StoreError::UnusableBuild)?;
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
        self.ensure_partitions_for(&day)?;
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

        for diagnostic in analysis
            .diagnostics
            .diagnostics
            .iter()
            .take(MAX_DIAGNOSTICS)
        {
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
        let derived = attempt_numbers(
            analysis
                .tests
                .tests
                .iter()
                .map(|test| (test.suite.as_str(), test.test.as_str())),
        );
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
            let status = match test.status {
                TestStatus::Passed => "passed",
                TestStatus::Failed => "failed",
                TestStatus::Started => "started",
            };
            tx.execute(
                "INSERT INTO build_tests (day,build_key,suite,name,status,seconds,message,attempt)
                 VALUES (to_date($1,'YYYY-MM-DD'),$2,$3,$4,$5,$6,$7,$8) ON CONFLICT DO NOTHING",
                &[
                    &day,
                    &key,
                    &test.suite,
                    &test.test,
                    &status,
                    &test.duration_seconds,
                    &test.message,
                    &attempt,
                ],
            )
            .map_err(|source| StoreError::Query {
                operation: "inserting test result",
                source,
            })?;
        }

        for (metadata_key, value) in &analysis.metadata.entries {
            tx.execute(
                "INSERT INTO build_metadata (day,build_key,key,value) VALUES (to_date($1,'YYYY-MM-DD'),$2,$3,$4) ON CONFLICT DO NOTHING",
                &[&day, &key, metadata_key, value],
            ).map_err(|source| StoreError::Query { operation: "inserting metadata", source })?;
        }

        tx.commit()?;
        notify_build_stored(&mut self.client, &key);
        Ok(true)
    }

    pub fn build_id_for_activity_log(&mut self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .client
            .query_opt(
                "SELECT build_key FROM builds WHERE build_key=$1 LIMIT 1",
                &[&key],
            )?
            .map(|row| row.get(0)))
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
    pub fn attach_tests(
        &mut self,
        build_key: &str,
        tests: &[(buildlens_core::TestResult, i32)],
    ) -> Result<usize, StoreError> {
        let Some(row) = self.client.query_opt(
            "SELECT day::TEXT FROM builds WHERE build_key=$1 LIMIT 1",
            &[&build_key],
        )?
        else {
            return Ok(0);
        };
        let day: String = row.get(0);
        // Nearly always a no-op — the build's own insert created this day's
        // partitions — but a prune that dropped the day between the build log
        // and the `.xcresult` arriving 70-90 seconds later is real, and without
        // this the results would land in the default partition.
        self.ensure_partitions_for(&day)?;
        let mut inserted = 0;
        let mut tx = self.client.transaction()?;
        for (test, attempt) in tests {
            let status = match test.status {
                TestStatus::Passed => "passed",
                TestStatus::Failed => "failed",
                TestStatus::Started => "started",
            };
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
        if tests
            .iter()
            .any(|(test, _)| test.status == TestStatus::Failed)
        {
            tx.execute(
                "UPDATE builds SET status='failed' WHERE build_key=$1",
                &[&build_key],
            )
            .map_err(|source| StoreError::Query {
                operation: "marking the build failed",
                source,
            })?;
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
        // Announced like a stored build: results arriving is what resolves a
        // build held at `pending_tests`, so the verdict a dashboard is showing
        // has just changed even though no new build row appeared.
        if inserted > 0 {
            notify_build_stored(&mut self.client, build_key);
        }
        Ok(inserted)
    }

    pub fn projects(&mut self) -> Result<Value, StoreError> {
        let rows=self.client.query("SELECT project,COUNT(*)::bigint FROM builds GROUP BY project ORDER BY COUNT(*) DESC,project",&[])?;
        Ok(
            serde_json::json!({"items":rows.iter().map(|r|serde_json::json!({"project":r.get::<_,String>(0),"builds":r.get::<_,i64>(1)})).collect::<Vec<_>>() }),
        )
    }
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
        Ok(
            serde_json::json!({"builds":rows.iter().map(|r|serde_json::json!({"id":r.get::<_,String>(0),"recorded_at":r.get::<_,i64>(1),"project":r.get::<_,String>(2),"category":r.get::<_,String>(3),"total_seconds":r.get::<_,f64>(4),"status":r.get::<_,Option<String>>(6),"raw_warnings":r.get::<_,i32>(8),"errors":r.get::<_,i32>(7),"cache_hit_rate":r.get::<_,Option<f64>>(5),"scheme":r.get::<_,Option<String>>(9),"total_tests":r.get::<_,i32>(10),"failed_tests":r.get::<_,i32>(11)})).collect::<Vec<_>>() }),
        )
    }
    pub fn duration_trend_for(
        &mut self,
        limit: i64,
        project: Option<&str>,
    ) -> Result<Value, StoreError> {
        let rows = if let Some(project) = project {
            self.client.query("SELECT build_key,EXTRACT(EPOCH FROM COALESCE(to_timestamp(started_at),received_at))::bigint,project,category,total_seconds,cache_hit_rate FROM builds WHERE project=$1 ORDER BY received_at DESC LIMIT $2",&[&project,&limit])?
        } else {
            self.client.query("SELECT build_key,EXTRACT(EPOCH FROM COALESCE(to_timestamp(started_at),received_at))::bigint,project,category,total_seconds,cache_hit_rate FROM builds ORDER BY received_at DESC LIMIT $1",&[&limit])?
        };
        Ok(
            serde_json::json!({"items":rows.iter().rev().map(|r|serde_json::json!({"id":r.get::<_,String>(0),"recorded_at":r.get::<_,i64>(1),"project":r.get::<_,String>(2),"category":r.get::<_,String>(3),"total_seconds":r.get::<_,f64>(4),"cache_hit_rate":r.get::<_,Option<f64>>(5)})).collect::<Vec<_>>() }),
        )
    }

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
    pub fn build_snapshot(&mut self, key: &str) -> Result<Option<Value>, StoreError> {
        let Some(r)=self.client.query_opt("SELECT build_key,EXTRACT(EPOCH FROM COALESCE(to_timestamp(started_at),received_at))::bigint,project,category,total_seconds,cache_hit_rate,compiled_count,machine_id,xcode_version,platform,architecture,status,error_count,warning_count,scheme,replayed_steps,build_user,build_host FROM builds WHERE build_key=$1",&[&key])? else {
            return Ok(None);
        };
        let targets=self.client.query("SELECT name,seconds,category,fetched_from_cache,compiled_count FROM build_targets WHERE build_key=$1 ORDER BY seconds DESC LIMIT 100",&[&key])?;
        let phases=self.client.query("SELECT name,seconds FROM build_phases WHERE build_key=$1 ORDER BY seconds DESC LIMIT 50",&[&key])?;
        let metadata = self.client.query(
            "SELECT key,value FROM build_metadata WHERE build_key=$1 ORDER BY key",
            &[&key],
        )?;
        let files=self.client.query("SELECT file,target,seconds,occurrences,step_type FROM build_files WHERE build_key=$1 ORDER BY seconds DESC LIMIT 50",&[&key])?;
        let swift=self.client.query("SELECT file,line,symbol,kind,milliseconds,target FROM build_swift_timings WHERE build_key=$1 ORDER BY milliseconds DESC LIMIT 50",&[&key])?;
        // Target durations for the share rule below, taken before the JSON
        // assembly borrows `targets`.
        let target_names: Vec<(String, f64)> = targets
            .iter()
            .map(|t| (t.get::<_, String>(0), t.get::<_, f64>(1)))
            .collect();
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
        let test_totals = self.client.query_one(
            "WITH final AS (
                 SELECT DISTINCT ON (suite,name) suite,name,status
                 FROM build_tests WHERE build_key=$1 ORDER BY suite,name,attempt DESC)
             SELECT (SELECT COUNT(*)::bigint FROM final),
                    (SELECT COUNT(*) FILTER (WHERE status='failed')::bigint FROM final),
                    (SELECT COALESCE(SUM(seconds),0.0) FROM build_tests WHERE build_key=$1)",
            &[&key],
        )?;
        // Capped like every other detail list on this page. Ordered by ordinal,
        // which is the timeline order the steps were transmitted in, so the
        // first 500 rows are the *start* of the build rather than an arbitrary
        // slice. `total_steps` reports how many exist so the page can say what
        // it is not showing instead of implying the build had 500.
        let steps=self.client.query(
            "SELECT step_type,title,file,target,architecture,seconds,started_at,ended_at,fetched_from_cache,executed
             FROM build_steps WHERE build_key=$1 ORDER BY ordinal LIMIT 500",&[&key])?;
        let step_totals = self.client.query_one(
            "SELECT COUNT(*)::bigint,COUNT(*) FILTER (WHERE executed)::bigint,
                    COALESCE(SUM(seconds) FILTER (WHERE executed),0.0)
             FROM build_steps WHERE build_key=$1",
            &[&key],
        )?;
        // The same advice the CLI reports, from the same function: a threshold
        // that differed between here and `analyze` would show as two tools
        // disagreeing about one build. Reconstructed from the rows just read
        // rather than stored, so it stays correct when the rules change.
        let advice = {
            let timings: Vec<buildlens_core::SwiftTimingMetric> = swift
                .iter()
                .map(|s| buildlens_core::SwiftTimingMetric {
                    kind: match s.get::<_, String>(3).as_str() {
                        "function_body" => buildlens_core::SwiftTimingKind::FunctionBody,
                        _ => buildlens_core::SwiftTimingKind::TypeCheck,
                    },
                    file: s.get::<_, String>(0),
                    line: s.get::<_, i32>(1).max(0) as u32,
                    column: 0,
                    symbol: s.get::<_, Option<String>>(2),
                    milliseconds: s.get::<_, f64>(4),
                    target: s.get::<_, Option<String>>(5),
                })
                .collect();
            let seconds_by_target: Vec<(&str, f64)> = target_names
                .iter()
                .map(|(name, seconds)| (name.as_str(), *seconds))
                .collect();
            buildlens_intel::from_timings(&timings, &seconds_by_target)
        };
        Ok(Some(serde_json::json!({
            "id":r.get::<_,String>(0),"recorded_at":r.get::<_,i64>(1),"project":r.get::<_,String>(2),"category":r.get::<_,String>(3),
            "total_seconds":r.get::<_,f64>(4),"cache_hit_rate":r.get::<_,Option<f64>>(5),"compiled_count":r.get::<_,i32>(6),"replayed_steps":r.get::<_,i32>(15),
            "machine_id":r.get::<_,Option<String>>(7),"xcode_version":r.get::<_,Option<String>>(8),"platform":r.get::<_,Option<String>>(9),
            // Null for every build except one sent at the identified tier.
            "user":r.get::<_,Option<String>>(16),"host":r.get::<_,Option<String>>(17),
            "architecture":r.get::<_,Option<String>>(10),"status":r.get::<_,Option<String>>(11),"errors":r.get::<_,i32>(12),"raw_warnings":r.get::<_,i32>(13),"scheme":r.get::<_,Option<String>>(14),
            "targets":targets.iter().map(|t|serde_json::json!({"name":t.get::<_,String>(0),"seconds":t.get::<_,f64>(1),"category":t.get::<_,String>(2),"fetched_from_cache":t.get::<_,bool>(3),"compiled_count":t.get::<_,i32>(4)})).collect::<Vec<_>>(),
            "phases":phases.iter().map(|p|serde_json::json!({"name":p.get::<_,String>(0),"seconds":p.get::<_,f64>(1)})).collect::<Vec<_>>(),
            "metadata":metadata.iter().map(|m|(m.get::<_,String>(0),Value::String(m.get::<_,String>(1)))).collect::<serde_json::Map<_,_>>(),
            "files":files.iter().map(|f|serde_json::json!({"file":f.get::<_,String>(0),"target":f.get::<_,Option<String>>(1),"seconds":f.get::<_,f64>(2),"compilations":f.get::<_,i32>(3),"step_type":f.get::<_,String>(4)})).collect::<Vec<_>>(),
            "steps":steps.iter().map(|s|serde_json::json!({"step_type":s.get::<_,String>(0),"title":s.get::<_,String>(1),"file":s.get::<_,Option<String>>(2),"target":s.get::<_,Option<String>>(3),"architecture":s.get::<_,Option<String>>(4),"seconds":s.get::<_,f64>(5),"started_at":s.get::<_,Option<f64>>(6),"ended_at":s.get::<_,Option<f64>>(7),"fetched_from_cache":s.get::<_,bool>(8),"executed":s.get::<_,bool>(9)})).collect::<Vec<_>>(),
            "step_totals":serde_json::json!({"total":step_totals.get::<_,i64>(0),"executed":step_totals.get::<_,i64>(1),"executed_seconds":step_totals.get::<_,f64>(2)}),
            "advice":advice,
            "swift":swift.iter().map(|s|serde_json::json!({"file":s.get::<_,String>(0),"line":s.get::<_,i32>(1),"symbol":s.get::<_,Option<String>>(2),"kind":s.get::<_,String>(3),"milliseconds":s.get::<_,f64>(4),"target":s.get::<_,Option<String>>(5)})).collect::<Vec<_>>(),
            "diagnostics":diagnostics.iter().map(|d|serde_json::json!({"fingerprint":d.get::<_,String>(0),"severity":d.get::<_,String>(1),"category":d.get::<_,String>(2),"message":d.get::<_,String>(3),"file":d.get::<_,Option<String>>(4),"line":d.get::<_,Option<i32>>(5),"target":d.get::<_,Option<String>>(6),"occurrences":d.get::<_,i32>(7)})).collect::<Vec<_>>(),
            "tests":tests.iter().map(|t|serde_json::json!({"suite":t.get::<_,String>(0),"test":t.get::<_,String>(1),"status":t.get::<_,String>(2),"seconds":t.get::<_,Option<f64>>(3),"message":t.get::<_,Option<String>>(4),"attempts":t.get::<_,i64>(5)})).collect::<Vec<_>>(),
            "test_totals":serde_json::json!({"total":test_totals.get::<_,i64>(0),"failed":test_totals.get::<_,i64>(1),"seconds":test_totals.get::<_,f64>(2)}),
        })))
    }

    /// p50/p95 over recent builds. Percentiles need a floor of history to mean
    /// anything, so a project with one build reports nulls rather than
    /// presenting a single sample as a distribution.
    pub fn duration_percentiles_for(
        &mut self,
        limit: i64,
        project: Option<&str>,
    ) -> Result<Value, StoreError> {
        let row=self.client.query_one(
            "WITH recent AS (SELECT total_seconds FROM builds WHERE ($1::TEXT IS NULL OR project=$1) ORDER BY received_at DESC LIMIT $2)
             SELECT COUNT(*)::bigint,
                    percentile_cont(0.5) WITHIN GROUP (ORDER BY total_seconds),
                    percentile_cont(0.95) WITHIN GROUP (ORDER BY total_seconds),
                    MIN(total_seconds),MAX(total_seconds),AVG(total_seconds) FROM recent",
            &[&project,&limit])?;
        let builds = row.get::<_, i64>(0);
        Ok(
            serde_json::json!({"builds":builds,"enough_history":builds>=MIN_HISTORY,
            "p50":row.get::<_,Option<f64>>(1),"p95":row.get::<_,Option<f64>>(2),
            "min_seconds":row.get::<_,Option<f64>>(3),"max_seconds":row.get::<_,Option<f64>>(4),"avg_seconds":row.get::<_,Option<f64>>(5)}),
        )
    }

    /// Per-calendar-day p50/p95, which is what makes a week-over-week
    /// regression visible rather than just a noisy per-build line.
    pub fn daily_percentiles_for(
        &mut self,
        days: i64,
        project: Option<&str>,
    ) -> Result<Value, StoreError> {
        let rows=self.client.query(
            "SELECT day::TEXT,COUNT(*)::bigint,
                    percentile_cont(0.5) WITHIN GROUP (ORDER BY total_seconds),
                    percentile_cont(0.95) WITHIN GROUP (ORDER BY total_seconds)
             FROM builds WHERE ($1::TEXT IS NULL OR project=$1) AND day >= (CURRENT_DATE - $2::INTEGER)
             GROUP BY day ORDER BY day",
            &[&project,&(days as i32)])?;
        Ok(
            serde_json::json!({"items":rows.iter().map(|r|serde_json::json!({"day":r.get::<_,String>(0),"builds":r.get::<_,i64>(1),"p50":r.get::<_,Option<f64>>(2),"p95":r.get::<_,Option<f64>>(3)})).collect::<Vec<_>>()}),
        )
    }

    /// Slowest targets averaged over recent builds — the "what should we fix
    /// first" list, ranked by mean rather than a single unlucky build.
    pub fn target_trend(
        &mut self,
        builds: i64,
        top: i64,
        project: Option<&str>,
    ) -> Result<Value, StoreError> {
        let rows=self.client.query(
            "WITH recent AS (SELECT build_key FROM builds WHERE ($1::TEXT IS NULL OR project=$1) ORDER BY received_at DESC LIMIT $2)
             SELECT t.name,COUNT(*)::bigint,AVG(t.seconds),MAX(t.seconds),
                    SUM(CASE WHEN t.fetched_from_cache THEN 1 ELSE 0 END)::bigint
             FROM build_targets t JOIN recent r ON r.build_key=t.build_key
             GROUP BY t.name ORDER BY AVG(t.seconds) DESC LIMIT $3",
            &[&project,&builds,&top])?;
        Ok(
            serde_json::json!({"items":rows.iter().map(|r|serde_json::json!({"name":r.get::<_,String>(0),"observations":r.get::<_,i64>(1),"avg_seconds":r.get::<_,Option<f64>>(2),"max_seconds":r.get::<_,Option<f64>>(3),"cached_builds":r.get::<_,i64>(4)})).collect::<Vec<_>>()}),
        )
    }

    /// Where the time goes by build phase, averaged over recent builds.
    pub fn phase_trend(
        &mut self,
        builds: i64,
        top: i64,
        project: Option<&str>,
    ) -> Result<Value, StoreError> {
        let rows=self.client.query(
            "WITH recent AS (SELECT build_key FROM builds WHERE ($1::TEXT IS NULL OR project=$1) ORDER BY received_at DESC LIMIT $2)
             SELECT p.name,COUNT(*)::bigint,AVG(p.seconds),MAX(p.seconds)
             FROM build_phases p JOIN recent r ON r.build_key=p.build_key
             GROUP BY p.name ORDER BY AVG(p.seconds) DESC LIMIT $3",
            &[&project,&builds,&top])?;
        Ok(
            serde_json::json!({"items":rows.iter().map(|r|serde_json::json!({"name":r.get::<_,String>(0),"observations":r.get::<_,i64>(1),"avg_seconds":r.get::<_,Option<f64>>(2),"max_seconds":r.get::<_,Option<f64>>(3)})).collect::<Vec<_>>()}),
        )
    }

    /// Slowest individual files to compile. `occurrences` separates "this file
    /// is slow" from "this file compiles once per architecture".
    pub fn slowest_files(
        &mut self,
        builds: i64,
        limit: i64,
        project: Option<&str>,
    ) -> Result<Value, StoreError> {
        let rows=self.client.query(
            "WITH recent AS (SELECT build_key FROM builds WHERE ($1::TEXT IS NULL OR project=$1) ORDER BY received_at DESC LIMIT $2)
             SELECT f.file,MAX(f.target),COUNT(*)::bigint,AVG(f.seconds),MAX(f.seconds),SUM(f.occurrences)::bigint
             FROM build_files f JOIN recent r ON r.build_key=f.build_key
             GROUP BY f.file ORDER BY AVG(f.seconds) DESC LIMIT $3",
            &[&project,&builds,&limit])?;
        Ok(
            serde_json::json!({"items":rows.iter().map(|r|serde_json::json!({"file":r.get::<_,String>(0),"target":r.get::<_,Option<String>>(1),"observations":r.get::<_,i64>(2),"avg_seconds":r.get::<_,Option<f64>>(3),"max_seconds":r.get::<_,Option<f64>>(4),"compilations":r.get::<_,Option<i64>>(5)})).collect::<Vec<_>>()}),
        )
    }

    /// Slowest Swift function bodies / type-check sites. Empty unless the
    /// project builds with the -warn-long-* flags, which the payload reports
    /// so the dashboard can say "not enabled" instead of "nothing slow".
    pub fn slowest_swift_timings(
        &mut self,
        builds: i64,
        limit: i64,
        project: Option<&str>,
    ) -> Result<Value, StoreError> {
        let rows=self.client.query(
            "WITH recent AS (SELECT build_key FROM builds WHERE ($1::TEXT IS NULL OR project=$1) ORDER BY received_at DESC LIMIT $2)
             SELECT s.file,s.line,MAX(s.symbol),s.kind,COUNT(*)::bigint,AVG(s.milliseconds),MAX(s.milliseconds),MAX(s.target)
             FROM build_swift_timings s JOIN recent r ON r.build_key=s.build_key
             GROUP BY s.file,s.line,s.kind ORDER BY AVG(s.milliseconds) DESC LIMIT $3",
            &[&project,&builds,&limit])?;
        Ok(
            serde_json::json!({"items":rows.iter().map(|r|serde_json::json!({"file":r.get::<_,String>(0),"line":r.get::<_,i32>(1),"symbol":r.get::<_,Option<String>>(2),"kind":r.get::<_,String>(3),"observations":r.get::<_,i64>(4),"avg_milliseconds":r.get::<_,Option<f64>>(5),"max_milliseconds":r.get::<_,Option<f64>>(6),"target":r.get::<_,Option<String>>(7)})).collect::<Vec<_>>()}),
        )
    }

    /// Warning and error counts per build, so a rising warning count is
    /// visible before it becomes an error.
    pub fn diagnostic_trend_for(
        &mut self,
        limit: i64,
        project: Option<&str>,
    ) -> Result<Value, StoreError> {
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
        Ok(
            serde_json::json!({"items":rows.iter().map(|r|serde_json::json!({"id":r.get::<_,String>(0),"recorded_at":r.get::<_,i64>(1),"warnings":r.get::<_,i64>(2),"errors":r.get::<_,i64>(3),"unique_diagnostics":r.get::<_,i64>(4)})).collect::<Vec<_>>()}),
        )
    }

    /// The diagnostics appearing in the most builds — the recurring ones worth
    /// fixing, as opposed to a one-off from a single broken build.
    pub fn diagnostic_clusters(
        &mut self,
        builds: i64,
        limit: i64,
        project: Option<&str>,
    ) -> Result<Value, StoreError> {
        let rows=self.client.query(
            "WITH recent AS (SELECT build_key FROM builds WHERE ($1::TEXT IS NULL OR project=$1) ORDER BY received_at DESC LIMIT $2)
             SELECT d.fingerprint,MAX(d.severity),MAX(d.category),MAX(d.message),MAX(d.file),COUNT(DISTINCT d.build_key)::bigint,SUM(d.occurrences)::bigint
             FROM build_diagnostics d JOIN recent r ON r.build_key=d.build_key
             GROUP BY d.fingerprint ORDER BY COUNT(DISTINCT d.build_key) DESC,SUM(d.occurrences) DESC LIMIT $3",
            &[&project,&builds,&limit])?;
        Ok(
            serde_json::json!({"items":rows.iter().map(|r|serde_json::json!({"fingerprint":r.get::<_,String>(0),"severity":r.get::<_,Option<String>>(1),"category":r.get::<_,Option<String>>(2),"message":r.get::<_,Option<String>>(3),"file":r.get::<_,Option<String>>(4),"builds":r.get::<_,i64>(5),"occurrences":r.get::<_,Option<i64>>(6)})).collect::<Vec<_>>()}),
        )
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
    pub fn flaky_tests(
        &mut self,
        builds: i64,
        limit: i64,
        project: Option<&str>,
    ) -> Result<Value, StoreError> {
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
        Ok(
            serde_json::json!({"items":rows.iter().map(|r|{let runs=r.get::<_,i64>(2);let failed=r.get::<_,i64>(3);let passed=r.get::<_,i64>(4);let retried=r.get::<_,i64>(6);serde_json::json!({"suite":r.get::<_,String>(0),"test":r.get::<_,String>(1),"runs":runs,"failed":failed,"passed":passed,"flaky":failed>0&&passed>0,"retried_builds":retried,"avg_seconds":r.get::<_,Option<f64>>(5)})}).collect::<Vec<_>>()}),
        )
    }

    /// Targets whose average duration in the most recent builds is materially
    /// worse than the window before them. This is a trend signal, not the
    /// baseline-matched regression detection history has always done — it makes
    /// no claim about environment matching, so the dashboard labels it as such.
    pub fn target_regressions(
        &mut self,
        builds: i64,
        limit: i64,
        project: Option<&str>,
    ) -> Result<Value, StoreError> {
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
        Ok(
            serde_json::json!({"items":rows.iter().map(|r|{let recent=r.get::<_,f64>(1);let prior=r.get::<_,f64>(2);serde_json::json!({"name":r.get::<_,String>(0),"current_seconds":recent,"previous_seconds":prior,"delta_seconds":recent-prior,"delta_percent":(recent-prior)/prior*100.0,"observations":r.get::<_,i64>(3),"confidence":"trend"})}).collect::<Vec<_>>()}),
        )
    }

    /// Machine, Xcode and platform mix across recent builds. A duration shift
    /// that lines up with an Xcode upgrade is an environment change, not a
    /// code regression, and this is what lets a developer tell them apart.
    pub fn environment_breakdown(
        &mut self,
        builds: i64,
        project: Option<&str>,
    ) -> Result<Value, StoreError> {
        let rows=self.client.query(
            "WITH recent AS (SELECT xcode_version,platform,architecture,machine_id,total_seconds
                             FROM builds WHERE ($1::TEXT IS NULL OR project=$1) ORDER BY received_at DESC LIMIT $2)
             SELECT COALESCE(xcode_version,'unknown'),COALESCE(platform,'unknown'),COALESCE(architecture,'unknown'),
                    COUNT(*)::bigint,COUNT(DISTINCT machine_id)::bigint,AVG(total_seconds)
             FROM recent GROUP BY 1,2,3 ORDER BY COUNT(*) DESC",
            &[&project,&builds])?;
        Ok(
            serde_json::json!({"items":rows.iter().map(|r|serde_json::json!({"xcode_version":r.get::<_,String>(0),"platform":r.get::<_,String>(1),"architecture":r.get::<_,String>(2),"builds":r.get::<_,i64>(3),"machines":r.get::<_,i64>(4),"avg_seconds":r.get::<_,Option<f64>>(5)})).collect::<Vec<_>>()}),
        )
    }

    /// Build counts per person, for the opt-in identified attribution tier.
    ///
    /// Rows with no `build_user` are excluded rather than bucketed as
    /// "unknown": a team part-way through opting in would otherwise show one
    /// giant anonymous bar that says nothing, and counting non-participants
    /// alongside participants invites reading the result as a full picture
    /// when it is only the identified subset.
    ///
    /// Returns an empty list where nobody has opted in, which is the normal
    /// case and is what the panel renders its empty state from.
    pub fn people_breakdown(
        &mut self,
        builds: i64,
        project: Option<&str>,
    ) -> Result<Value, StoreError> {
        let rows=self.client.query(
            "WITH recent AS (SELECT build_user,build_host,machine_id,total_seconds,status
                             FROM builds WHERE ($1::TEXT IS NULL OR project=$1) AND build_user IS NOT NULL
                             ORDER BY received_at DESC LIMIT $2)
             SELECT build_user,COUNT(*)::bigint,COUNT(DISTINCT COALESCE(build_host,machine_id))::bigint,
                    AVG(total_seconds),COUNT(*) FILTER (WHERE status='failed')::bigint
             FROM recent GROUP BY 1 ORDER BY COUNT(*) DESC",
            &[&project,&builds])?;
        Ok(
            serde_json::json!({"items":rows.iter().map(|r|serde_json::json!({
            "user":r.get::<_,String>(0),"builds":r.get::<_,i64>(1),"machines":r.get::<_,i64>(2),
            "avg_seconds":r.get::<_,Option<f64>>(3),"failed":r.get::<_,i64>(4)})).collect::<Vec<_>>()}),
        )
    }

    /// Builds older than `keep_days`, excluding the newest build of each
    /// project. Retention orders by `recorded_at`, not `id`: `collect --all`
    /// backfills old logs under new ids, so a higher id can hold an older build.
    ///
    /// Returns each build's day alongside its key, because every table is
    /// partitioned by day and a DELETE that names one prunes to a single
    /// partition instead of scanning every one.
    fn prunable_builds(&mut self, keep_days: u32) -> Result<Vec<(String, String)>, StoreError> {
        let rows = self.client.query(
            "WITH newest AS (
                 SELECT DISTINCT ON (project) build_key FROM builds
                 ORDER BY project, received_at DESC
             )
             SELECT day::TEXT, build_key FROM builds
             WHERE received_at < now() - ($1::INTEGER * INTERVAL '1 day')
               AND build_key NOT IN (SELECT build_key FROM newest)
             ORDER BY received_at",
            &[&(keep_days as i32)],
        )?;
        Ok(rows.iter().map(|row| (row.get(0), row.get(1))).collect())
    }

    /// Days whose every build is prunable, so the day can be dropped whole.
    ///
    /// A day holding a build that must be preserved — one inside the retention
    /// window, or the newest of its project — is excluded. Dropping a partition
    /// is all-or-nothing, and losing a project's newest build would silently
    /// break every regression comparison for it, which is the one thing
    /// `prune`'s row-wise path goes out of its way to protect.
    fn droppable_days(&mut self, keep_days: u32) -> Result<Vec<String>, StoreError> {
        let rows = self.client.query(
            "WITH newest AS (
                 SELECT DISTINCT ON (project) day, build_key FROM builds
                 ORDER BY project, received_at DESC
             )
             SELECT day::TEXT FROM builds
             GROUP BY day
             HAVING bool_and(received_at < now() - ($1::INTEGER * INTERVAL '1 day'))
                AND NOT bool_or((day, build_key) IN (SELECT day, build_key FROM newest))
             ORDER BY day",
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
        )?
        else {
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
        )?
        else {
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
    ///
    /// Exact, including the days `prune` will drop whole: every build in a
    /// droppable day is prunable by construction — that is what makes the day
    /// droppable — so this can never under-report what `prune` removes.
    pub fn prune_preview(&mut self, keep_days: u32) -> Result<Vec<String>, StoreError> {
        Ok(self
            .prunable_builds(keep_days)?
            .into_iter()
            .map(|(_, key)| key)
            .collect())
    }

    /// Deletes builds older than `keep_days`, keeping the newest build of each
    /// project so regression baselines never lose their chain.
    ///
    /// Two paths, because the tables are partitioned by day. A day whose every
    /// build is prunable is dropped as eight `DROP TABLE`s, which is a catalog
    /// update rather than a scan; whatever remains is deleted row-wise, one
    /// statement per day so the constant `day` prunes to a single partition.
    /// Before partitions existed this was eight DELETEs that each scanned their
    /// whole table.
    ///
    /// Not atomic across the two paths, deliberately: one transaction would hold
    /// ACCESS EXCLUSIVE on every dropped partition for the length of the
    /// row-wise deletes. A crash in between leaves fewer builds deleted than the
    /// return value claims, and re-running finishes the job.
    pub fn prune(&mut self, keep_days: u32) -> Result<usize, StoreError> {
        let doomed = self.prunable_builds(keep_days)?;
        if doomed.is_empty() {
            return Ok(0);
        }
        // Counted before anything is dropped, so the number reported matches
        // what `prune_preview` showed. An inconsistent count on a destructive
        // command is how a user stops trusting the preview.
        let total = doomed.len();

        let droppable: std::collections::HashSet<String> =
            self.droppable_days(keep_days)?.into_iter().collect();
        for day in &droppable {
            // Own transaction per day: the locks are released as each day
            // finishes rather than being held until the last one does.
            let mut tx = self.client.transaction()?;
            for table in DAY_PARTITIONED_TABLES {
                let child = partition_name(table, day);
                // IF EXISTS because a day can predate the per-day partitions and
                // still have its rows in the default, where the row-wise path
                // below catches them.
                tx.execute(format!("DROP TABLE IF EXISTS {child}").as_str(), &[])
                    .map_err(|source| StoreError::Query {
                        operation: "dropping a day partition",
                        source,
                    })?;
            }
            tx.commit()?;
        }

        // Whatever the drops did not cover: builds in days that hold something
        // worth keeping, and rows still in a default partition.
        let mut by_day: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
        for (day, key) in &doomed {
            by_day.entry(day.as_str()).or_default().push(key.as_str());
        }
        let mut tx = self.client.transaction()?;
        for (day, keys) in by_day {
            let keys: Vec<String> = keys.iter().map(|key| (*key).to_owned()).collect();
            for table in BUILD_SCOPED_TABLES {
                tx.execute(
                    format!(
                        "DELETE FROM {table}
                         WHERE day = to_date($1,'YYYY-MM-DD') AND build_key = ANY($2)"
                    )
                    .as_str(),
                    &[&day, &keys],
                )
                .map_err(|source| StoreError::Query {
                    operation: "pruning child rows",
                    source,
                })?;
            }
            tx.execute(
                "DELETE FROM builds WHERE day = to_date($1,'YYYY-MM-DD') AND build_key = ANY($2)",
                &[&day, &keys],
            )
            .map_err(|source| StoreError::Query {
                operation: "pruning builds",
                source,
            })?;
        }
        tx.commit()?;
        Ok(total)
    }

    pub fn git_context(&mut self, builds: i64, project: Option<&str>) -> Result<Value, StoreError> {
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
        Ok(
            serde_json::json!({"items":rows.iter().map(|r|serde_json::json!({"id":r.get::<_,String>(0),"recorded_at":r.get::<_,i64>(1),"total_seconds":r.get::<_,f64>(2),"category":r.get::<_,String>(3),"branch":r.get::<_,Option<String>>(4),"commit":r.get::<_,Option<String>>(5),"dirty":r.get::<_,Option<String>>(6)})).collect::<Vec<_>>()}),
        )
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
    "build_steps",
    "build_metadata",
];

/// Writes one build's steps.
///
/// `ordinal` is the row's index in the transmitted (timeline-ordered) list
/// rather than anything the log carries: it exists to make the primary key
/// unique, because a fingerprint repeats within a build. Numbering by position
/// also means reading rows back in `ordinal` order reproduces the timeline
/// exactly, including steps whose timestamps are missing.
fn insert_steps(tx: &mut Transaction<'_>, build: &WireBuild, day: &str) -> Result<(), StoreError> {
    for (ordinal, step) in build.steps.iter().enumerate() {
        tx.execute(
            "INSERT INTO build_steps (day,build_key,ordinal,fingerprint,step_type,title,file,target,
                                      architecture,seconds,started_at,ended_at,fetched_from_cache,executed)
             VALUES (to_date($1,'YYYY-MM-DD'),$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)
             ON CONFLICT DO NOTHING",
            &[
                &day,
                &build.build_key,
                &(ordinal as i32),
                &step.fingerprint,
                &step.step_type,
                &step.title,
                &step.file,
                &step.target,
                &step.architecture,
                &step.seconds,
                &step.started_at,
                &step.ended_at,
                &step.fetched_from_cache,
                &step.executed,
            ],
        )
        .map_err(|source| StoreError::Query { operation: "inserting build step", source })?;
    }
    Ok(())
}

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
                                 architecture,error_count,warning_count,status,build_user,build_host)
             VALUES ($1,to_date($2,'YYYY-MM-DD'),$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17)
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
                // Null unless the client chose the identified tier; the wire
                // type already discarded these for every weaker tier.
                &build.user,
                &build.host,
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
        .map_err(|source| StoreError::Query {
            operation: "inserting target",
            source,
        })?;
    }
    for phase in &build.phases {
        tx.execute(
            "INSERT INTO build_phases (day,build_key,name,seconds)
             VALUES (to_date($1,'YYYY-MM-DD'),$2,$3,$4) ON CONFLICT DO NOTHING",
            &[&day, &build.build_key, &phase.name, &phase.seconds],
        )
        .map_err(|source| StoreError::Query {
            operation: "inserting phase",
            source,
        })?;
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
        .map_err(|source| StoreError::Query {
            operation: "inserting file timing",
            source,
        })?;
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
        .map_err(|source| StoreError::Query {
            operation: "inserting swift timing",
            source,
        })?;
    }
    insert_steps(tx, build, day)?;
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
        .map_err(|source| StoreError::Query {
            operation: "inserting diagnostic",
            source,
        })?;
    }
    // Numbered exactly as the local path numbers them, so a build that arrived
    // over the wire and the same build collected locally agree about attempts.
    let attempts = attempt_numbers(
        build
            .tests
            .iter()
            .map(|test| (test.suite.as_str(), test.name.as_str())),
    );
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
        .map_err(|source| StoreError::Query {
            operation: "inserting test result",
            source,
        })?;
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
    Ok(serde_json::to_value(value)?
        .as_str()
        .unwrap_or("unknown")
        .to_owned())
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

/// `YYYY-MM-DD` to days since the Unix epoch — the inverse of
/// [`civil_from_days`], and Hinnant's `days_from_civil` for the same reason:
/// this crate needs two date conversions and no date crate.
///
/// Fallible where its inverse is not, because the input is not always ours.
/// `day_of` produces a well-formed day, but `attach_tests` reads one back out of
/// a database column, and partition DDL formats it straight into a statement —
/// so a malformed day must be rejected rather than interpolated.
fn days_from_civil(day: &str) -> Result<i64, StoreError> {
    let malformed = || StoreError::Migration(format!("malformed day: {day}"));
    let mut parts = day.split('-');
    let (Some(y), Some(m), Some(d), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(malformed());
    };
    // Length-checked as well as parsed: `to_date` would accept "2023-1-4", and
    // a partition named from those digits would not match the day it holds.
    if (y.len(), m.len(), d.len()) != (4, 2, 2) {
        return Err(malformed());
    }
    let (year, month, day_of_month) = (
        y.parse::<i64>().map_err(|_| malformed())?,
        m.parse::<i64>().map_err(|_| malformed())?,
        d.parse::<i64>().map_err(|_| malformed())?,
    );
    if !(1..=12).contains(&month) || !(1..=31).contains(&day_of_month) {
        return Err(malformed());
    }
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day_of_month - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Ok(era * 146_097 + doe - 719_468)
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
        assert!(
            parse_migrations("--@migration 0001\nSELECT 1;\n")
                .unwrap_err()
                .contains("malformed")
        );
    }

    /// Commentary before the first marker belongs to no migration and must not
    /// be executed as SQL.
    #[test]
    fn preamble_before_the_first_marker_is_dropped() {
        let parsed =
            parse_migrations("-- file comment\n--@migration 0001 one\nSELECT 1;\n").unwrap();
        assert_eq!(parsed.len(), 1);
        assert!(!parsed[0].sql.contains("file comment"));
        assert!(parsed[0].sql.contains("SELECT 1;"));
    }

    /// Attempts number per test, not per build: two different tests both start
    /// at 1, and a repeat of one does not advance the other.
    #[test]
    fn attempts_number_each_test_independently() {
        let numbered =
            attempt_numbers([("S", "a"), ("S", "b"), ("S", "a"), ("T", "a")].into_iter());
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
        assert_eq!(
            serde_plain(&DiagnosticSeverity::Warning).unwrap(),
            "warning"
        );
        assert_eq!(
            serde_plain(&DiagnosticCategory::SwiftConcurrency).unwrap(),
            "swift_concurrency"
        );
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

    #[test]
    fn partition_names_encode_the_day_without_separators() {
        assert_eq!(partition_name("builds", "2023-11-14"), "builds_p20231114");
        assert_eq!(
            partition_name("build_tests", "2024-02-29"),
            "build_tests_p20240229"
        );
        // A child must never be mistakable for a parent or for a default: both
        // `prune_covers_every_build_scoped_table` and the partition inventory
        // filter on those shapes.
        for table in DAY_PARTITIONED_TABLES {
            let child = partition_name(table, "2023-11-14");
            assert!(
                !child.ends_with("_default"),
                "{child} must not read as a default partition"
            );
            assert!(
                !DAY_PARTITIONED_TABLES.contains(&child.as_str()),
                "{child} must not collide with a parent name"
            );
        }
    }

    #[test]
    fn next_day_crosses_month_and_year_and_leap_boundaries() {
        assert_eq!(next_day("2023-11-14").unwrap(), "2023-11-15");
        assert_eq!(next_day("2023-11-30").unwrap(), "2023-12-01");
        assert_eq!(next_day("2023-12-31").unwrap(), "2024-01-01");
        // 2024 is a leap year and 2023 is not, so February ends differently.
        assert_eq!(next_day("2024-02-28").unwrap(), "2024-02-29");
        assert_eq!(next_day("2023-02-28").unwrap(), "2023-03-01");
    }

    /// The DDL formats a day straight into a statement, so a malformed one must
    /// be rejected rather than interpolated. `attach_tests` reads its day from a
    /// database column, which is why this is not merely defensive.
    #[test]
    fn next_day_rejects_a_malformed_day() {
        for bad in [
            "",
            "2023-11",
            "2023-11-14-1",
            "not-a-date",
            "2023-1-4",
            "2023-13-01",
            "2023-11-00",
        ] {
            assert!(next_day(bad).is_err(), "{bad:?} must not parse as a day");
        }
    }

    #[test]
    fn days_from_civil_inverts_civil_from_days() {
        for days in [-719_162_i64, -1, 0, 1, 19_000, 19_782, 100_000] {
            let civil = civil_from_days(days);
            assert_eq!(
                days_from_civil(&civil).unwrap(),
                days,
                "{civil} round-trips"
            );
        }
    }

    #[test]
    fn the_partition_window_names_every_day_inclusive() {
        let days = window_days("2023-11-14", 2, 2);
        assert_eq!(
            days,
            vec![
                "2023-11-12",
                "2023-11-13",
                "2023-11-14",
                "2023-11-15",
                "2023-11-16"
            ],
            "both ends are included and the anchor is in the middle"
        );
        assert_eq!(window_days("2023-11-14", 7, 7).len(), 15);
        assert_eq!(window_days("2023-11-14", 0, 0), vec!["2023-11-14"]);
    }

    /// The schema and [`DAY_PARTITIONED_TABLES`] must agree exactly. A ninth
    /// partitioned table added to the schema but not to the constant would never
    /// get per-day partitions — every one of its rows would land in the default,
    /// silently, which is the state this whole change exists to fix.
    #[test]
    fn every_day_partitioned_table_is_declared_partitioned_in_the_schema() {
        let declared: Vec<&str> = SCHEMA_SQL
            .lines()
            .filter(|line| line.contains("PARTITION BY RANGE (day)"))
            .filter_map(|line| {
                line.split_once("CREATE TABLE IF NOT EXISTS ")?
                    .1
                    .split_whitespace()
                    .next()
            })
            .collect();
        assert_eq!(
            declared.len(),
            DAY_PARTITIONED_TABLES.len(),
            "the schema declares {declared:?} as partitioned by day, but the constant lists \
             {DAY_PARTITIONED_TABLES:?}"
        );
        for table in &declared {
            assert!(
                DAY_PARTITIONED_TABLES.contains(table),
                "{table} is partitioned by day but is missing from DAY_PARTITIONED_TABLES, \
                 so no per-day partition is ever created for it"
            );
            assert!(
                SCHEMA_SQL.contains(&format!("{table}_default PARTITION OF {table} DEFAULT")),
                "{table} has no default partition, so a build dated outside the pre-created \
                 window would fail to insert"
            );
        }
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
        assert_eq!(
            serde_plain(&SwiftTimingKind::TypeCheck).unwrap(),
            "type_check"
        );
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
        assert!(
            !regressed(0.2, 0.4),
            "a 0.2s step doubling is not a regression"
        );
        // Absolute alone is not enough either, once the constant is 10%: a
        // 100s target gaining 0.6s is 0.6%.
        assert!(!regressed(100.0, 100.6));
        // And exactly at both thresholds counts, so the boundary is inclusive.
        assert!(regressed(
            5.0,
            5.0 + 5.0 * TARGET_REGRESSION_MIN_PERCENT / 100.0
        ));
    }
}
