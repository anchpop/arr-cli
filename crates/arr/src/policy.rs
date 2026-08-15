//! policy — coverage / add / replace / tracks / watch (arr.py lines 1525-2365).
//!
//! The requested-show policy as commands: per-season coverage vs aired
//! episodes (+ gap repair), strict-disambiguation add/replace, ffprobe
//! audio/sub track inspection, and the one-shot cron watchdog `watch`
//! (exit codes 0 ready / 1 pending / 2 verify-fail / 3 stuck / 4 stalled,
//! worst-wins across targets — crons parse both the text and the codes).

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use arr_api::json::items;
use arr_api::{
    api, bazarr_api, die, fmt_gb, jf_api, parse_seasons, pop_flags, resolve_id, try_api, ApiError,
    Flags, JsonExt,
};

// --- small local helpers ------------------------------------------------------

/// Python `"%s" % d.get(key)`: None -> "None", str as-is, bools True/False.
fn py_get(v: &Value, key: &str) -> String {
    match v.get(key) {
        None | Some(Value::Null) => "None".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => if *b { "True" } else { "False" }.into(),
        Some(x) => x.to_string(),
    }
}

/// Python truthiness for a JSON value (ffprobe dispositions are 0/1 ints).
pub(crate) fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn take_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn parse_int_flag(v: &str, flag: &str) -> i64 {
    v.trim()
        .parse()
        .unwrap_or_else(|_| die(&format!("bad {} '{}'", flag, v)))
}

/// urllib.parse.quote with the default safe='/' (letters, digits, `_.-~/`).
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

/// os.path.join for the two-part case the arr file paths use.
fn os_join(a: &str, b: &str) -> String {
    if b.starts_with('/') || a.is_empty() {
        b.to_string()
    } else if a.ends_with('/') {
        format!("{}{}", a, b)
    } else {
        format!("{}/{}", a, b)
    }
}

fn now_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn now_i64() -> i64 {
    now_f64() as i64
}

/// time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(secs)).
pub(crate) fn utc_iso(secs: i64) -> String {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Howard Hinnant's civil_from_days
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", y, m, d, h, mi, s)
}

/// Fallible api() twin for the flows that catch SystemExit in Python (the
/// watch retry, the other-sonarr twin probe). die() prints at raise time, so
/// this prints the exact api_t message to stderr and returns Err instead of
/// exiting — the caller decides whether to "re-raise" (exit 1) or continue.
fn api_r(
    svc: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
    timeout: u64,
) -> Result<Option<Value>, ()> {
    match try_api(svc, method, path, body, timeout) {
        Ok(v) => Ok(v),
        Err(e) => {
            let msg = match e {
                ApiError::Http { code, detail } => {
                    format!("{} {} -> HTTP {} {}", method, path, code, detail)
                }
                ApiError::Timeout => format!(
                    "{} {} timed out after {}s (indexer searches can be slow — retry)",
                    method, path, timeout
                ),
                ApiError::Net(reason) => format!("{} {} -> {}", method, path, reason),
            };
            eprintln!("arr: {}", msg);
            Err(())
        }
    }
}

// --- coverage (the requested-show policy as a command) -----------------------

#[derive(Debug, Clone)]
pub struct SeasonCov {
    pub season: i64,
    pub monitored: bool,
    pub aired: i64,
    pub files: i64,
    pub future: i64,
    /// monitored aired eps w/o file (full episode objects)
    pub missing: Vec<Value>,
    pub unmon_missing: i64,
}

/// Exact per-season coverage from the episode list (aired = airdate past).
fn series_coverage_r(
    svc: &str,
    sid: i64,
    series: Option<&Value>,
) -> Result<(Value, Vec<SeasonCov>), ()> {
    let s = match series {
        Some(v) => v.clone(),
        None => api_r(svc, "GET", &format!("/series/{}", sid), None, 120)?.unwrap_or(Value::Null),
    };
    let eps = api_r(svc, "GET", &format!("/episode?seriesId={}", sid), None, 120)?;
    let now = utc_iso(now_i64());
    let mut seasons: BTreeMap<i64, SeasonCov> = BTreeMap::new();
    for e in items(&eps) {
        let sn = e.i("seasonNumber");
        let d = seasons.entry(sn).or_insert_with(|| SeasonCov {
            season: sn,
            monitored: false,
            aired: 0,
            files: 0,
            future: 0,
            missing: vec![],
            unmon_missing: 0,
        });
        let air = e.s("airDateUtc");
        let aired = !air.is_empty() && take_chars(air, 19).as_str() <= now.as_str();
        if e.b("hasFile") {
            d.files += 1;
        }
        if aired {
            d.aired += 1;
            if !e.b("hasFile") {
                if e.b("monitored") {
                    d.missing.push(e.clone());
                } else {
                    d.unmon_missing += 1;
                }
            }
        } else if !e.b("hasFile") {
            d.future += 1;
        }
    }
    let mut mon: HashMap<i64, bool> = HashMap::new();
    for se in s.a("seasons") {
        mon.insert(se.i("seasonNumber"), se.b("monitored"));
    }
    let mut rows = vec![];
    for (sn, mut d) in seasons {
        d.monitored = mon.get(&sn).copied().unwrap_or(false);
        rows.push(d);
    }
    Ok((s, rows))
}

pub fn series_coverage(svc: &str, sid: i64, series: Option<&Value>) -> (Value, Vec<SeasonCov>) {
    series_coverage_r(svc, sid, series).unwrap_or_else(|_| std::process::exit(1))
}

/// Print per-season lines; return (fixable, askable) season rows.
pub fn coverage_print(rows: &[SeasonCov]) -> (Vec<SeasonCov>, Vec<SeasonCov>) {
    let (mut fixable, mut askable) = (vec![], vec![]);
    for d in rows {
        let sn = d.season;
        if sn == 0 {
            if d.aired != 0 || d.files != 0 {
                println!(
                    "  S0   specials: {}/{} aired on disk{}",
                    d.files,
                    d.aired,
                    if d.monitored { "" } else { " (unmonitored)" }
                );
            }
            continue;
        }
        let miss = d.missing.len();
        let state;
        if d.monitored && miss > 0 {
            state = if d.files != 0 {
                format!("⚠ PARTIAL — {} aired ep(s) missing", miss)
            } else {
                "⚠ MISSING (0 on disk)".to_string()
            };
            fixable.push(d.clone());
        } else if !d.monitored && d.files < d.aired {
            state = "unmonitored, not fetched".to_string();
            askable.push(d.clone());
        } else {
            state = "complete".to_string();
        }
        let extra = if d.future != 0 {
            format!(" (+{} unaired)", d.future)
        } else {
            String::new()
        };
        println!(
            "  S{:<2} {} {}/{} aired on disk{} — {}",
            sn,
            if d.monitored { "mon" } else { "off" },
            d.files,
            d.aired,
            extra,
            state
        );
    }
    (fixable, askable)
}

/// Trigger searches for the gaps: whole-season -> SeasonSearch, partial ->
/// one EpisodeSearch with the missing episode ids.
fn coverage_fix(svc: &str, sid: i64, fixable: &[SeasonCov], dry: bool) {
    for d in fixable {
        let (body, desc) = if d.files == 0 {
            (
                json!({"name": "SeasonSearch", "seriesId": sid, "seasonNumber": d.season}),
                format!("SeasonSearch S{}", d.season),
            )
        } else {
            let ids: Vec<i64> = d.missing.iter().map(|e| e.i("id")).collect();
            (
                json!({"name": "EpisodeSearch", "episodeIds": ids}),
                format!("EpisodeSearch S{} ({} eps)", d.season, d.missing.len()),
            )
        };
        if dry {
            println!("  DRY: {}", desc);
            continue;
        }
        let r = api(svc, "POST", "/command", Some(&body)).unwrap_or(Value::Null);
        println!("  queued {} (command id {})", desc, py_get(&r, "id"));
    }
}

fn coverage_all(svc: &str, flags: &Flags) {
    let fix = flags.has("--fix");
    let limit = parse_int_flag(flags.val_or("--limit", "10"), "--limit");
    let all = api(svc, "GET", "/series", None);
    let mut gaps: Vec<Value> = vec![];
    for s in items(&all) {
        if !s.b("monitored") {
            continue;
        }
        let st = s.at(&["statistics"]);
        if st.i("episodeFileCount") < st.i("episodeCount") {
            gaps.push(s.clone());
        }
    }
    if gaps.is_empty() {
        if !flags.has("--quiet") {
            println!(
                "coverage: every monitored {} series has its monitored aired episodes on disk",
                svc
            );
        }
        return;
    }
    println!("coverage: {} monitored series with gaps:", gaps.len());
    gaps.sort_by(|a, b| a.s("title").cmp(b.s("title")));
    let mut fixed = 0i64;
    for s in &gaps {
        let (_, rows) = series_coverage(svc, s.i("id"), Some(s));
        let fixable: Vec<SeasonCov> = rows
            .into_iter()
            .filter(|d| d.season != 0 && d.monitored && !d.missing.is_empty())
            .collect();
        if fixable.is_empty() {
            continue;
        }
        println!(
            "  [{}] {} — {}",
            s.i("id"),
            s.s("title"),
            fixable
                .iter()
                .map(|d| format!("S{} {}/{}", d.season, d.files, d.aired))
                .collect::<Vec<_>>()
                .join(", ")
        );
        if fix {
            if fixed < limit {
                coverage_fix(svc, s.i("id"), &fixable, flags.has("--dry-run"));
                fixed += 1;
            } else {
                println!("    (fix skipped — --limit {} reached this run)", limit);
            }
        }
    }
    if fix {
        println!(
            "(searches triggered for {} series{})",
            fixed,
            if fixed >= limit {
                "; raise --limit to fix more per run"
            } else {
                ""
            }
        );
    }
}

#[derive(Debug, Default)]
struct TrackCov {
    files: i64,
    unreadable: i64,
    missing_audio: Vec<String>,
    missing_subs: Vec<String>,
}

/// ffprobe every on-disk file of a series; per-season audio/sub coverage
/// for <lang> (embedded streams + sidecar subs).
fn season_track_coverage(svc: &str, sid: i64, lang: &str) -> BTreeMap<i64, TrackCov> {
    let s = api(svc, "GET", &format!("/series/{}", sid), None).unwrap_or(Value::Null);
    let ef = api(svc, "GET", &format!("/episodefile?seriesId={}", sid), None);
    let mut seasons: BTreeMap<i64, TrackCov> = BTreeMap::new();
    for f in items(&ef) {
        let path = if !f.s("path").is_empty() {
            f.s("path").to_string()
        } else {
            os_join(s.s("path"), f.s("relativePath"))
        };
        let label = if !f.s("relativePath").is_empty() {
            f.s("relativePath").to_string()
        } else {
            path.clone()
        };
        let sn = f.i("seasonNumber");
        let d = seasons.entry(sn).or_insert_with(TrackCov::default);
        let (audio, subs, ok) = file_tracks(&path);
        if !ok {
            d.unreadable += 1;
            continue;
        }
        d.files += 1;
        if !audio.iter().any(|t| t.lang == lang) {
            d.missing_audio.push(label.clone());
        }
        if !subs.iter().any(|t| t.lang == lang) {
            d.missing_subs.push(label);
        }
    }
    seasons
}

/// Kick Bazarr's search-missing for a series (main sonarr only).
fn bazarr_search_series(svc: &str, sid: i64, title: &str) {
    if svc != "sonarr" {
        println!(
            "  => '{}' lives on {} — Bazarr only watches the MAIN sonarr, so no subtitle search was triggered (grab a subbed/dual release instead)",
            title, svc
        );
        return;
    }
    let sid_s = sid.to_string();
    bazarr_api(
        "PATCH",
        "/series",
        &[("seriesid", sid_s.as_str()), ("action", "search-missing")],
        120,
        false,
    );
    println!(
        "  => Bazarr search-missing triggered for '{}' — subs download in the background as providers allow; check `arr bazarr wanted '{}'` later",
        title, title
    );
}

/// Per-season coverage vs AIRED episodes + gap repair (+ dub/sub coverage).
pub fn cmd_coverage(svc: &str, args: &[String]) {
    if !svc.starts_with("sonarr") {
        die("coverage: sonarr/sonarr-anime only");
    }
    let (flags, rest) = pop_flags(
        args,
        &[
            ("--fix", 0),
            ("--all", 0),
            ("--quiet", 0),
            ("--limit", 1),
            ("--dry-run", 0),
            ("--tracks", 0),
            ("--lang", 1),
            ("--fix-subs", 0),
        ],
    );
    if flags.has("--all") {
        return coverage_all(svc, &flags);
    }
    if rest.is_empty() {
        die("coverage: need <id|query> or --all");
    }
    let sid = resolve_id(svc, rest[0].as_str());
    let (s, rows) = series_coverage(svc, sid, None);
    println!("{} ({})", s.s("title"), py_get(&s, "year"));
    let (fixable, askable) = coverage_print(&rows);
    if fixable.is_empty() && askable.is_empty() {
        println!("  => complete: all monitored aired episodes on disk");
    }
    if !askable.is_empty() {
        println!(
            "  => unmonitored gaps: ask before grabbing; enable with `arr {} monitor {} s{}`",
            svc,
            sid,
            askable
                .iter()
                .map(|d| d.season.to_string())
                .collect::<Vec<_>>()
                .join(",s")
        );
    }
    if !fixable.is_empty() {
        if flags.has("--fix") {
            coverage_fix(svc, sid, &fixable, flags.has("--dry-run"));
        } else {
            println!("  => fixable gaps in monitored seasons: rerun with --fix to trigger searches");
        }
    }
    crate::browse::audit_warn(svc, sid, Some(&s));
    if !flags.has("--tracks") && !flags.has("--fix-subs") {
        return;
    }
    let lang = norm_lang(flags.val_or("--lang", "eng"));
    let tc = season_track_coverage(svc, sid, &lang);
    println!("tracks ({} audio/subs; embedded + sidecar):", lang);
    let (mut audio_gaps, mut sub_gaps) = (false, false);
    for (sn, d) in &tc {
        if d.files == 0 && d.unreadable == 0 {
            continue;
        }
        let ma = d.missing_audio.len() as i64;
        let ms = d.missing_subs.len() as i64;
        audio_gaps = audio_gaps || ma != 0;
        sub_gaps = sub_gaps || ms != 0;
        println!(
            "  S{:<2} audio {}/{}   subs {}/{}{}",
            sn,
            d.files - ma,
            d.files,
            d.files - ms,
            d.files,
            if d.unreadable != 0 {
                format!("   ({} unreadable)", d.unreadable)
            } else {
                String::new()
            }
        );
        for lbl in d.missing_audio.iter().take(4) {
            println!("       no {} audio: {}", lang, lbl);
        }
        if ma > 4 {
            println!("       ... +{} more without {} audio", ma - 4, lang);
        }
        for lbl in d.missing_subs.iter().take(4) {
            println!("       no {} subs:  {}", lang, lbl);
        }
        if ms > 4 {
            println!("       ... +{} more without {} subs", ms - 4, lang);
        }
    }
    if sub_gaps {
        if flags.has("--fix-subs") {
            bazarr_search_series(svc, sid, s.s("title"));
        } else if svc == "sonarr" {
            println!(
                "  => missing subs: `--fix-subs` (or `arr bazarr search --series {}`) asks Bazarr to fetch them",
                sid
            );
        } else {
            println!(
                "  => missing subs: {} is NOT Bazarr-covered (anime instance) — prefer a subbed/dual release",
                svc
            );
        }
    }
    if audio_gaps {
        println!(
            "  => missing {} audio can't be 'fetched' — needs a dub release: `arr {} releases {} --season N --audio {}`",
            lang, svc, sid, lang
        );
    }
}

// --- add / replace ------------------------------------------------------------

/// Lookup <term> and insist on ONE unambiguous winner. Multiple plausible
/// matches -> list candidates and refuse (the wrong-Gloria/wrong-Groove guard).
/// tvdb/tmdb/year use 0 as "no filter" (Python None/0 falsy).
fn lookup_pick(svc: &str, term: &str, tvdb: i64, tmdb: i64, year: i64) -> Value {
    let coll = if svc.starts_with("sonarr") { "series" } else { "movie" };
    let res = api(
        svc,
        "GET",
        &format!("/{}/lookup?term={}", coll, py_quote(term)),
        None,
    );
    let mut cands: Vec<Value> = items(&res).to_vec();
    if tvdb != 0 {
        cands.retain(|c| c.i("tvdbId") == tvdb);
    }
    if tmdb != 0 {
        cands.retain(|c| c.i("tmdbId") == tmdb);
    }
    if year != 0 {
        cands.retain(|c| c.i("year") == year);
    }
    if cands.is_empty() {
        die(&format!(
            "no lookup results for \"{}\"{}",
            term,
            if tvdb != 0 || tmdb != 0 || year != 0 {
                " with those filters"
            } else {
                ""
            }
        ));
    }
    if cands.len() == 1 {
        return cands[0].clone();
    }
    let tl = term.to_lowercase();
    let exact: Vec<&Value> = cands
        .iter()
        .filter(|c| {
            c.s("title").to_lowercase() == tl || c.s("originalTitle").to_lowercase() == tl
        })
        .collect();
    if exact.len() == 1 {
        return exact[0].clone();
    }
    eprintln!(
        "\"{}\" is ambiguous — candidates (narrow with --year / --{}):",
        term,
        if coll == "series" { "tvdb" } else { "tmdb" }
    );
    for c in cands.iter().take(8) {
        let idlab = if coll == "series" {
            format!("tvdb={}", py_get(c, "tvdbId"))
        } else {
            format!("tmdb={}", py_get(c, "tmdbId"))
        };
        let mut orig = String::new();
        if !c.s("originalTitle").is_empty() && c.s("originalTitle") != c.s("title") {
            orig = format!("  orig={}", c.s("originalTitle"));
        }
        eprintln!("  {} ({})  {}{}", py_get(c, "title"), py_get(c, "year"), idlab, orig);
    }
    die(&format!("refusing to guess between {} matches", cands.len()))
}

/// tvdb/tmdb: 0 = no filter (Python falsy None).
fn existing_by_ids_r(
    svc: &str,
    tvdb: i64,
    tmdb: i64,
    title: Option<&str>,
) -> Result<Option<Value>, ()> {
    let coll = if svc.starts_with("sonarr") { "series" } else { "movie" };
    let resp = api_r(svc, "GET", &format!("/{}", coll), None, 120)?;
    let list = items(&resp);
    for it in list {
        if tvdb != 0 && it.i("tvdbId") == tvdb {
            return Ok(Some(it.clone()));
        }
        if tmdb != 0 && it.i("tmdbId") == tmdb {
            return Ok(Some(it.clone()));
        }
    }
    if let Some(title) = title {
        let tl = title.to_lowercase();
        let exact: Vec<&Value> = list
            .iter()
            .filter(|it| it.s("title").to_lowercase() == tl)
            .collect();
        if exact.len() == 1 {
            return Ok(Some(exact[0].clone()));
        }
    }
    Ok(None)
}

fn existing_by_ids(svc: &str, tvdb: i64, tmdb: i64, title: Option<&str>) -> Option<Value> {
    existing_by_ids_r(svc, tvdb, tmdb, title).unwrap_or_else(|_| std::process::exit(1))
}

/// Default to the quality profile most of the existing library uses.
fn profile_and_root(svc: &str, quality: Option<&str>, root: Option<&str>) -> (i64, String, String) {
    let coll = if svc.starts_with("sonarr") { "series" } else { "movie" };
    let presp = api(svc, "GET", "/qualityprofile", None);
    let profiles = items(&presp);
    if profiles.is_empty() {
        die("add: no quality profiles configured");
    }
    let pid;
    if let Some(quality) = quality {
        let ql = quality.to_lowercase();
        let hits: Vec<&Value> = profiles
            .iter()
            .filter(|p| p.s("name").to_lowercase().contains(&ql))
            .collect();
        if hits.len() != 1 {
            die(&format!(
                "--quality '{}' matches {} profiles (have: {})",
                quality,
                hits.len(),
                profiles.iter().map(|p| p.s("name")).collect::<Vec<_>>().join(", ")
            ));
        }
        pid = hits[0].i("id");
    } else {
        // counts keyed in first-seen order so max() ties break like Python's
        let mut counts: HashMap<i64, i64> = HashMap::new();
        let mut order: Vec<i64> = vec![];
        let iresp = api(svc, "GET", &format!("/{}", coll), None);
        for it in items(&iresp) {
            let k = it.i("qualityProfileId");
            if !counts.contains_key(&k) {
                order.push(k);
            }
            *counts.entry(k).or_insert(0) += 1;
        }
        let valid: Vec<i64> = profiles.iter().map(|p| p.i("id")).collect();
        let mut best: Option<(i64, i64)> = None; // (pid, count)
        for k in &order {
            if !valid.contains(k) {
                continue;
            }
            let c = counts[k];
            if best.map(|(_, bc)| c > bc).unwrap_or(true) {
                best = Some((*k, c));
            }
        }
        pid = best.map(|(k, _)| k).unwrap_or_else(|| profiles[0].i("id"));
    }
    let pname = profiles
        .iter()
        .find(|p| p.i("id") == pid)
        .map(|p| p.s("name").to_string())
        .unwrap_or_default();
    let rresp = api(svc, "GET", "/rootfolder", None);
    let roots = items(&rresp);
    let rpath = match root {
        Some(r) => r.to_string(),
        None => roots.first().map(|r| r.s("path").to_string()).unwrap_or_default(),
    };
    if rpath.is_empty() {
        die("add: no root folder configured");
    }
    (pid, pname, rpath)
}

/// POST the new series/movie; returns the created item.
fn do_add(
    svc: &str,
    pick: &Value,
    seasons_spec: &str,
    quality: Option<&str>,
    root: Option<&str>,
    search: bool,
) -> Value {
    let is_series = svc.starts_with("sonarr");
    let (pid, pname, rpath) = profile_and_root(svc, quality, root);
    let mut body = serde_json::Map::new();
    body.insert("title".into(), pick.get("title").cloned().unwrap_or(Value::Null));
    body.insert("qualityProfileId".into(), json!(pid));
    body.insert("titleSlug".into(), pick.get("titleSlug").cloned().unwrap_or(Value::Null));
    body.insert(
        "images".into(),
        match pick.get("images") {
            Some(v) if truthy(v) => v.clone(),
            _ => json!([]),
        },
    );
    body.insert("year".into(), pick.get("year").cloned().unwrap_or(Value::Null));
    body.insert("rootFolderPath".into(), json!(rpath));
    body.insert("monitored".into(), json!(true));
    if is_series {
        body.insert("tvdbId".into(), pick.get("tvdbId").cloned().unwrap_or(Value::Null));
        body.insert("seasonFolder".into(), json!(true));
        let stype = if svc == "sonarr-anime" {
            "anime".to_string()
        } else if !pick.s("seriesType").is_empty() {
            pick.s("seriesType").to_string()
        } else {
            "standard".to_string()
        };
        body.insert("seriesType".into(), json!(stype));
        let mut seasons: Vec<Value> = pick.a("seasons").to_vec();
        if seasons_spec == "all" {
            for se in &mut seasons {
                let m = se.i("seasonNumber") > 0;
                if let Some(o) = se.as_object_mut() {
                    o.insert("monitored".into(), json!(m));
                }
            }
        } else if seasons_spec == "none" {
            for se in &mut seasons {
                if let Some(o) = se.as_object_mut() {
                    o.insert("monitored".into(), json!(false));
                }
            }
        } else {
            let want = parse_seasons(&seasons_spec.replace(['s', 'S'], ""));
            for se in &mut seasons {
                let m = want.contains(&se.i("seasonNumber"));
                if let Some(o) = se.as_object_mut() {
                    o.insert("monitored".into(), json!(m));
                }
            }
        }
        body.insert("seasons".into(), json!(seasons));
        body.insert("addOptions".into(), json!({"searchForMissingEpisodes": search}));
    } else {
        body.insert("tmdbId".into(), pick.get("tmdbId").cloned().unwrap_or(Value::Null));
        body.insert("minimumAvailability".into(), json!("released"));
        body.insert("addOptions".into(), json!({"searchForMovie": search}));
    }
    let created = api(
        svc,
        "POST",
        if is_series { "/series" } else { "/movie" },
        Some(&Value::Object(body)),
    )
    .unwrap_or(Value::Null);
    println!(
        "added [{}] {} ({}) — profile={} root={}",
        created.i("id"),
        created.s("title"),
        py_get(&created, "year"),
        pname,
        rpath
    );
    if is_series {
        let mon: Vec<String> = created
            .a("seasons")
            .iter()
            .filter(|se| se.b("monitored"))
            .map(|se| format!("S{}", se.i("seasonNumber")))
            .collect();
        println!(
            "  monitored seasons: {}",
            if mon.is_empty() { "none".to_string() } else { mon.join(",") }
        );
    }
    println!(
        "  search: {}",
        if search {
            "triggered (the arr picks per its quality profile)"
        } else {
            "skipped (--no-search)"
        }
    );
    created
}

/// Add a series/movie by title — strict disambiguation, sane defaults.
/// Stamp `add`'s requester/require-* tags without letting a failure abort the
/// command: the series/movie already exists at this point, and the
/// wait-and-report step after us is the half the caller actually asked for.
/// On failure, print the exact rerun command instead of dying.
fn stamp_add_tags(svc: &str, item_id: i64, flags: &Flags) {
    if let Some(req) = flags.val("--requester") {
        let req = req.trim();
        match crate::browse::try_stamp_label(svc, item_id, &format!("requester-{}", req), false) {
            Ok(_) => println!("  tagged requester:{} (download-notifier will DM them)", req),
            Err(e) => println!(
                "  ⚠ requester tag failed ({}) — rerun: arr {} tag {} --requester {}",
                e, svc, item_id, req
            ),
        }
    }
    for lab in crate::browse::require_labels(flags) {
        match crate::browse::try_stamp_label(svc, item_id, &lab, false) {
            Ok(_) => println!("  tagged {} (the ✅ ready DM will verify it via ffprobe)", lab),
            Err(e) => println!(
                "  ⚠ {} tag failed ({}) — rerun: arr {} tag {} --{} {}",
                lab,
                e,
                svc,
                item_id,
                if lab.starts_with("require-subs") { "require-subs" } else { "require-audio" },
                lab.rsplit('-').next().unwrap_or("eng")
            ),
        }
    }
}

pub fn cmd_add(svc: &str, args: &[String]) {
    if !(svc.starts_with("sonarr") || svc == "radarr") {
        die("add: sonarr/sonarr-anime/radarr only");
    }
    let (flags, rest) = pop_flags(
        args,
        &[
            ("--tvdb", 1),
            ("--tmdb", 1),
            ("--year", 1),
            ("--seasons", 1),
            ("--quality", 1),
            ("--root", 1),
            ("--no-search", 0),
            ("--requester", 1),
            ("--dry-run", 0),
            ("--require-subs", 1),
            ("--require-audio", 1),
            ("--no-wait", 0),
        ],
    );
    if rest.is_empty() {
        die("add: need a title");
    }
    // Reject a malformed --requester up front — dying after the add has
    // already mutated the library is the half-done shape we never want.
    if let Some(r) = flags.val("--requester") {
        if r.trim().is_empty() || !r.trim().chars().all(|c| c.is_ascii_digit()) {
            die(&format!("add: --requester must be a numeric Discord user id (got '{}')", r));
        }
    }
    let term = rest.join(" ");
    let is_series = svc.starts_with("sonarr");
    let tvdb = flags.val("--tvdb").map(|v| parse_int_flag(v, "--tvdb")).unwrap_or(0);
    let tmdb = flags.val("--tmdb").map(|v| parse_int_flag(v, "--tmdb")).unwrap_or(0);
    let year = flags.val("--year").map(|v| parse_int_flag(v, "--year")).unwrap_or(0);
    let pick = lookup_pick(svc, &term, tvdb, tmdb, year);
    let existing = existing_by_ids(
        svc,
        if is_series { pick.i("tvdbId") } else { 0 },
        if !is_series { pick.i("tmdbId") } else { 0 },
        Some(pick.s("title")),
    );
    if let Some(existing) = existing {
        println!(
            "already in {}: [{}] {} ({})",
            svc,
            existing.i("id"),
            existing.s("title"),
            py_get(&existing, "year")
        );
        stamp_add_tags(svc, existing.i("id"), &flags);
        if is_series {
            let (_, rows) = series_coverage(svc, existing.i("id"), None);
            let (fixable, askable) = coverage_print(&rows);
            if !fixable.is_empty() {
                println!(
                    "  => partial monitored season(s): `arr {} coverage {} --fix` to repair",
                    svc,
                    existing.i("id")
                );
            }
            if !askable.is_empty() {
                println!(
                    "  => whole season(s) unmonitored: ask the requester, then `arr {} monitor {} s...` + `coverage --fix`",
                    svc,
                    existing.i("id")
                );
            }
            if fixable.is_empty() && askable.is_empty() {
                println!("  => complete: all monitored aired episodes on disk");
            }
            crate::browse::audit_warn(svc, existing.i("id"), Some(&existing));
            println!(
                "  => dub/sub state NOT checked here — run `arr {} coverage {} --tracks` and report eng audio/sub gaps too (do this unprompted for anime)",
                svc,
                existing.i("id")
            );
        } else {
            println!(
                "  {}",
                if existing.b("hasFile") {
                    format!("ON DISK ({}GB)", fmt_gb(existing.i("sizeOnDisk")))
                } else {
                    format!("no file yet — `arr radarr grab {}` re-searches", existing.i("id"))
                }
            );
            crate::browse::audit_warn(svc, existing.i("id"), Some(&existing));
        }
        return;
    }
    // a series might live in the OTHER sonarr instance (anime vs normal)
    if is_series && pick.i("tvdbId") != 0 {
        let other = if svc == "sonarr" { "sonarr-anime" } else { "sonarr" };
        let twin = existing_by_ids_r(other, pick.i("tvdbId"), 0, None).unwrap_or(None);
        if let Some(twin) = twin {
            println!(
                "already in {}: [{}] {} — use that instance (`arr {} ...`)",
                other,
                twin.i("id"),
                twin.s("title"),
                other
            );
            return;
        }
    }
    if flags.has("--dry-run") {
        let ids = if is_series {
            format!("tvdb={}", py_get(&pick, "tvdbId"))
        } else {
            format!("tmdb={}", py_get(&pick, "tmdbId"))
        };
        println!(
            "DRY: would add {} ({}) {} to {} [seasons={} search={}]",
            py_get(&pick, "title"),
            py_get(&pick, "year"),
            ids,
            svc,
            if is_series { flags.val_or("--seasons", "all") } else { "-" },
            if !flags.has("--no-search") { "True" } else { "False" }
        );
        return;
    }
    let created = do_add(
        svc,
        &pick,
        flags.val_or("--seasons", "all"),
        flags.val("--quality"),
        flags.val("--root"),
        !flags.has("--no-search"),
    );
    stamp_add_tags(svc, created.i("id"), &flags);
    // By default, hang around briefly and report what the search actually
    // grabbed — so a single `add` answers "is it downloading?" without
    // status/history/queue follow-up calls.
    if !flags.has("--no-search") && !flags.has("--no-wait") {
        crate::acquire::report_first_grab(svc, created.i("id"), is_series, 60);
    }
}

/// Swap a wrong movie for the right one in one step.
pub fn cmd_replace(svc: &str, args: &[String]) {
    if svc != "radarr" {
        die("replace: radarr only");
    }
    let (flags, rest) = pop_flags(args, &[("--tmdb", 1), ("--year", 1), ("--yes", 0)]);
    if rest.len() < 2 {
        die("replace: usage: arr radarr replace <old id|query> <correct title...> [--tmdb ID] [--year Y] [--yes]");
    }
    let old = api(
        "radarr",
        "GET",
        &format!("/movie/{}", resolve_id("radarr", rest[0].as_str())),
        None,
    )
    .unwrap_or(Value::Null);
    let tmdb = flags.val("--tmdb").map(|v| parse_int_flag(v, "--tmdb")).unwrap_or(0);
    let year = flags.val("--year").map(|v| parse_int_flag(v, "--year")).unwrap_or(0);
    let pick = lookup_pick("radarr", &rest[1..].join(" "), 0, tmdb, year);
    if pick.i("tmdbId") == old.i("tmdbId") {
        die(&format!("replace: that's the same movie (tmdb {})", py_get(&old, "tmdbId")));
    }
    let go = flags.has("--yes");
    println!(
        "{}REPLACE [{}] {} ({}, {}GB on disk)",
        if go { "" } else { "[dry-run] " },
        old.i("id"),
        old.s("title"),
        py_get(&old, "year"),
        fmt_gb(old.i("sizeOnDisk"))
    );
    println!(
        "    with  {} ({}) tmdb={}",
        py_get(&pick, "title"),
        py_get(&pick, "year"),
        py_get(&pick, "tmdbId")
    );
    if !go {
        println!("  (pass --yes to delete the old movie + file and add the new one)");
        return;
    }
    let mut labels: Vec<String> = vec![];
    if !old.a("tags").is_empty() {
        let tresp = api("radarr", "GET", "/tag", None);
        let mut all_tags: HashMap<i64, String> = HashMap::new();
        for t in items(&tresp) {
            all_tags.insert(t.i("id"), t.s("label").to_string());
        }
        for t in old.a("tags") {
            let tid = t.as_i64().unwrap_or(0);
            if let Some(l) = all_tags.get(&tid) {
                if l.starts_with("requester-") {
                    labels.push(l.clone());
                }
            }
        }
    }
    api(
        "radarr",
        "DELETE",
        &format!("/movie/{}?deleteFiles=true", old.i("id")),
        None,
    );
    println!("deleted old movie + file");
    let created = do_add("radarr", &pick, "all", None, None, true);
    for lbl in labels {
        let tid = crate::browse::ensure_tag("radarr", &lbl);
        api(
            "radarr",
            "PUT",
            "/movie/editor",
            Some(&json!({"movieIds": [created.i("id")], "tags": [tid], "applyTags": "add"})),
        );
        println!("  carried tag {} over", lbl);
    }
}

// --- media track inspection (ffprobe) -----------------------------------------

const LANG_ALIASES: &[(&str, &str)] = &[
    ("en", "eng"),
    ("ja", "jpn"),
    ("jp", "jpn"),
    ("fr", "fre"),
    ("fra", "fre"),
    ("de", "ger"),
    ("deu", "ger"),
    ("ko", "kor"),
    ("zh", "chi"),
    ("zho", "chi"),
    ("cmn", "chi"),
    ("yue", "chi"),
    ("chn", "chi"),
    ("es", "spa"),
    ("it", "ita"),
    ("pt", "por"),
    ("ru", "rus"),
];

pub fn norm_lang(l: &str) -> String {
    let l = if l.is_empty() { "und".to_string() } else { l.to_lowercase() };
    for (a, b) in LANG_ALIASES {
        if *a == l {
            return (*b).to_string();
        }
    }
    l
}

#[derive(Debug, Clone)]
pub struct Track {
    pub lang: String,
    /// None when ffprobe reported no codec_name (Python carries None/null).
    pub codec: Option<String>,
    pub default: bool,
    pub forced: bool,
}

fn which_ffprobe() -> Option<String> {
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(':') {
        if dir.is_empty() {
            continue;
        }
        let cand = format!("{}/ffprobe", dir);
        if let Ok(md) = std::fs::metadata(&cand) {
            use std::os::unix::fs::PermissionsExt;
            if md.is_file() && md.permissions().mode() & 0o111 != 0 {
                return Some(cand);
            }
        }
    }
    None
}

/// ffprobe -show_streams as JSON; None = unreadable (error/timeout/bad JSON).
pub(crate) fn ffprobe_streams(path: &str) -> Option<Vec<Value>> {
    let exe = which_ffprobe().unwrap_or_else(|| {
        die("ffprobe not on PATH — add ffmpeg to environment.systemPackages and rebuild")
    });
    let mut child = match Command::new(&exe)
        .args(["-v", "error", "-print_format", "json", "-show_streams", path])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return None,
    };
    let mut stdout = child.stdout.take()?;
    let out_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let mut stderr = child.stderr.take()?;
    let err_thread = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = stderr.read_to_end(&mut v);
    });
    // subprocess.run(..., timeout=60) equivalent
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = out_thread.join();
                    let _ = err_thread.join();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_thread.join();
                let _ = err_thread.join();
                return None;
            }
        }
    };
    let out = out_thread.join().unwrap_or_default();
    let _ = err_thread.join();
    if !status.success() {
        return None;
    }
    match serde_json::from_str::<Value>(&out) {
        Ok(v) => Some(v.a("streams").to_vec()),
        Err(_) => None,
    }
}

/// External .srt/.ass/... next to the video count as subtitles too.
fn sidecar_subs(path: &str) -> Vec<Track> {
    let basename = path.rsplit('/').next().unwrap_or(path);
    let base = match basename.rfind('.') {
        Some(i) if i > 0 => &basename[..i],
        _ => basename,
    };
    let dir = match path.rfind('/') {
        Some(0) => "/",
        Some(i) => &path[..i],
        None => "",
    };
    let mut out = vec![];
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let f = entry.file_name().to_string_lossy().to_string();
        if !f.starts_with(base) || f == basename {
            continue;
        }
        let fl = f.to_lowercase();
        if !(fl.ends_with(".srt")
            || fl.ends_with(".ass")
            || fl.ends_with(".ssa")
            || fl.ends_with(".sub")
            || fl.ends_with(".vtt")
            // bitmap sidecars subtitle-harvest writes (Jellyfin serves all
            // three externally): PGS .sup, VobSub .idx(+.sub), Matroska .mks
            || fl.ends_with(".sup")
            || fl.ends_with(".idx")
            || fl.ends_with(".mks"))
        {
            continue;
        }
        let stripped = f[base.len()..].trim_matches('.').to_lowercase();
        let parts: Vec<&str> = stripped.split('.').collect();
        let langs: Vec<&str> = parts[..parts.len() - 1]
            .iter()
            .copied()
            .filter(|t| {
                t.chars().all(|c| c.is_alphabetic())
                    && (2..=3).contains(&t.chars().count())
                    && !["sdh", "cc", "hi"].contains(t)
            })
            .collect();
        out.push(Track {
            // last lang-looking token wins: the language sits closest to the
            // extension by convention ("Movie.yap.jpn.srt" is jpn, not "yap")
            lang: norm_lang(langs.last().copied().unwrap_or("")),
            codec: Some(format!("sidecar-{}", parts.last().copied().unwrap_or(""))),
            default: false,
            forced: parts.contains(&"forced"),
        });
    }
    out
}

/// (audio_tracks, sub_tracks, readable) for one video file.
pub fn file_tracks(path: &str) -> (Vec<Track>, Vec<Track>, bool) {
    let streams = ffprobe_streams(path);
    let (mut audio, mut subs) = (vec![], vec![]);
    if let Some(list) = &streams {
        for st in list {
            let ent = Track {
                lang: norm_lang(st.at(&["tags", "language"]).as_str().unwrap_or("")),
                codec: st.get("codec_name").and_then(|v| v.as_str()).map(String::from),
                default: truthy(st.at(&["disposition", "default"])),
                forced: truthy(st.at(&["disposition", "forced"])),
            };
            match st.s("codec_type") {
                "audio" => audio.push(ent),
                "subtitle" => subs.push(ent),
                _ => {}
            }
        }
    }
    subs.extend(sidecar_subs(path));
    (audio, subs, streams.is_some())
}

/// [(label, abspath)] for an item's on-disk files.
fn item_files_r(svc: &str, iid: i64, season: Option<i64>) -> Result<Vec<(String, String)>, ()> {
    let mut out = vec![];
    if svc.starts_with("sonarr") {
        let s = api_r(svc, "GET", &format!("/series/{}", iid), None, 120)?.unwrap_or(Value::Null);
        let ef = api_r(svc, "GET", &format!("/episodefile?seriesId={}", iid), None, 120)?;
        for f in items(&ef) {
            if let Some(se) = season {
                if f.i("seasonNumber") != se {
                    continue;
                }
            }
            let path = if !f.s("path").is_empty() {
                f.s("path").to_string()
            } else {
                os_join(s.s("path"), f.s("relativePath"))
            };
            let label = if !f.s("relativePath").is_empty() {
                f.s("relativePath").to_string()
            } else {
                path.clone()
            };
            out.push((label, path));
        }
    } else {
        let mf = api_r("radarr", "GET", &format!("/moviefile?movieId={}", iid), None, 120)?;
        for f in items(&mf) {
            let path = f.s("path").to_string();
            let label = if !f.s("relativePath").is_empty() {
                f.s("relativePath").to_string()
            } else {
                path.clone()
            };
            out.push((label, path));
        }
    }
    out.sort();
    Ok(out)
}

pub fn item_files(svc: &str, iid: i64, season: Option<i64>) -> Vec<(String, String)> {
    item_files_r(svc, iid, season).unwrap_or_else(|_| std::process::exit(1))
}

fn fmt_tracks(tr: &[Track]) -> String {
    let s = tr
        .iter()
        .map(|t| {
            format!(
                "{}{}({})",
                t.lang,
                if t.default { "*" } else { "" },
                t.codec.as_deref().unwrap_or("None")
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    if s.is_empty() {
        "-".into()
    } else {
        s
    }
}

// --- json.dumps(rows, indent=2) replica (insertion order + ensure_ascii) ------

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

fn dump_track(t: &Track, ind: usize) -> String {
    let p = " ".repeat(ind);
    format!(
        "{p}{{\n{p}  \"lang\": {},\n{p}  \"codec\": {},\n{p}  \"default\": {},\n{p}  \"forced\": {}\n{p}}}",
        py_json_str(&t.lang),
        t.codec.as_deref().map(py_json_str).unwrap_or_else(|| "null".into()),
        t.default,
        t.forced,
        p = p
    )
}

fn dump_tracks(list: &[Track], ind: usize) -> String {
    if list.is_empty() {
        return "[]".into();
    }
    let inner: Vec<String> = list.iter().map(|t| dump_track(t, ind + 2)).collect();
    format!("[\n{}\n{}]", inner.join(",\n"), " ".repeat(ind))
}

fn dump_rows(rows: &[(String, Vec<Track>, Vec<Track>)]) -> String {
    if rows.is_empty() {
        return "[]".into();
    }
    let parts: Vec<String> = rows
        .iter()
        .map(|(f, a, s)| {
            format!(
                "  {{\n    \"file\": {},\n    \"audio\": {},\n    \"subs\": {}\n  }}",
                py_json_str(f),
                dump_tracks(a, 4),
                dump_tracks(s, 4)
            )
        })
        .collect();
    format!("[\n{}\n]", parts.join(",\n"))
}

/// Audio + subtitle tracks per file — embedded (ffprobe) and sidecar subs.
pub fn cmd_tracks(svc: &str, args: &[String]) {
    if !(svc.starts_with("sonarr") || svc == "radarr") {
        die("tracks: sonarr/sonarr-anime/radarr only");
    }
    let (flags, rest) = pop_flags(
        args,
        &[
            ("--season", 1),
            ("--missing-audio", 1),
            ("--missing-subs", 1),
            ("--json", 0),
        ],
    );
    if rest.is_empty() {
        die("tracks: need an id or query");
    }
    let season = flags.val("--season").map(|v| parse_int_flag(v, "--season"));
    let iid = resolve_id(svc, rest[0].as_str());
    let files = item_files(svc, iid, season);
    if files.is_empty() {
        die("tracks: no files on disk for that item");
    }
    let want_a = flags.val("--missing-audio").map(norm_lang);
    let want_s = flags.val("--missing-subs").map(norm_lang);
    let mut rows: Vec<(String, Vec<Track>, Vec<Track>)> = vec![];
    let (mut flagged, mut unreadable) = (0i64, 0i64);
    for (label, path) in &files {
        let (audio, subs, ok) = file_tracks(path);
        if !ok {
            unreadable += 1;
            println!("  ?? unreadable: {}", label);
            continue;
        }
        rows.push((label.clone(), audio.clone(), subs.clone()));
        let miss_a = want_a
            .as_deref()
            .map(|w| !audio.iter().any(|t| t.lang == w))
            .unwrap_or(false);
        let miss_s = want_s
            .as_deref()
            .map(|w| !subs.iter().any(|t| t.lang == w))
            .unwrap_or(false);
        if (want_a.is_some() || want_s.is_some()) && !(miss_a || miss_s) {
            continue;
        }
        flagged += 1;
        if !flags.has("--json") {
            println!("  {}", label);
            println!("      audio: {}   subs: {}", fmt_tracks(&audio), fmt_tracks(&subs));
        }
    }
    if flags.has("--json") {
        println!("{}", dump_rows(&rows));
        return;
    }
    if want_a.is_some() || want_s.is_some() {
        let what = [
            want_a.as_ref().map(|w| format!("audio:{}", w)),
            want_s.as_ref().map(|w| format!("subs:{}", w)),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" / ");
        println!(
            "{}/{} file(s) missing {}{}",
            flagged,
            rows.len(),
            what,
            if unreadable != 0 {
                format!("; {} unreadable", unreadable)
            } else {
                String::new()
            }
        );
        if flagged != 0 && want_s.is_some() {
            if svc == "sonarr" {
                println!("  -> fetch subs via Bazarr: arr bazarr search --series {}", iid);
            } else if svc == "radarr" {
                println!("  -> fetch subs via Bazarr: arr bazarr search --movie {}", iid);
            } else {
                println!(
                    "  -> {} is not Bazarr-covered (anime instance); prefer a subbed/dual release",
                    svc
                );
            }
        }
    } else {
        println!(
            "({} file(s){})",
            rows.len(),
            if unreadable != 0 {
                format!("; {} unreadable", unreadable)
            } else {
                String::new()
            }
        );
    }
}

pub fn verify_lang(
    files: &[(String, String)],
    want_a: Option<&str>,
    want_s: Option<&str>,
) -> (i64, Vec<String>, Vec<String>) {
    let (mut n, mut miss_a, mut miss_s) = (0i64, vec![], vec![]);
    for (label, path) in files {
        let (audio, subs, ok) = file_tracks(path);
        if !ok {
            continue;
        }
        n += 1;
        if let Some(w) = want_a {
            if !audio.iter().any(|t| t.lang == w) {
                miss_a.push(label.clone());
            }
        }
        if let Some(w) = want_s {
            if !subs.iter().any(|t| t.lang == w) {
                miss_s.push(label.clone());
            }
        }
    }
    (n, miss_a, miss_s)
}

// --- watch (one-shot cron watchdog) --------------------------------------------

/// Find the arr item in Jellyfin by provider id, client-side (10.11's
/// AnyProviderIdEquals filter is broken — same approach as download-notifier).
fn jf_find_item(item: &Value, is_series: bool) -> Option<Value> {
    let r = jf_api(
        "/Items",
        &[
            ("IncludeItemTypes", if is_series { "Series" } else { "Movie" }),
            ("Recursive", "true"),
            ("Fields", "ProviderIds"),
            ("Limit", "100000"),
        ],
        60,
        "GET",
        true,
    )
    .unwrap_or(Value::Null);
    let mut want: Vec<(&str, String)> = vec![];
    if item.i("tvdbId") != 0 {
        want.push(("Tvdb", item.i("tvdbId").to_string()));
    }
    if item.i("tmdbId") != 0 {
        want.push(("Tmdb", item.i("tmdbId").to_string()));
    }
    if !item.s("imdbId").is_empty() {
        want.push(("Imdb", item.s("imdbId").to_string()));
    }
    for it in r.a("Items") {
        let pids = it.at(&["ProviderIds"]);
        for (k, v) in &want {
            let pv = match pids.get(*k) {
                None | Some(Value::Null) => String::new(),
                Some(Value::String(s)) => {
                    if s.is_empty() {
                        String::new()
                    } else {
                        s.clone()
                    }
                }
                Some(x) => {
                    if truthy(x) {
                        x.to_string()
                    } else {
                        String::new()
                    }
                }
            };
            if &pv == v {
                return Some(it.clone());
            }
        }
    }
    None
}

fn latest_history_date_r(svc: &str, iid: i64) -> Result<Option<String>, ()> {
    let mut dates: Vec<String> = vec![];
    if svc.starts_with("sonarr") {
        let rows = api_r(svc, "GET", &format!("/history/series?seriesId={}", iid), None, 120)?;
        for r in items(&rows) {
            dates.push(r.s("date").to_string());
        }
    } else {
        let data = api_r(
            svc,
            "GET",
            "/history?pageSize=100&sortKey=date&sortDirection=descending",
            None,
            120,
        )?
        .unwrap_or(Value::Null);
        for r in data.a("records") {
            if r.i("movieId") == iid {
                dates.push(r.s("date").to_string());
            }
        }
    }
    dates.sort();
    dates.reverse();
    Ok(match dates.first() {
        Some(d) if !d.is_empty() => Some(d.clone()),
        _ => None,
    })
}

fn watch_state_path() -> String {
    std::env::var("ARR_WATCH_STATE").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        format!("{}/.local/state/arr-watch.json", home)
    })
}

fn watch_state_load() -> serde_json::Map<String, Value> {
    std::fs::read_to_string(watch_state_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn watch_state_save(st: &serde_json::Map<String, Value>) {
    let path = watch_state_path();
    if let Some(dir) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string(&Value::Object(st.clone())) {
        let _ = std::fs::write(&path, text);
    }
}

// local copies of the queue helpers (arr.py 702-721) — watch needs FALLIBLE
// queue access for the --once retry, so these live here; dedupe at integration
// if acquire.rs ends up exporting equivalents.
fn queue_records_r(svc: &str, page_size: i64) -> Result<Value, ()> {
    let extra = if svc.starts_with("sonarr") {
        "includeUnknownSeriesItems=true&includeSeries=true&includeEpisode=true"
    } else {
        "includeUnknownMovieItems=true&includeMovie=true"
    };
    Ok(
        api_r(svc, "GET", &format!("/queue?pageSize={}&{}", page_size, extra), None, 120)?
            .unwrap_or(Value::Null),
    )
}

fn queue_status_messages(r: &Value) -> Vec<String> {
    let mut out = vec![];
    for sm in r.a("statusMessages") {
        for m in sm.a("messages") {
            out.push(match m {
                Value::String(s) => s.clone(),
                x => x.to_string(),
            });
        }
    }
    out
}

fn is_stuck_queue_record(r: &Value) -> bool {
    let state = r.s("trackedDownloadState");
    r.s("status") == "failed"
        || state == "importBlocked"
        || state == "importPending"
        || !r.s("errorMessage").is_empty()
}

fn rec_series_id(r: &Value) -> i64 {
    let v = r.i("seriesId");
    if v != 0 {
        v
    } else {
        r.at(&["series", "id"]).as_i64().unwrap_or(0)
    }
}

fn rec_movie_id(r: &Value) -> i64 {
    let v = r.at(&["movie", "id"]).as_i64().unwrap_or(0);
    if v != 0 {
        v
    } else {
        r.i("movieId")
    }
}

/// One-shot readiness check — exit 0 ready / 1 pending / 2 verify-fail /
/// 3 stuck / 4 stalled, worst-wins across targets. --once keeps cron memory
/// in ~/.local/state/arr-watch.json.
pub fn cmd_watch(svc: &str, args: &[String]) {
    if !(svc.starts_with("sonarr") || svc == "radarr") {
        die("watch: sonarr/sonarr-anime/radarr only");
    }
    let (flags, rest) = pop_flags(
        args,
        &[
            ("--season", 1),
            ("--until", 1),
            ("--verify-audio", 1),
            ("--verify-subs", 1),
            ("--max-age", 1),
            ("--quiet", 0),
            ("--once", 0),
        ],
    );
    if rest.is_empty() {
        die("watch: need an id or query");
    }
    let quiet = flags.has("--quiet");
    let until = flags.val_or("--until", "on-disk").to_string();
    if until != "on-disk" && until != "in-jellyfin" {
        die("watch: --until on-disk|in-jellyfin");
    }
    // resolve before the --once retry logic: a bad title should die loudly,
    // only downstream API hiccups are transient
    let ids: Vec<i64> = rest.iter().map(|r| resolve_id(svc, r.as_str())).collect();
    let mut worst = 0;
    for iid in ids {
        let code = watch_one(svc, iid, &flags, &until, quiet);
        worst = worst.max(code);
    }
    std::process::exit(worst);
}

fn watch_one(svc: &str, iid: i64, flags: &Flags, until: &str, quiet: bool) -> i32 {
    let once = flags.has("--once");
    let key = format!(
        "{}:{}:{}:{}:{}",
        svc,
        iid,
        until,
        flags.val_or("--verify-audio", ""),
        flags.val_or("--verify-subs", "")
    );
    let mut st = if once { watch_state_load() } else { serde_json::Map::new() };
    if once && st.get(&key).map(|e| e.s("phase")) == Some("ready") {
        return 0; // already announced — stay silent forever
    }
    let mut result = None;
    for attempt in 1..=2 {
        match watch_check(svc, iid, flags, until, quiet) {
            Ok(r) => {
                result = Some(r);
                break;
            }
            Err(()) => {
                // api died — transient service blip (message already on stderr)
                if attempt == 1 {
                    std::thread::sleep(Duration::from_secs(3));
                    continue;
                }
                if once {
                    return 1; // don't false-alert from a cron; try again next firing
                }
                std::process::exit(1);
            }
        }
    }
    let (code, out) = result.unwrap();
    if once {
        let prev = st
            .get(&key)
            .map(|e| e.s("phase").to_string())
            .filter(|p| !p.is_empty());
        if code == 0 {
            st.insert(key.clone(), json!({"phase": "ready", "ts": now_i64()}));
            watch_state_save(&st);
        } else if code == 2 || code == 3 || code == 4 {
            if prev.as_deref() == Some(format!("alert{}", code).as_str()) {
                return code; // already alerted for this failure mode — silent
            }
            st.insert(key.clone(), json!({"phase": format!("alert{}", code), "ts": now_i64()}));
            watch_state_save(&st);
        } else if prev.is_some() {
            // recovered to pending — re-arm the alert
            st.remove(&key);
            watch_state_save(&st);
        }
    }
    if !out.is_empty() {
        println!("{}", out);
    }
    code
}

/// Compute one item's watch status; returns (exit_code, text).
fn watch_check(
    svc: &str,
    iid: i64,
    flags: &Flags,
    until: &str,
    quiet: bool,
) -> Result<(i32, String), ()> {
    let season = flags.val("--season").map(|v| parse_int_flag(v, "--season"));
    let is_series = svc.starts_with("sonarr");
    let item = api_r(
        svc,
        "GET",
        &format!("/{}/{}", if is_series { "series" } else { "movie" }, iid),
        None,
        120,
    )?
    .unwrap_or(Value::Null);
    let qv = queue_records_r(svc, 1000)?;
    let mine: Vec<&Value> = qv
        .a("records")
        .iter()
        .filter(|r| {
            if is_series {
                rec_series_id(r) == iid
            } else {
                rec_movie_id(r) == iid
            }
        })
        .collect();
    let stuck: Vec<&&Value> = mine.iter().filter(|r| is_stuck_queue_record(r)).collect();
    if !stuck.is_empty() {
        let mut lines = vec![format!(
            "STUCK: {} — {} queue item(s) need intervention:",
            item.s("title"),
            stuck.len()
        )];
        for r in &stuck {
            lines.push(format!(
                "  {}/{}  {}",
                py_get(r, "status"),
                py_get(r, "trackedDownloadState"),
                take_chars(r.s("title"), 70)
            ));
            let msgs = queue_status_messages(r);
            if !msgs.is_empty() {
                lines.push(format!("    {}", take_chars(&msgs.join("; "), 220)));
            }
        }
        lines.push(format!("  -> arr {} stuck '{}' --fix", svc, item.s("title")));
        return Ok((3, lines.join("\n")));
    }
    let done;
    let prog_desc;
    if is_series {
        let (_, mut rows) = series_coverage_r(svc, iid, Some(&item))?;
        if let Some(se) = season {
            rows.retain(|d| d.season == se);
        }
        let missing: usize = rows
            .iter()
            .filter(|d| d.season != 0 || season == Some(0))
            .map(|d| d.missing.len())
            .sum();
        let have: i64 = rows.iter().map(|d| d.files).sum();
        done = missing == 0 && have > 0 && mine.is_empty();
        prog_desc = format!("{} aired monitored ep(s) still missing", missing);
    } else {
        done = item.b("hasFile") && mine.is_empty();
        prog_desc = "file not on disk yet".to_string();
    }
    if !done {
        if !mine.is_empty() {
            let size: f64 = mine.iter().map(|r| r.f("size")).sum();
            let left: f64 = mine.iter().map(|r| r.f("sizeleft")).sum();
            let pct = if size != 0.0 { (100.0 * (1.0 - left / size)) as i64 } else { 0 };
            return Ok((
                1,
                if quiet {
                    String::new()
                } else {
                    format!(
                        "downloading: {} — {} item(s), {}% ({}MB left)",
                        item.s("title"),
                        mine.len(),
                        pct,
                        (left / 1048576.0).round() as i64
                    )
                },
            ));
        }
        if let Some(max_age) = flags.val("--max-age") {
            let last = latest_history_date_r(svc, iid)?;
            let hours: f64 = max_age
                .trim()
                .parse()
                .unwrap_or_else(|_| die(&format!("bad --max-age '{}'", max_age)));
            let cutoff = utc_iso((now_f64() - hours * 3600.0) as i64);
            let last19 = last.as_deref().map(|l| take_chars(l, 19));
            if last.is_none() || last19.as_deref().unwrap() < cutoff.as_str() {
                return Ok((
                    4,
                    format!(
                        "STALLED: {} — {}; queue empty, no activity in {}h (last: {})\n  -> re-grab: arr {} grab {}{}",
                        item.s("title"),
                        prog_desc,
                        max_age,
                        take_chars(last.as_deref().unwrap_or("never"), 19),
                        svc,
                        iid,
                        match season {
                            Some(se) => format!(" --season {}", se),
                            None => String::new(),
                        }
                    ),
                ));
            }
        }
        return Ok((
            1,
            if quiet {
                String::new()
            } else {
                format!("pending: {} — {}, nothing in queue", item.s("title"), prog_desc)
            },
        ));
    }
    if until == "in-jellyfin" {
        let jf = jf_find_item(&item, is_series);
        if jf.is_none() {
            jf_api("/Library/Refresh", &[], 60, "POST", true); // nudge the scan
            return Ok((
                1,
                if quiet {
                    String::new()
                } else {
                    format!(
                        "imported: {} — waiting for Jellyfin to index it (refresh nudged)",
                        item.s("title")
                    )
                },
            ));
        }
    }
    let mut lines = vec![format!("READY: {} ({})", item.s("title"), py_get(&item, "year"))];
    let mut code = 0;
    if flags.val("--verify-audio").is_some() || flags.val("--verify-subs").is_some() {
        let wa = flags.val("--verify-audio").map(norm_lang);
        let ws = flags.val("--verify-subs").map(norm_lang);
        let files = item_files_r(svc, iid, season)?;
        let (n, ma, ms) = verify_lang(&files, wa.as_deref(), ws.as_deref());
        if let Some(wa) = &wa {
            lines.push(format!(
                "  audio {}: {}/{} file(s) ok{}",
                wa,
                n - ma.len() as i64,
                n,
                if !ma.is_empty() {
                    format!("; MISSING: {}", ma.iter().take(5).cloned().collect::<Vec<_>>().join(", "))
                } else {
                    String::new()
                }
            ));
        }
        if let Some(ws) = &ws {
            lines.push(format!(
                "  subs {}: {}/{} file(s) ok{}",
                ws,
                n - ms.len() as i64,
                n,
                if !ms.is_empty() {
                    format!("; MISSING: {}", ms.iter().take(5).cloned().collect::<Vec<_>>().join(", "))
                } else {
                    String::new()
                }
            ));
        }
        if !ma.is_empty() || !ms.is_empty() {
            code = 2;
            lines[0] = format!(
                "VERIFY-FAIL: {} ({}) — on disk but the language checks failed:",
                item.s("title"),
                py_get(&item, "year")
            );
        }
    }
    Ok((code, lines.join("\n")))
}
