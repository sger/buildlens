# BuildLens

Deterministic intelligence for `xcodebuild` logs. Point it at a build log or an
`.xcactivitylog` and it extracts diagnostics, test results, target dependency
graphs, precise build timings, PostgreSQL-backed history and regression
detection — the same input always producing the same output, with no model in the
loop.

**Local-first is a hard constraint, not a default.** Nothing is uploaded. The
dashboard binds `127.0.0.1` only. Logs are read in place, never copied. The
optional team server is inert unless you explicitly pass `--server`.

## Install

Build from a clone:

```sh
cargo build --release          # binary at target/release/buildlens
cargo test --workspace         # confirm the build is sound
```

One binary, no services, nothing to configure. Copy `target/release/buildlens`
onto your `PATH`, or run it through `cargo run --` as the examples below do.

The dashboard UI is a React app bundled into a single HTML file at
`crates/buildlens-dashboard/assets/index.html`, which the crate embeds with
`include_str!`. That bundle is generated output, committed so a plain
`cargo build` needs no Node toolchain — do not edit it directly. Rebuild it
after editing `dashboard-ui/src`:

```sh
cd dashboard-ui && npm install && node build.mjs
node build.mjs --check      # fails if the committed bundle is stale
```

**Requirements:** macOS (activity logs, `sysctl`/`pmset` metadata) and Xcode.
A Rust toolchain ([rustup.rs](https://rustup.rs)) is needed only when building
from source; `git` is optional, for branch metadata and change correlation.

## Quick start

Try it against a fixture in this repo — no Xcode build needed:

```sh
cargo run -- analyze fixtures/sample.log
```

```
BuildLens

BUILD
Status: failed
Scheme: AppTests
Targets in build graph: 3

FAILURES
Root cause clusters: 1

WARNINGS
Raw occurrences: 3
Unique issues: 2

TESTS
Total: 2
Failed: 1
Crashed: 1
Test operation: 12.480s
```

`Raw occurrences: 3` against `Unique issues: 2` is the point: the same warning
fired twice and was deduplicated by fingerprint.

On your own project, capture a log and analyze it:

```sh
xcodebuild -scheme MyApp -showBuildTimingSummary build 2>&1 | tee build.log
cargo run -- analyze build.log
```

### Record every build automatically

One command starts PostgreSQL, the dashboard, and a collector that imports each
build as Xcode writes it:

```sh
./scripts/start.sh
```

Then build in Xcode as usual (⌘B). Each build appears at
**http://127.0.0.1:8787** within a few seconds — no command to run per build,
and builds started from Xcode's UI are captured too.

```sh
./scripts/start.sh --stop   # stop the dashboard and collector
podman compose down         # also stop PostgreSQL (history is kept)
podman compose down -v      # stop it and DELETE all recorded builds
```

The script needs `podman` or `docker` for PostgreSQL, and leaves an
already-running database untouched. Logs are under
`$TMPDIR/buildlens-run/`. Override the defaults with `BUILDLENS_DATABASE_URL`,
`BUILDLENS_PORT`, or `BUILDLENS_BUILD_DIR`.

<details>
<summary>Running the pieces separately</summary>

```sh
podman compose up -d                                    # PostgreSQL on 5433
export DB=postgres://buildlens:buildlens@localhost:5433/buildlens

cargo run -- dashboard --db "$DB"                       # the dashboard, :8787
cargo run -- collect --watch --collect-all --db "$DB"   # import builds as they happen
cargo run -- collect --project MyApp --db "$DB" --collect-all   # or import once, by hand
cargo run -- collect --all --db "$DB"                   # backfill existing logs
```

There is one dashboard and one port: **8787**. A second binary,
`buildlens-server`, serves that same UI plus a `/v1/metrics` endpoint for
*receiving* builds from other machines. It is only for a shared team backend
and is never needed to look at your own builds — run it with
`DATABASE_URL=$DB cargo run -p buildlens-server`, and point clients at it with
`collect --server <url> --token <token>`. What a client transmits is the
explicit `buildlens_core::wire::WireBuild` document: build timings, target and
phase names, and optional hardware facts — never source paths, per-file
timings, or log contents.
</details>

Diagnostics and Swift type-check timings live in the *text* log, which the
collector never sees, so those panels need a paired save:

```sh
xcodebuild -scheme MyApp -showBuildTimingSummary build 2>&1 | tee build.log
cargo run -- history save build.log --activity-log <log>.xcactivitylog \
  --project MyApp --collect-all --db "$DB"
```

Swift hotspots additionally need `-Xfrontend -warn-long-function-bodies=100
-Xfrontend -warn-long-expression-type-checking=100` in the target's
**Other Swift Flags**.

To wire collection to an Xcode scheme post-action instead of running a watcher,
see the install instructions at the top of `scripts/xcode-post-action.sh`.

## Commands

| Command | Purpose |
| --- | --- |
| `analyze [LOG]` | Full analysis of a text or activity log; `-` reads stdin |
| `metrics <INPUT>` | Timing metrics from an activity log or text log |
| `dashboard` | Local read-only dashboard on `127.0.0.1` |
| `why <TARGET> <LOG>` | Shortest dependency path explaining why a target built |
| `warnings <LOG>` | Warnings only |
| `failures <LOG>` | Failures and root-cause clusters |
| `tests <LOG>` | Test results |
| `graph <LOG>` | Target dependency graph summary |
| `history save <LOG>` | Persist a build into PostgreSQL history |
| `history compare <LOG>` | Compare against an environment-matched baseline |
| `history prune --keep-days N` | Delete old builds; previews unless `--confirm` |
| `collect` | Find the newest log, wait until fully written, save it |

Run `cargo run -- <command> --help` for the flags of any one of them.

Exit codes: `analyze --fail-on errors|warnings|failures|any` exits `2` on
violation, which is what makes it usable as a CI gate. `2` rather than `1` so a
job can tell "the analysis ran and the build is bad" from "the analysis itself
failed". The policies are deliberately distinct: `failures` covers failing and
crashed tests, `errors` covers compile and link errors — a build that never
compiled has no test results at all — and `any` covers both.

## What it does

| | |
| --- | --- |
| Services required | none — PostgreSQL only for history and the dashboard |
| Built-in dashboard | yes, on `127.0.0.1` |
| Regression detection | against an environment-matched baseline |
| Test and flaky-test parsing | XCTest and Swift Testing |
| Dependency graph | including `why <target>` |
| Retention | `history prune --keep-days N` |
| Works with no backend | yes — `analyze` needs nothing but a log |

## Notes

- `DATABASE_URL` or the `--db` value must point to PostgreSQL. No local database
  file is created by `history`, `collect`, or `dashboard`.
- Timing metrics are path-redacted (`<home>`, `<repo>`) everywhere except
  `metrics --raw`; treat that output as sensitive.
- Swift function-body and type-check timings only exist when the project builds
  with `-warn-long-function-bodies` / `-warn-long-expression-type-checking`. An
  empty panel means the flags are not set.
- `.xcresult` bundles are not activity logs and are rejected; pass the
  `.xcactivitylog` from `Logs/Build/`.

## Repository layout

A workspace of small crates, all depending on `buildlens-core`, which holds the
shared data types and no logic.

| Crate | Responsibility |
| --- | --- |
| `buildlens-core` | Shared data types (`BuildAnalysis` and friends) |
| `buildlens-parser` | Streaming text-log parser |
| `buildlens-diagnostics` | Warning/error classification and fingerprinting |
| `buildlens-metrics` | SLF0 activity-log decoder and timing model |
| `buildlens-plugins` | Optional local metadata (git, hardware, thermal, CI, tags) |
| `buildlens-git` | Change correlation and ownership |
| `buildlens-intel` | Bottleneck ranking and evidence chains |
| `buildlens-graph` | Dependency graph queries over petgraph |
| `buildlens-storage` | PostgreSQL history and dashboard queries |
| `buildlens-report` | Terminal/JSON/Markdown rendering of an analysis |
| `buildlens-dashboard` | HTTP routes and the embedded dashboard UI |
| `buildlens-tests` | Test-log formats behind one trait, over `buildlens-parser` |
| `buildlens-cli` | The `buildlens` binary |
| `buildlens-server` | Optional team backend that receives builds from other machines |
