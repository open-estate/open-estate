#!/bin/sh
# OSRM dataset builder, step 1 of 2: download a Geofabrik .osm.pbf extract and verify its .md5.
#
# Layout on the volume (DATA_DIR, default /data):
#   /data/<version>/   finished datasets, <version> = UTC build date (YYYY-MM-DD)
#   /data/current      symlink to the dataset osrm-routed serves
#   /data/.staging/    the build in progress (removed and recreated by every run)
set -eu

: "${DATA_DIR:=/data}"
: "${EXTRACT_URL:=https://download.geofabrik.de/north-america/us/california-latest.osm.pbf}"
: "${EXTRACT_NAME:=california-latest}"

if [ "${SKIP_IF_CURRENT_EXISTS:-1}" = "1" ] && [ -e "$DATA_DIR/current/$EXTRACT_NAME.osrm.mldgr" ]; then
  echo "existing dataset found at $DATA_DIR/current; skipping download (set SKIP_IF_CURRENT_EXISTS=0 to rebuild)"
  exit 0
fi

STAGE="$DATA_DIR/.staging"
PBF="$EXTRACT_NAME.osm.pbf"

current=""
if [ -L "$DATA_DIR/current" ]; then
  current="$(basename "$(readlink "$DATA_DIR/current")")"
fi
rm -rf "$STAGE"
for dir in "$DATA_DIR"/[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]*; do
  [ -d "$dir" ] || continue
  name="$(basename "$dir")"
  if [ "$name" != "$current" ]; then
    echo "removing older dataset $name to make room"
    rm -rf "$dir"
  fi
done
mkdir -p "$STAGE"
cd "$STAGE"

version="$(date -u +%Y-%m-%d)"
if [ -e "$DATA_DIR/$version" ]; then
  version="$(date -u +%Y-%m-%dT%H%MZ)"
fi
echo "$version" > VERSION

ok=0
for attempt in 1 2 3; do
  curl -fsSL --retry 5 --retry-delay 15 -o "$PBF.md5" "$EXTRACT_URL.md5"
  curl -fSL --retry 5 --retry-delay 30 -o "$PBF" "$EXTRACT_URL"
  if md5sum -c "$PBF.md5"; then
    ok=1
    break
  fi
  echo "checksum mismatch on attempt $attempt; retrying in 60 s" >&2
  sleep 60
done
if [ "$ok" != 1 ]; then
  echo "could not verify $PBF against $EXTRACT_URL.md5" >&2
  exit 1
fi
echo "downloaded and verified $PBF for dataset $version"

# Stream-filter out buildings/landuse/POIs and non-street ways when osmium is present so
# osrm-extract uses less memory while preserving car-routable road classes, ferries, and restrictions.
if command -v osmium >/dev/null 2>&1; then
  echo "filtering $PBF to car-routable road network via osmium..."
  osmium tags-filter "$PBF" \
    w/highway=motorway,motorway_link,trunk,trunk_link,primary,primary_link,secondary,secondary_link,tertiary,tertiary_link,unclassified,residential,living_street,winter_road,ice_road \
    w/route=ferry,shuttle_train \
    r/type=restriction,route \
    n/barrier n/highway n/access n/motor_vehicle n/motorcar n/vehicle \
    -o "$STAGE/filtered.osm.pbf" --overwrite
  mv -f "$STAGE/filtered.osm.pbf" "$PBF"
  rm -f "$PBF.md5"
  ls -lh "$PBF"
fi
