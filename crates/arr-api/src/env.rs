//! API-key loading. Order (first hit wins): $ARR_API_KEY_<SVC>, the plain
//! key name as an env var (hermes gets SONARR_API_KEY=... directly and cannot
//! read the sops file), then the sops-rendered env file.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::util::die;

pub fn env_file_path() -> String {
    std::env::var("ARR_ENV_FILE").unwrap_or_else(|_| "/run/secrets/rendered/arr-cli.env".into())
}

fn env_file() -> &'static HashMap<String, String> {
    static CACHE: OnceLock<HashMap<String, String>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let mut map = HashMap::new();
        if let Ok(text) = std::fs::read_to_string(env_file_path()) {
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((k, v)) = line.split_once('=') {
                    map.insert(k.trim().to_string(), v.trim().to_string());
                }
            }
        }
        map
    })
}

pub fn env_key(name: &str, envvar: &str) -> String {
    if let Ok(v) = std::env::var(envvar) {
        if !v.is_empty() {
            return v;
        }
    }
    if let Ok(v) = std::env::var(name) {
        if !v.is_empty() {
            return v;
        }
    }
    match env_file().get(name) {
        Some(v) if !v.is_empty() => v.clone(),
        _ => die(&format!(
            "no key {} (looked at ${}, ${} and {})",
            name,
            envvar,
            name,
            env_file_path()
        )),
    }
}

pub fn get_key(svc: &str) -> String {
    let (_, _, key) = crate::svc_cfg(svc)
        .unwrap_or_else(|| die(&format!("unknown service '{}'", svc)));
    env_key(key, &format!("ARR_API_KEY_{}", svc.to_uppercase()))
}

pub fn sab_key() -> String {
    env_key("SABNZBD_API_KEY", "ARR_API_KEY_SAB")
}

pub fn jf_key() -> String {
    env_key("JELLYFIN_API_KEY", "ARR_API_KEY_JELLYFIN")
}

pub fn seerr_key() -> String {
    env_key("SEERR_API_KEY", "ARR_API_KEY_SEERR")
}

pub fn bazarr_key() -> String {
    env_key("BAZARR_API_KEY", "ARR_API_KEY_BAZARR")
}
