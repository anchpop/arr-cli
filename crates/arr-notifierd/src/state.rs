//! sqlite state. Schema (table/column names + value conventions) must match
//! the Python daemon exactly so the Rust port adopts the existing
//! /var/lib/download-notifier/state.db without migration.

use std::collections::HashSet;

use rusqlite::Connection;

use crate::config::cfg;

pub fn db_conn() -> rusqlite::Result<Connection> {
    let path = &cfg().state_db;
    if let Some(dir) = std::path::Path::new(path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let con = Connection::open(path)?;
    con.execute_batch(
        "CREATE TABLE IF NOT EXISTS notified(
              key TEXT PRIMARY KEY,
              discord_id TEXT, channel_id TEXT, message_id TEXT,
              last_content TEXT, last_pct INTEGER DEFAULT 0,
              phase TEXT,            -- 'downloading' | 'importing' | 'done'
              updated_at REAL)",
    )?;
    // Columns added when "ready" became "confirmed scanned into Jellyfin": we
    // stash each download's identity + title while it's still in the queue, so we
    // can check Jellyfin for it after it has left the queue.
    let mut have: HashSet<String> = HashSet::new();
    {
        let mut stmt = con.prepare("PRAGMA table_info(notified)")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
        for r in rows {
            have.insert(r?);
        }
    }
    for (col, decl) in [
        ("kind", "TEXT"),
        ("title", "TEXT"),
        ("tmdb", "TEXT"),
        ("imdb", "TEXT"),
        ("tvdb", "TEXT"),
        ("episodes", "TEXT"),
        ("jf_baseline", "INTEGER"),
        ("import_at", "REAL"),
        ("missing", "INTEGER"),
        ("attempts", "TEXT"),
        ("dlids", "TEXT"),
        ("poster", "TEXT"),
        ("year", "INTEGER"),
        ("ep_status", "TEXT"),
        ("multi", "INTEGER"),
        // language-requirement verification state (require-* tags)
        ("verify_ok", "INTEGER"),
        ("verify_line", "TEXT"),
        ("verify_deadline", "REAL"),
        ("verify_last", "REAL"),
        ("spooled", "INTEGER"),
        // failure escalation: 1 = Hermes already woken for this item
        ("fail_woken", "INTEGER"),
        // times a post-ready Jellyfin presence regression re-armed confirmation
        ("jf_regressions", "INTEGER"),
    ] {
        if !have.contains(col) {
            con.execute_batch(&format!("ALTER TABLE notified ADD COLUMN {} {}", col, decl))?;
        }
    }
    con.execute_batch(
        // preexisting: slots (inst:iid) to ignore — the backlog that was already
        // in the queue the first time we ever started, so a fresh install doesn't
        // spam DMs for downloads it didn't actually witness.
        // user_threads: per-user private thread used when the bot can't DM them.
        // stuck_seen: queue-wide stuck items (incl. unattributed) awaiting/past wake.
        "CREATE TABLE IF NOT EXISTS preexisting(slot TEXT PRIMARY KEY);
         CREATE TABLE IF NOT EXISTS user_threads(discord_id TEXT PRIMARY KEY, thread_id TEXT);
         CREATE TABLE IF NOT EXISTS stuck_seen(key TEXT PRIMARY KEY, first_seen REAL, woken INTEGER DEFAULT 0);",
    )?;
    Ok(con)
}
