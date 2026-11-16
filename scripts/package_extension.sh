#!/usr/bin/env bash
# Package the Open Estate Manifest V3 Browser Extension for Chrome Web Store / Edge Add-ons upload.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EXT_DIR="$ROOT_DIR/extension"
OUT_DIR="$ROOT_DIR/target"
OUT_ZIP="$OUT_DIR/open-estate-extension.zip"

if [ ! -f "$EXT_DIR/manifest.json" ]; then
  echo "ERROR: $EXT_DIR/manifest.json not found" >&2
  exit 1
fi

# Validate manifest and locale JSON files before packaging
python3 - <<'PY' "$EXT_DIR"
import json, pathlib, sys
ext = pathlib.Path(sys.argv[1])
manifest = json.loads((ext / "manifest.json").read_text(encoding="utf-8"))
assert manifest.get("manifest_version") == 3, "manifest_version must be 3"
for msg_file in sorted((ext / "locales").glob("*.json")):
    json.loads(msg_file.read_text(encoding="utf-8"))
for msg_file in sorted((ext / "_locales").glob("*/messages.json")):
    json.loads(msg_file.read_text(encoding="utf-8"))
loc_count = len(list((ext / "locales").glob("*.json"))) + len(list((ext / "_locales").glob("*/messages.json")))
print(f"Validated {manifest.get('name')} v{manifest.get('version')} ({loc_count} locale bundles)")
PY

mkdir -p "$OUT_DIR"
rm -f "$OUT_ZIP"

python3 - <<'PY' "$EXT_DIR" "$OUT_ZIP"
import pathlib, sys, zipfile
ext = pathlib.Path(sys.argv[1])
out_zip = pathlib.Path(sys.argv[2])
with zipfile.ZipFile(out_zip, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as zf:
    for p in sorted(ext.rglob("*")):
        if p.is_file() and not p.name.startswith("."):
            zf.write(p, p.relative_to(ext).as_posix())
print(f"Packaged Chrome Web Store archive -> {out_zip} ({out_zip.stat().st_size} bytes)")
PY
