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
import shutil
import subprocess
import sys
import time
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
# Bazarr (subtitle manager) API, port 6767; auth via X-API-KEY. NB Bazarr is
# wired to the MAIN sonarr (:8989) + radarr only — sonarr-anime items are not
# covered (Bazarr 1.x is single-instance-per-arr).
BAZARR = {"port": 6767, "key": "BAZARR_API_KEY"}


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
    # the hermes agent gets keys as plain env vars (SONARR_API_KEY=...) and
    # cannot read the sops-rendered file — check the direct name too
    if os.environ.get(name):
        return os.environ[name]
    val = _load_env_file().get(name)
    if not val:
        die("no key %s (looked at $%s, $%s and %s)" % (name, envvar, name, ENV_FILE))
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


def jf_api(path, params=None, timeout=60, method="GET", soft=False):
    qs = ("?" + urllib.parse.urlencode(params)) if params else ""
    url = "http://localhost:%d%s%s" % (JELLYFIN["port"], path, qs)
    req = urllib.request.Request(url, method=method)
    req.add_header("X-Emby-Token", jf_key())
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read().decode()
    except urllib.error.HTTPError as e:
        if soft:
            return None
        die("jellyfin %s -> HTTP %d" % (path, e.code))
    except (TimeoutError, urllib.error.URLError) as e:
        if soft:
            return None
        die("jellyfin %s -> %s" % (path, getattr(e, "reason", e)))
    return json.loads(raw) if raw.strip() else None


def bazarr_api(method, path, params=None, timeout=120, soft=False):
    qs = ("?" + urllib.parse.urlencode(params, doseq=True)) if params else ""
    url = "http://localhost:%d/api%s%s" % (BAZARR["port"], path, qs)
    req = urllib.request.Request(url, method=method)
    req.add_header("X-API-KEY", env_key(BAZARR["key"], "ARR_API_KEY_BAZARR"))
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read().decode()
    except urllib.error.HTTPError as e:
        if soft:
            return None
        die("bazarr %s %s -> HTTP %d %s" % (method, path, e.code, e.read().decode(errors="replace")[:200]))
    except (TimeoutError, urllib.error.URLError) as e:
        if soft:
            return None
        die("bazarr %s %s -> %s" % (method, path, getattr(e, "reason", e)))
    try:
        return json.loads(raw) if raw.strip() else None
    except ValueError:
        return raw


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


# --- requester tagging -------------------------------------------------------
# The download-notifier daemon (declared in /etc/nixos/configuration.nix) DMs the
# person who asked for a download, with a live progress bar. For requests that
# come in through Seerr it resolves the requester itself. For "hey bot, grab me
# X" asks made straight to Hermes, there's no Seerr row — so when Hermes grabs on
# someone's behalf it stamps the series/movie with a `requester:<discordId>` tag
# (their Discord user id) and the notifier reads that. Tag the DISCORD id, not a
# Jellyfin username: it's exactly what's needed to DM, Hermes already has it, and
# it works for people with no Jellyfin account.
def _ensure_tag(svc, label):
    for t in api(svc, "GET", "/tag") or []:
        if (t.get("label") or "").lower() == label.lower():
            return t["id"]
    return api(svc, "POST", "/tag", {"label": label})["id"]


def _stamp_label(svc, item_id, label, remove=False):
    """Add/remove one tag label on a series/movie via the editor endpoint."""
    if svc.startswith("sonarr"):
        coll, ids_field = "series", "seriesIds"
    elif svc == "radarr":
        coll, ids_field = "movie", "movieIds"
    else:
        die("tag: only sonarr/sonarr-anime/radarr have taggable items")
    tag_id = _ensure_tag(svc, label)
    api(svc, "PUT", "/%s/editor" % coll,
        {ids_field: [item_id], "tags": [tag_id],
         "applyTags": "remove" if remove else "add"})
    return coll


def _tag_requester(svc, query, discord_id, remove=False):
    did = str(discord_id).strip()
    if not did.isdigit():
        die("tag: --requester must be a numeric Discord user id (got %r)" % discord_id)
    item_id = resolve_id(svc, query)
    # Sonarr/Radarr tag labels allow only [a-z0-9-] (no colon), so `requester-<id>`.
    coll = _stamp_label(svc, item_id, "requester-" + did, remove)
    return coll, item_id, did


def _require_labels(flags):
    """['require-subs-eng', ...] from --require-subs/--require-audio flags.
    The download-notifier reads these at ready-time and appends a verified
    '🔎 eng subs ✓' line to the ✅ embed — no watcher cron needed."""
    out = []
    if flags.get("--require-subs"):
        out.append("require-subs-" + _norm_lang(flags["--require-subs"]))
    if flags.get("--require-audio"):
        out.append("require-audio-" + _norm_lang(flags["--require-audio"]))
    return out


def cmd_tag(svc, args):
    """arr <svc> tag <id|query> [--requester <discordId>]
        [--require-subs LANG] [--require-audio LANG] [--remove]
    requester-* routes download-notifier DMs; require-* makes its ✅ ready
    embed VERIFY the language (ffprobe) — use for "with English subs" asks."""
    flags, rest = pop_flags(args, {"--requester": 1, "--remove": 0,
                                   "--require-subs": 1, "--require-audio": 1})
    req_labels = _require_labels(flags)
    if not rest or not (flags.get("--requester") or req_labels):
        die("tag: usage: arr <svc> tag <id|query> [--requester <discordId>]"
            " [--require-subs LANG] [--require-audio LANG] [--remove]")
    remove = "--remove" in flags
    verb = "removed from" if remove else "added to"
    if flags.get("--requester"):
        coll, item_id, did = _tag_requester(svc, rest[0], flags["--requester"], remove)
        print("requester:%s %s %s #%d" % (did, verb, coll, item_id))
    for lab in req_labels:
        item_id = resolve_id(svc, rest[0])
        coll = _stamp_label(svc, item_id, lab, remove)
        print("%s %s %s #%d (notifier verifies at ready-time)" % (lab, verb, coll, item_id))


# --- commands ----------------------------------------------------------------
def _season_gap_lines(svc, s):
    """Cheap per-season gap flags from a series object's season statistics.
    'aired' below = Sonarr's episodeCount stat (monitored episodes that aired)."""
    out, gaps = [], False
    for se in s.get("seasons", []):
        sn = se["seasonNumber"]
        st = se.get("statistics") or {}
        have = st.get("episodeFileCount", 0)
        aired = st.get("episodeCount", 0)
        total = st.get("totalEpisodeCount", 0)
        if sn == 0:
            if total:
                out.append("      S0 specials: %d/%d on disk%s" % (
                    have, total, "" if se.get("monitored") else " (unmonitored)"))
            continue
        if se.get("monitored"):
            if have < aired:
                gaps = True
                out.append("      ⚠ S%d: %d/%d aired eps on disk (%s)" % (
                    sn, have, aired, "partial" if have else "missing"))
        elif have < total:
            gaps = True
            out.append("      ○ S%d: not monitored, %d/%d on disk" % (sn, have, total))
    if gaps:
        out.append("      -> gaps: `arr %s coverage %d --fix` searches missing MONITORED"
                   " eps; unmonitored seasons need the user's OK first" % (svc, s["id"]))
    return out


def cmd_status(svc, args):
    q = (args[0].lower() if args else "")
    if svc.startswith("sonarr"):
        items = sorted(api(svc, "GET", "/series"), key=lambda x: x["title"])
        matches = [s for s in items if not q or q in s["title"].lower()]
        for s in matches:
            st = s.get("statistics", {})
            print("[%d] %s (%s) — %s, mon=%s" % (s["id"], s["title"], s.get("year"), s["status"], s["monitored"]))
            print("      %d/%d eps on disk (%s%%), %sGB" % (
                st.get("episodeFileCount", 0), st.get("totalEpisodeCount", 0),
                st.get("percentOfEpisodes", 0), gb(st.get("sizeOnDisk"))))
            # exactly one match -> surface per-season coverage right here, so a
            # "do we have X?" check can't miss a partially-downloaded season
            if len(matches) == 1:
                for ln in _season_gap_lines(svc, s):
                    print(ln)
                _audit_warn(svc, s["id"], item=s)
    elif svc == "radarr":
        items = sorted(api(svc, "GET", "/movie"), key=lambda x: x["title"])
        matches = [m for m in items if not q or q in m["title"].lower()]
        for m in matches:
            disk = "ON DISK (%sGB)" % gb(m.get("sizeOnDisk")) if m.get("hasFile") else "MISSING"
            print("[%d] %s (%s) — %s, mon=%s, %s" % (m["id"], m["title"], m.get("year"), m["status"], m["monitored"], disk))
            if len(matches) == 1:
                _audit_warn(svc, m["id"], item=m)
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
    aflags, args = pop_flags(args, {"--audio": 1, "--timeout": 1})
    rels = api(svc, "GET", "/release?" + _release_query(svc, args),
               timeout=int(aflags.get("--timeout", SEARCH_TIMEOUT)))
    if aflags.get("--audio"):
        want = _norm_lang(aflags["--audio"])
        rels = [r for r in rels if _audio_match(r.get("title"), want)]
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
    usenet -> SABnzbd, torrent -> qBittorrent. Refuses >1 match without --all.
    Direct grabs skip the arr's TRaSH-synced profile scoring, so vet the
    release name yourself (fake upscales like "AI UPSCALE"/bluury-style
    groups, BR-DISK, LQ groups would all have been auto-rejected)."""
    flags, rest = pop_flags(args, {"--indexer": 1, "--group": 1, "--match": 1,
                                   "--cat": 1, "--limit": 1, "--all": 0, "--dry-run": 0})
    print("note: direct grabs skip the arr's TRaSH profile scoring — vet the name"
          " (upscale/LQ/BR-DISK markers) before pushing")
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


def _promote_downloads(records):
    """Move fresh grabs to the front of the download clients' queues. An
    interactive add/grab means a person is waiting — without this, new requests
    starve behind backlog churn (dead-release grinds hog every connection)."""
    bumped = 0
    for r in records:
        did = r.get("downloadId")
        if not did:
            continue
        try:
            if r.get("protocol") == "usenet":
                sab_api("queue", {"name": "priority", "value": did, "value2": "2"})
                sab_api("switch", {"value": did, "value2": "0"})
            else:
                urllib.request.urlopen(urllib.request.Request(
                    "http://127.0.0.1:%d/api/v2/torrents/topPrio" % QBIT["port"],
                    data=("hashes=%s" % did.lower()).encode()), timeout=15).read()
            bumped += 1
        except Exception:
            pass  # promotion is best-effort; the download itself is unaffected
    if bumped:
        print("  promoted %d download(s) to the front of the queue" % bumped)


def _report_first_grab(svc, iid, is_series, timeout=60):
    """Briefly poll the queue after triggering a search, so one command can
    honestly say "downloading <release>" instead of "search started" — this is
    what add/grab callers otherwise reconstruct with status/history/queue
    follow-ups. Fresh grabs get promoted to the front of the download queue
    (an interactive request means someone is waiting on it)."""
    deadline = time.time() + timeout
    while True:
        try:
            q = _queue_records(svc)["records"]
        except SystemExit:
            q = []
        if is_series:
            mine = [r for r in q if (r.get("seriesId") or (r.get("series") or {}).get("id")) == iid]
        else:
            mine = [r for r in q if ((r.get("movie") or {}).get("id") or r.get("movieId")) == iid]
        if mine:
            size = sum(r.get("size") or 0 for r in mine)
            print("  grabbed %d release(s), %sGB — downloading:" % (len(mine), gb(size)))
            for r in mine[:4]:
                print("    " + (r.get("title") or "?")[:75])
            if len(mine) > 4:
                print("    ... +%d more" % (len(mine) - 4))
            _promote_downloads(mine)
            return True
        if time.time() > deadline:
            print("  nothing grabbed in %ds — the search may still be running"
                  " (indexers can be slow) or releases are scarce. `arr %s queue`"
                  " shows late grabs; `arr %s releases %s` shows candidates with"
                  " reject reasons" % (timeout, svc, svc, iid))
            return False
        time.sleep(5)


def cmd_grab(svc, args):
    if svc == "prowlarr":
        return cmd_prowlarr_grab(args)
    flags, rest = pop_flags(args, {"--season": 1, "--episode": 1,
                                   "--override": 0, "--via-sab": 0, "--dry-run": 0,
                                   "--wait": 0, "--timeout": 1, "--requester": 1,
                                   "--no-wait": 0})
    if not rest:
        die("grab: need an id or query")
    override, dry = "--override" in flags, "--dry-run" in flags

    # Stamp the requester so the download-notifier DMs them with a progress bar.
    # Tagging the series/movie is branch-independent, so do it once up front.
    if flags.get("--requester") and not dry and svc != "prowlarr":
        _coll, _id, _did = _tag_requester(svc, rest[0], flags["--requester"])
        print("tagged requester:%s on %s #%d" % (_did, _coll, _id))

    if "--via-sab" in flags:
        # Fetch candidate usenet releases and hand their NZBs straight to SAB
        # under the service category — bypasses Sonarr's search cache entirely
        # (so it works for releases Sonarr's per-episode search can't see).
        print("note: --via-sab bypasses the TRaSH-synced profile — its upscale/LQ/"
              "BR-DISK protection doesn't apply; vet release names yourself")
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
        iid = resolve_id(svc, rest[0])
        if svc.startswith("sonarr"):
            if "--episode" in flags:
                body = {"name": "EpisodeSearch", "episodeIds": [int(flags["--episode"])]}
            elif "--season" in flags:
                body = {"name": "SeasonSearch", "seriesId": iid, "seasonNumber": int(flags["--season"])}
            else:
                body = {"name": "SeriesSearch", "seriesId": iid}
        else:
            body = {"name": "MoviesSearch", "movieIds": [iid]}
        if dry:
            print("DRY: POST /command " + json.dumps(body))
            return
        r = api(svc, "POST", "/command", body)
        print("queued %s (command id %s, status %s)" % (r["commandName"], r["id"], r["status"]))
        if "--wait" in flags:  # full search-command completion (bounded by --timeout)
            rec = _wait_command(svc, r["id"], timeout=int(flags.get("--timeout", 300)))
            print("  -> %s %s" % (rec.get("status"), rec.get("message") or ""))
        if "--no-wait" not in flags:
            _report_first_grab(svc, iid, svc.startswith("sonarr"),
                               timeout=int(flags.get("--timeout", 60)))
        return

    # --override: force-push every candidate release, bypassing rejections
    print("note: --override ignores every profile rejection (TRaSH scoring incl."
          " upscale/LQ/BR-DISK protection) — worth a concrete reason")
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
    """List queue items. Filter with a title substring or --disc/--encrypted/
    --quality NAME/--status STATE; --ids prints just the queue ids (pipe into
    queue-rm)."""
    flags, rest = pop_flags(args, {"--disc": 0, "--quality": 1, "--status": 1,
                                   "--title": 1, "--ids": 0, "--encrypted": 0})
    q = _queue_records(svc, page_size=1000)
    sel = _queue_select(q["records"], flags, rest)
    if "--ids" in flags:
        for r in sel:
            print(r["id"])
        return
    filt = "" if len(sel) == q["totalRecords"] else " (of %d)" % q["totalRecords"]
    print("queue: %d item(s)%s" % (len(sel), filt))
    sab_labels = _sab_flagged_labels()
    for r in sel:
        lbl = sab_labels.get((r.get("downloadId") or "").lower(), [])
        print("  %s/%s  %sMB left  %s%s" % (
            r.get("status"), r.get("trackedDownloadState"), mb(r.get("sizeleft")),
            ("[%s] " % ",".join(lbl)) if lbl else "", r.get("title")))
        if r.get("errorMessage"):
            print("        err: " + r["errorMessage"])
        msgs = _queue_status_messages(r)
        if msgs:
            print("        " + "; ".join(msgs))


def cmd_stuck(svc, args):
    """Show queue items that need intervention: failed/import blocked/pending.

    arr <svc> stuck [query] [--json|--quiet] [--fix [--yes]]
    --fix: for import-blocked/pending items, plan a force-import of the
    completed download into its linked series/movie (the fix that previously
    meant hand-crafting `arr import --match ...` every time). Shows the file->
    episode mapping; applies it (mode=move) only with --yes. Failed items get a
    suggested queue-rm+re-grab command instead."""
    flags, rest = pop_flags(args, {"--json": 0, "--quiet": 0, "--fix": 0, "--yes": 0})
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
    if "--fix" not in flags or not rows:
        return
    go = "--yes" in flags
    print("\nfix plan%s:" % ("" if go else " [dry-run — pass --yes to apply]"))
    cleared_dl = set()
    for r in rows:
        title = (r.get("title") or "")[:64]
        state = r.get("trackedDownloadState")
        if r.get("status") == "failed":
            print("  failed: %s" % title)
            print("    -> arr %s queue-rm %s --blocklist --yes   then re-grab" % (svc, r.get("id")))
            continue
        if state not in ("importBlocked", "importPending"):
            print("  %s: %s — no automated fix, inspect manually" % (state, title))
            continue
        # already satisfied? (episode/movie has a file — e.g. we just imported a
        # sibling record of the same season pack, or a better release landed)
        # NB the episode/movie objects EMBEDDED in queue records are stale
        # snapshots — query live state.
        satisfied = False
        if svc.startswith("sonarr") and r.get("episodeId"):
            satisfied = bool((api(svc, "GET", "/episode/%d" % r["episodeId"]) or {}).get("hasFile"))
        elif svc == "radarr" and ((r.get("movie") or {}).get("id") or r.get("movieId")):
            _mid = (r.get("movie") or {}).get("id") or r.get("movieId")
            satisfied = bool((api(svc, "GET", "/movie/%d" % _mid) or {}).get("hasFile"))
        if satisfied:
            if go:
                rm_client = "false" if r.get("downloadId") in cleared_dl else "true"
                cleared_dl.add(r.get("downloadId"))
                api(svc, "DELETE", "/queue/%d?removeFromClient=%s&blocklist=false" % (r["id"], rm_client))
                print("  satisfied: %s — already on disk; cleared stale queue record" % title)
            else:
                print("  satisfied: %s — already on disk; would clear stale queue record" % title)
            continue
        out = r.get("outputPath")
        if not out:
            print("  %s: %s — queue record has no outputPath; cannot map" % (state, title))
            continue
        if svc.startswith("sonarr"):
            sid = r.get("seriesId") or (r.get("series") or {}).get("id")
            if not sid:
                print("  %s: %s — not linked to a series; try `arr %s import '%s' --series <query>`"
                      % (state, title, svc, out))
                continue
            item = api(svc, "GET", "/series/%d" % sid)
            payload, skipped = _plan_series_import(svc, out, sid)
        else:
            mid = (r.get("movie") or {}).get("id") or r.get("movieId")
            if not mid:
                print("  %s: %s — not linked to a movie; try `arr radarr import '%s' --movie <query>`"
                      % (state, title, out))
                continue
            item = api(svc, "GET", "/movie/%d" % mid)
            payload, skipped = _plan_movie_import(out, mid)
        if not payload:
            gone = False
            try:
                os.stat(out)
            except FileNotFoundError:
                gone = True
            except OSError:
                pass  # permission-restricted view — can't tell, stay neutral
            if gone:
                print("  %s: %s — download folder is GONE (stale queue record)" % (state, title))
                print("    -> arr %s queue-rm %s --yes" % (svc, r.get("id")))
            elif skipped and all(s.endswith("(not a video file)") for s in skipped):
                print("  %s: %s — only non-video files (junk/malware release)" % (state, title))
                print("    -> arr %s queue-rm %s --blocklist --yes   then re-grab" % (svc, r.get("id")))
            else:
                print("  %s: %s — no mappable files in %s (%d skipped)" % (state, title, out, len(skipped)))
            continue
        print("  %s -> %s: %d file(s):" % (title, item.get("title"), len(payload)))
        for p in payload:
            print("    " + p["_label"])
        if skipped:
            print("    (skipped %d unmatched)" % len(skipped))
        if go:
            # the target dir sometimes doesn't exist yet and the import API
            # throws DirectoryNotFound — pre-create it if we're allowed to
            try:
                if item.get("path"):
                    os.makedirs(item["path"], exist_ok=True)
            except OSError:
                pass
            _run_manual_import(svc, payload, "move", wait=True)


def _wait_command(svc, cmd_id, timeout=300, interval=3):
    """Poll /command/<id> until it leaves queued/started; return the final record.

    Replaces the `for i in seq…; sleep 5; raw GET /command/<id>` poll loops."""
    start = time.time()
    while True:
        rec = api(svc, "GET", "/command/%d" % int(cmd_id))
        if rec.get("status") not in ("queued", "started"):
            return rec
        if time.time() - start > timeout:
            return rec  # caller inspects .status (still queued/started == timed out)
        time.sleep(interval)


def cmd_wait(svc, args):
    """Block until an arr command (search/grab/import/refresh/rename) finishes.

    arr <svc> wait <commandId> [--timeout SECONDS]
    Command ids come from `grab`, `import`, or any `raw POST /command`. Exits
    non-zero unless the command reached 'completed'."""
    flags, rest = pop_flags(args, {"--timeout": 1})
    if not rest:
        die("wait: need a command id (e.g. printed by `arr <svc> grab/import`)")
    rec = _wait_command(svc, rest[0], timeout=int(flags.get("--timeout", 300)))
    st = rec.get("status")
    name = rec.get("commandName") or rec.get("name") or ("command %s" % rest[0])
    print("%s: %s%s" % (name, st, ("  " + rec["message"]) if rec.get("message") else ""))
    if rec.get("exception"):
        print("  exception: " + str(rec["exception"])[:300])
    if st != "completed":
        sys.exit(2)


def cmd_episodes(svc, args):
    """List a series' episodes WITH ids — what you need to grab/import by episode.

    arr <svc> episodes <id|query> [--season N] [--missing] [--monitored] [--json]
    Columns: epId  SxxEyy  abs=ABS  ON/OFF(file)  m/-(monitored)  title
    Replaces `raw GET /episode?seriesId=N | jq …` for finding missing-ep ids."""
    if not svc.startswith("sonarr"):
        die("episodes: sonarr only")
    flags, rest = pop_flags(args, {"--season": 1, "--missing": 0,
                                   "--monitored": 0, "--json": 0})
    if not rest:
        die("episodes: need a series id or query")
    sid = resolve_id(svc, rest[0])
    eps = api(svc, "GET", "/episode?seriesId=%d" % sid)
    season = int(flags["--season"]) if "--season" in flags else None
    rows = []
    for e in eps:
        if season is not None and e.get("seasonNumber") != season:
            continue
        if "--missing" in flags and e.get("hasFile"):
            continue
        if "--monitored" in flags and not e.get("monitored"):
            continue
        rows.append(e)
    rows.sort(key=lambda e: (e.get("seasonNumber") or 0, e.get("episodeNumber") or 0))
    if "--json" in flags:
        print(json.dumps([{
            "id": e["id"], "season": e.get("seasonNumber"),
            "episode": e.get("episodeNumber"), "abs": e.get("absoluteEpisodeNumber"),
            "hasFile": e.get("hasFile"), "monitored": e.get("monitored"),
            "title": e.get("title"),
        } for e in rows], indent=2))
        return
    label = " (missing)" if "--missing" in flags else ""
    print("%d episode(s)%s:" % (len(rows), label))
    for e in rows:
        print("  %-7d S%02dE%02d  abs=%-4s %s %s  %s" % (
            e["id"], e.get("seasonNumber") or 0, e.get("episodeNumber") or 0,
            e.get("absoluteEpisodeNumber") or "-",
            "ON " if e.get("hasFile") else "OFF",
            "m" if e.get("monitored") else "-",
            e.get("title", "")))


def cmd_queue_rm(svc, args):
    """Remove queue record(s) by id or title pattern — the API way, no raw curl.

    arr <svc> queue-rm <id…|pattern> [--disc|--encrypted|--quality NAME|--status STATE]
        [--blocklist] [--keep-files] [--research] [--yes]
    Selects by queue id(s), a title substring, or a selector shared with `queue`
    (--disc = raw-disc BR-DISK/ISO, --encrypted = SAB-flagged password-protected
    fakes, --quality NAME, --status failed|stuck|…).
    Default removes the download from the client (removeFromClient=true);
    --keep-files leaves it; --blocklist avoids re-grabbing the same release;
    --research re-searches the affected movies/shows for a replacement. Dry-run
    unless --yes. So a raw-disc sweep is:
        arr radarr queue-rm --disc --blocklist --research --yes"""
    flags, rest = pop_flags(args, {"--blocklist": 0, "--keep-files": 0, "--yes": 0,
                                   "--disc": 0, "--quality": 1, "--status": 1,
                                   "--title": 1, "--research": 0, "--encrypted": 0})
    idkey = "seriesId" if svc.startswith("sonarr") else "movieId"
    selectors = ("--disc" in flags or "--encrypted" in flags
                 or flags.get("--quality") or flags.get("--status")
                 or flags.get("--title"))
    numeric_only = rest and all(str(x).isdigit() for x in rest)
    if not rest and not selectors:
        die("queue-rm: need a queue id, title pattern, or selector (--disc/--quality/--status)")
    # Light path: bare ids, no selector, no re-search — no need to pull the queue.
    if numeric_only and not selectors and "--research" not in flags:
        targets = [{"id": int(x), "title": "(queue id %s)" % x, idkey: None} for x in rest]
    else:
        recs = _queue_records(svc, page_size=1000)["records"]
        sel = _queue_select(recs, flags, rest)
        if not sel:
            die("queue-rm: nothing matched")
        if "--keep-files" not in flags:
            # A season pack is one download fanned out into per-episode queue
            # records. Removing from the client takes the whole pack out on the
            # first record — deleting the rest would just 404 on stale ids.
            seen_dl, dedup = set(), []
            for r in sel:
                dl = r.get("downloadId")
                if dl and dl in seen_dl:
                    continue
                if dl:
                    seen_dl.add(dl)
                dedup.append(r)
            sel = dedup
        targets = [{"id": r["id"], "title": r.get("title", ""), idkey: r.get(idkey)} for r in sel]
    rm_client = "false" if "--keep-files" in flags else "true"
    blocklist = "true" if "--blocklist" in flags else "false"
    research = "--research" in flags
    go = "--yes" in flags
    print("%squeue-rm %d item(s) (removeFromClient=%s, blocklist=%s%s):" % (
        "" if go else "[dry-run] ", len(targets), rm_client, blocklist,
        ", re-search" if research else ""))
    for t in targets[:40]:
        print("  %-10s %s" % (t["id"], t["title"][:70]))
    if len(targets) > 40:
        print("  … and %d more" % (len(targets) - 40))
    if not go:
        print("  (pass --yes to remove)")
        return
    failed, affected = 0, set()
    for t in targets:
        for attempt in (1, 2):
            try:
                api(svc, "DELETE", "/queue/%d?removeFromClient=%s&blocklist=%s" % (
                    t["id"], rm_client, blocklist))
                print("  removed: %s" % t["title"][:70])
                if t.get(idkey):
                    affected.add(t[idkey])
                break
            except SystemExit:  # 500 database-locked under load / stale id (404)
                if attempt == 1:
                    time.sleep(5)
                    continue
                failed += 1
                print("  FAILED (kept): %s — retry once the arr settles" % t["title"][:70])
    if research and affected:
        _research_items(svc, affected)
        print("re-search queued for %d %s" % (
            len(affected), "series" if svc.startswith("sonarr") else "movie(s)"))
    if failed:
        sys.exit(1)


def _is_disc_record(r):
    """A raw-disc queue record — full Blu-ray/DVD structure (ISO/BDMV/VIDEO_TS).
    The arr classifies these as BR-DISK; they neither import nor feed the encoder,
    so a queue full of them is wasted bytes. Quality name is authoritative; the
    title tokens catch anything the classifier misses."""
    qname = ((r.get("quality") or {}).get("quality") or {}).get("name") or ""
    if qname in ("BR-DISK", "Raw-HD"):
        return True
    t = (r.get("title") or "").lower()
    return any(tok in t for tok in (
        "bdmv", ".iso", " iso", "video_ts", "complete.bluray",
        "complete.uhd.bluray", "full.bluray", "complete blu-ray"))


def _sab_flagged_labels():
    """SAB's own per-job diagnosis: {nzo_id (lowercase): [labels]}. The labels
    SAB sets are the actionable ones the arrs can't see — ENCRYPTED (password-
    protected rar = fake release, job sits Paused forever), DUPLICATE,
    ALTERNATIVE. Keyed lowercase because the arrs store downloadId in whatever
    case they please."""
    try:
        slots = (sab_api("queue", {"start": "0", "limit": "100000"}) or {}).get("queue", {}).get("slots", []) or []
    except SystemExit:
        return {}
    return {(s.get("nzo_id") or "").lower(): (s.get("labels") or []) for s in slots}


_QUEUE_STATE_PREDS = {
    "failed": lambda r: r.get("status") == "failed",
    "stuck": _is_stuck_queue_record,
    "downloading": lambda r: r.get("status") == "downloading",
    "paused": lambda r: r.get("status") == "paused",
    "importing": lambda r: r.get("trackedDownloadState") in ("importPending", "importBlocked"),
}


def _queue_select(records, flags, positional):
    """Filter queue records by any mix of selectors — the shared language of
    `queue` (view) and `queue-rm` (act). positional: numeric queue ids (exact) or
    a title substring (back-compat). flags: --title PAT, --disc, --quality NAME,
    --status STATE (failed|stuck|downloading|paused|importing, or a raw status)."""
    sel = list(records)
    ids = {int(p) for p in positional if str(p).isdigit()}
    words = [p for p in positional if not str(p).isdigit()]
    if ids:
        sel = [r for r in sel if r.get("id") in ids]
    title_pat = flags.get("--title") or (" ".join(words) if words else None)
    if title_pat:
        tl = title_pat.lower()
        sel = [r for r in sel if tl in (r.get("title", "") or "").lower()]
    if "--disc" in flags:
        sel = [r for r in sel if _is_disc_record(r)]
    if "--encrypted" in flags:
        labels = _sab_flagged_labels()
        sel = [r for r in sel
               if "ENCRYPTED" in labels.get((r.get("downloadId") or "").lower(), [])]
    if flags.get("--quality"):
        qn = flags["--quality"].lower()
        sel = [r for r in sel
               if (((r.get("quality") or {}).get("quality") or {}).get("name") or "").lower() == qn]
    if flags.get("--status"):
        st = flags["--status"].lower()
        pred = _QUEUE_STATE_PREDS.get(st, lambda r, s=st: (r.get("status") or "").lower() == s)
        sel = [r for r in sel if pred(r)]
    return sel


def _research_items(svc, item_ids):
    """Trigger a fresh search for the given movie/series ids (after removing a bad
    grab, so the arr picks an importable replacement)."""
    ids = [i for i in item_ids if i]
    if not ids:
        return
    if svc.startswith("sonarr"):
        for iid in ids:
            api(svc, "POST", "/command", {"name": "SeriesSearch", "seriesId": iid})
    else:
        api("radarr", "POST", "/command", {"name": "MoviesSearch", "movieIds": ids})


def _disk_free_bytes(path="/data"):
    try:
        s = os.statvfs(path)
        return s.f_bavail * s.f_frsize, s.f_blocks * s.f_frsize
    except OSError:
        return None, None


def cmd_queue_overview(args):
    """Top-level `arr queue` — one rollup across SAB + every arr queue: total
    jobs, TB remaining, ETA, per-category counts, a quality histogram that flags
    raw-disc releases (won't import), and free space vs. what's left to download.
    The whole-picture answer to "what's the queue looking like?"."""
    flags, _ = pop_flags(args, {"--json": 0})
    sabq = (sab_api("queue", {"start": "0", "limit": "100000"}) or {}).get("queue", {}) or {}
    slots = sabq.get("slots", []) or []
    cat_counts, state_counts = {}, {}
    for s in slots:
        cat_counts[s.get("cat") or "?"] = cat_counts.get(s.get("cat") or "?", 0) + 1
        st = s.get("status") or "?"
        state_counts[st] = state_counts.get(st, 0) + 1
    try:
        mbleft = float(sabq.get("mbleft") or 0)
        mbtot = float(sabq.get("mb") or 0)
    except ValueError:
        mbleft = mbtot = 0.0
    tb_left = round(mbleft / 1048576, 2)
    # len(slots) is authoritative (paused included) — noofslots_total under-counts.
    total_jobs = max(len(slots), int(sabq.get("noofslots_total") or 0))

    # SAB-flagged encrypted jobs: password-protected rars = fake releases. SAB
    # pauses them and they sit forever unless someone acts.
    enc_ids = {(s.get("nzo_id") or "").lower() for s in slots
               if "ENCRYPTED" in (s.get("labels") or [])}

    # quality histogram + disc/encrypted tally across the arr queues (SAB slots
    # carry no quality; the arrs know which movie/series a job belongs to)
    qual, disc_by_svc, enc_by_svc = {}, {}, {}
    for svc in ("radarr", "sonarr", "sonarr-anime"):
        try:
            recs = _queue_records(svc, page_size=2000)["records"]
        except SystemExit:
            continue
        d = e = 0
        for r in recs:
            name = ((r.get("quality") or {}).get("quality") or {}).get("name") or "?"
            qual[name] = qual.get(name, 0) + 1
            if _is_disc_record(r):
                d += 1
            if (r.get("downloadId") or "").lower() in enc_ids:
                e += 1
        if d:
            disc_by_svc[svc] = d
        if e:
            enc_by_svc[svc] = e
    disc_total = sum(disc_by_svc.values())
    enc_total = sum(enc_by_svc.values())
    free_b, _tot_b = _disk_free_bytes("/data")

    if "--json" in flags:
        print(json.dumps({
            "totalJobs": total_jobs, "speed": sabq.get("speed"),
            "tbLeft": tb_left, "eta": sabq.get("timeleft"),
            "paused": sabq.get("paused"), "byCategory": cat_counts,
            "byState": state_counts, "byQuality": qual,
            "discItems": disc_by_svc, "discTotal": disc_total,
            "encryptedItems": enc_by_svc, "encryptedTotal": enc_total,
            "freeGB": gb(free_b) if free_b is not None else None,
        }, indent=2))
        return

    print("queue: %d job(s) in SABnzbd — %s left, %s @ %s, ETA %s%s" % (
        total_jobs, ("%.2f TiB" % tb_left) if tb_left >= 1 else ("%d GiB" % round(mbleft / 1024)),
        "downloading" if not sabq.get("paused") else "PAUSED",
        (sabq.get("speed") or "?") + "B/s", sabq.get("timeleft") or "?",
        ""))
    if cat_counts:
        print("  by category: " + ", ".join("%s %d" % (k, v) for k, v in sorted(
            cat_counts.items(), key=lambda kv: -kv[1])))
    if state_counts:
        print("  by state:    " + ", ".join("%s %d" % (k, v) for k, v in sorted(
            state_counts.items(), key=lambda kv: -kv[1])))
    if qual:
        print("  by quality:  " + ", ".join("%s %d" % (k, v) for k, v in sorted(
            qual.items(), key=lambda kv: -kv[1])))
    if free_b is not None:
        head = free_b - int(mbleft * 1048576)
        warn = "  ⚠ less than what's left to download" if head < 0 else ""
        print("  /data free:  %sGB (vs %.2f TiB queued)%s" % (gb(free_b), tb_left, warn))
    if disc_total:
        where = ", ".join("%s %d" % (k, v) for k, v in disc_by_svc.items())
        print("  ⚠ %d raw-disc item(s) (%s) — these won't import or re-encode; clear with "
              "`arr <svc> queue-rm --disc --blocklist --research --yes`" % (disc_total, where))
    if enc_total:
        where = ", ".join("%s %d" % (k, v) for k, v in enc_by_svc.items())
        print("  ⚠ %d ENCRYPTED job(s) (%s) — password-protected rar = fake release, SAB has "
              "them paused forever; clear with `arr <svc> queue-rm --encrypted --blocklist "
              "--research --yes`" % (enc_total, where))


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
    flags, rest = pop_flags(args, {"--group": 1, "--indexer": 1, "--limit": 1,
                                   "--json": 0, "--audio": 1})
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
    if flags.get("--audio"):
        # title-token heuristic (Dual Audio / English Dub / Multi ...) — the
        # dub-hunting filter; releases don't carry real audio metadata
        want = _norm_lang(flags["--audio"])
        rows = [r for r in rows if _audio_match(r.get("title"), want)]
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
    # NB: require the x/h codec prefix — a bare \b26[45]\b also matches real
    # episode numbers (Sgt. Frog abs 264/265 were unmappable until 2026-07-21)
    base = re.sub(r"\b(?:[xh]\.?26[45]|HEVC|AAC|MP3|XviD|FLAC|BD|WEB)\b", " ", base, flags=re.I)
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


VIDEO_EXT = (".mkv", ".mp4", ".avi", ".m4v", ".ts", ".webm", ".wmv", ".mpg", ".mpeg")


def _plan_series_import(svc, folder, sid, match="", mapmode="auto", season=None):
    """Map importable files in <folder> onto a series' episodes. Returns
    (payload, skipped) — payload rows carry a human '_label' key.
    Non-video files (.exe/.scr malware droppers in fake releases) are never
    mapped, even if the arr's manualimport endpoint lists them."""
    files = api(svc, "GET",
                "/manualimport?folder=%s&filterExistingFiles=false" % urllib.parse.quote(folder),
                timeout=SEARCH_TIMEOUT)
    eps = api(svc, "GET", "/episode?seriesId=%d" % sid)
    by_se = {(e["seasonNumber"], e["episodeNumber"]): e for e in eps}
    by_abs = {e["absoluteEpisodeNumber"]: e for e in eps if e.get("absoluteEpisodeNumber")}
    payload, skipped = [], []
    for f in files or []:
        name = (f.get("relativePath") or f.get("path") or "").split("/")[-1]
        if match and match not in name.lower():
            continue
        if not name.lower().endswith(VIDEO_EXT):
            skipped.append(name + " (not a video file)")
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
    return payload, skipped


def _plan_movie_import(folder, mid, match=""):
    files = api("radarr", "GET",
                "/manualimport?folder=%s&filterExistingFiles=false" % urllib.parse.quote(folder),
                timeout=SEARCH_TIMEOUT)
    payload, skipped = [], []
    for f in files or []:
        name = (f.get("relativePath") or f.get("path") or "").split("/")[-1]
        if match and match not in name.lower():
            continue
        if not name.lower().endswith(VIDEO_EXT):
            skipped.append(name + " (not a video file)")
            continue
        if not f.get("quality"):
            skipped.append(name)
            continue
        payload.append({
            "path": f["path"], "movieId": mid, "quality": f["quality"],
            "languages": f.get("languages") or [{"id": 1, "name": "English"}],
            "releaseGroup": f.get("releaseGroup", ""),
            "_label": name,
        })
    return payload, skipped


def _run_manual_import(svc, payload, impmode, batch=10, wait=False, timeout=300):
    """POST ManualImport in chunks of <=batch files. Big single imports were
    timing out (9+ episode batches hit the 120-300s wall); chunking + a wait
    between chunks keeps each command small and reports per-chunk results."""
    clean = [{k: v for k, v in p.items() if k != "_label"} for p in payload]
    chunks = [clean[i:i + batch] for i in range(0, len(clean), batch)] if batch > 0 else [clean]
    for i, chunk in enumerate(chunks):
        r = api(svc, "POST", "/command",
                {"name": "ManualImport", "importMode": impmode, "files": chunk})
        n = "%d/%d " % (i + 1, len(chunks)) if len(chunks) > 1 else ""
        print("ManualImport %squeued: id=%s status=%s (%d files)" % (
            n, r.get("id"), r.get("status"), len(chunk)))
        if wait or len(chunks) > 1:
            rec = _wait_command(svc, r["id"], timeout=timeout)
            print("  -> %s %s" % (rec.get("status"), rec.get("message") or ""))


def cmd_import(svc, args):
    """Force-import downloaded files into explicit episodes, bypassing name match.

    arr sonarr import <folder> --series <id|query> [--match SUBSTR] [--season N]
        [--map auto|abs|se] [--mode copy|move] [--batch N] [--dry-run]
    Maps each file to an episode by parsing its number (absolute or SxxExx).
    Imports are chunked --batch files at a time (default 10) to dodge timeouts.

    SAFETY: a folder may hold files from many shows. --match restricts to files
    whose name contains SUBSTR (case-insensitive). Without it, EVERY file under
    the folder is mapped onto this series — only safe for a single-show folder."""
    if svc == "radarr":
        return _cmd_import_radarr(args)
    if not svc.startswith("sonarr"):
        die("import: sonarr or radarr only")
    flags, rest = pop_flags(args, {"--series": 1, "--match": 1, "--season": 1,
                                   "--map": 1, "--mode": 1, "--dry-run": 0,
                                   "--wait": 0, "--timeout": 1, "--batch": 1})
    if not rest or "--series" not in flags:
        die("import: usage: arr sonarr import <folder> --series <id|query> "
            "[--match SUBSTR] [--season N] [--map auto|abs|se] [--mode copy|move] [--dry-run]")
    sid = resolve_id(svc, flags["--series"])
    season = int(flags["--season"]) if "--season" in flags else None
    impmode = flags.get("--mode", "copy")
    payload, skipped = _plan_series_import(
        svc, rest[0], sid, match=(flags.get("--match") or "").lower(),
        mapmode=flags.get("--map", "auto"), season=season)
    for p in payload:
        print("  " + p["_label"])
    if skipped:
        print("  (skipped %d unmatched: %s%s)" % (
            len(skipped), ", ".join(skipped[:3]), " ..." if len(skipped) > 3 else ""))
    if not payload:
        print("nothing to import")
        return
    if "--dry-run" in flags:
        print("DRY: would ManualImport %d file(s) [mode=%s]" % (len(payload), impmode))
        return
    _run_manual_import(svc, payload, impmode, batch=int(flags.get("--batch", 10)),
                       wait="--wait" in flags, timeout=int(flags.get("--timeout", 300)))


def _cmd_import_radarr(args):
    """Force-import downloaded file(s) into a movie, bypassing name matching.

    arr radarr import <folder> --movie <id|query> [--match SUBSTR]
        [--mode copy|move] [--dry-run] [--wait]"""
    flags, rest = pop_flags(args, {"--movie": 1, "--match": 1, "--mode": 1,
                                   "--dry-run": 0, "--wait": 0, "--timeout": 1, "--batch": 1})
    if not rest or "--movie" not in flags:
        die("import: usage: arr radarr import <folder> --movie <id|query> "
            "[--match SUBSTR] [--mode copy|move] [--dry-run] [--wait]")
    mid = resolve_id("radarr", flags["--movie"])
    impmode = flags.get("--mode", "copy")
    payload, skipped = _plan_movie_import(rest[0], mid,
                                          match=(flags.get("--match") or "").lower())
    for p in payload:
        print("  " + p["_label"])
    if skipped:
        print("  (skipped %d without parsed quality: %s%s)" % (
            len(skipped), ", ".join(skipped[:3]), " ..." if len(skipped) > 3 else ""))
    if not payload:
        print("nothing to import")
        return
    if "--dry-run" in flags:
        print("DRY: would ManualImport %d file(s) [mode=%s]" % (len(payload), impmode))
        return
    _run_manual_import("radarr", payload, impmode, batch=int(flags.get("--batch", 10)),
                       wait="--wait" in flags, timeout=int(flags.get("--timeout", 300)))


# --- coverage (the requested-show policy as a command) -----------------------
def _series_coverage(svc, sid, series=None):
    """Exact per-season coverage from the episode list (aired = airdate past).
    Returns (series, rows); each row: {season, monitored, aired, files, future,
    missing:[monitored aired eps w/o file], unmon_missing: count}."""
    s = series or api(svc, "GET", "/series/%d" % sid)
    eps = api(svc, "GET", "/episode?seriesId=%d" % sid)
    now = time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime())
    seasons = {}
    for e in eps:
        sn = e.get("seasonNumber", 0)
        d = seasons.setdefault(sn, {"season": sn, "aired": 0, "files": 0,
                                    "future": 0, "missing": [], "unmon_missing": 0})
        aired = bool(e.get("airDateUtc")) and e["airDateUtc"][:19] <= now
        if e.get("hasFile"):
            d["files"] += 1
        if aired:
            d["aired"] += 1
            if not e.get("hasFile"):
                if e.get("monitored"):
                    d["missing"].append(e)
                else:
                    d["unmon_missing"] += 1
        elif not e.get("hasFile"):
            d["future"] += 1
    mon = {se["seasonNumber"]: bool(se.get("monitored")) for se in (s.get("seasons") or [])}
    rows = []
    for sn in sorted(seasons):
        d = seasons[sn]
        d["monitored"] = mon.get(sn, False)
        rows.append(d)
    return s, rows


def _coverage_print(rows):
    """Print per-season lines; return (fixable, askable) season rows."""
    fixable, askable = [], []
    for d in rows:
        sn = d["season"]
        if sn == 0:
            if d["aired"] or d["files"]:
                print("  S0   specials: %d/%d aired on disk%s" % (
                    d["files"], d["aired"], "" if d["monitored"] else " (unmonitored)"))
            continue
        miss = len(d["missing"])
        if d["monitored"] and miss:
            state = ("⚠ PARTIAL — %d aired ep(s) missing" % miss) if d["files"] else "⚠ MISSING (0 on disk)"
            fixable.append(d)
        elif not d["monitored"] and d["files"] < d["aired"]:
            state = "unmonitored, not fetched"
            askable.append(d)
        else:
            state = "complete"
        extra = " (+%d unaired)" % d["future"] if d["future"] else ""
        print("  S%-2d %s %d/%d aired on disk%s — %s" % (
            sn, "mon" if d["monitored"] else "off", d["files"], d["aired"], extra, state))
    return fixable, askable


def _coverage_fix(svc, sid, fixable, dry=False):
    """Trigger searches for the gaps: whole-season -> SeasonSearch, partial ->
    one EpisodeSearch with the missing episode ids."""
    for d in fixable:
        if d["files"] == 0:
            body = {"name": "SeasonSearch", "seriesId": sid, "seasonNumber": d["season"]}
            desc = "SeasonSearch S%d" % d["season"]
        else:
            ids = [e["id"] for e in d["missing"]]
            body = {"name": "EpisodeSearch", "episodeIds": ids}
            desc = "EpisodeSearch S%d (%d eps)" % (d["season"], len(ids))
        if dry:
            print("  DRY: " + desc)
            continue
        r = api(svc, "POST", "/command", body)
        print("  queued %s (command id %s)" % (desc, r["id"]))


def _coverage_all(svc, flags):
    fix = "--fix" in flags
    limit = int(flags.get("--limit", "10"))
    gaps = []
    for s in api(svc, "GET", "/series"):
        if not s.get("monitored"):
            continue
        st = s.get("statistics") or {}
        if st.get("episodeFileCount", 0) < st.get("episodeCount", 0):
            gaps.append(s)
    if not gaps:
        if "--quiet" not in flags:
            print("coverage: every monitored %s series has its monitored aired episodes on disk" % svc)
        return
    print("coverage: %d monitored series with gaps:" % len(gaps))
    fixed = 0
    for s in sorted(gaps, key=lambda x: x["title"]):
        _, rows = _series_coverage(svc, s["id"], series=s)
        fixable = [d for d in rows if d["season"] != 0 and d["monitored"] and d["missing"]]
        if not fixable:
            continue
        print("  [%d] %s — %s" % (s["id"], s["title"], ", ".join(
            "S%d %d/%d" % (d["season"], d["files"], d["aired"]) for d in fixable)))
        if fix:
            if fixed < limit:
                _coverage_fix(svc, s["id"], fixable, dry="--dry-run" in flags)
                fixed += 1
            else:
                print("    (fix skipped — --limit %d reached this run)" % limit)
    if fix:
        print("(searches triggered for %d series%s)" % (
            fixed, "; raise --limit to fix more per run" if fixed >= limit else ""))


def _season_track_coverage(svc, sid, lang="eng"):
    """ffprobe every on-disk file of a series; per-season audio/sub coverage
    for <lang> (embedded streams + sidecar subs)."""
    s = api(svc, "GET", "/series/%d" % sid)
    seasons = {}
    for f in api(svc, "GET", "/episodefile?seriesId=%d" % sid) or []:
        path = f.get("path") or os.path.join(s.get("path", ""), f.get("relativePath", ""))
        label = f.get("relativePath") or path
        sn = f.get("seasonNumber", 0)
        d = seasons.setdefault(sn, {"files": 0, "unreadable": 0,
                                    "missing_audio": [], "missing_subs": []})
        audio, subs, ok = _file_tracks(path)
        if not ok:
            d["unreadable"] += 1
            continue
        d["files"] += 1
        if not any(t["lang"] == lang for t in audio):
            d["missing_audio"].append(label)
        if not any(t["lang"] == lang for t in subs):
            d["missing_subs"].append(label)
    return seasons


def _bazarr_search_series(svc, sid, title):
    """Kick Bazarr's search-missing for a series (main sonarr only)."""
    if svc != "sonarr":
        print("  => '%s' lives on %s — Bazarr only watches the MAIN sonarr, so no"
              " subtitle search was triggered (grab a subbed/dual release instead)" % (title, svc))
        return
    bazarr_api("PATCH", "/series", {"seriesid": sid, "action": "search-missing"})
    print("  => Bazarr search-missing triggered for '%s' — subs download in the"
          " background as providers allow; check `arr bazarr wanted '%s'` later" % (title, title))


def cmd_coverage(svc, args):
    """Per-season coverage vs AIRED episodes + gap repair (+ dub/sub coverage).

    arr sonarr coverage <id|query> [--fix] [--tracks] [--lang LANG] [--fix-subs] [--dry-run]
    arr sonarr coverage --all [--fix] [--quiet] [--limit N]
    Policy: partial/missing MONITORED seasons are fixable (--fix searches them);
    unmonitored seasons are only reported — ask the requester before grabbing.
    --tracks adds per-season audio/subtitle coverage for --lang (default eng),
    via ffprobe — the "do all episodes have an English dub/subs?" view.
    --fix-subs additionally triggers a Bazarr search for missing subs (main
    sonarr + radarr only; the anime instance isn't Bazarr-covered).
    --all sweeps every monitored series (cron-friendly; --quiet = silent when
    clean; --fix caps at --limit series per run to be kind to indexers)."""
    if not svc.startswith("sonarr"):
        die("coverage: sonarr/sonarr-anime only")
    flags, rest = pop_flags(args, {"--fix": 0, "--all": 0, "--quiet": 0,
                                   "--limit": 1, "--dry-run": 0,
                                   "--tracks": 0, "--lang": 1, "--fix-subs": 0})
    if "--all" in flags:
        return _coverage_all(svc, flags)
    if not rest:
        die("coverage: need <id|query> or --all")
    sid = resolve_id(svc, rest[0])
    s, rows = _series_coverage(svc, sid)
    print("%s (%s)" % (s["title"], s.get("year")))
    fixable, askable = _coverage_print(rows)
    if not fixable and not askable:
        print("  => complete: all monitored aired episodes on disk")
    if askable:
        print("  => unmonitored gaps: ask before grabbing; enable with `arr %s monitor %d s%s`" % (
            svc, sid, ",s".join(str(d["season"]) for d in askable)))
    if fixable:
        if "--fix" in flags:
            _coverage_fix(svc, sid, fixable, dry="--dry-run" in flags)
        else:
            print("  => fixable gaps in monitored seasons: rerun with --fix to trigger searches")
    _audit_warn(svc, sid, item=s)
    if "--tracks" not in flags and "--fix-subs" not in flags:
        return
    lang = _norm_lang(flags.get("--lang", "eng"))
    tc = _season_track_coverage(svc, sid, lang)
    print("tracks (%s audio/subs; embedded + sidecar):" % lang)
    audio_gaps = sub_gaps = False
    for sn in sorted(tc):
        d = tc[sn]
        if not d["files"] and not d["unreadable"]:
            continue
        ma, ms = len(d["missing_audio"]), len(d["missing_subs"])
        audio_gaps = audio_gaps or bool(ma)
        sub_gaps = sub_gaps or bool(ms)
        print("  S%-2d audio %d/%d   subs %d/%d%s" % (
            sn, d["files"] - ma, d["files"], d["files"] - ms, d["files"],
            ("   (%d unreadable)" % d["unreadable"]) if d["unreadable"] else ""))
        for lbl in d["missing_audio"][:4]:
            print("       no %s audio: %s" % (lang, lbl))
        if ma > 4:
            print("       ... +%d more without %s audio" % (ma - 4, lang))
        for lbl in d["missing_subs"][:4]:
            print("       no %s subs:  %s" % (lang, lbl))
        if ms > 4:
            print("       ... +%d more without %s subs" % (ms - 4, lang))
    if sub_gaps:
        if "--fix-subs" in flags:
            _bazarr_search_series(svc, sid, s["title"])
        elif svc == "sonarr":
            print("  => missing subs: `--fix-subs` (or `arr bazarr search --series %d`) asks Bazarr to fetch them" % sid)
        else:
            print("  => missing subs: %s is NOT Bazarr-covered (anime instance) — prefer a subbed/dual release" % svc)
    if audio_gaps:
        print("  => missing %s audio can't be 'fetched' — needs a dub release:"
              " `arr %s releases %d --season N --audio %s`" % (lang, svc, sid, lang))


# --- add / replace ------------------------------------------------------------
def _lookup_pick(svc, term, tvdb=None, tmdb=None, year=None):
    """Lookup <term> and insist on ONE unambiguous winner. Multiple plausible
    matches -> list candidates and refuse (the wrong-Gloria/wrong-Groove guard)."""
    coll = "series" if svc.startswith("sonarr") else "movie"
    res = api(svc, "GET", "/%s/lookup?term=%s" % (coll, urllib.parse.quote(term))) or []
    cands = res
    if tvdb:
        cands = [c for c in cands if c.get("tvdbId") == tvdb]
    if tmdb:
        cands = [c for c in cands if c.get("tmdbId") == tmdb]
    if year:
        cands = [c for c in cands if c.get("year") == year]
    if not cands:
        die('no lookup results for "%s"%s' % (
            term, " with those filters" if (tvdb or tmdb or year) else ""))
    if len(cands) == 1:
        return cands[0]
    tl = term.lower()
    exact = [c for c in cands
             if (c.get("title") or "").lower() == tl or (c.get("originalTitle") or "").lower() == tl]
    if len(exact) == 1:
        return exact[0]
    print('"%s" is ambiguous — candidates (narrow with --year / --%s):' % (
        term, "tvdb" if coll == "series" else "tmdb"), file=sys.stderr)
    for c in cands[:8]:
        idlab = ("tvdb=%s" % c.get("tvdbId")) if coll == "series" else ("tmdb=%s" % c.get("tmdbId"))
        orig = ""
        if c.get("originalTitle") and c["originalTitle"] != c.get("title"):
            orig = "  orig=%s" % c["originalTitle"]
        print("  %s (%s)  %s%s" % (c.get("title"), c.get("year"), idlab, orig), file=sys.stderr)
    die("refusing to guess between %d matches" % len(cands))


def _existing_by_ids(svc, tvdb=None, tmdb=None, title=None):
    coll = "series" if svc.startswith("sonarr") else "movie"
    items = api(svc, "GET", "/" + coll) or []
    for it in items:
        if tvdb and it.get("tvdbId") == tvdb:
            return it
        if tmdb and it.get("tmdbId") == tmdb:
            return it
    if title:
        tl = title.lower()
        exact = [it for it in items if (it.get("title") or "").lower() == tl]
        if len(exact) == 1:
            return exact[0]
    return None


def _profile_and_root(svc, quality=None, root=None):
    """Default to the quality profile most of the existing library uses."""
    coll = "series" if svc.startswith("sonarr") else "movie"
    profiles = api(svc, "GET", "/qualityprofile") or []
    if not profiles:
        die("add: no quality profiles configured")
    if quality:
        hits = [p for p in profiles if quality.lower() in p["name"].lower()]
        if len(hits) != 1:
            die("--quality '%s' matches %d profiles (have: %s)" % (
                quality, len(hits), ", ".join(p["name"] for p in profiles)))
        pid = hits[0]["id"]
    else:
        counts = {}
        for it in api(svc, "GET", "/" + coll) or []:
            counts[it.get("qualityProfileId")] = counts.get(it.get("qualityProfileId"), 0) + 1
        valid = {p["id"] for p in profiles}
        counts = {k: v for k, v in counts.items() if k in valid}
        pid = max(counts, key=counts.get) if counts else profiles[0]["id"]
    pname = next(p["name"] for p in profiles if p["id"] == pid)
    roots = api(svc, "GET", "/rootfolder") or []
    rpath = root or (roots[0]["path"] if roots else None)
    if not rpath:
        die("add: no root folder configured")
    return pid, pname, rpath


def _do_add(svc, pick, seasons_spec="all", quality=None, root=None, search=True):
    """POST the new series/movie; returns the created item."""
    is_series = svc.startswith("sonarr")
    pid, pname, rpath = _profile_and_root(svc, quality, root)
    body = {
        "title": pick["title"], "qualityProfileId": pid,
        "titleSlug": pick.get("titleSlug"), "images": pick.get("images") or [],
        "year": pick.get("year"), "rootFolderPath": rpath, "monitored": True,
    }
    if is_series:
        body["tvdbId"] = pick["tvdbId"]
        body["seasonFolder"] = True
        body["seriesType"] = "anime" if svc == "sonarr-anime" else (pick.get("seriesType") or "standard")
        seasons = pick.get("seasons") or []
        if seasons_spec == "all":
            for se in seasons:
                se["monitored"] = se["seasonNumber"] > 0
        elif seasons_spec == "none":
            for se in seasons:
                se["monitored"] = False
        else:
            want = _parse_seasons(re.sub(r"[sS]", "", seasons_spec))
            for se in seasons:
                se["monitored"] = se["seasonNumber"] in want
        body["seasons"] = seasons
        body["addOptions"] = {"searchForMissingEpisodes": bool(search)}
    else:
        body["tmdbId"] = pick["tmdbId"]
        body["minimumAvailability"] = "released"
        body["addOptions"] = {"searchForMovie": bool(search)}
    created = api(svc, "POST", "/series" if is_series else "/movie", body)
    print("added [%d] %s (%s) — profile=%s root=%s" % (
        created["id"], created["title"], created.get("year"), pname, rpath))
    if is_series:
        mon = [str(se["seasonNumber"]) for se in created.get("seasons", []) if se.get("monitored")]
        print("  monitored seasons: %s" % (",".join("S" + n for n in mon) or "none"))
    print("  search: %s" % ("triggered (the arr picks per its quality profile)" if search
                            else "skipped (--no-search)"))
    return created


def cmd_add(svc, args):
    """Add a series/movie by title — strict disambiguation, sane defaults.

    arr sonarr add <title> [--tvdb ID] [--year Y] [--seasons all|s1,s2|none]
    arr radarr  add <title> [--tmdb ID] [--year Y]
    common: [--quality NAME] [--root PATH] [--no-search]
            [--requester <discordId>] [--dry-run]

    Refuses ambiguous lookups (lists candidates; narrow with --year/--tvdb/
    --tmdb). If the item is ALREADY in the library it prints per-season
    coverage instead of adding — so "get X" requests surface partially-
    downloaded seasons automatically. --requester stamps the tag the
    download-notifier uses to DM that person a progress bar."""
    if not (svc.startswith("sonarr") or svc == "radarr"):
        die("add: sonarr/sonarr-anime/radarr only")
    flags, rest = pop_flags(args, {"--tvdb": 1, "--tmdb": 1, "--year": 1,
                                   "--seasons": 1, "--quality": 1, "--root": 1,
                                   "--no-search": 0, "--requester": 1, "--dry-run": 0,
                                   "--require-subs": 1, "--require-audio": 1,
                                   "--no-wait": 0})
    if not rest:
        die("add: need a title")
    term = " ".join(rest)
    is_series = svc.startswith("sonarr")
    pick = _lookup_pick(svc,
                        term,
                        tvdb=int(flags["--tvdb"]) if flags.get("--tvdb") else None,
                        tmdb=int(flags["--tmdb"]) if flags.get("--tmdb") else None,
                        year=int(flags["--year"]) if flags.get("--year") else None)
    existing = _existing_by_ids(svc,
                                tvdb=pick.get("tvdbId") if is_series else None,
                                tmdb=pick.get("tmdbId") if not is_series else None,
                                title=pick.get("title"))
    if existing:
        print("already in %s: [%d] %s (%s)" % (svc, existing["id"], existing["title"], existing.get("year")))
        if flags.get("--requester"):
            _tag_requester(svc, str(existing["id"]), flags["--requester"])
            print("  tagged requester:%s" % flags["--requester"])
        for lab in _require_labels(flags):
            _stamp_label(svc, existing["id"], lab)
            print("  tagged %s (notifier verifies at ready-time)" % lab)
        if is_series:
            _, rows = _series_coverage(svc, existing["id"])
            fixable, askable = _coverage_print(rows)
            if fixable:
                print("  => partial monitored season(s): `arr %s coverage %d --fix` to repair"
                      % (svc, existing["id"]))
            if askable:
                print("  => whole season(s) unmonitored: ask the requester, then "
                      "`arr %s monitor %d s...` + `coverage --fix`" % (svc, existing["id"]))
            if not fixable and not askable:
                print("  => complete: all monitored aired episodes on disk")
            _audit_warn(svc, existing["id"], item=existing)
            print("  => dub/sub state NOT checked here — run `arr %s coverage %d"
                  " --tracks` and report eng audio/sub gaps too (do this"
                  " unprompted for anime)" % (svc, existing["id"]))
        else:
            print("  " + ("ON DISK (%sGB)" % gb(existing.get("sizeOnDisk"))
                          if existing.get("hasFile") else
                          "no file yet — `arr radarr grab %d` re-searches" % existing["id"]))
            _audit_warn(svc, existing["id"], item=existing)
        return
    # a series might live in the OTHER sonarr instance (anime vs normal)
    if is_series and pick.get("tvdbId"):
        other = "sonarr-anime" if svc == "sonarr" else "sonarr"
        try:
            twin = _existing_by_ids(other, tvdb=pick["tvdbId"])
        except SystemExit:
            twin = None
        if twin:
            print("already in %s: [%d] %s — use that instance (`arr %s ...`)" % (
                other, twin["id"], twin["title"], other))
            return
    if "--dry-run" in flags:
        ids = ("tvdb=%s" % pick.get("tvdbId")) if is_series else ("tmdb=%s" % pick.get("tmdbId"))
        print("DRY: would add %s (%s) %s to %s [seasons=%s search=%s]" % (
            pick.get("title"), pick.get("year"), ids, svc,
            flags.get("--seasons", "all") if is_series else "-",
            "--no-search" not in flags))
        return
    created = _do_add(svc, pick, seasons_spec=flags.get("--seasons", "all"),
                      quality=flags.get("--quality"), root=flags.get("--root"),
                      search="--no-search" not in flags)
    if flags.get("--requester"):
        _tag_requester(svc, str(created["id"]), flags["--requester"])
        print("  tagged requester:%s (download-notifier will DM them)" % flags["--requester"])
    for lab in _require_labels(flags):
        _stamp_label(svc, created["id"], lab)
        print("  tagged %s (the ✅ ready DM will verify it via ffprobe)" % lab)
    # By default, hang around briefly and report what the search actually
    # grabbed — so a single `add` answers "is it downloading?" without
    # status/history/queue follow-up calls.
    if "--no-search" not in flags and "--no-wait" not in flags:
        _report_first_grab(svc, created["id"], is_series)


def cmd_replace(svc, args):
    """Swap a wrong movie for the right one in one step.

    arr radarr replace <old id|query> <correct title...> [--tmdb ID] [--year Y] [--yes]
    Deletes the old movie + file, adds the correct match (same strict
    disambiguation as `add`), carries any requester-* tag over, searches.
    Dry-run unless --yes. (The Les Visiteurs German→French class of fix.)"""
    if svc != "radarr":
        die("replace: radarr only")
    flags, rest = pop_flags(args, {"--tmdb": 1, "--year": 1, "--yes": 0})
    if len(rest) < 2:
        die("replace: usage: arr radarr replace <old id|query> <correct title...> "
            "[--tmdb ID] [--year Y] [--yes]")
    old = api("radarr", "GET", "/movie/%d" % resolve_id("radarr", rest[0]))
    pick = _lookup_pick("radarr", " ".join(rest[1:]),
                        tmdb=int(flags["--tmdb"]) if flags.get("--tmdb") else None,
                        year=int(flags["--year"]) if flags.get("--year") else None)
    if pick.get("tmdbId") == old.get("tmdbId"):
        die("replace: that's the same movie (tmdb %s)" % old.get("tmdbId"))
    go = "--yes" in flags
    print("%sREPLACE [%d] %s (%s, %sGB on disk)" % (
        "" if go else "[dry-run] ", old["id"], old["title"], old.get("year"), gb(old.get("sizeOnDisk"))))
    print("    with  %s (%s) tmdb=%s" % (pick.get("title"), pick.get("year"), pick.get("tmdbId")))
    if not go:
        print("  (pass --yes to delete the old movie + file and add the new one)")
        return
    labels = []
    if old.get("tags"):
        all_tags = {t["id"]: t.get("label", "") for t in api("radarr", "GET", "/tag") or []}
        labels = [all_tags[t] for t in old["tags"] if all_tags.get(t, "").startswith("requester-")]
    api("radarr", "DELETE", "/movie/%d?deleteFiles=true" % old["id"])
    print("deleted old movie + file")
    created = _do_add("radarr", pick)
    for lbl in labels:
        tid = _ensure_tag("radarr", lbl)
        api("radarr", "PUT", "/movie/editor",
            {"movieIds": [created["id"]], "tags": [tid], "applyTags": "add"})
        print("  carried tag %s over" % lbl)


# --- media track inspection (ffprobe) -----------------------------------------
LANG_ALIASES = {"en": "eng", "ja": "jpn", "jp": "jpn", "fr": "fre", "fra": "fre",
                "de": "ger", "deu": "ger", "ko": "kor", "zh": "chi", "zho": "chi",
                "es": "spa", "it": "ita", "pt": "por", "ru": "rus"}


def _norm_lang(l):
    l = (l or "und").lower()
    return LANG_ALIASES.get(l, l)


def _ffprobe_streams(path):
    exe = shutil.which("ffprobe")
    if not exe:
        die("ffprobe not on PATH — add ffmpeg to environment.systemPackages and rebuild")
    try:
        p = subprocess.run([exe, "-v", "error", "-print_format", "json", "-show_streams", path],
                           capture_output=True, text=True, timeout=60)
    except (OSError, subprocess.TimeoutExpired):
        return None
    if p.returncode != 0:
        return None
    try:
        return json.loads(p.stdout).get("streams") or []
    except ValueError:
        return None


def _sidecar_subs(path):
    """External .srt/.ass/... next to the video count as subtitles too."""
    base = os.path.splitext(os.path.basename(path))[0]
    out = []
    try:
        names = os.listdir(os.path.dirname(path))
    except OSError:
        return out
    for f in names:
        if not f.startswith(base) or f == os.path.basename(path):
            continue
        if not f.lower().endswith((".srt", ".ass", ".ssa", ".sub", ".vtt")):
            continue
        parts = f[len(base):].strip(".").lower().split(".")
        langs = [t for t in parts[:-1] if t.isalpha() and len(t) in (2, 3)
                 and t not in ("sdh", "cc", "hi")]
        out.append({"lang": _norm_lang(langs[0] if langs else None),
                    "codec": "sidecar-" + parts[-1], "default": False,
                    "forced": "forced" in parts})
    return out


def _file_tracks(path):
    """(audio_tracks, sub_tracks, readable) for one video file."""
    streams = _ffprobe_streams(path)
    audio, subs = [], []
    for st in streams or []:
        tags = st.get("tags") or {}
        disp = st.get("disposition") or {}
        ent = {"lang": _norm_lang(tags.get("language")), "codec": st.get("codec_name"),
               "default": bool(disp.get("default")), "forced": bool(disp.get("forced"))}
        if st.get("codec_type") == "audio":
            audio.append(ent)
        elif st.get("codec_type") == "subtitle":
            subs.append(ent)
    subs += _sidecar_subs(path)
    return audio, subs, streams is not None


def _item_files(svc, iid, season=None):
    """[(label, abspath)] for an item's on-disk files."""
    out = []
    if svc.startswith("sonarr"):
        s = api(svc, "GET", "/series/%d" % iid)
        for f in api(svc, "GET", "/episodefile?seriesId=%d" % iid) or []:
            if season is not None and f.get("seasonNumber") != season:
                continue
            path = f.get("path") or os.path.join(s.get("path", ""), f.get("relativePath", ""))
            out.append((f.get("relativePath") or path, path))
    else:
        for f in api("radarr", "GET", "/moviefile?movieId=%d" % iid) or []:
            out.append((f.get("relativePath") or f.get("path"), f.get("path")))
    out.sort()
    return out


def _fmt_tracks(tr):
    return ",".join("%s%s(%s)" % (t["lang"], "*" if t["default"] else "", t["codec"])
                    for t in tr) or "-"


def cmd_tracks(svc, args):
    """Audio + subtitle tracks per file — embedded (ffprobe) and sidecar subs.

    arr <svc> tracks <id|query> [--season N] [--missing-audio LANG]
        [--missing-subs LANG] [--json]
    LANG is ISO 639-2 (eng/jpn/fre...; 2-letter forms normalized). Answers
    "which NANA eps lack an English dub" / "which files have no English subs"
    in one command instead of a nix-shell ffprobe loop. '*' marks the default
    track (a non-eng default is the Loki-S1E6 class of bug)."""
    if not (svc.startswith("sonarr") or svc == "radarr"):
        die("tracks: sonarr/sonarr-anime/radarr only")
    flags, rest = pop_flags(args, {"--season": 1, "--missing-audio": 1,
                                   "--missing-subs": 1, "--json": 0})
    if not rest:
        die("tracks: need an id or query")
    season = int(flags["--season"]) if "--season" in flags else None
    iid = resolve_id(svc, rest[0])
    files = _item_files(svc, iid, season)
    if not files:
        die("tracks: no files on disk for that item")
    want_a = _norm_lang(flags["--missing-audio"]) if flags.get("--missing-audio") else None
    want_s = _norm_lang(flags["--missing-subs"]) if flags.get("--missing-subs") else None
    rows, flagged, unreadable = [], 0, 0
    for label, path in files:
        audio, subs, ok = _file_tracks(path)
        if not ok:
            unreadable += 1
            print("  ?? unreadable: %s" % label)
            continue
        rows.append({"file": label, "audio": audio, "subs": subs})
        miss_a = want_a and not any(t["lang"] == want_a for t in audio)
        miss_s = want_s and not any(t["lang"] == want_s for t in subs)
        if (want_a or want_s) and not (miss_a or miss_s):
            continue
        flagged += 1
        if "--json" not in flags:
            print("  %s" % label)
            print("      audio: %s   subs: %s" % (_fmt_tracks(audio), _fmt_tracks(subs)))
    if "--json" in flags:
        print(json.dumps(rows, indent=2))
        return
    if want_a or want_s:
        what = " / ".join(x for x in
                          ("audio:%s" % want_a if want_a else None,
                           "subs:%s" % want_s if want_s else None) if x)
        print("%d/%d file(s) missing %s%s" % (
            flagged, len(rows), what, "; %d unreadable" % unreadable if unreadable else ""))
        if flagged and want_s:
            if svc == "sonarr":
                print("  -> fetch subs via Bazarr: arr bazarr search --series %d" % iid)
            elif svc == "radarr":
                print("  -> fetch subs via Bazarr: arr bazarr search --movie %d" % iid)
            else:
                print("  -> %s is not Bazarr-covered (anime instance); prefer a subbed/dual release" % svc)
    else:
        print("(%d file(s)%s)" % (len(rows), "; %d unreadable" % unreadable if unreadable else ""))


def _verify_lang(files, want_a=None, want_s=None):
    n, miss_a, miss_s = 0, [], []
    for label, path in files:
        audio, subs, ok = _file_tracks(path)
        if not ok:
            continue
        n += 1
        if want_a and not any(t["lang"] == want_a for t in audio):
            miss_a.append(label)
        if want_s and not any(t["lang"] == want_s for t in subs):
            miss_s.append(label)
    return n, miss_a, miss_s


# --- watch (one-shot cron watchdog) --------------------------------------------
def _jf_find_item(item, is_series):
    """Find the arr item in Jellyfin by provider id, client-side (10.11's
    AnyProviderIdEquals filter is broken — same approach as download-notifier)."""
    r = jf_api("/Items", {"IncludeItemTypes": "Series" if is_series else "Movie",
                          "Recursive": "true", "Fields": "ProviderIds",
                          "Limit": "100000"}, soft=True) or {}
    want = {}
    if item.get("tvdbId"):
        want["Tvdb"] = str(item["tvdbId"])
    if item.get("tmdbId"):
        want["Tmdb"] = str(item["tmdbId"])
    if item.get("imdbId"):
        want["Imdb"] = str(item["imdbId"])
    for it in r.get("Items") or []:
        pids = it.get("ProviderIds") or {}
        for k, v in want.items():
            if str(pids.get(k) or "") == v:
                return it
    return None


def _latest_history_date(svc, iid):
    if svc.startswith("sonarr"):
        rows = api(svc, "GET", "/history/series?seriesId=%d" % iid) or []
    else:
        data = api(svc, "GET", "/history?pageSize=100&sortKey=date&sortDirection=descending")
        rows = [r for r in data["records"] if r.get("movieId") == iid]
    dates = sorted((r.get("date") or "" for r in rows), reverse=True)
    return dates[0] if dates and dates[0] else None


WATCH_STATE = os.environ.get(
    "ARR_WATCH_STATE", os.path.expanduser("~/.local/state/arr-watch.json"))


def _watch_state_load():
    try:
        with open(WATCH_STATE) as fh:
            return json.load(fh)
    except (OSError, ValueError):
        return {}


def _watch_state_save(st):
    try:
        os.makedirs(os.path.dirname(WATCH_STATE), exist_ok=True)
        with open(WATCH_STATE, "w") as fh:
            json.dump(st, fh)
    except OSError:
        pass


def cmd_watch(svc, args):
    """One-shot readiness check — THE generic replacement for per-title
    watch-*.sh watchdog scripts. Run it from a Hermes script-only cron job;
    with --once you need NO wrapper script and NO stamp files at all.

    arr <svc> watch <id|query> [<id|query> ...] [--season N]
        [--until on-disk|in-jellyfin] [--verify-audio LANG] [--verify-subs LANG]
        [--max-age HOURS] [--quiet] [--once]

    With --quiet it prints NOTHING while a download is progressing normally
    (a script-only cron delivers stdout only when non-empty), and reports on
    the state changes that matter:
      READY        exit 0   done (incl. optional Jellyfin visibility)
      pending      exit 1   still downloading / not indexed (silent w/ --quiet)
      VERIFY-FAIL  exit 2   on disk but --verify-audio/--verify-subs failed
      STUCK        exit 3   a queue item needs intervention (-> stuck --fix)
      STALLED      exit 4   nothing queued, gaps remain, idle > --max-age h
    --once adds cron-friendly memory (state in ~/.local/state/arr-watch.json):
    READY is announced a single time then stays silent forever; STUCK/STALLED
    alert once and re-alert only if the item recovers and breaks again; a
    transient API error is retried once, then treated as silent-pending
    instead of a false alert. Multiple targets are checked in one call
    (quote each); exit code is the worst one."""
    if not (svc.startswith("sonarr") or svc == "radarr"):
        die("watch: sonarr/sonarr-anime/radarr only")
    flags, rest = pop_flags(args, {"--season": 1, "--until": 1, "--verify-audio": 1,
                                   "--verify-subs": 1, "--max-age": 1, "--quiet": 0,
                                   "--once": 0})
    if not rest:
        die("watch: need an id or query")
    quiet = "--quiet" in flags
    until = flags.get("--until", "on-disk")
    if until not in ("on-disk", "in-jellyfin"):
        die("watch: --until on-disk|in-jellyfin")
    ids = [resolve_id(svc, r) for r in rest]  # resolve before --once try/except:
    # a bad title should die loudly, only downstream API hiccups are transient
    worst = 0
    for iid in ids:
        code = _watch_one(svc, iid, flags, until, quiet)
        worst = max(worst, code)
    sys.exit(worst)


def _watch_one(svc, iid, flags, until, quiet):
    once = "--once" in flags
    key = "%s:%d:%s:%s:%s" % (svc, iid, until, flags.get("--verify-audio", ""),
                              flags.get("--verify-subs", ""))
    st = _watch_state_load() if once else {}
    if once and st.get(key, {}).get("phase") == "ready":
        return 0  # already announced — stay silent forever
    for attempt in (1, 2):
        try:
            code, out = _watch_check(svc, iid, flags, until, quiet)
            break
        except SystemExit:  # api()/jf_api() die()d — transient service blip
            if attempt == 1:
                time.sleep(3)
                continue
            if once:  # don't false-alert from a cron; try again next firing
                return 1
            raise
    if once:
        prev = st.get(key, {}).get("phase")
        if code == 0:
            st[key] = {"phase": "ready", "ts": int(time.time())}
            _watch_state_save(st)
        elif code in (2, 3, 4):
            if prev == "alert%d" % code:
                return code  # already alerted for this failure mode — silent
            st[key] = {"phase": "alert%d" % code, "ts": int(time.time())}
            _watch_state_save(st)
        elif prev:  # recovered to pending — re-arm the alert
            del st[key]
            _watch_state_save(st)
    if out:
        print(out)
    return code


def _watch_check(svc, iid, flags, until, quiet):
    """Compute one item's watch status; returns (exit_code, text)."""
    season = int(flags["--season"]) if "--season" in flags else None
    is_series = svc.startswith("sonarr")
    item = api(svc, "GET", "/%s/%d" % ("series" if is_series else "movie", iid))
    q = _queue_records(svc, page_size=1000)["records"]
    if is_series:
        mine = [r for r in q if (r.get("seriesId") or (r.get("series") or {}).get("id")) == iid]
    else:
        mine = [r for r in q if ((r.get("movie") or {}).get("id") or r.get("movieId")) == iid]
    stuck = [r for r in mine if _is_stuck_queue_record(r)]
    if stuck:
        lines = ["STUCK: %s — %d queue item(s) need intervention:" % (item["title"], len(stuck))]
        for r in stuck:
            lines.append("  %s/%s  %s" % (r.get("status"), r.get("trackedDownloadState"),
                                          (r.get("title") or "")[:70]))
            msgs = _queue_status_messages(r)
            if msgs:
                lines.append("    " + "; ".join(msgs)[:220])
        lines.append("  -> arr %s stuck '%s' --fix" % (svc, item["title"]))
        return 3, "\n".join(lines)
    if is_series:
        _, rows = _series_coverage(svc, iid, series=item)
        if season is not None:
            rows = [d for d in rows if d["season"] == season]
        missing = sum(len(d["missing"]) for d in rows if d["season"] != 0 or season == 0)
        have = sum(d["files"] for d in rows)
        done = missing == 0 and have > 0 and not mine
        prog_desc = "%d aired monitored ep(s) still missing" % missing
    else:
        done = bool(item.get("hasFile")) and not mine
        prog_desc = "file not on disk yet"
    if not done:
        if mine:
            size = sum(r.get("size") or 0 for r in mine)
            left = sum(r.get("sizeleft") or 0 for r in mine)
            pct = int(100 * (1 - left / size)) if size else 0
            return 1, "" if quiet else ("downloading: %s — %d item(s), %d%% (%sMB left)" % (
                item["title"], len(mine), pct, mb(left)))
        if flags.get("--max-age"):
            last = _latest_history_date(svc, iid)
            cutoff = time.strftime("%Y-%m-%dT%H:%M:%S",
                                   time.gmtime(time.time() - float(flags["--max-age"]) * 3600))
            if not last or last[:19] < cutoff:
                return 4, ("STALLED: %s — %s; queue empty, no activity in %sh (last: %s)\n"
                           "  -> re-grab: arr %s grab %d%s" % (
                               item["title"], prog_desc, flags["--max-age"],
                               (last or "never")[:19], svc, iid,
                               (" --season %d" % season) if season is not None else ""))
        return 1, "" if quiet else ("pending: %s — %s, nothing in queue" % (
            item["title"], prog_desc))
    if until == "in-jellyfin":
        jf = _jf_find_item(item, is_series)
        if not jf:
            jf_api("/Library/Refresh", method="POST", soft=True)  # nudge the scan
            return 1, "" if quiet else (
                "imported: %s — waiting for Jellyfin to index it (refresh nudged)" % item["title"])
    lines = ["READY: %s (%s)" % (item["title"], item.get("year"))]
    code = 0
    if flags.get("--verify-audio") or flags.get("--verify-subs"):
        wa = _norm_lang(flags["--verify-audio"]) if flags.get("--verify-audio") else None
        ws = _norm_lang(flags["--verify-subs"]) if flags.get("--verify-subs") else None
        n, ma, ms = _verify_lang(_item_files(svc, iid, season), wa, ws)
        if wa:
            lines.append("  audio %s: %d/%d file(s) ok%s" % (
                wa, n - len(ma), n, ("; MISSING: " + ", ".join(ma[:5])) if ma else ""))
        if ws:
            lines.append("  subs %s: %d/%d file(s) ok%s" % (
                ws, n - len(ms), n, ("; MISSING: " + ", ".join(ms[:5])) if ms else ""))
        if ma or ms:
            code = 2
            lines[0] = ("VERIFY-FAIL: %s (%s) — on disk but the language checks"
                        " failed:" % (item["title"], item.get("year")))
    return code, "\n".join(lines)


# --- release audio-language heuristics ----------------------------------------
AUDIO_MARKERS = {
    "eng": ("dual audio", "dual-audio", "dualaudio", "dual]", "english dub",
            "eng dub", "engdub", "english audio", "dubbed", "multi audio",
            "multi-audio", "multi]"),
    "dual": ("dual audio", "dual-audio", "dualaudio", "dual]", "multi audio",
             "multi-audio"),
}


def _audio_match(title, lang):
    t = (title or "").lower()
    return any(m in t for m in AUDIO_MARKERS.get(lang, (lang,)))



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


# --- disk audit: unmanaged files = Jellyfin duplicates ------------------------
VIDEO_EXTS = (".mkv", ".mp4", ".m4v", ".avi", ".m2ts", ".ts", ".wmv", ".mov",
              ".webm", ".mpg", ".mpeg")
SIDECAR_EXTS = (".srt", ".ass", ".ssa", ".sub", ".idx", ".vtt", ".nfo")
QUARANTINE_ROOT = "/data/hermes/quarantine"


def _guess_ep(name):
    m = re.search(r"[Ss](\d{1,2})[Ee](\d{1,3})", name)
    return (int(m.group(1)), int(m.group(2))) if m else None


def _walk_videos(root):
    out = []
    for dirpath, dirs, files in os.walk(root):
        dirs[:] = [d for d in dirs if not d.startswith(".")]
        for f in files:
            if f.lower().endswith(VIDEO_EXTS):
                out.append(os.path.join(dirpath, f))
    return out


def _disk_audit(svc, iid, item=None):
    """Compare video files ON DISK under an item's folder with the files the arr
    TRACKS. Anything untracked is what makes Jellyfin show duplicate episodes /
    a wrong 'first episode' (e.g. a leftover Crunchyroll WEB-DL next to the
    Sonarr-managed file). Returns (item, unmanaged|None); None = folder not
    visible from here (can't audit). Each unmanaged entry:
    {path, size, ep:(s,e)|None, dup_of: tracked relativePath|None, sidecars:[]}."""
    is_series = svc.startswith("sonarr")
    if is_series:
        item = item or api(svc, "GET", "/series/%d" % iid)
        recs = api(svc, "GET", "/episodefile?seriesId=%d" % iid) or []
    else:
        item = item or api("radarr", "GET", "/movie/%d" % iid)
        recs = api("radarr", "GET", "/moviefile?movieId=%d" % iid) or []
    root = item.get("path") or ""
    if not root or not os.path.isdir(root):
        return item, None
    tracked, by_ep, ep_has_file = set(), {}, {}
    for f in recs:
        p = f.get("path") or os.path.join(root, f.get("relativePath", ""))
        tracked.add(os.path.realpath(p))
    if is_series:
        # authoritative (season, episode) -> tracked file map from the episode
        # list, so absolute-numbered anime releases resolve correctly
        rec_by_id = {f["id"]: f for f in recs}
        for e in api(svc, "GET", "/episode?seriesId=%d" % iid) or []:
            key = (e.get("seasonNumber"), e.get("episodeNumber"))
            ep_has_file[key] = bool(e.get("hasFile"))
            r = rec_by_id.get(e.get("episodeFileId"))
            if r:
                by_ep[key] = r.get("relativePath")
    unmanaged = []
    folder = os.path.basename(root.rstrip("/"))
    for p in _walk_videos(root):
        if os.path.realpath(p) in tracked:
            continue
        ep = _guess_ep(os.path.basename(p)) if is_series else None
        version = False
        if is_series:
            if ep is None:
                # no SxxEyy in the name (absolute-numbered anime release) —
                # let Sonarr's own parser map it, but only trust a same-series hit
                try:
                    pr = api(svc, "GET", "/parse?title="
                             + urllib.parse.quote(os.path.basename(p)))
                    pes = (pr or {}).get("episodes") or []
                    if pes and ((pr.get("series") or {}).get("id") == iid):
                        ep = (pes[0].get("seasonNumber"), pes[0].get("episodeNumber"))
                except Exception:
                    pass
            dup = by_ep.get(ep) if ep else None
        else:  # any extra video beside a tracked movie file duplicates it
            dup = (recs[0].get("relativePath") or recs[0].get("path")) if recs else None
            # "Movie (Year) - Some Label.mkv" beside the folder of the same name
            # is Jellyfin's INTENTIONAL multi-version convention (shows a version
            # picker, not a duplicate) — flag it, don't treat it as junk.
            stem = os.path.splitext(os.path.basename(p))[0]
            version = stem.startswith(folder + " - ")
        try:
            size = os.path.getsize(p)
        except OSError:
            size = 0
        base = os.path.splitext(os.path.basename(p))[0]
        side = []
        try:
            for g in os.listdir(os.path.dirname(p)):
                if (g != os.path.basename(p) and g.startswith(base)
                        and g.lower().endswith(SIDECAR_EXTS)):
                    side.append(os.path.join(os.path.dirname(p), g))
        except OSError:
            pass
        unmanaged.append({"path": p, "size": size, "ep": ep, "dup_of": dup,
                          "version": version, "sidecars": sorted(side),
                          "importable": bool(is_series and ep and dup is None
                                             and not ep_has_file.get(ep, False))})
    return item, sorted(unmanaged, key=lambda u: u["path"])


def _audit_warn(svc, iid, item=None):
    """Piggybacked proactive check for status/coverage/add — never raises."""
    try:
        _, un = _disk_audit(svc, iid, item=item)
    except Exception:
        return
    un = [u for u in (un or []) if not u.get("version")]
    if not un:
        return
    dups = sum(1 for u in un if u["dup_of"])
    print("  ⚠ %d unmanaged video file(s) on disk that %s does NOT track%s" % (
        len(un), svc, " — %d duplicate an episode/movie already on disk" % dups if dups else ""))
    print("    (Jellyfin will show these as duplicates/wrong versions)"
          " `arr %s audit %d` to inspect, `--quarantine --yes` to fix" % (svc, iid))


def _quarantine_moves(svc, item, unmanaged, dest=None, go=False):
    slug = re.sub(r"[^A-Za-z0-9]+", "-", item.get("title", "item")).strip("-").lower()
    dest = dest or os.path.join(QUARANTINE_ROOT, "%s-%s-%s" % (
        svc, slug, time.strftime("%Y%m%d")))
    moved = 0
    for u in unmanaged:
        for p in [u["path"]] + u["sidecars"]:
            if not go:
                print("  DRY: would move %s -> %s/" % (p, dest))
                continue
            os.makedirs(dest, exist_ok=True)
            tgt = os.path.join(dest, os.path.basename(p))
            n = 1
            while os.path.exists(tgt):
                stem, ext = os.path.splitext(os.path.basename(p))
                tgt = os.path.join(dest, "%s.%d%s" % (stem, n, ext))
                n += 1
            try:
                os.rename(p, tgt)
            except OSError as e:
                die("audit: move failed (%s)\n  media-group write needed — the hermes"
                    " agent has it; andrep needs the sudo-nix root path" % e)
            moved += 1
            print("  moved %s -> %s" % (p, tgt))
    return dest, moved


def cmd_audit(svc, args):
    """Disk-vs-database audit: video files the arr does NOT track.

    arr <svc> audit <id|query> [--quarantine] [--yes] [--dest DIR] [--json]
    arr <svc> audit --all [--quiet]
    Untracked files are what make Jellyfin show duplicate episodes (the leftover
    "Crunchyroll WEB-DL" ghost next to the managed file), wrong first episodes,
    etc. --quarantine MOVES them + their sidecar subs into a dated folder under
    /data/hermes/quarantine (dry-run unless --yes; nothing is deleted — restore
    = move back), then rescans the arr item and refreshes Jellyfin.
    --all sweeps the whole library and reports only offenders (cron-friendly)."""
    if not (svc.startswith("sonarr") or svc == "radarr"):
        die("audit: sonarr/sonarr-anime/radarr only")
    flags, rest = pop_flags(args, {"--quarantine": 0, "--yes": 0, "--dest": 1,
                                   "--json": 0, "--all": 0, "--quiet": 0,
                                   "--include-versions": 0})
    is_series = svc.startswith("sonarr")
    if "--all" in flags:
        items = api(svc, "GET", "/series" if is_series else "/movie")
        bad = skipped = 0
        for it in sorted(items, key=lambda x: x["title"]):
            try:
                _, un = _disk_audit(svc, it["id"], item=it)
            except Exception:
                un = None
            if un is None:
                skipped += 1
                continue
            un = [u for u in un if not u.get("version")]
            if un:
                bad += 1
                print("[%d] %s — %d unmanaged file(s), %sGB  (`arr %s audit %d`)" % (
                    it["id"], it["title"], len(un), gb(sum(u["size"] for u in un)),
                    svc, it["id"]))
        if not bad and "--quiet" not in flags:
            print("audit: no unmanaged files anywhere in %s%s" % (
                svc, " (%d folders not visible, skipped)" % skipped if skipped else ""))
        sys.exit(1 if bad else 0)
    if not rest:
        die("audit: need <id|query> or --all")
    iid = resolve_id(svc, rest[0])
    item, un = _disk_audit(svc, iid)
    if un is None:
        die("audit: folder %s not visible from this host — can't audit" % item.get("path"))
    if "--json" in flags:
        print(json.dumps(un, indent=2))
        return
    print("%s (%s) — %s" % (item["title"], item.get("year"), item.get("path")))
    if not un:
        print("  clean: every video file on disk is tracked by %s" % svc)
        return
    print("  %d unmanaged video file(s) (%sGB) NOT tracked by %s:" % (
        len(un), gb(sum(u["size"] for u in un)), svc))
    importable = unknown = 0
    for u in un:
        ep = "S%02dE%02d" % u["ep"] if u["ep"] else "?"
        print("    %s  %sMB  %s" % (ep, mb(u["size"]), u["path"]))
        if u.get("version"):
            print("          JELLYFIN VERSION ('Title (Year) - Label' naming ="
                  " intentional version picker) — skipped by --quarantine unless"
                  " --include-versions")
        elif u["dup_of"]:
            print("          DUPLICATE of tracked: %s" % u["dup_of"])
        elif u.get("importable"):
            importable += 1
            print("          UNIMPORTED — maps to %s which has NO tracked file" % ep)
        else:
            unknown += 1
            print("          unmatched — couldn't map to an episode; check by hand")
    if importable:
        print("  => %d file(s) are content for episodes with NO file — IMPORT them"
              " (`arr %s import '%s' --series %d --match <token> --map abs`),"
              " do NOT quarantine" % (importable, svc, item.get("path", ""), iid))
    if unknown:
        print("  => %d unmatched file(s): verify what they are (`arr %s parse"
              " \"<name>\"`) before quarantining" % (unknown, svc))
    movable = un if "--include-versions" in flags else [u for u in un if not u.get("version")]
    if "--quarantine" not in flags:
        if movable:
            print("  => Jellyfin shows these as duplicate/ghost entries. Rerun with"
                  " --quarantine [--yes] to move them out (kept, not deleted)")
        return
    if not movable:
        print("  => only intentional version files here — nothing to quarantine"
              " (override with --include-versions)")
        return
    go = "--yes" in flags
    dest, moved = _quarantine_moves(svc, item, movable, dest=flags.get("--dest"), go=go)
    if not go:
        print("  (dry-run — pass --yes to actually move %d file(s) to %s)" % (
            sum(1 + len(u["sidecars"]) for u in movable), dest))
        return
    print("  quarantined %d file(s) -> %s" % (moved, dest))
    if is_series:
        api(svc, "POST", "/command", {"name": "RescanSeries", "seriesId": iid})
    else:
        api("radarr", "POST", "/command", {"name": "RescanMovie", "movieId": iid})
    try:
        jf_api("/Library/Refresh", method="POST")
        print("  rescan + Jellyfin refresh triggered — verify with"
              " `arr jellyfin has '%s'`" % item["title"])
    except SystemExit:
        print("  arr rescan triggered (Jellyfin refresh failed — run"
              " `arr jellyfin refresh`)")


def _resolve_soft(svc, q):
    """Like resolve_id but never exits — returns (id, None) or (None, reason).
    Prefers an exact title match when a substring is ambiguous. For batch delete,
    where one bad title in a list shouldn't abort the whole run."""
    if str(q).isdigit():
        return int(q), None
    coll = "series" if svc.startswith("sonarr") else "movie"
    ql = q.lower()
    hits = []
    for it in api(svc, "GET", "/" + coll):
        names = [it.get("title", ""), it.get("titleSlug", "")]
        names += [a.get("title", "") for a in (it.get("alternateTitles") or [])]
        if any(ql in (n or "").lower() for n in names):
            hits.append(it)
    if not hits:
        return None, 'no match for "%s"' % q
    if len(hits) > 1:
        exact = [h for h in hits if (h.get("title", "") or "").lower() == ql]
        if len(exact) == 1:
            return exact[0]["id"], None
        cands = ", ".join("%s (%s) [%d]" % (h["title"], h.get("year"), h["id"]) for h in hits[:6])
        return None, 'ambiguous "%s" — %d matches: %s' % (q, len(hits), cands)
    return hits[0]["id"], None


def cmd_delete(svc, args):
    """Delete media via the arr API (deletes as the service user — no rm/perms
    fight — and updates the DB so it won't silently re-download). Dry-run unless
    --yes. Accepts MANY titles/ids at once. Also cancels each item's active
    downloads (removeFromClient) by default, so "delete X" fully stops wanting it
    — pass --keep-downloads to leave the queue alone, --blocklist to blocklist the
    cancelled releases. sonarr: --seasons X-Y (+ unmonitor) | --all (whole series).
    radarr: --file-only (keep entry, drop file) | default whole movie."""
    flags, rest = pop_flags(args, {"--seasons": 1, "--all": 0, "--file-only": 0,
                                   "--keep-monitored": 0, "--keep-downloads": 0,
                                   "--blocklist": 0, "--yes": 0})
    if not rest:
        die("delete: need an id or query")
    go = "--yes" in flags
    tag = "" if go else "[dry-run] "
    cancel = "--keep-downloads" not in flags
    blocklist = "true" if "--blocklist" in flags else "false"
    is_sonarr = svc.startswith("sonarr")

    # --- surgical single-item modes: --seasons (sonarr) / --file-only (radarr) ---
    if is_sonarr and "--seasons" in flags:
        if len(rest) != 1:
            die("delete --seasons: one show at a time")
        sid = resolve_id(svc, rest[0])
        s = api(svc, "GET", "/series/%d" % sid)
        allfiles = api(svc, "GET", "/episodefile?seriesId=%d" % sid) or []
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
        return
    if not is_sonarr and "--file-only" in flags:
        if len(rest) != 1:
            die("delete --file-only: one movie at a time")
        mid = resolve_id("radarr", rest[0])
        m = api("radarr", "GET", "/movie/%d" % mid)
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

    # --- whole-item delete (one or many) ---
    if is_sonarr and "--all" not in flags:
        die("sonarr delete: pass --seasons X-Y (surgical) or --all (whole series)")
    coll = "series" if is_sonarr else "movie"
    key = "seriesId" if is_sonarr else "movieId"
    # Fetch the queue once and index by item id, so a big batch doesn't re-pull it.
    by_item = {}
    if cancel:
        for r in _queue_records(svc, page_size=2000)["records"]:
            by_item.setdefault(r.get(key), []).append(r)

    ok = 0
    for q in rest:
        iid, err = _resolve_soft(svc, q)
        if iid is None:
            print("  ! %s — %s" % (q, err))
            continue
        item = api(svc, "GET", "/%s/%d" % (coll, iid))
        if is_sonarr:
            files = api(svc, "GET", "/episodefile?seriesId=%d" % iid) or []
            what = "SERIES '%s' + %d file(s) (%sGB)" % (
                item["title"], len(files), gb(sum(f.get("size", 0) for f in files)))
        else:
            what = "MOVIE '%s' + file (%sGB)" % (item["title"], gb(item.get("sizeOnDisk")))
        dls = by_item.get(iid, [])
        extra = (" + cancel %d download(s)" % len(dls)) if dls else ""
        print("%sDELETE %s%s" % (tag, what, extra))
        if not go:
            continue
        for r in dls:  # stop the client download first, while movieId still maps
            try:
                api(svc, "DELETE", "/queue/%d?removeFromClient=true&blocklist=%s" % (r["id"], blocklist))
            except SystemExit:
                pass  # record already gone — fine
        api(svc, "DELETE", "/%s/%d?deleteFiles=true" % (coll, iid))
        note = " (cancelled %d download%s)" % (len(dls), "" if len(dls) == 1 else "s") if dls else ""
        print("    deleted%s" % note)
        ok += 1
    if not go:
        print("  (pass --yes to delete)")
    else:
        print("done: %d/%d deleted" % (ok, len(rest)))


def cmd_delete_auto(args):
    """Top-level `arr delete <title|id> ... [--yes] [--keep-downloads]
    [--blocklist]` — no service needed. Works out per title whether it's a movie
    (Radarr) or a show (Sonarr/Sonarr-anime), cancels its active downloads, and
    removes it. Mixed movie/show lists are fine. Dry-run unless --yes."""
    flags, rest = pop_flags(args, {"--keep-downloads": 0, "--blocklist": 0, "--yes": 0})
    if not rest:
        die("delete: need one or more titles/ids")
    passthru = [f for f in ("--keep-downloads", "--blocklist", "--yes") if f in flags]
    libs = {}  # svc -> library list, fetched once each

    def matches_in(svc, q):
        if svc not in libs:
            coll = "series" if svc.startswith("sonarr") else "movie"
            libs[svc] = api(svc, "GET", "/" + coll)
        if str(q).isdigit():
            return [it for it in libs[svc] if it["id"] == int(q)]
        ql = q.lower()
        hits = []
        for it in libs[svc]:
            names = [it.get("title", ""), it.get("titleSlug", "")]
            names += [a.get("title", "") for a in (it.get("alternateTitles") or [])]
            if any(ql in (n or "").lower() for n in names):
                hits.append(it)
        exact = [h for h in hits if (h.get("title", "") or "").lower() == ql]
        return exact if len(exact) == 1 else hits

    groups = {}
    for q in rest:
        found = [(svc, h) for svc in ("radarr", "sonarr", "sonarr-anime")
                 for h in matches_in(svc, q)]
        if not found:
            print('  ! %s — no movie or show matches' % q)
            continue
        if len(found) > 1:
            where = ", ".join("%s:%s (%s) [%d]" % (s, h["title"], h.get("year"), h["id"])
                              for s, h in found[:6])
            print('  ! %s — matches %d items (%s); delete via `arr <svc> delete <id>`'
                  % (q, len(found), where))
            continue
        svc, h = found[0]
        groups.setdefault(svc, []).append(str(h["id"]))

    for svc in ("radarr", "sonarr", "sonarr-anime"):
        ids = groups.get(svc)
        if not ids:
            continue
        extra = ["--all"] if svc.startswith("sonarr") else []
        cmd_delete(svc, ids + extra + passthru)


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


def seerr_unfulfilled(args):
    """Requests whose REQUESTED content isn't actually on disk — judged against
    Sonarr/Sonarr-anime/Radarr season stats, not Seerr's own cached status.

    arr seerr unfulfilled [--fix] [--json] [--quiet]
    Also flags DIVERGED rows: Seerr says pending/partial but the requested
    seasons are complete (Seerr's whole-series status lies for season requests).
    --fix triggers arr-side searches (SeasonSearch/MoviesSearch) for the gaps."""
    flags, _ = pop_flags(args, {"--fix": 0, "--json": 0, "--quiet": 0})
    data = seerr_api("/request", {"take": "300", "skip": "0", "sort": "added"})
    movies = {m.get("tmdbId"): m for m in api("radarr", "GET", "/movie") or []}
    series_by_tvdb = {}
    for inst in ("sonarr", "sonarr-anime"):
        for s in api(inst, "GET", "/series") or []:
            series_by_tvdb.setdefault(s.get("tvdbId"), (inst, s))
    mstat = {1: "unknown", 2: "pending", 3: "processing", 4: "partial", 5: "available"}
    out = []
    for r in data.get("results", []):
        if r.get("status") == 3:  # declined
            continue
        m = r.get("media") or {}
        mtype = m.get("mediaType")
        title = m.get("title") or _seerr_title(mtype, m.get("tmdbId")) or "tmdb:%s" % m.get("tmdbId")
        row = {"id": r.get("id"), "title": title, "type": mtype,
               "by": (r.get("requestedBy") or {}).get("displayName", "?"),
               "seerr": mstat.get(m.get("status"), m.get("status")), "fix": None}
        if mtype == "movie":
            mv = movies.get(m.get("tmdbId"))
            if mv and mv.get("hasFile"):
                if m.get("status") != 5:
                    row["state"] = "DIVERGED — on disk but Seerr says %s" % row["seerr"]
                    out.append(row)
                continue
            if mv:
                row["state"] = "missing (radarr id %d)" % mv["id"]
                row["fix"] = ("radarr", mv["id"])
            else:
                row["state"] = "NOT IN RADARR"
            out.append(row)
        else:
            hit = series_by_tvdb.get(m.get("tvdbId"))
            want = sorted(x.get("seasonNumber") for x in (r.get("seasons") or [])
                          if x.get("seasonNumber") is not None)
            if not hit:
                row["state"] = "NOT IN SONARR%s" % (
                    " (requested S%s)" % ",".join(map(str, want)) if want else "")
                out.append(row)
                continue
            inst, s = hit
            gaps = []
            for se in s.get("seasons") or []:
                sn = se["seasonNumber"]
                if want and sn not in want:
                    continue
                if sn == 0 and not want:
                    continue
                st = se.get("statistics") or {}
                have, aired = st.get("episodeFileCount", 0), st.get("episodeCount", 0)
                if have < aired:
                    gaps.append((sn, have, aired))
            if not gaps:
                if m.get("status") != 5:
                    row["state"] = ("DIVERGED — requested seasons complete (%s) but Seerr says %s"
                                    % (inst, row["seerr"]))
                    out.append(row)
                continue
            row["state"] = "gaps: " + ", ".join("S%d %d/%d" % g for g in gaps)
            row["fix"] = (inst, s["id"], [g[0] for g in gaps])
            out.append(row)
    if "--json" in flags:
        print(json.dumps(out, indent=2))
        return
    if not out:
        if "--quiet" not in flags:
            print("unfulfilled: none — every non-declined request is on disk")
        return
    print("unfulfilled/diverged: %d request(s)" % len(out))
    for row in out:
        print("  #%-4s %-38s [%-5s] by %-12s %s" % (
            row["id"], str(row["title"])[:38], row["type"], row["by"][:12], row["state"]))
    if "--fix" not in flags:
        return
    print("fixes:")
    for row in out:
        f = row.get("fix")
        if not f:
            continue
        if f[0] == "radarr":
            r2 = api("radarr", "POST", "/command", {"name": "MoviesSearch", "movieIds": [f[1]]})
            print("  #%s %s: MoviesSearch queued (cmd %s)" % (row["id"], str(row["title"])[:30], r2["id"]))
        else:
            inst, sid2, seas = f
            for sn in seas:
                r2 = api(inst, "POST", "/command",
                         {"name": "SeasonSearch", "seriesId": sid2, "seasonNumber": sn})
                print("  #%s %s S%d: SeasonSearch queued on %s (cmd %s)" % (
                    row["id"], str(row["title"])[:30], sn, inst, r2["id"]))


def _jf_search_items(term, limit=10):
    r = jf_api("/Items", {"searchTerm": term, "Recursive": "true",
                          "IncludeItemTypes": "Movie,Series",
                          "Fields": "Path,ProviderIds", "Limit": str(limit)}, soft=True) or {}
    return r.get("Items") or []


def cmd_jf_has(args):
    """arr jellyfin has <title> — is it visible in the Jellyfin library NOW?
    (What users see; an arr import isn't 'ready' until this says yes.)"""
    if not args:
        die("jellyfin has: need a title")
    term = " ".join(args)
    hits = _jf_search_items(term)
    if not hits:
        print("not in Jellyfin: %s" % term)
        sys.exit(1)
    for it in hits:
        extra = ""
        if it.get("Type") == "Series":
            eps = jf_api("/Shows/%s/Episodes" % it["Id"], {"Limit": "1"}, soft=True)
            if eps and eps.get("TotalRecordCount") is not None:
                extra = "  %s episode(s)" % eps["TotalRecordCount"]
        print("  [%s] %s (%s)%s" % (it.get("Type"), it.get("Name"), it.get("ProductionYear"), extra))
        if it.get("Path"):
            print("      %s" % it["Path"])


def cmd_jf_refresh(args):
    """arr jellyfin refresh [--wait <title>] [--timeout SECS] — trigger a library
    scan; --wait polls until <title> is searchable (replaces curl+sleep loops)."""
    flags, _ = pop_flags(args, {"--wait": 1, "--timeout": 1})
    jf_api("/Library/Refresh", method="POST")
    print("library refresh triggered")
    if not flags.get("--wait"):
        return
    term = flags["--wait"]
    deadline = time.time() + int(flags.get("--timeout", "300"))
    while time.time() < deadline:
        hits = _jf_search_items(term, limit=5)
        if hits:
            print("visible in Jellyfin: %s (%s)" % (hits[0].get("Name"), hits[0].get("Type")))
            return
        time.sleep(5)
    print("timeout: '%s' not visible after %ss" % (term, flags.get("--timeout", "300")))
    sys.exit(2)


# --- Bazarr (subtitle manager) -------------------------------------------------
def bazarr_status(args):
    """Bazarr health: provider throttle state + wanted-subtitle backlog."""
    st = (bazarr_api("GET", "/system/status") or {}).get("data") or {}
    print("bazarr %s (sonarr %s, radarr %s)" % (
        st.get("bazarr_version"), st.get("sonarr_version"), st.get("radarr_version")))
    for p in (bazarr_api("GET", "/providers") or {}).get("data") or []:
        retry = p.get("retry")
        print("  provider %-18s %s%s" % (
            p.get("name"), p.get("status"),
            ("  (retry %s)" % retry) if retry and retry != "-" else ""))
    we = bazarr_api("GET", "/episodes/wanted", {"start": 0, "length": 1}) or {}
    wm = bazarr_api("GET", "/movies/wanted", {"start": 0, "length": 1}) or {}
    print("wanted: %s episode(s) + %s movie(s) missing subtitles" % (
        we.get("total", "?"), wm.get("total", "?")))
    print("(covers MAIN sonarr + radarr only — sonarr-anime items are not managed by Bazarr)")


def bazarr_wanted(args):
    """arr bazarr wanted [pattern] [--tv|--movies] [--limit N] — what Bazarr
    still owes subtitles for (its own wanted list, main sonarr + radarr)."""
    flags, rest = pop_flags(args, {"--tv": 0, "--movies": 0, "--limit": 1})
    pat = rest[0].lower() if rest else None
    lim = int(flags.get("--limit", "500"))
    shown = 0
    if "--movies" not in flags:
        data = bazarr_api("GET", "/episodes/wanted", {"start": 0, "length": lim}) or {}
        rows = data.get("data") or []
        for r in rows:
            title = r.get("seriesTitle") or ""
            if pat and pat not in title.lower():
                continue
            langs = ",".join(x.get("code3", "?") for x in r.get("missing_subtitles") or [])
            print("  tv     %-35s %-6s %-28s wants: %s" % (
                title[:35], r.get("episode_number"), (r.get("episodeTitle") or "")[:28], langs))
            shown += 1
        if not pat:
            print("  (tv total: %s)" % data.get("total"))
    if "--tv" not in flags:
        data = bazarr_api("GET", "/movies/wanted", {"start": 0, "length": lim}) or {}
        for r in data.get("data") or []:
            title = r.get("title") or ""
            if pat and pat not in title.lower():
                continue
            langs = ",".join(x.get("code3", "?") for x in r.get("missing_subtitles") or [])
            print("  movie  %-64s wants: %s" % (title[:64], langs))
            shown += 1
        if not pat:
            print("  (movie total: %s)" % data.get("total"))
    if pat and not shown:
        print("nothing in Bazarr's wanted list matching '%s' (NB anime-instance items never appear here)" % rest[0])


def bazarr_search(args):
    """arr bazarr search --series <sonarr id|query> | --movie <radarr id|query>
    Trigger Bazarr's search-missing for one item (uses the configured providers
    + OpenSubtitles membership; downloads happen in the background)."""
    flags, _ = pop_flags(args, {"--series": 1, "--movie": 1})
    if flags.get("--series"):
        sid = resolve_id("sonarr", flags["--series"])
        s = api("sonarr", "GET", "/series/%d" % sid)
        bazarr_api("PATCH", "/series", {"seriesid": sid, "action": "search-missing"})
        print("Bazarr search-missing triggered for series '%s' (sonarr id %d)" % (s["title"], sid))
    elif flags.get("--movie"):
        mid = resolve_id("radarr", flags["--movie"])
        m = api("radarr", "GET", "/movie/%d" % mid)
        bazarr_api("PATCH", "/movies", {"radarrid": mid, "action": "search-missing"})
        print("Bazarr search-missing triggered for movie '%s' (radarr id %d)" % (m["title"], mid))
    else:
        die("bazarr search: need --series <sonarr id|query> or --movie <radarr id|query>"
            " (sonarr-anime items are not Bazarr-covered)")
    print("(async — verify later with `arr bazarr wanted <title>` or `arr <svc> tracks <item> --missing-subs eng`)")


def bazarr_raw(args):
    if len(args) < 2:
        die("bazarr raw: usage: arr bazarr raw <METHOD> <path> [urlencoded-params]")
    method, path = args[0].upper(), args[1]
    if not path.startswith("/"):
        path = "/" + path
    params = dict(urllib.parse.parse_qsl(args[2])) if len(args) > 2 else None
    out = bazarr_api(method, path, params)
    print(json.dumps(out, indent=2) if out is not None else "(empty response)")


BAZARR_COMMANDS = {"status": bazarr_status, "wanted": bazarr_wanted,
                   "search": bazarr_search, "raw": bazarr_raw}


SEERR_COMMANDS = {"requests": seerr_requests, "request": seerr_request,
                  "unfulfilled": seerr_unfulfilled}


JELLYFIN_COMMANDS = {"unwatched": cmd_jf_unwatched, "has": cmd_jf_has,
                     "refresh": cmd_jf_refresh}


COMMANDS = {
    "status": cmd_status, "get": cmd_get, "seasons": cmd_seasons,
    "releases": cmd_releases, "grab": cmd_grab, "monitor": cmd_monitor,
    "queue": cmd_queue, "queue-rm": cmd_queue_rm, "stuck": cmd_stuck,
    "episodes": cmd_episodes, "wait": cmd_wait,
    "history": cmd_history, "wanted": cmd_wanted,
    "parse": cmd_parse, "search": cmd_search, "import": cmd_import,
    "files": cmd_files, "delete": cmd_delete, "audit": cmd_audit,
    "availability": cmd_availability, "lookup": cmd_lookup, "info": cmd_info,
    "tag": cmd_tag, "raw": cmd_raw,
    "add": cmd_add, "coverage": cmd_coverage, "tracks": cmd_tracks,
    "watch": cmd_watch, "replace": cmd_replace,
}

USAGE = """arr — Sonarr/Radarr/Prowlarr/SABnzbd CLI

  arr <service> <command> [args]
  service: sonarr | sonarr-anime | radarr | prowlarr | sab | jellyfin | seerr | bazarr
  (sonarr-anime = the dedicated anime Sonarr on :8990; all sonarr commands work
   against it, e.g. `arr sonarr-anime status`, `arr sonarr-anime seasons <show>`)

Commands (sonarr & radarr unless noted):
  status [query]                  list items (optionally filter by title)
        a single-match sonarr status also prints per-season coverage flags,
        so "do we have X?" surfaces partially-downloaded seasons by itself
  add <title> [--year Y] [--tvdb/--tmdb ID] [--seasons all|s1,s2|none]
        [--quality NAME] [--root PATH] [--no-search] [--requester <discordId>]
        [--require-subs L] [--require-audio L] [--no-wait] [--dry-run]
        add by title. REFUSES ambiguous matches (lists candidates — narrow by
        --year/--tvdb/--tmdb; the wrong-Gloria guard). If already in the
        library, prints its per-season coverage instead of adding. Defaults:
        monitor all regular seasons, library-majority quality profile, search
        immediately, then wait up to 60s and report what got grabbed — one
        command answers "is it downloading?" (--no-wait skips the wait).
        Fresh grabs are promoted to the front of the download queue (an
        interactive add means someone is waiting; backlog churn can't starve it).
        --requester wires up the download-notifier DM; --require-* makes the
        notifier's ready DM verify subs/dub languages via ffprobe.
  coverage <id|query> [--fix] [--tracks] [--lang LANG] [--fix-subs] [--dry-run]  (sonarr)
  coverage --all [--fix] [--quiet] [--limit N]
        per-season gaps vs AIRED episodes. --fix searches missing MONITORED
        eps (partial season -> EpisodeSearch, empty -> SeasonSearch);
        unmonitored seasons are only reported — ask the requester first.
        --tracks adds per-season DUB/SUB coverage for --lang (default eng) via
        ffprobe; --fix-subs also asks Bazarr to fetch missing subs (main sonarr
        only — the anime instance isn't Bazarr-covered).
        --all sweeps every monitored series (cron this; --quiet = silent when clean)
  tracks <id|query> [--season N] [--missing-audio LANG] [--missing-subs LANG] [--json]
        audio/sub tracks per file via ffprobe + sidecar .srt detection.
        `tracks NANA --missing-audio eng` = which eps lack an English dub;
        '*' marks the default track (catches wrong-default-audio bugs)
  watch <id|query> [more targets...] [--season N] [--until on-disk|in-jellyfin]
        [--verify-audio L] [--verify-subs L] [--max-age H] [--quiet] [--once]
        one-shot readiness check for cron watchdogs — silent while a download
        progresses (--quiet). exit: 0 ready, 1 pending, 2 verify-fail, 3 stuck,
        4 stalled (multi-target: worst code). --once = announce READY a single
        time, alert STUCK/STALLED/VERIFY-FAIL once (re-arm on recovery), retry
        transient API errors instead of false-alerting; state in
        ~/.local/state/arr-watch.json. Replaces per-title watch-*.sh scripts
        AND their stamp files — put the bare command in the cron, no wrapper.
  replace <old id|query> <correct title...> [--tmdb ID] [--year Y] [--yes]  (radarr)
        delete wrong movie+file, add the right one, carry requester tag, search.
        dry-run unless --yes (the German-Les-Visiteurs fix)
  get <id|query>                  full JSON for one item
  seasons <id|query>              (sonarr) per-season monitored + on-disk
  releases <id|query> [--season N|--episode EPID] [--audio eng|dual] [--timeout S]
        (--audio: dub heuristic on release names — Dual Audio/English Dub/Multi;
         --timeout: raise past the 300s default — anime searches can exceed it)
  grab <id|query> [--season N|--episode EPID] [--override|--via-sab] [--dry-run]
        [--wait] [--no-wait]
        no flags: search & let the arr decide (respects quality profile), then
        wait up to 60s and report what got grabbed (--no-wait skips; --timeout
        adjusts). --override: force-push every candidate release (bypass
        rejections). --via-sab: send candidate NZBs straight to SAB (bypass
        search cache). --wait: also block until the search command itself
        finishes (--timeout SECS, def 300)
        --requester <discordId>: stamp a requester:<id> tag so the download-notifier
              DMs that person a live progress bar (use when grabbing for someone)
  tag <id|query> [--requester <discordId>] [--require-subs LANG]
        [--require-audio LANG] [--remove]   (sonarr/radarr)
        requester-*: download-notifier DMs that person a live progress bar.
        require-*: the notifier's ✅ ready DM VERIFIES the language via ffprobe
        ("🔎 eng subs ✓" / "⚠ 1/3") — the fix for "with English subs" asks;
        no watcher cron needed. add accepts the same --require-* flags.
  episodes <id|query> [--season N] [--missing] [--monitored] [--json]   (sonarr)
        list episodes WITH ids (for grab/import by --episode). --missing = no file
  wait <commandId> [--timeout SECS]   block until a search/grab/import/refresh ends
  monitor <id|query> <spec>       sonarr: all|none|s1,s2,...   radarr: on|off
  info <id|query>                 concise identity (title/year/ids/origTitle/altTitles/state)
  lookup <term>                   TMDB/TVDB metadata search (disambiguate 1990 vs 2003)
  availability <id|query>         is it obtainable? searches the item's own titles + ids
  files <id|query> [--full]       files on disk (path/size/quality)
  audit <id|query> [--quarantine] [--yes] [--dest DIR] [--json]
  audit --all [--quiet]
        disk-vs-DB audit: video files the arr does NOT track (the cause of
        duplicate episodes / "wrong first episode" in Jellyfin). --quarantine
        moves them + sidecars to /data/hermes/quarantine (dry-run unless
        --yes; restore = move back), then rescans arr + Jellyfin.
        status/coverage/add warn about these automatically for single items;
        --all sweeps the whole library (exit 1 if offenders found — cron it).
  delete <id|query> ...           DELETE via API (dry-run unless --yes):
        takes MANY titles/ids at once and also cancels each item's active
        downloads (removeFromClient), so "delete X Y Z" fully stops wanting them.
        --keep-downloads leaves the queue alone; --blocklist blocklists the
        cancelled releases.
        sonarr: --seasons X-Y [--keep-monitored] (surgical) | --all (whole series)
        radarr: --file-only (drop file, keep movie) | (default: movie + file)

Top-level (no service prefix):
  queue [--json]                  whole-picture rollup across SAB + every arr
        queue: total jobs, TB left, ETA, per-category/state/quality counts,
        free space vs. queued, and a raw-disc warning. (Per-service `arr <svc>
        queue` is the filterable item listing.)
  delete <title|id> ...           removes each item whether it's a movie or a
        show and cancels its downloads; mixed lists fine. [--yes]
        [--keep-downloads] [--blocklist]

  parse "<release title>"         show how the arr maps a title (series+episode)
  import <folder> --series <id|query> [--match SUBSTR] [--season N]
        [--map auto|abs|se] [--mode copy|move] [--batch N] [--dry-run] [--wait]  (sonarr)
        force-import files into episodes, bypassing name matching (maps by
        absolute/SxxExx number). --match filters filenames (USE IT in a shared
        download folder, or every file gets mapped onto this series).
        Chunks --batch files per API call (default 10) so big imports can't time out.
  import <folder> --movie <id|query> [--match SUBSTR] [--mode copy|move]
        [--dry-run] [--wait]   (radarr) force-import file(s) into a movie
  queue [query] [--disc|--encrypted|--quality NAME|--status STATE] [--ids]   list queue items
        filter by title/quality/state; --ids prints just ids to pipe into queue-rm
  queue-rm <id…|pat> [--disc|--encrypted|--quality NAME|--status STATE]
        [--blocklist] [--keep-files] [--research] [--yes]   remove queue record(s)
        via API (dry-run unless --yes); default removes from the download client.
        --research re-searches the affected movies/shows. A raw-disc sweep:
        `arr radarr queue-rm --disc --blocklist --research --yes`
  stuck [query] [--json|--quiet] [--fix [--yes]]   queue items needing intervention
        --fix: auto-plan force-imports for import-blocked items (applies w/ --yes);
        failed items get a suggested queue-rm+re-grab line
  history [id|query] | wanted
  search <query> [--group X] [--indexer Y] [--limit N] [--audio eng|dual] [--json]  (prowlarr)
  grab <query> [--indexer X] [--group/--match Y] [--cat C] [--all] [--dry-run]
        (prowlarr) grab a matched release to the right client by protocol:
        usenet -> SABnzbd, torrent -> qBittorrent. Refuses >1 match w/o --all.
  raw <METHOD> <path> [json]      arbitrary API call

SABnzbd:
  arr sab queue [pat] | status | add <url> [--cat C] | prio <pat> [--top]
      | history [pat] | cleanup <pat> [--yes]   (cleanup deletes via SAB, files too)

Jellyfin / Seerr:
  arr jellyfin unwatched [--min-seasons N]   series on disk no user has watched
  arr jellyfin has <title>                   is it visible in the library NOW?
  arr jellyfin refresh [--wait <title>] [--timeout S]   trigger scan (+poll until visible)
  arr seerr requests [--pending] [query]     who requested what (+ media status)
  arr seerr request <id>                     full request detail
  arr seerr unfulfilled [--fix] [--json] [--quiet]   requests vs ACTUAL disk state
        (cross-checked against the arrs; flags Seerr-says-partial-but-complete
        divergences; --fix triggers arr-side searches for real gaps)

Bazarr (subtitle manager — covers MAIN sonarr + radarr; NOT sonarr-anime):
  arr bazarr status                     provider health/throttles + wanted backlog
  arr bazarr wanted [pat] [--tv|--movies]   items Bazarr still owes subs for
  arr bazarr search --series <q> | --movie <q>   trigger its subtitle search
  arr bazarr raw <METHOD> <path> [urlencoded-params]
  Prefer Bazarr for subtitles (it holds the OpenSubtitles membership) over
  hand-rolled subliminal runs.

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
            die("jellyfin: unwatched|has|refresh")
        fn = JELLYFIN_COMMANDS.get(argv[1])
        if not fn:
            die("unknown jellyfin command '%s'" % argv[1])
        fn(argv[2:])
        return
    if svc == "seerr":
        if len(argv) < 2:
            die("seerr: requests|request|unfulfilled")
        fn = SEERR_COMMANDS.get(argv[1])
        if not fn:
            die("unknown seerr command '%s'" % argv[1])
        fn(argv[2:])
        return
    if svc == "bazarr":
        if len(argv) < 2:
            die("bazarr: status|wanted|search|raw")
        fn = BAZARR_COMMANDS.get(argv[1])
        if not fn:
            die("unknown bazarr command '%s'" % argv[1])
        fn(argv[2:])
        return
    if svc == "delete":
        cmd_delete_auto(argv[1:])
        return
    if svc == "queue":
        cmd_queue_overview(argv[1:])
        return
    if svc not in SERVICES:
        die("unknown service '%s' (want sonarr|sonarr-anime|radarr|prowlarr|sab|jellyfin|seerr|bazarr)" % svc)
    if len(argv) < 2:
        print(USAGE)
        sys.exit(1)
    cmd, rest = argv[1], argv[2:]
    fn = COMMANDS.get(cmd)
    if not fn:
        die("unknown command '%s' (try: arr --help)" % cmd)
    fn(svc, rest)


if __name__ == "__main__":
    try:
        import signal
        signal.signal(signal.SIGPIPE, signal.SIG_DFL)  # no traceback when piped to head
    except (ImportError, AttributeError, ValueError):
        pass
    main(sys.argv[1:])
