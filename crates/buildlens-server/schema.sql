-- Shared PostgreSQL schema. `builds`, `build_targets` and `build_phases` hold
-- what a team server receives over the wire; the tables below them hold the
-- richer detail that only ever comes from a local `collect`, which reads the
-- activity log directly and is not bound by the wire payload.
--
-- Everything is partitioned by day so that dropping old data becomes a
-- partition drop and day-scoped queries can skip whole ranges. Only the DEFAULT
-- partitions are declared here; the per-day ones are created by
-- `buildlens_storage::ensure_day_partitions`, because this file runs before the
-- migrations and cannot name a day anyway. A default that stays empty is the
-- healthy state — it is the safety net for a build dated outside the pre-created
-- window, not where rows are meant to live.
--
-- This file IS executed, by both `buildlens_server::store::migrate` and
-- `buildlens_storage::PostgresStore::migrate`, which `include_str!` it. One
-- definition means the two cannot disagree about what a BuildLens database
-- contains.
--
-- Everything here is the *baseline*: statements that are safe to re-run
-- unconditionally, because each is `IF NOT EXISTS`. Changes that cannot be
-- expressed that way — dropping a constraint, rewriting a primary key,
-- backfilling a column — belong in `migrations.sql` instead, which runs after
-- this file and records what it has already applied. Adding such a change
-- here would either fail on the second startup or silently skip on the first.
CREATE TABLE IF NOT EXISTS builds (build_key TEXT NOT NULL, day DATE NOT NULL, project TEXT NOT NULL, category TEXT NOT NULL, total_seconds DOUBLE PRECISION NOT NULL, compiled_count INTEGER NOT NULL, cache_hit_rate DOUBLE PRECISION, started_at DOUBLE PRECISION, machine_id TEXT, xcode_version TEXT, platform TEXT, architecture TEXT, received_at TIMESTAMPTZ NOT NULL DEFAULT now(), PRIMARY KEY (day, build_key)) PARTITION BY RANGE (day);
CREATE TABLE IF NOT EXISTS builds_default PARTITION OF builds DEFAULT;
CREATE TABLE IF NOT EXISTS build_targets (day DATE NOT NULL, build_key TEXT NOT NULL, name TEXT NOT NULL, seconds DOUBLE PRECISION NOT NULL, category TEXT NOT NULL, fetched_from_cache BOOLEAN NOT NULL, compiled_count INTEGER NOT NULL, PRIMARY KEY (day, build_key, name)) PARTITION BY RANGE (day);
CREATE TABLE IF NOT EXISTS build_targets_default PARTITION OF build_targets DEFAULT;
CREATE TABLE IF NOT EXISTS build_phases (day DATE NOT NULL, build_key TEXT NOT NULL, name TEXT NOT NULL, seconds DOUBLE PRECISION NOT NULL, PRIMARY KEY (day, build_key, name)) PARTITION BY RANGE (day);
CREATE TABLE IF NOT EXISTS build_phases_default PARTITION OF build_phases DEFAULT;
CREATE INDEX IF NOT EXISTS idx_builds_project_day ON builds (project, day);
CREATE INDEX IF NOT EXISTS idx_targets_name ON build_targets (name);

-- Almost every dashboard panel is a "last N builds" window: `ORDER BY
-- received_at DESC LIMIT n`, optionally scoped to one project. The dashboard
-- fires fifteen of them every couple of seconds, and none of them had an index
-- to walk -- `idx_builds_project_day` leads with `project` and then `day`, which
-- answers a day-range question, not a recency one. Without these, each panel
-- sorted the whole table to return thirty rows.
--
-- Two indexes rather than one because the queries come in both shapes: the
-- unscoped feed (`/api/summary`) reads straight down the first, and a
-- project-scoped panel seeks into the second. A single `(project, received_at)`
-- cannot serve the unscoped form, since `project` leads it.
--
-- DESC to match the queries' direction. Postgres can walk an ASC index
-- backwards, so this is not strictly required, but stating it keeps the index
-- and the query shape obviously aligned.
CREATE INDEX IF NOT EXISTS idx_builds_received_at ON builds (received_at DESC);
CREATE INDEX IF NOT EXISTS idx_builds_project_received_at ON builds (project, received_at DESC);

-- Added after the first release, so these are ALTERs rather than columns in
-- the CREATE above: an existing database already has the table, and
-- CREATE TABLE IF NOT EXISTS silently skips a changed column list.
ALTER TABLE builds ADD COLUMN IF NOT EXISTS error_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE builds ADD COLUMN IF NOT EXISTS warning_count INTEGER NOT NULL DEFAULT 0;
-- Xcode's own verdict. Nullable: a text log states no verdict, and NULL must
-- read as unknown rather than as success.
ALTER TABLE builds ADD COLUMN IF NOT EXISTS status TEXT;
-- The scheme a build ran, from the activity log's preparation section. Absent
-- for text logs, which never name one.
ALTER TABLE builds ADD COLUMN IF NOT EXISTS scheme TEXT;


-- Local-only detail below. A team server never receives these rows: source
-- paths and per-file timings expose a repository's layout, which is exactly
-- why `buildlens_core::wire::WireBuild` omits them. Writing them here is safe
-- because a `collect` run stores to a database the developer chose.

-- Per-file compile timings. Keyed by architecture as well as name, because
-- Xcode compiles a file once per architecture and collapsing those rows turns
-- "compiles four times" into "is slow".
CREATE TABLE IF NOT EXISTS build_files (day DATE NOT NULL, build_key TEXT NOT NULL, file TEXT NOT NULL, architecture TEXT NOT NULL DEFAULT '', seconds DOUBLE PRECISION NOT NULL, target TEXT, step_type TEXT NOT NULL, occurrences INTEGER NOT NULL, PRIMARY KEY (day, build_key, file, architecture)) PARTITION BY RANGE (day);
CREATE TABLE IF NOT EXISTS build_files_default PARTITION OF build_files DEFAULT;

-- Swift function-body / expression type-check timings. Only present when the
-- project builds with -warn-long-function-bodies or
-- -warn-long-expression-type-checking, so an empty table is normal, not a bug.
CREATE TABLE IF NOT EXISTS build_swift_timings (day DATE NOT NULL, build_key TEXT NOT NULL, kind TEXT NOT NULL, file TEXT NOT NULL, line INTEGER NOT NULL, column_number INTEGER NOT NULL, symbol TEXT, milliseconds DOUBLE PRECISION NOT NULL, target TEXT, PRIMARY KEY (day, build_key, kind, file, line, column_number)) PARTITION BY RANGE (day);
CREATE TABLE IF NOT EXISTS build_swift_timings_default PARTITION OF build_swift_timings DEFAULT;

-- Deduplicated diagnostics, one row per fingerprint per build, matching how
-- the parser already collapses them.
CREATE TABLE IF NOT EXISTS build_diagnostics (day DATE NOT NULL, build_key TEXT NOT NULL, fingerprint TEXT NOT NULL, severity TEXT NOT NULL, category TEXT NOT NULL, occurrences INTEGER NOT NULL, message TEXT NOT NULL, file TEXT, line INTEGER, target TEXT, PRIMARY KEY (day, build_key, fingerprint)) PARTITION BY RANGE (day);
CREATE TABLE IF NOT EXISTS build_diagnostics_default PARTITION OF build_diagnostics DEFAULT;

-- Test results, so flaky and repeatedly-failing tests are visible over time.
--
-- The `attempt` column and its place in the primary key are added by migration
-- 0002 rather than declared here, so that a database created before that
-- migration and one created after it end up with the same key. A fresh
-- database gets this CREATE and then immediately the migration.
CREATE TABLE IF NOT EXISTS build_tests (day DATE NOT NULL, build_key TEXT NOT NULL, suite TEXT NOT NULL, name TEXT NOT NULL, status TEXT NOT NULL, seconds DOUBLE PRECISION, message TEXT, PRIMARY KEY (day, build_key, suite, name)) PARTITION BY RANGE (day);
CREATE TABLE IF NOT EXISTS build_tests_default PARTITION OF build_tests DEFAULT;

-- Collected metadata (git branch/commit/dirty, hardware facts, CI provider,
-- user tags) as key/value pairs, mirroring the local plugin namespaces.
CREATE TABLE IF NOT EXISTS build_metadata (day DATE NOT NULL, build_key TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY (day, build_key, key)) PARTITION BY RANGE (day);
CREATE TABLE IF NOT EXISTS build_metadata_default PARTITION OF build_metadata DEFAULT;

-- Partition inventory, for answering "is partitioning actually working here".
--
-- `estimated_rows` is `reltuples`: an estimate, and -1 until the table has been
-- analyzed, so it is not a count and must not be reported as one. The number
-- worth watching is a non-empty `*_default` in steady state, which means builds
-- are arriving dated outside the pre-created window.
CREATE OR REPLACE VIEW buildlens_partitions AS
SELECT parent.relname AS parent,
       child.relname AS partition,
       pg_get_expr(child.relpartbound, child.oid) AS bounds,
       child.reltuples::bigint AS estimated_rows,
       pg_total_relation_size(child.oid) AS bytes
FROM pg_inherits
JOIN pg_class child ON child.oid = pg_inherits.inhrelid
JOIN pg_class parent ON parent.oid = pg_inherits.inhparent
-- Scoped by name to BuildLens's own partitioned tables, and by `to_regclass` to
-- the current `search_path`: a database holding several BuildLens schemas (which
-- is how the tests isolate themselves) must not have one schema's view report
-- another's partitions.
WHERE parent.relname IN ('builds', 'build_targets', 'build_phases', 'build_files',
                         'build_swift_timings', 'build_diagnostics', 'build_tests',
                         'build_metadata')
  AND parent.oid = to_regclass(parent.relname)
ORDER BY parent.relname, child.relname;

CREATE INDEX IF NOT EXISTS idx_files_file ON build_files (file);
CREATE INDEX IF NOT EXISTS idx_swift_file ON build_swift_timings (file);
CREATE INDEX IF NOT EXISTS idx_diagnostics_fingerprint ON build_diagnostics (fingerprint);
CREATE INDEX IF NOT EXISTS idx_tests_name ON build_tests (suite, name);
CREATE INDEX IF NOT EXISTS idx_metadata_key ON build_metadata (key);

-- Every detail panel joins a child table to a "recent builds" CTE on
-- `build_key` alone. The primary keys all lead with `day`, so `build_key` is not
-- a usable prefix of any of them and each join fell back to a full scan of the
-- child -- worst on `build_files`, which carries up to 500 rows per build.
--
-- `prune`'s row-wise path benefits too: it deletes by `(day, build_key)`, and
-- these give it an index for the second half of that predicate.
CREATE INDEX IF NOT EXISTS idx_targets_build ON build_targets (build_key);
CREATE INDEX IF NOT EXISTS idx_phases_build ON build_phases (build_key);
CREATE INDEX IF NOT EXISTS idx_files_build ON build_files (build_key);
CREATE INDEX IF NOT EXISTS idx_swift_build ON build_swift_timings (build_key);
CREATE INDEX IF NOT EXISTS idx_diagnostics_build ON build_diagnostics (build_key);
CREATE INDEX IF NOT EXISTS idx_tests_build ON build_tests (build_key);
CREATE INDEX IF NOT EXISTS idx_metadata_build ON build_metadata (build_key);
