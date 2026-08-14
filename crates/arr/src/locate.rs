//! Cross-service resolution and `arr where`.
//!
//! The service prefix is this CLI's one leaky abstraction: callers think in
//! titles, but every command wants `sonarr` vs `sonarr-anime` vs `radarr` up
//! front — and guessing wrong returns "no match", which reads exactly like
//! "not in the library". `locate()` removes the guess so `arr status <title>`
//! works with no prefix; `arr where` answers the question that guess was
//! usually in service of: where is this in the pipeline (Seerr request -> arr
//! -> download queue -> disk -> Jellyfin), and what is the next command?

use std::collections::HashMap;

use arr_api::{api, die, fmt_gb, jf_api, pop_flags, sab_api, seerr_api, JsonExt};
use serde_json::Value;

use crate::{browse, integrations, policy};

/// Services that hold library items (prowlarr has none).
pub const ITEM_SVCS: [&str; 3] = ["radarr", "sonarr", "sonarr-anime"];

pub struct Hit {
    pub svc: &'static str,
    pub item: Value,
}

fn as_list(v: Option<Value>) -> Vec<Value> {
    match v {
        Some(Value::Array(a)) => a,
        _ => vec![],
    }
}

fn year_of(item: &Value) -> String {
    match item.get("year").and_then(Value::as_i64) {
        Some(y) if y > 0 => y.to_string(),
        _ => "?".to_string(),
    }
}

fn library<'a>(cache: &'a mut HashMap<&'static str, Vec<Value>>, svc: &'static str) -> &'a [Value] {
    cache.entry(svc).or_insert_with(|| {
        let coll = if arr_api::is_series(svc) { "series" } else { "movie" };
        as_list(api(svc, "GET", &format!("/{}", coll), None))
    })
}

/// Substring match over title / slug / alternate titles — the same rule
/// resolve_id and matches_in use, so a prefix-free query resolves identically.
fn title_hits(lib: &[Value], ql: &str) -> Vec<Value> {
    lib.iter()
        .filter(|it| {
            let mut names: Vec<&str> = vec![it.s("title"), it.s("titleSlug")];
            names.extend(it.a("alternateTitles").iter().map(|a| a.s("title")));
            names.iter().any(|n| n.to_lowercase().contains(ql))
        })
        .cloned()
        .collect()
}

/// Find a title across radarr/sonarr/sonarr-anime. Err carries a message ready
/// to print: "no movie or show matching …" or an "ambiguous …" candidate list.
pub fn locate(q: &str) -> Result<Hit, String> {
    if !q.is_empty() && q.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!(
            "\"{q}\" is a numeric id, and ids are only unique within one service — name the service (`arr sonarr-anime … {q}`) or pass the title instead"
        ));
    }
    let ql = q.to_lowercase();
    let mut cache: HashMap<&'static str, Vec<Value>> = HashMap::new();
    let mut hits: Vec<Hit> = vec![];
    for svc in ITEM_SVCS {
        for item in title_hits(library(&mut cache, svc), &ql) {
            hits.push(Hit { svc, item });
        }
    }
    match hits.len() {
        0 => Err(format!("no movie or show matching \"{}\"", q)),
        1 => Ok(hits.pop().unwrap()),
        n => {
            // An exact title beats substring noise ("Fargo" over "Fargo: ...").
            let exact: Vec<usize> = hits
                .iter()
                .enumerate()
                .filter(|(_, h)| h.item.s("title").to_lowercase() == ql)
                .map(|(i, _)| i)
                .collect();
            if exact.len() == 1 {
                return Ok(hits.swap_remove(exact[0]));
            }
            let cands: Vec<String> = hits
                .iter()
                .take(6)
                .map(|h| {
                    format!("{}:{} ({}) [{}]", h.svc, h.item.s("title"), year_of(&h.item), h.item.i("id"))
                })
                .collect();
            Err(format!(
                "ambiguous \"{}\" — {} matches: {}; rerun with the service and id",
                q,
                n,
                cands.join(", ")
            ))
        }
    }
}

/// Commands whose first positional is an `<id|query>`, so they can run without
/// a service prefix. Excludes multi-target (`watch`) and service-specific
/// (`replace`, `parse`, `import`, `search`) commands.
pub fn is_item_command(cmd: &str) -> bool {
    matches!(
        cmd,
        "status"
            | "get"
            | "seasons"
            | "releases"
            | "grab"
            | "monitor"
            | "episodes"
            | "history"
            | "files"
            | "audit"
            | "availability"
            | "info"
            | "tag"
            | "coverage"
            | "tracks"
            | "searches"
    )
}

/// `arr <command> <title> …` with no service prefix: resolve the title, then
/// run the ordinary per-service command against the resolved id.
pub fn dispatch(cmd: &str, args: &[String]) {
    if args.is_empty() || args[0].starts_with('-') {
        die(&format!(
            "arr {cmd}: put the title first (`arr {cmd} 'Lycoris Recoil' …`), or name the service (`arr <svc> {cmd} …`)"
        ));
    }
    let hit = match locate(&args[0]) {
        Ok(h) => h,
        Err(e) => die(&e),
    };
    println!("→ {} #{} {} ({})", hit.svc, hit.item.i("id"), hit.item.s("title"), year_of(&hit.item));
    let mut rest = args.to_vec();
    // Every item command resolves its first positional via resolve_id, which
    // takes a bare id — so pin the resolution instead of letting it re-run.
    // `status` is the exception: its first arg is a title FILTER, not an id.
    if cmd != "status" {
        rest[0] = hit.item.i("id").to_string();
    }
    crate::run_svc_command(hit.svc, cmd, &rest);
}

// --- arr where ---------------------------------------------------------------

fn norm_words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// Every word of `title` present in `haystack` — matches "Lycoris Recoil"
/// against "Lycoris.Recoil.S01E01.1080p...".
fn name_matches(haystack: &str, title: &str) -> bool {
    let hw = norm_words(haystack);
    let tw = norm_words(title);
    !tw.is_empty() && tw.iter().all(|w| hw.contains(w))
}

fn rec_item_id(r: &Value, is_series: bool) -> i64 {
    if is_series {
        let s = r.i("seriesId");
        if s != 0 { s } else { r.at(&["series", "id"]).as_i64().unwrap_or(0) }
    } else {
        let m = r.at(&["movie", "id"]).as_i64().unwrap_or(0);
        if m != 0 { m } else { r.i("movieId") }
    }
}

const RSTAT: &[(i64, &str)] =
    &[(1, "pending"), (2, "approved"), (3, "declined"), (4, "failed"), (5, "completed")];
const MSTAT: &[(i64, &str)] =
    &[(1, "unknown"), (2, "pending"), (3, "processing"), (4, "partial"), (5, "available")];

fn stat_name(map: &[(i64, &str)], v: i64) -> String {
    map.iter().find(|(k, _)| *k == v).map(|(_, n)| n.to_string()).unwrap_or_else(|| v.to_string())
}

/// Seerr requests for this title, matched by tmdb/tvdb id (ids come from the
/// arr item when we have one, else from a Seerr search).
fn seerr_lines(title: &str, item: Option<&Value>) -> Vec<String> {
    let (mut tmdb, mut tvdb) = (0i64, 0i64);
    if let Some(it) = item {
        tmdb = it.i("tmdbId");
        tvdb = it.i("tvdbId");
    }
    if tmdb == 0 && tvdb == 0 {
        let sr = seerr_api("/search", &[("query", title)], 30, true).unwrap_or(Value::Null);
        if let Some(first) = sr
            .a("results")
            .iter()
            .find(|r| matches!(r.s("mediaType"), "movie" | "tv"))
        {
            tmdb = first.i("id");
        }
    }
    if tmdb == 0 && tvdb == 0 {
        return vec![];
    }
    let data =
        seerr_api("/request", &[("take", "300"), ("skip", "0"), ("sort", "added")], 60, true)
            .unwrap_or(Value::Null);
    let mut out = vec![];
    for r in data.a("results") {
        let m = r.at(&["media"]);
        let hit = (tmdb != 0 && m.i("tmdbId") == tmdb) || (tvdb != 0 && m.i("tvdbId") == tvdb);
        if !hit {
            continue;
        }
        let by = match r.at(&["requestedBy"]).get("displayName").and_then(Value::as_str) {
            Some(n) => n.to_string(),
            None => "?".to_string(),
        };
        out.push(format!(
            "request #{} by {} — {} (media {}){}",
            r.i("id"),
            by,
            stat_name(RSTAT, r.i("status")).to_uppercase(),
            stat_name(MSTAT, m.i("status")),
            match r.s("createdAt").split('T').next() {
                Some(d) if !d.is_empty() => format!(", {}", d),
                _ => String::new(),
            }
        ));
    }
    out
}

/// arr where <title> — the whole pipeline in one call: which service holds it,
/// what is on disk, whether anything is downloading for it, the Seerr request
/// state, Jellyfin visibility, and the command that moves it forward.
pub fn cmd_where(args: &[String]) {
    let (_flags, rest) = pop_flags(args, &[]);
    if rest.is_empty() {
        die("where: need a title (e.g. `arr where 'Lycoris Recoil'`)");
    }
    let q = rest.join(" ");

    let located = locate(&q);
    if let Err(e) = &located {
        if e.starts_with("ambiguous") {
            die(e);
        }
    }
    let hit = located.ok();
    let title = hit.as_ref().map(|h| h.item.s("title").to_string()).unwrap_or_else(|| q.clone());
    println!("{}", title);

    // --- library ---
    let mut next: Vec<String> = vec![];
    let mut complete = false;
    let mut disk_files: i64 = 0;
    match &hit {
        None => {
            println!("  library    not in radarr, sonarr or sonarr-anime");
            next.push(format!(
                "arr radarr add '{q}'   (or `arr sonarr add` / `arr sonarr-anime add` for a show)"
            ));
        }
        Some(h) => {
            let it = &h.item;
            println!(
                "  library    {} #{} \"{}\" ({}) — {}",
                h.svc,
                it.i("id"),
                it.s("title"),
                year_of(it),
                if it.b("monitored") { "monitored" } else { "NOT monitored" }
            );
        }
    }

    // --- disk ---
    if let Some(h) = &hit {
        let it = &h.item;
        if arr_api::is_series(h.svc) {
            let (_s, cov) = policy::series_coverage(h.svc, it.i("id"), Some(it));
            let files: i64 = cov.iter().map(|c| c.files).sum();
            let aired: i64 = cov.iter().filter(|c| c.season != 0).map(|c| c.aired).sum();
            let pct = if aired > 0 { files * 100 / aired } else { 0 };
            println!("  disk       {}/{} aired episodes ({}%)", files, aired, pct);
            for c in cov.iter().filter(|c| c.season != 0) {
                let done = c.aired > 0 && c.files >= c.aired;
                let flag = if done {
                    "✓"
                } else if c.monitored {
                    "⚠"
                } else {
                    "○"
                };
                println!(
                    "             {} S{:<2} {}/{}{}",
                    flag,
                    c.season,
                    c.files,
                    c.aired,
                    if c.monitored { "" } else { "  (unmonitored — searches skip it)" }
                );
            }
            complete = aired > 0 && files >= aired;
            disk_files = files;
            let gaps_mon = cov.iter().any(|c| c.season != 0 && c.monitored && !c.missing.is_empty());
            let gaps_unmon = cov.iter().any(|c| c.season != 0 && !c.monitored && c.files < c.aired);
            if gaps_mon {
                next.push(format!("arr {} grab {}", h.svc, it.i("id")));
            } else if gaps_unmon {
                next.push(format!(
                    "arr {} grab {} --monitor   (gaps are all in unmonitored seasons)",
                    h.svc,
                    it.i("id")
                ));
            }
        } else {
            let has = it.b("hasFile");
            complete = has;
            if has {
                disk_files = 1;
                println!(
                    "  disk       1 file, {}GB — {}",
                    fmt_gb(it.at(&["movieFile", "size"]).as_i64().unwrap_or(0)),
                    it.at(&["movieFile", "quality", "quality", "name"]).as_str().unwrap_or("?")
                );
            } else {
                println!("  disk       no file");
                next.push(format!("arr {} grab {}", h.svc, it.i("id")));
            }
        }
    }

    // --- active searches --- (a grinding SeriesSearch is invisible in the
    // queue — without this line "arr queue: nothing downloading" reads as
    // "nothing is happening" while the arr is mid-search)
    if let Some(h) = &hit {
        let active: Vec<Value> = browse::search_commands(h.svc, Some(h.item.i("id")))
            .into_iter()
            .filter(|c| matches!(c.s("status"), "started" | "queued"))
            .collect();
        if !active.is_empty() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            for (i, c) in active.iter().enumerate() {
                println!(
                    "  {}{}",
                    if i == 0 { "search     " } else { "           " },
                    browse::search_command_line(c, now)
                );
            }
        }
    }

    // --- arr queue ---
    let mut download_ids: Vec<String> = vec![];
    if let Some(h) = &hit {
        let is_series = arr_api::is_series(h.svc);
        let recs = browse::queue_records(h.svc, 1000);
        let mine: Vec<&Value> =
            recs.a("records").iter().filter(|r| rec_item_id(r, is_series) == h.item.i("id")).collect();
        if mine.is_empty() {
            println!("  arr queue  nothing downloading");
        } else {
            println!("  arr queue  {} item(s)", mine.len());
            for r in mine.iter().take(6) {
                let (size, left) = (r.f("size"), r.f("sizeleft"));
                let pct = if size > 0.0 { ((size - left) / size * 100.0) as i64 } else { 0 };
                println!(
                    "             {:>3}% {} — {}",
                    pct,
                    r.s("status"),
                    crate::browse::queue_record_summary(r).s("title")
                );
                let did = r.s("downloadId");
                if !did.is_empty() {
                    download_ids.push(did.to_lowercase());
                }
            }
            next.push(format!(
                "arr {} grab {}   (already downloading — this promotes it to the front of the queue)",
                h.svc,
                h.item.i("id")
            ));
        }
    }

    // --- SAB queue ---
    let sab = sab_api("queue", &[("limit", "500")], 60);
    let slots = sab.at(&["queue"]).a("slots").to_vec();
    let mine: Vec<&Value> = slots
        .iter()
        .filter(|s| {
            download_ids.contains(&s.s("nzo_id").to_lowercase()) || name_matches(s.s("filename"), &title)
        })
        .collect();
    if mine.is_empty() {
        println!("  sab queue  no matching job ({} queued overall)", slots.len());
    } else {
        for s in mine.iter().take(6) {
            println!(
                "  sab queue  {} prio={} {}MB left — {}",
                s.s("status"),
                s.s("priority"),
                s.s("mbleft"),
                s.s("filename")
            );
        }
    }

    // --- seerr ---
    let sl = seerr_lines(&title, hit.as_ref().map(|h| &h.item));
    if sl.is_empty() {
        println!("  seerr      no request");
    } else {
        for (i, l) in sl.iter().enumerate() {
            println!("  {}{}", if i == 0 { "seerr      " } else { "           " }, l);
        }
        if sl.iter().any(|l| l.contains("FAILED")) {
            next.push("the Seerr request FAILED — re-request in the UI, or just grab it above".into());
        }
    }

    // --- jellyfin ---
    let jf = integrations::jf_search_items(&title, 5);
    if jf.is_empty() {
        println!("  jellyfin   not in the library");
        if complete {
            next.push(format!("arr jellyfin refresh --wait '{}'", title));
        }
    } else {
        // A series item with ZERO episodes while the arr has files on disk means
        // Jellyfin's scan dropped them (e.g. the AniDB "Season Unknown" cascade
        // delete on a freshly-added anime) — a rescan with warm metadata caches
        // restores them. Without this flag the line reads as healthy and "next"
        // says nothing to do.
        let mut warned_empty = false;
        for it in jf.iter().take(3) {
            let mut extra = String::new();
            if it.s("Type") == "Series" {
                if let Some(e) =
                    jf_api(&format!("/Shows/{}/Episodes", it.s("Id")), &[("Limit", "1")], 60, "GET", true)
                {
                    let n = e.i("TotalRecordCount");
                    extra = format!(", {} episode(s)", n);
                    if n == 0 && disk_files > 0 {
                        extra.push_str("  ⚠ files on disk but no episodes scanned in");
                        if !warned_empty {
                            warned_empty = true;
                            next.push(format!(
                                "arr jellyfin refresh --wait '{}'   (rescan usually restores the missing episodes)",
                                title
                            ));
                        }
                    }
                }
            }
            println!("  jellyfin   [{}] {}{}", it.s("Type"), it.s("Name"), extra);
        }
    }

    // --- next ---
    if next.is_empty() {
        println!("  next       nothing to do");
    } else {
        for (i, n) in next.iter().enumerate() {
            println!("  {}{}", if i == 0 { "next       " } else { "           " }, n);
        }
    }
}
