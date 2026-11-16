#!/usr/bin/env python3
"""Health check for assets/parcel_layers.json — the registry of public parcel-outline layers.

For every layer and each of its stored test locations this sends the same two queries the server sends
(see `fetch_registry_parcel_outline` in src/property/public_records.rs):

  1. point-intersects at the test location  -> must return a polygon that contains the point
  2. the situs-address filter inside a +/-400 m envelope (only when the layer has `where`)
                                             -> must return that same parcel

Only the parcel-id and situs-address fields named in the registry are requested — never owner or mailing
fields. Standard library only.

    python3 scripts/verify_parcel_layers.py                 # whole registry
    python3 scripts/verify_parcel_layers.py --only TX       # one state (or a county FIPS)
    python3 scripts/verify_parcel_layers.py --ipv4          # hosts without an IPv6 route

Exit code 1 when any layer fails. Layers are public services run by states and counties; a failure usually
means the service moved (several embed a year in their name) and the entry needs a new URL.
"""
import json, math, os, re, socket, sys, time, urllib.parse, urllib.request

REGISTRY = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "assets", "parcel_layers.json")
UA = {"User-Agent": "open-estate-parcel-layer-check/1.0"}
RE_LINE = re.compile(r"^(\d+(?:-\d+)?[A-Za-z]?)\s+(.+)$")
RE_DIRS = re.compile(r"(?i)\b(?:N|S|E|W|NE|NW|SE|SW|North|South|East|West)\b\.?")
RE_SUFFIX = re.compile(r"(?i)\b(?:St|Street|Ave|Av|Avenue|Rd|Road|Dr|Drive|Ln|Lane|Ct|Court|Blvd|Boulevard|Way|Wy|Pl|Place|Cir|Circle|Ter|Terrace|Pkwy|Parkway|Hwy|Highway)\b\.?$")
RE_SKIP = re.compile(r"(?i)^(?:calle|cll|corte|casa|camino|paseo|via|de|del|la|el|los|las|san|santa)$")
RE_ORD = re.compile(r"^(\d+)(?:ST|ND|RD|TH)$")


def addr_key(address):
    """House-number token and street keyword, as `AddrKey::parse` derives them on the server."""
    m = RE_LINE.match(re.split(r"[,\r\n]", address)[0].strip())
    if not m:
        return "", ""
    hy = re.match(r"\d+-\d+", m.group(1))
    num = hy.group(0) if hy else re.match(r"\d+", m.group(1)).group(0)
    core = re.sub(r"\s+#.*$", "", RE_SUFFIX.sub("", RE_DIRS.sub(" ", m.group(2)).strip()))
    words = [re.sub(r"[^0-9A-Za-z]", "", w) for w in core.split()]
    words = [w for w in words if w and (len(w) >= 3 or w[0].isdigit()) and not RE_SKIP.match(w)]
    if not words:
        return num, ""
    kw = words[-1].upper()
    mo = RE_ORD.match(kw)
    return num, (mo.group(1) if mo else kw)


def query(url, params, timeout=15):
    q = url.rstrip("/") + "/query?" + urllib.parse.urlencode(dict(f="json", inSR="4326", outSR="4326", spatialRel="esriSpatialRelIntersects", returnGeometry="true", **params))
    t0 = time.time()
    try:
        with urllib.request.urlopen(urllib.request.Request(q, headers=UA), timeout=timeout) as r:
            j = json.loads(r.read().decode("utf-8", "replace"))
    except Exception as e:  # noqa: BLE001
        return None, f"{type(e).__name__}: {e}"[:120], int((time.time() - t0) * 1000)
    ms = int((time.time() - t0) * 1000)
    if isinstance(j, dict) and j.get("error"):
        return None, json.dumps(j["error"])[:120], ms
    return j.get("features", []), None, ms


def contains(feat, lat, lon):
    for ring in (feat.get("geometry") or {}).get("rings", []):
        inside, j = False, len(ring) - 1
        for i in range(len(ring)):
            xi, yi, xj, yj = ring[i][0], ring[i][1], ring[j][0], ring[j][1]
            if ((yi > lat) != (yj > lat)) and (lon < (xj - xi) * (lat - yi) / ((yj - yi) or 1e-15) + xi):
                inside = not inside
            j = i
        if inside:
            return True
    return False


def main():
    args = sys.argv[1:]
    if "--ipv4" in args:
        args.remove("--ipv4")
        gai = socket.getaddrinfo
        socket.getaddrinfo = lambda *a, **k: [x for x in gai(*a, **k) if x[0] == socket.AF_INET]
    only = args[args.index("--only") + 1].upper() if "--only" in args else None
    layers = json.load(open(REGISTRY, encoding="utf-8"))["layers"]
    failed = 0
    for l in layers:
        tag = f"{l['state']} {','.join(l.get('counties') or []) or 'statewide'}"
        if only and only not in tag.upper():
            continue
        fields = ",".join(dict.fromkeys(x for x in [l["apn"], l.get("addr"), l.get("num"), l.get("street")] if x))
        ok, notes, times = True, [], []
        for lat, lon in l.get("tests", []):
            p = dict(geometry=f"{lon},{lat}", geometryType="esriGeometryPoint", outFields=fields)
            if l.get("filter"):
                p["where"] = l["filter"]
            feats, err, ms = query(l["url"], p)
            times.append(ms)
            hit = next((f for f in (feats or []) if contains(f, lat, lon)), None)
            if not hit:
                ok = False
                notes.append(err or f"no parcel contains {lat},{lon}")
                continue
            a = hit["attributes"]
            situs = str(a.get(l["addr"]) or "").strip() if l.get("addr") else ""
            if not situs and l.get("num") and l.get("street") and a.get(l["num"]) not in (None, "", 0):
                situs = f"{str(a.get(l['num'])).strip()} {str(a.get(l['street']) or '').strip()}"
            num, kw = addr_key(situs) if situs else ("", "")
            mode = l.get("where")
            if mode and num and kw:
                where = {"upper": f"UPPER({l.get('addr')}) LIKE '{num} %{kw}%'", "plain": f"{l.get('addr')} LIKE '{num} %{kw}%'", "street": f"UPPER({l.get('street')}) LIKE '%{kw}%'"}[mode]
                if l.get("filter"):
                    where = f"({where}) AND ({l['filter']})"
                dlat, dlon = 400 / 111320.0, 400 / (111320.0 * math.cos(math.radians(lat)))
                time.sleep(0.25)
                f1, e1, ms1 = query(l["url"], dict(where=where, geometry=f"{lon - dlon},{lat - dlat},{lon + dlon},{lat + dlat}", geometryType="esriGeometryEnvelope", outFields=fields))
                times.append(ms1)
                same = [f for f in (f1 or []) if str(f["attributes"].get(l["apn"])) == str(a.get(l["apn"]))]
                if not same:
                    ok = False
                    notes.append(e1 or f"address filter did not return the parcel ({where})")
            time.sleep(0.25)
        failed += not ok
        print(f"{'PASS' if ok else 'FAIL'}  {tag:22} {max(times or [0]):>6} ms max  {l['url']}" + ("" if ok else "\n        " + "; ".join(notes)), flush=True)
    print(f"\n{failed} layer(s) failed" if failed else "\nall layers passed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
