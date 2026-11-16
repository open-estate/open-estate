#!/bin/sh
# OSRM dataset builder, step 2 of 2 (ghcr.io/project-osrm/osrm-backend): build the MLD dataset
# from the verified extract, publish it as /data/<version>, atomically repoint /data/current at it,
# and keep the newest KEEP_DATASETS datasets.
set -eu

: "${DATA_DIR:=/data}"
: "${EXTRACT_NAME:=california-latest}"
: "${OSRM_PROFILE:=/opt/car.lua}"
: "${OSRM_THREADS:=2}"
: "${KEEP_DATASETS:=1}"

if [ "${SKIP_IF_CURRENT_EXISTS:-1}" = "1" ] && [ -e "$DATA_DIR/current/$EXTRACT_NAME.osrm.mldgr" ] && [ ! -d "$DATA_DIR/.staging" ]; then
  echo "existing dataset active at $DATA_DIR/current; skipping build"
  exit 0
fi

STAGE="$DATA_DIR/.staging"
cd "$STAGE"
version="$(cat VERSION)"
if [ -e "$DATA_DIR/$version" ]; then
  echo "dataset $version already exists; refusing to overwrite it" >&2
  exit 1
fi

osrm-extract -p "$OSRM_PROFILE" --data_version "$version" -t "$OSRM_THREADS" "$EXTRACT_NAME.osm.pbf"
osrm-partition -t "$OSRM_THREADS" "$EXTRACT_NAME.osrm"
osrm-customize -t "$OSRM_THREADS" "$EXTRACT_NAME.osrm"
rm -f "$EXTRACT_NAME.osm.pbf"

osrm-routed --algorithm mld --mmap --trial=1 "$STAGE/$EXTRACT_NAME.osrm"

cd "$DATA_DIR"
mv "$STAGE" "$DATA_DIR/$version"

ln -sfn "$version" "$DATA_DIR/.current.next"
mv -T "$DATA_DIR/.current.next" "$DATA_DIR/current"
echo "activated dataset $version"

ls -1d [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]* 2>/dev/null | sort -r |
  tail -n +"$((KEEP_DATASETS + 1))" | while read -r old; do
    [ "$old" = "$version" ] && continue
    echo "removing old dataset $old"
    rm -rf "${DATA_DIR:?}/$old"
  done
echo "datasets kept: $(ls -1d [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]* | tr '\n' ' ')"
