#!/bin/sh
# OSRM router entrypoint (ghcr.io/project-osrm/osrm-backend). Serves the dataset pointed to by
# /data/current and exits cleanly if /data/current is repointed to a newer dataset so Docker
# (`restart: unless-stopped`) restarts osrm-routed on the updated graph.
set -eu

: "${DATA_DIR:=/data}"
: "${EXTRACT_NAME:=california-latest}"
: "${OSRM_MAX_TABLE_SIZE:=1000}"
: "${WATCH_INTERVAL_S:=60}"

trap 'exit 0' TERM INT

until [ -e "$DATA_DIR/current/$EXTRACT_NAME.osrm.mldgr" ]; do
  echo "waiting for an OSRM dataset at $DATA_DIR/current..."
  sleep 15 &
  wait $! || true
done

target="$(readlink -f "$DATA_DIR/current")"
echo "serving dataset $(basename "$target") with --mmap"
osrm-routed --algorithm mld --mmap --max-table-size "$OSRM_MAX_TABLE_SIZE" "$target/$EXTRACT_NAME.osrm" &
pid=$!
trap 'kill -TERM "$pid" 2>/dev/null || true; wait "$pid" || true; exit 0' TERM INT

while kill -0 "$pid" 2>/dev/null; do
  sleep "$WATCH_INTERVAL_S" &
  wait $! || true
  now="$(readlink -f "$DATA_DIR/current" 2>/dev/null || true)"
  if [ -n "$now" ] && [ "$now" != "$target" ]; then
    echo "dataset changed to $(basename "$now"); restarting osrm-routed"
    kill -TERM "$pid" 2>/dev/null || true
    wait "$pid" || true
    exit 0
  fi
done
wait "$pid"
