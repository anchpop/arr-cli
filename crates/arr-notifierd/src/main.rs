//! arr-notifierd — Rust port of download-notifier.py: proactive Discord DMs
//! with a live progress bar. Polls the Radarr / Sonarr / Sonarr-anime queues,
//! resolves each in-flight download to the person who asked for it (Seerr
//! request *or* a `requester-<id>` tag set by the `arr` CLI), opens a Discord
//! DM, and keeps editing one message as the download progresses. State lives
//! in sqlite so restarts don't double-DM or lose message handles, and we only
//! edit when the rendered text actually changes.

mod arrs;
mod config;
mod discord;
mod embeds;
mod state;
mod util;
mod verify;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::Duration;

use arr_api::JsonExt;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::arrs::{
    aggregate_queue, arr_get, episode_hasfile_map, err_str, fetch_item, jf_has_movie,
    jf_series_episode_count, seerr_get, trigger_jellyfin_scan, Group,
};
use crate::config::{
    cfg, DONE_THRESHOLD, FAIL_STALL_WINDOW, FAIL_WAKE_THRESHOLD, JF_CONFIRM_TIMEOUT,
    JF_REGRESSION_MAX, JF_REGRESSION_WINDOW, MISSING_GRACE, STUCK_SWEEP_EVERY, STUCK_WAKE_AFTER,
    VERIFY_RECHECK_EVERY, VERIFY_RECHECK_WINDOW,
};
use crate::discord::{edit_message, send_message, send_via_thread};
use crate::embeds::{build_embed, render_chart, watchable, SeasonLine};
use crate::util::{log, now, py_round, scalar_str, split_key};
use crate::verify::{bazarr_kick, dv5_warning, verify_language, wake_hermes, wake_hermes_failed};

// ── people map (jellyfin username -> discord id) ────────────────────────────
fn load_people() -> (HashMap<String, String>, HashSet<String>) {
    let mut by_jellyfin = HashMap::new();
    let mut valid_discord = HashSet::new();
    let mut names = HashMap::new();
    let pj = &cfg().people_json;
    if !pj.is_empty() && std::path::Path::new(pj).exists() {
        let text = std::fs::read_to_string(pj).unwrap_or_else(|e| {
            log(&format!("FATAL: cannot read {}: {}", pj, e));
            std::process::exit(1)
        });
        let people: Value = serde_json::from_str(&text).unwrap_or_else(|e| {
            log(&format!("FATAL: bad json in {}: {}", pj, e));
            std::process::exit(1)
        });
        for p in people.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
            let did = scalar_str(p.get("discord")).trim().to_string();
            if !did.is_empty() {
                valid_discord.insert(did.clone());
                let name = p.s("name");
                if !name.is_empty() {
                    names.insert(did.clone(), name.to_string());
                }
                let jf = scalar_str(p.get("jellyfin"));
                if !jf.is_empty() {
                    by_jellyfin.insert(jf.to_lowercase(), did.clone());
                }
            }
        }
    }
    discord::set_people_names(names);
    (by_jellyfin, valid_discord)
}

// ── core ────────────────────────────────────────────────────────────────────

/// (instance, internal_id) -> set(discord_id) for processing Seerr requests.
fn seerr_requester_map(
    people_by_jf: &HashMap<String, String>,
) -> HashMap<(&'static str, i64), BTreeSet<String>> {
    let mut out: HashMap<(&'static str, i64), BTreeSet<String>> = HashMap::new();
    let res = seerr_get("request?take=200&filter=processing");
    let empty = vec![];
    let results = res
        .as_ref()
        .and_then(|r| r.get("results"))
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
    for r in results {
        let media = r.at(&["media"]);
        if !media.has("externalServiceId") {
            continue;
        }
        let ext = media.i("externalServiceId");
        let rtype = r.s("type");
        let sid = media.i("serviceId");
        let Some(instn) = cfg()
            .instances
            .iter()
            .find(|i| i.seerr_type == rtype && i.seerr_service_id == sid)
        else {
            continue;
        };
        let jf = r
            .at(&["requestedBy", "jellyfinUsername"])
            .as_str()
            .unwrap_or("")
            .to_lowercase();
        if let Some(did) = people_by_jf.get(&jf) {
            out.entry((instn.name, ext)).or_default().insert(did.clone());
        }
    }
    out
}

/// internal_id -> set(discord_id) parsed from `requester-<id>` arr tags.
fn tag_requester_map(instn: &str, valid_discord: &HashSet<String>) -> HashMap<i64, BTreeSet<String>> {
    let Some(tags) = arr_get(instn, "tag") else {
        return HashMap::new();
    };
    // tag id -> discord id (only requester-<id> tags we recognise)
    let mut id_to_discord: HashMap<i64, String> = HashMap::new();
    for t in tags.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        let label = t.s("label").trim().to_string();
        if let Some(rest) = label.strip_prefix("requester-") {
            let did = rest.trim();
            if valid_discord.contains(did) {
                id_to_discord.insert(t.i("id"), did.to_string());
            }
        }
    }
    if id_to_discord.is_empty() {
        return HashMap::new();
    }
    let resource = if instn == "radarr" { "movie" } else { "series" };
    let mut out: HashMap<i64, BTreeSet<String>> = HashMap::new();
    let items = arr_get(instn, resource).unwrap_or(Value::Null);
    for item in items.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        let dids: BTreeSet<String> = item
            .a("tags")
            .iter()
            .filter_map(|tid| tid.as_i64())
            .filter_map(|tid| id_to_discord.get(&tid).cloned())
            .collect();
        if !dids.is_empty() {
            out.insert(item.i("id"), dids);
        }
    }
    out
}

/// Fold this poll into the per-download-attempt history. A download whose
/// queue downloadId set is disjoint from the one we were tracking is a NEW
/// release Radarr/Sonarr grabbed after the previous one failed — so the
/// previous attempt is marked failed and a new bar begins.
fn evolve_attempts(
    mut attempts: Vec<Value>,
    stored_dlids: &BTreeSet<String>,
    g: &Group,
) -> (Vec<Value>, BTreeSet<String>) {
    let new_dlids = g.dlids.clone();
    let pct = g.pct;
    if attempts.is_empty() {
        return (vec![json!({"pct": pct, "status": "active"})], new_dlids);
    }
    let replaced =
        !new_dlids.is_empty() && !stored_dlids.is_empty() && new_dlids.is_disjoint(stored_dlids);
    let last_status = attempts.last().map(|a| a.s("status").to_string()).unwrap_or_default();
    if replaced || last_status != "active" {
        if last_status == "active" {
            // superseded before importing ⇒ it failed
            attempts.last_mut().unwrap()["status"] = json!("failed");
        }
        attempts.push(json!({"pct": pct, "status": "active"}));
    } else {
        let last = attempts.last_mut().unwrap();
        let newpct = last.i("pct").max(pct);
        last["pct"] = json!(newpct);
    }
    let dlids = if new_dlids.is_empty() {
        stored_dlids.clone()
    } else {
        new_dlids
    };
    (attempts, dlids)
}

/// Per-episode status for the season chart. An episode in the queue that
/// Sonarr already has a file for is a quality *upgrade* — the requester can
/// watch it right now — so it renders 🔁 and counts as done instead of
/// masquerading as a fresh download (a 146GB remux upgrade once made 24
/// watchable episodes show ⏳ for a day). Fresh in-queue episodes keep their
/// queue state; departed ones resolve via hasFile (present ⇒ done ✅,
/// absent ⇒ failed ❌). Accumulates so finished episodes stay.
fn compute_ep_status(
    instn: &str,
    iid: &str,
    g: Option<&Group>,
    stored: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut ep_status = stored.clone();
    let empty = BTreeMap::new();
    let in_queue = g.map(|g| &g.ep_state).unwrap_or(&empty);
    let hasfile = episode_hasfile_map(instn, iid);
    for (k, st) in in_queue {
        let newst = match &hasfile {
            Some(m) if m.get(k).copied().unwrap_or(false) => "upgrading".to_string(),
            Some(_) => st.clone(),
            // episode fetch failed: keep an existing 'upgrading' rather than
            // regressing a watchable episode to ⏳ for one API blip
            None if ep_status.get(k).is_some_and(|s| s.as_str() == "upgrading") => continue,
            None => st.clone(),
        };
        ep_status.insert(k.clone(), newst);
    }
    if let Some(m) = &hasfile {
        // only resolve departures when the episode list actually loaded — a
        // failed fetch must not mark everything ❌
        let departed: Vec<String> = ep_status
            .keys()
            .filter(|k| !in_queue.contains_key(*k))
            .cloned()
            .collect();
        for k in departed {
            let st = if m.get(&k).copied().unwrap_or(false) {
                "done"
            } else {
                "failed"
            };
            ep_status.insert(k, st.to_string());
        }
    }
    ep_status
}

/// Seasons that should render as a byte bar: every known episode of the
/// season is in flight from exactly ONE download (a season pack). Seasons
/// with mixed states or per-episode downloads keep the per-episode glyphs.
fn season_bar_lines(
    g: &Group,
    ep_status: &BTreeMap<String, String>,
) -> BTreeMap<i64, SeasonLine> {
    let mut out = BTreeMap::new();
    for (s, agg) in &g.seasons {
        if agg.dlids.len() != 1 || agg.eps < 2 {
            continue;
        }
        let prefix = format!("{}x", s);
        let season_eps: Vec<&String> = ep_status
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .collect();
        let uniform_inflight = season_eps.iter().all(|k| {
            matches!(
                ep_status.get(*k).map(|s| s.as_str()),
                Some("downloading") | Some("importing")
            )
        });
        if !uniform_inflight || season_eps.len() as i64 != agg.eps {
            continue;
        }
        let size = if agg.size != 0.0 { agg.size } else { 1.0 };
        let pct = py_round((size - agg.left) / size * 100.0).clamp(0, 100);
        out.insert(
            *s,
            SeasonLine {
                pct,
                importing: agg.importing,
            },
        );
    }
    out
}

fn parse_attempts(s: Option<&str>) -> Vec<Value> {
    s.and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default()
}

fn parse_ep_status(s: Option<&str>) -> BTreeMap<String, String> {
    s.and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default()
}

fn handle(con: &Connection, key: &str, discord_id: &str, title: &str, g: &Group) {
    let timeleft = g.timeleft.as_str();
    let importing = g.importing;
    type HandleRow = (
        Option<String>, // channel_id
        Option<String>, // message_id
        Option<String>, // last_content
        Option<String>, // phase
        Option<String>, // attempts
        Option<String>, // dlids
        Option<String>, // ep_status
        Option<i64>,    // multi
    );
    let row: Option<HandleRow> = con
        .query_row(
            "SELECT channel_id, message_id, last_content, phase, attempts, dlids, ep_status, multi \
             FROM notified WHERE key=?",
            [key],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            },
        )
        .optional()
        .unwrap_or(None);
    if let Some(r) = &row {
        if r.3.as_deref() == Some("blocked") {
            return; // can't DM this person for this download — don't retry every poll
        }
    }
    let kind = g.kind;
    let episodes_json = serde_json::to_string(&g.episodes).unwrap_or_default();
    let (mut cid, mut mid) = match &row {
        Some(r) => (r.0.clone(), r.1.clone()),
        None => (None, None),
    };
    let (instn, iid_str) = split_key(key);

    // Upgrade guard: a queue entry for something already fully on disk is a
    // quality upgrade (a profile chasing e.g. a BD remux over an imported
    // release). The requester can watch it right now, so a finished row is
    // never regressed to 'downloading' and a fresh row never DMs — the
    // upgrade runs to completion invisibly.
    let phase = row.as_ref().and_then(|r| r.3.clone()).unwrap_or_default();
    if row.is_none() || phase == "done" || phase == "importing" {
        let already_watchable = if kind == "movie" {
            matches!(fetch_item(&instn, "movie", &iid_str), Ok(Some(m)) if m.b("hasFile"))
        } else {
            !g.episodes.is_empty()
                && episode_hasfile_map(&instn, &iid_str).is_some_and(|m| {
                    g.episodes
                        .iter()
                        .all(|(s, e)| m.get(&format!("{}x{}", s, e)).copied().unwrap_or(false))
                })
        };
        if already_watchable {
            if row.is_none() {
                let _ = con.execute(
                    "INSERT INTO notified(key,discord_id,phase,kind,title,tmdb,imdb,tvdb,poster,year,updated_at) \
                     VALUES(?,?,'done',?,?,?,?,?,?,?,?) ON CONFLICT(key) DO NOTHING",
                    params![key, discord_id, kind, title, g.tmdb, g.imdb, g.tvdb, g.poster, g.year, now()],
                );
                log(&format!(
                    "upgrade-only download for {:?} — already watchable, not DMing {}",
                    title, discord_id
                ));
            }
            return;
        }
    }

    // Multi-file (season grabbed as separate episode files) → per-episode chart;
    // single download (movie / season pack) → byte bar + attempts. Sticky once set.
    let stored_dlids: BTreeSet<String> = row
        .as_ref()
        .and_then(|r| r.5.as_deref())
        .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
        .map(|v| v.into_iter().collect())
        .unwrap_or_default();
    let row_multi = row.as_ref().map(|r| r.7.unwrap_or(0) != 0).unwrap_or(false);
    let multi = kind == "tv" && (g.dlids.len() > 1 || row_multi);

    let payload;
    let pct;
    let attempts_out: Vec<Value>;
    let dlids_out: BTreeSet<String>;
    let ep_json: Option<String>;
    let mut total_eps = 0i64;
    if multi {
        let stored_ep = parse_ep_status(row.as_ref().and_then(|r| r.6.as_deref()));
        let ep_status = compute_ep_status(&instn, &iid_str, Some(g), &stored_ep);
        let season_bars = season_bar_lines(g, &ep_status);
        let chart = render_chart(&ep_status, Some(&season_bars));
        // Every episode already watchable (done or upgrading) ⇒ nothing the
        // requester is actually waiting on: go straight to the Jellyfin
        // confirm instead of holding the ✅ hostage to an upgrade's ETA.
        if chart.total > 0 && ep_status.values().all(|s| watchable(s)) && mid.is_some() {
            let payload = build_embed(
                "adding",
                title,
                g.year,
                g.poster.as_deref(),
                &[],
                "",
                false,
                Some(&chart),
                None,
            );
            if !cfg().dry_run {
                edit_message(cid.as_deref().unwrap_or(""), mid.as_deref().unwrap_or(""), &payload);
            }
            let _ = con.execute(
                "UPDATE notified SET phase='importing', last_content=?, ep_status=?, \
                 import_at=?, updated_at=? WHERE key=?",
                params![
                    serde_json::to_string(&payload).unwrap_or_default(),
                    serde_json::to_string(&ep_status).unwrap_or_default(),
                    now(),
                    now(),
                    key
                ],
            );
            log(&format!(
                "DM->{} all {} episodes watchable (rest is upgrades), awaiting Jellyfin: {}",
                discord_id, chart.total, title
            ));
            return;
        }
        total_eps = chart.total;
        pct = if chart.total != 0 {
            py_round(chart.done as f64 / chart.total as f64 * 100.0)
        } else {
            0
        };
        payload = build_embed(
            "downloading",
            title,
            g.year,
            g.poster.as_deref(),
            &[],
            timeleft,
            false,
            Some(&chart),
            None,
        );
        attempts_out = vec![];
        dlids_out = g.dlids.union(&stored_dlids).cloned().collect();
        ep_json = Some(serde_json::to_string(&ep_status).unwrap_or_default());
    } else {
        let attempts = parse_attempts(row.as_ref().and_then(|r| r.4.as_deref()));
        let (a, d) = evolve_attempts(attempts, &stored_dlids, g);
        pct = a.last().map(|x| x.i("pct")).unwrap_or(0);
        payload = build_embed(
            "downloading",
            title,
            g.year,
            g.poster.as_deref(),
            &a,
            timeleft,
            importing,
            None,
            None,
        );
        attempts_out = a;
        dlids_out = d;
        ep_json = None;
    }
    let content = serde_json::to_string(&payload).unwrap_or_default(); // change-detection key
    let aj = serde_json::to_string(&attempts_out).unwrap_or_default();
    let dj = serde_json::to_string(&dlids_out).unwrap_or_default(); // BTreeSet ⇒ sorted
    let mflag: i64 = if multi { 1 } else { 0 };

    // The (item, recipient) → message_id mapping is durable: once we have a
    // message for this key we ALWAYS edit that one, never post a second. So a
    // re-grab, a flicker that briefly marked it done, or a later season all reuse
    // the same DM instead of spawning duplicates. We only POST when no message
    // exists yet for this key.
    if mid.is_none() {
        if cfg().dry_run {
            log(&format!("DRY would DM {}: {:?}", discord_id, content));
            cid = Some("dry".to_string());
            mid = Some("dry".to_string());
        } else {
            // Users already known to be DM-blocked have a private thread → go
            // straight there (skip the futile DM attempt + its rate-limit hit).
            let known_blocked = con
                .query_row(
                    "SELECT 1 FROM user_threads WHERE discord_id=?",
                    [discord_id],
                    |_| Ok(()),
                )
                .optional()
                .unwrap_or(None)
                .is_some();
            if known_blocked {
                let (c, m) = send_via_thread(con, discord_id, &payload);
                cid = c;
                mid = m;
            } else {
                let (c, m) = send_message(discord_id, &payload); // try a real DM first
                cid = c;
                mid = m;
                if mid.is_none() {
                    // DM blocked (Discord 50278: "Allow DMs from server members"
                    // is off — blocks both directions). Fall back to a private
                    // thread, which isn't subject to DM privacy.
                    log(&format!(
                        "cannot DM {} ({:?}) — falling back to private thread",
                        discord_id, title
                    ));
                    let (c, m) = send_via_thread(con, discord_id, &payload);
                    cid = c;
                    mid = m;
                }
            }
            if mid.is_none() {
                // Both DM and thread failed (e.g. missing thread perms). Record
                // 'blocked' so we don't retry every poll.
                log(&format!(
                    "cannot reach {} ({:?}) via DM or thread — marking blocked",
                    discord_id, title
                ));
                let _ = con.execute(
                    "INSERT INTO notified(key,discord_id,phase,title,updated_at) \
                     VALUES(?,?, 'blocked', ?, ?) \
                     ON CONFLICT(key) DO UPDATE SET phase='blocked', updated_at=excluded.updated_at",
                    params![key, discord_id, title, now()],
                );
                return;
            }
        }
        // Baseline Jellyfin episode count for TV, captured now (pre-import) so we
        // can later tell when the NEW episodes have been scanned in.
        let baseline: i64 = if kind == "tv" {
            jf_series_episode_count(&g.tvdb, &g.tmdb, &g.imdb)
        } else {
            0
        };
        let _ = con.execute(
            "INSERT INTO notified(key,discord_id,channel_id,message_id,last_content,last_pct,\
             phase,updated_at,kind,title,tmdb,imdb,tvdb,episodes,jf_baseline,import_at,missing,attempts,dlids,poster,year,ep_status,multi) \
             VALUES(?,?,?,?,?,?, 'downloading', ?,?,?,?,?,?,?,?, NULL, 0,?,?,?,?,?,?) \
             ON CONFLICT(key) DO UPDATE SET channel_id=excluded.channel_id, \
             message_id=excluded.message_id, last_content=excluded.last_content, \
             last_pct=excluded.last_pct, phase='downloading', updated_at=excluded.updated_at, \
             kind=excluded.kind, title=excluded.title, tmdb=excluded.tmdb, imdb=excluded.imdb, \
             tvdb=excluded.tvdb, episodes=excluded.episodes, jf_baseline=excluded.jf_baseline, \
             import_at=NULL, missing=0, attempts=excluded.attempts, dlids=excluded.dlids, \
             poster=excluded.poster, year=excluded.year, ep_status=excluded.ep_status, multi=excluded.multi",
            params![
                key,
                discord_id,
                cid,
                mid,
                content,
                pct,
                now(),
                kind,
                title,
                g.tmdb,
                g.imdb,
                g.tvdb,
                episodes_json,
                baseline,
                aj,
                dj,
                g.poster,
                g.year,
                ep_json,
                mflag
            ],
        );
        log(&format!(
            "DM->{} start: {} {}%{}",
            discord_id,
            title,
            pct,
            if multi {
                format!(" ({} eps)", total_eps)
            } else {
                format!(" (jf baseline {})", baseline)
            }
        ));
        return;
    }

    // Resurrected from a terminal state by NEW content (an upgrade would have
    // returned in the guard above): re-baseline the Jellyfin episode count so
    // 'ready' waits for the new episodes, not the old total.
    if kind == "tv" && matches!(phase.as_str(), "done" | "failed") {
        let b = jf_series_episode_count(&g.tvdb, &g.tmdb, &g.imdb);
        let _ = con.execute(
            "UPDATE notified SET jf_baseline=? WHERE key=?",
            params![b, key],
        );
    }

    // Reuse the existing message. It's back in the queue, so it's (re)downloading —
    // flip phase back to 'downloading' (recovers from a flicker/failed that hit a
    // terminal state), reset the missing counter, refresh identity + attempts.
    let _ = con.execute(
        "UPDATE notified SET phase='downloading', missing=0, \
         kind=?, title=?, tmdb=?, imdb=?, tvdb=?, episodes=?, attempts=?, dlids=?, \
         poster=COALESCE(?,poster), year=COALESCE(?,year), ep_status=?, multi=? WHERE key=?",
        params![
            kind,
            title,
            g.tmdb,
            g.imdb,
            g.tvdb,
            episodes_json,
            aj,
            dj,
            g.poster,
            g.year,
            ep_json,
            mflag,
            key
        ],
    );
    let last_content = row.as_ref().and_then(|r| r.2.clone());
    if last_content.as_deref() != Some(content.as_str()) {
        // only edit when the rendered embed changes
        if cfg().dry_run {
            log(&format!("DRY would edit {}: {:?}", discord_id, content));
        } else {
            edit_message(cid.as_deref().unwrap_or(""), mid.as_deref().unwrap_or(""), &payload);
        }
        let _ = con.execute(
            "UPDATE notified SET last_content=?, last_pct=?, updated_at=? WHERE key=?",
            params![content, pct, now(), key],
        );
        log(&format!("DM->{} update: {} {}%", discord_id, title, pct));
    }
}

/// A tracked download is no longer in any queue. For a movie we can tell
/// import from failure via Radarr's hasFile: present ⇒ wait for Jellyfin;
/// absent ⇒ the release failed (show the failed bar; a re-grab will resurrect
/// it as a new attempt). TV has no cheap per-grab hasFile, so a high-progress
/// leave is treated as imported and the Jellyfin confirm/timeout is the
/// backstop.
fn on_left_queue(con: &Connection, key: &str) {
    type LeftRow = (
        Option<String>, // discord_id
        Option<String>, // channel_id
        Option<String>, // message_id
        Option<i64>,    // last_pct
        Option<String>, // title
        Option<String>, // kind
        Option<String>, // attempts
        Option<String>, // poster
        Option<i64>,    // year
        Option<String>, // ep_status
        Option<i64>,    // multi
        Option<i64>,    // fail_woken
    );
    let row: Option<LeftRow> = con
        .query_row(
            "SELECT discord_id, channel_id, message_id, last_pct, title, kind, attempts, poster, \
             year, ep_status, multi, fail_woken FROM notified WHERE key=?",
            [key],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                    r.get(10)?,
                    r.get(11)?,
                ))
            },
        )
        .optional()
        .unwrap_or(None);
    let Some((
        discord_id,
        cid,
        mid,
        last_pct,
        title_raw,
        kind_raw,
        attempts_json,
        poster,
        year,
        ep_status_json,
        multi,
        fail_woken,
    )) = row
    else {
        return;
    };
    let discord_id = discord_id.unwrap_or_default();
    let title = match &title_raw {
        Some(t) if !t.is_empty() => t.clone(),
        _ => "Your request".to_string(),
    };
    let kind = kind_raw.unwrap_or_default();
    let mut attempts = parse_attempts(attempts_json.as_deref());
    let (instn, iid) = split_key(key);
    let multi = multi.unwrap_or(0) != 0;
    let mut fail_woken = fail_woken.unwrap_or(0);
    let cid = cid.unwrap_or_default();
    let mid = mid.unwrap_or_default();

    if multi {
        // season chart: resolve every episode (done vs failed) via hasFile
        let stored = parse_ep_status(ep_status_json.as_deref());
        let ep_status = compute_ep_status(&instn, &iid, None, &stored);
        let chart = render_chart(&ep_status, None);
        if chart.done == 0 {
            // nothing actually imported — stay quiet
            log(&format!("season gone, 0/{} imported, no ping: {}", chart.total, title));
            let _ = con.execute(
                "UPDATE notified SET phase='done', updated_at=? WHERE key=?",
                params![now(), key],
            );
            return;
        }
        let payload = build_embed(
            "adding",
            &title,
            year,
            poster.as_deref(),
            &[],
            "",
            false,
            Some(&chart),
            None,
        );
        if !cfg().dry_run {
            edit_message(&cid, &mid, &payload);
        }
        let _ = con.execute(
            "UPDATE notified SET phase='importing', last_content=?, ep_status=?, import_at=?, \
             updated_at=? WHERE key=?",
            params![
                serde_json::to_string(&payload).unwrap_or_default(),
                serde_json::to_string(&ep_status).unwrap_or_default(),
                now(),
                now(),
                key
            ],
        );
        trigger_jellyfin_scan(); // don't wait on Jellyfin's slow real-time watcher
        log(&format!(
            "DM->{} season imported ({}/{}), awaiting Jellyfin: {}",
            discord_id, chart.done, chart.total, title
        ));
        return;
    }

    let imported: Option<bool> = if kind == "movie" {
        match fetch_item(&instn, "movie", &iid) {
            // Definitive 404: the movie was deleted from the arr on purpose —
            // drop the tracking row, never a failure ping (2026-07-26 fix).
            Ok(None) => {
                let _ = con.execute("DELETE FROM notified WHERE key=?", [key]);
                log(&format!(
                    "{}:{} deleted from arr — dropped tracking, no failure ping: {}",
                    instn, iid, title
                ));
                return;
            }
            Ok(Some(m)) => Some(m.b("hasFile")),
            Err(e) => {
                // transient — same net effect as the Python (WARN + no file seen)
                log(&format!("WARN {} GET movie/{} failed: {}", instn, iid, err_str(&e)));
                Some(false)
            }
        }
    } else {
        None
    };

    if imported == Some(false) {
        // movie left the queue without a file ⇒ it failed
        if let Some(last) = attempts.last_mut() {
            if last.s("status") == "active" {
                last["status"] = json!("failed");
            }
        }
        let payload = build_embed(
            "failed",
            &title,
            year,
            poster.as_deref(),
            &attempts,
            "",
            false,
            None,
            None,
        );
        if !cfg().dry_run {
            edit_message(&cid, &mid, &payload);
        }
        // repeated failures → wake the agent to find an alternate source
        let fails = attempts.iter().filter(|a| a.s("status") == "failed").count() as i64;
        if fails >= FAIL_WAKE_THRESHOLD && fail_woken == 0 {
            fail_woken = if wake_hermes_failed(&instn, &iid, &kind, &title, &attempts) {
                1
            } else {
                0
            };
        }
        let _ = con.execute(
            "UPDATE notified SET phase='failed', last_content=?, attempts=?, updated_at=?, \
             fail_woken=? WHERE key=?",
            params![
                serde_json::to_string(&payload).unwrap_or_default(),
                serde_json::to_string(&attempts).unwrap_or_default(),
                now(),
                fail_woken,
                key
            ],
        );
        log(&format!("DM->{} FAILED (no file): {}", discord_id, title));
        return;
    }

    if last_pct.unwrap_or(0) < DONE_THRESHOLD && !imported.unwrap_or(false) {
        log(&format!(
            "download gone early ({}%), no ping: {}",
            last_pct.unwrap_or(0),
            title
        ));
        let _ = con.execute(
            "UPDATE notified SET phase='done', updated_at=? WHERE key=?",
            params![now(), key],
        );
        return;
    }

    let payload = build_embed(
        "adding",
        &title,
        year,
        poster.as_deref(),
        &attempts,
        "",
        false,
        None,
        None,
    );
    if cfg().dry_run {
        log(&format!("DRY would set importing {}: {}", discord_id, title));
    } else {
        edit_message(&cid, &mid, &payload);
    }
    let _ = con.execute(
        "UPDATE notified SET phase='importing', last_content=?, import_at=?, updated_at=? WHERE key=?",
        params![serde_json::to_string(&payload).unwrap_or_default(), now(), now(), key],
    );
    trigger_jellyfin_scan(); // don't wait on Jellyfin's slow real-time watcher
    log(&format!("DM->{} imported, awaiting Jellyfin: {}", discord_id, title));
}

/// Sweep items waiting on Jellyfin; send the 'ready' ping once Jellyfin has
/// actually scanned them in (or after a timeout, so we never stay silent).
fn confirm_imported(con: &Connection) {
    struct R {
        key: String,
        discord_id: String,
        cid: String,
        mid: String,
        kind: String,
        title: Option<String>,
        tmdb: String,
        imdb: String,
        tvdb: String,
        jf_baseline: i64,
        import_at: Option<f64>,
        attempts: Option<String>,
        poster: Option<String>,
        year: Option<i64>,
        ep_status: Option<String>,
        multi: bool,
    }
    let rows: Vec<R> = {
        let mut stmt = match con.prepare(
            "SELECT key,discord_id,channel_id,message_id,kind,title,tmdb,imdb,tvdb,jf_baseline,\
             import_at,attempts,poster,year,ep_status,multi FROM notified WHERE phase='importing'",
        ) {
            Ok(s) => s,
            Err(_) => return,
        };
        let mapped = stmt.query_map([], |r| {
            Ok(R {
                key: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                discord_id: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                cid: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                mid: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                kind: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                title: r.get(5)?,
                tmdb: r.get::<_, Option<String>>(6)?.unwrap_or_default(),
                imdb: r.get::<_, Option<String>>(7)?.unwrap_or_default(),
                tvdb: r.get::<_, Option<String>>(8)?.unwrap_or_default(),
                jf_baseline: r.get::<_, Option<i64>>(9)?.unwrap_or(0),
                import_at: r.get(10)?,
                attempts: r.get(11)?,
                poster: r.get(12)?,
                year: r.get(13)?,
                ep_status: r.get(14)?,
                multi: r.get::<_, Option<i64>>(15)?.unwrap_or(0) != 0,
            })
        });
        match mapped {
            Ok(it) => it.filter_map(|r| r.ok()).collect(),
            Err(_) => return,
        }
    };
    for r in rows {
        let present = if r.kind == "movie" {
            jf_has_movie(&r.tmdb, &r.imdb)
        } else {
            jf_series_episode_count(&r.tvdb, &r.tmdb, &r.imdb) > r.jf_baseline
        };
        let timed_out = r
            .import_at
            .map(|t| now() - t > JF_CONFIRM_TIMEOUT)
            .unwrap_or(false);
        if !present && !timed_out {
            continue;
        }
        let (instn, iid) = split_key(&r.key);
        let res = verify_language(&instn, &iid, &r.kind, r.import_at);
        let verify = res.as_ref().map(|v| v.line.clone());
        let title = match &r.title {
            Some(t) if !t.is_empty() => t.clone(),
            _ => "Your request".to_string(),
        };
        let mut payload = if r.multi {
            let stored = parse_ep_status(r.ep_status.as_deref());
            let ep_status = compute_ep_status(&instn, &iid, None, &stored);
            build_embed(
                "ready",
                &title,
                r.year,
                r.poster.as_deref(),
                &[],
                "",
                false,
                Some(&render_chart(&ep_status, None)),
                verify.as_deref(),
            )
        } else {
            build_embed(
                "ready",
                &title,
                r.year,
                r.poster.as_deref(),
                &parse_attempts(r.attempts.as_deref()),
                "",
                false,
                None,
                verify.as_deref(),
            )
        };
        if let Some(dv5) = dv5_warning(&instn, &iid, &r.kind, r.import_at) {
            let d = payload["embeds"][0]["description"]
                .as_str()
                .unwrap_or("")
                .to_string();
            payload["embeds"][0]["description"] = json!(format!("{}\n{}", d, dv5));
        }
        if cfg().dry_run {
            log(&format!("DRY would mark ready {}: {}", r.discord_id, title));
        } else {
            edit_message(&r.cid, &r.mid, &payload);
        }
        // verification failed → self-heal loop: Bazarr can fetch main-instance
        // subs itself; anything else (dubs, anime subs) wakes Hermes via the
        // gateway webhook right away. The recheck sweep keeps re-probing and
        // live-updates the 🔎 line, and escalates to Hermes if Bazarr doesn't
        // deliver within BAZARR_GRACE.
        let mut spooled: i64 = 1;
        if let Some(res) = &res {
            if !res.ok {
                let bazarr_fixable =
                    !res.gaps_subs.is_empty() && res.gaps_audio.is_empty() && instn != "sonarr-anime";
                if bazarr_fixable {
                    bazarr_kick(&instn, &iid);
                    spooled = 0; // give Bazarr a grace window before waking Hermes
                } else {
                    let t = r.title.clone().unwrap_or_default();
                    spooled = if wake_hermes(&instn, &iid, &r.kind, &t, res) { 1 } else { 0 };
                }
            }
        }
        let verify_ok: Option<i64> = res.as_ref().map(|v| if v.ok { 1 } else { 0 });
        let _ = con.execute(
            "UPDATE notified SET phase='done', last_content=?, updated_at=?, \
             verify_ok=?, verify_line=?, verify_deadline=?, verify_last=?, spooled=? WHERE key=?",
            params![
                serde_json::to_string(&payload).unwrap_or_default(),
                now(),
                verify_ok,
                verify,
                now() + VERIFY_RECHECK_WINDOW,
                now(),
                spooled,
                r.key
            ],
        );
        log(&format!(
            "DM->{} READY ({}): {}{}",
            r.discord_id,
            if present { "jellyfin" } else { "timeout" },
            title,
            verify
                .as_ref()
                .map(|v| format!(" [verify: {}]", v))
                .unwrap_or_default()
        ));
    }
}

/// A "ready" confirmation can be un-done: Jellyfin's follow-up metadata refresh
/// may delete the very episodes whose arrival we confirmed (new anime series →
/// episodes land under a virtual "Season Unknown" while AniDB data is cold →
/// the refresh removes that season and cascades the episodes away). Re-check
/// recently-readied items; on regression, trigger a rescan — with warm metadata
/// caches the episodes come back mapped correctly — and flip the item back to
/// 'importing' so confirm_imported() re-verifies before "ready" stands again.
fn recheck_jf_presence(con: &Connection) {
    let cutoff = now() - JF_REGRESSION_WINDOW;
    let rows: Vec<(String, String, i64, String, String, String, i64)> = {
        let mut stmt = match con.prepare(
            "SELECT key,kind,jf_baseline,tmdb,imdb,tvdb,COALESCE(jf_regressions,0) \
             FROM notified WHERE phase='done' AND updated_at>? AND COALESCE(jf_regressions,0)<?",
        ) {
            Ok(s) => s,
            Err(_) => return,
        };
        let mapped = stmt.query_map(params![cutoff, JF_REGRESSION_MAX], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                r.get::<_, Option<i64>>(6)?.unwrap_or(0),
            ))
        });
        match mapped {
            Ok(it) => it.filter_map(|r| r.ok()).collect(),
            Err(_) => return,
        }
    };
    for (key, kind, jf_baseline, tmdb, imdb, tvdb, regressions) in rows {
        let present = if kind == "movie" {
            jf_has_movie(&tmdb, &imdb)
        } else {
            jf_series_episode_count(&tvdb, &tmdb, &imdb) > jf_baseline
        };
        if present {
            continue;
        }
        log(&format!(
            "jellyfin presence REGRESSED after ready (attempt {}/{}), rescanning + re-arming: {}",
            regressions + 1,
            JF_REGRESSION_MAX,
            key
        ));
        trigger_jellyfin_scan();
        let _ = con.execute(
            "UPDATE notified SET phase='importing', import_at=?, \
             jf_regressions=COALESCE(jf_regressions,0)+1 WHERE key=?",
            params![now(), key],
        );
    }
}

/// Re-probe items whose language verification failed; live-edit the 🔎 line in
/// the ✅ embed when subs/audio arrive, and wake Hermes if a Bazarr-fixable gap
/// outlives its grace window.
fn recheck_verifications(con: &Connection) {
    let now_t = now();
    type VRow = (
        String,         // key
        Option<String>, // discord_id (unused, parity)
        Option<String>, // channel_id
        Option<String>, // message_id
        Option<String>, // kind
        Option<String>, // title
        Option<f64>,    // import_at
        Option<String>, // attempts
        Option<String>, // poster
        Option<i64>,    // year
        Option<String>, // ep_status
        Option<i64>,    // multi
        Option<String>, // verify_line
        Option<f64>,    // verify_last (unused, parity)
        Option<f64>,    // verify_deadline (unused, parity)
        Option<i64>,    // spooled
    );
    let rows: Vec<VRow> = {
        let mut stmt = match con.prepare(
            "SELECT key,discord_id,channel_id,message_id,kind,title,import_at,attempts, \
             poster,year,ep_status,multi,verify_line,verify_last,verify_deadline,spooled \
             FROM notified WHERE phase='done' AND verify_ok=0 AND verify_deadline>? \
             AND (verify_last IS NULL OR verify_last<?)",
        ) {
            Ok(s) => s,
            Err(_) => return,
        };
        let mapped = stmt.query_map(params![now_t, now_t - VERIFY_RECHECK_EVERY], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
                r.get(11)?,
                r.get(12)?,
                r.get(13)?,
                r.get(14)?,
                r.get(15)?,
            ))
        });
        match mapped {
            Ok(it) => it.filter_map(|r| r.ok()).collect(),
            Err(_) => return,
        }
    };
    for (
        key,
        _discord_id,
        cid,
        mid,
        kind,
        title_raw,
        import_at,
        attempts_json,
        poster,
        year,
        ep_status_json,
        multi,
        old_line,
        _verify_last,
        _deadline,
        spooled,
    ) in rows
    {
        let (instn, iid) = split_key(&key);
        let kind = kind.unwrap_or_default();
        let Some(res) = verify_language(&instn, &iid, &kind, import_at) else {
            // tags removed → stop tracking
            let _ = con.execute(
                "UPDATE notified SET verify_ok=1, verify_last=? WHERE key=?",
                params![now_t, key],
            );
            continue;
        };
        if old_line.as_deref() != Some(res.line.as_str()) {
            // something changed → update the embed
            let title = match &title_raw {
                Some(t) if !t.is_empty() => t.clone(),
                _ => "Your request".to_string(),
            };
            let payload = if multi.unwrap_or(0) != 0 {
                let stored = parse_ep_status(ep_status_json.as_deref());
                let ep_status = compute_ep_status(&instn, &iid, None, &stored);
                build_embed(
                    "ready",
                    &title,
                    year,
                    poster.as_deref(),
                    &[],
                    "",
                    false,
                    Some(&render_chart(&ep_status, None)),
                    Some(&res.line),
                )
            } else {
                build_embed(
                    "ready",
                    &title,
                    year,
                    poster.as_deref(),
                    &parse_attempts(attempts_json.as_deref()),
                    "",
                    false,
                    None,
                    Some(&res.line),
                )
            };
            if !cfg().dry_run {
                edit_message(
                    cid.as_deref().unwrap_or(""),
                    mid.as_deref().unwrap_or(""),
                    &payload,
                );
            }
            log(&format!("verify update {}: {}", key, res.line));
        }
        let mut spooled = spooled.unwrap_or(0);
        if !res.ok && spooled == 0 && (now_t - import_at.unwrap_or(now_t)) > config::BAZARR_GRACE {
            let t = title_raw.clone().unwrap_or_default();
            spooled = if wake_hermes(&instn, &iid, &kind, &t, &res) { 1 } else { 0 };
        }
        let _ = con.execute(
            "UPDATE notified SET verify_ok=?, verify_line=?, verify_last=?, spooled=? WHERE key=?",
            params![if res.ok { 1 } else { 0 }, res.line, now_t, spooled, key],
        );
    }
}

/// Failed items with no replacement grab for FAIL_STALL_WINDOW: this is the
/// 'requester doesn't see it and nothing is happening' state — wake the agent
/// once, and be honest in the embed that a hunt is underway. (A new grab
/// resurrects the row via handle(), so anything sitting in phase='failed' this
/// long truly has nothing in flight.)
fn recheck_failures(con: &Connection) {
    let now_t = now();
    type FRow = (
        String,         // key
        Option<String>, // channel_id
        Option<String>, // message_id
        Option<String>, // title
        Option<String>, // kind
        Option<String>, // attempts
        Option<String>, // poster
        Option<i64>,    // year
    );
    let rows: Vec<FRow> = {
        let mut stmt = match con.prepare(
            "SELECT key, channel_id, message_id, title, kind, attempts, poster, year \
             FROM notified WHERE phase='failed' AND (fail_woken IS NULL OR fail_woken=0) \
             AND updated_at < ?",
        ) {
            Ok(s) => s,
            Err(_) => return,
        };
        let mapped = stmt.query_map(params![now_t - FAIL_STALL_WINDOW], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        });
        match mapped {
            Ok(it) => it.filter_map(|r| r.ok()).collect(),
            Err(_) => return,
        }
    };
    for (key, cid, mid, title_raw, kind_raw, attempts_json, poster, year) in rows {
        let (instn, iid) = split_key(&key);
        let kind = kind_raw.unwrap_or_default();
        let title_log = title_raw.clone().unwrap_or_default();
        // Type-driven arr_item_gone: only a definitive 404 drops the row —
        // a transient error must never delete tracking or skip escalation.
        match fetch_item(&instn, &kind, &iid) {
            Ok(None) => {
                let _ = con.execute("DELETE FROM notified WHERE key=?", [&key]);
                log(&format!(
                    "{}:{} deleted from arr — dropped failed-row, no escalation: {}",
                    instn, iid, title_log
                ));
                continue;
            }
            Ok(Some(m)) => {
                // a file appearing means someone already fixed it
                if kind == "movie" && m.b("hasFile") {
                    let _ = con.execute(
                        "UPDATE notified SET phase='importing', import_at=?, updated_at=? WHERE key=?",
                        params![now_t, now_t, key],
                    );
                    continue;
                }
            }
            Err(_) => {} // transient — proceed exactly as the Python did (gone=False)
        }
        let attempts = parse_attempts(attempts_json.as_deref());
        let woken = wake_hermes_failed(&instn, &iid, &kind, &title_log, &attempts);
        if woken {
            let title = match &title_raw {
                Some(t) if !t.is_empty() => t.clone(),
                _ => "Your request".to_string(),
            };
            let mut payload = build_embed(
                "failed",
                &title,
                year,
                poster.as_deref(),
                &attempts,
                "",
                false,
                None,
                None,
            );
            let d = payload["embeds"][0]["description"]
                .as_str()
                .unwrap_or("")
                .to_string();
            payload["embeds"][0]["description"] = json!(format!(
                "{}\n_No working release found so far — asked the agent to hunt an alternate source._",
                d
            ));
            if !cfg().dry_run {
                edit_message(cid.as_deref().unwrap_or(""), mid.as_deref().unwrap_or(""), &payload);
            }
        }
        let _ = con.execute(
            "UPDATE notified SET fail_woken=?, updated_at=? WHERE key=?",
            params![if woken { 1 } else { 0 }, now_t, key],
        );
    }
}

// ── queue-wide stuck sweep ──────────────────────────────────────────────────
static LAST_STUCK_SWEEP: std::sync::Mutex<f64> = std::sync::Mutex::new(0.0);

fn stuck_reason(r: &Value) -> String {
    let mut msgs: Vec<String> = Vec::new();
    for m in r.a("statusMessages") {
        let inner = m.a("messages");
        if !inner.is_empty() {
            msgs.extend(inner.iter().filter_map(|x| x.as_str().map(String::from)));
        } else if !m.s("title").is_empty() {
            msgs.push(m.s("title").to_string());
        }
    }
    if !r.s("errorMessage").is_empty() {
        msgs.push(r.s("errorMessage").to_string());
    }
    let joined = msgs
        .iter()
        .filter(|x| !x.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("; ");
    joined.chars().take(160).collect()
}

fn is_stuck_record(r: &Value) -> bool {
    if r.s("status") == "failed" {
        return true;
    }
    let tds = r.s("trackedDownloadState");
    if tds == "importBlocked" || tds == "importFailed" {
        return true;
    }
    tds == "importPending" && r.s("trackedDownloadStatus") == "warning"
}

/// Queue-wide stuck detection covering what per-request tracking can't see
/// (unattributed backfill/RSS grabs, import-blocked items). Persistently stuck
/// items wake Hermes once, batched, via the queue-stuck webhook — the agent
/// triages; nothing gets dumped into chat.
fn sweep_stuck(con: &Connection) {
    let now_t = now();
    {
        let mut last = LAST_STUCK_SWEEP.lock().unwrap();
        if now_t - *last < STUCK_SWEEP_EVERY {
            return;
        }
        *last = now_t;
    }
    // dict semantics: insertion order for iteration, last write wins per key.
    let mut order: Vec<String> = Vec::new();
    let mut current: HashMap<String, (String, Value)> = HashMap::new();
    for i in &cfg().instances {
        let unk = if i.name == "radarr" {
            "includeUnknownMovieItems=true"
        } else {
            "includeUnknownSeriesItems=true"
        };
        let q = arr_get(i.name, &format!("queue?pageSize=500&{}", unk)).unwrap_or_else(|| json!({}));
        for r in q.a("records") {
            if is_stuck_record(r) {
                let dlid = r.s("downloadId");
                let k = if !dlid.is_empty() {
                    format!("{}:{}", i.name, dlid)
                } else {
                    format!("{}:q{}", i.name, r.i("id"))
                };
                if !current.contains_key(&k) {
                    order.push(k.clone());
                }
                current.insert(k, (i.name.to_string(), r.clone()));
            }
        }
    }
    let seen: Vec<String> = {
        let mut stmt = match con.prepare("SELECT key FROM stuck_seen") {
            Ok(s) => s,
            Err(_) => return,
        };
        let mapped = stmt.query_map([], |r| r.get::<_, String>(0));
        let out: Vec<String> = match mapped {
            Ok(it) => it.filter_map(|r| r.ok()).collect(),
            Err(_) => return,
        };
        out
    };
    for k in &seen {
        if !current.contains_key(k) {
            // resolved — re-arm for any future re-stick
            let _ = con.execute("DELETE FROM stuck_seen WHERE key=?", [k]);
        }
    }
    let mut ripe: Vec<String> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    for k in &order {
        let (instn, r) = &current[k];
        let row: Option<(Option<f64>, Option<i64>)> = con
            .query_row(
                "SELECT first_seen, woken FROM stuck_seen WHERE key=?",
                [k],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .unwrap_or(None);
        match row {
            None => {
                let _ = con.execute(
                    "INSERT INTO stuck_seen(key, first_seen, woken) VALUES(?,?,0)",
                    params![k, now_t],
                );
            }
            Some((first_seen, woken)) => {
                if woken.unwrap_or(0) == 0 && now_t - first_seen.unwrap_or(now_t) > STUCK_WAKE_AFTER
                {
                    ripe.push(k.clone());
                    let t = r.s("title");
                    let t = if t.is_empty() { "?" } else { t };
                    let reason = stuck_reason(r);
                    let reason = if reason.is_empty() {
                        r.s("status").to_string()
                    } else {
                        reason
                    };
                    lines.push(format!(
                        "{}: {} — {}",
                        instn,
                        t.chars().take(70).collect::<String>(),
                        reason
                    ));
                }
            }
        }
    }
    if !ripe.is_empty() {
        let items = lines.iter().take(10).cloned().collect::<Vec<_>>().join("\n");
        let ok = verify::wake_hermes_stuck(ripe.len(), &items);
        if ok {
            log(&format!("hermes woken for {} stuck queue item(s)", ripe.len()));
            for k in &ripe {
                let _ = con.execute("UPDATE stuck_seen SET woken=1 WHERE key=?", [k]);
            }
        }
    }
}

/// First-ever run: remember everything already downloading so we ignore the
/// backlog and only notify about downloads that start after we're live.
fn seed_preexisting(con: &Connection) {
    let has = |sql: &str| {
        con.query_row(sql, [], |_| Ok(()))
            .optional()
            .unwrap_or(None)
            .is_some()
    };
    if has("SELECT 1 FROM notified LIMIT 1") {
        return; // not a fresh DB — nothing to seed
    }
    if has("SELECT 1 FROM preexisting LIMIT 1") {
        return; // already seeded
    }
    // Fetch every queue BEFORE inserting anything: the Python crashed (and was
    // restarted with nothing committed) if a queue read failed mid-seed, so a
    // partial ignore-set was never persisted. Exiting pre-insert is the same
    // all-or-nothing behavior.
    let mut slots: Vec<String> = Vec::new();
    for i in &cfg().instances {
        if i.key.is_empty() {
            continue;
        }
        let Some(groups) = aggregate_queue(i.name) else {
            log(&format!(
                "FATAL: queue fetch failed for {} during first-boot seeding",
                i.name
            ));
            std::process::exit(1);
        };
        for iid in groups.keys() {
            slots.push(format!("{}:{}", i.name, iid));
        }
    }
    let n = slots.len();
    for slot in slots {
        let _ = con.execute("INSERT OR IGNORE INTO preexisting(slot) VALUES(?)", [slot]);
    }
    log(&format!(
        "seeded {} pre-existing downloads to ignore on this fresh start",
        n
    ));
}

fn poll_once(
    con: &Connection,
    people_by_jf: &HashMap<String, String>,
    valid_discord: &HashSet<String>,
) {
    let seerr_map = seerr_requester_map(people_by_jf);
    let mut active_keys: HashSet<String> = HashSet::new();
    let mut present_slots: HashSet<String> = HashSet::new();
    let mut ok_instances: HashSet<&str> = HashSet::new(); // instances whose queue we actually read
    let ignored: HashSet<String> = {
        let mut stmt = match con.prepare("SELECT slot FROM preexisting") {
            Ok(s) => s,
            Err(_) => return,
        };
        let mapped = stmt.query_map([], |r| r.get::<_, String>(0));
        let out: HashSet<String> = match mapped {
            Ok(it) => it.filter_map(|r| r.ok()).collect(),
            Err(_) => return,
        };
        out
    };

    for i in &cfg().instances {
        if i.key.is_empty() {
            continue;
        }
        let Some(groups) = aggregate_queue(i.name) else {
            // Queue fetch failed — don't let a transient hiccup look like "every
            // download cleared" (that would wipe the ignore-set AND false-fire
            // "imported" on everything still downloading). Skip this instance.
            continue;
        };
        ok_instances.insert(i.name);
        let tag_map = tag_requester_map(i.name, valid_discord);
        for (iid, g) in &groups {
            let slot = format!("{}:{}", i.name, iid);
            present_slots.insert(slot.clone());
            if ignored.contains(&slot) {
                continue; // part of the startup backlog — skip until it clears
            }
            let mut requesters: BTreeSet<String> = BTreeSet::new();
            if let Some(s) = seerr_map.get(&(i.name, *iid)) {
                requesters.extend(s.iter().cloned());
            }
            if let Some(s) = tag_map.get(iid) {
                requesters.extend(s.iter().cloned());
            }
            if requesters.is_empty() {
                continue;
            }
            let title = match &g.title {
                Some(t) if !t.is_empty() => t.clone(),
                _ => format!("{} #{}", i.name, iid),
            };
            for did in &requesters {
                let key = format!("{}:{}:{}", i.name, iid, did);
                active_keys.insert(key.clone());
                handle(con, &key, did, &title, g);
            }
        }
    }

    // A pre-existing slot that has cleared the queue is forgotten, so the same
    // series/movie gets normal treatment for its next (genuinely new) download.
    // Only prune slots for instances we actually read this cycle.
    let stale: Vec<&String> = ignored
        .iter()
        .filter(|s| !present_slots.contains(*s))
        .filter(|s| {
            let inst_part = s.rsplitn(2, ':').nth(1).unwrap_or("");
            ok_instances.contains(inst_part)
        })
        .collect();
    for s in stale {
        let _ = con.execute("DELETE FROM preexisting WHERE slot=?", [s]);
    }

    // A tracked 'downloading' item that's been absent from its (successfully-
    // read) queue for MISSING_GRACE consecutive polls has really left — it
    // imported (→ wait for Jellyfin) or was removed (→ quiet). A shorter
    // absence is just a flicker; we bump a counter and wait rather than
    // declaring it finished.
    let rows: Vec<(String, Option<i64>)> = {
        let mut stmt = match con.prepare("SELECT key, missing FROM notified WHERE phase='downloading'")
        {
            Ok(s) => s,
            Err(_) => return,
        };
        let mapped = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)));
        let out: Vec<(String, Option<i64>)> = match mapped {
            Ok(it) => it.filter_map(|r| r.ok()).collect(),
            Err(_) => return,
        };
        out
    };
    for (key, missing) in rows {
        let inst_part = key.rsplitn(3, ':').nth(2).unwrap_or("");
        if !ok_instances.contains(inst_part) || active_keys.contains(&key) {
            continue;
        }
        let missing = missing.unwrap_or(0) + 1;
        if missing >= MISSING_GRACE {
            on_left_queue(con, &key);
        } else {
            let _ = con.execute(
                "UPDATE notified SET missing=? WHERE key=?",
                params![missing, key],
            );
        }
    }

    // Items already imported: ping once Jellyfin has actually scanned them in.
    confirm_imported(con);
    // Recently-readied items whose episodes Jellyfin deleted again: rescan + re-arm.
    recheck_jf_presence(con);
    // Failed language verifications: re-probe, live-update the 🔎 line, escalate.
    recheck_verifications(con);
    // Failed downloads with nothing in flight: wake the agent to hunt a source.
    recheck_failures(con);
    // Queue-wide stuck sweep (import-blocked + unattributed items).
    sweep_stuck(con);
}

// ── self-test: prove the embed post+edit+ready mechanic against a real DM ────
fn selftest(discord_id: &str) -> i32 {
    let (title, year) = ("Kung Fu Panda", Some(2008));
    let poster = "https://image.tmdb.org/t/p/original/wWt4JYXTg5Wr3xBW2phBrMKgp3x.jpg";
    log(&format!("selftest -> DM {}", discord_id));
    let mut att = vec![json!({"pct": 0, "status": "active"})];
    let (cid, mid) = send_message(
        discord_id,
        &build_embed("downloading", title, year, Some(poster), &att, "00:05:00", false, None, None),
    );
    let (Some(cid), Some(mid)) = (cid, mid) else {
        log("selftest FAILED: could not send DM");
        return 1;
    };
    for pct in [18, 47, 73, 96] {
        std::thread::sleep(Duration::from_millis(1500));
        att.last_mut().unwrap()["pct"] = json!(pct);
        let tl = format!("00:0{}:00", std::cmp::max(1, 5 - pct / 20));
        edit_message(
            &cid,
            &mid,
            &build_embed("downloading", title, year, Some(poster), &att, &tl, false, None, None),
        );
        log(&format!("  edited -> {}%", pct));
    }
    // demo a failed attempt + retry, then ready
    std::thread::sleep(Duration::from_millis(1500));
    att.last_mut().unwrap()["status"] = json!("failed");
    att.push(json!({"pct": 34, "status": "active"}));
    edit_message(
        &cid,
        &mid,
        &build_embed("downloading", title, year, Some(poster), &att, "00:02:00", false, None, None),
    );
    std::thread::sleep(Duration::from_millis(1500));
    edit_message(
        &cid,
        &mid,
        &build_embed("ready", title, year, Some(poster), &att, "", false, None, None),
    );
    log("selftest OK (check your Discord DMs)");
    0
}

fn run() -> i32 {
    if cfg().token.is_empty() {
        log("FATAL: DISCORD_BOT_TOKEN not set");
        return 2;
    }
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 && args[1] == "--selftest" {
        return selftest(&args[2]);
    }

    let (people_by_jf, valid_discord) = load_people();
    log(&format!(
        "loaded {} people ({} with jellyfin); dry_run={}; poll={}s; state={}",
        valid_discord.len(),
        people_by_jf.len(),
        if cfg().dry_run { "True" } else { "False" },
        cfg().poll_interval,
        cfg().state_db
    ));
    let con = match state::db_conn() {
        Ok(c) => c,
        Err(e) => {
            log(&format!("FATAL: state db: {}", e));
            return 1;
        }
    };
    seed_preexisting(&con);
    let once = args.iter().any(|a| a == "--once");
    loop {
        // Python wraps poll_once in a broad try/except; catch panics so one bad
        // poll can't kill the daemon.
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            poll_once(&con, &people_by_jf, &valid_discord)
        }));
        if let Err(e) = r {
            let msg = e
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| e.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic".to_string());
            log(&format!("poll error: {}", msg));
        }
        if once {
            return 0;
        }
        std::thread::sleep(Duration::from_secs(cfg().poll_interval));
    }
}

fn main() {
    std::process::exit(run());
}
