//! HTTP clients for every service the CLI drives. The `try_api` variant exists
//! because the daemon must distinguish "definitive 404" from "transient error"
//! (a deleted movie must never look like a failed download — see the 2026-07-26
//! incident); the die-variants replicate arr.py's exact error strings.

use std::time::Duration;

use serde_json::Value;

use crate::env::{bazarr_key, get_key, jf_key, sab_key, seerr_key};
use crate::util::die;
use crate::{BAZARR_PORT, JELLYFIN_PORT, SAB_PORT, SEERR_PORT};

#[derive(Debug)]
pub enum ApiError {
    /// Non-2xx response. `detail` is the first 500 chars of the body.
    Http { code: u16, detail: String },
    Timeout,
    Net(String),
}

impl ApiError {
    pub fn is_404(&self) -> bool {
        matches!(self, ApiError::Http { code: 404, .. })
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().build()
}

fn run(
    req: ureq::Request,
    body: Option<&Value>,
    timeout: u64,
) -> Result<Option<Value>, ApiError> {
    let req = req.timeout(Duration::from_secs(timeout));
    let resp = match body {
        Some(b) => req.send_json(b.clone()),
        None => req.call(),
    };
    match resp {
        Ok(r) => {
            let raw = r.into_string().map_err(|e| ApiError::Net(e.to_string()))?;
            if raw.trim().is_empty() {
                Ok(None)
            } else {
                match serde_json::from_str(&raw) {
                    Ok(v) => Ok(Some(v)),
                    // SAB and Bazarr sometimes answer plain text ("ok\n")
                    Err(_) => Ok(Some(Value::String(raw))),
                }
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

pub fn form_encode(params: &[(&str, &str)]) -> String {
    let mut ser = form_urlencoded::Serializer::new(String::new());
    for (k, v) in params {
        ser.append_pair(k, v);
    }
    ser.finish()
}

/// Fallible arr call (radarr/sonarr/sonarr-anime/prowlarr).
pub fn try_api(
    svc: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
    timeout: u64,
) -> Result<Option<Value>, ApiError> {
    let (port, ver, _) = crate::svc_cfg(svc)
        .unwrap_or_else(|| die(&format!("unknown service '{}'", svc)));
    let url = format!("http://localhost:{}/api/{}{}", port, ver, path);
    let req = agent().request(method, &url).set("X-Api-Key", &get_key(svc));
    run(req, body, timeout)
}

/// Die-on-error arr call with arr.py's exact messages. Default timeout 120s.
pub fn api(svc: &str, method: &str, path: &str, body: Option<&Value>) -> Option<Value> {
    api_t(svc, method, path, body, 120)
}

pub fn api_t(
    svc: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
    timeout: u64,
) -> Option<Value> {
    match try_api(svc, method, path, body, timeout) {
        Ok(v) => v,
        Err(ApiError::Http { code, detail }) => {
            die(&format!("{} {} -> HTTP {} {}", method, path, code, detail))
        }
        Err(ApiError::Timeout) => die(&format!(
            "{} {} timed out after {}s (indexer searches can be slow — retry)",
            method, path, timeout
        )),
        Err(ApiError::Net(reason)) => die(&format!("{} {} -> {}", method, path, reason)),
    }
}

/// SABnzbd: ?mode=...&output=json&apikey=... Returns JSON, or the raw body as
/// a Value::String when SAB answers plain text.
pub fn sab_api(mode: &str, params: &[(&str, &str)], timeout: u64) -> Value {
    let mut q = vec![("mode", mode), ("output", "json")];
    let key = sab_key();
    q.push(("apikey", &key));
    q.extend_from_slice(params);
    let url = format!("http://localhost:{}/api/?{}", SAB_PORT, form_encode(&q));
    match run(agent().request("GET", &url), None, timeout) {
        Ok(Some(v)) => v,
        Ok(None) => Value::String(String::new()),
        Err(ApiError::Http { code, .. }) => die(&format!("sab {} -> HTTP {}", mode, code)),
        Err(ApiError::Timeout) => die(&format!("sab {} -> timed out", mode)),
        Err(ApiError::Net(reason)) => die(&format!("sab {} -> {}", mode, reason)),
    }
}

/// Jellyfin (X-Emby-Token). `soft` returns None on any error instead of dying.
pub fn jf_api(
    path: &str,
    params: &[(&str, &str)],
    timeout: u64,
    method: &str,
    soft: bool,
) -> Option<Value> {
    let qs = if params.is_empty() { String::new() } else { format!("?{}", form_encode(params)) };
    let url = format!("http://localhost:{}{}{}", JELLYFIN_PORT, path, qs);
    let req = agent().request(method, &url).set("X-Emby-Token", &jf_key());
    match run(req, None, timeout) {
        Ok(v) => v,
        Err(_) if soft => None,
        Err(ApiError::Http { code, .. }) => die(&format!("jellyfin {} -> HTTP {}", path, code)),
        Err(ApiError::Timeout) => die(&format!("jellyfin {} -> timed out", path)),
        Err(ApiError::Net(reason)) => die(&format!("jellyfin {} -> {}", path, reason)),
    }
}

/// Bazarr (X-API-KEY). Repeated query keys are legal (doseq in the Python).
pub fn bazarr_api(
    method: &str,
    path: &str,
    params: &[(&str, &str)],
    timeout: u64,
    soft: bool,
) -> Option<Value> {
    let qs = if params.is_empty() { String::new() } else { format!("?{}", form_encode(params)) };
    let url = format!("http://localhost:{}/api{}{}", BAZARR_PORT, path, qs);
    let req = agent().request(method, &url).set("X-API-KEY", &bazarr_key());
    match run(req, None, timeout) {
        Ok(v) => v,
        Err(_) if soft => None,
        Err(ApiError::Http { code, detail }) => {
            let d: String = detail.chars().take(200).collect();
            die(&format!("bazarr {} {} -> HTTP {} {}", method, path, code, d))
        }
        Err(ApiError::Timeout) => die(&format!("bazarr {} {} -> timed out", method, path)),
        Err(ApiError::Net(reason)) => die(&format!("bazarr {} {} -> {}", method, path, reason)),
    }
}

/// qBittorrent WebUI (localhost auth bypass): form-encoded POST.
pub fn qbit_post_form(path: &str, form: &[(&str, &str)]) -> Result<String, ApiError> {
    let url = format!("http://localhost:{}{}", crate::QBIT_PORT, path);
    let req = agent()
        .request("POST", &url)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .timeout(Duration::from_secs(60));
    match req.send_string(&form_encode(form)) {
        Ok(r) => r.into_string().map_err(|e| ApiError::Net(e.to_string())),
        Err(ureq::Error::Status(code, r)) => Err(ApiError::Http {
            code,
            detail: r.into_string().unwrap_or_default().chars().take(500).collect(),
        }),
        Err(ureq::Error::Transport(t)) => Err(ApiError::Net(t.to_string())),
    }
}

/// Seerr (X-Api-Key), GET only.
pub fn seerr_api(path: &str, params: &[(&str, &str)], timeout: u64, soft: bool) -> Option<Value> {
    let qs = if params.is_empty() { String::new() } else { format!("?{}", form_encode(params)) };
    let url = format!("http://localhost:{}/api/v1{}{}", SEERR_PORT, path, qs);
    let req = agent().request("GET", &url).set("X-Api-Key", &seerr_key());
    match run(req, None, timeout) {
        Ok(v) => v,
        Err(_) if soft => None,
        Err(ApiError::Http { code, .. }) => die(&format!("seerr {} -> HTTP {}", path, code)),
        Err(ApiError::Timeout) => die(&format!("seerr {} -> timed out", path)),
        Err(ApiError::Net(reason)) => die(&format!("seerr {} -> {}", path, reason)),
    }
}
