# Architecture

BuildLens turns Xcode build logs into queryable history. This document covers
the parts where the design is not obvious from the code: the activity-log
decoder, the storage layout, and the boundaries between crates.

For what the tool does and how to run it, see the [README](README.md).

## The shape of the thing

A workspace of small crates, all depending on `buildlens-core`, which holds the
shared data types and no logic.

```
                      ┌──────────────────┐
   .xcactivitylog ───▶│ buildlens-metrics│──┐
                      └──────────────────┘  │
                      ┌──────────────────┐  │   ┌──────────────┐
   text build log ───▶│ buildlens-parser │──┼──▶│buildlens-core│
                      └──────────────────┘  │   │  (types)     │
                      ┌──────────────────┐  │   └──────┬───────┘
                      │buildlens-diagnos.│──┘          │
                      └──────────────────┘             ▼
                                              ┌─────────────────┐
                                              │buildlens-storage│
                                              │   (Postgres)    │
                                              └────────┬────────┘
                                       ┌───────────────┴────────────┐
                                       ▼                            ▼
                              ┌─────────────────┐        ┌──────────────────┐
                              │buildlens-dashbd.│        │ buildlens-server │
                              │   127.0.0.1     │        │   team backend   │
                              └─────────────────┘        └──────────────────┘
```

Two inputs produce one type. A text log and an `.xcactivitylog` carry
overlapping but unequal information — the activity log has precise per-step
timings and no compiler diagnostic text; the text log has the diagnostics and
only coarse timings. Both normalise into `BuildMetrics`, so everything
downstream is written once.

## Decoding `.xcactivitylog`

Xcode writes build logs as gzipped **SLF0**, an undocumented serialisation of
the `IDEActivityLog` object graph. Nothing reads it but Xcode, so the format is
reverse-engineered and can change between releases without notice.

The decoder is split in two, which is the main design decision worth
explaining.

### The lexer knows nothing about Xcode

[`slf/lexer.rs`](crates/buildlens-metrics/src/slf/lexer.rs) turns bytes into
tokens and stops there. A token is a hex-digit prefix followed by a one-byte
sigil that says what the prefix meant:

| Sigil | Meaning | Prefix is |
| --- | --- | --- |
| `#` | integer | the value |
| `^` | double | IEEE-754 bits, or empty for null |
| `"` | string | a byte length that follows |
| `*` | JSON blob | a byte length that follows |
| `%` | class name | a byte length that follows |
| `@` | class reference | an index into the class table |
| `(` | list | element count |
| `-` | null | empty |

Strings are returned as `&[u8]` slices borrowed from the input, never copied.
The log is decompressed once into a buffer and the object graph points into it.
Decompression is bounded by `MAX_DECOMPRESSED_BYTES`, so a corrupt or hostile
log cannot expand into unbounded memory.

### The parser knows nothing about bytes

[`slf/parser.rs`](crates/buildlens-metrics/src/slf/parser.rs) consumes tokens
and builds `IdeActivityLog`. It handles class references, nested sections,
and the fields that only exist past a given format version —
`ATTACHMENTS_MIN_VERSION = 11`.

The split matters because the two halves fail differently. A malformed byte
stream is unrecoverable and returns `SlfError`. An *unexpected shape* — a field
Xcode added, a section nested somewhere new — is recoverable: the parser records
a warning and carries on with what it understood. A newer format version than
`MAX_KNOWN_VERSION` warns rather than refuses, because a partially-understood
log is worth more than an error, and Xcode ships new versions on its own
schedule.

That distinction is why the decoder survived Xcode 26 changing the log's
structure. See `collect_targets_from_steps` in
[`activity.rs`](crates/buildlens-metrics/src/activity.rs): Xcode 26 stopped
wrapping build steps in a per-target section, leaving them as flat siblings of
the root. The normal walk finds no targets at all and every per-target number
comes out empty. Rather than special-casing a version, the mapper notices it
recovered zero targets and rebuilds them from the steps themselves — a fallback
that assumes no particular nesting and therefore does not care which Xcode
release caused the flattening.

### Time

Activity logs use `CFAbsoluteTime` — seconds since 2001-01-01, not 1970. The
offset is applied once, at the mapping boundary
([`slf/mod.rs`](crates/buildlens-metrics/src/slf/mod.rs)), so no downstream
code carries two notions of an epoch.

## Diagnostic fingerprinting

The same warning fires once per translation unit that includes the header, so a
raw count is meaningless. `buildlens-diagnostics` normalises each diagnostic —
column numbers dropped, paths redacted, the parts that vary between otherwise
identical occurrences removed — and hashes the result into a fingerprint. Equal
fingerprints collapse into one row carrying an occurrence count.

Fingerprints are **persisted and compared across builds**, which makes them a
compatibility surface: they are built from `as_str`, never `{:?}`, because a
derived `Debug` representation is not a stable contract and a Rust upgrade that
reformatted it would silently split every historical diagnostic into two.

## Storage

One schema, one implementation, shared by the local dashboard and the team
server. `buildlens-server` once carried its own copy covering only builds,
targets and phases, and a server that migrated a fresh database hit `relation
does not exist` the moment a client pushed a build carrying per-file detail.
The schema now lives in one place and the server delegates to it.

### Everything is partitioned by day

Nine tables are `PARTITION BY RANGE (day)`. Retention is the reason: `history
prune --keep-days N` drops whole partitions instead of issuing a `DELETE` that
would scan, write tombstones, and leave the table needing a vacuum.

The list of partitioned tables is a constant, `DAY_PARTITIONED_TABLES`, and a
test asserts it matches the tables the schema actually declares partitioned. A
tenth partitioned table cannot be added without the suite failing — schema drift
between a `CREATE TABLE` and the code that manages its children is exactly the
kind of bug that stays invisible until a prune quietly misses a table.

Partitions are created with `CREATE TABLE ... (LIKE parent)` followed by `ALTER
TABLE ... ATTACH PARTITION`, deliberately not the shorter `CREATE TABLE ...
PARTITION OF`: Postgres scans the default partition on that form.

They are also created **after** migrations, never before. Migration 0002 adds
`attempt` to `build_tests`' primary key — so a test Xcode retried inside one
build is recorded as two rows rather than being dropped by `ON CONFLICT DO
NOTHING`, which is what makes fail-then-pass flakiness visible at all. Postgres
names a partitioned table's primary key after the table and offers no `ALTER
... ALTER CONSTRAINT`, so the constraint is dropped and recreated. A child
partition created ahead of that would be born with the old key.

### Two advisory locks, not one

`MIGRATION_LOCK` serialises schema migration. `PARTITION_LOCK` serialises
per-day partition DDL, and its second key is the day — so two processes
creating partitions for *different* days never wait on each other, and a long
migration on one database does not block an ingest that only needs today's
partition.

The pre-creation window is one day either side of today, not a week. Every
`connect` runs it under the migration lock and each day costs eight
`CREATE`/`ATTACH` pairs, so a wide window turns every CLI invocation and every
test into hundreds of DDL statements serialised against each other — measured at
**3.5× slower on a 35-test suite**, for partitions nothing was going to write
to. The window only needs to cover the days a running process will be asked for
without reconnecting: today, tomorrow for a clock running fast or a process
alive over midnight, and yesterday for a build that finished just before it.
A backfill dated last month is handled on the write path instead, paying one
short DDL for the first build of that day and nothing after.

### `LISTEN`/`NOTIFY` for liveness

The local setup is two processes: `collect --watch` writes, the dashboard
reads. The dashboard cannot learn about a build by any in-process means, and
originally found out only when its response cache expired — so a finished build
did not appear until you reloaded the page.

Postgres is the one thing both processes already share, so the announcement
goes through it, on channel `buildlens_builds`. Three details:

- **Sent after commit, never inside the transaction.** A `NOTIFY` inside a
  transaction is queued until commit anyway, but sending it afterwards also
  means no listener is woken for a build that then rolls back.
- **`pg_notify($1, $2)`, not `NOTIFY`,** whose channel and payload must be
  literals. A build key is Xcode's string, not ours, and does not belong in
  concatenated SQL.
- **Failure is logged once per process and swallowed.** This is a latency
  optimisation layered on polling that still works; refusing to store a build
  because its announcement failed would trade a real feature for a cosmetic
  one.

The same reasoning covers the collector: FSEvents is the fast path for noticing
a finished log, and the periodic scan remains underneath it. Every push-based
mechanism here has a poll behind it, so a missed event costs latency rather
than a lost build.

## What crosses the network

Nothing, unless `--server` is passed.

When it is, a client parses locally and pushes one explicit document —
`buildlens_core::wire::WireBuild` — reviewable in a single file, so no field
starts travelling by accident. The server receives already-parsed metrics
under 1 MiB, which is why there is no queue, worker, or object store: Postgres
is the only dependency.

Wire version 2 widened that document to carry full build detail, **source
paths, diagnostic text and test names included**, so a team dashboard shows
what a local one does; version 1 sent totals only. The raw log never leaves the
machine either way.

## Testing

The parts that fail silently get explicit tests:

- **The dashboard bundle is generated and committed.** Editing
  `dashboard-ui/src` without rebuilding leaves a stale UI that still compiles
  and still passes every Rust test, so CI runs `node dashboard-ui/build.mjs
  --check`.
- **Postgres tests skip themselves when `BUILDLENS_TEST_DATABASE_URL` is
  unset**, which is right for a laptop and wrong for CI. CI sets
  `BUILDLENS_REQUIRE_DATABASE=1`, turning a missing URL into a failure rather
  than a green run that exercised nothing. Each test takes its own schema, so
  they run concurrently without interfering.
- **Fixtures cover the failure modes**, not just the happy path:
  `graph-cycle.log`, `malformed.log`, `codesign-failure.log`,
  `test-crash.log`, `test-retry.log`, `xctest-memory-leak.log`.
