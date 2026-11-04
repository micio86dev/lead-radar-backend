#!/bin/sh
# One container running the API and the worker against the same SQLite volume.
#
# Railway (and any single-node host with one volume) cannot share a SQLite file between two services,
# so this entrypoint applies migrations, runs the idempotent seeds, then starts the worker in the
# background and the API in the foreground. Docker Compose keeps using the separate binaries.
set -eu

DB_BIN=/usr/local/bin/db
API_BIN=/usr/local/bin/api
WORKER_BIN=/usr/local/bin/worker

"$DB_BIN" migrate
"$DB_BIN" seed

# The token in the environment is the single source of truth: a rotation invalidates the previous
# bootstrap hash on the next start instead of leaving it usable forever.
if [ -n "${BOOTSTRAP_API_TOKEN:-}" ]; then
  "$DB_BIN" prune-tokens || true
fi

"$WORKER_BIN" &
WORKER_PID=$!

shutdown() {
  kill "$WORKER_PID" 2>/dev/null || true
  [ -n "${API_PID:-}" ] && kill "$API_PID" 2>/dev/null || true
  wait 2>/dev/null || true
}
trap shutdown INT TERM

"$API_BIN" &
API_PID=$!

# If the API exits, stop the worker too and propagate the exit code.
wait "$API_PID"
STATUS=$?
kill "$WORKER_PID" 2>/dev/null || true
exit "$STATUS"
