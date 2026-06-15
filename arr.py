#!/usr/bin/env python3
"""arr — CLI over the Sonarr / Radarr / Prowlarr REST APIs for this box.

Deployed declaratively from /etc/nixos/configuration.nix:
  * the script is packaged as `arr` on PATH (python3, stdlib only);
  * API keys are rendered by sops-nix into an andrep-readable env file in
    /run/secrets (tmpfs, RAM-only) — so this script reads them with no sudo.

Key source (first hit wins):
  1. env var ARR_API_KEY_<SERVICE>           (e.g. ARR_API_KEY_SONARR=...)
  2. the sops-rendered env file              ($ARR_ENV_FILE or the default)
"""
import json
import os
import re
import sys
import urllib.error
import urllib.parse
import urllib.request

ENV_FILE = os.environ.get("ARR_ENV_FILE", "/run/secrets/rendered/arr-cli.env")

SERVICES = {
    "sonarr":       {"port": 8989, "ver": "v3", "key": "SONARR_API_KEY"},
    # Dedicated anime Sonarr instance (nixflix.sonarr-anime). Same v3 series API
    # as sonarr — every sonarr command works against it; series logic keys off
    # svc.startswith("sonarr"), so this is treated as a series (not movie) svc.
    "sonarr-anime": {"port": 8990, "ver": "v3", "key": "SONARR_ANIME_API_KEY"},
    "radarr":       {"port": 7878, "ver": "v3", "key": "RADARR_API_KEY"},
    "prowlarr":     {"port": 9696, "ver": "v1", "key": "PROWLARR_API_KEY"},
}

# SABnzbd is not a *arr; its HTTP API is ?mode=...&apikey=... on port 8085.
SAB = {"port": 8085, "key": "SABNZBD_API_KEY"}
# qBittorrent WebUI API (localhost auth bypass enabled), port 8080.
QBIT = {"port": 8080}
# Jellyfin media server API, port 8096; auth via the X-Emby-Token header.
JELLYFIN = {"port": 8096, "key": "JELLYFIN_API_KEY"}
# Jellyseerr (request frontend) API, port 5055; auth via X-Api-Key.
SEERR = {"port": 5055, "key": "SEERR_API_KEY"}


def die(msg):
    print("arr: " + msg, file=sys.stderr)
    sys.exit(1)


# --- key loading -------------------------------------------------------------
_env_cache = None


def _load_env_file():
    global _env_cache
    if _env_cache is not None:
        return _env_cache
    _env_cache = {}
    try:
        with open(ENV_FILE) as fh:
            for line in fh:
                line = line.strip()
                if not line or line.startswith("#") or "=" not in line:
                    continue
                k, v = line.split("=", 1)
                _env_cache[k.strip()] = v.strip()
    except OSError:
        pass
    return _env_cache


def env_key(name, envvar):
    if os.environ.get(envvar):
        return os.environ[envvar]
    val = _load_env_file().get(name)
    if not val:
        die("no key %s (looked at $%s and %s)" % (name, envvar, ENV_FILE))
    return val


def get_key(svc):
    return env_key(SERVICES[svc]["key"], "ARR_API_KEY_" + svc.upper())


def sab_key():
    return env_key(SAB["key"], "ARR_API_KEY_SAB")


def jf_key():
    return env_key(JELLYFIN["key"], "ARR_API_KEY_JELLYFIN")


def seerr_key():
    return env_key(SEERR["key"], "ARR_API_KEY_SEERR")


# --- HTTP --------------------------------------------------------------------
def api(svc, method, path, body=None, timeout=120):
    cfg = SERVICES[svc]
    url = "http://localhost:%d/api/%s%s" % (cfg["port"], cfg["ver"], path)
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    req.add_header("X-Api-Key", get_key(svc))
    if data is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read().decode()
    except urllib.error.HTTPError as e:
        detail = e.read().decode(errors="replace")[:500]
        die("%s %s -> HTTP %d %s" % (method, path, e.code, detail))
    except (TimeoutError, urllib.error.URLError) as e:
        reason = getattr(e, "reason", e)
        if isinstance(reason, TimeoutError) or isinstance(e, TimeoutError):
            die("%s %s timed out after %ds (indexer searches can be slow — retry)" % (method, path, timeout))
        die("%s %s -> %s" % (method, path, reason))
    return json.loads(raw) if raw.strip() else None


def sab_api(mode, params=None, timeout=120):
    q = {"mode": mode, "output": "json", "apikey": sab_key()}
    if params:
        q.update(params)
    url = "http://localhost:%d/api/?%s" % (SAB["port"], urllib.parse.urlencode(q))
    try:
        with urllib.request.urlopen(url, timeout=timeout) as resp:
            raw = resp.read().decode()
    except urllib.error.HTTPError as e:
        die("sab %s -> HTTP %d" % (mode, e.code))
    except (TimeoutError, urllib.error.URLError) as e:
        die("sab %s -> %s" % (mode, getattr(e, "reason", e)))
    try:
        return json.loads(raw)
    except ValueError:
        return raw


def jf_api(path, params=None, timeout=60):
    qs = ("?" + urllib.parse.urlencode(params)) if params else ""
    url = "http://localhost:%d%s%s" % (JELLYFIN["port"], path, qs)
    req = urllib.request.Request(url, method="GET")
    req.add_header("X-Emby-Token", jf_key())
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return json.loads(resp.read().decode())
    except urllib.error.HTTPError as e:
        die("jellyfin %s -> HTTP %d" % (path, e.code))
    except (TimeoutError, urllib.error.URLError) as e:
        die("jellyfin %s -> %s" % (path, getattr(e, "reason", e)))


def seerr_api(path, params=None, timeout=60, soft=False):
    qs = ("?" + urllib.parse.urlencode(params)) if params else ""
    url = "http://localhost:%d/api/v1%s%s" % (SEERR["port"], path, qs)
    req = urllib.request.Request(url, method="GET")
    req.add_header("X-Api-Key", seerr_key())
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return json.loads(resp.read().decode())
    except urllib.error.HTTPError as e:
        if soft:
            return None
        die("seerr %s -> HTTP %d" % (path, e.code))
    except (TimeoutError, urllib.error.URLError) as e:
        if soft:
            return None
        die("seerr %s -> %s" % (path, getattr(e, "reason", e)))


# --- helpers -----------------------------------------------------------------
def gb(n):
    return round((n or 0) / 1073741824, 1)


def mb(n):
    return round((n or 0) / 1048576)


def resolve_id(svc, q):
    """numeric -> int; title substring -> unique id (or list candidates & die)."""
    if str(q).isdigit():
        return int(q)
    coll = "series" if svc.startswith("sonarr") else "movie"
    items = api(svc, "GET", "/" + coll)
    ql = q.lower()
    hits = []
    for it in items:
        names = [it.get("title", ""), it.get("titleSlug", "")]
        names += [a.get("title", "") for a in (it.get("alternateTitles") or [])]
        if any(ql in (n or "").lower() for n in names):
            hits.append(it)
    if not hits:
        die('no %s matching "%s"' % (coll, q))
    if len(hits) > 1:
        print('ambiguous "%s" — %d matches:' % (q, len(hits)), file=sys.stderr)
        for h in hits:
            print("  %d\t%s (%s)" % (h["id"], h["title"], h.get("year")), file=sys.stderr)
        die("narrow the query or pass a numeric id")
    return hits[0]["id"]


def pop_flags(args, spec):
    """Pull flags out of args. spec: {flag: nargs(0|1)}. Returns (vals, rest)."""
    vals, rest, i = {}, [], 0
    while i < len(args):
        a = args[i]
        if a in spec:
            if spec[a] == 0:
                vals[a] = True
                i += 1
            else:
                vals[a] = args[i + 1]
                i += 2
        else:
            rest.append(a)
            i += 1
    return vals, rest


# --- commands ----------------------------------------------------------------
def cmd_status(svc, args):
    q = (args[0].lower() if args else "")
    if svc.startswith("sonarr"):
        items = sorted(api(svc, "GET", "/series"), key=lambda x: x["title"])
        for s in items:
            if q and q not in s["title"].lower():
                continue
            st = s.get("statistics", {})
            print("[%d] %s (%s) — %s, mon=%s" % (s["id"], s["title"], s.get("year"), s["status"], s["monitored"]))
            print("      %d/%d eps on disk (%s%%), %sGB" % (
                st.get("episodeFileCount", 0), st.get("totalEpisodeCount", 0),
                st.get("percentOfEpisodes", 0), gb(st.get("sizeOnDisk"))))
    elif svc == "radarr":
        items = sorted(api(svc, "GET", "/movie"), key=lambda x: x["title"])
        for m in items:
            if q and q not in m["title"].lower():
                continue
            disk = "ON DISK (%sGB)" % gb(m.get("sizeOnDisk")) if m.get("hasFile") else "MISSING"
            print("[%d] %s (%s) — %s, mon=%s, %s" % (m["id"], m["title"], m.get("year"), m["status"], m["monitored"], disk))
    else:
        die("status: sonarr|radarr only")


def cmd_get(svc, args):
    if not args:
        die("get: need an id or query")
    coll = "series" if svc.startswith("sonarr") else "movie"
    print(json.dumps(api(svc, "GET", "/%s/%d" % (coll, resolve_id(svc, args[0]))), indent=2))


def cmd_seasons(svc, args):
    if not svc.startswith("sonarr"):
        die("seasons: sonarr only")
    s = api(svc, "GET", "/series/%d" % resolve_id(svc, args[0]))
    print(s["title"])
    for se in s["seasons"]:
        st = se.get("statistics", {})
        print("  S%d  mon=%s  %d/%d on disk" % (
            se["seasonNumber"], se["monitored"],
            st.get("episodeFileCount", 0), st.get("totalEpisodeCount", 0)))


def _release_query(svc, args):
    flags, rest = pop_flags(args, {"--season": 1, "--episode": 1})
    if not rest:
        die("need an id or query")
    if svc.startswith("sonarr"):
        sid = resolve_id(svc, rest[0])
        if "--episode" in flags:
            return "episodeId=%s" % flags["--episode"]
        if "--season" in flags:
            return "seriesId=%d&seasonNumber=%s" % (sid, flags["--season"])
        die("sonarr releases: need --season N or --episode EPID")
    else:
        return "movieId=%d" % resolve_id(svc, rest[0])


SEARCH_TIMEOUT = 300  # interactive indexer searches (/release) can take minutes


def cmd_releases(svc, args):
    rels = api(svc, "GET", "/release?" + _release_query(svc, args), timeout=SEARCH_TIMEOUT)
    rels.sort(key=lambda r: (r.get("rejected", False), -(r.get("seeders") or 0)))
    print("found %d release(s):" % len(rels))
    for r in rels:
        mark = "✗" if r.get("rejected") else "✓"
        seed = " %ss" % r["seeders"] if r.get("seeders") is not None else ""
        print("  %s %sMB  %s  %s%s  %s" % (
            mark, mb(r.get("size")), r["quality"]["quality"]["name"], r["protocol"], seed, r["title"]))
        if r.get("rejected"):
            print("        reject: " + "; ".join(r.get("rejections") or []))
        print("        guid=%s  indexerId=%s" % (r.get("guid"), r.get("indexerId")))


def sab_add_url(url, cat, name=None):
    params = {"name": url, "cat": cat}
    if name:
        params["nzbname"] = name
    r = sab_api("addurl", params)
    return isinstance(r, dict) and r.get("status") is True


def qbit_add(link, cat):
    data = urllib.parse.urlencode({"urls": link, "category": cat}).encode()
    req = urllib.request.Request("http://localhost:%d/api/v2/torrents/add" % QBIT["port"],
                                 data=data, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return resp.read().decode().strip().lower().startswith("ok") or resp.status == 200
    except (TimeoutError, urllib.error.URLError, urllib.error.HTTPError) as e:
        die("qbit add -> %s" % getattr(e, "reason", e))


def cmd_prowlarr_grab(args):
    """arr prowlarr grab <query> [--indexer X] [--group/--match Y] [--cat C]
        [--all] [--dry-run]
    Search across indexers and send the matching release to the right client:
    usenet -> SABnzbd, torrent -> qBittorrent. Refuses >1 match without --all."""
    flags, rest = pop_flags(args, {"--indexer": 1, "--group": 1, "--match": 1,
                                   "--cat": 1, "--limit": 1, "--all": 0, "--dry-run": 0})
    if not rest:
        die("prowlarr grab: need a search query")
    cat = flags.get("--cat", "sonarr")
    qs = urllib.parse.urlencode({"query": rest[0], "type": "search",
                                 "limit": flags.get("--limit", "100")})
    res = api("prowlarr", "GET", "/search?" + qs, timeout=SEARCH_TIMEOUT) or []
    seen, rows = set(), []
    for r in res:
        k = (r.get("title"), r.get("indexer"))
        if k in seen:
            continue
        seen.add(k)
        rows.append(r)
    if flags.get("--indexer"):
        rows = [r for r in rows if flags["--indexer"].lower() in (r.get("indexer", "").lower())]
    for f in ("--group", "--match"):
        if flags.get(f):
            rows = [r for r in rows if flags[f].lower() in (r.get("title", "").lower())]
    if not rows:
        die("no releases match")
    if len(rows) > 1 and "--all" not in flags:
        print("multiple matches — narrow with --match/--group/--indexer, or pass --all:", file=sys.stderr)
        for r in sorted(rows, key=lambda r: -(r.get("size") or 0))[:20]:
            sd = " %ss" % r["seeders"] if r.get("seeders") is not None else ""
            print("  [%s] %sMB %s%s  %s" % (r.get("indexer"), mb(r.get("size")),
                                            r.get("protocol"), sd, r.get("title")), file=sys.stderr)
        die("refusing to grab %d releases without --all" % len(rows))
    n = 0
    for r in rows:
        proto, title = r.get("protocol"), r.get("title")
        if "--dry-run" in flags:
            print("DRY %s -> %s: %s" % (proto, "qbit" if proto == "torrent" else "sab", title))
            n += 1
            continue
        if proto == "usenet":
            ok = sab_add_url(r.get("downloadUrl"), cat=cat, name=title)
            tag = "sab"
        else:
            g = r.get("guid", "") or ""
            link = g if g.startswith("magnet:") else (r.get("downloadUrl") or r.get("magnetUrl"))
            ok = qbit_add(link, cat=cat)
            tag = "qbit"
        print(("%s+ " % tag if ok else "FAIL ") + title)
        n += 1
    print("(%d release(s) grabbed to cat=%s)" % (n, cat))


def cmd_grab(svc, args):
    if svc == "prowlarr":
        return cmd_prowlarr_grab(args)
    flags, rest = pop_flags(args, {"--season": 1, "--episode": 1,
                                   "--override": 0, "--via-sab": 0, "--dry-run": 0})
    if not rest:
        die("grab: need an id or query")
    override, dry = "--override" in flags, "--dry-run" in flags

    if "--via-sab" in flags:
        # Fetch candidate usenet releases and hand their NZBs straight to SAB
        # under the service category — bypasses Sonarr's search cache entirely
        # (so it works for releases Sonarr's per-episode search can't see).
        rels = api(svc, "GET", "/release?" + _release_query(svc, args), timeout=SEARCH_TIMEOUT)
        seen, n = set(), 0
        for r in rels:
            if r.get("protocol") != "usenet" or not r.get("downloadUrl"):
                continue
            g = r.get("guid")
            if g in seen:
                continue
            seen.add(g)
            n += 1
            if dry:
                print("DRY via-sab: " + r["title"])
                continue
            ok = sab_add_url(r["downloadUrl"], cat=svc, name=r.get("title"))
            print(("added: " if ok else "FAILED: ") + r["title"])
        print("(%d usenet release(s) sent to SAB cat=%s)" % (n, svc))
        return

    if not override:
        # let the arr search & decide (respects the quality profile)
        if svc.startswith("sonarr"):
            sid = resolve_id(svc, rest[0])
            if "--episode" in flags:
                body = {"name": "EpisodeSearch", "episodeIds": [int(flags["--episode"])]}
            elif "--season" in flags:
                body = {"name": "SeasonSearch", "seriesId": sid, "seasonNumber": int(flags["--season"])}
            else:
                body = {"name": "SeriesSearch", "seriesId": sid}
        else:
            body = {"name": "MoviesSearch", "movieIds": [resolve_id(svc, rest[0])]}
        if dry:
            print("DRY: POST /command " + json.dumps(body))
            return
        r = api(svc, "POST", "/command", body)
        print("queued %s (command id %s, status %s)" % (r["commandName"], r["id"], r["status"]))
        return

    # --override: force-push every candidate release, bypassing rejections
    rels = api(svc, "GET", "/release?" + _release_query(svc, args), timeout=SEARCH_TIMEOUT)
    seen, count = set(), 0
    for r in rels:
        g = r.get("guid")
        if g in seen:
            continue
        seen.add(g)
        count += 1
        if dry:
            print("DRY push: " + r["title"])
            continue
        try:
            api(svc, "POST", "/release", {"guid": g, "indexerId": r["indexerId"]})
            print("pushed: " + r["title"])
        except SystemExit:
            print("FAILED: " + r["title"], file=sys.stderr)
    print("(%d release(s) processed)" % count)


def cmd_monitor(svc, args):
    if len(args) < 2:
        die("monitor: need <id|query> <spec>")
    ident, spec = args[0], args[1]
    if svc == "radarr":
        val = {"on": True, "true": True, "off": False, "false": False}.get(spec.lower())
        if val is None:
            die("radarr monitor: on|off")
        mid = resolve_id(svc, ident)
        m = api(svc, "GET", "/movie/%d" % mid)
        m["monitored"] = val
        api(svc, "PUT", "/movie/%d" % mid, m)
        print("radarr movie %d monitored=%s" % (mid, val))
        return
    sid = resolve_id(svc, ident)
    s = api(svc, "GET", "/series/%d" % sid)
    if spec == "all":
        for se in s["seasons"]:
            se["monitored"] = True
    elif spec == "none":
        for se in s["seasons"]:
            se["monitored"] = False
    else:
        want = {int(x.lstrip("sS")) for x in spec.split(",")}
        for se in s["seasons"]:
            se["monitored"] = se["seasonNumber"] in want
    api(svc, "PUT", "/series/%d" % sid, s)
    on = ",".join("S%d" % se["seasonNumber"] for se in s["seasons"] if se["monitored"]) or "none"
    print("updated %s: %s" % (s["title"], on))


def _queue_records(svc, page_size=1000):
    if svc.startswith("sonarr"):
        extra = "includeUnknownSeriesItems=true&includeSeries=true&includeEpisode=true"
    else:
        extra = "includeUnknownMovieItems=true&includeMovie=true"
    return api(svc, "GET", "/queue?pageSize=%d&%s" % (page_size, extra))


def _queue_status_messages(r):
    return [m for sm in (r.get("statusMessages") or []) for m in sm.get("messages", [])]


def _is_stuck_queue_record(r):
    status = r.get("status")
    state = r.get("trackedDownloadState")
    return (
        status == "failed"
        or state in ("importBlocked", "importPending")
        or bool(r.get("errorMessage"))
    )


def _queue_record_summary(r):
    return {
        "id": r.get("id"),
        "title": r.get("title"),
        "status": r.get("status"),
        "trackedDownloadStatus": r.get("trackedDownloadStatus"),
        "trackedDownloadState": r.get("trackedDownloadState"),
        "sizeleftMb": mb(r.get("sizeleft")),
        "downloadId": r.get("downloadId"),
        "outputPath": r.get("outputPath"),
        "errorMessage": r.get("errorMessage"),
        "statusMessages": _queue_status_messages(r),
    }


def cmd_queue(svc, args):
    pat = args[0].lower() if args else None
    q = _queue_records(svc, page_size=200)
    print("queue: %d item(s)%s" % (q["totalRecords"], (" matching '%s'" % args[0]) if pat else ""))
    for r in q["records"]:
        if pat and pat not in (r.get("title", "").lower()):
            continue
        print("  %s/%s  %sMB left  %s" % (
            r.get("status"), r.get("trackedDownloadState"), mb(r.get("sizeleft")), r.get("title")))
        if r.get("errorMessage"):
            print("        err: " + r["errorMessage"])
        msgs = _queue_status_messages(r)
        if msgs:
            print("        " + "; ".join(msgs))


def cmd_stuck(svc, args):
    """Show only queue items that need intervention: failed/import blocked/pending."""
    flags, rest = pop_flags(args, {"--json": 0, "--quiet": 0})
    pat = rest[0].lower() if rest else None
    q = _queue_records(svc, page_size=1000)
    rows = []
    for r in q["records"]:
        if pat and pat not in (r.get("title", "").lower()):
            continue
        if _is_stuck_queue_record(r):
            rows.append(r)
    if "--json" in flags:
        print(json.dumps([_queue_record_summary(r) for r in rows], indent=2))
        return
    if rows:
        print("stuck: %d %s queue item(s)%s" % (len(rows), svc, (" matching '%s'" % rest[0]) if pat else ""))
    elif "--quiet" not in flags:
        print("stuck: 0 %s queue item(s)%s" % (svc, (" matching '%s'" % rest[0]) if pat else ""))
    for r in rows:
        print("  %s/%s  %sMB left  %s" % (
            r.get("status"), r.get("trackedDownloadState"), mb(r.get("sizeleft")), r.get("title")))
        if r.get("errorMessage"):
            print("        err: " + r["errorMessage"])
        msgs = _queue_status_messages(r)
        if msgs:
            print("        " + "; ".join(msgs))


def cmd_history(svc, args):
    if args:
        hid = resolve_id(svc, args[0])
        if svc.startswith("sonarr"):
            rows = api(svc, "GET", "/history/series?seriesId=%d" % hid) or []
            rows = sorted(rows, key=lambda x: x["date"], reverse=True)[:20]
        else:
            data = api(svc, "GET", "/history?pageSize=100&sortKey=date&sortDirection=descending")
            rows = [r for r in data["records"] if r.get("movieId") == hid][:20]
    else:
        data = api(svc, "GET", "/history?pageSize=20&sortKey=date&sortDirection=descending")
        rows = data["records"]
    for r in rows:
        print("  %s  %s  %s" % (r["date"][:19], r["eventType"], r.get("sourceTitle", "")))


def cmd_wanted(svc, args):
    sortk = "airDateUtc" if svc.startswith("sonarr") else "title"
    data = api(svc, "GET", "/wanted/missing?pageSize=50&sortKey=%s&sortDirection=descending" % sortk)
    print("missing: %d" % data["totalRecords"])
    for r in data["records"][:40]:
        if "episodeNumber" in r:
            ser = (r.get("series") or {}).get("title", "?")
            print("  %s  S%dE%d  %s" % (ser, r["seasonNumber"], r["episodeNumber"], r.get("title", "")))
        else:
            print("  %s (%s)" % (r.get("title"), r.get("year")))


def cmd_raw(svc, args):
    if len(args) < 2:
        die("raw: usage: arr <svc> raw <METHOD> <path> [json-body]")
    method, path = args[0].upper(), args[1]
    if not path.startswith("/"):
        path = "/" + path
    body = json.loads(args[2]) if len(args) > 2 else None
    out = api(svc, method, path, body)
    print(json.dumps(out, indent=2) if out is not None else "(empty response)")


def cmd_parse(svc, args):
    """Show how Sonarr/Radarr interprets a release title (series + episode map)."""
    if not args:
        die("parse: need a release title")
    r = api(svc, "GET", "/parse?title=" + urllib.parse.quote(args[0]))
    pe = (r or {}).get("parsedEpisodeInfo") or {}
    series = (r or {}).get("series") or {}
    print("title: " + args[0])
    print("  series: " + (series.get("title") or "— NO MATCH —"))
    print("  parsed: season=%s eps=%s absolute=%s" % (
        pe.get("seasonNumber"), pe.get("episodeNumbers"), pe.get("absoluteEpisodeNumbers")))
    mapped = ", ".join("S%dE%d" % (e["seasonNumber"], e["episodeNumber"])
                       for e in ((r or {}).get("episodes") or []))
    print("  mapped: " + (mapped or "— none —"))


def cmd_search(svc, args):
    """Cross-indexer Prowlarr search with dedup + group/indexer filters."""
    if svc != "prowlarr":
        die("search: prowlarr only (sonarr/radarr use status/releases)")
    flags, rest = pop_flags(args, {"--group": 1, "--indexer": 1, "--limit": 1, "--json": 0})
    if not rest:
        die("search: need a query")
    qs = urllib.parse.urlencode({"query": rest[0], "type": "search",
                                 "limit": flags.get("--limit", "100")})
    res = api("prowlarr", "GET", "/search?" + qs, timeout=SEARCH_TIMEOUT) or []
    seen, rows = set(), []
    for r in res:
        k = (r.get("title"), r.get("indexer"))
        if k in seen:
            continue
        seen.add(k)
        rows.append(r)
    if flags.get("--group"):
        g = flags["--group"].lower()
        rows = [r for r in rows if g in (r.get("title", "").lower())]
    if flags.get("--indexer"):
        ix = flags["--indexer"].lower()
        rows = [r for r in rows if ix in (r.get("indexer", "").lower())]
    if "--json" in flags:
        print(json.dumps(rows, indent=2))
        return
    rows.sort(key=lambda r: -(r.get("size") or 0))
    print("results: %d" % len(rows))
    for r in rows[:100]:
        sd = " %ss" % r["seeders"] if r.get("seeders") is not None else ""
        print("  [%s] %sMB %s%s  %s" % (
            r.get("indexer"), mb(r.get("size")), r.get("protocol"), sd, r.get("title")))


def _episode_number(name):
    """Parse an episode reference from a filename.
    Returns ('se', season, ep) | ('abs', n) | None."""
    base = re.sub(r"\.(mkv|mp4|avi|m4v)$", "", name, flags=re.I)
    base = re.sub(r"\b\d{3,4}p\b", " ", base, flags=re.I)        # 480p/720p/1080p
    base = re.sub(r"\[[0-9A-Fa-f]{6,8}\]", " ", base)            # crc hashes
    base = re.sub(r"\b(?:x?26[45]|HEVC|AAC|MP3|XviD|FLAC|BD|WEB)\b", " ", base, flags=re.I)
    m = re.search(r"S(\d{1,2})\s*E(\d{1,3})", base, re.I)
    if m:
        return ("se", int(m.group(1)), int(m.group(2)))
    m = re.search(r"episode[ ._-]*0*(\d{1,3})", base, re.I)
    if m:
        return ("abs", int(m.group(1)))
    m = re.search(r"[-–][ ._]*0*(\d{1,3})(?:v\d)?\b", base)      # "- 104"
    if m:
        return ("abs", int(m.group(1)))
    nums = re.findall(r"\b0*(\d{1,3})\b", base)                  # fallback: last 1-3 digit
    if nums:
        return ("abs", int(nums[-1]))
    return None


def _choose_ep(name, mode, season, by_se, by_abs):
    info = _episode_number(name)
    if not info:
        return None
    if info[0] == "se":
        s, e = info[1], info[2]
        if mode in ("auto", "se") and (s, e) in by_se:
            return by_se[(s, e)]
        if mode in ("auto", "abs") and e in by_abs:   # treat ep-part as absolute
            return by_abs[e]
        return None
    n = info[1]
    if mode != "abs" and season is not None and (season, n) in by_se:
        return by_se[(season, n)]
    if mode != "se" and n in by_abs:
        return by_abs[n]
    if season is not None and (season, n) in by_se:
        return by_se[(season, n)]
    return None


def cmd_import(svc, args):
    """Force-import downloaded files into explicit episodes, bypassing name match.

    arr sonarr import <folder> --series <id|query> [--match SUBSTR] [--season N]
        [--map auto|abs|se] [--mode copy|move] [--dry-run]
    Maps each file to an episode by parsing its number (absolute or SxxExx).

    SAFETY: a folder may hold files from many shows. --match restricts to files
    whose name contains SUBSTR (case-insensitive). Without it, EVERY file under
    the folder is mapped onto this series — only safe for a single-show folder."""
    if not svc.startswith("sonarr"):
        die("import: sonarr only")
    flags, rest = pop_flags(args, {"--series": 1, "--match": 1, "--season": 1,
                                   "--map": 1, "--mode": 1, "--dry-run": 0})
    if not rest or "--series" not in flags:
        die("import: usage: arr sonarr import <folder> --series <id|query> "
            "[--match SUBSTR] [--season N] [--map auto|abs|se] [--mode copy|move] [--dry-run]")
    sid = resolve_id(svc, flags["--series"])
    match = (flags.get("--match") or "").lower()
    mapmode = flags.get("--map", "auto")
    season = int(flags["--season"]) if "--season" in flags else None
    impmode = flags.get("--mode", "copy")
    files = api(svc, "GET",
                "/manualimport?folder=%s&filterExistingFiles=false" % urllib.parse.quote(rest[0]),
                timeout=SEARCH_TIMEOUT)
    eps = api(svc, "GET", "/episode?seriesId=%d" % sid)
    by_se = {(e["seasonNumber"], e["episodeNumber"]): e for e in eps}
    by_abs = {e["absoluteEpisodeNumber"]: e for e in eps if e.get("absoluteEpisodeNumber")}
    payload, skipped = [], []
    for f in files or []:
        name = (f.get("relativePath") or f.get("path") or "").split("/")[-1]
        if match and match not in name.lower():
            continue
        ep = _choose_ep(name, mapmode, season, by_se, by_abs)
        if not ep or not f.get("quality"):
            skipped.append(name)
            continue
        payload.append({
            "path": f["path"], "seriesId": sid, "episodeIds": [ep["id"]],
            "quality": f["quality"], "languages": f.get("languages") or [{"id": 1, "name": "English"}],
            "releaseGroup": f.get("releaseGroup", ""),
            "_label": "%s  ->  S%dE%d" % (name, ep["seasonNumber"], ep["episodeNumber"]),
        })
    for p in payload:
        print("  " + p.pop("_label"))
    if skipped:
        print("  (skipped %d unmatched: %s%s)" % (
            len(skipped), ", ".join(skipped[:3]), " ..." if len(skipped) > 3 else ""))
    if not payload:
        print("nothing to import")
        return
    if "--dry-run" in flags:
        print("DRY: would ManualImport %d file(s) [mode=%s]" % (len(payload), impmode))
        return
    r = api(svc, "POST", "/command",
            {"name": "ManualImport", "importMode": impmode, "files": payload})
    print("ManualImport queued: id=%s status=%s (%d files)" % (
        r.get("id"), r.get("status"), len(payload)))


# --- SABnzbd commands (arr sab <cmd>) ---------------------------------------
def sab_queue(args):
    q = sab_api("queue", {"limit": "500"})["queue"]
    pat = args[0].lower() if args else None
    print("queue: %s items, %sMB left, speed=%sKB/s, status=%s%s" % (
        q.get("noofslots"), q.get("mbleft"), q.get("kbpersec"), q.get("status"),
        " (PAUSED)" if q.get("paused") else ""))
    for s in q["slots"]:
        if pat and pat not in s["filename"].lower():
            continue
        print("  %s%% %s prio=%s  %s" % (
            s.get("percentage"), s.get("status"), s.get("priority"), s["filename"][:70]))


def sab_status(args):
    q = sab_api("queue", {"limit": "1"})["queue"]
    print("speed=%sKB/s status=%s paused=%s mbleft=%s" % (
        q.get("kbpersec"), q.get("status"), q.get("paused"), q.get("mbleft")))
    st = sab_api("server_stats").get("servers", {})
    for name, s in st.items():
        print("  server %s: today=%sMB" % (name, round((s.get("day") or 0) / 1048576)))
    w = sab_api("warnings").get("warnings", [])
    if w:
        print("recent warnings:")
        for x in w[-5:]:
            print("  " + (x.get("text") if isinstance(x, dict) else str(x)))


def sab_add(args):
    flags, rest = pop_flags(args, {"--cat": 1, "--name": 1})
    if not rest:
        die("sab add: need an NZB url")
    ok = sab_add_url(rest[0], cat=flags.get("--cat", "*"), name=flags.get("--name"))
    print("added" if ok else "FAILED")


def sab_prio(args):
    """arr sab prio <pattern> [--top|--force|--normal] — reprioritize queue items."""
    flags, rest = pop_flags(args, {"--top": 0, "--force": 0, "--normal": 0})
    if not rest:
        die("sab prio: need a filename pattern")
    pat = rest[0].lower()
    prio = "0" if "--normal" in flags else "2"   # 2=Force (default)
    q = sab_api("queue", {"limit": "500"})["queue"]
    hits = [s for s in q["slots"] if pat in s["filename"].lower()]
    for s in hits:
        sab_api("queue", {"name": "priority", "value": s["nzo_id"], "value2": prio})
        if "--top" in flags or "--force" in flags:
            sab_api("switch", {"value": s["nzo_id"], "value2": "0"})
        print("  prioritized: " + s["filename"][:60])
    print("(%d item(s))" % len(hits))


def sab_history(args):
    pat = args[0].lower() if args else None
    h = sab_api("history", {"limit": "50"})["history"]
    for s in h["slots"]:
        if pat and pat not in s["name"].lower():
            continue
        print("  %s  %s%s" % (s.get("status"), s["name"][:60],
                              ("  ! " + s["fail_message"]) if s.get("fail_message") else ""))


def sab_cleanup(args):
    """arr sab cleanup <pattern> [--yes] — delete completed downloads (and their
    files) matching a name pattern via the SAB API. SAB removes the files as the
    sabnzbd user, sidestepping the ownership/permission walls that trip up rm."""
    flags, rest = pop_flags(args, {"--yes": 0})
    if not rest:
        die("sab cleanup: need a name pattern")
    pat = rest[0].lower()
    h = sab_api("history", {"limit": "500"})["history"]
    hits = [s for s in h["slots"] if pat in s["name"].lower()]
    if not hits:
        print("no completed downloads matching '%s'" % rest[0])
        return
    go = "--yes" in flags
    print("%s%d completed download(s) matching '%s' (delete removes files too):" % (
        "" if go else "[dry-run] ", len(hits), rest[0]))
    for s in hits:
        print("  %s  %s" % (s.get("status"), s["name"][:64]))
    if not go:
        print("  (pass --yes to delete history entries + files)")
        return
    n = 0
    for s in hits:
        r = sab_api("history", {"name": "delete", "value": s["nzo_id"], "del_files": "1"})
        if isinstance(r, dict) and r.get("status"):
            n += 1
    print("deleted %d item(s) + their files" % n)


SAB_COMMANDS = {
    "queue": sab_queue, "status": sab_status, "add": sab_add,
    "prio": sab_prio, "history": sab_history, "cleanup": sab_cleanup,
}


def _parse_seasons(spec):
    """'3-8' / '3,5' / '3-5,7' -> set of ints."""
    out = set()
    for part in spec.split(","):
        part = part.strip()
        if "-" in part:
            a, b = part.split("-", 1)
            out.update(range(int(a), int(b) + 1))
        elif part:
            out.add(int(part))
    return out


def _qname(f):
    return ((f.get("quality") or {}).get("quality") or {}).get("name", "?")


def cmd_files(svc, args):
    """List the files actually on disk for an item (path, size, quality)."""
    flags, rest = pop_flags(args, {"--full": 0})
    if not rest:
        die("files: need an id or query")
    if svc.startswith("sonarr"):
        sid = resolve_id(svc, rest[0])
        files = api(svc, "GET", "/episodefile?seriesId=%d" % sid) or []
        by_season = {}
        for f in files:
            by_season.setdefault(f["seasonNumber"], []).append(f)
        print("%d file(s), %sGB total" % (len(files), gb(sum(f.get("size", 0) for f in files))))
        for sn in sorted(by_season):
            fs = by_season[sn]
            print("  S%d: %d files, %sGB" % (sn, len(fs), gb(sum(f.get("size", 0) for f in fs))))
            if "--full" in flags:
                for f in sorted(fs, key=lambda x: x.get("relativePath", "")):
                    print("      %sMB  %s  %s" % (mb(f.get("size")), _qname(f), f.get("relativePath")))
    else:
        mid = resolve_id("radarr", rest[0])
        files = api("radarr", "GET", "/moviefile?movieId=%d" % mid) or []
        print("%d file(s):" % len(files))
        for f in files:
            print("  %sGB  %s  %s" % (gb(f.get("size")), _qname(f), f.get("relativePath") or f.get("path")))


def cmd_delete(svc, args):
    """Delete media via the arr API (deletes as the service user — no rm/perms
    fight — and updates the DB so it won't silently re-download). Dry-run unless
    --yes. sonarr: --seasons X-Y (+ unmonitor) | --all. radarr: --file-only | (default whole movie)."""
    flags, rest = pop_flags(args, {"--seasons": 1, "--all": 0, "--file-only": 0,
                                   "--keep-monitored": 0, "--yes": 0})
    if not rest:
        die("delete: need an id or query")
    go = "--yes" in flags
    tag = "" if go else "[dry-run] "
    if svc.startswith("sonarr"):
        sid = resolve_id(svc, rest[0])
        s = api(svc, "GET", "/series/%d" % sid)
        allfiles = api(svc, "GET", "/episodefile?seriesId=%d" % sid) or []
        if "--all" in flags:
            print("%sDELETE SERIES '%s' + %d file(s) (%sGB)" % (
                tag, s["title"], len(allfiles), gb(sum(f.get("size", 0) for f in allfiles))))
            if not go:
                print("  (pass --yes to delete)")
                return
            api(svc, "DELETE", "/series/%d?deleteFiles=true" % sid)
            print("deleted series + files")
            return
        if "--seasons" not in flags:
            die("sonarr delete: need --seasons X-Y or --all")
        want = _parse_seasons(flags["--seasons"])
        files = [f for f in allfiles if f["seasonNumber"] in want]
        unmon = "--keep-monitored" not in flags
        print("%s'%s' seasons %s: delete %d file(s) (%sGB)%s" % (
            tag, s["title"], ",".join(map(str, sorted(want))), len(files),
            gb(sum(f.get("size", 0) for f in files)), " + unmonitor" if unmon else ""))
        for sn in sorted(want):
            fs = [f for f in files if f["seasonNumber"] == sn]
            print("  S%d: %d files, %sGB" % (sn, len(fs), gb(sum(f.get("size", 0) for f in fs))))
        if not go:
            print("  (pass --yes to delete)")
            return
        ids = [f["id"] for f in files]
        if ids:
            api(svc, "DELETE", "/episodefile/bulk", {"episodeFileIds": ids})
        if unmon:
            for se in s["seasons"]:
                if se["seasonNumber"] in want:
                    se["monitored"] = False
            api(svc, "PUT", "/series/%d" % sid, s)
        print("deleted %d file(s)%s" % (len(ids), " + unmonitored" if unmon else ""))
    else:
        mid = resolve_id("radarr", rest[0])
        m = api("radarr", "GET", "/movie/%d" % mid)
        if "--file-only" in flags:
            files = api("radarr", "GET", "/moviefile?movieId=%d" % mid) or []
            print("%s'%s': delete file only (%d, %sGB) — keeps movie for re-grab" % (
                tag, m["title"], len(files), gb(sum(f.get("size", 0) for f in files))))
            if not go:
                print("  (pass --yes to delete)")
                return
            for f in files:
                api("radarr", "DELETE", "/moviefile/%d" % f["id"])
            print("deleted %d file(s)" % len(files))
            return
        print("%sDELETE MOVIE '%s' + file (%sGB)" % (tag, m["title"], gb(m.get("sizeOnDisk"))))
        if not go:
            print("  (pass --yes to delete)")
            return
        api("radarr", "DELETE", "/movie/%d?deleteFiles=true" % mid)
        print("deleted movie + file")


def cmd_jf_unwatched(args):
    """Series with >=N regular seasons on disk that NO Jellyfin user has played
    or has progress on — the 'safe to delete' report. Sized via Sonarr."""
    flags, _ = pop_flags(args, {"--min-seasons": 1})
    min_seasons = int(flags.get("--min-seasons", "2"))
    watched = set()
    for u in jf_api("/Users"):
        for filt in ("IsPlayed", "IsResumable"):
            r = jf_api("/Users/%s/Items" % u["Id"],
                       {"IncludeItemTypes": "Episode", "Recursive": "true",
                        "Filters": filt, "Fields": "SeriesName", "Limit": "100000"})
            for it in r.get("Items", []):
                if it.get("SeriesName"):
                    watched.add(it["SeriesName"].lower())
    cand = []
    for s in api("sonarr", "GET", "/series"):
        st = s.get("statistics") or {}
        nseas = sum(1 for se in s["seasons"]
                    if se["seasonNumber"] > 0 and (se.get("statistics") or {}).get("episodeFileCount", 0) > 0)
        if nseas < min_seasons or st.get("episodeFileCount", 0) == 0:
            continue
        if s["title"].lower() in watched:
            continue
        cand.append(s)
    cand.sort(key=lambda s: -((s.get("statistics") or {}).get("sizeOnDisk", 0))
              )
    tot = sum((s.get("statistics") or {}).get("sizeOnDisk", 0) for s in cand)
    print("unwatched: %d series (>=%d seasons on disk, no play/progress by any Jellyfin user), %sGB" % (
        len(cand), min_seasons, gb(tot)))
    print("  %-38s %5s %5s %9s" % ("series", "seas", "eps", "size"))
    for s in cand:
        st = s.get("statistics") or {}
        nseas = sum(1 for se in s["seasons"]
                    if se["seasonNumber"] > 0 and (se.get("statistics") or {}).get("episodeFileCount", 0) > 0)
        print("  %-38s %5d %5d %7sGB" % (s["title"][:38], nseas, st.get("episodeFileCount", 0), gb(st.get("sizeOnDisk"))))


def _item_titles(item):
    """Distinct title variants for an item: title, originalTitle, altTitles."""
    out, seen = [], set()
    for t in [item.get("title"), item.get("originalTitle")] + \
             [a.get("title") for a in (item.get("alternateTitles") or [])]:
        if t and t.lower() not in seen:
            seen.add(t.lower())
            out.append(t)
    return out


def cmd_availability(svc, args):
    """Is this obtainable on our indexers? Searches using the item's OWN stored
    titles + imdb/tmdb ids (no guessing romanizations), reports found/unavailable."""
    if not args:
        die("availability: need an id or query")
    coll = "series" if svc.startswith("sonarr") else "movie"
    iid = resolve_id(svc, args[0])
    item = api(svc, "GET", "/%s/%d" % (coll, iid))
    year = item.get("year")
    print("%s (%s)  imdb=%s tmdb=%s" % (item.get("title"), year, item.get("imdbId"), item.get("tmdbId")))
    found = 0
    if svc == "radarr":
        rels = api("radarr", "GET", "/release?movieId=%d" % iid, timeout=SEARCH_TIMEOUT) or []
        ok = [r for r in rels if not r.get("rejected")]
        print("  radarr id-search: %d release(s), %d not rejected" % (len(rels), len(ok)))
        found += len(ok)
    uniq = set()
    print("  prowlarr title searches:")
    for t in _item_titles(item)[:6]:
        q = "%s %s" % (t, year) if year else t
        res = api("prowlarr", "GET", "/search?" + urllib.parse.urlencode(
            {"query": q, "type": "search", "limit": "30"}), timeout=SEARCH_TIMEOUT) or []
        for r in res:
            uniq.add(r.get("title"))
        print("    %-46s %d" % (q[:46], len(res)))
    found += len(uniq)
    print("  => %s" % ("AVAILABLE" if found else "UNAVAILABLE on current indexers"))


def cmd_lookup(svc, args):
    """Search TMDB/TVDB metadata for a title (disambiguate, e.g. 1990 vs 2003)."""
    if not args:
        die("lookup: need a search term")
    coll = "series" if svc.startswith("sonarr") else "movie"
    res = api(svc, "GET", "/%s/lookup?term=%s" % (coll, urllib.parse.quote(" ".join(args))))
    for it in (res or [])[:12]:
        ids = "tmdb=%s imdb=%s" % (it.get("tmdbId"), it.get("imdbId"))
        if it.get("tvdbId"):
            ids += " tvdb=%s" % it["tvdbId"]
        orig = ""
        if it.get("originalTitle") and it["originalTitle"] != it.get("title"):
            orig = "  orig=%s" % it["originalTitle"]
        print("  %s (%s)  %s%s" % (it.get("title"), it.get("year"), ids, orig))


def cmd_info(svc, args):
    """Concise identity for an item (title/year/ids/originalTitle/altTitles/state)."""
    if not args:
        die("info: need an id or query")
    coll = "series" if svc.startswith("sonarr") else "movie"
    iid = resolve_id(svc, args[0])
    it = api(svc, "GET", "/%s/%d" % (coll, iid))
    print("%s (%s)" % (it.get("title"), it.get("year")))
    ids = "id=%d tmdb=%s imdb=%s" % (iid, it.get("tmdbId"), it.get("imdbId"))
    if it.get("tvdbId"):
        ids += " tvdb=%s" % it["tvdbId"]
    print("  " + ids)
    if it.get("originalTitle"):
        print("  originalTitle: %s" % it["originalTitle"])
    alts = [a.get("title") for a in (it.get("alternateTitles") or []) if a.get("title")]
    if alts:
        print("  altTitles: %s" % ", ".join(alts[:12]))
    st = it.get("statistics") or {}
    if svc.startswith("sonarr"):
        disk = "%d/%d eps, %sGB" % (st.get("episodeFileCount", 0), st.get("totalEpisodeCount", 0), gb(st.get("sizeOnDisk")))
    else:
        disk = ("ON DISK %sGB" % gb(it.get("sizeOnDisk"))) if it.get("hasFile") else "no file"
    print("  monitored=%s  %s" % (it.get("monitored"), disk))
    print("  path: %s" % it.get("path"))


_seerr_title_cache = {}


def _seerr_title(mtype, tmdb):
    if not tmdb:
        return None
    key = (mtype, tmdb)
    if key in _seerr_title_cache:
        return _seerr_title_cache[key]
    d = seerr_api("/%s/%s" % ("tv" if mtype == "tv" else "movie", tmdb), soft=True) or {}
    t = d.get("name") if mtype == "tv" else d.get("title")
    _seerr_title_cache[key] = t
    return t


def seerr_requests(args):
    """Jellyseerr requests: who requested what, request + media status."""
    flags, rest = pop_flags(args, {"--pending": 0, "--limit": 1})
    params = {"take": flags.get("--limit", "50"), "skip": "0", "sort": "added"}
    if "--pending" in flags:
        params["filter"] = "pending"
    data = seerr_api("/request", params)
    rstat = {1: "pending", 2: "approved", 3: "declined", 4: "failed", 5: "completed"}
    mstat = {1: "unknown", 2: "pending", 3: "processing", 4: "partial", 5: "available"}
    pat = rest[0].lower() if rest else None
    rows = []
    for r in data.get("results", []):
        m = r.get("media") or {}
        title = m.get("title") or _seerr_title(m.get("mediaType"), m.get("tmdbId")) or "tmdb:%s" % m.get("tmdbId")
        if pat and pat not in str(title).lower():
            continue
        rows.append((r, m, title))
    for r, m, title in rows:
        print("  #%s  %s [%s]  req=%s media=%s  by %s  %s" % (
            r.get("id"), title, m.get("mediaType"),
            rstat.get(r.get("status"), r.get("status")),
            mstat.get(m.get("status"), m.get("status")),
            (r.get("requestedBy") or {}).get("displayName", "?"),
            (r.get("createdAt") or "")[:10]))
    print("(%d request(s))" % len(rows))


def seerr_request(args):
    if not args:
        die("seerr request: need a request id")
    print(json.dumps(seerr_api("/request/%s" % args[0]), indent=2))


SEERR_COMMANDS = {"requests": seerr_requests, "request": seerr_request}


JELLYFIN_COMMANDS = {"unwatched": cmd_jf_unwatched}


COMMANDS = {
    "status": cmd_status, "get": cmd_get, "seasons": cmd_seasons,
    "releases": cmd_releases, "grab": cmd_grab, "monitor": cmd_monitor,
    "queue": cmd_queue, "stuck": cmd_stuck, "history": cmd_history, "wanted": cmd_wanted,
    "parse": cmd_parse, "search": cmd_search, "import": cmd_import,
    "files": cmd_files, "delete": cmd_delete,
    "availability": cmd_availability, "lookup": cmd_lookup, "info": cmd_info,
    "raw": cmd_raw,
}

USAGE = """arr — Sonarr/Radarr/Prowlarr/SABnzbd CLI

  arr <service> <command> [args]
  service: sonarr | sonarr-anime | radarr | prowlarr | sab | jellyfin | seerr
  (sonarr-anime = the dedicated anime Sonarr on :8990; all sonarr commands work
   against it, e.g. `arr sonarr-anime status`, `arr sonarr-anime seasons <show>`)

Commands (sonarr & radarr unless noted):
  status [query]                  list items (optionally filter by title)
  get <id|query>                  full JSON for one item
  seasons <id|query>              (sonarr) per-season monitored + on-disk
  releases <id|query> [--season N|--episode EPID]
  grab <id|query> [--season N|--episode EPID] [--override|--via-sab] [--dry-run]
        no flags: search & let the arr decide (respects quality profile)
        --override: force-push every candidate release (bypass rejections)
        --via-sab: send candidate NZBs straight to SAB (bypass search cache)
  monitor <id|query> <spec>       sonarr: all|none|s1,s2,...   radarr: on|off
  info <id|query>                 concise identity (title/year/ids/origTitle/altTitles/state)
  lookup <term>                   TMDB/TVDB metadata search (disambiguate 1990 vs 2003)
  availability <id|query>         is it obtainable? searches the item's own titles + ids
  files <id|query> [--full]       files on disk (path/size/quality)
  delete <id|query> ...           DELETE via API (dry-run unless --yes):
        sonarr: --seasons X-Y [--keep-monitored] | --all
        radarr: --file-only (drop file, keep movie) | (default: movie + file)
  parse "<release title>"         show how the arr maps a title (series+episode)
  import <folder> --series <id|query> [--match SUBSTR] [--season N]
        [--map auto|abs|se] [--mode copy|move] [--dry-run]
        force-import files into episodes, bypassing name matching (maps by
        absolute/SxxExx number). --match filters filenames (USE IT in a shared
        download folder, or every file gets mapped onto this series).
  queue [query]                    show download queue
  stuck [query] [--json|--quiet]   queue items needing intervention (failed/import-blocked/pending)
  history [id|query] | wanted
  search <query> [--group X] [--indexer Y] [--limit N] [--json]   (prowlarr)
  grab <query> [--indexer X] [--group/--match Y] [--cat C] [--all] [--dry-run]
        (prowlarr) grab a matched release to the right client by protocol:
        usenet -> SABnzbd, torrent -> qBittorrent. Refuses >1 match w/o --all.
  raw <METHOD> <path> [json]      arbitrary API call

SABnzbd:
  arr sab queue [pat] | status | add <url> [--cat C] | prio <pat> [--top]
      | history [pat] | cleanup <pat> [--yes]   (cleanup deletes via SAB, files too)

Jellyfin / Seerr:
  arr jellyfin unwatched [--min-seasons N]   series on disk no user has watched
  arr seerr requests [--pending] [query]     who requested what (+ media status)
  arr seerr request <id>                     full request detail

Keys come from $ARR_API_KEY_<SVC> / $ARR_API_KEY_{SAB,JELLYFIN,SEERR} or the
sops-rendered env file (%s). No sudo required.""" % ENV_FILE


def main(argv):
    if not argv or argv[0] in ("-h", "--help", "help"):
        print(USAGE)
        sys.exit(0 if argv else 1)
    svc = argv[0]
    if svc == "sab":
        if len(argv) < 2:
            die("sab: queue|status|add|prio|history|cleanup")
        fn = SAB_COMMANDS.get(argv[1])
        if not fn:
            die("unknown sab command '%s'" % argv[1])
        fn(argv[2:])
        return
    if svc == "jellyfin":
        if len(argv) < 2:
            die("jellyfin: unwatched")
        fn = JELLYFIN_COMMANDS.get(argv[1])
        if not fn:
            die("unknown jellyfin command '%s'" % argv[1])
        fn(argv[2:])
        return
    if svc == "seerr":
        if len(argv) < 2:
            die("seerr: requests|request")
        fn = SEERR_COMMANDS.get(argv[1])
        if not fn:
            die("unknown seerr command '%s'" % argv[1])
        fn(argv[2:])
        return
    if svc not in SERVICES:
        die("unknown service '%s' (want sonarr|sonarr-anime|radarr|prowlarr|sab|jellyfin|seerr)" % svc)
    if len(argv) < 2:
        print(USAGE)
        sys.exit(1)
    cmd, rest = argv[1], argv[2:]
    fn = COMMANDS.get(cmd)
    if not fn:
        die("unknown command '%s' (try: arr --help)" % cmd)
    fn(svc, rest)


if __name__ == "__main__":
    main(sys.argv[1:])
