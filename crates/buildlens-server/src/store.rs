use postgres::{Client, NoTls};
use r2d2_postgres::PostgresConnectionManager;
use thiserror::Error;

/// Everything the store can fail with.
///
/// Only database failures: payload validation happens before the store is
/// reached, so there is no "rejected" case here. Callers must treat this as
/// internal — `postgres::Error`'s `Display` can name tables, constraints and
/// connection details, none of which belong in a response.
#[derive(Debug, Error)]
pub enum ServerError {
    #[error("database error: {0}")]
    Database(#[from] postgres::Error),
    /// The pool could not hand out a connection within its timeout — every
    /// connection is busy, or Postgres is unreachable. Distinct from
    /// [`ServerError::Database`] because the cause is capacity, not a query.
    #[error("no database connection available: {0}")]
    Pool(#[from] r2d2::Error),
    /// `migrations.sql` could not be parsed. A programming error rather than a
    /// runtime one — the file is compiled in — so it surfaces at startup,
    /// before any traffic, rather than leaving a migration quietly unapplied.
    #[error("invalid migration definition: {0}")]
    Migration(String),
}

/// A pool of Postgres connections shared by every worker thread.
///
/// This replaced a single `Mutex<Client>`. With one connection behind a global
/// lock, every request — reads included — serialised against every other, so a
/// slow analytical query blocked ingest behind it. Connections are cheap
/// relative to that contention, and each request borrows one for its duration.
pub type Pool = r2d2::Pool<PostgresConnectionManager<NoTls>>;

/// One borrowed connection, returned to the pool when dropped.
pub type PooledConnection = r2d2::PooledConnection<PostgresConnectionManager<NoTls>>;

/// Builds the pool. Does not run migrations — see [`migrate`], which the
/// caller runs once on a dedicated connection before serving traffic.
pub fn pool(url: &str, size: u32) -> Result<Pool, ServerError> {
    let manager = PostgresConnectionManager::new(url.parse::<postgres::Config>()?, NoTls);
    // `min_idle(1)` keeps one connection warm so the first request after an
    // idle period does not pay connection setup. The pool reconnects on its
    // own if Postgres restarts under us, which is the other reason this is a
    // pool rather than N long-lived clients.
    Ok(r2d2::Pool::builder()
        .max_size(size)
        .min_idle(Some(1))
        .build(manager)?)
}

/// Idempotent schema creation, run once at startup.
///
/// Deliberately not called per-connection: with a pool, that would re-run
/// every ALTER on every connection the pool opens, for no benefit. The
/// advisory lock still guards against *other* BuildLens processes starting
/// against the same database concurrently.
///
/// `builds` is partitioned by day so that dropping old data becomes a
/// partition drop and day-scoped queries can skip whole ranges. Note that
/// only a DEFAULT partition is created here: until per-day partitions are
/// added, every row lands in the default and neither benefit applies. The
/// declaration is in place so adding them later needs no table rewrite.
pub fn migrate(client: &mut Client) -> Result<(), ServerError> {
    // Delegated to `buildlens-storage`, which owns the schema and the
    // migration ledger. This function used to carry its own copy of the
    // migration logic; two implementations of one schema is what let a server
    // and a local collector disagree about what tables exist. The advisory
    // lock and the versioned migrations both live there now.
    buildlens_storage::migrate(client).map_err(|error| match error {
        buildlens_storage::StoreError::Database(source) => ServerError::Database(source),
        other => ServerError::Migration(other.to_string()),
    })
}

/// Connects once and applies the schema. Used at startup and by tests.
pub fn connect_and_migrate(url: &str) -> Result<Client, ServerError> {
    let mut client = Client::connect(url, NoTls)?;
    migrate(&mut client)?;
    Ok(client)
}


/// The query surface, over one connection borrowed from the [`Pool`] for the
/// life of a single request.
///
/// Holds a `&mut Client` rather than owning one: the connection belongs to the
/// pool, and tying the borrow to this struct's lifetime is what guarantees it
/// goes back when the request ends.
pub struct PostgresStore<'a> {
    client: &'a mut Client,
}

impl<'a> PostgresStore<'a> {
    /// Wraps a borrowed connection. Runs no migrations — the schema is applied
    /// once at startup by [`migrate`].
    pub fn new(client: &'a mut Client) -> Self {
        Self { client }
    }

    // Writing is deliberately absent here.
    //
    // Ingest goes through `buildlens_storage::PostgresStore::insert`, the same
    // writer a local `collect --db` uses. This struct once had its own
    // narrower INSERT that knew nothing about file timings, Swift timings,
    // diagnostics or tests, so a build pushed to a team server silently lost
    // the detail it carried while a locally collected one kept it. Two writers
    // for one schema is what made that possible; there is now one.

    /// Recent builds, newest first.
    pub fn builds(&mut self, limit: i64) -> Result<serde_json::Value, ServerError> {
        let rows = self.client.query(
            "SELECT build_key, day::TEXT, project, category, total_seconds,
                    cache_hit_rate, machine_id, xcode_version, platform, architecture,
                    COALESCE(started_at, EXTRACT(EPOCH FROM received_at)),
                    error_count, warning_count, status
             FROM builds ORDER BY received_at DESC LIMIT $1",
            &[&limit],
        )?;
        let items: Vec<serde_json::Value> = rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "build_key": row.get::<_, String>(0),
                    "day": row.get::<_, String>(1),
                    "project": row.get::<_, String>(2),
                    "category": row.get::<_, String>(3),
                    "total_seconds": row.get::<_, f64>(4),
                    "cache_hit_rate": row.get::<_, Option<f64>>(5),
                    "machine_id": row.get::<_, Option<String>>(6),
                    "xcode_version": row.get::<_, Option<String>>(7),
                    "platform": row.get::<_, Option<String>>(8),
                    "architecture": row.get::<_, Option<String>>(9),
                    "recorded_at": row.get::<_, f64>(10),
                    "error_count": row.get::<_, i32>(11),
                    "warning_count": row.get::<_, i32>(12),
                    "status": row.get::<_, Option<String>>(13),
                })
            })
            .collect();
        Ok(serde_json::json!({ "items": items }))
    }

    /// Per-day p50/p95 per project — the fleet view Postgres exists for.
    pub fn stats(&mut self, days: i64) -> Result<serde_json::Value, ServerError> {
        let rows = self.client.query(
            "SELECT project, day::TEXT, COUNT(*)::BIGINT,
                    percentile_cont(0.5) WITHIN GROUP (ORDER BY total_seconds),
                    percentile_cont(0.95) WITHIN GROUP (ORDER BY total_seconds),
                    COUNT(DISTINCT machine_id)::BIGINT
             FROM builds
             -- Inclusive: `>` would make days=30 cover 29 days.
             WHERE day >= (CURRENT_DATE - $1::INTEGER)
             GROUP BY project, day
             ORDER BY project, day",
            &[&(days as i32)],
        )?;
        let items: Vec<serde_json::Value> = rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "project": row.get::<_, String>(0),
                    "day": row.get::<_, String>(1),
                    "builds": row.get::<_, i64>(2),
                    "p50": row.get::<_, Option<f64>>(3),
                    "p95": row.get::<_, Option<f64>>(4),
                    "machines": row.get::<_, i64>(5),
                })
            })
            .collect();
        Ok(serde_json::json!({ "items": items }))
    }

    /// Everything the dashboard's build-detail page renders: the build row
    /// plus its targets and phases. Without the child rows the page loads and
    /// shows nothing, so they belong in the same call.
    pub fn build_detail(&mut self, key: &str) -> Result<Option<serde_json::Value>, ServerError> {
        let rows = self.client.query(
            "SELECT build_key, day::TEXT, project, category, total_seconds,
                    cache_hit_rate, machine_id, xcode_version, platform, architecture,
                    COALESCE(started_at, EXTRACT(EPOCH FROM received_at)), compiled_count,
                    error_count, warning_count, status
             FROM builds WHERE build_key = $1 LIMIT 1",
            &[&key],
        )?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let targets = self.client.query(
            "SELECT name, seconds, category, fetched_from_cache, compiled_count
             FROM build_targets WHERE build_key = $1 ORDER BY seconds DESC",
            &[&key],
        )?;
        let phases = self.client.query(
            "SELECT name, seconds FROM build_phases WHERE build_key = $1
             ORDER BY seconds DESC",
            &[&key],
        )?;
        Ok(Some(serde_json::json!({
            "id": row.get::<_, String>(0),
            "build_key": row.get::<_, String>(0),
            "day": row.get::<_, String>(1),
            "project": row.get::<_, String>(2),
            "category": row.get::<_, String>(3),
            "total_seconds": row.get::<_, f64>(4),
            "cache_hit_rate": row.get::<_, Option<f64>>(5),
            "machine_id": row.get::<_, Option<String>>(6),
            "xcode_version": row.get::<_, Option<String>>(7),
            "platform": row.get::<_, Option<String>>(8),
            "architecture": row.get::<_, Option<String>>(9),
            "recorded_at": row.get::<_, f64>(10),
            "compiled_count": row.get::<_, i32>(11),
            "error_count": row.get::<_, i32>(12),
            "warning_count": row.get::<_, i32>(13),
            // Xcode's verdict verbatim; absent stays absent.
            "status": row.get::<_, Option<String>>(14),
            "targets": targets.iter().map(|t| serde_json::json!({
                "name": t.get::<_, String>(0),
                "seconds": t.get::<_, f64>(1),
                "category": t.get::<_, Option<String>>(2),
                "fetched_from_cache": t.get::<_, Option<bool>>(3),
                "compiled_count": t.get::<_, Option<i32>>(4),
            })).collect::<Vec<_>>(),
            "phases": phases.iter().map(|p| serde_json::json!({
                "name": p.get::<_, String>(0),
                "seconds": p.get::<_, f64>(1),
            })).collect::<Vec<_>>(),
        })))
    }

    /// Targets ranked by average seconds over the most recent `builds` builds,
    /// optionally scoped to one project. This is the dashboard's "Slowest
    /// targets" panel; `slowest_targets` below answers the fleet-wide question
    /// over a day window instead.
    pub fn ranked_targets(
        &mut self,
        builds: i64,
        top: i64,
        project: Option<&str>,
    ) -> Result<serde_json::Value, ServerError> {
        let rows = self.client.query(
            "WITH recent AS (
                 SELECT build_key FROM builds
                 WHERE ($3::TEXT IS NULL OR project = $3)
                 ORDER BY received_at DESC LIMIT $1
             )
             SELECT t.name, AVG(t.seconds), MAX(t.seconds), COUNT(*)::BIGINT
             FROM build_targets t JOIN recent r ON r.build_key = t.build_key
             GROUP BY t.name ORDER BY AVG(t.seconds) DESC LIMIT $2",
            &[&builds, &top, &project],
        )?;
        Ok(ranked(rows))
    }

    /// Phases ranked the same way — where the build spends its time.
    pub fn ranked_phases(
        &mut self,
        builds: i64,
        top: i64,
        project: Option<&str>,
    ) -> Result<serde_json::Value, ServerError> {
        let rows = self.client.query(
            "WITH recent AS (
                 SELECT build_key FROM builds
                 WHERE ($3::TEXT IS NULL OR project = $3)
                 ORDER BY received_at DESC LIMIT $1
             )
             SELECT p.name, AVG(p.seconds), MAX(p.seconds), COUNT(*)::BIGINT
             FROM build_phases p JOIN recent r ON r.build_key = p.build_key
             GROUP BY p.name ORDER BY AVG(p.seconds) DESC LIMIT $2",
            &[&builds, &top, &project],
        )?;
        Ok(ranked(rows))
    }

    /// p50/p95 over recent builds. `enough_history` gates the dashboard tile so
    /// a percentile is never quoted from two builds.
    pub fn percentiles(
        &mut self,
        limit: i64,
        project: Option<&str>,
    ) -> Result<serde_json::Value, ServerError> {
        let rows = self.client.query(
            "WITH recent AS (
                 SELECT total_seconds FROM builds
                 WHERE ($2::TEXT IS NULL OR project = $2)
                 ORDER BY received_at DESC LIMIT $1
             )
             SELECT COUNT(*)::BIGINT,
                    percentile_cont(0.5) WITHIN GROUP (ORDER BY total_seconds),
                    percentile_cont(0.95) WITHIN GROUP (ORDER BY total_seconds)
             FROM recent",
            &[&limit, &project],
        )?;
        let row = rows.first();
        let builds = row.map(|r| r.get::<_, i64>(0)).unwrap_or(0);
        Ok(serde_json::json!({
            "builds": builds,
            "enough_history": builds >= 5,
            "p50": row.and_then(|r| r.get::<_, Option<f64>>(1)),
            "p95": row.and_then(|r| r.get::<_, Option<f64>>(2)),
        }))
    }

    /// Per-calendar-day medians, for the week-over-week panel.
    pub fn daily(
        &mut self,
        limit: i64,
        project: Option<&str>,
    ) -> Result<serde_json::Value, ServerError> {
        let rows = self.client.query(
            "SELECT day::TEXT, COUNT(*)::BIGINT,
                    percentile_cont(0.5) WITHIN GROUP (ORDER BY total_seconds),
                    percentile_cont(0.95) WITHIN GROUP (ORDER BY total_seconds)
             FROM builds WHERE ($2::TEXT IS NULL OR project = $2)
             GROUP BY day ORDER BY day DESC LIMIT $1",
            &[&limit, &project],
        )?;
        let items: Vec<serde_json::Value> = rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "day": row.get::<_, String>(0),
                    "builds": row.get::<_, i64>(1),
                    "p50": row.get::<_, Option<f64>>(2),
                    "p95": row.get::<_, Option<f64>>(3),
                })
            })
            .collect();
        Ok(serde_json::json!({ "items": items }))
    }

    /// Slowest targets across the fleet, which is the query a single machine
    /// cannot answer.
    pub fn slowest_targets(&mut self, days: i64, limit: i64) -> Result<serde_json::Value, ServerError> {
        let rows = self.client.query(
            "SELECT name, COUNT(*)::BIGINT, AVG(seconds), MAX(seconds)
             FROM build_targets
             -- Inclusive, matching `stats`.
             WHERE day >= (CURRENT_DATE - $1::INTEGER)
             GROUP BY name ORDER BY AVG(seconds) DESC LIMIT $2",
            &[&(days as i32), &limit],
        )?;
        let items: Vec<serde_json::Value> = rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "name": row.get::<_, String>(0),
                    "observations": row.get::<_, i64>(1),
                    "avg_seconds": row.get::<_, Option<f64>>(2),
                    "max_seconds": row.get::<_, Option<f64>>(3),
                })
            })
            .collect();
        Ok(serde_json::json!({ "items": items }))
    }
}

/// Shared row shape for the ranked-bar panels (name, avg, max, observations).
fn ranked(rows: Vec<postgres::Row>) -> serde_json::Value {
    let items: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            serde_json::json!({
                "name": row.get::<_, String>(0),
                "avg_seconds": row.get::<_, Option<f64>>(1),
                "max_seconds": row.get::<_, Option<f64>>(2),
                "observations": row.get::<_, i64>(3),
            })
        })
        .collect();
    serde_json::json!({ "items": items })
}



/// A real `ServerError::Database`, for tests that assert how store failures
/// are reported. Connecting to a closed port is the cheapest way to obtain a
/// genuine `postgres::Error` rather than a stand-in.
#[cfg(test)]
pub fn connect_error_for_tests() -> ServerError {
    Client::connect("postgres://127.0.0.1:1/nodb", NoTls)
        .err()
        .expect("connecting to a closed port fails")
        .into()
}

/// A real `ServerError::Pool`, for the test asserting that pool exhaustion is
/// reported as an internal failure rather than described to the client. A pool
/// pointed at a closed port fails its connection timeout.
#[cfg(test)]
pub fn pool_error_for_tests() -> ServerError {
    let manager = PostgresConnectionManager::new(
        "postgres://127.0.0.1:1/nodb".parse().expect("valid config"),
        NoTls,
    );
    r2d2::Pool::builder()
        .max_size(1)
        .min_idle(Some(0))
        .connection_timeout(std::time::Duration::from_millis(50))
        .build_unchecked(manager)
        .get()
        .err()
        .expect("connecting to a closed port fails")
        .into()
}
