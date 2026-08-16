#!/usr/bin/env bash
# Start everything BuildLens needs on one machine: PostgreSQL, the dashboard,
# and the collector that imports each build as it happens.
#
#   ./scripts/start.sh          # start all three, follow the logs
#   ./scripts/start.sh --stop   # stop the dashboard and watcher
#
# PostgreSQL keeps running after --stop, because dropping it would take your
# build history offline; `podman compose down` stops it, and `down -v` deletes
# the data.
set -euo pipefail

cd "$(dirname "$0")/.."

DB="${BUILDLENS_DATABASE_URL:-postgres://buildlens:buildlens@localhost:5433/buildlens}"
PORT="${BUILDLENS_PORT:-8787}"
BUILD_DIR="${BUILDLENS_BUILD_DIR:-$HOME/Library/Developer/Xcode/DerivedData}"
# Matches container_name in docker-compose.yml. docker-compose.dev.yml sets
# none, so db_ready falls back to a direct connection.
DB_CONTAINER="${BUILDLENS_DB_CONTAINER:-buildlens_db}"
RUN_DIR="${TMPDIR:-/tmp}/buildlens-run"
mkdir -p "$RUN_DIR"

say() { printf '%s\n' "$*"; }

# Frees a port held by a *buildlens* process this script did not start — a
# dashboard run by hand leaves no pidfile, so stop_one cannot see it and the new
# one would die with "Address already in use".
#
# Only our own process is ever killed. Killing whatever happens to hold the port
# would silently terminate an unrelated program the user is running, which is a
# far worse outcome than refusing to start.
free_port() {
  local port="${1:?free_port needs a port}" pids pid name ours=""
  pids="$(lsof -nP -iTCP:"$port" -sTCP:LISTEN -t 2>/dev/null || true)"
  [[ -z "$pids" ]] && return 0

  for pid in $pids; do
    name="$(ps -o comm= -p "$pid" 2>/dev/null | xargs basename 2>/dev/null || true)"
    if [[ "$name" == "buildlens" ]]; then
      ours="$ours $pid"
    else
      say "error: port $port is held by '$name' (pid $pid), which BuildLens did not start."
      say "       Stop it, or run with BUILDLENS_PORT=<other port>."
      exit 1
    fi
  done

  say "    port $port was held by an earlier buildlens, stopping it"
  # shellcheck disable=SC2086
  kill $ours 2>/dev/null || true
  for _ in $(seq 1 20); do
    lsof -nP -iTCP:"$port" -sTCP:LISTEN -t >/dev/null 2>&1 || return 0
    sleep 0.25
  done
  say "error: port $port is still in use; stop that process and retry."
  exit 1
}

stop_one() {
  local name="${1:?stop_one needs a process name}"
  local pidfile="$RUN_DIR/$name.pid"
  if [[ -f "$pidfile" ]] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then
    kill "$(cat "$pidfile")" 2>/dev/null || true
    say "stopped $name"
  fi
  rm -f "$pidfile"
}

if [[ "${1:-}" == "--stop" ]]; then
  stop_one dashboard
  stop_one watcher
  say "PostgreSQL is still running — 'podman compose down' stops it."
  exit 0
fi

# Prefer podman, fall back to docker; either provides `compose`.
CONTAINER_CLI=""
for candidate in podman docker; do
  if command -v "$candidate" >/dev/null 2>&1; then CONTAINER_CLI="$candidate"; break; fi
done
if [[ -z "$CONTAINER_CLI" ]]; then
  say "error: neither podman nor docker found; one is needed to run PostgreSQL."
  exit 1
fi

# Whether PostgreSQL is accepting connections.
#
# Asks the database directly rather than `exec`-ing into a container by name:
# docker-compose.dev.yml sets no container_name, so a name-based probe reports
# "not ready" for a database that is running perfectly well, and the script
# waits a minute before failing. This also works when PostgreSQL is not in a
# container at all.
db_ready() {
  "$CONTAINER_CLI" exec "$DB_CONTAINER" pg_isready -U buildlens -d buildlens >/dev/null 2>&1 \
    || pg_isready -d "$DB" >/dev/null 2>&1
}

say "==> PostgreSQL"
# Only bring the stack up when the database is not already serving. `compose
# up` may hand off to an external provider that *recreates* the container, and
# a recreate reinitialises the data directory — i.e. it destroys build history.
# An already-running database is left strictly alone.
if db_ready; then
  say "    already running, leaving it alone"
else
  "$CONTAINER_CLI" compose up -d
fi

# The container reports healthy slightly before it accepts connections, so wait
# on a real query rather than on container state.
printf '    waiting for the database'
for _ in $(seq 1 60); do
  if db_ready; then
    ready=1; break
  fi
  printf '.'
  sleep 1
done
printf '\n'
if [[ "${ready:-}" != "1" ]]; then
  say "error: database did not become ready. Check '$CONTAINER_CLI compose logs postgres'."
  exit 1
fi

say "==> Building the CLI"
cargo build --quiet

# One dashboard, one port. `buildlens-server` is the same UI plus a /v1/metrics
# endpoint for receiving builds from *other* machines; it is not needed to look
# at your own builds, so it is not started here.
stop_one dashboard
stop_one watcher
free_port "$PORT"

say "==> Dashboard on http://127.0.0.1:$PORT"
./target/debug/buildlens dashboard --db "$DB" --port "$PORT" \
  >"$RUN_DIR/dashboard.log" 2>&1 &
echo $! >"$RUN_DIR/dashboard.pid"

say "==> Watching $BUILD_DIR"
# --watch-interval 2 rather than the 5s default: this watcher exists so a build
# shows up while you are still looking at the dashboard, and a scan is a cheap
# directory stat.
./target/debug/buildlens collect --watch --collect-all --watch-interval 2 \
  --build-dir "$BUILD_DIR" --db "$DB" >"$RUN_DIR/watcher.log" 2>&1 &
echo $! >"$RUN_DIR/watcher.pid"

sleep 3
for name in dashboard watcher; do
  if ! kill -0 "$(cat "$RUN_DIR/$name.pid")" 2>/dev/null; then
    say "error: $name exited immediately. Its log:"
    tail -20 "$RUN_DIR/$name.log"
    # Do not leave the other half running: a lone watcher with no dashboard
    # looks like everything is fine until you open the page.
    stop_one dashboard
    stop_one watcher
    exit 1
  fi
done

say ""
say "Ready. Build in Xcode and the build appears at http://127.0.0.1:$PORT"
say "Logs:  $RUN_DIR/{dashboard,watcher}.log"
say "Stop:  ./scripts/start.sh --stop"
say ""
say "Following the watcher — Ctrl-C leaves everything running."
tail -f "$RUN_DIR/watcher.log"
