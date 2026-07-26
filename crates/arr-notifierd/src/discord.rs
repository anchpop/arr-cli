//! Discord REST (same bot token as Hermes): DMs, PATCH edits, 429 back-off,
//! and the private-thread fallback for DM-blocked users (Discord 50278).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use arr_api::JsonExt;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::config::cfg;
use crate::util::log;

/// Discord REST call with 429 back-off. Returns parsed JSON (or None).
pub fn discord(method: &str, path: &str, body: Option<&Value>) -> Option<Value> {
    let url = format!("https://discord.com/api/v10{}", path);
    for _attempt in 0..6 {
        // Discord's edge is Cloudflare and rejects the default UA (err 1010) —
        // send the documented DiscordBot User-Agent form.
        let req = ureq::agent()
            .request(method, &url)
            .set("Authorization", &format!("Bot {}", cfg().token))
            .set(
                "User-Agent",
                "DiscordBot (https://beef.baby, 1.0) download-notifier",
            )
            .timeout(Duration::from_secs(30));
        let resp = match body {
            Some(b) => req.send_json(b.clone()),
            None => req.call(),
        };
        match resp {
            Ok(r) => {
                let raw = r.into_string().unwrap_or_default();
                if raw.is_empty() {
                    return None;
                }
                return serde_json::from_str(&raw).ok();
            }
            Err(ureq::Error::Status(429, r)) => {
                let raw = r.into_string().unwrap_or_default();
                let retry = serde_json::from_str::<Value>(&raw)
                    .ok()
                    .and_then(|v| v.get("retry_after").and_then(|x| x.as_f64()))
                    .unwrap_or(1.0);
                log(&format!("discord 429 — sleeping {:.1}s", retry));
                std::thread::sleep(Duration::from_secs_f64(retry + 0.25));
                continue;
            }
            Err(ureq::Error::Status(code, r)) => {
                let snippet: String = r.into_string().unwrap_or_default().chars().take(200).collect();
                log(&format!(
                    "discord {} {} HTTP {}: {:?}",
                    method, path, code, snippet
                ));
                return None;
            }
            Err(ureq::Error::Transport(t)) => {
                log(&format!("discord {} {} error: {}", method, path, t));
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
    None
}

fn dm_channels() -> &'static Mutex<HashMap<String, String>> {
    static M: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

pub fn dm_channel(discord_id: &str) -> Option<String> {
    if let Some(c) = dm_channels().lock().unwrap().get(discord_id) {
        return Some(c.clone());
    }
    let res = discord(
        "POST",
        "/users/@me/channels",
        Some(&json!({ "recipient_id": discord_id })),
    );
    let cid = res
        .as_ref()
        .map(|r| r.s("id").to_string())
        .filter(|c| !c.is_empty());
    if let Some(c) = &cid {
        dm_channels()
            .lock()
            .unwrap()
            .insert(discord_id.to_string(), c.clone());
    }
    cid
}

pub fn send_message(discord_id: &str, payload: &Value) -> (Option<String>, Option<String>) {
    let Some(cid) = dm_channel(discord_id) else {
        return (None, None);
    };
    let res = discord("POST", &format!("/channels/{}/messages", cid), Some(payload));
    let mid = res
        .as_ref()
        .map(|r| r.s("id").to_string())
        .filter(|m| !m.is_empty());
    (Some(cid), mid)
}

pub fn edit_message(cid: &str, mid: &str, payload: &Value) -> Option<Value> {
    discord(
        "PATCH",
        &format!("/channels/{}/messages/{}", cid, mid),
        Some(payload),
    )
}

// ── private-thread fallback (for users whose DM privacy blocks the bot) ──────
// A user with "Allow DMs from server members" off can't be DM'd by the bot AND
// can't DM the bot — but a guild private thread isn't subject to DM privacy, so
// we deliver their updates there instead. One persistent thread per user, in the
// #bot channel; only they (+ server admins) can see it. The per-download message
// + edit flow is identical to a DM (we just store the thread id as channel_id).

// discord id -> display name (for naming private threads)
static PEOPLE_NAMES: OnceLock<HashMap<String, String>> = OnceLock::new();

pub fn set_people_names(names: HashMap<String, String>) {
    let _ = PEOPLE_NAMES.set(names);
}

fn thread_name(discord_id: &str) -> String {
    let name = PEOPLE_NAMES
        .get()
        .and_then(|m| m.get(discord_id))
        .map(|s| s.as_str())
        .unwrap_or("your");
    format!("📥 {} — downloads", name)
}

pub fn get_or_create_thread(con: &Connection, discord_id: &str) -> Option<String> {
    let existing: Option<String> = con
        .query_row(
            "SELECT thread_id FROM user_threads WHERE discord_id=?",
            [discord_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()
        .unwrap_or(None)
        .flatten();
    if let Some(tid) = existing {
        if !tid.is_empty() {
            return Some(tid);
        }
    }
    let nudge = &cfg().nudge_channel;
    if nudge.is_empty() {
        return None;
    }
    let thr = discord(
        "POST",
        &format!("/channels/{}/threads", nudge),
        Some(&json!({
            "name": thread_name(discord_id), "type": 12,
            "auto_archive_duration": 10080, "invitable": false
        })),
    );
    let tid = thr
        .as_ref()
        .map(|t| t.s("id").to_string())
        .filter(|t| !t.is_empty())?;
    discord(
        "PUT",
        &format!("/channels/{}/thread-members/{}", tid, discord_id),
        None,
    );
    let _ = con.execute(
        "INSERT OR REPLACE INTO user_threads(discord_id, thread_id) VALUES(?,?)",
        rusqlite::params![discord_id, tid],
    );
    log(&format!(
        "created private thread {} for {} ({})",
        tid,
        discord_id,
        thread_name(discord_id)
    ));
    Some(tid)
}

/// Post into the user's private thread (pinging them). Returns (thread_id, message_id).
pub fn send_via_thread(
    con: &Connection,
    discord_id: &str,
    payload: &Value,
) -> (Option<String>, Option<String>) {
    let Some(mut tid) = get_or_create_thread(con, discord_id) else {
        return (None, None);
    };
    // mention => notifies + ensures membership
    let mut body = payload.clone();
    body["content"] = Value::String(format!("<@{}>", discord_id));
    let res = discord("POST", &format!("/channels/{}/messages", tid), Some(&body));
    let mut mid = res
        .as_ref()
        .map(|r| r.s("id").to_string())
        .filter(|m| !m.is_empty());
    if mid.is_none() {
        // thread likely deleted — recreate once and retry
        let _ = con.execute("DELETE FROM user_threads WHERE discord_id=?", [discord_id]);
        let Some(t2) = get_or_create_thread(con, discord_id) else {
            return (None, None);
        };
        tid = t2;
        let res = discord("POST", &format!("/channels/{}/messages", tid), Some(&body));
        mid = res
            .as_ref()
            .map(|r| r.s("id").to_string())
            .filter(|m| !m.is_empty());
    }
    match mid {
        Some(m) => (Some(tid), Some(m)),
        None => (None, None),
    }
}
