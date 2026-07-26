//! disk.rs — the `arr sab *` family, on-disk file listing (`files`), the
//! disk-vs-DB audit (`audit` + the proactive ⚠ warning), delete (surgical and
//! whole-item, plus the top-level service-agnostic `arr delete`), and the
//! identity/metadata commands (`jellyfin unwatched`, `availability`, `lookup`,
//! `info`). Ported from arr.py lines 2382-3076; output strings are API.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use arr_api::http::form_encode;
use arr_api::{
    api, api_t, die, fmt_gb, jf_api, mb, parse_seasons, pop_flags, resolve_id, sab_api, try_api,
    JsonExt,
};

const SEARCH_TIMEOUT: u64 = 300; // interactive indexer searches (/release) can take minutes

// --- small Python-parity helpers ---------------------------------------------

/// Render a JSON value the way Python's `"%s" %` renders the equivalent object:
/// None/True/False, bare strings, ints as ints.
fn pys(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(s) => s.clone(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(f) = n.as_f64() {
                if f.is_finite() && f == f.trunc() && f.abs() < 1e15 {
                    format!("{:.1}", f)
                } else {
                    format!("{}", f)
                }
            } else {
                n.to_string()
            }
        }
        other => other.to_string(),
    }
}

/// Python truthiness for a JSON value.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `s[:n]` (character slice).
fn trunc(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// `api(...) or []` — response as a list, empty on None/non-array.
fn as_list(v: Option<Value>) -> Vec<Value> {
    match v {
        Some(Value::Array(a)) => a,
        _ => vec![],
    }
}

/// urllib.parse.quote (safe='/'): unreserved + '/' kept, rest %XX on UTF-8 bytes.
fn py_quote(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' | b'/' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Python round() — banker's rounding to int.
fn py_round(x: f64) -> i64 {
    let f = x.floor();
    let diff = x - f;
    if diff > 0.5 {
        f as i64 + 1
    } else if diff < 0.5 {
        f as i64
    } else {
        let fi = f as i64;
        if fi % 2 == 0 {
            fi
        } else {
            fi + 1
        }
    }
}

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

fn dirname(p: &str) -> &str {
    match p.rfind('/') {
        Some(0) => "/",
        Some(i) => &p[..i],
        None => "",
    }
}

fn path_join(a: &str, b: &str) -> String {
    if b.starts_with('/') {
        b.to_string()
    } else if a.ends_with('/') {
        format!("{}{}", a, b)
    } else {
        format!("{}/{}", a, b)
    }
}

/// os.path.splitext on a basename.
fn splitext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && !name[..i].chars().all(|c| c == '.') => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// os.path.realpath — canonicalize when the path exists, else pass through.
fn realpath(p: &str) -> String {
    fs::canonicalize(p)
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_else(|_| p.to_string())
}

/// time.strftime("%Y%m%d") — local date.
fn today_yyyymmdd() -> String {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!("{:04}{:02}{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)
    }
}

fn queue_records(svc: &str, page_size: i64) -> Value {
    let extra = if svc.starts_with("sonarr") {
        "includeUnknownSeriesItems=true&includeSeries=true&includeEpisode=true"
    } else {
        "includeUnknownMovieItems=true&includeMovie=true"
    };
    api(svc, "GET", &format!("/queue?pageSize={}&{}", page_size, extra), None)
        .unwrap_or(Value::Null)
}

/// POST /Library/Refresh, distinguishing success from failure (Python wraps the
/// die-on-error jf_api in `except SystemExit`; our jf_api can't be caught, so
/// this is a minimal fallible HTTP client for the one call that needs it).
fn try_jf_refresh() -> bool {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;
    let addr: std::net::SocketAddr = ([127, 0, 0, 1], arr_api::JELLYFIN_PORT).into();
    let mut s = match TcpStream::connect_timeout(&addr, Duration::from_secs(60)) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(60)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(60)));
    let req = format!(
        "POST /Library/Refresh HTTP/1.1\r\nHost: localhost:{}\r\nX-Emby-Token: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        arr_api::JELLYFIN_PORT,
        arr_api::jf_key()
    );
    if s.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() >= 512 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(0);
    (200..400).contains(&code)
}

// --- arr sab * ----------------------------------------------------------------

pub fn sab_queue(args: &[String]) {
    let q = sab_api("queue", &[("limit", "500")], 120).at(&["queue"]).clone();
    let pat = args.first().map(|a| a.to_lowercase());
    println!(
        "queue: {} items, {}MB left, speed={}KB/s, status={}{}",
        pys(q.at(&["noofslots"])),
        pys(q.at(&["mbleft"])),
        pys(q.at(&["kbpersec"])),
        pys(q.at(&["status"])),
        if truthy(q.at(&["paused"])) { " (PAUSED)" } else { "" }
    );
    for s in q.a("slots") {
        if let Some(p) = &pat {
            if !s.s("filename").to_lowercase().contains(p) {
                continue;
            }
        }
        println!(
            "  {}% {} prio={}  {}",
            pys(s.at(&["percentage"])),
            pys(s.at(&["status"])),
            pys(s.at(&["priority"])),
            trunc(s.s("filename"), 70)
        );
    }
}

pub fn sab_status(_args: &[String]) {
    let q = sab_api("queue", &[("limit", "1")], 120).at(&["queue"]).clone();
    println!(
        "speed={}KB/s status={} paused={} mbleft={}",
        pys(q.at(&["kbpersec"])),
        pys(q.at(&["status"])),
        pys(q.at(&["paused"])),
        pys(q.at(&["mbleft"]))
    );
    let stats = sab_api("server_stats", &[], 120);
    if let Some(servers) = stats.get("servers").and_then(Value::as_object) {
        for (name, s) in servers {
            let day = s.at(&["day"]).as_f64().unwrap_or(0.0);
            println!("  server {}: today={}MB", name, py_round(day / 1048576.0));
        }
    }
    let w = sab_api("warnings", &[], 120);
    let warns = w.a("warnings");
    if !warns.is_empty() {
        println!("recent warnings:");
        let start = warns.len().saturating_sub(5);
        for x in &warns[start..] {
            let text = if x.is_object() { pys(x.at(&["text"])) } else { pys(x) };
            println!("  {}", text);
        }
    }
}

fn sab_add_url(url: &str, cat: &str, name: Option<&str>) -> bool {
    let mut params: Vec<(&str, &str)> = vec![("name", url), ("cat", cat)];
    if let Some(n) = name {
        if !n.is_empty() {
            params.push(("nzbname", n));
        }
    }
    let r = sab_api("addurl", &params, 120);
    r.is_object() && matches!(r.get("status"), Some(Value::Bool(true)))
}

pub fn sab_add(args: &[String]) {
    let (flags, rest) = pop_flags(args, &[("--cat", 1), ("--name", 1)]);
    if rest.is_empty() {
        die("sab add: need an NZB url");
    }
    let ok = sab_add_url(&rest[0], flags.val_or("--cat", "*"), flags.val("--name"));
    println!("{}", if ok { "added" } else { "FAILED" });
}

/// arr sab prio <pattern> [--top|--force|--normal] — reprioritize queue items.
pub fn sab_prio(args: &[String]) {
    let (flags, rest) = pop_flags(args, &[("--top", 0), ("--force", 0), ("--normal", 0)]);
    if rest.is_empty() {
        die("sab prio: need a filename pattern");
    }
    let pat = rest[0].to_lowercase();
    let prio = if flags.has("--normal") { "0" } else { "2" }; // 2=Force (default)
    let q = sab_api("queue", &[("limit", "500")], 120).at(&["queue"]).clone();
    let hits: Vec<&Value> = q
        .a("slots")
        .iter()
        .filter(|s| s.s("filename").to_lowercase().contains(&pat))
        .collect();
    for s in &hits {
        sab_api(
            "queue",
            &[("name", "priority"), ("value", s.s("nzo_id")), ("value2", prio)],
            120,
        );
        if flags.has("--top") || flags.has("--force") {
            sab_api("switch", &[("value", s.s("nzo_id")), ("value2", "0")], 120);
        }
        println!("  prioritized: {}", trunc(s.s("filename"), 60));
    }
    println!("({} item(s))", hits.len());
}

pub fn sab_history(args: &[String]) {
    let pat = args.first().map(|a| a.to_lowercase());
    let h = sab_api("history", &[("limit", "50")], 120).at(&["history"]).clone();
    for s in h.a("slots") {
        if let Some(p) = &pat {
            if !s.s("name").to_lowercase().contains(p) {
                continue;
            }
        }
        let fail = if truthy(s.at(&["fail_message"])) {
            format!("  ! {}", s.s("fail_message"))
        } else {
            String::new()
        };
        println!("  {}  {}{}", pys(s.at(&["status"])), trunc(s.s("name"), 60), fail);
    }
}

/// arr sab cleanup <pattern> [--yes] — delete completed downloads (and their
/// files) matching a name pattern via the SAB API. SAB removes the files as the
/// sabnzbd user, sidestepping the ownership/permission walls that trip up rm.
pub fn sab_cleanup(args: &[String]) {
    let (flags, rest) = pop_flags(args, &[("--yes", 0)]);
    if rest.is_empty() {
        die("sab cleanup: need a name pattern");
    }
    let pat = rest[0].to_lowercase();
    let h = sab_api("history", &[("limit", "500")], 120).at(&["history"]).clone();
    let hits: Vec<&Value> = h
        .a("slots")
        .iter()
        .filter(|s| s.s("name").to_lowercase().contains(&pat))
        .collect();
    if hits.is_empty() {
        println!("no completed downloads matching '{}'", rest[0]);
        return;
    }
    let go = flags.has("--yes");
    println!(
        "{}{} completed download(s) matching '{}' (delete removes files too):",
        if go { "" } else { "[dry-run] " },
        hits.len(),
        rest[0]
    );
    for s in &hits {
        println!("  {}  {}", pys(s.at(&["status"])), trunc(s.s("name"), 64));
    }
    if !go {
        println!("  (pass --yes to delete history entries + files)");
        return;
    }
    let mut n = 0;
    for s in &hits {
        let r = sab_api(
            "history",
            &[("name", "delete"), ("value", s.s("nzo_id")), ("del_files", "1")],
            120,
        );
        if r.is_object() && truthy(r.at(&["status"])) {
            n += 1;
        }
    }
    println!("deleted {} item(s) + their files", n);
}

// --- files --------------------------------------------------------------------

fn qname(f: &Value) -> String {
    match f.at(&["quality", "quality", "name"]) {
        Value::Null => "?".into(),
        v => pys(v),
    }
}

/// List the files actually on disk for an item (path, size, quality).
pub fn cmd_files(svc: &str, args: &[String]) {
    let (flags, rest) = pop_flags(args, &[("--full", 0)]);
    if rest.is_empty() {
        die("files: need an id or query");
    }
    if svc.starts_with("sonarr") {
        let sid = resolve_id(svc, &rest[0]);
        let files = as_list(api(svc, "GET", &format!("/episodefile?seriesId={}", sid), None));
        let mut by_season: BTreeMap<i64, Vec<&Value>> = BTreeMap::new();
        for f in &files {
            by_season.entry(f.i("seasonNumber")).or_default().push(f);
        }
        let tot: i64 = files.iter().map(|f| f.i("size")).sum();
        println!("{} file(s), {}GB total", files.len(), fmt_gb(tot));
        for (sn, fs_) in &by_season {
            let t: i64 = fs_.iter().map(|f| f.i("size")).sum();
            println!("  S{}: {} files, {}GB", sn, fs_.len(), fmt_gb(t));
            if flags.has("--full") {
                let mut sorted_fs = fs_.clone();
                sorted_fs.sort_by(|a, b| a.s("relativePath").cmp(b.s("relativePath")));
                for f in sorted_fs {
                    println!(
                        "      {}MB  {}  {}",
                        mb(f.i("size")),
                        qname(f),
                        pys(f.at(&["relativePath"]))
                    );
                }
            }
        }
    } else {
        let mid = resolve_id("radarr", &rest[0]);
        let files = as_list(api("radarr", "GET", &format!("/moviefile?movieId={}", mid), None));
        println!("{} file(s):", files.len());
        for f in &files {
            let path = if truthy(f.at(&["relativePath"])) {
                f.s("relativePath").to_string()
            } else {
                pys(f.at(&["path"]))
            };
            println!("  {}GB  {}  {}", fmt_gb(f.i("size")), qname(f), path);
        }
    }
}

// --- disk audit: unmanaged files = Jellyfin duplicates ------------------------

const VIDEO_EXTS: &[&str] = &[
    ".mkv", ".mp4", ".m4v", ".avi", ".m2ts", ".ts", ".wmv", ".mov", ".webm", ".mpg", ".mpeg",
];
const SIDECAR_EXTS: &[&str] = &[".srt", ".ass", ".ssa", ".sub", ".idx", ".vtt", ".nfo"];
const QUARANTINE_ROOT: &str = "/data/hermes/quarantine";

/// re.search(r"[Ss](\d{1,2})[Ee](\d{1,3})", name)
fn guess_ep(name: &str) -> Option<(i64, i64)> {
    let cs: Vec<char> = name.chars().collect();
    let n = cs.len();
    for i in 0..n {
        if cs[i] != 'S' && cs[i] != 's' {
            continue;
        }
        for take in [2usize, 1] {
            let dstart = i + 1;
            let dend = i + 1 + take; // exclusive; E sits at dend
            if dend + 1 >= n {
                continue; // need room for 'E' and at least one digit
            }
            if !(dstart..dend).all(|k| cs[k].is_ascii_digit()) {
                continue;
            }
            if cs[dend] != 'E' && cs[dend] != 'e' {
                continue;
            }
            let mut k = dend + 1;
            while k < n && k - (dend + 1) < 3 && cs[k].is_ascii_digit() {
                k += 1;
            }
            if k == dend + 1 {
                continue;
            }
            let sn: String = cs[dstart..dend].iter().collect();
            let en: String = cs[dend + 1..k].iter().collect();
            return Some((sn.parse().ok()?, en.parse().ok()?));
        }
    }
    None
}

/// os.walk (topdown, dot-dirs pruned, unreadable dirs skipped) collecting video
/// files. Order is irrelevant downstream (results get path-sorted) but we sort
/// directory entries for determinism.
fn walk_videos(root: &Path, out: &mut Vec<String>) {
    let rd = match fs::read_dir(root) {
        Ok(r) => r,
        Err(_) => return,
    };
    let mut entries: Vec<fs::DirEntry> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    let mut dirs = vec![];
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let ft = match e.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_dir() {
            if !name.starts_with('.') {
                dirs.push(e.path());
            }
        } else {
            let l = name.to_lowercase();
            if VIDEO_EXTS.iter().any(|x| l.ends_with(x)) {
                out.push(e.path().to_string_lossy().into_owned());
            }
        }
    }
    for d in dirs {
        walk_videos(&d, out);
    }
}

/// One untracked on-disk video file, as reported by `disk_audit`.
#[derive(Debug, Clone)]
pub struct Unmanaged {
    pub path: String,
    pub size: i64,
    pub ep: Option<(i64, i64)>,
    pub dup_of: Option<String>,
    pub version: bool,
    pub sidecars: Vec<String>,
    pub importable: bool,
}

/// json.dumps(un, indent=2) — Python key order and ensure_ascii escaping.
fn py_json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) >= 0x20 && (c as u32) <= 0x7e => out.push(c),
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

fn dump_unmanaged_json(un: &[Unmanaged]) -> String {
    if un.is_empty() {
        return "[]".into();
    }
    let mut items = vec![];
    for u in un {
        let ep = match u.ep {
            Some((s, e)) => format!("[\n      {},\n      {}\n    ]", s, e),
            None => "null".into(),
        };
        let dup = match &u.dup_of {
            Some(d) => py_json_str(d),
            None => "null".into(),
        };
        let side = if u.sidecars.is_empty() {
            "[]".to_string()
        } else {
            format!(
                "[\n{}\n    ]",
                u.sidecars
                    .iter()
                    .map(|s| format!("      {}", py_json_str(s)))
                    .collect::<Vec<_>>()
                    .join(",\n")
            )
        };
        items.push(format!(
            "  {{\n    \"path\": {},\n    \"size\": {},\n    \"ep\": {},\n    \"dup_of\": {},\n    \"version\": {},\n    \"sidecars\": {},\n    \"importable\": {}\n  }}",
            py_json_str(&u.path),
            u.size,
            ep,
            dup,
            u.version,
            side,
            u.importable
        ));
    }
    format!("[\n{}\n]", items.join(",\n"))
}

/// Compare video files ON DISK under an item's folder with the files the arr
/// TRACKS. Anything untracked is what makes Jellyfin show duplicate episodes /
/// a wrong 'first episode' (e.g. a leftover Crunchyroll WEB-DL next to the
/// Sonarr-managed file). Returns (item, unmanaged|None); None = folder not
/// visible from here (can't audit).
pub fn disk_audit(svc: &str, iid: i64, item: Option<&Value>) -> (Value, Option<Vec<Unmanaged>>) {
    let series = svc.starts_with("sonarr");
    let (item, recs): (Value, Vec<Value>) = if series {
        let it = match item {
            Some(v) => v.clone(),
            None => api(svc, "GET", &format!("/series/{}", iid), None).unwrap_or(Value::Null),
        };
        let recs = as_list(api(svc, "GET", &format!("/episodefile?seriesId={}", iid), None));
        (it, recs)
    } else {
        let it = match item {
            Some(v) => v.clone(),
            None => api("radarr", "GET", &format!("/movie/{}", iid), None).unwrap_or(Value::Null),
        };
        let recs = as_list(api("radarr", "GET", &format!("/moviefile?movieId={}", iid), None));
        (it, recs)
    };
    let root = item.s("path").to_string();
    if root.is_empty() || !Path::new(&root).is_dir() {
        return (item, None);
    }
    let mut tracked: HashSet<String> = HashSet::new();
    let mut by_ep: HashMap<(i64, i64), Option<String>> = HashMap::new();
    let mut ep_has_file: HashMap<(i64, i64), bool> = HashMap::new();
    for f in &recs {
        let p = if truthy(f.at(&["path"])) {
            f.s("path").to_string()
        } else {
            path_join(&root, f.s("relativePath"))
        };
        tracked.insert(realpath(&p));
    }
    if series {
        // authoritative (season, episode) -> tracked file map from the episode
        // list, so absolute-numbered anime releases resolve correctly
        let mut rec_by_id: HashMap<i64, &Value> = HashMap::new();
        for f in &recs {
            rec_by_id.insert(f.i("id"), f);
        }
        let eps = as_list(api(svc, "GET", &format!("/episode?seriesId={}", iid), None));
        for e in &eps {
            let key = (e.i("seasonNumber"), e.i("episodeNumber"));
            ep_has_file.insert(key, e.b("hasFile"));
            if let Some(r) = rec_by_id.get(&e.i("episodeFileId")) {
                let rel = r.get("relativePath").and_then(Value::as_str).map(str::to_string);
                by_ep.insert(key, rel);
            }
        }
    }
    let mut unmanaged: Vec<Unmanaged> = vec![];
    let folder = basename(root.trim_end_matches('/')).to_string();
    let mut videos = vec![];
    walk_videos(Path::new(&root), &mut videos);
    for p in videos {
        if tracked.contains(&realpath(&p)) {
            continue;
        }
        let bn = basename(&p).to_string();
        let mut ep = if series { guess_ep(&bn) } else { None };
        let mut version = false;
        let dup: Option<String>;
        if series {
            if ep.is_none() {
                // no SxxEyy in the name (absolute-numbered anime release) —
                // let Sonarr's own parser map it, but only trust a same-series hit
                let pr = api(svc, "GET", &format!("/parse?title={}", py_quote(&bn)), None)
                    .unwrap_or(Value::Null);
                let pes = pr.a("episodes");
                if !pes.is_empty() && pr.at(&["series", "id"]).as_i64() == Some(iid) {
                    ep = Some((pes[0].i("seasonNumber"), pes[0].i("episodeNumber")));
                }
            }
            dup = ep.and_then(|k| by_ep.get(&k).cloned().flatten());
        } else {
            // any extra video beside a tracked movie file duplicates it
            dup = if let Some(r0) = recs.first() {
                match r0.get("relativePath").and_then(Value::as_str) {
                    Some(s) if !s.is_empty() => Some(s.to_string()),
                    _ => r0.get("path").and_then(Value::as_str).map(str::to_string),
                }
            } else {
                None
            };
            // "Movie (Year) - Some Label.mkv" beside the folder of the same name
            // is Jellyfin's INTENTIONAL multi-version convention (shows a version
            // picker, not a duplicate) — flag it, don't treat it as junk.
            let (stem, _) = splitext(&bn);
            version = stem.starts_with(&format!("{} - ", folder));
        }
        let size = fs::metadata(&p).map(|m| m.len() as i64).unwrap_or(0);
        let (base, _) = splitext(&bn);
        let mut side = vec![];
        let dir = dirname(&p).to_string();
        if let Ok(rd) = fs::read_dir(&dir) {
            for g in rd.flatten() {
                let gname = g.file_name().to_string_lossy().into_owned();
                let gl = gname.to_lowercase();
                if gname != bn
                    && gname.starts_with(base)
                    && SIDECAR_EXTS.iter().any(|x| gl.ends_with(x))
                {
                    side.push(path_join(&dir, &gname));
                }
            }
        }
        side.sort();
        let importable = series
            && ep.is_some()
            && dup.is_none()
            && !ep
                .map(|k| ep_has_file.get(&k).copied().unwrap_or(false))
                .unwrap_or(false);
        unmanaged.push(Unmanaged {
            path: p,
            size,
            ep,
            dup_of: dup,
            version,
            sidecars: side,
            importable,
        });
    }
    unmanaged.sort_by(|a, b| a.path.cmp(&b.path));
    (item, Some(unmanaged))
}


/// re.sub(r"[^A-Za-z0-9]+", "-", title).strip("-").lower()
fn slugify(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_lowercase()
}

fn quarantine_moves(
    svc: &str,
    item: &Value,
    unmanaged: &[&Unmanaged],
    dest: Option<&str>,
    go: bool,
) -> (String, i64) {
    let title = item.get("title").and_then(Value::as_str).unwrap_or("item");
    let slug = slugify(title);
    let dest = match dest {
        Some(d) => d.to_string(),
        None => format!("{}/{}-{}-{}", QUARANTINE_ROOT, svc, slug, today_yyyymmdd()),
    };
    let mut moved = 0i64;
    for u in unmanaged {
        let mut paths = vec![u.path.clone()];
        paths.extend(u.sidecars.iter().cloned());
        for p in paths {
            if !go {
                println!("  DRY: would move {} -> {}/", p, dest);
                continue;
            }
            if let Err(e) = fs::create_dir_all(&dest) {
                die(&format!(
                    "audit: move failed ({})\n  media-group write needed — the hermes agent has it; andrep needs the sudo-nix root path",
                    e
                ));
            }
            let bn = basename(&p).to_string();
            let mut tgt = path_join(&dest, &bn);
            let mut n = 1;
            while Path::new(&tgt).exists() {
                let (stem, ext) = splitext(&bn);
                tgt = path_join(&dest, &format!("{}.{}{}", stem, n, ext));
                n += 1;
            }
            // os.rename semantics: no cross-device fallback (quarantine lives on
            // the same bcachefs as the library, so rename is atomic and fine)
            if let Err(e) = fs::rename(&p, &tgt) {
                die(&format!(
                    "audit: move failed ({})\n  media-group write needed — the hermes agent has it; andrep needs the sudo-nix root path",
                    e
                ));
            }
            moved += 1;
            println!("  moved {} -> {}", p, tgt);
        }
    }
    (dest, moved)
}

/// Disk-vs-database audit: video files the arr does NOT track.
///
/// arr <svc> audit <id|query> [--quarantine] [--yes] [--dest DIR] [--json]
/// arr <svc> audit --all [--quiet]
/// Untracked files are what make Jellyfin show duplicate episodes (the leftover
/// "Crunchyroll WEB-DL" ghost next to the managed file), wrong first episodes,
/// etc. --quarantine MOVES them + their sidecar subs into a dated folder under
/// /data/hermes/quarantine (dry-run unless --yes; nothing is deleted — restore
/// = move back), then rescans the arr item and refreshes Jellyfin.
/// --all sweeps the whole library and reports only offenders (cron-friendly).
pub fn cmd_audit(svc: &str, args: &[String]) {
    if !(svc.starts_with("sonarr") || svc == "radarr") {
        die("audit: sonarr/sonarr-anime/radarr only");
    }
    let (flags, rest) = pop_flags(
        args,
        &[
            ("--quarantine", 0),
            ("--yes", 0),
            ("--dest", 1),
            ("--json", 0),
            ("--all", 0),
            ("--quiet", 0),
            ("--include-versions", 0),
        ],
    );
    let series = svc.starts_with("sonarr");
    if flags.has("--all") {
        let mut items = as_list(api(svc, "GET", if series { "/series" } else { "/movie" }, None));
        items.sort_by(|a, b| a.s("title").cmp(b.s("title")));
        let (mut bad, mut skipped) = (0i64, 0i64);
        for it in &items {
            let (_, un) = disk_audit(svc, it.i("id"), Some(it));
            match un {
                None => skipped += 1,
                Some(un) => {
                    let un: Vec<&Unmanaged> = un.iter().filter(|u| !u.version).collect();
                    if !un.is_empty() {
                        bad += 1;
                        let tot: i64 = un.iter().map(|u| u.size).sum();
                        println!(
                            "[{}] {} — {} unmanaged file(s), {}GB  (`arr {} audit {}`)",
                            it.i("id"),
                            it.s("title"),
                            un.len(),
                            fmt_gb(tot),
                            svc,
                            it.i("id")
                        );
                    }
                }
            }
        }
        if bad == 0 && !flags.has("--quiet") {
            let suffix = if skipped > 0 {
                format!(" ({} folders not visible, skipped)", skipped)
            } else {
                String::new()
            };
            println!("audit: no unmanaged files anywhere in {}{}", svc, suffix);
        }
        std::process::exit(if bad > 0 { 1 } else { 0 });
    }
    if rest.is_empty() {
        die("audit: need <id|query> or --all");
    }
    let iid = resolve_id(svc, &rest[0]);
    let (item, un) = disk_audit(svc, iid, None);
    let un = match un {
        None => die(&format!(
            "audit: folder {} not visible from this host — can't audit",
            pys(item.at(&["path"]))
        )),
        Some(u) => u,
    };
    if flags.has("--json") {
        println!("{}", dump_unmanaged_json(&un));
        return;
    }
    println!(
        "{} ({}) — {}",
        item.s("title"),
        pys(item.at(&["year"])),
        pys(item.at(&["path"]))
    );
    if un.is_empty() {
        println!("  clean: every video file on disk is tracked by {}", svc);
        return;
    }
    let tot: i64 = un.iter().map(|u| u.size).sum();
    println!(
        "  {} unmanaged video file(s) ({}GB) NOT tracked by {}:",
        un.len(),
        fmt_gb(tot),
        svc
    );
    let (mut importable, mut unknown) = (0i64, 0i64);
    for u in &un {
        let ep = match u.ep {
            Some((s, e)) => format!("S{:02}E{:02}", s, e),
            None => "?".into(),
        };
        println!("    {}  {}MB  {}", ep, mb(u.size), u.path);
        if u.version {
            println!("          JELLYFIN VERSION ('Title (Year) - Label' naming = intentional version picker) — skipped by --quarantine unless --include-versions");
        } else if u.dup_of.as_deref().map(|d| !d.is_empty()).unwrap_or(false) {
            println!("          DUPLICATE of tracked: {}", u.dup_of.as_deref().unwrap());
        } else if u.importable {
            importable += 1;
            println!("          UNIMPORTED — maps to {} which has NO tracked file", ep);
        } else {
            unknown += 1;
            println!("          unmatched — couldn't map to an episode; check by hand");
        }
    }
    if importable > 0 {
        println!(
            "  => {} file(s) are content for episodes with NO file — IMPORT them (`arr {} import '{}' --series {} --match <token> --map abs`), do NOT quarantine",
            importable,
            svc,
            item.s("path"),
            iid
        );
    }
    if unknown > 0 {
        println!(
            "  => {} unmatched file(s): verify what they are (`arr {} parse \"<name>\"`) before quarantining",
            unknown, svc
        );
    }
    let movable: Vec<&Unmanaged> = if flags.has("--include-versions") {
        un.iter().collect()
    } else {
        un.iter().filter(|u| !u.version).collect()
    };
    if !flags.has("--quarantine") {
        if !movable.is_empty() {
            println!("  => Jellyfin shows these as duplicate/ghost entries. Rerun with --quarantine [--yes] to move them out (kept, not deleted)");
        }
        return;
    }
    if movable.is_empty() {
        println!("  => only intentional version files here — nothing to quarantine (override with --include-versions)");
        return;
    }
    let go = flags.has("--yes");
    let (dest, moved) = quarantine_moves(svc, &item, &movable, flags.val("--dest"), go);
    if !go {
        let cnt: usize = movable.iter().map(|u| 1 + u.sidecars.len()).sum();
        println!(
            "  (dry-run — pass --yes to actually move {} file(s) to {})",
            cnt, dest
        );
        return;
    }
    println!("  quarantined {} file(s) -> {}", moved, dest);
    if series {
        api(svc, "POST", "/command", Some(&json!({"name": "RescanSeries", "seriesId": iid})));
    } else {
        api("radarr", "POST", "/command", Some(&json!({"name": "RescanMovie", "movieId": iid})));
    }
    if try_jf_refresh() {
        println!(
            "  rescan + Jellyfin refresh triggered — verify with `arr jellyfin has '{}'`",
            item.s("title")
        );
    } else {
        println!("  arr rescan triggered (Jellyfin refresh failed — run `arr jellyfin refresh`)");
    }
}

// --- delete -------------------------------------------------------------------

/// Like resolve_id but never exits — Ok(id) or Err(reason). Prefers an exact
/// title match when a substring is ambiguous. For batch delete, where one bad
/// title in a list shouldn't abort the whole run.
pub fn resolve_soft(svc: &str, q: &str) -> Result<i64, String> {
    if !q.is_empty() && q.chars().all(|c| c.is_ascii_digit()) {
        return Ok(q.parse().unwrap_or(-1));
    }
    let coll = if svc.starts_with("sonarr") { "series" } else { "movie" };
    let ql = q.to_lowercase();
    let mut hits: Vec<Value> = vec![];
    for it in as_list(api(svc, "GET", &format!("/{}", coll), None)) {
        let mut names: Vec<String> = vec![it.s("title").to_string(), it.s("titleSlug").to_string()];
        for a in it.a("alternateTitles") {
            names.push(a.s("title").to_string());
        }
        if names.iter().any(|n| n.to_lowercase().contains(&ql)) {
            hits.push(it);
        }
    }
    if hits.is_empty() {
        return Err(format!("no match for \"{}\"", q));
    }
    if hits.len() > 1 {
        let exact: Vec<&Value> = hits
            .iter()
            .filter(|h| h.s("title").to_lowercase() == ql)
            .collect();
        if exact.len() == 1 {
            return Ok(exact[0].i("id"));
        }
        let cands: Vec<String> = hits
            .iter()
            .take(6)
            .map(|h| format!("{} ({}) [{}]", h.s("title"), pys(h.at(&["year"])), h.i("id")))
            .collect();
        return Err(format!(
            "ambiguous \"{}\" — {} matches: {}",
            q,
            hits.len(),
            cands.join(", ")
        ));
    }
    Ok(hits[0].i("id"))
}

/// Delete media via the arr API (deletes as the service user — no rm/perms
/// fight — and updates the DB so it won't silently re-download). Dry-run unless
/// --yes. Accepts MANY titles/ids at once. Also cancels each item's active
/// downloads (removeFromClient) by default — pass --keep-downloads to leave the
/// queue alone, --blocklist to blocklist the cancelled releases.
pub fn cmd_delete(svc: &str, args: &[String]) {
    let (flags, rest) = pop_flags(
        args,
        &[
            ("--seasons", 1),
            ("--all", 0),
            ("--file-only", 0),
            ("--keep-monitored", 0),
            ("--keep-downloads", 0),
            ("--blocklist", 0),
            ("--yes", 0),
        ],
    );
    if rest.is_empty() {
        die("delete: need an id or query");
    }
    let go = flags.has("--yes");
    let tag = if go { "" } else { "[dry-run] " };
    let cancel = !flags.has("--keep-downloads");
    let blocklist = if flags.has("--blocklist") { "true" } else { "false" };
    let is_sonarr = svc.starts_with("sonarr");

    // --- surgical single-item modes: --seasons (sonarr) / --file-only (radarr) ---
    if is_sonarr && flags.has("--seasons") {
        if rest.len() != 1 {
            die("delete --seasons: one show at a time");
        }
        let sid = resolve_id(svc, &rest[0]);
        let mut s = api(svc, "GET", &format!("/series/{}", sid), None).unwrap_or(Value::Null);
        let allfiles = as_list(api(svc, "GET", &format!("/episodefile?seriesId={}", sid), None));
        let want = parse_seasons(flags.val_or("--seasons", ""));
        let files: Vec<&Value> = allfiles
            .iter()
            .filter(|f| want.contains(&f.i("seasonNumber")))
            .collect();
        let unmon = !flags.has("--keep-monitored");
        let seasons_str = want.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(",");
        let tot: i64 = files.iter().map(|f| f.i("size")).sum();
        println!(
            "{}'{}' seasons {}: delete {} file(s) ({}GB){}",
            tag,
            s.s("title"),
            seasons_str,
            files.len(),
            fmt_gb(tot),
            if unmon { " + unmonitor" } else { "" }
        );
        for sn in &want {
            let fs_: Vec<&&Value> = files.iter().filter(|f| f.i("seasonNumber") == *sn).collect();
            let t: i64 = fs_.iter().map(|f| f.i("size")).sum();
            println!("  S{}: {} files, {}GB", sn, fs_.len(), fmt_gb(t));
        }
        if !go {
            println!("  (pass --yes to delete)");
            return;
        }
        let ids: Vec<i64> = files.iter().map(|f| f.i("id")).collect();
        if !ids.is_empty() {
            api(svc, "DELETE", "/episodefile/bulk", Some(&json!({ "episodeFileIds": ids })));
        }
        if unmon {
            if let Some(seasons) = s.get_mut("seasons").and_then(Value::as_array_mut) {
                for se in seasons {
                    if want.contains(&se.i("seasonNumber")) {
                        se["monitored"] = Value::Bool(false);
                    }
                }
            }
            api(svc, "PUT", &format!("/series/{}", sid), Some(&s));
        }
        println!(
            "deleted {} file(s){}",
            ids.len(),
            if unmon { " + unmonitored" } else { "" }
        );
        return;
    }
    if !is_sonarr && flags.has("--file-only") {
        if rest.len() != 1 {
            die("delete --file-only: one movie at a time");
        }
        let mid = resolve_id("radarr", &rest[0]);
        let m = api("radarr", "GET", &format!("/movie/{}", mid), None).unwrap_or(Value::Null);
        let files = as_list(api("radarr", "GET", &format!("/moviefile?movieId={}", mid), None));
        let tot: i64 = files.iter().map(|f| f.i("size")).sum();
        println!(
            "{}'{}': delete file only ({}, {}GB) — keeps movie for re-grab",
            tag,
            m.s("title"),
            files.len(),
            fmt_gb(tot)
        );
        if !go {
            println!("  (pass --yes to delete)");
            return;
        }
        for f in &files {
            api("radarr", "DELETE", &format!("/moviefile/{}", f.i("id")), None);
        }
        println!("deleted {} file(s)", files.len());
        return;
    }

    // --- whole-item delete (one or many) ---
    if is_sonarr && !flags.has("--all") {
        die("sonarr delete: pass --seasons X-Y (surgical) or --all (whole series)");
    }
    let coll = if is_sonarr { "series" } else { "movie" };
    let key = if is_sonarr { "seriesId" } else { "movieId" };
    // Fetch the queue once and index by item id, so a big batch doesn't re-pull it.
    let mut by_item: HashMap<i64, Vec<Value>> = HashMap::new();
    if cancel {
        for r in queue_records(svc, 2000).a("records") {
            by_item.entry(r.i(key)).or_default().push(r.clone());
        }
    }

    let mut ok = 0i64;
    let empty: Vec<Value> = vec![];
    for q in &rest {
        let iid = match resolve_soft(svc, q) {
            Ok(i) => i,
            Err(err) => {
                println!("  ! {} — {}", q, err);
                continue;
            }
        };
        let item = api(svc, "GET", &format!("/{}/{}", coll, iid), None).unwrap_or(Value::Null);
        let what = if is_sonarr {
            let files = as_list(api(svc, "GET", &format!("/episodefile?seriesId={}", iid), None));
            let tot: i64 = files.iter().map(|f| f.i("size")).sum();
            format!(
                "SERIES '{}' + {} file(s) ({}GB)",
                item.s("title"),
                files.len(),
                fmt_gb(tot)
            )
        } else {
            format!("MOVIE '{}' + file ({}GB)", item.s("title"), fmt_gb(item.i("sizeOnDisk")))
        };
        let dls = by_item.get(&iid).unwrap_or(&empty);
        let extra = if !dls.is_empty() {
            format!(" + cancel {} download(s)", dls.len())
        } else {
            String::new()
        };
        println!("{}DELETE {}{}", tag, what, extra);
        if !go {
            continue;
        }
        for r in dls {
            // stop the client download first, while movieId still maps;
            // ignore errors (record already gone — fine)
            let _ = try_api(
                svc,
                "DELETE",
                &format!("/queue/{}?removeFromClient=true&blocklist={}", r.i("id"), blocklist),
                None,
                120,
            );
        }
        api(svc, "DELETE", &format!("/{}/{}?deleteFiles=true", coll, iid), None);
        let note = if !dls.is_empty() {
            format!(
                " (cancelled {} download{})",
                dls.len(),
                if dls.len() == 1 { "" } else { "s" }
            )
        } else {
            String::new()
        };
        println!("    deleted{}", note);
        ok += 1;
    }
    if !go {
        println!("  (pass --yes to delete)");
    } else {
        println!("done: {}/{} deleted", ok, rest.len());
    }
}

fn matches_in(libs: &mut HashMap<String, Vec<Value>>, svc: &str, q: &str) -> Vec<Value> {
    if !libs.contains_key(svc) {
        let coll = if svc.starts_with("sonarr") { "series" } else { "movie" };
        libs.insert(svc.to_string(), as_list(api(svc, "GET", &format!("/{}", coll), None)));
    }
    let lib = &libs[svc];
    if !q.is_empty() && q.chars().all(|c| c.is_ascii_digit()) {
        let id: i64 = q.parse().unwrap_or(-1);
        return lib.iter().filter(|it| it.i("id") == id).cloned().collect();
    }
    let ql = q.to_lowercase();
    let mut hits: Vec<Value> = vec![];
    for it in lib {
        let mut names: Vec<&str> = vec![it.s("title"), it.s("titleSlug")];
        names.extend(it.a("alternateTitles").iter().map(|a| a.s("title")));
        if names.iter().any(|n| n.to_lowercase().contains(&ql)) {
            hits.push(it.clone());
        }
    }
    let exact: Vec<Value> = hits
        .iter()
        .filter(|h| h.s("title").to_lowercase() == ql)
        .cloned()
        .collect();
    if exact.len() == 1 {
        exact
    } else {
        hits
    }
}

/// Top-level `arr delete <title|id> ... [--yes] [--keep-downloads]
/// [--blocklist]` — no service needed. Works out per title whether it's a movie
/// (Radarr) or a show (Sonarr/Sonarr-anime), cancels its active downloads, and
/// removes it. Mixed movie/show lists are fine. Dry-run unless --yes.
pub fn cmd_delete_auto(args: &[String]) {
    let (flags, rest) = pop_flags(args, &[("--keep-downloads", 0), ("--blocklist", 0), ("--yes", 0)]);
    if rest.is_empty() {
        die("delete: need one or more titles/ids");
    }
    let passthru: Vec<String> = ["--keep-downloads", "--blocklist", "--yes"]
        .iter()
        .copied()
        .filter(|f| flags.has(f))
        .map(str::to_string)
        .collect();
    let mut libs: HashMap<String, Vec<Value>> = HashMap::new(); // svc -> library list, fetched once each
    let svcs = ["radarr", "sonarr", "sonarr-anime"];

    let mut groups: HashMap<&str, Vec<String>> = HashMap::new();
    for q in &rest {
        let mut found: Vec<(&str, Value)> = vec![];
        for svc in svcs {
            for h in matches_in(&mut libs, svc, q) {
                found.push((svc, h));
            }
        }
        if found.is_empty() {
            println!("  ! {} — no movie or show matches", q);
            continue;
        }
        if found.len() > 1 {
            let where_: Vec<String> = found
                .iter()
                .take(6)
                .map(|(s, h)| format!("{}:{} ({}) [{}]", s, h.s("title"), pys(h.at(&["year"])), h.i("id")))
                .collect();
            println!(
                "  ! {} — matches {} items ({}); delete via `arr <svc> delete <id>`",
                q,
                found.len(),
                where_.join(", ")
            );
            continue;
        }
        let (svc, h) = &found[0];
        groups.entry(*svc).or_default().push(h.i("id").to_string());
    }

    for svc in svcs {
        let ids = match groups.get(svc) {
            Some(v) if !v.is_empty() => v.clone(),
            _ => continue,
        };
        let mut argv = ids;
        if svc.starts_with("sonarr") {
            argv.push("--all".into());
        }
        argv.extend(passthru.iter().cloned());
        cmd_delete(svc, &argv);
    }
}

// --- jellyfin unwatched / identity commands -----------------------------------

fn count_seasons_with_files(s: &Value) -> i64 {
    s.a("seasons")
        .iter()
        .filter(|se| se.i("seasonNumber") > 0 && se.at(&["statistics"]).i("episodeFileCount") > 0)
        .count() as i64
}

/// Series with >=N regular seasons on disk that NO Jellyfin user has played
/// or has progress on — the 'safe to delete' report. Sized via Sonarr.
pub fn cmd_jf_unwatched(args: &[String]) {
    let (flags, _) = pop_flags(args, &[("--min-seasons", 1)]);
    let min_seasons: i64 = flags
        .val_or("--min-seasons", "2")
        .parse()
        .unwrap_or_else(|_| die("bad --min-seasons"));
    let mut watched: HashSet<String> = HashSet::new();
    let users = jf_api("/Users", &[], 60, "GET", false).unwrap_or(Value::Null);
    for u in users.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        for filt in ["IsPlayed", "IsResumable"] {
            let r = jf_api(
                &format!("/Users/{}/Items", u.s("Id")),
                &[
                    ("IncludeItemTypes", "Episode"),
                    ("Recursive", "true"),
                    ("Filters", filt),
                    ("Fields", "SeriesName"),
                    ("Limit", "100000"),
                ],
                60,
                "GET",
                false,
            )
            .unwrap_or(Value::Null);
            for it in r.a("Items") {
                if truthy(it.at(&["SeriesName"])) {
                    watched.insert(it.s("SeriesName").to_lowercase());
                }
            }
        }
    }
    let mut cand: Vec<Value> = vec![];
    for s in as_list(api("sonarr", "GET", "/series", None)) {
        let nseas = count_seasons_with_files(&s);
        if nseas < min_seasons || s.at(&["statistics"]).i("episodeFileCount") == 0 {
            continue;
        }
        if watched.contains(&s.s("title").to_lowercase()) {
            continue;
        }
        cand.push(s);
    }
    cand.sort_by_key(|s| std::cmp::Reverse(s.at(&["statistics"]).i("sizeOnDisk")));
    let tot: i64 = cand.iter().map(|s| s.at(&["statistics"]).i("sizeOnDisk")).sum();
    println!(
        "unwatched: {} series (>={} seasons on disk, no play/progress by any Jellyfin user), {}GB",
        cand.len(),
        min_seasons,
        fmt_gb(tot)
    );
    println!("  {:<38} {:>5} {:>5} {:>9}", "series", "seas", "eps", "size");
    for s in &cand {
        let st = s.at(&["statistics"]);
        let nseas = count_seasons_with_files(s);
        println!(
            "  {:<38} {:>5} {:>5} {:>7}GB",
            trunc(s.s("title"), 38),
            nseas,
            st.i("episodeFileCount"),
            fmt_gb(st.i("sizeOnDisk"))
        );
    }
}

/// Distinct title variants for an item: title, originalTitle, altTitles.
fn item_titles(item: &Value) -> Vec<String> {
    let mut all: Vec<Option<String>> = vec![
        item.get("title").and_then(Value::as_str).map(String::from),
        item.get("originalTitle").and_then(Value::as_str).map(String::from),
    ];
    for a in item.a("alternateTitles") {
        all.push(a.get("title").and_then(Value::as_str).map(String::from));
    }
    let mut out = vec![];
    let mut seen: HashSet<String> = HashSet::new();
    for t in all.into_iter().flatten() {
        if !t.is_empty() && seen.insert(t.to_lowercase()) {
            out.push(t);
        }
    }
    out
}

/// Is this obtainable on our indexers? Searches using the item's OWN stored
/// titles + imdb/tmdb ids (no guessing romanizations), reports found/unavailable.
pub fn cmd_availability(svc: &str, args: &[String]) {
    if args.is_empty() {
        die("availability: need an id or query");
    }
    let coll = if svc.starts_with("sonarr") { "series" } else { "movie" };
    let iid = resolve_id(svc, &args[0]);
    let item = api(svc, "GET", &format!("/{}/{}", coll, iid), None).unwrap_or(Value::Null);
    let year = item.at(&["year"]).clone();
    println!(
        "{} ({})  imdb={} tmdb={}",
        pys(item.at(&["title"])),
        pys(&year),
        pys(item.at(&["imdbId"])),
        pys(item.at(&["tmdbId"]))
    );
    let mut found = 0usize;
    if svc == "radarr" {
        let rels = as_list(api_t(
            "radarr",
            "GET",
            &format!("/release?movieId={}", iid),
            None,
            SEARCH_TIMEOUT,
        ));
        let ok = rels.iter().filter(|r| !truthy(r.at(&["rejected"]))).count();
        println!("  radarr id-search: {} release(s), {} not rejected", rels.len(), ok);
        found += ok;
    }
    let mut uniq: HashSet<Option<String>> = HashSet::new();
    println!("  prowlarr title searches:");
    for t in item_titles(&item).into_iter().take(6) {
        let q = if truthy(&year) { format!("{} {}", t, pys(&year)) } else { t.clone() };
        let res = as_list(api_t(
            "prowlarr",
            "GET",
            &format!(
                "/search?{}",
                form_encode(&[("query", q.as_str()), ("type", "search"), ("limit", "30")])
            ),
            None,
            SEARCH_TIMEOUT,
        ));
        for r in &res {
            uniq.insert(r.get("title").and_then(Value::as_str).map(String::from));
        }
        println!("    {:<46} {}", trunc(&q, 46), res.len());
    }
    found += uniq.len();
    println!(
        "  => {}",
        if found > 0 { "AVAILABLE" } else { "UNAVAILABLE on current indexers" }
    );
}

/// Search TMDB/TVDB metadata for a title (disambiguate, e.g. 1990 vs 2003).
pub fn cmd_lookup(svc: &str, args: &[String]) {
    if args.is_empty() {
        die("lookup: need a search term");
    }
    let coll = if svc.starts_with("sonarr") { "series" } else { "movie" };
    let res = as_list(api(
        svc,
        "GET",
        &format!("/{}/lookup?term={}", coll, py_quote(&args.join(" "))),
        None,
    ));
    for it in res.iter().take(12) {
        let mut ids = format!(
            "tmdb={} imdb={}",
            pys(it.at(&["tmdbId"])),
            pys(it.at(&["imdbId"]))
        );
        if truthy(it.at(&["tvdbId"])) {
            ids.push_str(&format!(" tvdb={}", pys(it.at(&["tvdbId"]))));
        }
        let mut orig = String::new();
        if truthy(it.at(&["originalTitle"])) && it.s("originalTitle") != it.s("title") {
            orig = format!("  orig={}", it.s("originalTitle"));
        }
        println!(
            "  {} ({})  {}{}",
            pys(it.at(&["title"])),
            pys(it.at(&["year"])),
            ids,
            orig
        );
    }
}

/// Concise identity for an item (title/year/ids/originalTitle/altTitles/state).
pub fn cmd_info(svc: &str, args: &[String]) {
    if args.is_empty() {
        die("info: need an id or query");
    }
    let coll = if svc.starts_with("sonarr") { "series" } else { "movie" };
    let iid = resolve_id(svc, &args[0]);
    let it = api(svc, "GET", &format!("/{}/{}", coll, iid), None).unwrap_or(Value::Null);
    println!("{} ({})", pys(it.at(&["title"])), pys(it.at(&["year"])));
    let mut ids = format!(
        "id={} tmdb={} imdb={}",
        iid,
        pys(it.at(&["tmdbId"])),
        pys(it.at(&["imdbId"]))
    );
    if truthy(it.at(&["tvdbId"])) {
        ids.push_str(&format!(" tvdb={}", pys(it.at(&["tvdbId"]))));
    }
    println!("  {}", ids);
    if truthy(it.at(&["originalTitle"])) {
        println!("  originalTitle: {}", it.s("originalTitle"));
    }
    let alts: Vec<&str> = it
        .a("alternateTitles")
        .iter()
        .map(|a| a.s("title"))
        .filter(|t| !t.is_empty())
        .collect();
    if !alts.is_empty() {
        println!(
            "  altTitles: {}",
            alts.iter().take(12).cloned().collect::<Vec<_>>().join(", ")
        );
    }
    let st = it.at(&["statistics"]);
    let disk = if svc.starts_with("sonarr") {
        format!(
            "{}/{} eps, {}GB",
            st.i("episodeFileCount"),
            st.i("totalEpisodeCount"),
            fmt_gb(st.i("sizeOnDisk"))
        )
    } else if truthy(it.at(&["hasFile"])) {
        format!("ON DISK {}GB", fmt_gb(it.i("sizeOnDisk")))
    } else {
        "no file".to_string()
    };
    println!("  monitored={}  {}", pys(it.at(&["monitored"])), disk);
    println!("  path: {}", pys(it.at(&["path"])));
}
