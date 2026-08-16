use buildlens_core::wire::WireBuild;
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
    // Serialised against every other BuildLens process on this database:
    // concurrent starts would run the same ALTER TABLEs and deadlock.
    client.execute("SELECT pg_advisory_lock($1)", &[&0x6275_696c_i64])?;
    let result = migrate_locked(client);
    let unlock = client.execute("SELECT pg_advisory_unlock($1)", &[&0x6275_696c_i64]);
    result?;
    unlock?;
    Ok(())
}

/// Connects once and applies the schema. Used at startup and by tests.
pub fn connect_and_migrate(url: &str) -> Result<Client, ServerError> {
    let mut client = Client::connect(url, NoTls)?;
    migrate(&mut client)?;
    Ok(client)
}

fn migrate_locked(client: &mut Client) -> Result<(), ServerError> {
    client.batch_execute(
            "CREATE TABLE IF NOT EXISTS builds (
                build_key TEXT NOT NULL,
                day DATE NOT NULL,
                project TEXT NOT NULL,
                category TEXT NOT NULL,
                total_seconds DOUBLE PRECISION NOT NULL,
                compiled_count INTEGER NOT NULL,
                cache_hit_rate DOUBLE PRECISION,
                started_at DOUBLE PRECISION,
                machine_id TEXT,
                xcode_version TEXT,
                platform TEXT,
                architecture TEXT,
                received_at TIMESTAMPTZ NOT NULL DEFAULT now(),
                PRIMARY KEY (day, build_key)
            ) PARTITION BY RANGE (day);

            CREATE TABLE IF NOT EXISTS builds_default PARTITION OF builds DEFAULT;

            CREATE TABLE IF NOT EXISTS build_targets (
                day DATE NOT NULL,
                build_key TEXT NOT NULL,
                name TEXT NOT NULL,
                seconds DOUBLE PRECISION NOT NULL,
                category TEXT NOT NULL,
                fetched_from_cache BOOLEAN NOT NULL,
                compiled_count INTEGER NOT NULL,
                PRIMARY KEY (day, build_key, name)
            ) PARTITION BY RANGE (day);

            CREATE TABLE IF NOT EXISTS build_targets_default PARTITION OF build_targets DEFAULT;

            CREATE TABLE IF NOT EXISTS build_phases (
                day DATE NOT NULL,
                build_key TEXT NOT NULL,
                name TEXT NOT NULL,
                seconds DOUBLE PRECISION NOT NULL,
                PRIMARY KEY (day, build_key, name)
            ) PARTITION BY RANGE (day);

            CREATE TABLE IF NOT EXISTS build_phases_default PARTITION OF build_phases DEFAULT;

            CREATE INDEX IF NOT EXISTS idx_builds_project_day ON builds (project, day);
            CREATE INDEX IF NOT EXISTS idx_targets_name ON build_targets (name);

            -- Added after the first release. These must be ALTERs, not columns
            -- in the CREATE above: an existing database already has the table,
            -- and CREATE TABLE IF NOT EXISTS silently skips a changed column
            -- list, leaving the new columns missing at INSERT time.
            ALTER TABLE builds ADD COLUMN IF NOT EXISTS error_count INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE builds ADD COLUMN IF NOT EXISTS warning_count INTEGER NOT NULL DEFAULT 0;
            -- Nullable: a text log states no verdict, and NULL must read as
            -- unknown rather than as success.
            ALTER TABLE builds ADD COLUMN IF NOT EXISTS status TEXT;
            ALTER TABLE builds ADD COLUMN IF NOT EXISTS scheme TEXT;",
    )?;
    Ok(())
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

    /// Stores a build. Re-sending the same build is a no-op, so a client that
    /// retries after a network failure never double-counts.
    pub fn insert(&mut self, build: &WireBuild) -> Result<bool, ServerError> {
        let day = day_of(build.started_at);
        let category = build.category.as_str().to_owned();
        // The column is TEXT; `as_str` is the same spelling serde writes, so
        // the stored value matches what a client sent. `None` stays NULL,
        // which reads as unknown rather than as success.
        let status = build.status.map(|status| status.as_str());
        let mut transaction = self.client.transaction()?;
        let inserted = transaction.execute(
            "INSERT INTO builds (build_key, day, project, category, total_seconds,
                                 compiled_count, cache_hit_rate, started_at, machine_id,
                                 xcode_version, platform, architecture,
                                 error_count, warning_count, status)
             VALUES ($1, to_date($2, 'YYYY-MM-DD'), $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
                     $13, $14, $15)
             ON CONFLICT (day, build_key) DO NOTHING",
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
        )?;
        if inserted == 0 {
            transaction.commit()?;
            return Ok(false);
        }
        for target in &build.targets {
            let target_category = target.category.as_str().to_owned();
            transaction.execute(
                "INSERT INTO build_targets (day, build_key, name, seconds, category,
                                            fetched_from_cache, compiled_count)
                 VALUES (to_date($1, 'YYYY-MM-DD'), $2, $3, $4, $5, $6, $7)
                 ON CONFLICT DO NOTHING",
                &[
                    &day,
                    &build.build_key,
                    &target.name,
                    &target.seconds,
                    &target_category,
                    &target.fetched_from_cache,
                    &(target.compiled_count as i32),
                ],
            )?;
        }
        for phase in &build.phases {
            transaction.execute(
                "INSERT INTO build_phases (day, build_key, name, seconds)
                 VALUES (to_date($1, 'YYYY-MM-DD'), $2, $3, $4)
                 ON CONFLICT DO NOTHING",
                &[&day, &build.build_key, &phase.name, &phase.seconds],
            )?;
        }
        transaction.commit()?;
        Ok(true)
    }

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

/// Partition day for a build, from its start time; falls back to today when
/// the log carried no timestamp.
fn day_of(started_at: Option<f64>) -> String {
    let seconds = match started_at {
        Some(seconds) if seconds.is_finite() && seconds >= 0.0 => seconds as i64,
        _ => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    };
    let days = seconds.div_euclid(86_400);
    civil_from_days(days)
}

/// Days since the Unix epoch to `YYYY-MM-DD` (Howard Hinnant's civil_from_days).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_epoch_days_to_civil_dates() {
        assert_eq!(civil_from_days(0), "1970-01-01");
        assert_eq!(civil_from_days(19_000), "2022-01-08");
        // 2024 was a leap year: day 60 of that year is Feb 29.
        assert_eq!(civil_from_days(19_782), "2024-02-29");
    }

    #[test]
    fn day_of_uses_the_build_start_time() {
        assert_eq!(day_of(Some(0.0)), "1970-01-01");
        assert_eq!(day_of(Some(1_700_000_000.0)), "2023-11-14");
        // A missing or nonsensical timestamp still yields a valid partition day.
        assert_eq!(day_of(None).len(), 10);
        assert_eq!(day_of(Some(f64::NAN)).len(), 10);
        assert_eq!(day_of(Some(-1.0)).len(), 10);
    }
}
