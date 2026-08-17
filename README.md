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

There is one dashboard and one port: **8787**. `buildlens dashboard` and the
`buildlens-server` binary run the *same* server — one implementation, so the
two cannot drift. They differ only in how they are configured: the local
command binds loopback with no token, because a password you set for yourself
guards nothing, while `buildlens-server` reads its settings from the
environment and refuses to start without one.

So the local dashboard also exposes `/v1/metrics`, and pointing another machine
at it with `collect --server <url>` works. What a client transmits is the
explicit `buildlens_core::wire::WireBuild` document: build timings, target and
phase names, and optional hardware facts — never source paths, per-file
timings, or log contents. Locally collected builds keep that detail, because
`collect --db` writes to Postgres directly rather than over the wire.
</details>

## Running the team server in a container

Only for a shared backend — collecting your own builds needs none of this.

The server receives already-parsed metrics: clients parse locally and push a
JSON document under 1 MiB, so there is no slow server-side work and no queue,
worker or object store to run. Postgres is the only dependency.

```sh
printf 'BUILDLENS_TOKEN=%s\n' "$(openssl rand -hex 32)" > .env
podman compose -f docker-compose.server.yml up -d --build
curl -fsS http://127.0.0.1:8788/health          # {"status":"ok"}
```

The server also serves the dashboard at **http://127.0.0.1:8788/** — the same
UI the local `buildlens dashboard` shows, so a team can read the fleet's builds
without running anything. Paste the token into the field in the header on first
load; the page keeps it in `localStorage` for later visits.

`docker-compose.server.yml` runs Postgres *and* the server;
`docker-compose.yml` runs Postgres alone and is what the local workflow above
uses. Both share the `buildlens-pgdata` volume, so a machine that has been
collecting locally keeps its history when it starts serving a team.

Push a build to it:

```sh
cargo run -- collect --project MyApp --collect-all \
  --server http://127.0.0.1:8788 --token "$(grep BUILDLENS_TOKEN .env | cut -d= -f2)"
```

**Configuration.** `BUILDLENS_TOKEN` is required: without it the server refuses
to start rather than accepting writes from anyone who can reach the port. To run
unauthenticated on a trusted network, opt out explicitly with
`BUILDLENS_ALLOW_ANONYMOUS=1`. `BUILDLENS_POOL_SIZE` and `BUILDLENS_THREADS`
(both 8) size the Postgres connection pool and the worker threads; threads above
pool size only block waiting for a connection. `/v1/metrics` is rate limited to
120 requests per minute per source address.

Two routes answer without a token, and neither returns build data: `/health`,
because a container healthcheck has no credential to present (it also skips
Postgres, so a brief database blip does not restart a working server), and `/`,
the dashboard's HTML shell — a browser navigating to a URL cannot attach an
`Authorization` header, so gating the page would make it unreachable rather
than protected. Every `/api/*` call the page then makes is authenticated. If
nobody should reach the page at all, that is a network control: a VPN, or a
proxy in front.

**What the server dashboard shows.** Builds, durations, percentiles, targets
and phases — everything a pushed `WireBuild` carries. The Files, Swift,
Diagnostics and Tests panels render empty, because that detail is written only
by a local `collect --db` and is deliberately not transmitted to a team server.

**No TLS.** The token crosses the network in cleartext, so keep this on a
trusted network or a VPN, or terminate TLS in a reverse proxy in front of it.
The compose file binds `127.0.0.1:8788`; change it to `8788:8788` to accept
connections from other machines, and understand the above before you do.

<details>
<summary>Deploying to a Synology NAS</summary>

The NAS is x86_64 while an Apple Silicon Mac is not, so build for the target
architecture explicitly:

```sh
podman build --platform linux/amd64 -t buildlens:local .
podman save buildlens:local | ssh nas 'docker load'
```

Then copy `docker-compose.server.yml` and a `.env` holding the token to the NAS
and start it from Container Manager or `docker compose`. Postgres is not
published to the host in that file, both to keep the surface small and because
DSM ships its own PostgreSQL that may already hold port 5433.

Base images come from `ghcr.io` rather than Docker Hub. Both stages are Debian
12 and share a glibc — mixing distributions between builder and runtime
produces a binary that will not start.
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

### Collecting from `xcodebuild` (terminal and CI)

**`xcodebuild` often writes no build log at all.** Xcode.app always writes one;
`xcodebuild` frequently does not, even when it recompiles — a build with four
compile tasks can still leave `Logs/Build/` empty. The symptom is silence: the
watcher polls, nothing appears, and there is nothing to find because nothing
was written.

Two flags fix it, for two different reasons:

```sh
xcodebuild -scheme MyApp -destination '...' \
  -derivedDataPath build/DerivedData \
  -resultBundlePath build/result.xcresult \
  build

cargo run -- collect --build-dir build/DerivedData --db "$DB"
```

`-resultBundlePath` is what makes Xcode write the `.xcactivitylog`.
`-derivedDataPath` controls *where* it lands, keeping it out of the shared
`~/Library/Developer/Xcode/DerivedData/Build/` where no `Logs/Build` exists.
Whether `-derivedDataPath` alone suffices varies by project, so pass both.

`-resultBundlePath` fails if the bundle already exists — remove it first, or
use a fresh path per run.

Two logs that are *not* builds and are deliberately skipped: `Logs/Package`
entries (SPM dependency resolution), and the log `xcodebuild clean` writes,
which times a second or two and compiles nothing. Recording either would put a
phantom entry beside real builds of the same project.

When a DerivedData root holds only a shared `Build/` and no logs anywhere,
BuildLens names that case rather than reporting an empty result — from
`collect`, and once at watcher startup.

`--build-dir` is otherwise optional. When Xcode sets `$BUILD_DIR` — in a scheme
post-action, or any build script Xcode spawns — the log directory is resolved
from it by searching upward for a `Logs/Build` sibling. That is one code path
for local Xcode.app builds, terminal `xcodebuild`, and CI: the same variable and
the same search, with no per-caller special cases. It follows
`-derivedDataPath` wherever you point it, and handles archive builds, whose
`$BUILD_DIR` sits several levels deeper than a normal build's.

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
