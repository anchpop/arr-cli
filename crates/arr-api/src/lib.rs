//! arr-api — shared plumbing for the `arr` CLI and `arr-notifierd` daemon.
//!
//! Faithful port of arr.py's foundation: env-file key loading, the per-service
//! HTTP clients, and the small helpers every command leans on. All HTTP entry
//! points come in two flavors: a fallible `try_*` (for the daemon, which must
//! survive transient errors) and a die-on-error wrapper matching arr.py's exact
//! error strings (for the CLI).

pub mod env;
pub mod http;
pub mod json;
pub mod util;

pub use env::{
    bindery_key, env_key, get_key, jf_item_url, jf_key, jf_public_url, sab_key, seerr_key,
    wizarr_key, wizarr_public_url,
};
pub use http::{
    api, api_t, bazarr_api, bindery_api, jf_api, sab_api, seerr_api, shelfmark_api, try_api,
    try_seerr, wizarr_api, ApiError,
};
pub use json::JsonExt;
pub use util::{die, fmt_gb, gb, mb, parse_seasons, pop_flags, resolve_id, Flags};

/// Arr service registry (mirrors SERVICES in arr.py). `sonarr-anime` is a
/// second Sonarr: series logic keys off `is_series()`, not the name.
pub fn svc_cfg(svc: &str) -> Option<(u16, &'static str, &'static str)> {
    Some(match svc {
        "sonarr" => (8989, "v3", "SONARR_API_KEY"),
        "sonarr-anime" => (8990, "v3", "SONARR_ANIME_API_KEY"),
        "radarr" => (7878, "v3", "RADARR_API_KEY"),
        "prowlarr" => (9696, "v1", "PROWLARR_API_KEY"),
        _ => return None,
    })
}

pub fn is_series(svc: &str) -> bool {
    svc.starts_with("sonarr")
}

pub const SAB_PORT: u16 = 8085;
pub const QBIT_PORT: u16 = 8080;
pub const JELLYFIN_PORT: u16 = 8096;
pub const SEERR_PORT: u16 = 5055;
pub const BAZARR_PORT: u16 = 6767;
pub const BINDERY_PORT: u16 = 8787;
pub const SHELFMARK_PORT: u16 = 8084;
pub const WIZARR_PORT: u16 = 5690;
