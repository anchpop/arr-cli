//! Env-var config, the INSTANCES registry, and tuning constants — a faithful
//! mirror of download-notifier.py's module-level config block.

use std::sync::OnceLock;

pub const SEERR_URL: &str = "http://localhost:5055";
pub const JELLYFIN_URL: &str = "http://localhost:8096";

pub const VERIFY_RECHECK_EVERY: f64 = 300.0; // re-probe a failed verification every 5 min
pub const VERIFY_RECHECK_WINDOW: f64 = 48.0 * 3600.0; // ...for up to 48h after ready
pub const BAZARR_GRACE: f64 = 30.0 * 60.0; // give a Bazarr kick 30 min before waking Hermes
// Repeated download failures wake Hermes to hunt an alternate source: after
// FAIL_WAKE_THRESHOLD failed attempts (below that, the arr is still working
// its own candidate list and the agent would duplicate it), or when a failed
// item has gone FAIL_STALL_WINDOW with no replacement grab and no file.
// Thresholds per Andre (2026-07-21): 10 attempts / 12 hours. One wake per
// item; the requester's embed self-heals the moment a new grab starts.
pub const FAIL_WAKE_THRESHOLD: i64 = 10;
pub const FAIL_STALL_WINDOW: f64 = 12.0 * 3600.0;
pub const STUCK_SWEEP_EVERY: f64 = 300.0;
pub const STUCK_WAKE_AFTER: f64 = 15.0 * 60.0;

pub const BAR_WIDTH: i64 = 12;
pub const DONE_THRESHOLD: i64 = 95; // only treat a vanished download as finished if it got this far
// Once a download imports we wait for Jellyfin to actually scan it in before the
// "ready to watch" ping. If Jellyfin never confirms within this window we send
// it anyway rather than stay silent.
pub const JF_CONFIRM_TIMEOUT: f64 = 45.0 * 60.0;
// A download can briefly drop out of the arr queue without having actually
// finished. Require it to be absent this many consecutive polls before we treat
// it as "left the queue" — otherwise a flicker looks like completion and
// re-sends a fresh DM when it reappears (the duplicate-message bug).
pub const MISSING_GRACE: i64 = 3;

/// One arr instance: how to talk to it + how Seerr refers to it.
pub struct Inst {
    pub name: &'static str,
    pub url: &'static str,
    pub key: String,
    pub id_field: &'static str,
    pub queue_extra: &'static str,
    /// Seerr's (media_type, serviceId) for this instance.
    pub seerr_type: &'static str,
    pub seerr_service_id: i64,
}

pub struct Config {
    pub token: String,
    pub people_json: String,
    pub state_db: String,
    pub poll_interval: u64,
    pub dry_run: bool,
    pub seerr_key: String,
    pub jellyfin_key: String,
    // #bot channel for the private-thread fallback when a requester's Discord
    // blocks bot-initiated DMs (50278).
    pub nudge_channel: String,
    pub ffprobe: String, // store path injected by Nix
    pub bazarr_url: String,
    pub bazarr_key: String,
    pub langgap_webhook_url: String,
    pub langgap_webhook_secret: String,
    pub failed_webhook_url: String,
    pub stuck_webhook_url: String,
    pub instances: Vec<Inst>,
}

fn envs(k: &str, default: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| default.to_string())
}

pub fn cfg() -> &'static Config {
    static C: OnceLock<Config> = OnceLock::new();
    C.get_or_init(|| Config {
        token: envs("DISCORD_BOT_TOKEN", ""),
        people_json: envs("PEOPLE_JSON", ""),
        state_db: envs("STATE_DB", "/var/lib/download-notifier/state.db"),
        poll_interval: envs("POLL_INTERVAL", "20").parse().unwrap_or_else(|_| {
            eprintln!("FATAL: POLL_INTERVAL is not an integer");
            std::process::exit(2)
        }),
        dry_run: !matches!(envs("DRY_RUN", "").as_str(), "" | "0" | "false" | "no"),
        seerr_key: envs("SEERR_API_KEY", ""),
        jellyfin_key: envs("JELLYFIN_API_KEY", ""),
        nudge_channel: envs("NUDGE_CHANNEL_ID", ""),
        ffprobe: envs("FFPROBE", "ffprobe"),
        bazarr_url: envs("BAZARR_URL", "http://localhost:6767"),
        bazarr_key: envs("BAZARR_API_KEY", ""),
        langgap_webhook_url: envs("LANGGAP_WEBHOOK_URL", ""),
        langgap_webhook_secret: envs("LANGGAP_WEBHOOK_SECRET", ""),
        failed_webhook_url: envs("FAILED_WEBHOOK_URL", ""),
        stuck_webhook_url: envs("STUCK_WEBHOOK_URL", ""),
        instances: vec![
            Inst {
                name: "radarr",
                url: "http://localhost:7878",
                key: envs("RADARR_API_KEY", ""),
                id_field: "movieId",
                queue_extra: "includeMovie=true",
                seerr_type: "movie",
                seerr_service_id: 0,
            },
            Inst {
                name: "sonarr",
                url: "http://localhost:8989",
                key: envs("SONARR_API_KEY", ""),
                id_field: "seriesId",
                queue_extra: "includeSeries=true&includeEpisode=true",
                seerr_type: "tv",
                seerr_service_id: 0,
            },
            Inst {
                name: "sonarr-anime",
                url: "http://localhost:8990",
                key: envs("SONARR_ANIME_API_KEY", ""),
                id_field: "seriesId",
                queue_extra: "includeSeries=true&includeEpisode=true",
                seerr_type: "tv",
                seerr_service_id: 1,
            },
        ],
    })
}

pub fn inst(name: &str) -> Option<&'static Inst> {
    cfg().instances.iter().find(|i| i.name == name)
}
