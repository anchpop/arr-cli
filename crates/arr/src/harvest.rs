//! harvest.rs — subtitle harvest: mine subtitle tracks out of candidate
//! releases WITHOUT replacing the library file.
//!
//! The insight (2026-08-14, original-language subtitle hunt): a release only
//! needs to be downloaded to *extract its subtitle streams* — the video is
//! discarded, so the existing (usually better) file is never at risk, quality
//! profiles don't matter, and the whole downgrade-vs-language tension in
//! `stuck --fix` never arises. Harvest downloads live in their own SAB
//! category (`subs-harvest`), invisible to Radarr: no queue records, no
//! import, no blocklist bookkeeping — a failed candidate is deleted and noted
//! in the tried-list, and the next one is grabbed.
//!
//! Radarr-only for now (the hunt is movie-scoped; extend to sonarr when a
//! show actually needs it).
//!
//!   arr radarr subtitle-harvest <id|query> --subs LANG [--grab] [--limit N] [--dry-run]
//!   arr radarr subtitle-harvest --collect [--dry-run]     process completed downloads
//!   arr radarr subtitle-harvest --adopt [--yes]           take over Radarr-queued
//!                                                language-replacement downloads
//!   arr radarr subtitle-harvest --status                  jobs in flight + tried-list

use std::collections::HashMap;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use arr_api::json::items;
use arr_api::{api, api_t, die, mb, pop_flags, resolve_id, sab_api, JsonExt};

use crate::policy::{file_tracks, norm_lang};

const SAB_CATEGORY: &str = "subs-harvest";
const STATE_PATH: &str = "/data/downloads/.subs-harvest.json";
/// Payload files smaller than this are samples/extras, not the feature.
const MIN_VIDEO_BYTES: u64 = 50 * 1024 * 1024;

pub fn cmd_harvest(svc: &str, args: &[String]) {
    if svc != "radarr" {
        die("harvest: radarr only (sidecar subtitle harvest is movie-scoped for now)");
    }
    let (flags, rest) = pop_flags(
        args,
        &[
            ("--subs", 1),
            ("--grab", 0),
            ("--match", 1),
            ("--fallback", 1),
            ("--limit", 1),
            ("--dry-run", 0),
            ("--collect", 0),
            ("--adopt", 0),
            ("--status", 0),
            ("--yes", 0),
        ],
    );
    if flags.has("--collect") {
        return collect(flags.has("--dry-run"));
    }
    if flags.has("--adopt") {
        return adopt(flags.has("--yes"));
    }
    if flags.has("--status") {
        return status();
    }
    if rest.is_empty() {
        die("harvest: need <id|query> --subs LANG   (or --collect / --adopt / --status)");
    }
    let lang = norm_lang(flags.val("--subs").unwrap_or_else(|| {
        die("harvest: --subs LANG is required (which subtitle language to mine)")
    }));
    let limit: usize = flags.val_or("--limit", "8").parse().unwrap_or(8);
    // `--fallback 'A || B'`: pre-picked runner-up releases, tried IN ORDER by
    // --collect whenever a payload probe finds no target subs. The picking is
    // still human/agent judgment — this just queues the judgment up front.
    let fallbacks: Vec<String> = flags
        .val("--fallback")
        .map(|v| v.split("||").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let grab = if flags.has("--grab") {
        match flags.val("--match").filter(|m| !m.is_empty()) {
            Some(m) => Some(m),
            None => die("subtitle-harvest: --grab needs --match '<title substring>' — the caller picks the release, the CLI only ranks (list first without --grab)"),
        }
    } else {
        None
    };
    if grab.is_none() && !fallbacks.is_empty() {
        // register a fallback chain for an already-in-flight harvest
        let mid = resolve_id("radarr", &rest[0]);
        fallbacks_set(mid, &lang, &fallbacks);
        println!(
            "registered {} fallback pick(s) for m{} ({}): tried in order when a payload probe fails",
            fallbacks.len(),
            mid,
            lang
        );
        return;
    }
    hunt_one(&rest[0], &lang, grab, &fallbacks, flags.has("--dry-run"), limit);
}

// --- per-movie: check, score candidates, grab --------------------------------

fn hunt_one(query: &str, lang: &str, grab: Option<&str>, fallbacks: &[String], dry: bool, limit: usize) {
    let mid = resolve_id("radarr", query);
    let movie = api("radarr", "GET", &format!("/movie/{}", mid), None).unwrap_or(Value::Null);
    let title = movie.s("title").to_string();
    println!("{} (#{}) — want {} subs", title, mid, lang);

    let path = movie_file_path(mid, &movie);
    if let Some(p) = &path {
        let (_, subs, readable) = file_tracks(p);
        if readable && subs.iter().any(|t| t.lang == *lang) {
            println!("already satisfied: {} subs present on {}", lang, p);
            return;
        }
        if !readable {
            println!("  ⚠ existing file unreadable by ffprobe: {}", p);
        }
    } else {
        println!("  note: movie has no file on disk — a normal `arr radarr grab` (which imports) may serve better than a harvest");
    }

    // In-flight harvest for this movie already? Don't double-grab.
    let sabq = sab_api("queue", &[("start", "0"), ("limit", "1000")], 120);
    for s in sabq.at(&["queue", "slots"]).as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        if s.s("cat") == SAB_CATEGORY && parse_job_name(s.s("filename")).0 == mid {
            println!(
                "harvest already downloading: {} ({}%) — run `arr radarr subtitle-harvest --collect` when done",
                s.s("filename"),
                s.s("percentage")
            );
            return;
        }
    }

    let tried = tried_titles(mid, lang);
    println!("searching indexers (movieId={}) ...", mid);
    let rels = api_t(
        "radarr",
        "GET",
        &format!("/release?movieId={}", mid),
        None,
        crate::browse::SEARCH_TIMEOUT,
    );
    let mut cands: Vec<(i64, Vec<&'static str>, Value)> = Vec::new();
    let mut seen: std::collections::HashSet<String> = Default::default();
    for r in items(&rels) {
        if r.s("protocol") != "usenet" || r.s("downloadUrl").is_empty() {
            continue; // torrents: possible via `arr prowlarr grab --cat subs-harvest`, not automated here
        }
        let t = r.s("title").to_string();
        if seen.contains(&t) || tried.contains(&t) {
            continue;
        }
        seen.insert(t.clone());
        let (score, why) = score_release(&t, lang);
        cands.push((score, why, r.clone()));
    }
    // best evidence first; among equals the smallest download (we only want the text)
    cands.sort_by(|a, b| (-a.0, a.2.i("size")).cmp(&(-b.0, b.2.i("size"))));

    if cands.is_empty() {
        println!("no untried usenet candidates ({} already tried)", tried.len());
        return;
    }
    println!("candidates (score / size / title):");
    for (score, why, r) in cands.iter().take(limit) {
        println!(
            "  [{:+}] {}MB  {}  ({})",
            score,
            mb(r.i("size")),
            r.s("title"),
            if why.is_empty() { "no language evidence".into() } else { why.join(", ") }
        );
    }
    // The score is name-pattern evidence, not knowledge — the caller (a
    // person or the agent driving this) picks the release; the CLI only
    // ranks and annotates. Mirrors `grab --match`.
    let matchf = match grab {
        None => {
            println!(
                "(pick one with --grab --match '<title substring>' — the ranking is name-evidence only)"
            );
            return;
        }
        Some(m) => m.to_lowercase(),
    };
    let picked: Vec<&(i64, Vec<&'static str>, Value)> = cands
        .iter()
        .filter(|(_, _, r)| r.s("title").to_lowercase().contains(&matchf))
        .collect();
    let (_, _, best) = match picked.as_slice() {
        [] => {
            println!("no candidate matches '{}' — check the exact titles above", matchf);
            return;
        }
        [one] => *one,
        many => {
            println!("--match '{}' hits {} candidates — narrow it:", matchf, many.len());
            for (_, _, r) in many.iter().take(10) {
                println!("  {}", r.s("title"));
            }
            return;
        }
    };
    if dry {
        println!("DRY grab: {}", best.s("title"));
        return;
    }
    ensure_sab_category();
    let job = job_name(mid, lang, best.s("title"));
    if crate::acquire::sab_add_url(best.s("downloadUrl"), SAB_CATEGORY, &job) {
        record_grab(mid, lang, best.s("title"));
        println!("grabbed -> SAB cat={} as {}", SAB_CATEGORY, job);
        if !fallbacks.is_empty() {
            fallbacks_set(mid, lang, fallbacks);
            println!(
                "  + {} fallback pick(s) registered — --collect grabs the next one automatically if the payload probe fails",
                fallbacks.len()
            );
        }
        println!("Radarr never sees this download. When it completes: arr radarr subtitle-harvest --collect");
    } else {
        println!("FAILED to add to SAB: {}", best.s("title"));
    }
}

// --- candidate scoring -------------------------------------------------------

/// Language evidence a release *name* can carry. Native-market releases are
/// the ones that embed original-language subs; western "ESub" rips carry
/// English only.
fn native_markers(lang: &str) -> &'static [&'static str] {
    match lang {
        "chi" => &["chinese", "mandarin", "cantonese"],
        "hin" => &["hindi"],
        "jpn" => &["japanese"],
        "kor" => &["korean"],
        "fre" => &["french", "truefrench", "vff", "vfq"],
        "ger" => &["german"],
        "spa" => &["spanish", "castellano", "latino"],
        "ita" => &["italian"],
        "rus" => &["russian"],
        "por" => &["portuguese", "brazilian"],
        "tha" => &["thai"],
        "tel" => &["telugu"],
        "tam" => &["tamil"],
        "vie" => &["vietnamese"],
        "pol" => &["polish"],
        "tur" => &["turkish"],
        "ara" => &["arabic"],
        "heb" => &["hebrew"],
        "cze" => &["czech"],
        "swe" => &["swedish"],
        "dan" => &["danish"],
        "nor" => &["norwegian", "norsk"],
        _ => &[],
    }
}

fn score_release(title: &str, lang: &str) -> (i64, Vec<&'static str>) {
    let t = title.to_lowercase();
    let mut score = 0i64;
    let mut why: Vec<&'static str> = vec![];
    if native_markers(lang).iter().any(|m| t.contains(m)) {
        score += 3;
        why.push("native-market marker");
    }
    if ["msub", "multi-sub", "multi sub", "multisub", "multi.sub"].iter().any(|m| t.contains(m)) {
        score += 2;
        why.push("multi-sub marker");
    }
    if lang == "eng" && t.contains("esub") {
        score += 2;
        why.push("esub marker");
    }
    if t.contains("remux") {
        score += 2;
        why.push("remux (full disc tracks)");
    } else if ["bluray", "blu-ray", "bdrip", "bdremux"].iter().any(|m| t.contains(m)) {
        score += 2;
        why.push("bluray source");
    } else if t.contains("web-dl") || t.contains("webdl") {
        score += 1;
        why.push("web-dl source");
    }
    if t.contains("yts") || t.contains("yify") {
        score -= 3;
        why.push("yts/yify (strips subs)");
    }
    if ["telesync", "hdts", ".cam.", "camrip"].iter().any(|m| t.contains(m)) {
        score -= 5;
        why.push("cam/ts");
    }
    (score, why)
}

// --- SAB plumbing ------------------------------------------------------------

/// `harvest.m<movieId>.<lang>.<safe-title>` — the SAB job name is the whole
/// contract between grab and collect (no state needed to route a payload).
fn job_name(mid: i64, lang: &str, title: &str) -> String {
    let safe: String = title
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || ".-_".contains(c) { c } else { '.' })
        .take(80)
        .collect();
    format!("harvest.m{}.{}.{}", mid, lang, safe)
}

/// (movieId, lang) from a job name; (0, "") when it isn't ours.
fn parse_job_name(name: &str) -> (i64, String) {
    let rest = match name.strip_prefix("harvest.m") {
        Some(r) => r,
        None => return (0, String::new()),
    };
    let mut it = rest.splitn(3, '.');
    let mid = it.next().and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    let lang = it.next().unwrap_or("").to_string();
    if mid > 0 && (2..=3).contains(&lang.len()) {
        (mid, lang)
    } else {
        (0, String::new())
    }
}

fn ensure_sab_category() {
    let cfg = sab_api("get_config", &[("section", "categories")], 120);
    let present = cfg
        .at(&["config", "categories"])
        .as_array()
        .map(|a| a.iter().any(|c| c.s("name") == SAB_CATEGORY))
        .unwrap_or(false);
    if !present {
        sab_api(
            "set_config",
            &[
                ("section", "categories"),
                ("name", SAB_CATEGORY),
                ("dir", SAB_CATEGORY),
                ("priority", "-1"), // Low: harvests never crowd real requests
            ],
            120,
        );
        println!("created SAB category '{}' (dir={}, prio=Low)", SAB_CATEGORY, SAB_CATEGORY);
    }
}

// --- tried-list state --------------------------------------------------------
// /data/downloads/.subs-harvest.json — {"tried": {"<mid>:<lang>": [{title, when, outcome}]}}
// Advisory: a write failure warns and continues (worst case a candidate is
// retried later); the job name alone is enough to route any payload.

fn state_load() -> Value {
    std::fs::read_to_string(STATE_PATH)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({"tried": {}}))
}

fn state_save(st: &Value) {
    let tmp = format!("{}.tmp", STATE_PATH);
    let ser = serde_json::to_string_pretty(st).unwrap_or_default();
    let ok = std::fs::write(&tmp, ser).and_then(|_| std::fs::rename(&tmp, STATE_PATH));
    if let Err(e) = ok {
        eprintln!("arr: warn: couldn't persist harvest state to {} ({})", STATE_PATH, e);
    }
}

fn tried_titles(mid: i64, lang: &str) -> std::collections::HashSet<String> {
    state_load()
        .at(&["tried"])
        .get(format!("{}:{}", mid, lang))
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|e| e.s("title").to_string()).collect())
        .unwrap_or_default()
}

fn record_outcome(mid: i64, lang: &str, title: &str, outcome: &str) {
    let mut st = state_load();
    let key = format!("{}:{}", mid, lang);
    let entry = json!({"title": title, "when": now_iso(), "outcome": outcome});
    let tried = st
        .get_mut("tried")
        .and_then(Value::as_object_mut)
        .expect("state always has tried");
    match tried.get_mut(&key).and_then(Value::as_array_mut) {
        // A grab recorded as "downloading" is finalized in place by collect.
        Some(list) => {
            if let Some(prev) = list
                .iter_mut()
                .find(|e| e.s("title") == title)
            {
                *prev = entry;
            } else {
                list.push(entry);
            }
        }
        None => {
            tried.insert(key, json!([entry]));
        }
    }
    state_save(&st);
}

fn record_grab(mid: i64, lang: &str, title: &str) {
    record_outcome(mid, lang, title, "downloading");
}

// Pre-picked runner-up releases per movie+lang, consumed head-first by
// collect when a payload probe fails. state: {"fallbacks": {"mid:lang": ["match", ...]}}

fn fallbacks_set(mid: i64, lang: &str, matches: &[String]) {
    let mut st = state_load();
    if !st.has("fallbacks") {
        st["fallbacks"] = json!({});
    }
    st["fallbacks"][format!("{}:{}", mid, lang)] = json!(matches);
    state_save(&st);
}

/// Take the whole remaining chain for a movie (any lang key — chains are
/// per-movie in practice; the lang in the key is just bookkeeping).
fn fallbacks_take(mid: i64) -> Option<(String, Vec<String>)> {
    let st = state_load();
    let fb = st.at(&["fallbacks"]).as_object()?;
    let prefix = format!("{}:", mid);
    for (k, v) in fb {
        if k.starts_with(&prefix) {
            let list: Vec<String> = v
                .as_array()
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default();
            if !list.is_empty() {
                return Some((k.clone(), list));
            }
        }
    }
    None
}

fn fallbacks_store(key: &str, remaining: &[String]) {
    let mut st = state_load();
    if !st.has("fallbacks") {
        st["fallbacks"] = json!({});
    }
    if remaining.is_empty() {
        if let Some(o) = st["fallbacks"].as_object_mut() {
            o.remove(key);
        }
    } else {
        st["fallbacks"][key] = json!(remaining);
    }
    state_save(&st);
}

/// A payload probe failed: grab the next pre-picked release, if any. Works
/// down the chain until one grab succeeds (a vanished release just advances).
fn try_fallback(mid: i64, lang: &str, dry: bool) {
    let (key, mut chain) = match fallbacks_take(mid) {
        Some(x) => x,
        None => return,
    };
    println!("  fallback chain has {} pick(s) — searching for the next one", chain.len());
    if dry {
        println!("  DRY: would grab first available of: {}", chain.join(" || "));
        return;
    }
    let rels = match arr_api::try_api(
        "radarr",
        "GET",
        &format!("/release?movieId={}", mid),
        None,
        crate::browse::SEARCH_TIMEOUT,
    ) {
        Ok(v) => v,
        Err(_) => {
            println!("  release search failed — chain kept for next --collect");
            return;
        }
    };
    while !chain.is_empty() {
        let want = chain.remove(0).to_lowercase();
        let mut cands: Vec<&Value> = items(&rels)
            .iter()
            .filter(|r| {
                r.s("protocol") == "usenet"
                    && !r.s("downloadUrl").is_empty()
                    && r.s("title").to_lowercase().contains(&want)
            })
            .collect();
        cands.sort_by_key(|r| r.i("size"));
        match cands.first() {
            Some(best) => {
                ensure_sab_category();
                let job = job_name(mid, lang, best.s("title"));
                if crate::acquire::sab_add_url(best.s("downloadUrl"), SAB_CATEGORY, &job) {
                    record_grab(mid, lang, best.s("title"));
                    fallbacks_store(&key, &chain);
                    println!("  fallback grabbed: {}", best.s("title"));
                    return;
                }
                println!("  fallback SAB add failed for '{}' — trying next", want);
            }
            None => println!("  fallback '{}' matches no available release — trying next", want),
        }
    }
    fallbacks_store(&key, &chain);
    println!("  fallback chain exhausted — film goes to the provider/retry path");
}

fn now_iso() -> String {
    // chrono-free local-enough timestamp (UTC), matching the notes elsewhere
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    crate::policy::utc_iso(secs)
}

// --- collect: process completed harvest downloads ----------------------------

fn collect(dry: bool) {
    // downloading harvests: report only
    let sabq = sab_api("queue", &[("start", "0"), ("limit", "1000")], 120);
    let mut active = 0;
    for s in sabq.at(&["queue", "slots"]).as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        if s.s("cat") == SAB_CATEGORY {
            active += 1;
            println!("downloading: {} ({}%)", s.s("filename"), s.s("percentage"));
        }
    }

    let hist = sab_api(
        "history",
        &[("start", "0"), ("limit", "500"), ("category", SAB_CATEGORY)],
        120,
    );
    let slots: Vec<Value> = hist
        .at(&["history", "slots"])
        .as_array()
        .map(|a| a.iter().filter(|s| s.s("category") == SAB_CATEGORY).cloned().collect())
        .unwrap_or_default();
    if slots.is_empty() && active == 0 {
        println!("no harvest downloads in SAB (queue or history)");
        return;
    }
    let mut harvested_any = false;
    for s in &slots {
        let name = s.s("name");
        let nzo = s.s("nzo_id").to_string();
        let (mid, name_lang) = parse_job_name(name);
        match s.s("status") {
            "Completed" => {}
            "Failed" => {
                println!("FAILED download: {} — removing", name);
                if !dry {
                    if mid > 0 {
                        record_outcome(mid, &name_lang, name, "download-failed");
                    }
                    sab_delete_history(&nzo);
                }
                continue;
            }
            other => {
                println!("{}: {} (leaving in history)", other, name);
                continue;
            }
        }
        if mid == 0 {
            println!("⚠ unroutable job name '{}' (not harvest.m<id>.<lang>.*) — leaving for manual handling", name);
            continue;
        }
        println!("processing: {}", name);
        let storage = s.s("storage").to_string();
        let outcome = harvest_payload(mid, &name_lang, &storage, dry);
        if !dry {
            record_outcome(mid, &name_lang, name, &outcome);
            // Fail closed: only delete a payload we actually probed. An
            // "empty"/unroutable outcome may be a bug on our side (2026-08-14:
            // storage pointed at the file, not the dir, and a good payload was
            // deleted unprobed) — leave it for the next run / manual look.
            let probed = outcome.starts_with("harvested")
                || outcome.starts_with("no-")
                || outcome == "already-satisfied";
            if probed {
                sab_delete_history(&nzo); // del_files=1 — the payload video is never kept
                println!("  payload deleted (SAB history + files)");
            } else {
                println!("  payload KEPT (outcome: {}) — will retry next --collect", outcome);
            }
            // probe came up empty -> advance the pre-picked fallback chain
            if let Some(missing) = outcome.strip_prefix("no-").and_then(|s| s.strip_suffix("-subs")) {
                let lang = missing.split('+').next().unwrap_or(&name_lang).to_string();
                try_fallback(mid, &lang, dry);
            }
        }
        if outcome.starts_with("harvested") {
            harvested_any = true;
        }
    }
    if harvested_any && !dry {
        // soft refresh so Jellyfin notices the new sidecars
        arr_api::jf_api("/Library/Refresh", &[], 60, "POST", true);
        println!("Jellyfin library refresh triggered");
    }
}

/// Extract every wanted subtitle language present in the payload next to the
/// movie's existing file. Returns an outcome string for the tried-list.
fn harvest_payload(mid: i64, name_lang: &str, storage: &str, dry: bool) -> String {
    let movie = api("radarr", "GET", &format!("/movie/{}", mid), None).unwrap_or(Value::Null);
    if movie.is_null() {
        println!("  ⚠ movie #{} no longer in Radarr — skipping extraction", mid);
        return "movie-gone".into();
    }
    // Wanted langs: the movie's live require-subs-* tags override the job-name
    // hint (tags may have been edited since the grab).
    let mut wanted: Vec<String> = require_subs_langs(&movie);
    if wanted.is_empty() && !name_lang.is_empty() {
        wanted.push(norm_lang(name_lang));
    }
    if wanted.is_empty() {
        println!("  ⚠ no require-subs-* tag and no lang in job name — nothing to harvest");
        return "no-target-lang".into();
    }
    let dest = match movie_file_path(mid, &movie) {
        Some(p) => p,
        None => {
            println!("  ⚠ movie has no file on disk to attach sidecars to — skipping");
            return "no-library-file".into();
        }
    };
    // still missing = wanted minus what the library file already has
    let (_, have, _) = file_tracks(&dest);
    let missing: Vec<String> =
        wanted.iter().filter(|l| !have.iter().any(|t| t.lang == **l)).cloned().collect();
    if missing.is_empty() {
        println!("  {} subs already present on library file — nothing to do", wanted.join("+"));
        return "already-satisfied".into();
    }

    let (videos, loose) = payload_scan(storage);
    if videos.is_empty() && loose.is_empty() {
        println!("  ⚠ no video or subtitle files found under {}", storage);
        return "empty-payload".into();
    }
    let mut got: Vec<String> = vec![];
    for lang in &missing {
        match harvest_lang(&videos, &loose, lang, &dest, dry) {
            Some(signal) => {
                println!("  {} subs harvested (signal: {})", lang, signal);
                got.push(lang.clone());
            }
            None => println!(
                "  no {} subtitles in payload (checked stream language tags, stream titles, loose file names, text script)",
                lang
            ),
        }
    }
    if got.is_empty() {
        return format!("no-{}-subs", missing.join("+"));
    }
    // verify: the sidecars must now show up on the library file
    let (_, after, _) = file_tracks(&dest);
    for l in &got {
        let ok = after.iter().any(|t| t.lang == *l);
        println!(
            "  verify: {} subs on library file {}",
            l,
            if ok { "✓" } else { "⚠ NOT VISIBLE (check sidecar name/perms)" }
        );
    }
    format!("harvested-{}", got.join("+"))
}

/// One language, every signal in evidence order. Returns the signal that
/// produced a sidecar, or None.
///
/// 1. embedded stream whose language TAG matches
/// 2. embedded stream whose TITLE names the language while the tag is und/absent
///    ("Hindi", "Korean (SDH)" — release muxers fill titles more often than tags)
/// 3. loose subtitle FILE whose name indicates the language ("Hindi.srt",
///    "2_Korean.srt", "movie.hin.srt", an .idx/.sub pair)
/// 4. und/untagged TEXT streams judged by their content's writing system —
///    a stream that is mostly Devanagari IS the Hindi track (non-Latin-script
///    targets only; Latin-script languages are indistinguishable this cheaply)
fn harvest_lang(
    videos: &[String],
    loose: &[String],
    lang: &str,
    dest: &str,
    dry: bool,
) -> Option<&'static str> {
    // 1 + 2: embedded streams, tag first then title
    for pass in 0..2 {
        for v in videos {
            let streams = match crate::policy::ffprobe_streams(v) {
                Some(s) => s,
                None => continue,
            };
            let matches: Vec<&Value> = streams
                .iter()
                .filter(|st| {
                    if st.s("codec_type") != "subtitle" {
                        return false;
                    }
                    let tag = norm_lang(st.at(&["tags", "language"]).as_str().unwrap_or(""));
                    if pass == 0 {
                        tag == *lang
                    } else {
                        tag == "und" && name_indicates(st.at(&["tags", "title"]).as_str().unwrap_or(""), lang)
                    }
                })
                .collect();
            if !matches.is_empty() && extract_streams(v, &matches, lang, dest, dry) > 0 {
                return Some(if pass == 0 { "embedded language tag" } else { "embedded stream title" });
            }
        }
    }
    // 3: loose subtitle files named for the language
    let mut copied = 0;
    for f in loose {
        let fname = f.rsplit('/').next().unwrap_or(f);
        if !name_indicates(fname, lang) {
            continue;
        }
        if f.to_lowercase().ends_with(".sub") && loose.iter().any(|o| o.to_lowercase().ends_with(".idx")) {
            continue; // the .idx half of a VobSub pair drives the copy of both
        }
        copied += copy_loose_sub(f, loose, lang, dest, dry);
    }
    if copied > 0 {
        return Some("loose subtitle file name");
    }
    // 4: untagged text streams, judged by writing system
    if script_check_supported(lang) {
        for v in videos {
            let streams = match crate::policy::ffprobe_streams(v) {
                Some(s) => s,
                None => continue,
            };
            for st in &streams {
                if st.s("codec_type") != "subtitle" {
                    continue;
                }
                let tag = norm_lang(st.at(&["tags", "language"]).as_str().unwrap_or(""));
                if tag != "und" {
                    continue; // tagged as something else — believe the tag
                }
                if !matches!(st.s("codec_name"), "subrip" | "srt" | "ass" | "ssa" | "mov_text" | "webvtt") {
                    continue; // bitmap subs have no text to judge
                }
                let text = match probe_stream_text(v, st.i("index")) {
                    Some(t) => t,
                    None => continue,
                };
                if text_matches_lang(&text, lang)
                    && extract_streams(v, &[st], lang, dest, dry) > 0
                {
                    return Some("text script detection");
                }
            }
        }
    }
    None
}

/// Language evidence in a file/track NAME: full language words (from the
/// release-scoring marker list) or unambiguous ISO-ish code tokens. "hi" and
/// "it" are deliberately absent (hearing-impaired / English words).
fn name_indicates(name: &str, lang: &str) -> bool {
    let n = name.to_lowercase();
    if native_markers(lang).iter().any(|m| n.contains(m)) {
        return true;
    }
    if lang == "eng" && n.contains("english") {
        return true;
    }
    let codes: &[&str] = match lang {
        "eng" => &["eng", "en"],
        "hin" => &["hin"],
        "kor" => &["kor", "ko"],
        "chi" => &["chi", "zho", "chs", "cht", "zh"],
        "jpn" => &["jpn", "jap", "ja"],
        "fre" => &["fre", "fra", "fr"],
        "ger" => &["ger", "deu", "de"],
        "spa" => &["spa", "esp", "es"],
        "ita" => &["ita"],
        "rus" => &["rus", "ru"],
        "por" => &["por", "pob", "pt"],
        "tha" => &["tha", "th"],
        "tel" => &["tel"],
        "tam" => &["tam"],
        _ => &[],
    };
    // the normalized 3-letter code itself always counts as a token
    n.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|tok| tok == lang || codes.contains(&tok))
}

/// Copy a loose subtitle file (plus the .sub half of a VobSub pair) as a
/// language-named sidecar next to `dest`. Returns #files placed.
fn copy_loose_sub(src: &str, loose: &[String], lang: &str, dest: &str, dry: bool) -> usize {
    let base = dest.rsplit_once('.').map(|(b, _)| b.to_string()).unwrap_or_else(|| dest.into());
    let ext = src.rsplit('.').next().unwrap_or("").to_lowercase();
    let mut placed = 0;
    let mut pairs: Vec<(String, String)> = vec![(src.to_string(), format!("{}.{}.{}", base, lang, ext))];
    if ext == "idx" {
        // VobSub: bring the .sub half along under the same sidecar name
        let sub_src = format!("{}.sub", src.strip_suffix(".idx").unwrap_or(src));
        if loose.iter().any(|o| o == &sub_src) || std::path::Path::new(&sub_src).exists() {
            pairs.push((sub_src, format!("{}.{}.sub", base, lang)));
        } else {
            println!("  ⚠ {} has no matching .sub — VobSub pair incomplete, skipping", src);
            return 0;
        }
    }
    for (from, to) in pairs {
        if std::path::Path::new(&to).exists() {
            println!("  sidecar already exists: {}", to);
            continue;
        }
        if dry {
            println!("  DRY copy {} -> {}", from, to);
            placed += 1;
            continue;
        }
        match std::fs::copy(&from, &to) {
            Ok(_) => {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o644));
                println!("  copied {} -> {}", from, to);
                placed += 1;
            }
            Err(e) => println!("  copy FAILED {} -> {}: {}", from, to, e),
        }
    }
    placed
}

/// Dump one text subtitle stream as SRT to stdout (bounded) for script
/// detection — no temp files.
fn probe_stream_text(video: &str, index: i64) -> Option<String> {
    let out = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-i", video, "-map"])
        .arg(format!("0:{}", index))
        .args(["-c:s", "srt", "-f", "srt", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut n = s.len().min(2_000_000);
    while n > 0 && !s.is_char_boundary(n) {
        n -= 1; // never truncate mid-codepoint (multibyte scripts!)
    }
    s.truncate(n);
    Some(s)
}

fn script_check_supported(lang: &str) -> bool {
    matches!(lang, "hin" | "kor" | "jpn" | "chi" | "tha" | "rus" | "ara" | "heb" | "tel" | "tam")
}

/// Is this subtitle text written (mostly) in `lang`'s writing system?
/// Blunt on purpose: ≥200 letters, ≥30% in the target script. Cyrillic→rus /
/// Devanagari→hin conflate sibling languages, which is fine here — the movie's
/// own original language is the context. chi vs jpn disambiguate on kana.
fn text_matches_lang(text: &str, lang: &str) -> bool {
    let (mut letters, mut target, mut kana, mut cjk) = (0usize, 0usize, 0usize, 0usize);
    for c in text.chars() {
        let u = c as u32;
        if c.is_alphabetic() {
            letters += 1;
        }
        match u {
            0x3040..=0x30FF => kana += 1,
            0x4E00..=0x9FFF => cjk += 1,
            _ => {}
        }
        let hit = match lang {
            "hin" => (0x0900..=0x097F).contains(&u),
            "kor" => (0xAC00..=0xD7AF).contains(&u) || (0x1100..=0x11FF).contains(&u),
            "jpn" => (0x3040..=0x30FF).contains(&u) || (0x4E00..=0x9FFF).contains(&u),
            "chi" => (0x4E00..=0x9FFF).contains(&u),
            "tha" => (0x0E00..=0x0E7F).contains(&u),
            "rus" => (0x0400..=0x04FF).contains(&u),
            "ara" => (0x0600..=0x06FF).contains(&u),
            "heb" => (0x0590..=0x05FF).contains(&u),
            "tel" => (0x0C00..=0x0C7F).contains(&u),
            "tam" => (0x0B80..=0x0BFF).contains(&u),
            _ => false,
        };
        if hit {
            target += 1;
        }
    }
    if letters < 200 || target * 10 < letters * 3 {
        return false;
    }
    match lang {
        // Japanese text always mixes kana in; Chinese has (almost) none
        "jpn" => kana * 50 >= kana + cjk,
        "chi" => kana * 50 < kana + cjk,
        _ => true,
    }
}

/// Extract matching streams as sidecars next to `dest`. Returns #files written.
fn extract_streams(video: &str, subs: &[&Value], lang: &str, dest: &str, dry: bool) -> usize {
    let base = dest.rsplit_once('.').map(|(b, _)| b.to_string()).unwrap_or_else(|| dest.into());
    let mut written = 0usize;
    let mut did_full = false;
    for st in subs {
        let forced = crate::policy::truthy(st.at(&["disposition", "forced"]));
        if !forced && did_full {
            continue; // one full track is enough
        }
        let codec = st.s("codec_name");
        let (ext, conv): (&str, Option<&str>) = match codec {
            "subrip" | "srt" => ("srt", None),
            "ass" | "ssa" => ("ass", None),
            "mov_text" => ("srt", Some("srt")),
            "webvtt" => ("vtt", None),
            "hdmv_pgs_subtitle" => ("sup", None),
            // VobSub: ffmpeg can't write a bare .idx/.sub pair, but a
            // subtitle-only Matroska (.mks) sidecar carries it losslessly and
            // Jellyfin serves .mks externally. (mkvextract would give .idx/.sub
            // directly, but mkvtoolnix isn't on the box.)
            "dvd_subtitle" => ("mks", None),
            other => {
                println!("  skipping unsupported subtitle codec '{}'", other);
                continue;
            }
        };
        let out = format!("{}.{}{}.{}", base, lang, if forced { ".forced" } else { "" }, ext);
        if std::path::Path::new(&out).exists() {
            println!("  sidecar already exists: {}", out);
            if !forced {
                did_full = true;
            }
            continue;
        }
        let idx = st.i("index").to_string();
        if dry {
            println!("  DRY extract 0:{} ({}) -> {}", idx, codec, out);
            written += 1;
            if !forced {
                did_full = true;
            }
            continue;
        }
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-nostdin", "-v", "error", "-y", "-i", video, "-map"])
            .arg(format!("0:{}", idx));
        match conv {
            Some(c) => {
                cmd.args(["-c:s", c]);
            }
            None => {
                cmd.args(["-c", "copy"]);
            }
        }
        cmd.arg(&out).stdout(Stdio::null()).stderr(Stdio::piped());
        let res = cmd.output();
        let ok = matches!(&res, Ok(o) if o.status.success())
            && std::fs::metadata(&out).map(|m| m.len() > 0).unwrap_or(false);
        if ok {
            // Jellyfin runs as another user: the sidecar must be world-readable
            // (the 2026-08-14 hermes .ja.srt was 0660 and invisible to it).
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o644));
            println!("  extracted 0:{} ({}) -> {}", idx, codec, out);
            written += 1;
            if !forced {
                did_full = true;
            }
        } else {
            let _ = std::fs::remove_file(&out); // never leave a truncated sidecar
            let errtxt = res
                .map(|o| String::from_utf8_lossy(&o.stderr).chars().take(200).collect::<String>())
                .unwrap_or_else(|e| e.to_string());
            println!("  extract FAILED 0:{} ({}): {}", idx, codec, errtxt.trim());
        }
    }
    written
}

/// (video files largest-first, loose subtitle files) under the payload.
fn payload_scan(dir: &str) -> (Vec<String>, Vec<String>) {
    let mut videos = vec![];
    let mut subs = vec![];
    // SAB's history `storage` sometimes points at the payload FILE itself
    // (single-file jobs), not the job directory — walk the parent then.
    let root = if std::path::Path::new(dir).is_file() {
        dir.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_else(|| dir.to_string())
    } else {
        dir.to_string()
    };
    let mut stack = vec![root];
    while let Some(d) = stack.pop() {
        let entries = match std::fs::read_dir(&d) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p.to_string_lossy().into_owned());
                continue;
            }
            let name = p.to_string_lossy().to_lowercase();
            if [".mkv", ".mp4", ".avi", ".m2ts", ".ts", ".wmv"].iter().any(|x| name.ends_with(x))
                && e.metadata().map(|m| m.len() >= MIN_VIDEO_BYTES).unwrap_or(false)
            {
                videos.push(p.to_string_lossy().into_owned());
            } else if [".srt", ".ass", ".ssa", ".sup", ".vtt", ".idx", ".sub"]
                .iter()
                .any(|x| name.ends_with(x))
            {
                subs.push(p.to_string_lossy().into_owned());
            }
        }
    }
    videos.sort_by_key(|p| std::cmp::Reverse(std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)));
    subs.sort();
    (videos, subs)
}

fn sab_delete_history(nzo: &str) {
    sab_api(
        "history",
        &[("name", "delete"), ("value", nzo), ("del_files", "1")],
        120,
    );
}

// --- adopt: take over Radarr-queued language-replacement downloads ------------

/// A language-recovery download sitting in RADARR's queue will, on completion,
/// REPLACE the library file (that's what a queue item is). Adoption re-points
/// it at the harvest pipeline instead: rename to the harvest contract name,
/// move it to the harvest SAB category, then drop the Radarr queue record
/// (keeping the download). Only items whose movie carries require-subs-* tags
/// (and no require-audio-*: audio genuinely needs replacement) and already has
/// a file are adopted — everything else is left alone.
fn adopt(yes: bool) {
    let q = api("radarr", "GET", "/queue?pageSize=1000&page=1", None).unwrap_or(Value::Null);
    let records: Vec<Value> = q.a("records").to_vec();
    let tags = tag_labels();
    let mut movies: HashMap<i64, Value> = HashMap::new();
    let mut plans: Vec<(Value, i64, String, &'static str)> = vec![]; // (record, mid, lang, action)
    let mut skipped: Vec<String> = vec![];
    for r in &records {
        let mid = r.i("movieId");
        if mid == 0 {
            continue;
        }
        let movie = movies.entry(mid).or_insert_with(|| {
            api("radarr", "GET", &format!("/movie/{}", mid), None).unwrap_or(Value::Null)
        });
        let subs = require_subs_langs_with(movie, &tags);
        let audio = require_langs_with(movie, &tags, "require-audio-");
        if subs.is_empty() {
            continue; // not a subtitle-hunt item
        }
        let label = format!("{} — {}", movie.s("title"), r.s("title"));
        if !audio.is_empty() {
            skipped.push(format!("{} (require-audio too: sidecars can't fix audio — leave as replacement)", label));
            continue;
        }
        if !movie.b("hasFile") {
            skipped.push(format!("{} (no library file yet — let it import normally)", label));
            continue;
        }
        if r.s("protocol") != "usenet" {
            skipped.push(format!("{} (torrent — adopt by hand)", label));
            continue;
        }
        let action = if r.s("status") == "completed" { "harvest-now" } else { "convert" };
        // job-name lang is a routing hint only — collect re-reads the live
        // require-subs-* tags and harvests every missing language
        plans.push((r.clone(), mid, subs.join("+"), action));
    }
    for s in &skipped {
        println!("skip: {}", s);
    }
    if plans.is_empty() {
        println!("nothing to adopt ({} queue records checked)", records.len());
        return;
    }
    println!("{} adoption(s) planned:", plans.len());
    for (r, mid, lang, action) in &plans {
        println!("  [{}] m{} {} — {}", action, mid, lang, r.s("title"));
    }
    if !yes {
        println!("(dry-run — pass --yes to apply)");
        return;
    }
    ensure_sab_category();
    let mut converted = 0;
    let mut harvested = 0;
    for (r, mid, lang, action) in &plans {
        let nzo = r.s("downloadId").to_string();
        let qid = r.i("id");
        let first_lang = lang.split('+').next().unwrap_or(lang).to_string();
        if *action == "convert" {
            // 1) rename to the harvest contract name (routes collect later)
            let newname = job_name(*mid, &first_lang, r.s("title"));
            sab_api("queue", &[("name", "rename"), ("value", &nzo), ("value2", &newname)], 120);
            // 2) move to the harvest category (Radarr's CDH never scans it)
            sab_api("change_cat", &[("value", &nzo), ("value2", SAB_CATEGORY)], 120);
            // 3) verify BOTH took effect before touching the Radarr record —
            //    if the SAB job vanished mid-flight, leave the record alone
            let check = sab_api("queue", &[("start", "0"), ("limit", "1000")], 120);
            let seen = check
                .at(&["queue", "slots"])
                .as_array()
                .map(|a| a.iter().any(|s| s.s("nzo_id") == nzo && s.s("cat") == SAB_CATEGORY))
                .unwrap_or(false);
            if !seen {
                println!("  ⚠ m{}: SAB job {} not confirmed in cat {} — leaving Radarr record untouched", mid, nzo, SAB_CATEGORY);
                continue;
            }
            api(
                "radarr",
                "DELETE",
                &format!("/queue/{}?removeFromClient=false&blocklist=false", qid),
                None,
            );
            record_grab(*mid, &first_lang, r.s("title"));
            println!("  converted: m{} {} — {}", mid, lang, r.s("title"));
            converted += 1;
        } else {
            // completed + waiting in Radarr's queue: harvest in place, then
            // drop record AND payload, blocklisting so nothing re-grabs it
            let out = r.s("outputPath").to_string();
            let dir = if std::path::Path::new(&out).is_file() {
                out.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or(out.clone())
            } else {
                out.clone()
            };
            let outcome = harvest_payload(*mid, &first_lang, &dir, false);
            record_outcome(*mid, &first_lang, r.s("title"), &outcome);
            api(
                "radarr",
                "DELETE",
                &format!("/queue/{}?removeFromClient=true&blocklist=true", qid),
                None,
            );
            println!("  harvested-in-place ({}) + record removed: {}", outcome, r.s("title"));
            harvested += 1;
        }
    }
    println!(
        "adopted {} item(s): {} converted to cat={}, {} harvested in place",
        converted + harvested,
        converted,
        SAB_CATEGORY,
        harvested
    );
    if converted > 0 {
        println!("run `arr radarr subtitle-harvest --collect` as downloads complete (cron-friendly)");
    }
}

// --- status ------------------------------------------------------------------

fn status() {
    let sabq = sab_api("queue", &[("start", "0"), ("limit", "1000")], 120);
    let mut n = 0;
    for s in sabq.at(&["queue", "slots"]).as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        if s.s("cat") == SAB_CATEGORY {
            n += 1;
            println!("downloading: {} ({}%, {}MB left)", s.s("filename"), s.s("percentage"), s.s("mbleft"));
        }
    }
    let hist = sab_api(
        "history",
        &[("start", "0"), ("limit", "100"), ("category", SAB_CATEGORY)],
        120,
    );
    for s in hist.at(&["history", "slots"]).as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        if s.s("category") == SAB_CATEGORY {
            n += 1;
            println!("history [{}]: {}", s.s("status"), s.s("name"));
        }
    }
    if n == 0 {
        println!("no harvest jobs in SAB");
    }
    let st = state_load();
    if let Some(tried) = st.at(&["tried"]).as_object() {
        if !tried.is_empty() {
            println!("tried-list ({} movie/lang pair(s)):", tried.len());
            for (k, v) in tried {
                let outcomes: Vec<String> = v
                    .as_array()
                    .map(|a| a.iter().map(|e| format!("{} [{}]", e.s("title"), e.s("outcome"))).collect())
                    .unwrap_or_default();
                println!("  {}: {}", k, outcomes.join("; "));
            }
        }
    }
}

// --- shared helpers ----------------------------------------------------------

fn movie_file_path(mid: i64, movie: &Value) -> Option<String> {
    let mf = api("radarr", "GET", &format!("/moviefile?movieId={}", mid), None);
    for f in items(&mf) {
        let p = f.s("path");
        if !p.is_empty() {
            return Some(p.to_string());
        }
        let rel = f.s("relativePath");
        if !rel.is_empty() && !movie.s("path").is_empty() {
            return Some(format!("{}/{}", movie.s("path").trim_end_matches('/'), rel));
        }
    }
    None
}

fn tag_labels() -> HashMap<i64, String> {
    let tags = api("radarr", "GET", "/tag", None);
    items(&tags).iter().map(|t| (t.i("id"), t.s("label").to_string())).collect()
}

fn require_subs_langs(movie: &Value) -> Vec<String> {
    require_subs_langs_with(movie, &tag_labels())
}

fn require_subs_langs_with(movie: &Value, tags: &HashMap<i64, String>) -> Vec<String> {
    require_langs_with(movie, tags, "require-subs-")
}

fn require_langs_with(movie: &Value, tags: &HashMap<i64, String>, prefix: &str) -> Vec<String> {
    let mut out: Vec<String> = movie
        .a("tags")
        .iter()
        .filter_map(|id| tags.get(&id.as_i64().unwrap_or(0)))
        .filter_map(|l| l.to_lowercase().strip_prefix(prefix).map(norm_lang))
        .collect();
    out.dedup();
    out
}

#[cfg(test)]
mod harvest_tests {
    use super::*;

    #[test]
    fn job_names_round_trip() {
        let n = job_name(403, "hin", "Dil.Chahta.Hai.2001.Hindi.BluRay.1080p [x265]");
        assert!(n.starts_with("harvest.m403.hin."));
        assert!(!n.contains(' ') && !n.contains('['));
        assert_eq!(parse_job_name(&n), (403, "hin".to_string()));
        assert_eq!(parse_job_name("Some.Random.Release.2020"), (0, String::new()));
    }

    #[test]
    fn filename_language_indication() {
        assert!(name_indicates("2_Korean.srt", "kor"));
        assert!(name_indicates("Hindi.srt", "hin"));
        assert!(name_indicates("Movie.Name.2010.hin.srt", "hin"));
        assert!(name_indicates("English (SDH).srt", "eng"));
        assert!(name_indicates("subs/movie.fr.forced.srt", "fre"));
        assert!(!name_indicates("Movie.HI.srt", "hin")); // hi = hearing-impaired, not Hindi
        assert!(!name_indicates("It.Follows.srt", "ita")); // "it" is an English word
        assert!(!name_indicates("English.srt", "kor"));
    }

    #[test]
    fn text_script_detection() {
        let hindi = "नमस्ते दुनिया यह एक परीक्षण है ".repeat(30);
        let english = "hello world this is a test of the script detector ".repeat(30);
        let korean = "안녕하세요 세계 이것은 시험입니다 ".repeat(30);
        let japanese = "こんにちは世界これはテストです漢字も含む ".repeat(30);
        let chinese = "你好世界这是一个测试字幕文件内容 ".repeat(30);
        assert!(text_matches_lang(&hindi, "hin"));
        assert!(!text_matches_lang(&english, "hin"));
        assert!(text_matches_lang(&korean, "kor"));
        assert!(text_matches_lang(&japanese, "jpn"));
        assert!(!text_matches_lang(&japanese, "chi")); // kana present -> not Chinese
        assert!(text_matches_lang(&chinese, "chi"));
        let short: String = hindi.chars().take(20).collect();
        assert!(!text_matches_lang(&short, "hin")); // too short to judge
        assert!(!text_matches_lang(&english, "eng")); // Latin targets unsupported
    }

    #[test]
    fn scoring_prefers_native_market_over_yts() {
        let (native, _) = score_release("Sholay.1975.HINDI.1080p.BluRay.x264-NATIVE", "hin");
        let (yts, _) = score_release("Sholay.1975.720p.BluRay.x264-YTS", "hin");
        assert!(native > yts);
        assert!(native >= 5); // hindi + bluray
        let (esub_not_hin, _) = score_release("Movie.2020.1080p.WEB-DL.ESub", "hin");
        assert!(esub_not_hin <= 1); // esub only counts toward eng
    }
}
