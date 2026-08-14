//! Language-requirement verification (require-subs-<l> / require-audio-<l>
//! arr tags → ffprobe of the imported files), the Bazarr kick, the DV
//! profile-5 warning, and the HMAC-signed Hermes gateway webhooks.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use arr_api::JsonExt;
use serde_json::{json, Value};

use crate::arrs::arr_get;
use crate::config::cfg;
use crate::util::{log, utc_ts};

fn norm_lang(lang: &str) -> String {
    let l = lang.to_lowercase();
    match l.as_str() {
        "en" => "eng",
        "ja" | "jp" => "jpn",
        "fr" | "fra" => "fre",
        "de" | "deu" => "ger",
        "ko" => "kor",
        // Chinese discs tag audio any of zh/zho/chi/cmn (Mandarin)/yue
        // (Cantonese)/chn; fold them all into "chi" so require-audio-chi
        // matches whatever the release used (Better Days, 2026-08-14).
        "zh" | "zho" | "cmn" | "yue" | "chn" => "chi",
        "es" => "spa",
        "it" => "ita",
        "pt" => "por",
        "ru" => "rus",
        _ => return l,
    }
    .to_string()
}

/// (audio_langs, sub_langs) demanded by require-* tags on the arr item.
fn requirement_langs(instn: &str, iid: &str, kind: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut labels: HashMap<i64, String> = HashMap::new();
    if let Some(tags) = arr_get(instn, "tag") {
        for t in tags.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
            labels.insert(t.i("id"), t.s("label").to_lowercase());
        }
    }
    let path = if kind == "movie" {
        format!("movie/{}", iid)
    } else {
        format!("series/{}", iid)
    };
    let item = arr_get(instn, &path);
    let (mut want_a, mut want_s) = (BTreeSet::new(), BTreeSet::new());
    if let Some(item) = &item {
        for tid in item.a("tags") {
            let lab = tid
                .as_i64()
                .and_then(|t| labels.get(&t))
                .map(|s| s.as_str())
                .unwrap_or("");
            if let Some(rest) = lab.strip_prefix("require-audio-") {
                want_a.insert(norm_lang(rest));
            } else if let Some(rest) = lab.strip_prefix("require-subs-") {
                want_s.insert(norm_lang(rest));
            }
        }
    }
    (want_a, want_s)
}

/// subprocess.run([...], capture_output=True, timeout=N) equivalent: returns
/// stdout, kills the child on timeout.
fn run_ffprobe(args: &[&str], timeout: u64) -> Result<String, String> {
    let mut child = Command::new(&cfg().ffprobe)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut out = child.stdout.take().ok_or("no stdout")?;
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + Duration::from_secs(timeout);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("timed out after {}s", timeout));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(reader.join().unwrap_or_default())
}

fn ffprobe_json(args: &[&str], timeout: u64) -> Result<Value, String> {
    let out = run_ffprobe(args, timeout)?;
    let text = if out.trim().is_empty() { "{}" } else { out.as_str() };
    serde_json::from_str(text).map_err(|e| e.to_string())
}

/// (audio_langs, sub_langs) present in one file — embedded + sidecar subs.
fn file_langs(path: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let (mut audio, mut subs) = (BTreeSet::new(), BTreeSet::new());
    match ffprobe_json(
        &["-v", "error", "-print_format", "json", "-show_streams", path],
        60,
    ) {
        Ok(v) => {
            for st in v.a("streams") {
                let lang = norm_lang(st.at(&["tags", "language"]).as_str().unwrap_or(""));
                match st.s("codec_type") {
                    "audio" => {
                        audio.insert(lang);
                    }
                    "subtitle" => {
                        subs.insert(lang);
                    }
                    _ => {}
                }
            }
        }
        Err(e) => log(&format!("WARN ffprobe {}: {}", path, e)),
    }
    let p = std::path::Path::new(path);
    let fname = p.file_name().and_then(|f| f.to_str()).unwrap_or("");
    let base = match fname.rfind('.') {
        Some(i) => &fname[..i],
        None => fname,
    };
    if let Some(dir) = p.parent() {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for ent in rd.flatten() {
                let f = ent.file_name().to_string_lossy().to_string();
                let fl = f.to_lowercase();
                if f.starts_with(base)
                    && f != fname
                    && [".srt", ".ass", ".ssa", ".sub", ".vtt"]
                        .iter()
                        .any(|e| fl.ends_with(e))
                {
                    let tail = f[base.len()..].trim_matches('.').to_lowercase();
                    let parts: Vec<&str> = tail.split('.').collect();
                    let langs: Vec<&str> = parts[..parts.len().saturating_sub(1)]
                        .iter()
                        .copied()
                        .filter(|t| {
                            (t.len() == 2 || t.len() == 3)
                                && t.chars().all(|c| c.is_alphabetic())
                                && !["sdh", "cc", "hi"].contains(t)
                        })
                        .collect();
                    subs.insert(match langs.first() {
                        Some(l) => norm_lang(l),
                        None => "und".to_string(),
                    });
                }
            }
        }
    }
    (audio, subs)
}

/// Paths of the item's imported file(s); for series, only files added since
/// ~this download started (so a 300-episode library isn't re-probed).
fn item_media_files(instn: &str, iid: &str, kind: &str, since: Option<f64>) -> Vec<String> {
    if kind == "movie" {
        let m = arr_get(instn, &format!("movie/{}", iid)).unwrap_or(Value::Null);
        let p = m.at(&["movieFile", "path"]).as_str().unwrap_or("");
        return if p.is_empty() {
            vec![]
        } else {
            vec![p.to_string()]
        };
    }
    let s = arr_get(instn, &format!("series/{}", iid)).unwrap_or(Value::Null);
    let cutoff = since.map(|t| utc_ts(t - 6.0 * 3600.0));
    let files = arr_get(instn, &format!("episodefile?seriesId={}", iid)).unwrap_or(Value::Null);
    let mut out = Vec::new();
    for f in files.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        if let Some(c) = &cutoff {
            let da = f.s("dateAdded");
            let da19: String = da.chars().take(19).collect();
            if da19.as_str() < c.as_str() {
                continue;
            }
        }
        let p = f.s("path");
        let p = if !p.is_empty() {
            p.to_string()
        } else {
            format!("{}/{}", s.s("path"), f.s("relativePath"))
        };
        out.push(p);
    }
    out.truncate(40);
    out
}

/// Fallback-less Dolby Vision (profile 5) renders green/pink in most players
/// until the media-encoder's automatic rescue re-encodes it to HDR10 — worth a
/// heads-up in the ready ping so the first viewing isn't a bug report.
pub fn dv5_warning(instn: &str, iid: &str, kind: &str, import_at: Option<f64>) -> Option<String> {
    let files = item_media_files(instn, iid, kind, import_at);
    for p in files.iter().take(5) {
        let r = match ffprobe_json(
            &[
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream_side_data_list",
                "-print_format",
                "json",
                p,
            ],
            30,
        ) {
            Ok(v) => v,
            Err(e) => {
                log(&format!("WARN dv5 check {}:{}: {}", instn, iid, e));
                return None;
            }
        };
        let streams = r.a("streams");
        let first = streams.first().cloned().unwrap_or_else(|| json!({}));
        for sd in first.a("side_data_list") {
            if sd.get("dv_profile").and_then(|v| v.as_i64()) == Some(5) {
                return Some(
                    "🎨 This copy uses Dolby Vision profile 5 — colors may look wrong in some \
                     players (browsers especially) for a few hours until the automatic \
                     re-encode finishes."
                        .to_string(),
                );
            }
        }
    }
    None
}

pub struct VerifyRes {
    pub line: String, // "🔎 eng subs ✓ · …"
    pub files: usize,
    pub ok: bool,
    pub gaps_audio: BTreeMap<String, String>, // lang -> "k/n"
    pub gaps_subs: BTreeMap<String, String>,
}

/// Verify require-* tags against the imported files. Returns None when the
/// item has no requirements (the common case: zero extra work). Verification
/// must never block the ready ping — all failure paths log + fall through.
pub fn verify_language(
    instn: &str,
    iid: &str,
    kind: &str,
    import_at: Option<f64>,
) -> Option<VerifyRes> {
    let (want_a, want_s) = requirement_langs(instn, iid, kind);
    if want_a.is_empty() && want_s.is_empty() {
        return None;
    }
    let files = item_media_files(instn, iid, kind, import_at);
    if files.is_empty() {
        return None;
    }
    let mut n = 0usize;
    let mut ok_a: BTreeMap<&String, usize> = want_a.iter().map(|l| (l, 0)).collect();
    let mut ok_s: BTreeMap<&String, usize> = want_s.iter().map(|l| (l, 0)).collect();
    for p in &files {
        let (a, s) = file_langs(p);
        n += 1;
        for l in &want_a {
            if a.contains(l) {
                *ok_a.get_mut(l).unwrap() += 1;
            }
        }
        for l in &want_s {
            if s.contains(l) {
                *ok_s.get_mut(l).unwrap() += 1;
            }
        }
    }
    let mut bits = Vec::new();
    let (mut gaps_a, mut gaps_s) = (BTreeMap::new(), BTreeMap::new());
    for l in &want_a {
        let k = ok_a[l];
        if k == n {
            bits.push(format!("{} audio ✓", l));
        } else {
            bits.push(format!("**{} audio ⚠ {}/{}**", l, k, n));
            gaps_a.insert(l.clone(), format!("{}/{}", k, n));
        }
    }
    for l in &want_s {
        let k = ok_s[l];
        if k == n {
            bits.push(format!("{} subs ✓", l));
        } else {
            bits.push(format!("**{} subs ⚠ {}/{}**", l, k, n));
            gaps_s.insert(l.clone(), format!("{}/{}", k, n));
        }
    }
    let ok = gaps_a.is_empty() && gaps_s.is_empty();
    Some(VerifyRes {
        line: format!("🔎 {}", bits.join(" · ")),
        files: n,
        ok,
        gaps_audio: gaps_a,
        gaps_subs: gaps_s,
    })
}

/// Ask Bazarr to fetch missing subs (main sonarr + radarr only).
pub fn bazarr_kick(instn: &str, iid: &str) -> bool {
    let c = cfg();
    if c.bazarr_key.is_empty() || instn == "sonarr-anime" {
        return false;
    }
    let (path, q): (&str, Vec<(&str, String)>) = if instn == "radarr" {
        (
            "movies",
            vec![
                ("radarrid", iid.to_string()),
                ("action", "search-missing".to_string()),
            ],
        )
    } else {
        (
            "series",
            vec![
                ("seriesid", iid.to_string()),
                ("action", "search-missing".to_string()),
            ],
        )
    };
    let pairs: Vec<(&str, &str)> = q.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let url = format!(
        "{}/api/{}?{}",
        c.bazarr_url,
        path,
        arr_api::http::form_encode(&pairs)
    );
    let req = ureq::agent()
        .request("PATCH", &url)
        .set("X-API-KEY", &c.bazarr_key)
        .timeout(Duration::from_secs(30));
    match req.call() {
        Ok(_) => {
            log(&format!("bazarr search-missing kicked for {}:{}", instn, iid));
            true
        }
        Err(e) => {
            log(&format!("WARN bazarr kick {}:{}: {}", instn, iid, e));
            false
        }
    }
}

/// HMAC-signed POST to a Hermes gateway webhook (event-driven agent wake).
/// Returns true if the gateway accepted the delivery.
fn post_webhook(url: &str, payload: &Value) -> bool {
    let secret = &cfg().langgap_webhook_secret;
    if url.is_empty() || secret.is_empty() {
        return false;
    }
    let body = serde_json::to_string(payload).unwrap_or_default();
    use hmac::Mac;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes())
        .expect("hmac accepts any key length");
    mac.update(body.as_bytes());
    let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
    let req = ureq::agent()
        .post(url)
        .set("Content-Type", "application/json")
        .set("X-Hub-Signature-256", &sig)
        .timeout(Duration::from_secs(30));
    match req.send_string(&body) {
        Ok(r) => matches!(r.status(), 200 | 202),
        Err(e) => {
            let tail = url.rsplit('/').next().unwrap_or(url);
            log(&format!("WARN hermes webhook {}: {}", tail, e));
            false
        }
    }
}

/// Wake the agent for a language gap it needs to fix.
pub fn wake_hermes(instn: &str, iid: &str, kind: &str, title: &str, res: &VerifyRes) -> bool {
    let mut gap_bits: Vec<String> = res
        .gaps_audio
        .iter()
        .map(|(l, c)| format!("{} audio missing ({} files ok)", l, c))
        .collect();
    gap_bits.extend(
        res.gaps_subs
            .iter()
            .map(|(l, c)| format!("{} subs missing ({} files ok)", l, c)),
    );
    let t = if title.is_empty() { "?" } else { title };
    let ok = post_webhook(
        &cfg().langgap_webhook_url,
        &json!({
            "event": "language-gap", "title": t,
            "inst": instn, "iid": iid, "kind": kind,
            "gaps": gap_bits.join("; "), "files": res.files
        }),
    );
    if ok {
        log(&format!(
            "hermes woken for language gap {}:{} ({})",
            instn, iid, title
        ));
    }
    ok
}

/// Wake the agent to hunt an alternate source for a failing download.
pub fn wake_hermes_failed(
    instn: &str,
    iid: &str,
    kind: &str,
    title: &str,
    attempts: &[Value],
) -> bool {
    let fails = attempts.iter().filter(|a| a.s("status") == "failed").count();
    let last = attempts
        .last()
        .map(|a| a.s("title"))
        .filter(|t| !t.is_empty())
        .unwrap_or("?");
    let t = if title.is_empty() { "?" } else { title };
    let ok = post_webhook(
        &cfg().failed_webhook_url,
        &json!({
            "event": "download-failed", "title": t,
            "inst": instn, "iid": iid, "kind": kind,
            "fails": fails, "last": last.chars().take(90).collect::<String>()
        }),
    );
    if ok {
        log(&format!(
            "hermes woken for failing download {}:{} ({}, {} fails)",
            instn, iid, title, fails
        ));
    }
    ok
}

/// Queue-stuck wake (batched triage request).
pub fn wake_hermes_stuck(count: usize, items: &str) -> bool {
    post_webhook(
        &cfg().stuck_webhook_url,
        &json!({ "event": "queue-stuck", "count": count, "items": items }),
    )
}
