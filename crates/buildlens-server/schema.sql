-- Shared PostgreSQL schema. `builds`, `build_targets` and `build_phases` hold
-- what a team server receives over the wire; the tables below them hold the
-- richer detail that only ever comes from a local `collect`, which reads the
-- activity log directly and is not bound by the wire payload.
--
-- Everything is partitioned by day so that dropping old data becomes a
-- partition drop and day-scoped queries can skip whole ranges. Only DEFAULT
-- partitions are created here: until per-day partitions are added, every row
-- lands in the default and neither benefit applies.
--
-- This file documents the schema; it is not executed. `store.rs::migrate` is
-- what actually runs, and the two must be kept in step.
CREATE TABLE IF NOT EXISTS builds (build_key TEXT NOT NULL, day DATE NOT NULL, project TEXT NOT NULL, category TEXT NOT NULL, total_seconds DOUBLE PRECISION NOT NULL, compiled_count INTEGER NOT NULL, cache_hit_rate DOUBLE PRECISION, started_at DOUBLE PRECISION, machine_id TEXT, xcode_version TEXT, platform TEXT, architecture TEXT, received_at TIMESTAMPTZ NOT NULL DEFAULT now(), PRIMARY KEY (day, build_key)) PARTITION BY RANGE (day);
CREATE TABLE IF NOT EXISTS builds_default PARTITION OF builds DEFAULT;
CREATE TABLE IF NOT EXISTS build_targets (day DATE NOT NULL, build_key TEXT NOT NULL, name TEXT NOT NULL, seconds DOUBLE PRECISION NOT NULL, category TEXT NOT NULL, fetched_from_cache BOOLEAN NOT NULL, compiled_count INTEGER NOT NULL, PRIMARY KEY (day, build_key, name)) PARTITION BY RANGE (day);
CREATE TABLE IF NOT EXISTS build_targets_default PARTITION OF build_targets DEFAULT;
CREATE TABLE IF NOT EXISTS build_phases (day DATE NOT NULL, build_key TEXT NOT NULL, name TEXT NOT NULL, seconds DOUBLE PRECISION NOT NULL, PRIMARY KEY (day, build_key, name)) PARTITION BY RANGE (day);
CREATE TABLE IF NOT EXISTS build_phases_default PARTITION OF build_phases DEFAULT;
CREATE INDEX IF NOT EXISTS idx_builds_project_day ON builds (project, day);
CREATE INDEX IF NOT EXISTS idx_targets_name ON build_targets (name);

-- Added after the first release, so these are ALTERs rather than columns in
-- the CREATE above: an existing database already has the table, and
-- CREATE TABLE IF NOT EXISTS silently skips a changed column list.
ALTER TABLE builds ADD COLUMN IF NOT EXISTS error_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE builds ADD COLUMN IF NOT EXISTS warning_count INTEGER NOT NULL DEFAULT 0;
-- Nullable: a text log states no verdict, and NULL must read as unknown
-- rather than as success.
ALTER TABLE builds ADD COLUMN IF NOT EXISTS status TEXT;


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
CREATE TABLE IF NOT EXISTS build_tests (day DATE NOT NULL, build_key TEXT NOT NULL, suite TEXT NOT NULL, name TEXT NOT NULL, status TEXT NOT NULL, seconds DOUBLE PRECISION, message TEXT, PRIMARY KEY (day, build_key, suite, name)) PARTITION BY RANGE (day);
CREATE TABLE IF NOT EXISTS build_tests_default PARTITION OF build_tests DEFAULT;

-- Collected metadata (git branch/commit/dirty, hardware facts, CI provider,
-- user tags) as key/value pairs, mirroring the local plugin namespaces.
CREATE TABLE IF NOT EXISTS build_metadata (day DATE NOT NULL, build_key TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY (day, build_key, key)) PARTITION BY RANGE (day);
CREATE TABLE IF NOT EXISTS build_metadata_default PARTITION OF build_metadata DEFAULT;

-- Added after the first release, so this runs as an ALTER rather than living
-- in the CREATE above: existing databases already have the table, and
-- CREATE TABLE IF NOT EXISTS would silently skip a changed column list.
ALTER TABLE builds ADD COLUMN IF NOT EXISTS error_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE builds ADD COLUMN IF NOT EXISTS warning_count INTEGER NOT NULL DEFAULT 0;
-- Xcode's own verdict. Nullable: a text log never states one, and NULL must
-- read as unknown rather than as success.
ALTER TABLE builds ADD COLUMN IF NOT EXISTS status TEXT;
-- The scheme a build ran, from the activity log's preparation section. Absent
-- for text logs, which never name one.
ALTER TABLE builds ADD COLUMN IF NOT EXISTS scheme TEXT;

CREATE INDEX IF NOT EXISTS idx_files_file ON build_files (file);
CREATE INDEX IF NOT EXISTS idx_swift_file ON build_swift_timings (file);
CREATE INDEX IF NOT EXISTS idx_diagnostics_fingerprint ON build_diagnostics (fingerprint);
CREATE INDEX IF NOT EXISTS idx_tests_name ON build_tests (suite, name);
CREATE INDEX IF NOT EXISTS idx_metadata_key ON build_metadata (key);
