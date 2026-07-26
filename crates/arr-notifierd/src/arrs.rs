//! The daemon's own thin HTTP clients (arr instances from env-var config,
//! Seerr, Jellyfin) plus queue aggregation. `fetch_item` is the type-driven
//! arr_item_gone: Ok(None) is a DEFINITIVE 404, Err is transient.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use arr_api::{ApiError, JsonExt};
use serde_json::Value;

use crate::config::{cfg, inst, JELLYFIN_URL, SEERR_URL};
use crate::util::{log, now, py_round, truthy_scalar_str};

pub fn err_str(e: &ApiError) -> String {
    match e {
        ApiError::Http { code, detail } => {
            format!("HTTP {} {}", code, detail.chars().take(120).collect::<String>())
        }
        ApiError::Timeout => "timed out".to_string(),
        ApiError::Net(m) => m.clone(),
    }
}

fn http_get_json(
    url: &str,
    headers: &[(&str, &str)],
    timeout: u64,
) -> Result<Option<Value>, ApiError> {
    let mut req = ureq::agent()
        .request("GET", url)
        .timeout(Duration::from_secs(timeout));
    for (k, v) in headers {
        req = req.set(k, v);
    }
    match req.call() {
        Ok(r) => {
            let raw = r.into_string().map_err(|e| ApiError::Net(e.to_string()))?;
            if raw.trim().is_empty() {
                Ok(None)
            } else {
                serde_json::from_str(&raw)
                    .map(Some)
                    .map_err(|e| ApiError::Net(format!("bad json: {}", e)))
            }
        }
        Err(ureq::Error::Status(code, r)) => {
            let detail: String = r.into_string().unwrap_or_default().chars().take(500).collect();
            Err(ApiError::Http { code, detail })
        }
        Err(ureq::Error::Transport(t)) => {
            let msg = t.to_string();
            if msg.contains("timed out") || msg.contains("timeout") {
                Err(ApiError::Timeout)
            } else {
                Err(ApiError::Net(msg))
            }
        }
    }
}

/// Fallible GET against one of our arr instances.
pub fn arr_try_get(instn: &str, path: &str) -> Result<Option<Value>, ApiError> {
    let i = inst(instn).ok_or_else(|| ApiError::Net(format!("unknown instance '{}'", instn)))?;
    http_get_json(
        &format!("{}/api/v3/{}", i.url, path),
        &[("X-Api-Key", &i.key)],
        30,
    )
}

/// Python arr_get: WARN + None on any error (the daemon must survive blips).
pub fn arr_get(instn: &str, path: &str) -> Option<Value> {
    match arr_try_get(instn, path) {
        Ok(v) => v,
        Err(e) => {
            log(&format!("WARN {} GET {} failed: {}", instn, path, err_str(&e)));
            None
        }
    }
}

/// Fetch the tracked movie/series. `Ok(None)` means a DEFINITIVE HTTP 404 —
/// the item was deleted from the arr on purpose. To this tracker a deleted
/// movie otherwise looks exactly like a failed download, and escalating that
/// wakes the agent to 'helpfully' re-add what was just removed. Transient
/// errors (timeouts, Radarr's 'database is locked' 500s) are `Err` so a
/// hiccup is never mistaken for a deletion.
pub fn fetch_item(instn: &str, kind: &str, iid: &str) -> Result<Option<Value>, ApiError> {
    let path = if kind == "movie" {
        format!("movie/{}", iid)
    } else {
        format!("series/{}", iid)
    };
    match arr_try_get(instn, &path) {
        Ok(v) => Ok(Some(v.unwrap_or(Value::Null))),
        Err(e) if e.is_404() => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn seerr_get(path: &str) -> Option<Value> {
    match http_get_json(
        &format!("{}/api/v1/{}", SEERR_URL, path),
        &[("X-Api-Key", &cfg().seerr_key)],
        30,
    ) {
        Ok(v) => v,
        Err(e) => {
            log(&format!("WARN seerr GET {} failed: {}", path, err_str(&e)));
            None
        }
    }
}

// ── Jellyfin ("is it actually scanned in and watchable?") ───────────────────
// Jellyfin 10.11's AnyProviderIdEquals filter is broken (ignores the filter and
// returns the whole library), so we pull the lists once and match provider ids
// client-side. Cached briefly so a poll doesn't hammer Jellyfin.
pub fn jf_get(path: &str, params: &[(&str, &str)]) -> Option<Value> {
    let qs = if params.is_empty() {
        String::new()
    } else {
        format!("?{}", arr_api::http::form_encode(params))
    };
    match http_get_json(
        &format!("{}{}{}", JELLYFIN_URL, path, qs),
        &[("X-Emby-Token", &cfg().jellyfin_key)],
        30,
    ) {
        Ok(v) => v,
        Err(e) => {
            log(&format!("WARN jellyfin GET {} failed: {}", path, err_str(&e)));
            None
        }
    }
}

static LAST_JF_SCAN: Mutex<f64> = Mutex::new(0.0);

/// Kick a Jellyfin library scan when a download imports. The arrs don't notify
/// Jellyfin on import and its real-time watcher can lag minutes, so we nudge
/// it — a manual scan ingests new files in seconds. Throttled (30s) so
/// back-to-back imports don't spam it.
pub fn trigger_jellyfin_scan() {
    if cfg().jellyfin_key.is_empty() {
        return;
    }
    {
        let mut last = LAST_JF_SCAN.lock().unwrap();
        if now() - *last < 30.0 {
            return;
        }
        *last = now();
    }
    let req = ureq::agent()
        .request("POST", &format!("{}/Library/Refresh", JELLYFIN_URL))
        .set("X-Emby-Token", &cfg().jellyfin_key)
        .timeout(Duration::from_secs(30));
    match req.call() {
        Ok(_) => log("triggered Jellyfin library scan (download imported)"),
        Err(e) => log(&format!("WARN jellyfin scan trigger failed: {}", e)),
    }
}

struct JfCache {
    t: f64,
    movies: Option<HashSet<String>>,
    series: HashMap<String, String>, // provider key -> jellyfin series id
}

fn jf_cache() -> &'static Mutex<JfCache> {
    static C: OnceLock<Mutex<JfCache>> = OnceLock::new();
    C.get_or_init(|| {
        Mutex::new(JfCache {
            t: 0.0,
            movies: None,
            series: HashMap::new(),
        })
    })
}

fn jf_refresh() {
    {
        let c = jf_cache().lock().unwrap();
        if now() - c.t < 45.0 && c.movies.is_some() {
            return;
        }
    }
    let mut movies = HashSet::new();
    let md = jf_get(
        "/Items",
        &[
            ("Recursive", "true"),
            ("IncludeItemTypes", "Movie"),
            ("Fields", "ProviderIds"),
            ("Limit", "100000"),
        ],
    );
    if let Some(md) = &md {
        for it in md.a("Items") {
            let pid = it.at(&["ProviderIds"]);
            for k in ["Tmdb", "Imdb"] {
                let v = truthy_scalar_str(pid.get(k));
                if !v.is_empty() {
                    movies.insert(format!("{}:{}", k.to_lowercase(), v.to_lowercase()));
                }
            }
        }
    }
    let mut series = HashMap::new();
    let sd = jf_get(
        "/Items",
        &[
            ("Recursive", "true"),
            ("IncludeItemTypes", "Series"),
            ("Fields", "ProviderIds"),
            ("Limit", "100000"),
        ],
    );
    if let Some(sd) = &sd {
        for it in sd.a("Items") {
            let pid = it.at(&["ProviderIds"]);
            for k in ["Tvdb", "Tmdb", "Imdb"] {
                let v = truthy_scalar_str(pid.get(k));
                if !v.is_empty() {
                    series.insert(
                        format!("{}:{}", k.to_lowercase(), v.to_lowercase()),
                        it.s("Id").to_string(),
                    );
                }
            }
        }
    }
    // Only adopt the new snapshot if at least the movie pull worked (avoid
    // blowing the cache away to empty on a transient Jellyfin hiccup).
    if md.is_some() {
        let mut c = jf_cache().lock().unwrap();
        c.t = now();
        c.movies = Some(movies);
        c.series = series;
    }
}

pub fn jf_has_movie(tmdb: &str, imdb: &str) -> bool {
    jf_refresh();
    let c = jf_cache().lock().unwrap();
    let have = c.movies.as_ref();
    let chk = |k: &str, v: &str| {
        !v.is_empty()
            && have.map_or(false, |h| h.contains(&format!("{}:{}", k, v.to_lowercase())))
    };
    chk("tmdb", tmdb) || chk("imdb", imdb)
}

/// How many episodes Jellyfin currently has for this series (0 if the series
/// isn't scanned in yet). We compare this against the count captured when the
/// download started — a numbering-agnostic 'new episodes are now in Jellyfin'
/// signal that survives the Sonarr↔Jellyfin absolute-vs-seasonal mismatch
/// (anime episodes show up as e.g. S1E5.. not S1E1, so matching exact numbers
/// is no good).
pub fn jf_series_episode_count(tvdb: &str, tmdb: &str, imdb: &str) -> i64 {
    jf_refresh();
    let jf_id = {
        let c = jf_cache().lock().unwrap();
        let mut found = None;
        for (k, v) in [("tvdb", tvdb), ("tmdb", tmdb), ("imdb", imdb)] {
            if !v.is_empty() {
                if let Some(id) = c.series.get(&format!("{}:{}", k, v.to_lowercase())) {
                    found = Some(id.clone());
                    break;
                }
            }
        }
        found
    };
    let Some(jf_id) = jf_id else {
        return 0; // series not scanned into Jellyfin yet
    };
    let d = jf_get(&format!("/Shows/{}/Episodes", jf_id), &[("Limit", "100000")]);
    match &d {
        Some(d) => {
            let trc = d.i("TotalRecordCount");
            if trc != 0 {
                trc
            } else {
                d.a("Items").len() as i64
            }
        }
        None => 0,
    }
}

// ── queue aggregation ───────────────────────────────────────────────────────

pub struct Group {
    pub size: f64,
    pub left: f64,
    pub importing: bool,
    pub timeleft: String,
    pub title: Option<String>,
    pub kind: &'static str, // "tv" | "movie"
    pub year: Option<i64>,
    pub poster: Option<String>,
    pub tmdb: String, // "" when absent — matches str(g["tmdb"] or "")
    pub imdb: String,
    pub tvdb: String,
    pub episodes: BTreeSet<(i64, i64)>,
    pub dlids: BTreeSet<String>,
    pub ep_state: BTreeMap<String, String>,
    pub pct: i64,
}

/// internal_id -> aggregated download state. None on API error — distinct from
/// an empty queue; the caller skips the instance for this cycle.
pub fn aggregate_queue(instn: &str) -> Option<BTreeMap<i64, Group>> {
    let i = inst(instn)?;
    let is_tv = i.seerr_type == "tv";
    let res = arr_get(instn, &format!("queue?pageSize=500&{}", i.queue_extra))?;
    let mut groups: BTreeMap<i64, Group> = BTreeMap::new();
    for rec in res.a("records") {
        let iid = match rec.get(i.id_field).and_then(|v| v.as_i64()) {
            Some(x) => x,
            None => continue,
        };
        let g = groups.entry(iid).or_insert_with(|| Group {
            size: 0.0,
            left: 0.0,
            importing: false,
            timeleft: String::new(),
            title: None,
            kind: if is_tv { "tv" } else { "movie" },
            year: None,
            poster: None,
            tmdb: String::new(),
            imdb: String::new(),
            tvdb: String::new(),
            episodes: BTreeSet::new(),
            dlids: BTreeSet::new(),
            ep_state: BTreeMap::new(),
            pct: 0,
        });
        if let Some(d) = rec.get("downloadId").and_then(|v| v.as_str()) {
            if !d.is_empty() {
                g.dlids.insert(d.to_string());
            }
        }
        g.size += rec.f("size");
        g.left += rec.f("sizeleft");
        if rec.s("trackedDownloadState").starts_with("import") {
            g.importing = true;
        }
        let tl = rec.s("timeleft");
        if !tl.is_empty() && (g.timeleft.is_empty() || tl > g.timeleft.as_str()) {
            g.timeleft = tl.to_string();
        }
        let parent = if is_tv { rec.get("series") } else { rec.get("movie") };
        if let Some(parent) = parent.filter(|p| !p.is_null()) {
            if g.title.as_deref().unwrap_or("").is_empty() {
                let t = parent.s("title");
                if !t.is_empty() {
                    g.title = Some(t.to_string());
                }
            }
            if g.tmdb.is_empty() {
                g.tmdb = truthy_scalar_str(parent.get("tmdbId"));
            }
            if g.imdb.is_empty() {
                g.imdb = truthy_scalar_str(parent.get("imdbId"));
            }
            if g.year.is_none() {
                g.year = parent.get("year").and_then(|v| v.as_i64()).filter(|y| *y != 0);
            }
            if g.poster.is_none() {
                g.poster = parent.a("images").iter().find_map(|img| {
                    if img.s("coverType") == "poster" && !img.s("remoteUrl").is_empty() {
                        Some(img.s("remoteUrl").to_string())
                    } else {
                        None
                    }
                });
            }
            if is_tv && g.tvdb.is_empty() {
                g.tvdb = truthy_scalar_str(parent.get("tvdbId"));
            }
        }
        if is_tv {
            let ep = rec.at(&["episode"]);
            if ep.has("seasonNumber") && ep.has("episodeNumber") {
                let (s, e) = (ep.i("seasonNumber"), ep.i("episodeNumber"));
                g.episodes.insert((s, e));
                let importing = rec.s("trackedDownloadState").starts_with("import")
                    || rec.f("sizeleft") == 0.0;
                g.ep_state.insert(
                    format!("{}x{}", s, e),
                    (if importing { "importing" } else { "downloading" }).to_string(),
                );
            }
        }
        if g.title.as_deref().unwrap_or("").is_empty() {
            let t = rec.s("title");
            if !t.is_empty() {
                g.title = Some(t.to_string());
            }
        }
    }
    for g in groups.values_mut() {
        let size = if g.size != 0.0 { g.size } else { 1.0 };
        g.pct = py_round((size - g.left) / size * 100.0).clamp(0, 100);
    }
    Some(groups)
}
