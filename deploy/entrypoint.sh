#!/bin/sh
# Fix the data volume ownership, then drop privileges.
#
# Hosts mount volumes owned by root while the application runs as UID 10001: without this step SQLite
# cannot create its file ("unable to open database file"). The process still ends up unprivileged.
set -eu

if [ "$(id -u)" = "0" ]; then
  DATA_DIR="${DATA_DIR:-/data}"
  if [ -d "$DATA_DIR" ]; then
    # The directory and the database files are what SQLite needs; no recursive walk over the volume.
    chown 10001:10001 "$DATA_DIR" 2>/dev/null || true
    chown 10001:10001 "$DATA_DIR"/*.db 2>/dev/null || true
    chown 10001:10001 "$DATA_DIR"/*.db-wal 2>/dev/null || true
    chown 10001:10001 "$DATA_DIR"/*.db-shm 2>/dev/null || true
  fi
  if command -v setpriv >/dev/null 2>&1; then
    exec setpriv --reuid=10001 --regid=10001 --clear-groups "$@"
  fi
  echo "entrypoint: setpriv is unavailable, refusing to run as root" >&2
  exit 1
fi

exec "$@"
