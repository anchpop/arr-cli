//! browse.rs — read-mostly commands: tag, status, get, seasons, releases,
//! monitor, queue, wait, episodes, history, wanted, raw, parse, search.
//! Output strings, flags and exit codes have parsers (Hermes' skills and
//! crons) — evolve them additively; grep `skills/` before rewording.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arr_api::env::env_file_path;
use arr_api::http::form_encode;
use arr_api::http::{try_api, ApiError};
use arr_api::{api, api_t, die, fmt_gb, mb, pop_flags, resolve_id, Flags, JsonExt, SAB_PORT};
use serde_json::{json, Value};

/// interactive indexer searches (/release) can take minutes
pub const SEARCH_TIMEOUT: u64 = 300;

// --- Python-compat formatting helpers ----------------------------------------

/// Python str(float): integral floats get a trailing ".0".
fn py_float(f: f64) -> String {
    if f.is_finite() && f == f.trunc() && f.abs() < 1e16 {
        format!("{:.1}", f)
    } else {
        format!("{}", f)
    }
}

/// Python repr() — for %s of lists/dicts (e.g. parse's episodeNumbers).
fn py_repr(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(b) => (if *b { "True" } else { "False" }).to_string(),
        Value::Number(n) => {
            if n.is_f64() {
                py_float(n.as_f64().unwrap_or(0.0))
            } else {
                n.to_string()
            }
        }
        Value::String(s) => format!("'{}'", s),
        Value::Array(a) => {
            let inner: Vec<String> = a.iter().map(py_repr).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Object(m) => {
            let inner: Vec<String> =
                m.iter().map(|(k, v)| format!("'{}': {}", k, py_repr(v))).collect();
            format!("{{{}}}", inner.join(", "))
        }
    }
}

/// Python "%s" % value — None -> "None", True/False, floats with ".0".
fn py_scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => py_repr(other),
    }
}

/// Python "%s" % r.get(key) — absent key -> "None".
fn py_get(v: &Value, key: &str) -> String {
    match v.get(key) {
        None => "None".to_string(),
        Some(x) => py_scalar(x),
    }
}

/// Python "%s" % d.get(key, default) — default only when the key is ABSENT
/// (a present-but-null value still renders "None").
fn py_get_or(v: &Value, key: &str, default: &str) -> String {
    match v.get(key) {
        None => default.to_string(),
        Some(x) => py_scalar(x),
    }
}

/// json.dumps(..., indent=2) with ensure_ascii=True. NB serde_json's Map is
/// sorted by key (no preserve_order feature), so key ORDER can differ from
/// the Python's document order on raw API dumps.
fn py_json_pretty(v: &Value) -> String {
    let mut out = String::new();
    pj(v, 0, &mut out);
    out
}

fn pj(v: &Value, ind: usize, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if n.is_f64() {
                out.push_str(&py_float(n.as_f64().unwrap_or(0.0)));
            } else {
                out.push_str(&n.to_string());
            }
        }
        Value::String(s) => out.push_str(&py_json_string(s)),
        Value::Array(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push_str("[\n");
            for (i, x) in a.iter().enumerate() {
                out.push_str(&" ".repeat(ind + 2));
                pj(x, ind + 2, out);
                if i + 1 < a.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&" ".repeat(ind));
            out.push(']');
        }
        Value::Object(m) => {
            if m.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{\n");
            for (i, (k, x)) in m.iter().enumerate() {
                out.push_str(&" ".repeat(ind + 2));
                out.push_str(&py_json_string(k));
                out.push_str(": ");
                pj(x, ind + 2, out);
                if i + 1 < m.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&" ".repeat(ind));
            out.push('}');
        }
    }
}

/// JSON string literal, ensure_ascii style (non-ASCII -> \uXXXX).
fn py_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if (c as u32) < 0x7f => out.push(c),
            c => {
                let cp = c as u32;
                if cp > 0xFFFF {
                    let v = cp - 0x10000;
                    out.push_str(&format!(
                        "\\u{:04x}\\u{:04x}",
                        0xD800 + (v >> 10),
                        0xDC00 + (v & 0x3FF)
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

/// urllib.parse.quote (safe="/"): percent-encode everything but unreserved + /.
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

fn now_f64() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}



// --- language / release-title heuristics --------------------------------------

fn norm_lang(l: &str) -> String {
    let l = if l.is_empty() { "und".to_string() } else { l.to_lowercase() };
    match l.as_str() {
        "en" => "eng",
        "ja" | "jp" => "jpn",
        "fr" | "fra" => "fre",
        "de" | "deu" => "ger",
        "ko" => "kor",
        "zh" | "zho" => "chi",
        "es" => "spa",
        "it" => "ita",
        "pt" => "por",
        "ru" => "rus",
        other => other,
    }
    .to_string()
}

/// title-token heuristic (Dual Audio / English Dub / Multi ...) — releases
/// don't carry real audio metadata.
pub fn audio_match(title: &str, lang: &str) -> bool {
    let t = title.to_lowercase();
    let markers: &[&str] = match lang {
        "eng" => &[
            "dual audio",
            "dual-audio",
            "dualaudio",
            "dual]",
            "english dub",
            "eng dub",
            "engdub",
            "english audio",
            "dubbed",
            "multi audio",
            "multi-audio",
            "multi]",
        ],
        "dual" => &["dual audio", "dual-audio", "dualaudio", "dual]", "multi audio", "multi-audio"],
        _ => return t.contains(lang),
    };
    markers.iter().any(|m| t.contains(m))
}

// --- requester tagging -------------------------------------------------------
// The download-notifier daemon (declared in /etc/nixos/configuration.nix) DMs the
// person who asked for a download, with a live progress bar. For requests that
// come in through Seerr it resolves the requester itself. For "hey bot, grab me
// X" asks made straight to Hermes, there's no Seerr row — so when Hermes grabs on
// someone's behalf it stamps the series/movie with a `requester:<discordId>` tag
// (their Discord user id) and the notifier reads that. Tag the DISCORD id, not a
// Jellyfin username: it's exactly what's needed to DM, Hermes already has it, and
// it works for people with no Jellyfin account.

pub fn ensure_tag(svc: &str, label: &str) -> i64 {
    try_ensure_tag(svc, label).unwrap_or_else(|e| die(&e))
}

fn try_ensure_tag(svc: &str, label: &str) -> Result<i64, String> {
    let err = |m: &str, e: &ApiError| crate::acquire::api_err_msg(m, "/tag", 120, e);
    let tags = try_api(svc, "GET", "/tag", None, 120)
        .map_err(|e| err("GET", &e))?
        .unwrap_or(Value::Null);
    if let Some(arr) = tags.as_array() {
        for t in arr {
            if t.s("label").to_lowercase() == label.to_lowercase() {
                return Ok(t.i("id"));
            }
        }
    }
    try_api(svc, "POST", "/tag", Some(&json!({ "label": label })), 120)
        .map_err(|e| err("POST", &e))
        .map(|v| v.map(|v| v.i("id")).unwrap_or(0))
}

/// Add/remove one tag label on a series/movie via the editor endpoint.
pub fn stamp_label(svc: &str, item_id: i64, label: &str, remove: bool) -> &'static str {
    try_stamp_label(svc, item_id, label, remove).unwrap_or_else(|e| die(&e))
}

/// Fallible stamp_label — for `add`, where a failed tag must not abort the
/// wait-and-report half of the command (a half-done add whose caller can't
/// tell which half happened is the worst shape for the agent).
pub fn try_stamp_label(
    svc: &str,
    item_id: i64,
    label: &str,
    remove: bool,
) -> Result<&'static str, String> {
    let (coll, ids_field) = if svc.starts_with("sonarr") {
        ("series", "seriesIds")
    } else if svc == "radarr" {
        ("movie", "movieIds")
    } else {
        return Err("tag: only sonarr/sonarr-anime/radarr have taggable items".into());
    };
    let tag_id = try_ensure_tag(svc, label)?;
    let mut body = serde_json::Map::new();
    body.insert(ids_field.to_string(), json!([item_id]));
    body.insert("tags".to_string(), json!([tag_id]));
    body.insert("applyTags".to_string(), json!(if remove { "remove" } else { "add" }));
    let path = format!("/{}/editor", coll);
    try_api(svc, "PUT", &path, Some(&Value::Object(body)), 120)
        .map_err(|e| crate::acquire::api_err_msg("PUT", &path, 120, &e))?;
    Ok(coll)
}

pub fn tag_requester(svc: &str, query: &str, discord_id: &str, remove: bool) -> (String, i64, String) {
    let did = discord_id.trim().to_string();
    if did.is_empty() || !did.chars().all(|c| c.is_ascii_digit()) {
        die(&format!("tag: --requester must be a numeric Discord user id (got '{}')", discord_id));
    }
    let item_id = resolve_id(svc, query);
    // Sonarr/Radarr tag labels allow only [a-z0-9-] (no colon), so `requester-<id>`.
    let coll = stamp_label(svc, item_id, &format!("requester-{}", did), remove);
    (coll.to_string(), item_id, did)
}

/// ['require-subs-eng', ...] from --require-subs/--require-audio flags.
/// The download-notifier reads these at ready-time and appends a verified
/// '🔎 eng subs ✓' line to the ✅ embed — no watcher cron needed.
pub fn require_labels(flags: &Flags) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(v) = flags.val("--require-subs") {
        if !v.is_empty() {
            out.push(format!("require-subs-{}", norm_lang(v)));
        }
    }
    if let Some(v) = flags.val("--require-audio") {
        if !v.is_empty() {
            out.push(format!("require-audio-{}", norm_lang(v)));
        }
    }
    out
}

/// arr <svc> tag <id|query> [--requester <discordId>]
///     [--require-subs LANG] [--require-audio LANG] [--remove]
/// requester-* routes download-notifier DMs; require-* makes its ✅ ready
/// embed VERIFY the language (ffprobe) — use for "with English subs" asks.
pub fn cmd_tag(svc: &str, args: &[String]) {
    let (flags, rest) = pop_flags(
        args,
        &[("--requester", 1), ("--remove", 0), ("--require-subs", 1), ("--require-audio", 1)],
    );
    let req_labels = require_labels(&flags);
    let has_requester = flags.val("--requester").map_or(false, |v| !v.is_empty());
    if rest.is_empty() || !(has_requester || !req_labels.is_empty()) {
        die("tag: usage: arr <svc> tag <id|query> [--requester <discordId>] [--require-subs LANG] [--require-audio LANG] [--remove]");
    }
    let remove = flags.has("--remove");
    let verb = if remove { "removed from" } else { "added to" };
    if has_requester {
        let (coll, item_id, did) =
            tag_requester(svc, &rest[0], flags.val("--requester").unwrap_or(""), remove);
        println!("requester:{} {} {} #{}", did, verb, coll, item_id);
    }
    for lab in &req_labels {
        let item_id = resolve_id(svc, &rest[0]);
        let coll = stamp_label(svc, item_id, lab, remove);
        println!("{} {} {} #{} (notifier verifies at ready-time)", lab, verb, coll, item_id);
    }
}

// --- commands ----------------------------------------------------------------

/// Cheap per-season gap flags from a series object's season statistics.
/// 'aired' below = Sonarr's episodeCount stat (monitored episodes that aired).
pub fn season_gap_lines(svc: &str, s: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let mut gaps = false;
    for se in s.a("seasons") {
        let sn = se.i("seasonNumber");
        let st = se.at(&["statistics"]);
        let have = st.i("episodeFileCount");
        let aired = st.i("episodeCount");
        let total = st.i("totalEpisodeCount");
        if sn == 0 {
            if total != 0 {
                out.push(format!(
                    "      S0 specials: {}/{} on disk{}",
                    have,
                    total,
                    if se.b("monitored") { "" } else { " (unmonitored)" }
                ));
            }
            continue;
        }
        if se.b("monitored") {
            if have < aired {
                gaps = true;
                out.push(format!(
                    "      ⚠ S{}: {}/{} aired eps on disk ({})",
                    sn,
                    have,
                    aired,
                    if have != 0 { "partial" } else { "missing" }
                ));
            }
        } else if have < total {
            gaps = true;
            out.push(format!("      ○ S{}: not monitored, {}/{} on disk", sn, have, total));
        }
    }
    if gaps {
        out.push(format!(
            "      -> gaps: `arr {} coverage {} --fix` searches missing MONITORED eps; unmonitored seasons need the user's OK first",
            svc,
            s.i("id")
        ));
    }
    out
}

pub fn cmd_status(svc: &str, args: &[String]) {
    let q = args.first().map(|a| a.to_lowercase()).unwrap_or_default();
    if svc.starts_with("sonarr") {
        let resp = api(svc, "GET", "/series", None).unwrap_or(Value::Null);
        let mut items: Vec<Value> = resp.as_array().cloned().unwrap_or_default();
        items.sort_by(|a, b| a.s("title").cmp(b.s("title")));
        let matches: Vec<&Value> =
            items.iter().filter(|s| q.is_empty() || s.s("title").to_lowercase().contains(&q)).collect();
        for s in &matches {
            let s: &Value = s;
            let st = s.at(&["statistics"]);
            println!(
                "[{}] {} ({}) — {}, mon={}",
                s.i("id"),
                s.s("title"),
                py_get(s, "year"),
                s.s("status"),
                py_get(s, "monitored")
            );
            println!(
                "      {}/{} eps on disk ({}%), {}GB",
                st.i("episodeFileCount"),
                st.i("totalEpisodeCount"),
                py_get_or(st, "percentOfEpisodes", "0"),
                fmt_gb(st.i("sizeOnDisk"))
            );
            // exactly one match -> surface per-season coverage right here, so a
            // "do we have X?" check can't miss a partially-downloaded season
            if matches.len() == 1 {
                for ln in season_gap_lines(svc, s) {
                    println!("{}", ln);
                }
                audit_warn(svc, s.i("id"), Some(s));
            }
        }
    } else if svc == "radarr" {
        let resp = api(svc, "GET", "/movie", None).unwrap_or(Value::Null);
        let mut items: Vec<Value> = resp.as_array().cloned().unwrap_or_default();
        items.sort_by(|a, b| a.s("title").cmp(b.s("title")));
        let matches: Vec<&Value> =
            items.iter().filter(|m| q.is_empty() || m.s("title").to_lowercase().contains(&q)).collect();
        for m in &matches {
            let m: &Value = m;
            let disk = if m.b("hasFile") {
                format!("ON DISK ({}GB)", fmt_gb(m.i("sizeOnDisk")))
            } else {
                "MISSING".to_string()
            };
            println!(
                "[{}] {} ({}) — {}, mon={}, {}",
                m.i("id"),
                m.s("title"),
                py_get(m, "year"),
                m.s("status"),
                py_get(m, "monitored"),
                disk
            );
            if matches.len() == 1 {
                audit_warn(svc, m.i("id"), Some(m));
            }
        }
    } else {
        die("status: sonarr|radarr only");
    }
}

pub fn cmd_get(svc: &str, args: &[String]) {
    if args.is_empty() {
        die("get: need an id or query");
    }
    let coll = if svc.starts_with("sonarr") { "series" } else { "movie" };
    let v = api(svc, "GET", &format!("/{}/{}", coll, resolve_id(svc, &args[0])), None);
    println!("{}", py_json_pretty(&v.unwrap_or(Value::Null)));
}

pub fn cmd_seasons(svc: &str, args: &[String]) {
    if !svc.starts_with("sonarr") {
        die("seasons: sonarr only");
    }
    let arg0 = args.first().unwrap_or_else(|| die("seasons: need an id or query"));
    let s = api(svc, "GET", &format!("/series/{}", resolve_id(svc, arg0)), None)
        .unwrap_or(Value::Null);
    println!("{}", s.s("title"));
    for se in s.a("seasons") {
        let st = se.at(&["statistics"]);
        println!(
            "  S{}  mon={}  {}/{} on disk",
            se.i("seasonNumber"),
            py_get(se, "monitored"),
            st.i("episodeFileCount"),
            st.i("totalEpisodeCount")
        );
    }
}

fn release_query(svc: &str, args: &[String]) -> String {
    let (flags, rest) = pop_flags(args, &[("--season", 1), ("--episode", 1)]);
    if rest.is_empty() {
        die("need an id or query");
    }
    if svc.starts_with("sonarr") {
        let sid = resolve_id(svc, &rest[0]);
        if flags.has("--episode") {
            return format!("episodeId={}", flags.val("--episode").unwrap_or(""));
        }
        if flags.has("--season") {
            return format!("seriesId={}&seasonNumber={}", sid, flags.val("--season").unwrap_or(""));
        }
        die("sonarr releases: need --season N or --episode EPID");
    } else {
        format!("movieId={}", resolve_id(svc, &rest[0]))
    }
}

pub fn cmd_releases(svc: &str, args: &[String]) {
    let (aflags, args) = pop_flags(args, &[("--audio", 1), ("--timeout", 1)]);
    let timeout: u64 = match aflags.val("--timeout") {
        Some(t) => t.parse().unwrap_or_else(|_| die(&format!("releases: bad --timeout '{}'", t))),
        None => SEARCH_TIMEOUT,
    };
    let resp = api_t(svc, "GET", &format!("/release?{}", release_query(svc, &args)), None, timeout);
    let mut rels: Vec<Value> =
        resp.as_ref().and_then(Value::as_array).cloned().unwrap_or_default();
    if let Some(a) = aflags.val("--audio") {
        if !a.is_empty() {
            let want = norm_lang(a);
            rels.retain(|r| audio_match(r.s("title"), &want));
        }
    }
    rels.sort_by(|a, b| {
        (a.b("rejected"), -a.i("seeders")).cmp(&(b.b("rejected"), -b.i("seeders")))
    });
    println!("found {} release(s):", rels.len());
    for r in &rels {
        let mark = if r.b("rejected") { "✗" } else { "✓" };
        let seed = if r.has("seeders") {
            format!(" {}s", py_get(r, "seeders"))
        } else {
            String::new()
        };
        println!(
            "  {} {}MB  {}  {}{}  {}",
            mark,
            mb(r.i("size")),
            r.at(&["quality", "quality", "name"]).as_str().unwrap_or(""),
            r.s("protocol"),
            seed,
            r.s("title")
        );
        if r.b("rejected") {
            let rejs: Vec<&str> =
                r.a("rejections").iter().map(|x| x.as_str().unwrap_or("")).collect();
            println!("        reject: {}", rejs.join("; "));
        }
        println!("        guid={}  indexerId={}", py_get(r, "guid"), py_get(r, "indexerId"));
    }
}

pub fn cmd_monitor(svc: &str, args: &[String]) {
    if args.len() < 2 {
        die("monitor: need <id|query> <spec>");
    }
    let (ident, spec) = (&args[0], &args[1]);
    if svc == "radarr" {
        let val = match spec.to_lowercase().as_str() {
            "on" | "true" => true,
            "off" | "false" => false,
            _ => die("radarr monitor: on|off"),
        };
        let mid = resolve_id(svc, ident);
        let mut m = api(svc, "GET", &format!("/movie/{}", mid), None).unwrap_or(Value::Null);
        m["monitored"] = Value::Bool(val);
        api(svc, "PUT", &format!("/movie/{}", mid), Some(&m));
        println!("radarr movie {} monitored={}", mid, if val { "True" } else { "False" });
        return;
    }
    let sid = resolve_id(svc, ident);
    let mut s = api(svc, "GET", &format!("/series/{}", sid), None).unwrap_or(Value::Null);
    if spec == "all" || spec == "none" {
        let val = spec == "all";
        if let Some(seasons) = s.get_mut("seasons").and_then(Value::as_array_mut) {
            for se in seasons {
                se["monitored"] = Value::Bool(val);
            }
        }
    } else {
        let want: HashSet<i64> = spec
            .split(',')
            .map(|x| {
                x.trim_start_matches(['s', 'S'])
                    .parse::<i64>()
                    .unwrap_or_else(|_| die(&format!("monitor: bad season spec '{}'", spec)))
            })
            .collect();
        if let Some(seasons) = s.get_mut("seasons").and_then(Value::as_array_mut) {
            for se in seasons {
                let sn = se.i("seasonNumber");
                se["monitored"] = Value::Bool(want.contains(&sn));
            }
        }
    }
    api(svc, "PUT", &format!("/series/{}", sid), Some(&s));
    let on: Vec<String> = s
        .a("seasons")
        .iter()
        .filter(|se| se.b("monitored"))
        .map(|se| format!("S{}", se.i("seasonNumber")))
        .collect();
    println!(
        "updated {}: {}",
        s.s("title"),
        if on.is_empty() { "none".to_string() } else { on.join(",") }
    );
}

// --- queue -------------------------------------------------------------------

pub fn queue_records(svc: &str, page_size: i64) -> Value {
    let extra = if svc.starts_with("sonarr") {
        "includeUnknownSeriesItems=true&includeSeries=true&includeEpisode=true"
    } else {
        "includeUnknownMovieItems=true&includeMovie=true"
    };
    api(svc, "GET", &format!("/queue?pageSize={}&{}", page_size, extra), None)
        .unwrap_or(Value::Null)
}

pub fn queue_status_messages(r: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for sm in r.a("statusMessages") {
        for m in sm.a("messages") {
            out.push(m.as_str().unwrap_or("").to_string());
        }
    }
    out
}

pub fn is_stuck_queue_record(r: &Value) -> bool {
    let state = r.s("trackedDownloadState");
    r.s("status") == "failed"
        || state == "importBlocked"
        || state == "importPending"
        || !r.s("errorMessage").is_empty()
}

pub fn queue_record_summary(r: &Value) -> Value {
    let mut m = serde_json::Map::new();
    for k in ["id", "title", "status", "trackedDownloadStatus", "trackedDownloadState"] {
        m.insert(k.to_string(), r.get(k).cloned().unwrap_or(Value::Null));
    }
    m.insert("sizeleftMb".to_string(), Value::from(mb(r.i("sizeleft"))));
    for k in ["downloadId", "outputPath", "errorMessage"] {
        m.insert(k.to_string(), r.get(k).cloned().unwrap_or(Value::Null));
    }
    m.insert(
        "statusMessages".to_string(),
        Value::Array(queue_status_messages(r).into_iter().map(Value::String).collect()),
    );
    Value::Object(m)
}

/// A raw-disc queue record — full Blu-ray/DVD structure (ISO/BDMV/VIDEO_TS).
/// The arr classifies these as BR-DISK; they neither import nor feed the encoder,
/// so a queue full of them is wasted bytes. Quality name is authoritative; the
/// title tokens catch anything the classifier misses.
fn is_disc_record(r: &Value) -> bool {
    let qname = r.at(&["quality", "quality", "name"]).as_str().unwrap_or("");
    if qname == "BR-DISK" || qname == "Raw-HD" {
        return true;
    }
    let t = r.s("title").to_lowercase();
    ["bdmv", ".iso", " iso", "video_ts", "complete.bluray", "complete.uhd.bluray", "full.bluray", "complete blu-ray"]
        .iter()
        .any(|tok| t.contains(tok))
}

/// SAB's own per-job diagnosis: {nzo_id (lowercase): [labels]}. The labels
/// SAB sets are the actionable ones the arrs can't see — ENCRYPTED (password-
/// protected rar = fake release, job sits Paused forever), DUPLICATE,
/// ALTERNATIVE. Keyed lowercase because the arrs store downloadId in whatever
/// case they please. Any failure (SAB down, no key) -> empty map, matching the
/// Python's `except SystemExit: return {}`.
fn sab_flagged_labels() -> HashMap<String, Vec<String>> {
    let mut out = HashMap::new();
    let key = match try_sab_key() {
        Some(k) => k,
        None => return out,
    };
    let qs = form_encode(&[
        ("mode", "queue"),
        ("output", "json"),
        ("apikey", key.as_str()),
        ("start", "0"),
        ("limit", "100000"),
    ]);
    let body = match http10_get(SAB_PORT, &format!("/api/?{}", qs)) {
        Some(b) => b,
        None => return out,
    };
    let v: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return out,
    };
    for s in v.at(&["queue"]).a("slots") {
        let labels: Vec<String> =
            s.a("labels").iter().map(|l| l.as_str().unwrap_or("").to_string()).collect();
        out.insert(s.s("nzo_id").to_lowercase(), labels);
    }
    out
}

/// SAB api key without die() — env vars first, then the sops-rendered file
/// (arr-api only offers the die-on-missing variant).
fn try_sab_key() -> Option<String> {
    for var in ["ARR_API_KEY_SAB", "SABNZBD_API_KEY"] {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    let text = std::fs::read_to_string(env_file_path()).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == "SABNZBD_API_KEY" && !v.trim().is_empty() {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// Minimal fallible localhost HTTP GET (arr-api's sab_api dies on error; this
/// must survive SAB being down). HTTP/1.0 + Connection: close = no chunking,
/// read to EOF.
fn http10_get(port: u16, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(120))).ok()?;
    stream.set_write_timeout(Some(Duration::from_secs(120))).ok()?;
    write!(
        stream,
        "GET {} HTTP/1.0\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n",
        path, port
    )
    .ok()?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).to_string();
    let (head, body) = text.split_once("\r\n\r\n")?;
    if head.split_whitespace().nth(1)? != "200" {
        return None;
    }
    Some(body.to_string())
}

/// Filter queue records by any mix of selectors — the shared language of
/// `queue` (view) and `queue-rm` (act). positional: numeric queue ids (exact) or
/// a title substring (back-compat). flags: --title PAT, --disc, --quality NAME,
/// --status STATE (failed|stuck|downloading|paused|importing, or a raw status).
fn queue_select(records: &[Value], flags: &Flags, positional: &[String]) -> Vec<Value> {
    let mut sel: Vec<Value> = records.to_vec();
    let ids: HashSet<i64> =
        positional.iter().filter(|p| p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty())
            .filter_map(|p| p.parse().ok())
            .collect();
    let words: Vec<&String> =
        positional.iter().filter(|p| !(p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty())).collect();
    if !ids.is_empty() {
        sel.retain(|r| ids.contains(&r.i("id")));
    }
    let title_pat: Option<String> = match flags.val("--title") {
        Some(t) if !t.is_empty() => Some(t.to_string()),
        _ => {
            if !words.is_empty() {
                Some(words.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" "))
            } else {
                None
            }
        }
    };
    if let Some(pat) = title_pat {
        let tl = pat.to_lowercase();
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
    if let Some(qn) = flags.val("--quality") {
        if !qn.is_empty() {
            let qn = qn.to_lowercase();
            sel.retain(|r| {
                r.at(&["quality", "quality", "name"]).as_str().unwrap_or("").to_lowercase() == qn
            });
        }
    }
    if let Some(st) = flags.val("--status") {
        if !st.is_empty() {
            let st = st.to_lowercase();
            sel.retain(|r| match st.as_str() {
                "failed" => r.s("status") == "failed",
                "stuck" => is_stuck_queue_record(r),
                "downloading" => r.s("status") == "downloading",
                "paused" => r.s("status") == "paused",
                "importing" => {
                    let s = r.s("trackedDownloadState");
                    s == "importPending" || s == "importBlocked"
                }
                _ => r.s("status").to_lowercase() == st,
            });
        }
    }
    sel
}

/// List queue items. Filter with a title substring or --disc/--encrypted/
/// --quality NAME/--status STATE; --ids prints just the queue ids (pipe into
/// queue-rm).
pub fn cmd_queue(svc: &str, args: &[String]) {
    let (flags, rest) = pop_flags(
        args,
        &[("--disc", 0), ("--quality", 1), ("--status", 1), ("--title", 1), ("--ids", 0), ("--encrypted", 0)],
    );
    let q = queue_records(svc, 1000);
    let records: Vec<Value> = q.a("records").to_vec();
    let sel = queue_select(&records, &flags, &rest);
    if flags.has("--ids") {
        for r in &sel {
            println!("{}", r.i("id"));
        }
        return;
    }
    let filt = if sel.len() as i64 == q.i("totalRecords") {
        String::new()
    } else {
        format!(" (of {})", q.i("totalRecords"))
    };
    println!("queue: {} item(s){}", sel.len(), filt);
    let sab_labels = sab_flagged_labels();
    for r in &sel {
        let lbl = sab_labels.get(&r.s("downloadId").to_lowercase()).cloned().unwrap_or_default();
        let lbls = if lbl.is_empty() { String::new() } else { format!("[{}] ", lbl.join(",")) };
        println!(
            "  {}/{}  {}MB left  {}{}",
            py_get(r, "status"),
            py_get(r, "trackedDownloadState"),
            mb(r.i("sizeleft")),
            lbls,
            py_get(r, "title")
        );
        if !r.s("errorMessage").is_empty() {
            println!("        err: {}", r.s("errorMessage"));
        }
        let msgs = queue_status_messages(r);
        if !msgs.is_empty() {
            println!("        {}", msgs.join("; "));
        }
    }
}

// --- command polling ----------------------------------------------------------

/// Poll /command/<id> until it leaves queued/started; return the final record.
///
/// Replaces the `for i in seq…; sleep 5; raw GET /command/<id>` poll loops.
pub fn wait_command(svc: &str, cmd_id: i64, timeout: f64) -> Value {
    let start = now_f64();
    loop {
        let rec =
            api(svc, "GET", &format!("/command/{}", cmd_id), None).unwrap_or(Value::Null);
        let st = rec.s("status");
        if st != "queued" && st != "started" {
            return rec;
        }
        if now_f64() - start > timeout {
            return rec; // caller inspects .status (still queued/started == timed out)
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

/// Block until an arr command (search/grab/import/refresh/rename) finishes.
///
// --- search-command inspection ------------------------------------------------
// The arrs run searches as background "commands" (GET /command); a slow indexer
// can keep a SeriesSearch grinding per-episode for an hour+. These helpers make
// that state visible/controllable so nobody has to hand-poll the API with curl.

/// Parse "YYYY-MM-DDTHH:MM:SS[.fff]Z" → unix seconds. Inverse of policy's
/// utc_iso (Hinnant days_from_civil). None on anything unparsable.
pub fn iso_epoch(s: &str) -> Option<i64> {
    if s.len() < 19 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r).and_then(|t| t.parse::<i64>().ok());
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let yy = if m <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146097 + doe - 719468) * 86400 + h * 3600 + mi * 60 + sec)
}

pub fn fmt_age(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{}s", s)
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

/// Search-type commands from GET /command (the arrs keep recently-finished
/// ones listed for a few minutes), newest first. `item_id` filters to one
/// series/movie via the command body; EpisodeSearch commands carry only
/// episode ids, so they only appear in the unfiltered listing.
pub fn search_commands(svc: &str, item_id: Option<i64>) -> Vec<Value> {
    let cmds = api(svc, "GET", "/command", None).unwrap_or(Value::Null);
    let mut v: Vec<Value> = cmds
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c.s("name").ends_with("Search"))
        .filter(|c| match item_id {
            None => true,
            Some(id) => {
                let b = c.at(&["body"]);
                b.i("seriesId") == id
                    || b.a("movieIds").iter().any(|m| m.as_i64() == Some(id))
            }
        })
        .collect();
    v.sort_by_key(|c| std::cmp::Reverse(c.i("id")));
    v
}

/// One display line per command: id, name, state, age, live message.
/// A search that's been running >15m gets a ⚠ — with healthy indexers even a
/// full per-episode series sweep finishes well inside that.
pub fn search_command_line(c: &Value, now: i64) -> String {
    let st = c.s("status");
    let (stamp, verb) = match st {
        "started" => (c.s("started"), "running"),
        "queued" => (c.s("queued"), "queued"),
        _ => (c.s("ended"), st),
    };
    let age = iso_epoch(stamp).map(|t| fmt_age(now - t)).unwrap_or_default();
    let warn = if st == "started" && iso_epoch(c.s("started")).map_or(false, |t| now - t > 900) {
        " ⚠"
    } else {
        ""
    };
    let msg = c.s("message");
    format!(
        "#{} {} {} {}{}{}",
        c.i("id"),
        c.s("name"),
        verb,
        age,
        warn,
        if msg.is_empty() { String::new() } else { format!(" — {}", msg) }
    )
}

/// arr <svc> searches [id|query] — the arr's active/recent search commands.
/// Answers "is the search still going / where is it / is it stuck?" after a
/// grab/add reports nothing landed yet.
pub fn cmd_searches(svc: &str, args: &[String]) {
    let (_flags, rest) = pop_flags(args, &[]);
    let item_id = rest.first().map(|q| crate::disk::resolve_soft(svc, q))
        .transpose()
        .unwrap_or_else(|e| die(&e));
    let cmds = search_commands(svc, item_id);
    if cmds.is_empty() {
        println!(
            "no active or recent search commands{} on {} (finished ones drop off the list after a few minutes)",
            item_id.map(|i| format!(" for item {}", i)).unwrap_or_default(),
            svc
        );
        return;
    }
    let now = now_epoch();
    for c in &cmds {
        println!("{}", search_command_line(c, now));
    }
    if cmds.iter().any(|c| c.s("status") == "queued") {
        println!("  (a QUEUED command can be cancelled: arr {} cancel <id>)", svc);
    }
    if cmds.iter().any(|c| c.s("status") == "started") {
        println!(
            "  (a STARTED command runs to completion — the arrs can't cancel it; a narrower search runs in parallel, e.g. arr {} grab <id> --season N)",
            svc
        );
    }
}

/// arr <svc> cancel <commandId...> — cancel QUEUED arr commands
/// (DELETE /command/{id}). The arrs refuse to cancel a command that has
/// already STARTED executing (HTTP 409) — those run to completion; the move
/// then is to fire a narrower search alongside (searches run in parallel).
pub fn cmd_cancel(svc: &str, args: &[String]) {
    let (_flags, rest) = pop_flags(args, &[]);
    if rest.is_empty() {
        die("cancel: need command id(s) — see `arr <svc> searches`");
    }
    let mut failed = false;
    for id_s in &rest {
        let id: i64 = id_s
            .parse()
            .unwrap_or_else(|_| die(&format!("cancel: bad command id '{}' (see `arr {} searches`)", id_s, svc)));
        let before = api(svc, "GET", &format!("/command/{}", id), None).unwrap_or(Value::Null);
        match try_api(svc, "DELETE", &format!("/command/{}", id), None, 120) {
            Ok(_) => println!(
                "cancelled #{} {} (was {})",
                id,
                before.s("name"),
                py_get(&before, "status")
            ),
            Err(ApiError::Http { code: 409, .. }) => {
                failed = true;
                println!(
                    "can't cancel #{} {} — it already STARTED ({}), and the arrs only cancel queued commands. It will finish on its own; a narrower search can run in parallel meanwhile (e.g. grab --season N)",
                    id,
                    before.s("name"),
                    py_get(&before, "status")
                );
            }
            Err(e) => {
                failed = true;
                println!(
                    "FAILED to cancel #{}: {}",
                    id,
                    crate::acquire::api_err_msg("DELETE", &format!("/command/{}", id), 120, &e)
                );
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
}

pub fn now_epoch() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// One-line indexer-health summary from Prowlarr's /indexerstatus (per-indexer
/// failure backoff): "3/4 prowlarr indexers backed off (Nyaa 34m left, …)".
/// This is the difference between "releases are scarce" and "the indexers are
/// down" — the diagnosis a bare zero-grab can't make. Prowlarr is the source
/// because every arr indexer here is Prowlarr-proxied and the arrs themselves
/// don't expose /indexerstatus (404 on sonarr/radarr v4).
/// None when Prowlarr isn't reachable (diagnosis must never break flow).
pub fn indexer_backoff_line() -> Option<String> {
    let idx = try_api("prowlarr", "GET", "/indexer", None, 30).ok()??;
    let enabled: HashMap<i64, String> = idx
        .as_array()?
        .iter()
        .filter(|i| i.b("enable"))
        .map(|i| (i.i("id"), i.s("name").to_string()))
        .collect();
    if enabled.is_empty() {
        return Some("prowlarr has NO enabled indexers — searches can't find anything".into());
    }
    let status = try_api("prowlarr", "GET", "/indexerstatus", None, 30).ok()??;
    let now = now_epoch();
    let mut off: Vec<String> = vec![];
    for s in status.as_array()? {
        let Some(name) = enabled.get(&s.i("indexerId")) else { continue };
        if let Some(t) = iso_epoch(s.s("disabledTill")) {
            if t > now {
                off.push(format!("{} {} left", name, fmt_age(t - now)));
            }
        }
    }
    Some(if off.is_empty() {
        format!("all {} prowlarr indexers healthy (no failure backoff)", enabled.len())
    } else {
        format!(
            "{}/{} prowlarr indexers backed off after failures ({})",
            off.len(),
            enabled.len(),
            off.join(", ")
        )
    })
}

/// arr <svc> wait <commandId> [--timeout SECONDS]
/// Command ids come from `grab`, `import`, or any `raw POST /command`. Exits
/// non-zero unless the command reached 'completed'.
pub fn cmd_wait(svc: &str, args: &[String]) {
    let (flags, rest) = pop_flags(args, &[("--timeout", 1)]);
    if rest.is_empty() {
        die("wait: need a command id (e.g. printed by `arr <svc> grab/import`)");
    }
    let cmd_id: i64 =
        rest[0].parse().unwrap_or_else(|_| die(&format!("wait: bad command id '{}'", rest[0])));
    let timeout: i64 = flags
        .val_or("--timeout", "300")
        .parse()
        .unwrap_or_else(|_| die(&format!("wait: bad --timeout '{}'", flags.val_or("--timeout", ""))));
    let rec = wait_command(svc, cmd_id, timeout as f64);
    let st = rec.s("status").to_string();
    let name = if !rec.s("commandName").is_empty() {
        rec.s("commandName").to_string()
    } else if !rec.s("name").is_empty() {
        rec.s("name").to_string()
    } else {
        format!("command {}", rest[0])
    };
    let msg = if !rec.s("message").is_empty() {
        format!("  {}", rec.s("message"))
    } else {
        String::new()
    };
    println!("{}: {}{}", name, py_get(&rec, "status"), msg);
    match rec.get("exception") {
        Some(Value::String(e)) if !e.is_empty() => {
            println!("  exception: {}", e.chars().take(300).collect::<String>());
        }
        Some(v) if !v.is_null() => {
            println!("  exception: {}", py_repr(v).chars().take(300).collect::<String>());
        }
        _ => {}
    }
    if st != "completed" {
        std::process::exit(2);
    }
}

/// List a series' episodes WITH ids — what you need to grab/import by episode.
///
/// arr <svc> episodes <id|query> [--season N] [--missing] [--monitored] [--json]
/// Columns: epId  SxxEyy  abs=ABS  ON/OFF(file)  m/-(monitored)  title
/// Replaces `raw GET /episode?seriesId=N | jq …` for finding missing-ep ids.
pub fn cmd_episodes(svc: &str, args: &[String]) {
    if !svc.starts_with("sonarr") {
        die("episodes: sonarr only");
    }
    let (flags, rest) =
        pop_flags(args, &[("--season", 1), ("--missing", 0), ("--monitored", 0), ("--json", 0)]);
    if rest.is_empty() {
        die("episodes: need a series id or query");
    }
    let sid = resolve_id(svc, &rest[0]);
    let resp = api(svc, "GET", &format!("/episode?seriesId={}", sid), None).unwrap_or(Value::Null);
    let season: Option<i64> = flags.val("--season").map(|v| {
        v.parse().unwrap_or_else(|_| die(&format!("episodes: bad --season '{}'", v)))
    });
    let mut rows: Vec<Value> = Vec::new();
    for e in resp.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        if let Some(sn) = season {
            if e.i("seasonNumber") != sn {
                continue;
            }
        }
        if flags.has("--missing") && e.b("hasFile") {
            continue;
        }
        if flags.has("--monitored") && !e.b("monitored") {
            continue;
        }
        rows.push(e.clone());
    }
    rows.sort_by_key(|e| (e.i("seasonNumber"), e.i("episodeNumber")));
    if flags.has("--json") {
        // hand-rendered to keep the Python dict's key order (id, season, …)
        if rows.is_empty() {
            println!("[]");
            return;
        }
        let objs: Vec<String> = rows
            .iter()
            .map(|e| {
                format!(
                    "  {{\n    \"id\": {},\n    \"season\": {},\n    \"episode\": {},\n    \"abs\": {},\n    \"hasFile\": {},\n    \"monitored\": {},\n    \"title\": {}\n  }}",
                    json_field(e, "id"),
                    json_field(e, "seasonNumber"),
                    json_field(e, "episodeNumber"),
                    json_field(e, "absoluteEpisodeNumber"),
                    json_field(e, "hasFile"),
                    json_field(e, "monitored"),
                    json_field(e, "title"),
                )
            })
            .collect();
        println!("[\n{}\n]", objs.join(",\n"));
        return;
    }
    let label = if flags.has("--missing") { " (missing)" } else { "" };
    println!("{} episode(s){}:", rows.len(), label);
    for e in &rows {
        let abs = e.i("absoluteEpisodeNumber");
        let abs_s = if abs != 0 { abs.to_string() } else { "-".to_string() };
        println!(
            "  {:<7} S{:02}E{:02}  abs={:<4} {} {}  {}",
            e.i("id"),
            e.i("seasonNumber"),
            e.i("episodeNumber"),
            abs_s,
            if e.b("hasFile") { "ON " } else { "OFF" },
            if e.b("monitored") { "m" } else { "-" },
            e.s("title")
        );
    }
}

/// One JSON scalar in Python json.dumps style (missing key -> null).
fn json_field(e: &Value, key: &str) -> String {
    match e.get(key) {
        None | Some(Value::Null) => "null".to_string(),
        Some(Value::Bool(b)) => (if *b { "true" } else { "false" }).to_string(),
        Some(Value::Number(n)) => {
            if n.is_f64() {
                py_float(n.as_f64().unwrap_or(0.0))
            } else {
                n.to_string()
            }
        }
        Some(Value::String(s)) => py_json_string(s),
        Some(other) => py_json_pretty(other),
    }
}

// --- history / wanted / raw / parse / search ----------------------------------

pub fn cmd_history(svc: &str, args: &[String]) {
    let rows: Vec<Value>;
    if !args.is_empty() {
        let hid = resolve_id(svc, &args[0]);
        if svc.starts_with("sonarr") {
            let resp = api(svc, "GET", &format!("/history/series?seriesId={}", hid), None)
                .unwrap_or(Value::Null);
            let mut rs: Vec<Value> = resp.as_array().cloned().unwrap_or_default();
            rs.sort_by(|a, b| b.s("date").cmp(a.s("date")));
            rs.truncate(20);
            rows = rs;
        } else {
            let data = api(svc, "GET", "/history?pageSize=100&sortKey=date&sortDirection=descending", None)
                .unwrap_or(Value::Null);
            rows = data
                .a("records")
                .iter()
                .filter(|r| r.i("movieId") == hid)
                .take(20)
                .cloned()
                .collect();
        }
    } else {
        let data = api(svc, "GET", "/history?pageSize=20&sortKey=date&sortDirection=descending", None)
            .unwrap_or(Value::Null);
        rows = data.a("records").to_vec();
    }
    for r in &rows {
        println!(
            "  {}  {}  {}",
            r.s("date").chars().take(19).collect::<String>(),
            r.s("eventType"),
            r.s("sourceTitle")
        );
    }
}

pub fn cmd_wanted(svc: &str, args: &[String]) {
    let _ = args;
    let sortk = if svc.starts_with("sonarr") { "airDateUtc" } else { "title" };
    let data = api(
        svc,
        "GET",
        &format!("/wanted/missing?pageSize=50&sortKey={}&sortDirection=descending", sortk),
        None,
    )
    .unwrap_or(Value::Null);
    println!("missing: {}", data.i("totalRecords"));
    for r in data.a("records").iter().take(40) {
        if r.get("episodeNumber").is_some() {
            let ser = match r.at(&["series"]).get("title") {
                None => "?".to_string(),
                Some(v) => py_scalar(v),
            };
            println!("  {}  S{}E{}  {}", ser, r.i("seasonNumber"), r.i("episodeNumber"), r.s("title"));
        } else {
            println!("  {} ({})", py_get(r, "title"), py_get(r, "year"));
        }
    }
}

pub fn cmd_raw(svc: &str, args: &[String]) {
    if args.len() < 2 {
        die("raw: usage: arr <svc> raw <METHOD> <path> [json-body]");
    }
    let method = args[0].to_uppercase();
    let mut path = args[1].clone();
    if !path.starts_with('/') {
        path = format!("/{}", path);
    }
    let body: Option<Value> = if args.len() > 2 {
        Some(serde_json::from_str(&args[2]).unwrap_or_else(|e| die(&format!("raw: bad json body: {}", e))))
    } else {
        None
    };
    let out = api(svc, &method, &path, body.as_ref());
    match out {
        Some(v) => println!("{}", py_json_pretty(&v)),
        None => println!("(empty response)"),
    }
}

/// Show how Sonarr/Radarr interprets a release title (series + episode map).
pub fn cmd_parse(svc: &str, args: &[String]) {
    if args.is_empty() {
        die("parse: need a release title");
    }
    let r = api(svc, "GET", &format!("/parse?title={}", urlquote(&args[0])), None)
        .unwrap_or(Value::Null);
    let pe = r.at(&["parsedEpisodeInfo"]);
    let series = r.at(&["series"]);
    println!("title: {}", args[0]);
    let stitle = series.s("title");
    println!("  series: {}", if stitle.is_empty() { "— NO MATCH —" } else { stitle });
    println!(
        "  parsed: season={} eps={} absolute={}",
        py_get(pe, "seasonNumber"),
        py_get(pe, "episodeNumbers"),
        py_get(pe, "absoluteEpisodeNumbers")
    );
    let mapped: Vec<String> = r
        .a("episodes")
        .iter()
        .map(|e| format!("S{}E{}", e.i("seasonNumber"), e.i("episodeNumber")))
        .collect();
    println!("  mapped: {}", if mapped.is_empty() { "— none —".to_string() } else { mapped.join(", ") });
}

/// Cross-indexer Prowlarr search with dedup + group/indexer filters.
pub fn cmd_search(svc: &str, args: &[String]) {
    if svc != "prowlarr" {
        die("search: prowlarr only (sonarr/radarr use status/releases)");
    }
    let (flags, rest) =
        pop_flags(args, &[("--group", 1), ("--indexer", 1), ("--limit", 1), ("--json", 0), ("--audio", 1)]);
    if rest.is_empty() {
        die("search: need a query");
    }
    let qs = form_encode(&[
        ("query", rest[0].as_str()),
        ("type", "search"),
        ("limit", flags.val_or("--limit", "100")),
    ]);
    let res = api_t("prowlarr", "GET", &format!("/search?{}", qs), None, SEARCH_TIMEOUT);
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut rows: Vec<Value> = Vec::new();
    for r in res.as_ref().and_then(Value::as_array).map(|a| a.as_slice()).unwrap_or(&[]) {
        let k = (py_get(r, "title"), py_get(r, "indexer"));
        if seen.contains(&k) {
            continue;
        }
        seen.insert(k);
        rows.push(r.clone());
    }
    if let Some(g) = flags.val("--group") {
        if !g.is_empty() {
            let g = g.to_lowercase();
            rows.retain(|r| r.s("title").to_lowercase().contains(&g));
        }
    }
    if let Some(ix) = flags.val("--indexer") {
        if !ix.is_empty() {
            let ix = ix.to_lowercase();
            rows.retain(|r| r.s("indexer").to_lowercase().contains(&ix));
        }
    }
    if let Some(a) = flags.val("--audio") {
        if !a.is_empty() {
            // title-token heuristic (Dual Audio / English Dub / Multi ...) — the
            // dub-hunting filter; releases don't carry real audio metadata
            let want = norm_lang(a);
            rows.retain(|r| audio_match(r.s("title"), &want));
        }
    }
    if flags.has("--json") {
        println!("{}", py_json_pretty(&Value::Array(rows)));
        return;
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.i("size")));
    println!("results: {}", rows.len());
    for r in rows.iter().take(100) {
        let sd = if r.has("seeders") { format!(" {}s", py_get(r, "seeders")) } else { String::new() };
        println!(
            "  [{}] {}MB {}{}  {}",
            py_get(r, "indexer"),
            mb(r.i("size")),
            py_get(r, "protocol"),
            sd,
            py_get(r, "title")
        );
    }
}

// --- per-season coverage (exact, from the episode list) ------------------------
// Private copies for cmd_status's single-match path; the coverage command
// proper lives in policy.rs (dedupe later).




// --- disk audit (private copy for cmd_status's proactive warning) --------------
// The full audit command lives in disk.rs; this is just enough of arr.py's
// _disk_audit to drive _audit_warn (dedupe later).

const VIDEO_EXTS: [&str; 11] =
    [".mkv", ".mp4", ".m4v", ".avi", ".m2ts", ".ts", ".wmv", ".mov", ".webm", ".mpg", ".mpeg"];

struct Unmanaged {
    path: PathBuf,
    dup_of: Option<String>,
    version: bool,
}

fn guess_ep(name: &str) -> Option<(i64, i64)> {
    // re.search(r"[Ss](\d{1,2})[Ee](\d{1,3})", name), backtracking included
    let b = name.as_bytes();
    for i in 0..b.len() {
        if b[i] != b's' && b[i] != b'S' {
            continue;
        }
        let dstart = i + 1;
        let mut j = dstart;
        while j < b.len() && b[j].is_ascii_digit() && j - dstart < 2 {
            j += 1;
        }
        if j == dstart {
            continue;
        }
        for dlen in (1..=(j - dstart)).rev() {
            let k = dstart + dlen;
            if k >= b.len() || (b[k] != b'e' && b[k] != b'E') {
                continue;
            }
            let estart = k + 1;
            let mut m = estart;
            while m < b.len() && b[m].is_ascii_digit() && m - estart < 3 {
                m += 1;
            }
            if m == estart {
                continue;
            }
            let s_num: i64 = std::str::from_utf8(&b[dstart..k]).ok()?.parse().ok()?;
            let e_num: i64 = std::str::from_utf8(&b[estart..m]).ok()?.parse().ok()?;
            return Some((s_num, e_num));
        }
    }
    None
}

fn walk_videos(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for ent in rd.flatten() {
                let name = ent.file_name().to_string_lossy().to_string();
                let is_dir = ent.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if is_dir {
                    if !name.starts_with('.') {
                        stack.push(ent.path());
                    }
                } else {
                    let lower = name.to_lowercase();
                    if VIDEO_EXTS.iter().any(|e| lower.ends_with(e)) {
                        out.push(ent.path());
                    }
                }
            }
        }
    }
    out
}

fn realpath(p: &str) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p))
}

/// Compare video files ON DISK under an item's folder with the files the arr
/// TRACKS. Anything untracked is what makes Jellyfin show duplicate episodes /
/// a wrong 'first episode'. Returns (item, unmanaged|None); None = folder not
/// visible from here (can't audit).
fn disk_audit(svc: &str, iid: i64, item: Option<&Value>) -> (Value, Option<Vec<Unmanaged>>) {
    let is_series = svc.starts_with("sonarr");
    let (item, recs): (Value, Vec<Value>) = if is_series {
        let it = match item {
            Some(v) => v.clone(),
            None => api(svc, "GET", &format!("/series/{}", iid), None).unwrap_or(Value::Null),
        };
        let r = api(svc, "GET", &format!("/episodefile?seriesId={}", iid), None)
            .as_ref()
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        (it, r)
    } else {
        let it = match item {
            Some(v) => v.clone(),
            None => api("radarr", "GET", &format!("/movie/{}", iid), None).unwrap_or(Value::Null),
        };
        let r = api("radarr", "GET", &format!("/moviefile?movieId={}", iid), None)
            .as_ref()
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        (it, r)
    };
    let root = item.s("path").to_string();
    if root.is_empty() || !Path::new(&root).is_dir() {
        return (item, None);
    }
    let mut tracked: HashSet<PathBuf> = HashSet::new();
    for f in &recs {
        let p = if !f.s("path").is_empty() {
            f.s("path").to_string()
        } else {
            Path::new(&root).join(f.s("relativePath")).to_string_lossy().to_string()
        };
        tracked.insert(realpath(&p));
    }
    // authoritative (season, episode) -> tracked file map from the episode
    // list, so absolute-numbered anime releases resolve correctly
    let mut by_ep: HashMap<(i64, i64), String> = HashMap::new();
    if is_series {
        let rec_by_id: HashMap<i64, &Value> = recs.iter().map(|f| (f.i("id"), f)).collect();
        let eps = api(svc, "GET", &format!("/episode?seriesId={}", iid), None).unwrap_or(Value::Null);
        for e in eps.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
            let key = (e.i("seasonNumber"), e.i("episodeNumber"));
            if let Some(r) = rec_by_id.get(&e.i("episodeFileId")) {
                by_ep.insert(key, r.s("relativePath").to_string());
            }
        }
    }
    let folder = Path::new(root.trim_end_matches('/'))
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut unmanaged = Vec::new();
    for p in walk_videos(Path::new(&root)) {
        if tracked.contains(&realpath(&p.to_string_lossy())) {
            continue;
        }
        let base_name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let mut ep = if is_series { guess_ep(&base_name) } else { None };
        let mut version = false;
        let dup: Option<String>;
        if is_series {
            if ep.is_none() {
                // no SxxEyy in the name (absolute-numbered anime release) —
                // let Sonarr's own parser map it, but only trust a same-series hit
                let pr = api(svc, "GET", &format!("/parse?title={}", urlquote(&base_name)), None)
                    .unwrap_or(Value::Null);
                let pes = pr.a("episodes");
                if !pes.is_empty() && pr.at(&["series"]).i("id") == iid {
                    ep = Some((pes[0].i("seasonNumber"), pes[0].i("episodeNumber")));
                }
            }
            dup = ep
                .and_then(|k| by_ep.get(&k).cloned())
                .filter(|s| !s.is_empty());
        } else {
            // any extra video beside a tracked movie file duplicates it
            dup = recs
                .first()
                .map(|r| {
                    let rp = r.s("relativePath");
                    if !rp.is_empty() { rp.to_string() } else { r.s("path").to_string() }
                })
                .filter(|s| !s.is_empty());
            // "Movie (Year) - Some Label.mkv" beside the folder of the same name
            // is Jellyfin's INTENTIONAL multi-version convention (shows a version
            // picker, not a duplicate) — flag it, don't treat it as junk.
            let stem = base_name.rsplit_once('.').map(|(s, _)| s.to_string()).unwrap_or(base_name.clone());
            version = stem.starts_with(&format!("{} - ", folder));
        }
        unmanaged.push(Unmanaged { path: p, dup_of: dup, version });
    }
    unmanaged.sort_by(|a, b| a.path.cmp(&b.path));
    (item, Some(unmanaged))
}

/// Piggybacked proactive check for status/coverage/add — never raises.
pub fn audit_warn(svc: &str, iid: i64, item: Option<&Value>) {
    let (_, un) = disk_audit(svc, iid, item);
    let un: Vec<Unmanaged> = match un {
        Some(u) => u.into_iter().filter(|u| !u.version).collect(),
        None => return,
    };
    if un.is_empty() {
        return;
    }
    let dups = un.iter().filter(|u| u.dup_of.is_some()).count();
    println!(
        "  ⚠ {} unmanaged video file(s) on disk that {} does NOT track{}",
        un.len(),
        svc,
        if dups > 0 {
            format!(" — {} duplicate an episode/movie already on disk", dups)
        } else {
            String::new()
        }
    );
    println!(
        "    (Jellyfin will show these as duplicates/wrong versions) `arr {} audit {}` to inspect, `--quarantine --yes` to fix",
        svc, iid
    );
}
