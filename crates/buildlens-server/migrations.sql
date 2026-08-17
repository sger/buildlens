-- Ordered, versioned schema changes.
--
-- `schema.sql` holds the baseline: statements safe to re-run on every startup
-- because each is `IF NOT EXISTS`. This file holds the changes that cannot be
-- written that way — dropping a constraint, rewriting a primary key,
-- backfilling a column — each applied exactly once and recorded in
-- `schema_migrations`.
--
-- The format is deliberately plain so the file stays readable as SQL: a
-- `--@migration <version> <name>` line opens a migration, and every statement
-- until the next such line belongs to it. `store::migrations` parses this and
-- runs each unapplied version in order, inside one transaction per migration,
-- so a failure leaves the database on the last version that fully applied
-- rather than half-way through this one.
--
-- Rules for adding one:
--   * Append; never renumber or edit an applied migration. A database that
--     already ran version N will not run it again, so an edit reaches new
--     databases only and the two silently diverge.
--   * Make it idempotent where it costs nothing (`IF EXISTS`, `IF NOT
--     EXISTS`), so a half-applied database recovers rather than wedges.
--   * There is no down path on purpose. Rolling a schema backwards under a
--     running fleet loses data; roll forward with a new migration instead.

--@migration 0001 baseline
-- The starting point for databases that predate this file. `schema.sql` has
-- already created everything, so this migration exists only to record that
-- version 0001 is the state that file describes. Nothing to do.
SELECT 1;

--@migration 0002 test_attempts
-- Record every attempt of a test within one build, not just the first.
--
-- `build_tests` was keyed `(day, build_key, suite, name)` and written with
-- ON CONFLICT DO NOTHING, so when Xcode retried a failing test inside a single
-- build the retry was dropped at insert. A fail-then-pass — the most common
-- CI flakiness signal there is — was recorded as a plain failure, and the
-- parser's retry data died one step before the database.
--
-- Existing rows are all first attempts by construction: the old key made a
-- second row for the same test impossible. So the DEFAULT 1 backfill is exact
-- rather than a guess.
ALTER TABLE build_tests ADD COLUMN IF NOT EXISTS attempt INTEGER NOT NULL DEFAULT 1;

-- Postgres names a partitioned table's primary key after the table. Dropping
-- and recreating is the only way to change its column list; there is no
-- ALTER ... ALTER CONSTRAINT for this.
ALTER TABLE build_tests DROP CONSTRAINT IF EXISTS build_tests_pkey;
ALTER TABLE build_tests ADD PRIMARY KEY (day, build_key, suite, name, attempt);

--@migration 0003 replayed_steps
-- How many steps a build's log restated from an earlier build without
-- re-running them.
--
-- Xcode re-emits the whole build graph in an incremental log, keeping each
-- replayed step's original duration, so a 9.7-second rebuild can list twelve
-- thousand steps it never executed. Storing the count makes the difference
-- between a build's logged size and its actual work visible on the build page,
-- which is the number that varies on a machine with no build cache.
ALTER TABLE builds ADD COLUMN IF NOT EXISTS replayed_steps INTEGER NOT NULL DEFAULT 0;
