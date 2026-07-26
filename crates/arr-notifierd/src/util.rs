//! log() with the [timestamp] prefix (journald captures stdout), time helpers,
//! Python-compatible rounding, and loose JSON scalar coercion.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn tm_fmt(tm: &libc::tm) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

fn local_ts() -> String {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm_fmt(&tm)
    }
}

/// Python time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(epoch)).
pub fn utc_ts(epoch: f64) -> String {
    unsafe {
        let t = epoch as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        libc::gmtime_r(&t, &mut tm);
        tm_fmt(&tm)
    }
}

/// Rust's println! stdout is always line-buffered — flush=True equivalent.
pub fn log(msg: &str) {
    println!("[{}] {}", local_ts(), msg);
}

/// Python round(): banker's rounding to int.
pub fn py_round(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// Python str(value) for JSON scalars ("" for null/missing/other).
pub fn scalar_str(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// Python `x or fallback` semantics folded into str(): a falsy value (null,
/// 0, "") becomes "" so `str(g["tmdb"] or "")` round-trips exactly.
pub fn truthy_scalar_str(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => {
            if n.as_f64() == Some(0.0) {
                String::new()
            } else {
                n.to_string()
            }
        }
        _ => String::new(),
    }
}

/// key "inst:iid:did" (or "inst:iid") -> (inst, iid), like key.split(":", 2).
pub fn split_key(key: &str) -> (String, String) {
    let mut it = key.splitn(3, ':');
    (
        it.next().unwrap_or("").to_string(),
        it.next().unwrap_or("").to_string(),
    )
}
