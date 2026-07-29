//! acquire.rs — the commands that pull media in and repair the download
//! pipeline: grab (arr-search / --override / --via-sab / prowlarr direct),
//! stuck (blocked-import auto-repair), queue-rm, the top-level queue
//! overview, and force-import (ManualImport by parsed episode number).
//!
//! Port of arr.py's sab_add_url / qbit_add / cmd_prowlarr_grab /
//! _promote_downloads / _report_first_grab / cmd_grab / cmd_stuck /
//! cmd_queue_rm / cmd_queue_overview / cmd_import machinery.

use std::collections::{HashMap, HashSet};
use std::io::{Read as _, Write as _};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use arr_api::http::qbit_post_form;
use arr_api::json::items;
use arr_api::{
    api, api_t, die, fmt_gb, gb, mb, pop_flags, resolve_id, sab_api, try_api, ApiError, Flags,
    JsonExt,
};

/// interactive indexer searches (/release) can take minutes
const SEARCH_TIMEOUT: u64 = 300;

// --- small Python-parity helpers ---------------------------------------------

/// Python truthiness of a JSON value.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map_or(false, |f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn truthy_key(v: &Value, key: &str) -> bool {
    v.get(key).map_or(false, truthy)
}

/// Python `"%s" % v.get(key)` — "None" for missing/null, True/False for bools.
fn ps(v: &Value, key: &str) -> String {
    match v.get(key) {
        None | Some(Value::Null) => "None".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(true)) => "True".into(),
        Some(Value::Bool(false)) => "False".into(),
        Some(Value::Number(n)) => n.to_string(),
        Some(other) => other.to_string(),
    }
}

/// Python `s[:n]` (chars, not bytes).
fn trunc(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Python int(str) — die instead of a ValueError traceback.
fn py_int(s: &str) -> i64 {
    s.trim()
        .parse::<i64>()
        .unwrap_or_else(|_| die(&format!("invalid literal for int() with base 10: '{}'", s)))
}

fn flag_int(flags: &Flags, name: &str, default: i64) -> i64 {
    match flags.val(name) {
        Some(v) => py_int(v),
        None => default,
    }
}

/// Python `flags.get(f)` truthiness for value-flags (empty string is falsy).
fn flag_truthy<'a>(flags: &'a Flags, name: &str) -> Option<&'a str> {
    flags.val(name).filter(|v| !v.is_empty())
}

/// urllib.parse.quote with the default safe="/".
fn urlquote(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// The exact message api()/api_t() would die with — for call sites where the
/// Python catches SystemExit (die() has already printed, then execution
/// continues), we print the same line to stderr and carry on.
fn api_err_msg(method: &str, path: &str, timeout: u64, e: &ApiError) -> String {
    match e {
        ApiError::Http { code, detail } => {
            format!("{} {} -> HTTP {} {}", method, path, code, detail)
        }
        ApiError::Timeout => format!(
            "{} {} timed out after {}s (indexer searches can be slow — retry)",
            method, path, timeout
        ),
        ApiError::Net(reason) => format!("{} {} -> {}", method, path, reason),
    }
}

fn queue_path(svc: &str, page_size: i64) -> String {
    let extra = if svc.starts_with("sonarr") {
        "includeUnknownSeriesItems=true&includeSeries=true&includeEpisode=true"
    } else {
        "includeUnknownMovieItems=true&includeMovie=true"
    };
    format!("/queue?pageSize={}&{}", page_size, extra)
}

/// _queue_records with the Python callers' `except SystemExit` behavior:
/// on API error, print the die() message to stderr and return Err.
fn queue_records_caught(svc: &str, page_size: i64) -> Result<Vec<Value>, ()> {
    let path = queue_path(svc, page_size);
    match try_api(svc, "GET", &path, None, 120) {
        Ok(v) => Ok(v
            .map(|q| q.a("records").to_vec())
            .unwrap_or_default()),
        Err(e) => {
            eprintln!("arr: {}", api_err_msg("GET", &path, 120, &e));
            Err(())
        }
    }
}

// --- fallible SAB (for call sites where the Python catches SystemExit) -------

/// Minimal HTTP/1.0 GET against a localhost service (HTTP/1.0 so the server
/// never chunks; read to EOF). arr-api's sab_api dies on error, but
/// _sab_flagged_labels must survive SAB being down.
fn http10_get(port: u16, path: &str, timeout: u64) -> Result<String, String> {
    let addr: std::net::SocketAddr = format!("127.0.0.1:{}", port)
        .parse()
        .map_err(|e: std::net::AddrParseError| e.to_string())?;
    let mut conn = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(timeout))
        .map_err(|e| e.to_string())?;
    conn.set_read_timeout(Some(Duration::from_secs(timeout))).ok();
    conn.set_write_timeout(Some(Duration::from_secs(timeout))).ok();
    conn.write_all(
        format!(
            "GET {} HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            path
        )
        .as_bytes(),
    )
    .map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    conn.read_to_end(&mut raw).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = match text.find("\r\n\r\n") {
        Some(i) => (&text[..i], &text[i + 4..]),
        None => (text.as_str(), ""),
    };
    let code: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    if !(200..300).contains(&code) {
        return Err(format!("HTTP {}", code));
    }
    Ok(body.to_string())
}

/// Fallible SAB API call (same URL shape as arr-api's sab_api).
fn sab_try_get(mode: &str, params: &[(&str, &str)]) -> Result<Value, String> {
    let key = arr_api::sab_key();
    let mut q: Vec<(&str, &str)> = vec![("mode", mode), ("output", "json"), ("apikey", &key)];
    q.extend_from_slice(params);
    let body = http10_get(
        arr_api::SAB_PORT,
        &format!("/api/?{}", arr_api::http::form_encode(&q)),
        120,
    )?;
    match serde_json::from_str(&body) {
        Ok(v) => Ok(v),
        Err(_) => Ok(Value::String(body)),
    }
}

// --- download clients ---------------------------------------------------------

fn sab_add_url(url: &str, cat: &str, name: &str) -> bool {
    let mut params: Vec<(&str, &str)> = vec![("name", url), ("cat", cat)];
    if !name.is_empty() {
        params.push(("nzbname", name));
    }
    let r = sab_api("addurl", &params, 120);
    r.b("status")
}

fn qbit_add(link: &str, cat: &str) -> bool {
    match qbit_post_form("/api/v2/torrents/add", &[("urls", link), ("category", cat)]) {
        Ok(_) => true, // 2xx — Python's `.startswith("ok") or resp.status == 200`
        Err(e) => {
            let reason = match e {
                ApiError::Http { code, .. } => format!("HTTP {}", code),
                ApiError::Timeout => "timed out".into(),
                ApiError::Net(m) => m,
            };
            die(&format!("qbit add -> {}", reason))
        }
    }
}

// --- requester tagging (private copies; cmd_tag itself lives elsewhere) ------

fn ensure_tag(svc: &str, label: &str) -> i64 {
    let tags = api(svc, "GET", "/tag", None);
    for t in items(&tags) {
        if t.s("label").to_lowercase() == label.to_lowercase() {
            return t.i("id");
        }
    }
    api(svc, "POST", "/tag", Some(&json!({ "label": label })))
        .unwrap_or(Value::Null)
        .i("id")
}

/// Add/remove one tag label on a series/movie via the editor endpoint.
fn stamp_label(svc: &str, item_id: i64, label: &str, remove: bool) -> &'static str {
    let (coll, ids_field) = if svc.starts_with("sonarr") {
        ("series", "seriesIds")
    } else if svc == "radarr" {
        ("movie", "movieIds")
    } else {
        die("tag: only sonarr/sonarr-anime/radarr have taggable items")
    };
    let tag_id = ensure_tag(svc, label);
    let mut body = Map::new();
    body.insert(ids_field.to_string(), json!([item_id]));
    body.insert("tags".to_string(), json!([tag_id]));
    body.insert(
        "applyTags".to_string(),
        json!(if remove { "remove" } else { "add" }),
    );
    api(svc, "PUT", &format!("/{}/editor", coll), Some(&Value::Object(body)));
    coll
}

fn tag_requester(svc: &str, query: &str, discord_id: &str) -> (&'static str, i64, String) {
    let did = discord_id.trim().to_string();
    if did.is_empty() || !did.chars().all(|c| c.is_ascii_digit()) {
        die(&format!(
            "tag: --requester must be a numeric Discord user id (got '{}')",
            discord_id
        ));
    }
    let item_id = resolve_id(svc, query);
    // Sonarr/Radarr tag labels allow only [a-z0-9-] (no colon), so `requester-<id>`.
    let coll = stamp_label(svc, item_id, &format!("requester-{}", did), false);
    (coll, item_id, did)
}

// --- release query (shared by grab --override/--via-sab) ---------------------

fn release_query(svc: &str, args: &[String]) -> String {
    let (flags, rest) = pop_flags(args, &[("--season", 1), ("--episode", 1)]);
    if rest.is_empty() {
        die("need an id or query");
    }
    if svc.starts_with("sonarr") {
        let sid = resolve_id(svc, &rest[0]);
        if flags.has("--episode") {
            return format!("episodeId={}", flags.val_or("--episode", ""));
        }
        if flags.has("--season") {
            return format!("seriesId={}&seasonNumber={}", sid, flags.val_or("--season", ""));
        }
        die("sonarr releases: need --season N or --episode EPID")
    } else {
        format!("movieId={}", resolve_id(svc, &rest[0]))
    }
}

// --- prowlarr direct grab ----------------------------------------------------

/// arr prowlarr grab <query> [--indexer X] [--group/--match Y] [--cat C]
///     [--all] [--dry-run]
/// Search across indexers and send the matching release to the right client:
/// usenet -> SABnzbd, torrent -> qBittorrent. Refuses >1 match without --all.
/// Direct grabs skip the arr's TRaSH-synced profile scoring, so vet the
/// release name yourself.
pub fn cmd_prowlarr_grab(args: &[String]) {
    let (flags, rest) = pop_flags(
        args,
        &[
            ("--indexer", 1),
            ("--group", 1),
            ("--match", 1),
            ("--cat", 1),
            ("--limit", 1),
            ("--all", 0),
            ("--dry-run", 0),
        ],
    );
    println!(
        "note: direct grabs skip the arr's TRaSH profile scoring — vet the name (upscale/LQ/BR-DISK markers) before pushing"
    );
    if rest.is_empty() {
        die("prowlarr grab: need a search query");
    }
    let cat = flags.val_or("--cat", "sonarr");
    let qs = arr_api::http::form_encode(&[
        ("query", rest[0].as_str()),
        ("type", "search"),
        ("limit", flags.val_or("--limit", "100")),
    ]);
    let res = api_t("prowlarr", "GET", &format!("/search?{}", qs), None, SEARCH_TIMEOUT);
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut rows: Vec<Value> = Vec::new();
    for r in items(&res) {
        let k = (r.s("title").to_string(), r.s("indexer").to_string());
        if seen.contains(&k) {
            continue;
        }
        seen.insert(k);
        rows.push(r.clone());
    }
    if let Some(ix) = flag_truthy(&flags, "--indexer") {
        let ixl = ix.to_lowercase();
        rows.retain(|r| r.s("indexer").to_lowercase().contains(&ixl));
    }
    for f in ["--group", "--match"] {
        if let Some(pat) = flag_truthy(&flags, f) {
            let pl = pat.to_lowercase();
            rows.retain(|r| r.s("title").to_lowercase().contains(&pl));
        }
    }
    if rows.is_empty() {
        die("no releases match");
    }
    if rows.len() > 1 && !flags.has("--all") {
        eprintln!("multiple matches — narrow with --match/--group/--indexer, or pass --all:");
        let mut sorted = rows.clone();
        sorted.sort_by_key(|r| std::cmp::Reverse(r.i("size")));
        for r in sorted.iter().take(20) {
            let sd = if r.has("seeders") {
                format!(" {}s", ps(r, "seeders"))
            } else {
                String::new()
            };
            eprintln!(
                "  [{}] {}MB {}{}  {}",
                ps(r, "indexer"),
                mb(r.i("size")),
                ps(r, "protocol"),
                sd,
                ps(r, "title")
            );
        }
        die(&format!("refusing to grab {} releases without --all", rows.len()));
    }
    let mut n = 0;
    for r in &rows {
        let proto = r.s("protocol");
        let title = r.s("title");
        if flags.has("--dry-run") {
            println!(
                "DRY {} -> {}: {}",
                ps(r, "protocol"),
                if proto == "torrent" { "qbit" } else { "sab" },
                ps(r, "title")
            );
            n += 1;
            continue;
        }
        let (ok, tag) = if proto == "usenet" {
            (sab_add_url(r.s("downloadUrl"), cat, title), "sab")
        } else {
            let g = r.s("guid");
            let link = if g.starts_with("magnet:") {
                g.to_string()
            } else if truthy_key(r, "downloadUrl") {
                r.s("downloadUrl").to_string()
            } else {
                r.s("magnetUrl").to_string()
            };
            (qbit_add(&link, cat), "qbit")
        };
        println!(
            "{}{}",
            if ok { format!("{}+ ", tag) } else { "FAIL ".into() },
            title
        );
        n += 1;
    }
    println!("({} release(s) grabbed to cat={})", n, cat);
}

// --- grab (arr search / override / via-sab) ----------------------------------

/// Move fresh grabs to the front of the download clients' queues. An
/// interactive add/grab means a person is waiting — without this, new requests
/// starve behind backlog churn (dead-release grinds hog every connection).
fn promote_downloads(records: &[Value]) {
    let mut bumped = 0;
    for r in records {
        let did = r.s("downloadId");
        if did.is_empty() {
            continue;
        }
        if r.s("protocol") == "usenet" {
            sab_api(
                "queue",
                &[("name", "priority"), ("value", did), ("value2", "2")],
                120,
            );
            sab_api("switch", &[("value", did), ("value2", "0")], 120);
            bumped += 1;
        } else {
            // promotion is best-effort; the download itself is unaffected
            if qbit_post_form("/api/v2/torrents/topPrio", &[("hashes", &did.to_lowercase())])
                .is_ok()
            {
                bumped += 1;
            }
        }
    }
    if bumped > 0 {
        println!("  promoted {} download(s) to the front of the queue", bumped);
    }
}

fn series_id_of(r: &Value) -> i64 {
    if r.i("seriesId") != 0 {
        r.i("seriesId")
    } else {
        r.at(&["series", "id"]).as_i64().unwrap_or(0)
    }
}

fn movie_id_of(r: &Value) -> i64 {
    let mid = r.at(&["movie", "id"]).as_i64().unwrap_or(0);
    if mid != 0 {
        mid
    } else {
        r.i("movieId")
    }
}

/// Briefly poll the queue after triggering a search, so one command can
/// honestly say "downloading <release>" instead of "search started". Fresh
/// grabs get promoted to the front of the download queue.
pub fn report_first_grab(svc: &str, iid: i64, is_series: bool, timeout: i64) -> bool {
    let start = Instant::now();
    loop {
        let q = queue_records_caught(svc, 1000).unwrap_or_default();
        let mine: Vec<&Value> = q
            .iter()
            .filter(|&r| {
                if is_series {
                    series_id_of(r) == iid
                } else {
                    movie_id_of(r) == iid
                }
            })
            .collect();
        if !mine.is_empty() {
            let size: i64 = mine.iter().map(|r| r.i("size")).sum();
            println!(
                "  grabbed {} release(s), {}GB — downloading:",
                mine.len(),
                fmt_gb(size)
            );
            for r in mine.iter().take(4) {
                let t = r.s("title");
                println!("    {}", trunc(if t.is_empty() { "?" } else { t }, 75));
            }
            if mine.len() > 4 {
                println!("    ... +{} more", mine.len() - 4);
            }
            let owned: Vec<Value> = mine.into_iter().cloned().collect();
            promote_downloads(&owned);
            return true;
        }
        if start.elapsed().as_secs_f64() > timeout as f64 {
            println!(
                "  nothing grabbed in {}s — the search may still be running (indexers can be slow) or releases are scarce. `arr {} queue` shows late grabs; `arr {} releases {}` shows candidates with reject reasons",
                timeout, svc, svc, iid
            );
            return false;
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

/// Monitoring gate. The arrs' *Search commands only consider MONITORED items,
/// so grabbing an unmonitored season runs a search, reports success, and pulls
/// nothing — a silent no-op that reads like "no releases exist". Report it
/// (and stop, rather than burn the 60s wait); `--monitor` turns monitoring on
/// additively — unlike `arr <svc> monitor <id> s2`, which unmonitors the rest.
fn ensure_monitored(svc: &str, iid: i64, flags: &Flags, dry: bool) -> bool {
    if flags.has("--episode") {
        // explicit episode ids are searched regardless of monitoring
        return true;
    }
    if svc == "radarr" {
        let mut m = api(svc, "GET", &format!("/movie/{}", iid), None).unwrap_or(Value::Null);
        if m.b("monitored") {
            return true;
        }
        if flags.has("--monitor") && dry {
            println!("DRY: would monitor movie {}", iid);
            return true;
        }
        if !flags.has("--monitor") {
            println!(
                "  ⚠ this movie is NOT monitored — a search grabs nothing. Rerun with --monitor (or `arr radarr monitor {} on`)",
                iid
            );
            return false;
        }
        m["monitored"] = Value::Bool(true);
        api(svc, "PUT", &format!("/movie/{}", iid), Some(&m));
        println!("  monitored the movie (was off)");
        return true;
    }

    let mut s = api(svc, "GET", &format!("/series/{}", iid), None).unwrap_or(Value::Null);
    let scope: Vec<i64> = match flags.has("--season") {
        true => vec![py_int(flags.val_or("--season", ""))],
        false => s.a("seasons").iter().map(|se| se.i("seasonNumber")).filter(|n| *n != 0).collect(),
    };
    let off: Vec<i64> = s
        .a("seasons")
        .iter()
        .filter(|se| scope.contains(&se.i("seasonNumber")) && !se.b("monitored"))
        .map(|se| se.i("seasonNumber"))
        .collect();
    if off.is_empty() && s.b("monitored") {
        return true;
    }
    let names = |v: &[i64]| v.iter().map(|n| format!("S{}", n)).collect::<Vec<_>>().join(",");

    if flags.has("--monitor") {
        if dry {
            println!(
                "DRY: would monitor {} on series {}",
                if off.is_empty() { "the series".to_string() } else { names(&off) },
                iid
            );
            return true;
        }
        s["monitored"] = Value::Bool(true);
        if let Some(seasons) = s.get_mut("seasons").and_then(Value::as_array_mut) {
            for se in seasons {
                if scope.contains(&se.i("seasonNumber")) {
                    se["monitored"] = Value::Bool(true);
                }
            }
        }
        api(svc, "PUT", &format!("/series/{}", iid), Some(&s));
        println!(
            "  monitored {} (other seasons untouched)",
            if off.is_empty() { "the series".to_string() } else { names(&off) }
        );
        return true;
    }
    if !off.is_empty() && off.len() == scope.len() {
        println!(
            "  ⚠ {} unmonitored — the search would grab nothing. Rerun with --monitor to include {}",
            names(&off),
            if off.len() == 1 { "it" } else { "them" }
        );
        return false;
    }
    if !off.is_empty() {
        println!("  note: {} unmonitored and will be skipped; --monitor includes them", names(&off));
    }
    true
}

pub fn cmd_grab(svc: &str, args: &[String]) {
    if svc == "prowlarr" {
        return cmd_prowlarr_grab(args);
    }
    let (flags, rest) = pop_flags(
        args,
        &[
            ("--season", 1),
            ("--episode", 1),
            ("--override", 0),
            ("--via-sab", 0),
            ("--dry-run", 0),
            ("--wait", 0),
            ("--timeout", 1),
            ("--requester", 1),
            ("--no-wait", 0),
            ("--monitor", 0),
        ],
    );
    if rest.is_empty() {
        die("grab: need an id or query");
    }
    let overridef = flags.has("--override");
    let dry = flags.has("--dry-run");

    // Stamp the requester so the download-notifier DMs them with a progress bar.
    // Tagging the series/movie is branch-independent, so do it once up front.
    if let Some(req) = flag_truthy(&flags, "--requester") {
        // Skip when it isn't in the library yet — the add handoff below passes
        // --requester along, and tagging would resolve-and-die before we get
        // there.
        if !dry && crate::disk::resolve_soft(svc, &rest[0]).is_ok() {
            let (coll, id, did) = tag_requester(svc, &rest[0], req);
            println!("tagged requester:{} on {} #{}", did, coll, id);
        }
    }

    if flags.has("--via-sab") {
        // Fetch candidate usenet releases and hand their NZBs straight to SAB
        // under the service category — bypasses Sonarr's search cache entirely
        // (so it works for releases Sonarr's per-episode search can't see).
        println!(
            "note: --via-sab bypasses the TRaSH-synced profile — its upscale/LQ/BR-DISK protection doesn't apply; vet release names yourself"
        );
        let rels = api_t(
            svc,
            "GET",
            &format!("/release?{}", release_query(svc, args)),
            None,
            SEARCH_TIMEOUT,
        );
        let mut seen: HashSet<String> = HashSet::new();
        let mut n = 0;
        for r in items(&rels) {
            if r.s("protocol") != "usenet" || !truthy_key(r, "downloadUrl") {
                continue;
            }
            let g = r.s("guid").to_string();
            if seen.contains(&g) {
                continue;
            }
            seen.insert(g);
            n += 1;
            if dry {
                println!("DRY via-sab: {}", r.s("title"));
                continue;
            }
            let ok = sab_add_url(r.s("downloadUrl"), svc, r.s("title"));
            println!("{}{}", if ok { "added: " } else { "FAILED: " }, r.s("title"));
        }
        println!("({} usenet release(s) sent to SAB cat={})", n, svc);
        return;
    }

    if !overridef {
        // let the arr search & decide (respects the quality profile)
        let iid = match crate::disk::resolve_soft(svc, &rest[0]) {
            Ok(id) => id,
            // Not in the library yet: `add` is the same intent (monitor +
            // search + wait + promote), so converge instead of making the
            // caller discover which of the two verbs applies.
            Err(e) if e.starts_with("no match") => {
                println!("not in the {} library yet — adding it (which searches too)", svc);
                let mut add_args = vec![rest[0].clone()];
                if let Some(v) = flag_truthy(&flags, "--requester") {
                    add_args.push("--requester".into());
                    add_args.push(v.to_string());
                }
                for f in ["--dry-run", "--no-wait"] {
                    if flags.has(f) {
                        add_args.push(f.into());
                    }
                }
                return crate::policy::cmd_add(svc, &add_args);
            }
            Err(e) => die(&e),
        };
        if !ensure_monitored(svc, iid, &flags, dry) {
            return;
        }
        let (body, dry_str) = if svc.starts_with("sonarr") {
            if flags.has("--episode") {
                let e = py_int(flags.val_or("--episode", ""));
                (
                    json!({"name": "EpisodeSearch", "episodeIds": [e]}),
                    format!("{{\"name\": \"EpisodeSearch\", \"episodeIds\": [{}]}}", e),
                )
            } else if flags.has("--season") {
                let sn = py_int(flags.val_or("--season", ""));
                (
                    json!({"name": "SeasonSearch", "seriesId": iid, "seasonNumber": sn}),
                    format!(
                        "{{\"name\": \"SeasonSearch\", \"seriesId\": {}, \"seasonNumber\": {}}}",
                        iid, sn
                    ),
                )
            } else {
                (
                    json!({"name": "SeriesSearch", "seriesId": iid}),
                    format!("{{\"name\": \"SeriesSearch\", \"seriesId\": {}}}", iid),
                )
            }
        } else {
            (
                json!({"name": "MoviesSearch", "movieIds": [iid]}),
                format!("{{\"name\": \"MoviesSearch\", \"movieIds\": [{}]}}", iid),
            )
        };
        if dry {
            println!("DRY: POST /command {}", dry_str);
            return;
        }
        let r = api(svc, "POST", "/command", Some(&body)).unwrap_or(Value::Null);
        println!(
            "queued {} (command id {}, status {})",
            ps(&r, "commandName"),
            ps(&r, "id"),
            ps(&r, "status")
        );
        if flags.has("--wait") {
            // full search-command completion (bounded by --timeout)
            let rec = wait_command(svc, r.i("id"), flag_int(&flags, "--timeout", 300));
            println!("  -> {} {}", ps(&rec, "status"), rec.s("message"));
        }
        if !flags.has("--no-wait") {
            report_first_grab(
                svc,
                iid,
                svc.starts_with("sonarr"),
                flag_int(&flags, "--timeout", 60),
            );
        }
        return;
    }

    // --override: force-push every candidate release, bypassing rejections
    println!(
        "note: --override ignores every profile rejection (TRaSH scoring incl. upscale/LQ/BR-DISK protection) — worth a concrete reason"
    );
    let rels = api_t(
        svc,
        "GET",
        &format!("/release?{}", release_query(svc, args)),
        None,
        SEARCH_TIMEOUT,
    );
    let mut seen: HashSet<String> = HashSet::new();
    let mut count = 0;
    for r in items(&rels) {
        let g = r.s("guid").to_string();
        if seen.contains(&g) {
            continue;
        }
        seen.insert(g);
        count += 1;
        if dry {
            println!("DRY push: {}", r.s("title"));
            continue;
        }
        let body = json!({
            "guid": r.get("guid").cloned().unwrap_or(Value::Null),
            "indexerId": r.get("indexerId").cloned().unwrap_or(Value::Null),
        });
        match try_api(svc, "POST", "/release", Some(&body), 120) {
            Ok(_) => println!("pushed: {}", r.s("title")),
            Err(e) => {
                // Python catches the SystemExit from die() here — the die
                // message has already hit stderr, then the FAILED line follows.
                eprintln!("arr: {}", api_err_msg("POST", "/release", 120, &e));
                eprintln!("FAILED: {}", r.s("title"));
            }
        }
    }
    println!("({} release(s) processed)", count);
}

// --- stuck (blocked-import repair) -------------------------------------------

/// The target dir sometimes doesn't exist yet and the import API throws
/// DirectoryNotFound — pre-create it if we're allowed to. arr may run as a
/// helper user while Radarr/Sonarr writes as a service user in the parent
/// directory's shared group; a newly-created helper-owned directory would
/// otherwise be unwritable by the service and ManualImport misleadingly
/// completes while importing nothing. When we own the target, inherit the
/// parent's group and grant group rwx. (Python wraps this in try/except
/// OSError: pass — any failure silently abandons the rest.)
fn prepare_target_dir(item: &Value) {
    let target = item.s("path");
    if target.is_empty() {
        return;
    }
    let _ = (|| -> Option<()> {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o770)
            .create(target)
            .ok()?;
        let st = std::fs::metadata(target).ok()?;
        if st.uid() == unsafe { libc::geteuid() } {
            let parent = std::path::Path::new(target)
                .parent()
                .unwrap_or_else(|| std::path::Path::new(""));
            let parent_gid = std::fs::metadata(parent).ok()?.gid();
            let c = std::ffi::CString::new(target).ok()?;
            if unsafe { libc::chown(c.as_ptr(), u32::MAX, parent_gid) } != 0 {
                return None;
            }
            let mode = st.mode() & 0o7777;
            std::fs::set_permissions(target, std::fs::Permissions::from_mode(mode | 0o070))
                .ok()?;
        }
        Some(())
    })();
}

/// Show queue items that need intervention: failed/import blocked/pending.
///
/// arr <svc> stuck [query] [--json|--quiet] [--fix [--yes]]
/// --fix: for import-blocked/pending items, plan a force-import of the
/// completed download into its linked series/movie. Shows the file->episode
/// mapping; applies it (mode=move) only with --yes. Failed items get a
/// suggested queue-rm+re-grab command instead.
pub fn cmd_stuck(svc: &str, args: &[String]) {
    let (flags, rest) = pop_flags(
        args,
        &[("--json", 0), ("--quiet", 0), ("--fix", 0), ("--yes", 0)],
    );
    let pat: Option<String> = rest.first().map(|s| s.to_lowercase());
    let q = crate::browse::queue_records(svc, 1000);
    let mut rows: Vec<Value> = Vec::new();
    for r in q.a("records") {
        if let Some(p) = &pat {
            if !r.s("title").to_lowercase().contains(p.as_str()) {
                continue;
            }
        }
        if crate::browse::is_stuck_queue_record(r) {
            rows.push(r.clone());
        }
    }
    if flags.has("--json") {
        let summaries: Vec<Value> = rows
            .iter()
            .map(|r| crate::browse::queue_record_summary(r))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&Value::Array(summaries)).unwrap_or_else(|_| "[]".into())
        );
        return;
    }
    let matching = if pat.is_some() {
        format!(" matching '{}'", rest[0])
    } else {
        String::new()
    };
    if !rows.is_empty() {
        println!("stuck: {} {} queue item(s){}", rows.len(), svc, matching);
    } else if !flags.has("--quiet") {
        println!("stuck: 0 {} queue item(s){}", svc, matching);
    }
    for r in &rows {
        println!(
            "  {}/{}  {}MB left  {}",
            ps(r, "status"),
            ps(r, "trackedDownloadState"),
            mb(r.i("sizeleft")),
            ps(r, "title")
        );
        if truthy_key(r, "errorMessage") {
            println!("        err: {}", r.s("errorMessage"));
        }
        let msgs = crate::browse::queue_status_messages(r);
        if !msgs.is_empty() {
            println!("        {}", msgs.join("; "));
        }
    }
    if !flags.has("--fix") || rows.is_empty() {
        return;
    }
    let go = flags.has("--yes");
    println!(
        "\nfix plan{}:",
        if go { "" } else { " [dry-run — pass --yes to apply]" }
    );
    let mut cleared_dl: HashSet<String> = HashSet::new();
    for r in &rows {
        let title = trunc(r.s("title"), 64);
        let state = ps(r, "trackedDownloadState");
        if r.s("status") == "failed" {
            println!("  failed: {}", title);
            println!(
                "    -> arr {} queue-rm {} --blocklist --yes   then re-grab",
                svc,
                ps(r, "id")
            );
            continue;
        }
        let state_raw = r.s("trackedDownloadState");
        if state_raw != "importBlocked" && state_raw != "importPending" {
            println!("  {}: {} — no automated fix, inspect manually", state, title);
            continue;
        }
        // already satisfied? (episode/movie has a file — e.g. we just imported a
        // sibling record of the same season pack, or a better release landed)
        // NB the episode/movie objects EMBEDDED in queue records are stale
        // snapshots — query live state.
        let mut satisfied = false;
        if svc.starts_with("sonarr") && r.i("episodeId") != 0 {
            satisfied = api(svc, "GET", &format!("/episode/{}", r.i("episodeId")), None)
                .map_or(false, |e| e.b("hasFile"));
        } else if svc == "radarr" {
            let mid = movie_id_of(r);
            if mid != 0 {
                satisfied = api(svc, "GET", &format!("/movie/{}", mid), None)
                    .map_or(false, |m| m.b("hasFile"));
            }
        }
        if satisfied {
            if go {
                let dl = r.s("downloadId").to_string();
                let rm_client = if cleared_dl.contains(&dl) { "false" } else { "true" };
                cleared_dl.insert(dl);
                api(
                    svc,
                    "DELETE",
                    &format!(
                        "/queue/{}?removeFromClient={}&blocklist=false",
                        r.i("id"),
                        rm_client
                    ),
                    None,
                );
                println!(
                    "  satisfied: {} — already on disk; cleared stale queue record",
                    title
                );
            } else {
                println!(
                    "  satisfied: {} — already on disk; would clear stale queue record",
                    title
                );
            }
            continue;
        }
        let out = r.s("outputPath").to_string();
        if out.is_empty() {
            println!("  {}: {} — queue record has no outputPath; cannot map", state, title);
            continue;
        }
        let item: Value;
        let payload: Vec<Value>;
        let skipped: Vec<String>;
        if svc.starts_with("sonarr") {
            let sid = series_id_of(r);
            if sid == 0 {
                println!(
                    "  {}: {} — not linked to a series; try `arr {} import '{}' --series <query>`",
                    state, title, svc, out
                );
                continue;
            }
            item = api(svc, "GET", &format!("/series/{}", sid), None).unwrap_or(Value::Null);
            let (p, s) = plan_series_import(svc, &out, sid, "", "auto", None);
            payload = p;
            skipped = s;
        } else {
            let mid = movie_id_of(r);
            if mid == 0 {
                println!(
                    "  {}: {} — not linked to a movie; try `arr radarr import '{}' --movie <query>`",
                    state, title, out
                );
                continue;
            }
            item = api(svc, "GET", &format!("/movie/{}", mid), None).unwrap_or(Value::Null);
            let (p, s) = plan_movie_import(&out, mid, "");
            payload = p;
            skipped = s;
        }
        if payload.is_empty() {
            let gone = match std::fs::metadata(&out) {
                Ok(_) => false,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                Err(_) => false, // permission-restricted view — can't tell, stay neutral
            };
            if gone {
                println!("  {}: {} — download folder is GONE (stale queue record)", state, title);
                println!("    -> arr {} queue-rm {} --yes", svc, ps(r, "id"));
            } else if !skipped.is_empty()
                && skipped.iter().all(|s| s.ends_with("(not a video file)"))
            {
                println!("  {}: {} — only non-video files (junk/malware release)", state, title);
                println!(
                    "    -> arr {} queue-rm {} --blocklist --yes   then re-grab",
                    svc,
                    ps(r, "id")
                );
            } else {
                println!(
                    "  {}: {} — no mappable files in {} ({} skipped)",
                    state,
                    title,
                    out,
                    skipped.len()
                );
            }
            continue;
        }
        println!("  {} -> {}: {} file(s):", title, ps(&item, "title"), payload.len());
        for p in &payload {
            println!("    {}", p.s("_label"));
        }
        if !skipped.is_empty() {
            println!("    (skipped {} unmatched)", skipped.len());
        }
        if go {
            prepare_target_dir(&item);
            run_manual_import(svc, &payload, "move", 10, true, 300);
        }
    }
}

/// Poll /command/<id> until it leaves queued/started; return the final record.
fn wait_command(svc: &str, cmd_id: i64, timeout: i64) -> Value {
    let start = Instant::now();
    loop {
        let rec = api(svc, "GET", &format!("/command/{}", cmd_id), None).unwrap_or(Value::Null);
        let st = rec.s("status");
        if st != "queued" && st != "started" {
            return rec;
        }
        if start.elapsed().as_secs_f64() > timeout as f64 {
            return rec; // caller inspects .status (still queued/started == timed out)
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

// --- queue-rm / queue overview -----------------------------------------------

/// Remove queue record(s) by id or title pattern — the API way, no raw curl.
///
/// arr <svc> queue-rm <id…|pattern> [--disc|--encrypted|--quality NAME|--status STATE]
///     [--blocklist] [--keep-files] [--research] [--yes]
/// Dry-run unless --yes.
pub fn cmd_queue_rm(svc: &str, args: &[String]) {
    let (flags, rest) = pop_flags(
        args,
        &[
            ("--blocklist", 0),
            ("--keep-files", 0),
            ("--yes", 0),
            ("--disc", 0),
            ("--quality", 1),
            ("--status", 1),
            ("--title", 1),
            ("--research", 0),
            ("--encrypted", 0),
        ],
    );
    let idkey = if svc.starts_with("sonarr") { "seriesId" } else { "movieId" };
    let selectors = flags.has("--disc")
        || flags.has("--encrypted")
        || flag_truthy(&flags, "--quality").is_some()
        || flag_truthy(&flags, "--status").is_some()
        || flag_truthy(&flags, "--title").is_some();
    let numeric_only = !rest.is_empty()
        && rest
            .iter()
            .all(|x| !x.is_empty() && x.chars().all(|c| c.is_ascii_digit()));
    if rest.is_empty() && !selectors {
        die("queue-rm: need a queue id, title pattern, or selector (--disc/--quality/--status)");
    }
    struct Target {
        id: i64,
        title: String,
        item: Option<i64>,
    }
    let mut targets: Vec<Target> = Vec::new();
    // Light path: bare ids, no selector, no re-search — no need to pull the queue.
    if numeric_only && !selectors && !flags.has("--research") {
        for x in &rest {
            targets.push(Target {
                id: py_int(x),
                title: format!("(queue id {})", x),
                item: None,
            });
        }
    } else {
        let q = crate::browse::queue_records(svc, 1000);
        let mut sel = queue_select(q.a("records"), &flags, &rest);
        if sel.is_empty() {
            die("queue-rm: nothing matched");
        }
        if !flags.has("--keep-files") {
            // A season pack is one download fanned out into per-episode queue
            // records. Removing from the client takes the whole pack out on the
            // first record — deleting the rest would just 404 on stale ids.
            let mut seen_dl: HashSet<String> = HashSet::new();
            let mut dedup: Vec<Value> = Vec::new();
            for r in sel {
                let dl = r.s("downloadId").to_string();
                if !dl.is_empty() && seen_dl.contains(&dl) {
                    continue;
                }
                if !dl.is_empty() {
                    seen_dl.insert(dl);
                }
                dedup.push(r);
            }
            sel = dedup;
        }
        for r in &sel {
            let item = match r.get(idkey) {
                Some(v) if !v.is_null() => v.as_i64().filter(|i| *i != 0),
                _ => None,
            };
            targets.push(Target {
                id: r.i("id"),
                title: r.s("title").to_string(),
                item,
            });
        }
    }
    let rm_client = if flags.has("--keep-files") { "false" } else { "true" };
    let blocklist = if flags.has("--blocklist") { "true" } else { "false" };
    let research = flags.has("--research");
    let go = flags.has("--yes");
    println!(
        "{}queue-rm {} item(s) (removeFromClient={}, blocklist={}{}):",
        if go { "" } else { "[dry-run] " },
        targets.len(),
        rm_client,
        blocklist,
        if research { ", re-search" } else { "" }
    );
    for t in targets.iter().take(40) {
        println!("  {:<10} {}", t.id, trunc(&t.title, 70));
    }
    if targets.len() > 40 {
        println!("  … and {} more", targets.len() - 40);
    }
    if !go {
        println!("  (pass --yes to remove)");
        return;
    }
    let mut failed = 0;
    let mut affected: HashSet<i64> = HashSet::new();
    for t in &targets {
        let path = format!(
            "/queue/{}?removeFromClient={}&blocklist={}",
            t.id, rm_client, blocklist
        );
        for attempt in 1..=2 {
            match try_api(svc, "DELETE", &path, None, 120) {
                Ok(_) => {
                    println!("  removed: {}", trunc(&t.title, 70));
                    if let Some(i) = t.item {
                        affected.insert(i);
                    }
                    break;
                }
                // 500 database-locked under load / stale id (404)
                Err(e) => {
                    eprintln!("arr: {}", api_err_msg("DELETE", &path, 120, &e));
                    if attempt == 1 {
                        std::thread::sleep(Duration::from_secs(5));
                        continue;
                    }
                    failed += 1;
                    println!(
                        "  FAILED (kept): {} — retry once the arr settles",
                        trunc(&t.title, 70)
                    );
                }
            }
        }
    }
    if research && !affected.is_empty() {
        research_items(svc, &affected);
        println!(
            "re-search queued for {} {}",
            affected.len(),
            if svc.starts_with("sonarr") { "series" } else { "movie(s)" }
        );
    }
    if failed > 0 {
        std::process::exit(1);
    }
}

/// A raw-disc queue record — full Blu-ray/DVD structure (ISO/BDMV/VIDEO_TS).
/// The arr classifies these as BR-DISK; they neither import nor feed the
/// encoder, so a queue full of them is wasted bytes. Quality name is
/// authoritative; the title tokens catch anything the classifier misses.
pub(crate) fn is_disc_record(r: &Value) -> bool {
    let qname = r.at(&["quality", "quality", "name"]).as_str().unwrap_or("");
    if qname == "BR-DISK" || qname == "Raw-HD" {
        return true;
    }
    let t = r.s("title").to_lowercase();
    [
        "bdmv",
        ".iso",
        " iso",
        "video_ts",
        "complete.bluray",
        "complete.uhd.bluray",
        "full.bluray",
        "complete blu-ray",
    ]
    .iter()
    .any(|tok| t.contains(tok))
}

/// SAB's own per-job diagnosis: {nzo_id (lowercase): [labels]}. The labels SAB
/// sets are the actionable ones the arrs can't see — ENCRYPTED (password-
/// protected rar = fake release, job sits Paused forever), DUPLICATE,
/// ALTERNATIVE. Keyed lowercase because the arrs store downloadId in whatever
/// case they please. Survives SAB being down (Python catches the SystemExit
/// after die() printed its message — we print the same shape and return {}).
pub(crate) fn sab_flagged_labels() -> HashMap<String, Vec<String>> {
    let v = match sab_try_get("queue", &[("start", "0"), ("limit", "100000")]) {
        Ok(v) => v,
        Err(reason) => {
            eprintln!("arr: sab queue -> {}", reason);
            return HashMap::new();
        }
    };
    let mut out = HashMap::new();
    if let Some(slots) = v.at(&["queue", "slots"]).as_array() {
        for s in slots {
            out.insert(
                s.s("nzo_id").to_lowercase(),
                s.a("labels")
                    .iter()
                    .map(|l| l.as_str().unwrap_or("").to_string())
                    .collect(),
            );
        }
    }
    out
}

/// Filter queue records by any mix of selectors — the shared language of
/// `queue` (view) and `queue-rm` (act). positional: numeric queue ids (exact)
/// or a title substring (back-compat). flags: --title PAT, --disc, --quality
/// NAME, --status STATE (failed|stuck|downloading|paused|importing, or a raw
/// status).
pub(crate) fn queue_select(records: &[Value], flags: &Flags, positional: &[String]) -> Vec<Value> {
    let mut sel: Vec<Value> = records.to_vec();
    let ids: HashSet<i64> = positional
        .iter()
        .filter(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        .filter_map(|p| p.parse().ok())
        .collect();
    let words: Vec<&str> = positional
        .iter()
        .filter(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()))
        .map(|p| p.as_str())
        .collect();
    if !ids.is_empty() {
        sel.retain(|r| ids.contains(&r.i("id")));
    }
    let title_pat = match flag_truthy(flags, "--title") {
        Some(t) => Some(t.to_string()),
        None if !words.is_empty() => Some(words.join(" ")),
        None => None,
    };
    if let Some(tp) = title_pat.filter(|t| !t.is_empty()) {
        let tl = tp.to_lowercase();
        sel.retain(|r| r.s("title").to_lowercase().contains(&tl));
    }
    if flags.has("--disc") {
        sel.retain(is_disc_record);
    }
    if flags.has("--encrypted") {
        let labels = sab_flagged_labels();
        sel.retain(|r| {
            labels
                .get(&r.s("downloadId").to_lowercase())
                .map_or(false, |l| l.iter().any(|x| x == "ENCRYPTED"))
        });
    }
    if let Some(q) = flag_truthy(flags, "--quality") {
        let qn = q.to_lowercase();
        sel.retain(|r| {
            r.at(&["quality", "quality", "name"])
                .as_str()
                .unwrap_or("")
                .to_lowercase()
                == qn
        });
    }
    if let Some(st) = flag_truthy(flags, "--status") {
        let st = st.to_lowercase();
        sel.retain(|r| match st.as_str() {
            "failed" => r.s("status") == "failed",
            "stuck" => crate::browse::is_stuck_queue_record(r),
            "downloading" => r.s("status") == "downloading",
            "paused" => r.s("status") == "paused",
            "importing" => {
                let s = r.s("trackedDownloadState");
                s == "importPending" || s == "importBlocked"
            }
            _ => r.s("status").to_lowercase() == st,
        });
    }
    sel
}

/// Trigger a fresh search for the given movie/series ids (after removing a bad
/// grab, so the arr picks an importable replacement).
fn research_items(svc: &str, item_ids: &HashSet<i64>) {
    let ids: Vec<i64> = item_ids.iter().copied().filter(|i| *i != 0).collect();
    if ids.is_empty() {
        return;
    }
    if svc.starts_with("sonarr") {
        for iid in &ids {
            api(
                svc,
                "POST",
                "/command",
                Some(&json!({"name": "SeriesSearch", "seriesId": iid})),
            );
        }
    } else {
        api(
            "radarr",
            "POST",
            "/command",
            Some(&json!({"name": "MoviesSearch", "movieIds": ids})),
        );
    }
}

/// os.statvfs semantics: (f_bavail * f_frsize, f_blocks * f_frsize).
fn disk_free_bytes(path: &str) -> Option<(u64, u64)> {
    let c = std::ffi::CString::new(path).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some((
        st.f_bavail as u64 * st.f_frsize as u64,
        st.f_blocks as u64 * st.f_frsize as u64,
    ))
}

/// Python float(x or 0) over a JSON value; Err mirrors ValueError.
fn py_float(v: Option<&Value>) -> Result<f64, ()> {
    match v {
        None | Some(Value::Null) => Ok(0.0),
        Some(Value::Bool(b)) => Ok(if *b { 1.0 } else { 0.0 }),
        Some(Value::Number(n)) => Ok(n.as_f64().unwrap_or(0.0)),
        Some(Value::String(s)) => {
            if s.is_empty() {
                Ok(0.0) // falsy -> float(0)
            } else {
                s.trim().parse().map_err(|_| ())
            }
        }
        Some(_) => Err(()),
    }
}

/// Insertion-ordered counter (Python dict semantics — the text output sorts by
/// -count with stable ties in first-seen order).
fn bump(counts: &mut Vec<(String, i64)>, key: &str) {
    if let Some(e) = counts.iter_mut().find(|(k, _)| k == key) {
        e.1 += 1;
    } else {
        counts.push((key.to_string(), 1));
    }
}

fn counts_json(counts: &[(String, i64)]) -> Value {
    let mut m = Map::new();
    for (k, v) in counts {
        m.insert(k.clone(), json!(v));
    }
    Value::Object(m)
}

fn counts_line(counts: &[(String, i64)]) -> String {
    let mut sorted = counts.to_vec();
    sorted.sort_by_key(|(_, v)| std::cmp::Reverse(*v));
    sorted
        .iter()
        .map(|(k, v)| format!("{} {}", k, v))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Top-level `arr queue` — one rollup across SAB + every arr queue: total
/// jobs, TB remaining, ETA, per-category counts, a quality histogram that
/// flags raw-disc releases (won't import), and free space vs. what's left to
/// download. The whole-picture answer to "what's the queue looking like?".
pub fn cmd_queue_overview(args: &[String]) {
    let (flags, _) = pop_flags(args, &[("--json", 0)]);
    let sab = sab_api("queue", &[("start", "0"), ("limit", "100000")], 120);
    let sabq = match sab.get("queue") {
        Some(v) if !v.is_null() => v.clone(),
        _ => json!({}),
    };
    let slots = sabq.a("slots");
    let mut cat_counts: Vec<(String, i64)> = Vec::new();
    let mut state_counts: Vec<(String, i64)> = Vec::new();
    for s in slots {
        let cat = s.s("cat");
        bump(&mut cat_counts, if cat.is_empty() { "?" } else { cat });
        let st = s.s("status");
        bump(&mut state_counts, if st.is_empty() { "?" } else { st });
    }
    let (mbleft, _mbtot) = match (py_float(sabq.get("mbleft")), py_float(sabq.get("mb"))) {
        (Ok(a), Ok(b)) => (a, b),
        _ => (0.0, 0.0),
    };
    let tb_left = (mbleft / 1048576.0 * 100.0).round_ties_even() / 100.0;
    // len(slots) is authoritative (paused included) — noofslots_total under-counts.
    let noof = match sabq.get("noofslots_total") {
        Some(Value::Number(n)) => n.as_i64().unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
        Some(Value::String(s)) if !s.is_empty() => py_int(s),
        _ => 0,
    };
    let total_jobs = (slots.len() as i64).max(noof);

    // SAB-flagged encrypted jobs: password-protected rars = fake releases. SAB
    // pauses them and they sit forever unless someone acts.
    let enc_ids: HashSet<String> = slots
        .iter()
        .filter(|s| s.a("labels").iter().any(|l| l.as_str() == Some("ENCRYPTED")))
        .map(|s| s.s("nzo_id").to_lowercase())
        .collect();

    // quality histogram + disc/encrypted tally across the arr queues (SAB slots
    // carry no quality; the arrs know which movie/series a job belongs to)
    let mut qual: Vec<(String, i64)> = Vec::new();
    let mut disc_by_svc: Vec<(&str, i64)> = Vec::new();
    let mut enc_by_svc: Vec<(&str, i64)> = Vec::new();
    for qsvc in ["radarr", "sonarr", "sonarr-anime"] {
        let recs = match queue_records_caught(qsvc, 2000) {
            Ok(r) => r,
            Err(()) => continue,
        };
        let mut d = 0;
        let mut e = 0;
        for r in &recs {
            let name = r.at(&["quality", "quality", "name"]).as_str().unwrap_or("");
            bump(&mut qual, if name.is_empty() { "?" } else { name });
            if is_disc_record(r) {
                d += 1;
            }
            if enc_ids.contains(&r.s("downloadId").to_lowercase()) {
                e += 1;
            }
        }
        if d > 0 {
            disc_by_svc.push((qsvc, d));
        }
        if e > 0 {
            enc_by_svc.push((qsvc, e));
        }
    }
    let disc_total: i64 = disc_by_svc.iter().map(|(_, v)| v).sum();
    let enc_total: i64 = enc_by_svc.iter().map(|(_, v)| v).sum();
    let free_b = disk_free_bytes("/data").map(|(free, _)| free);

    if flags.has("--json") {
        let mut obj = Map::new();
        obj.insert("totalJobs".into(), json!(total_jobs));
        obj.insert(
            "speed".into(),
            sabq.get("speed").cloned().unwrap_or(Value::Null),
        );
        obj.insert("tbLeft".into(), json!(tb_left));
        obj.insert(
            "eta".into(),
            sabq.get("timeleft").cloned().unwrap_or(Value::Null),
        );
        obj.insert(
            "paused".into(),
            sabq.get("paused").cloned().unwrap_or(Value::Null),
        );
        obj.insert("byCategory".into(), counts_json(&cat_counts));
        obj.insert("byState".into(), counts_json(&state_counts));
        obj.insert("byQuality".into(), counts_json(&qual));
        let mut disc_m = Map::new();
        for (k, v) in &disc_by_svc {
            disc_m.insert(k.to_string(), json!(v));
        }
        obj.insert("discItems".into(), Value::Object(disc_m));
        obj.insert("discTotal".into(), json!(disc_total));
        let mut enc_m = Map::new();
        for (k, v) in &enc_by_svc {
            enc_m.insert(k.to_string(), json!(v));
        }
        obj.insert("encryptedItems".into(), Value::Object(enc_m));
        obj.insert("encryptedTotal".into(), json!(enc_total));
        obj.insert(
            "freeGB".into(),
            free_b.map(|f| json!(gb(f as i64))).unwrap_or(Value::Null),
        );
        println!(
            "{}",
            serde_json::to_string_pretty(&Value::Object(obj)).unwrap_or_else(|_| "{}".into())
        );
        return;
    }

    let left_str = if tb_left >= 1.0 {
        format!("{:.2} TiB", tb_left)
    } else {
        format!("{} GiB", (mbleft / 1024.0).round_ties_even() as i64)
    };
    let speed = sabq.s("speed");
    println!(
        "queue: {} job(s) in SABnzbd — {} left, {} @ {}B/s, ETA {}",
        total_jobs,
        left_str,
        if !truthy_key(&sabq, "paused") { "downloading" } else { "PAUSED" },
        if speed.is_empty() { "?" } else { speed },
        {
            let tl = sabq.s("timeleft");
            if tl.is_empty() { "?" } else { tl }
        }
    );
    if !cat_counts.is_empty() {
        println!("  by category: {}", counts_line(&cat_counts));
    }
    if !state_counts.is_empty() {
        println!("  by state:    {}", counts_line(&state_counts));
    }
    if !qual.is_empty() {
        println!("  by quality:  {}", counts_line(&qual));
    }
    if let Some(free) = free_b {
        let head = free as i64 - (mbleft * 1048576.0) as i64;
        let warn = if head < 0 { "  ⚠ less than what's left to download" } else { "" };
        println!(
            "  /data free:  {}GB (vs {:.2} TiB queued){}",
            fmt_gb(free as i64),
            tb_left,
            warn
        );
    }
    if disc_total > 0 {
        let hits = disc_by_svc
            .iter()
            .map(|(k, v)| format!("{} {}", k, v))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "  ⚠ {} raw-disc item(s) ({}) — these won't import or re-encode; clear with `arr <svc> queue-rm --disc --blocklist --research --yes`",
            disc_total, hits
        );
    }
    if enc_total > 0 {
        let hits = enc_by_svc
            .iter()
            .map(|(k, v)| format!("{} {}", k, v))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "  ⚠ {} ENCRYPTED job(s) ({}) — password-protected rar = fake release, SAB has them paused forever; clear with `arr <svc> queue-rm --encrypted --blocklist --research --yes`",
            enc_total, hits
        );
    }
}

// --- force-import (ManualImport by parsed episode number) --------------------

enum EpRef {
    Se(i64, i64),
    Abs(i64),
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// \b before cs[i] (which must itself be a word char).
fn at_word_start(cs: &[char], i: usize) -> bool {
    (i == 0 || !is_word(cs[i - 1])) && i < cs.len() && is_word(cs[i])
}

/// \b at position e where cs[e-1] is a word char.
fn boundary_after(cs: &[char], e: usize) -> bool {
    e > 0 && is_word(cs[e - 1]) && (e >= cs.len() || !is_word(cs[e]))
}

fn parse_digits(cs: &[char]) -> i64 {
    cs.iter().collect::<String>().parse().unwrap_or(0)
}

/// re.sub(r"\b\d{3,4}p\b", " ", ...) — 480p/720p/1080p resolution tokens.
fn sub_resolution(s: &str) -> String {
    let cs: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < cs.len() {
        if cs[i].is_ascii_digit() && at_word_start(&cs, i) {
            let mut d = 0;
            while i + d < cs.len() && cs[i + d].is_ascii_digit() {
                d += 1;
            }
            if (d == 3 || d == 4)
                && i + d < cs.len()
                && (cs[i + d] == 'p' || cs[i + d] == 'P')
                && boundary_after(&cs, i + d + 1)
            {
                out.push(' ');
                i += d + 1;
                continue;
            }
        }
        out.push(cs[i]);
        i += 1;
    }
    out
}

/// re.sub(r"\[[0-9A-Fa-f]{6,8}\]", " ", ...) — crc hashes.
fn sub_crc(s: &str) -> String {
    let cs: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < cs.len() {
        if cs[i] == '[' {
            let mut h = 0;
            while i + 1 + h < cs.len() && cs[i + 1 + h].is_ascii_hexdigit() {
                h += 1;
            }
            if (6..=8).contains(&h) && i + 1 + h < cs.len() && cs[i + 1 + h] == ']' {
                out.push(' ');
                i += h + 2;
                continue;
            }
        }
        out.push(cs[i]);
        i += 1;
    }
    out
}

/// re.sub(r"\b(?:[xh]\.?26[45]|HEVC|AAC|MP3|XviD|FLAC|BD|WEB)\b", " ", ..., re.I)
/// NB: require the x/h codec prefix — a bare \b26[45]\b also matches real
/// episode numbers (Sgt. Frog abs 264/265 were unmappable until 2026-07-21)
fn sub_codecs(s: &str) -> String {
    let cs: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    'outer: while i < cs.len() {
        if at_word_start(&cs, i) {
            let lc = cs[i].to_ascii_lowercase();
            // [xh]\.?26[45]
            if lc == 'x' || lc == 'h' {
                let mut j = i + 1;
                if j < cs.len() && cs[j] == '.' {
                    j += 1;
                }
                if j + 3 <= cs.len()
                    && cs[j] == '2'
                    && cs[j + 1] == '6'
                    && (cs[j + 2] == '4' || cs[j + 2] == '5')
                    && boundary_after(&cs, j + 3)
                {
                    out.push(' ');
                    i = j + 3;
                    continue;
                }
            }
            for tok in ["hevc", "aac", "mp3", "xvid", "flac", "bd", "web"] {
                let l = tok.len();
                if i + l <= cs.len()
                    && cs[i..i + l]
                        .iter()
                        .map(|c| c.to_ascii_lowercase())
                        .eq(tok.chars())
                    && boundary_after(&cs, i + l)
                {
                    out.push(' ');
                    i += l;
                    continue 'outer;
                }
            }
        }
        out.push(cs[i]);
        i += 1;
    }
    out
}

/// re.search(r"S(\d{1,2})\s*E(\d{1,3})", ..., re.I)
fn find_se(cs: &[char]) -> Option<(i64, i64)> {
    for i in 0..cs.len() {
        if cs[i] != 's' && cs[i] != 'S' {
            continue;
        }
        for t in [2usize, 1] {
            // greedy season digits, backtracking 2 -> 1
            let d_end = i + 1 + t;
            if d_end > cs.len() || !cs[i + 1..d_end].iter().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let mut j = d_end;
            while j < cs.len() && cs[j].is_whitespace() {
                j += 1;
            }
            if j < cs.len() && (cs[j] == 'e' || cs[j] == 'E') {
                let mut e_len = 0;
                while e_len < 3 && j + 1 + e_len < cs.len() && cs[j + 1 + e_len].is_ascii_digit() {
                    e_len += 1;
                }
                if e_len >= 1 {
                    return Some((
                        parse_digits(&cs[i + 1..d_end]),
                        parse_digits(&cs[j + 1..j + 1 + e_len]),
                    ));
                }
            }
        }
    }
    None
}

/// re.search(r"episode[ ._-]*0*(\d{1,3})", ..., re.I)
fn find_episode_kw(cs: &[char]) -> Option<i64> {
    for i in 0..cs.len() {
        if i + 7 > cs.len()
            || !cs[i..i + 7]
                .iter()
                .map(|c| c.to_ascii_lowercase())
                .eq("episode".chars())
        {
            continue;
        }
        let mut j = i + 7;
        while j < cs.len() && matches!(cs[j], ' ' | '.' | '_' | '-') {
            j += 1;
        }
        let mut d = 0;
        while j + d < cs.len() && cs[j + d].is_ascii_digit() {
            d += 1;
        }
        if d == 0 {
            continue;
        }
        let run = &cs[j..j + d];
        let z = run.iter().take_while(|c| **c == '0').count();
        return Some(if z == d {
            0
        } else {
            parse_digits(&run[z..z + (d - z).min(3)])
        });
    }
    None
}

/// re.search(r"[-–][ ._]*0*(\d{1,3})(?:v\d)?\b", ...) — "- 104"
fn find_dash_num(cs: &[char]) -> Option<i64> {
    for i in 0..cs.len() {
        if cs[i] != '-' && cs[i] != '–' {
            continue;
        }
        let mut j = i + 1;
        while j < cs.len() && matches!(cs[j], ' ' | '.' | '_') {
            j += 1;
        }
        let mut d = 0;
        while j + d < cs.len() && cs[j + d].is_ascii_digit() {
            d += 1;
        }
        if d == 0 {
            continue;
        }
        let max_z = cs[j..j + d].iter().take_while(|c| **c == '0').count();
        // regex backtracking order: 0* greedy, then \d{1,3} greedy, then the
        // optional v\d (greedy), then \b — first success wins.
        for zc in (0..=max_z).rev() {
            let avail = d - zc;
            if avail == 0 {
                continue;
            }
            for t in (1..=avail.min(3)).rev() {
                let end = j + zc + t;
                if end + 2 <= cs.len()
                    && cs[end] == 'v'
                    && cs[end + 1].is_ascii_digit()
                    && boundary_after(cs, end + 2)
                {
                    return Some(parse_digits(&cs[j + zc..end]));
                }
                if boundary_after(cs, end) {
                    return Some(parse_digits(&cs[j + zc..end]));
                }
            }
        }
    }
    None
}

/// re.findall(r"\b0*(\d{1,3})\b", ...)[-1] — last standalone 1-3 digit number.
fn find_last_num(cs: &[char]) -> Option<i64> {
    let mut result = None;
    let mut i = 0;
    while i < cs.len() {
        if cs[i].is_ascii_digit() && (i == 0 || !is_word(cs[i - 1])) {
            let mut d = 0;
            while i + d < cs.len() && cs[i + d].is_ascii_digit() {
                d += 1;
            }
            if i + d >= cs.len() || !is_word(cs[i + d]) {
                let z = cs[i..i + d].iter().take_while(|c| **c == '0').count();
                if z == d {
                    result = Some(0);
                } else if d - z <= 3 {
                    result = Some(parse_digits(&cs[i + z..i + d]));
                }
            }
            i += d;
        } else {
            i += 1;
        }
    }
    result
}

/// Parse an episode reference from a filename.
/// Returns Se(season, ep) | Abs(n) | None.
fn episode_number(name: &str) -> Option<EpRef> {
    let mut base = name.to_string();
    for ext in [".mkv", ".mp4", ".avi", ".m4v"] {
        if base.to_lowercase().ends_with(ext) {
            base.truncate(base.len() - ext.len());
            break;
        }
    }
    let base = sub_resolution(&base); // 480p/720p/1080p
    let base = sub_crc(&base); // crc hashes
    let base = sub_codecs(&base);
    let cs: Vec<char> = base.chars().collect();
    if let Some((s, e)) = find_se(&cs) {
        return Some(EpRef::Se(s, e));
    }
    if let Some(n) = find_episode_kw(&cs) {
        return Some(EpRef::Abs(n));
    }
    if let Some(n) = find_dash_num(&cs) {
        return Some(EpRef::Abs(n));
    }
    find_last_num(&cs).map(EpRef::Abs) // fallback: last 1-3 digit
}

fn choose_ep<'a>(
    name: &str,
    mode: &str,
    season: Option<i64>,
    by_se: &'a HashMap<(i64, i64), Value>,
    by_abs: &'a HashMap<i64, Value>,
) -> Option<&'a Value> {
    let info = episode_number(name)?;
    match info {
        EpRef::Se(s, e) => {
            if mode == "auto" || mode == "se" {
                if let Some(ep) = by_se.get(&(s, e)) {
                    return Some(ep);
                }
            }
            if mode == "auto" || mode == "abs" {
                // treat ep-part as absolute
                if let Some(ep) = by_abs.get(&e) {
                    return Some(ep);
                }
            }
            None
        }
        EpRef::Abs(n) => {
            if mode != "abs" {
                if let Some(sn) = season {
                    if let Some(ep) = by_se.get(&(sn, n)) {
                        return Some(ep);
                    }
                }
            }
            if mode != "se" {
                if let Some(ep) = by_abs.get(&n) {
                    return Some(ep);
                }
            }
            if let Some(sn) = season {
                if let Some(ep) = by_se.get(&(sn, n)) {
                    return Some(ep);
                }
            }
            None
        }
    }
}

const VIDEO_EXT: [&str; 9] = [
    ".mkv", ".mp4", ".avi", ".m4v", ".ts", ".webm", ".wmv", ".mpg", ".mpeg",
];

fn manual_import_files(svc: &str, folder: &str) -> Option<Value> {
    api_t(
        svc,
        "GET",
        &format!(
            "/manualimport?folder={}&filterExistingFiles=false",
            urlquote(folder)
        ),
        None,
        SEARCH_TIMEOUT,
    )
}

fn import_file_name(f: &Value) -> String {
    let rp = if !f.s("relativePath").is_empty() {
        f.s("relativePath")
    } else {
        f.s("path")
    };
    rp.rsplit('/').next().unwrap_or("").to_string()
}

fn languages_of(f: &Value) -> Value {
    if truthy_key(f, "languages") {
        f.get("languages").cloned().unwrap_or(Value::Null)
    } else {
        json!([{"id": 1, "name": "English"}])
    }
}

/// Map importable files in <folder> onto a series' episodes. Returns
/// (payload, skipped) — payload rows carry a human '_label' key.
/// Non-video files (.exe/.scr malware droppers in fake releases) are never
/// mapped, even if the arr's manualimport endpoint lists them.
fn plan_series_import(
    svc: &str,
    folder: &str,
    sid: i64,
    mat: &str,
    mapmode: &str,
    season: Option<i64>,
) -> (Vec<Value>, Vec<String>) {
    let files = manual_import_files(svc, folder);
    let eps = api(svc, "GET", &format!("/episode?seriesId={}", sid), None);
    let mut by_se: HashMap<(i64, i64), Value> = HashMap::new();
    let mut by_abs: HashMap<i64, Value> = HashMap::new();
    for e in items(&eps) {
        by_se.insert((e.i("seasonNumber"), e.i("episodeNumber")), e.clone());
        if e.i("absoluteEpisodeNumber") != 0 {
            by_abs.insert(e.i("absoluteEpisodeNumber"), e.clone());
        }
    }
    let mut payload: Vec<Value> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for f in items(&files) {
        let name = import_file_name(f);
        if !mat.is_empty() && !name.to_lowercase().contains(mat) {
            continue;
        }
        if !VIDEO_EXT.iter().any(|e| name.to_lowercase().ends_with(e)) {
            skipped.push(format!("{} (not a video file)", name));
            continue;
        }
        let ep = choose_ep(&name, mapmode, season, &by_se, &by_abs);
        if ep.is_none() || !truthy_key(f, "quality") {
            skipped.push(name);
            continue;
        }
        let ep = ep.unwrap();
        payload.push(json!({
            "path": f.get("path").cloned().unwrap_or(Value::Null),
            "seriesId": sid,
            "episodeIds": [ep.i("id")],
            "quality": f.get("quality").cloned().unwrap_or(Value::Null),
            "languages": languages_of(f),
            "releaseGroup": f.get("releaseGroup").cloned().unwrap_or(json!("")),
            "_label": format!("{}  ->  S{}E{}", name, ep.i("seasonNumber"), ep.i("episodeNumber")),
        }));
    }
    (payload, skipped)
}

fn plan_movie_import(folder: &str, mid: i64, mat: &str) -> (Vec<Value>, Vec<String>) {
    let files = manual_import_files("radarr", folder);
    let mut payload: Vec<Value> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for f in items(&files) {
        let name = import_file_name(f);
        if !mat.is_empty() && !name.to_lowercase().contains(mat) {
            continue;
        }
        if !VIDEO_EXT.iter().any(|e| name.to_lowercase().ends_with(e)) {
            skipped.push(format!("{} (not a video file)", name));
            continue;
        }
        if !truthy_key(f, "quality") {
            skipped.push(name);
            continue;
        }
        payload.push(json!({
            "path": f.get("path").cloned().unwrap_or(Value::Null),
            "movieId": mid,
            "quality": f.get("quality").cloned().unwrap_or(Value::Null),
            "languages": languages_of(f),
            "releaseGroup": f.get("releaseGroup").cloned().unwrap_or(json!("")),
            "_label": name,
        }));
    }
    (payload, skipped)
}

/// POST ManualImport in chunks of <=batch files. Big single imports were
/// timing out (9+ episode batches hit the 120-300s wall); chunking + a wait
/// between chunks keeps each command small and reports per-chunk results.
fn run_manual_import(svc: &str, payload: &[Value], impmode: &str, batch: i64, wait: bool, timeout: i64) {
    let clean: Vec<Value> = payload
        .iter()
        .map(|p| {
            if let Value::Object(m) = p {
                let mut m = m.clone();
                m.remove("_label");
                Value::Object(m)
            } else {
                p.clone()
            }
        })
        .collect();
    let chunks: Vec<Vec<Value>> = if batch > 0 {
        clean.chunks(batch as usize).map(|c| c.to_vec()).collect()
    } else {
        vec![clean]
    };
    for (i, chunk) in chunks.iter().enumerate() {
        let r = api(
            svc,
            "POST",
            "/command",
            Some(&json!({"name": "ManualImport", "importMode": impmode, "files": chunk})),
        )
        .unwrap_or(Value::Null);
        let n = if chunks.len() > 1 {
            format!("{}/{} ", i + 1, chunks.len())
        } else {
            String::new()
        };
        println!(
            "ManualImport {}queued: id={} status={} ({} files)",
            n,
            ps(&r, "id"),
            ps(&r, "status"),
            chunk.len()
        );
        if wait || chunks.len() > 1 {
            let rec = wait_command(svc, r.i("id"), timeout);
            println!("  -> {} {}", ps(&rec, "status"), rec.s("message"));
        }
    }
}

/// Force-import downloaded files into explicit episodes, bypassing name match.
///
/// arr sonarr import <folder> --series <id|query> [--match SUBSTR] [--season N]
///     [--map auto|abs|se] [--mode copy|move] [--batch N] [--dry-run]
///
/// SAFETY: a folder may hold files from many shows. --match restricts to files
/// whose name contains SUBSTR (case-insensitive). Without it, EVERY file under
/// the folder is mapped onto this series — only safe for a single-show folder.
pub fn cmd_import(svc: &str, args: &[String]) {
    if svc == "radarr" {
        return cmd_import_radarr(args);
    }
    if !svc.starts_with("sonarr") {
        die("import: sonarr or radarr only");
    }
    let (flags, rest) = pop_flags(
        args,
        &[
            ("--series", 1),
            ("--match", 1),
            ("--season", 1),
            ("--map", 1),
            ("--mode", 1),
            ("--dry-run", 0),
            ("--wait", 0),
            ("--timeout", 1),
            ("--batch", 1),
        ],
    );
    if rest.is_empty() || !flags.has("--series") {
        die("import: usage: arr sonarr import <folder> --series <id|query> [--match SUBSTR] [--season N] [--map auto|abs|se] [--mode copy|move] [--dry-run]");
    }
    let sid = resolve_id(svc, flags.val_or("--series", ""));
    let season = if flags.has("--season") {
        Some(py_int(flags.val_or("--season", "")))
    } else {
        None
    };
    let impmode = flags.val_or("--mode", "copy");
    let mat = flags.val("--match").unwrap_or("").to_lowercase();
    let (payload, skipped) =
        plan_series_import(svc, &rest[0], sid, &mat, flags.val_or("--map", "auto"), season);
    for p in &payload {
        println!("  {}", p.s("_label"));
    }
    if !skipped.is_empty() {
        println!(
            "  (skipped {} unmatched: {}{})",
            skipped.len(),
            skipped.iter().take(3).cloned().collect::<Vec<_>>().join(", "),
            if skipped.len() > 3 { " ..." } else { "" }
        );
    }
    if payload.is_empty() {
        println!("nothing to import");
        return;
    }
    if flags.has("--dry-run") {
        println!("DRY: would ManualImport {} file(s) [mode={}]", payload.len(), impmode);
        return;
    }
    run_manual_import(
        svc,
        &payload,
        impmode,
        flag_int(&flags, "--batch", 10),
        flags.has("--wait"),
        flag_int(&flags, "--timeout", 300),
    );
}

/// Force-import downloaded file(s) into a movie, bypassing name matching.
///
/// arr radarr import <folder> --movie <id|query> [--match SUBSTR]
///     [--mode copy|move] [--dry-run] [--wait]
fn cmd_import_radarr(args: &[String]) {
    let (flags, rest) = pop_flags(
        args,
        &[
            ("--movie", 1),
            ("--match", 1),
            ("--mode", 1),
            ("--dry-run", 0),
            ("--wait", 0),
            ("--timeout", 1),
            ("--batch", 1),
        ],
    );
    if rest.is_empty() || !flags.has("--movie") {
        die("import: usage: arr radarr import <folder> --movie <id|query> [--match SUBSTR] [--mode copy|move] [--dry-run] [--wait]");
    }
    let mid = resolve_id("radarr", flags.val_or("--movie", ""));
    let impmode = flags.val_or("--mode", "copy");
    let mat = flags.val("--match").unwrap_or("").to_lowercase();
    let (payload, skipped) = plan_movie_import(&rest[0], mid, &mat);
    for p in &payload {
        println!("  {}", p.s("_label"));
    }
    if !skipped.is_empty() {
        println!(
            "  (skipped {} without parsed quality: {}{})",
            skipped.len(),
            skipped.iter().take(3).cloned().collect::<Vec<_>>().join(", "),
            if skipped.len() > 3 { " ..." } else { "" }
        );
    }
    if payload.is_empty() {
        println!("nothing to import");
        return;
    }
    if flags.has("--dry-run") {
        println!("DRY: would ManualImport {} file(s) [mode={}]", payload.len(), impmode);
        return;
    }
    run_manual_import(
        "radarr",
        &payload,
        impmode,
        flag_int(&flags, "--batch", 10),
        flags.has("--wait"),
        flag_int(&flags, "--timeout", 300),
    );
}
