//! Seerr / Jellyfin / Bazarr command families (port of arr.py lines 3077-3353).
//!
//! Output strings are parity-critical — Hermes' skills and Andre's muscle
//! memory parse them. Python renders absent fields as "None" via `%s`; the
//! `py_str` helper reproduces that, and `py_dumps` reproduces
//! `json.dumps(..., indent=2)` (ensure_ascii escaping included).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use arr_api::json::items;
use arr_api::{api, bazarr_api, die, jf_api, pop_flags, resolve_id, seerr_api, JsonExt};

// --- Python-compat rendering helpers -----------------------------------------

/// Python `"%s" % value`: None -> "None", True/False, numbers/strings verbatim.
fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(b) => (if *b { "True" } else { "False" }).to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Python truthiness.
fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `s[:n]` (slices code points, not bytes).
fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// JSON string literal the way Python's json.dumps writes it (ensure_ascii:
/// everything outside printable ASCII becomes \uXXXX, non-BMP as surrogates).
fn py_json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if (c as u32) <= 0x7e => out.push(c),
            c => {
                let cp = c as u32;
                if cp > 0xffff {
                    let v = cp - 0x10000;
                    out.push_str(&format!(
                        "\\u{:04x}\\u{:04x}",
                        0xd800 + (v >> 10),
                        0xdc00 + (v & 0x3ff)
                    ));
                } else {
                    out.push_str(&format!("\\u{:04x}", cp));
                }
            }
        }
    }
    out.push('"');
    out
}

/// json.dumps(v, indent=2). NB serde_json's Value sorts object keys, so key
/// order can differ from Python (which keeps API order) — values/indentation
/// match exactly.
fn py_dumps(v: &Value, level: usize) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => (if *b { "true" } else { "false" }).to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => py_json_string(s),
        Value::Array(a) => {
            if a.is_empty() {
                return "[]".to_string();
            }
            let pad = "  ".repeat(level + 1);
            let inner: Vec<String> =
                a.iter().map(|x| format!("{}{}", pad, py_dumps(x, level + 1))).collect();
            format!("[\n{}\n{}]", inner.join(",\n"), "  ".repeat(level))
        }
        Value::Object(o) => {
            if o.is_empty() {
                return "{}".to_string();
            }
            let pad = "  ".repeat(level + 1);
            let inner: Vec<String> = o
                .iter()
                .map(|(k, x)| format!("{}{}: {}", pad, py_json_string(k), py_dumps(x, level + 1)))
                .collect();
            format!("{{\n{}\n{}}}", inner.join(",\n"), "  ".repeat(level))
        }
    }
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Python `(r.get("requestedBy") or {}).get("displayName", "?")` — default
/// only when the key is absent; an explicit null prints as "None".
fn requested_by(r: &Value) -> String {
    let rb = r.at(&["requestedBy"]);
    if rb.is_object() {
        match rb.get("displayName") {
            Some(v) => py_str(v),
            None => "?".to_string(),
        }
    } else {
        "?".to_string()
    }
}

/// `dict.get(status, status)` against an int->name map: mapped name as a
/// string Value, else the raw status value (number/null) passed through.
fn stat_value(map: &[(i64, &str)], v: &Value) -> Value {
    if let Some(n) = v.as_i64() {
        if let Some((_, name)) = map.iter().find(|(k, _)| *k == n) {
            return Value::String(name.to_string());
        }
    }
    v.clone()
}

const RSTAT: &[(i64, &str)] =
    &[(1, "pending"), (2, "approved"), (3, "declined"), (4, "failed"), (5, "completed")];
const MSTAT: &[(i64, &str)] =
    &[(1, "unknown"), (2, "pending"), (3, "processing"), (4, "partial"), (5, "available")];

// --- Seerr -------------------------------------------------------------------

fn title_cache() -> &'static Mutex<HashMap<(String, i64), Option<String>>> {
    static CACHE: OnceLock<Mutex<HashMap<(String, i64), Option<String>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn seerr_title(mtype: &str, tmdb: i64) -> Option<String> {
    if tmdb == 0 {
        return None;
    }
    let key = (mtype.to_string(), tmdb);
    if let Some(hit) = title_cache().lock().unwrap().get(&key) {
        return hit.clone();
    }
    let d = seerr_api(
        &format!("/{}/{}", if mtype == "tv" { "tv" } else { "movie" }, tmdb),
        &[],
        60,
        true,
    )
    .unwrap_or(Value::Null);
    let field = if mtype == "tv" { "name" } else { "title" };
    let t = d.get(field).and_then(Value::as_str).map(str::to_string);
    title_cache().lock().unwrap().insert(key, t.clone());
    t
}

/// Title with arr.py's fallback chain: media.title, then a Seerr tv/movie
/// details lookup, then "tmdb:<id>".
fn media_title(m: &Value) -> String {
    let t = m.s("title");
    if !t.is_empty() {
        return t.to_string();
    }
    match seerr_title(m.s("mediaType"), m.i("tmdbId")).filter(|s| !s.is_empty()) {
        Some(t) => t,
        None => format!("tmdb:{}", py_str(m.at(&["tmdbId"]))),
    }
}

/// Jellyseerr requests: who requested what, request + media status.
pub fn seerr_requests(args: &[String]) {
    let (flags, rest) = pop_flags(args, &[("--pending", 0), ("--limit", 1)]);
    let take = flags.val_or("--limit", "50").to_string();
    let mut params: Vec<(&str, &str)> =
        vec![("take", take.as_str()), ("skip", "0"), ("sort", "added")];
    if flags.has("--pending") {
        params.push(("filter", "pending"));
    }
    let data = seerr_api("/request", &params, 60, false).unwrap_or(Value::Null);
    let pat = rest.first().map(|s| s.to_lowercase()).filter(|p| !p.is_empty());
    let mut rows: Vec<(Value, Value, String)> = Vec::new();
    for r in data.a("results") {
        let m = r.at(&["media"]);
        let title = media_title(m);
        if let Some(p) = &pat {
            if !title.to_lowercase().contains(p.as_str()) {
                continue;
            }
        }
        rows.push((r.clone(), m.clone(), title));
    }
    for (r, m, title) in &rows {
        println!(
            "  #{}  {} [{}]  req={} media={}  by {}  {}",
            py_str(r.at(&["id"])),
            title,
            py_str(m.at(&["mediaType"])),
            py_str(&stat_value(RSTAT, r.at(&["status"]))),
            py_str(&stat_value(MSTAT, m.at(&["status"]))),
            requested_by(r),
            truncate_chars(r.s("createdAt"), 10)
        );
    }
    println!("({} request(s))", rows.len());
}

pub fn seerr_request(args: &[String]) {
    if args.is_empty() {
        die("seerr request: need a request id");
    }
    let v = seerr_api(&format!("/request/{}", args[0]), &[], 60, false).unwrap_or(Value::Null);
    println!("{}", py_dumps(&v, 0));
}

enum Fix {
    Radarr(i64),
    Season(String, i64, Vec<i64>),
}

struct Row {
    id: Value,
    title: String,
    typ: Value,
    by: String,
    seerr: Value,
    fix: Option<Fix>,
    state: String,
}

impl Row {
    fn fix_value(&self) -> Value {
        match &self.fix {
            None => Value::Null,
            Some(Fix::Radarr(id)) => json!(["radarr", id]),
            Some(Fix::Season(inst, sid, seas)) => json!([inst, sid, seas]),
        }
    }
    /// One row of json.dumps(out, indent=2), preserving arr.py's dict
    /// insertion order (id, title, type, by, seerr, fix, state).
    fn json(&self) -> String {
        format!(
            "  {{\n    \"id\": {},\n    \"title\": {},\n    \"type\": {},\n    \"by\": {},\n    \"seerr\": {},\n    \"fix\": {},\n    \"state\": {}\n  }}",
            py_dumps(&self.id, 2),
            py_json_string(&self.title),
            py_dumps(&self.typ, 2),
            py_json_string(&self.by),
            py_dumps(&self.seerr, 2),
            py_dumps(&self.fix_value(), 2),
            py_json_string(&self.state)
        )
    }
}

/// Requests whose REQUESTED content isn't actually on disk — judged against
/// Sonarr/Sonarr-anime/Radarr season stats, not Seerr's own cached status.
///
/// arr seerr unfulfilled [--fix] [--json] [--quiet]
/// Also flags DIVERGED rows: Seerr says pending/partial but the requested
/// seasons are complete (Seerr's whole-series status lies for season requests).
/// --fix triggers arr-side searches (SeasonSearch/MoviesSearch) for the gaps.
pub fn seerr_unfulfilled(args: &[String]) {
    let (flags, _) = pop_flags(args, &[("--fix", 0), ("--json", 0), ("--quiet", 0)]);
    let data = seerr_api("/request", &[("take", "300"), ("skip", "0"), ("sort", "added")], 60, false)
        .unwrap_or(Value::Null);
    let movies_resp = api("radarr", "GET", "/movie", None);
    let mut movies: HashMap<i64, Value> = HashMap::new();
    for m in items(&movies_resp) {
        movies.insert(m.i("tmdbId"), m.clone());
    }
    let mut series_by_tvdb: HashMap<i64, (String, Value)> = HashMap::new();
    for inst in ["sonarr", "sonarr-anime"] {
        let resp = api(inst, "GET", "/series", None);
        for s in items(&resp) {
            series_by_tvdb
                .entry(s.i("tvdbId"))
                .or_insert_with(|| (inst.to_string(), s.clone()));
        }
    }
    let mut out: Vec<Row> = Vec::new();
    for r in data.a("results") {
        if r.i("status") == 3 {
            // declined
            continue;
        }
        let m = r.at(&["media"]);
        let mtype = m.s("mediaType").to_string();
        let seerr_v = stat_value(MSTAT, m.at(&["status"]));
        let base = |state: String, fix: Option<Fix>| Row {
            id: r.at(&["id"]).clone(),
            title: media_title(m),
            typ: m.at(&["mediaType"]).clone(),
            by: requested_by(r),
            seerr: seerr_v.clone(),
            fix,
            state,
        };
        if mtype == "movie" {
            let mv = movies.get(&m.i("tmdbId"));
            if let Some(mv) = mv {
                if mv.b("hasFile") {
                    if m.i("status") != 5 {
                        out.push(base(
                            format!("DIVERGED — on disk but Seerr says {}", py_str(&seerr_v)),
                            None,
                        ));
                    }
                    continue;
                }
            }
            match mv {
                Some(mv) => out.push(base(
                    format!("missing (radarr id {})", mv.i("id")),
                    Some(Fix::Radarr(mv.i("id"))),
                )),
                None => out.push(base("NOT IN RADARR".to_string(), None)),
            }
        } else {
            let hit = series_by_tvdb.get(&m.i("tvdbId"));
            let mut want: Vec<i64> = r
                .a("seasons")
                .iter()
                .filter(|x| x.has("seasonNumber"))
                .map(|x| x.i("seasonNumber"))
                .collect();
            want.sort();
            let Some((inst, s)) = hit else {
                let req = if !want.is_empty() {
                    format!(
                        " (requested S{})",
                        want.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(",")
                    )
                } else {
                    String::new()
                };
                out.push(base(format!("NOT IN SONARR{}", req), None));
                continue;
            };
            let mut gaps: Vec<(i64, i64, i64)> = Vec::new();
            for se in s.a("seasons") {
                let sn = se.i("seasonNumber");
                if !want.is_empty() && !want.contains(&sn) {
                    continue;
                }
                if sn == 0 && want.is_empty() {
                    continue;
                }
                let st = se.at(&["statistics"]);
                let (have, aired) = (st.i("episodeFileCount"), st.i("episodeCount"));
                if have < aired {
                    gaps.push((sn, have, aired));
                }
            }
            if gaps.is_empty() {
                if m.i("status") != 5 {
                    out.push(base(
                        format!(
                            "DIVERGED — requested seasons complete ({}) but Seerr says {}",
                            inst,
                            py_str(&seerr_v)
                        ),
                        None,
                    ));
                }
                continue;
            }
            let state = format!(
                "gaps: {}",
                gaps.iter()
                    .map(|(sn, have, aired)| format!("S{} {}/{}", sn, have, aired))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let seas: Vec<i64> = gaps.iter().map(|g| g.0).collect();
            out.push(base(state, Some(Fix::Season(inst.clone(), s.i("id"), seas))));
        }
    }
    if flags.has("--json") {
        if out.is_empty() {
            println!("[]");
        } else {
            let rows: Vec<String> = out.iter().map(Row::json).collect();
            println!("[\n{}\n]", rows.join(",\n"));
        }
        return;
    }
    if out.is_empty() {
        if !flags.has("--quiet") {
            println!("unfulfilled: none — every non-declined request is on disk");
        }
        return;
    }
    println!("unfulfilled/diverged: {} request(s)", out.len());
    for row in &out {
        println!(
            "  #{:<4} {:<38} [{:<5}] by {:<12} {}",
            py_str(&row.id),
            truncate_chars(&row.title, 38),
            py_str(&row.typ),
            truncate_chars(&row.by, 12),
            row.state
        );
    }
    if !flags.has("--fix") {
        return;
    }
    println!("fixes:");
    for row in &out {
        match &row.fix {
            None => continue,
            Some(Fix::Radarr(mid)) => {
                let r2 = api(
                    "radarr",
                    "POST",
                    "/command",
                    Some(&json!({"name": "MoviesSearch", "movieIds": [mid]})),
                )
                .unwrap_or(Value::Null);
                println!(
                    "  #{} {}: MoviesSearch queued (cmd {})",
                    py_str(&row.id),
                    truncate_chars(&row.title, 30),
                    py_str(r2.at(&["id"]))
                );
            }
            Some(Fix::Season(inst, sid, seas)) => {
                for sn in seas {
                    let r2 = api(
                        inst,
                        "POST",
                        "/command",
                        Some(&json!({"name": "SeasonSearch", "seriesId": sid, "seasonNumber": sn})),
                    )
                    .unwrap_or(Value::Null);
                    println!(
                        "  #{} {} S{}: SeasonSearch queued on {} (cmd {})",
                        py_str(&row.id),
                        truncate_chars(&row.title, 30),
                        sn,
                        inst,
                        py_str(r2.at(&["id"]))
                    );
                }
            }
        }
    }
}

// --- Jellyfin ----------------------------------------------------------------

pub fn jf_search_items(term: &str, limit: usize) -> Vec<Value> {
    let limit = limit.to_string();
    let r = jf_api(
        "/Items",
        &[
            ("searchTerm", term),
            ("Recursive", "true"),
            ("IncludeItemTypes", "Movie,Series"),
            ("Fields", "Path,ProviderIds"),
            ("Limit", limit.as_str()),
        ],
        60,
        "GET",
        true,
    );
    r.map(|v| v.a("Items").to_vec()).unwrap_or_default()
}

/// arr jellyfin has <title> — is it visible in the Jellyfin library NOW?
/// (What users see; an arr import isn't 'ready' until this says yes.)
pub fn cmd_jf_has(args: &[String]) {
    if args.is_empty() {
        die("jellyfin has: need a title");
    }
    let term = args.join(" ");
    let hits = jf_search_items(&term, 10);
    if hits.is_empty() {
        println!("not in Jellyfin: {}", term);
        std::process::exit(1);
    }
    for it in &hits {
        let mut extra = String::new();
        if it.s("Type") == "Series" {
            let eps = jf_api(&format!("/Shows/{}/Episodes", it.s("Id")), &[("Limit", "1")], 60, "GET", true);
            if let Some(eps) = &eps {
                if py_truthy(eps) && eps.has("TotalRecordCount") {
                    extra = format!("  {} episode(s)", py_str(eps.at(&["TotalRecordCount"])));
                }
            }
        }
        println!(
            "  [{}] {} ({}){}",
            py_str(it.at(&["Type"])),
            py_str(it.at(&["Name"])),
            py_str(it.at(&["ProductionYear"])),
            extra
        );
        if !it.s("Path").is_empty() {
            println!("      {}", it.s("Path"));
        }
    }
}

/// arr jellyfin refresh [--wait <title>] [--timeout SECS] — trigger a library
/// scan; --wait polls until <title> is searchable (replaces curl+sleep loops).
pub fn cmd_jf_refresh(args: &[String]) {
    let (flags, _) = pop_flags(args, &[("--wait", 1), ("--timeout", 1)]);
    jf_api("/Library/Refresh", &[], 60, "POST", false);
    println!("library refresh triggered");
    let term = match flags.val("--wait") {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => return,
    };
    let timeout_raw = flags.val_or("--timeout", "300").to_string();
    let timeout: i64 = timeout_raw
        .trim()
        .parse()
        .unwrap_or_else(|_| die(&format!("bad --timeout '{}'", timeout_raw)));
    let deadline = now_secs() + timeout as f64;
    while now_secs() < deadline {
        let hits = jf_search_items(&term, 5);
        if let Some(first) = hits.first() {
            println!(
                "visible in Jellyfin: {} ({})",
                py_str(first.at(&["Name"])),
                py_str(first.at(&["Type"]))
            );
            return;
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    println!("timeout: '{}' not visible after {}s", term, timeout_raw);
    std::process::exit(2);
}

// --- Bazarr (subtitle manager) -------------------------------------------------

/// Bazarr health: provider throttle state + wanted-subtitle backlog.
pub fn bazarr_status(_args: &[String]) {
    let resp = bazarr_api("GET", "/system/status", &[], 120, false).unwrap_or(Value::Null);
    let st = resp.at(&["data"]);
    println!(
        "bazarr {} (sonarr {}, radarr {})",
        py_str(st.at(&["bazarr_version"])),
        py_str(st.at(&["sonarr_version"])),
        py_str(st.at(&["radarr_version"]))
    );
    let provs = bazarr_api("GET", "/providers", &[], 120, false).unwrap_or(Value::Null);
    for p in provs.a("data") {
        let retry = p.at(&["retry"]);
        let retry_s = py_str(retry);
        let suffix = if py_truthy(retry) && retry_s != "-" {
            format!("  (retry {})", retry_s)
        } else {
            String::new()
        };
        println!("  provider {:<18} {}{}", py_str(p.at(&["name"])), py_str(p.at(&["status"])), suffix);
    }
    let we = bazarr_api("GET", "/episodes/wanted", &[("start", "0"), ("length", "1")], 120, false)
        .unwrap_or(Value::Null);
    let wm = bazarr_api("GET", "/movies/wanted", &[("start", "0"), ("length", "1")], 120, false)
        .unwrap_or(Value::Null);
    let tot = |v: &Value| match v.get("total") {
        Some(t) => py_str(t),
        None => "?".to_string(),
    };
    println!("wanted: {} episode(s) + {} movie(s) missing subtitles", tot(&we), tot(&wm));
    println!("(covers MAIN sonarr + radarr only — sonarr-anime items are not managed by Bazarr)");
}

/// arr bazarr wanted [pattern] [--tv|--movies] [--limit N] — what Bazarr
/// still owes subtitles for (its own wanted list, main sonarr + radarr).
pub fn bazarr_wanted(args: &[String]) {
    let (flags, rest) = pop_flags(args, &[("--tv", 0), ("--movies", 0), ("--limit", 1)]);
    let pat = rest.first().map(|s| s.to_lowercase()).filter(|p| !p.is_empty());
    let lim_raw = flags.val_or("--limit", "500").to_string();
    let lim: i64 = lim_raw
        .trim()
        .parse()
        .unwrap_or_else(|_| die(&format!("bad --limit '{}'", lim_raw)));
    let lim_s = lim.to_string();
    let langs_of = |r: &Value| {
        r.a("missing_subtitles")
            .iter()
            .map(|x| match x.get("code3") {
                Some(v) => py_str(v),
                None => "?".to_string(),
            })
            .collect::<Vec<_>>()
            .join(",")
    };
    let mut shown = 0;
    if !flags.has("--movies") {
        let data =
            bazarr_api("GET", "/episodes/wanted", &[("start", "0"), ("length", lim_s.as_str())], 120, false)
                .unwrap_or(Value::Null);
        for r in data.a("data") {
            let title = r.s("seriesTitle");
            if let Some(p) = &pat {
                if !title.to_lowercase().contains(p.as_str()) {
                    continue;
                }
            }
            println!(
                "  tv     {:<35} {:<6} {:<28} wants: {}",
                truncate_chars(title, 35),
                py_str(r.at(&["episode_number"])),
                truncate_chars(r.s("episodeTitle"), 28),
                langs_of(r)
            );
            shown += 1;
        }
        if pat.is_none() {
            println!("  (tv total: {})", py_str(data.at(&["total"])));
        }
    }
    if !flags.has("--tv") {
        let data =
            bazarr_api("GET", "/movies/wanted", &[("start", "0"), ("length", lim_s.as_str())], 120, false)
                .unwrap_or(Value::Null);
        for r in data.a("data") {
            let title = r.s("title");
            if let Some(p) = &pat {
                if !title.to_lowercase().contains(p.as_str()) {
                    continue;
                }
            }
            println!("  movie  {:<64} wants: {}", truncate_chars(title, 64), langs_of(r));
            shown += 1;
        }
        if pat.is_none() {
            println!("  (movie total: {})", py_str(data.at(&["total"])));
        }
    }
    if pat.is_some() && shown == 0 {
        println!(
            "nothing in Bazarr's wanted list matching '{}' (NB anime-instance items never appear here)",
            rest[0]
        );
    }
}

/// arr bazarr search --series <sonarr id|query> | --movie <radarr id|query>
/// Trigger Bazarr's search-missing for one item (uses the configured providers
/// + OpenSubtitles membership; downloads happen in the background).
pub fn bazarr_search(args: &[String]) {
    let (flags, _) = pop_flags(args, &[("--series", 1), ("--movie", 1)]);
    if let Some(q) = flags.val("--series").filter(|v| !v.is_empty()) {
        let sid = resolve_id("sonarr", q);
        let s = api("sonarr", "GET", &format!("/series/{}", sid), None).unwrap_or(Value::Null);
        bazarr_api(
            "PATCH",
            "/series",
            &[("seriesid", sid.to_string().as_str()), ("action", "search-missing")],
            120,
            false,
        );
        println!(
            "Bazarr search-missing triggered for series '{}' (sonarr id {})",
            s.s("title"),
            sid
        );
    } else if let Some(q) = flags.val("--movie").filter(|v| !v.is_empty()) {
        let mid = resolve_id("radarr", q);
        let m = api("radarr", "GET", &format!("/movie/{}", mid), None).unwrap_or(Value::Null);
        bazarr_api(
            "PATCH",
            "/movies",
            &[("radarrid", mid.to_string().as_str()), ("action", "search-missing")],
            120,
            false,
        );
        println!(
            "Bazarr search-missing triggered for movie '{}' (radarr id {})",
            m.s("title"),
            mid
        );
    } else {
        die("bazarr search: need --series <sonarr id|query> or --movie <radarr id|query> (sonarr-anime items are not Bazarr-covered)");
    }
    println!("(async — verify later with `arr bazarr wanted <title>` or `arr <svc> tracks <item> --missing-subs eng`)");
}

/// Python urllib.parse.unquote_plus: '+' -> space, %XX percent-decoding,
/// invalid UTF-8 replaced (errors="replace").
fn unquote_plus(s: &str) -> String {
    let s = s.replace('+', " ");
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // '%' followed by two hex digits decodes; anything else passes through
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(b) = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Python urllib.parse.parse_qsl + dict(): '&'-separated k=v pairs, pairs
/// without '=' dropped, duplicate keys keep first position / last value.
fn parse_qsl_dict(qs: &str) -> Vec<(String, String)> {
    let mut params: Vec<(String, String)> = Vec::new();
    for field in qs.split('&') {
        let Some((k, v)) = field.split_once('=') else { continue };
        let (k, v) = (unquote_plus(k), unquote_plus(v));
        if let Some(e) = params.iter_mut().find(|(pk, _)| *pk == k) {
            e.1 = v;
        } else {
            params.push((k, v));
        }
    }
    params
}

pub fn bazarr_raw(args: &[String]) {
    if args.len() < 2 {
        die("bazarr raw: usage: arr bazarr raw <METHOD> <path> [urlencoded-params]");
    }
    let method = args[0].to_uppercase();
    let mut path = args[1].clone();
    if !path.starts_with('/') {
        path = format!("/{}", path);
    }
    let params: Vec<(String, String)> =
        if args.len() > 2 { parse_qsl_dict(&args[2]) } else { Vec::new() };
    let params_ref: Vec<(&str, &str)> =
        params.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let out = bazarr_api(&method, &path, &params_ref, 120, false);
    match out {
        Some(v) => println!("{}", py_dumps(&v, 0)),
        None => println!("(empty response)"),
    }
}
