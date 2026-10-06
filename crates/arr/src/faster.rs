//! ETA reporting and an opt-in, verified Radarr/SAB replacement.
use arr_api::{die, fmt_gb, pop_flags, resolve_id, try_api, JsonExt};
use serde_json::Value;
use std::time::{Duration, Instant};

const SLOW_SECONDS: f64 = 600.0;
const MIN_SAVING_SECONDS: f64 = 300.0;
const MAX_REMAINING_FRACTION: f64 = 0.5;
const VERIFY_SECONDS: u64 = 150;
const MIN_RECEIVED_MB: f64 = 100.0;
const MAX_MISSING_FRACTION: f64 = 0.05;
const MAX_TRIES: usize = 5;
const SPEED_SAMPLE_SECONDS: u64 = 30;
const JUNK_FORMAT_SCORE: i64 = -10_000;
type Result<T> = std::result::Result<T, String>;

fn arr(method: &str, path: &str) -> Result<Value> {
    try_api("radarr", method, path, None, 10)
        .map(|v| v.unwrap_or(Value::Null))
        .map_err(|e| format!("{e:?}"))
}
fn sab(params: &[(&str, &str)]) -> Result<Value> {
    let v = crate::acquire::sab_try_get_timeout("queue", params, 10)?;
    if v.get("status") == Some(&Value::Bool(false)) {
        return Err(format!("SAB: {}", v.s("error")));
    }
    Ok(v)
}
fn sab_action(name: &str, id: &str) -> Result<()> {
    let mut params = vec![("name", name), ("value", id)];
    if name == "delete" {
        params.push(("del_files", "1"));
    }
    if !sab(&params)?.b("status") {
        return Err(format!("SAB {name} unconfirmed"));
    }
    Ok(())
}
fn set_priority(id: &str, priority: i64) -> Result<()> {
    let v = sab(&[
        ("name", "priority"),
        ("value", id),
        ("value2", &priority.to_string()),
    ])?;
    if !v
        .get("position")
        .and_then(Value::as_i64)
        .is_some_and(|p| p >= 0)
    {
        return Err("SAB priority change unconfirmed".into());
    }
    Ok(())
}
fn priority(slot: &Value) -> Result<i64> {
    match slot.s("priority") {
        "Force" => Ok(2),
        "High" => Ok(1),
        "Normal" => Ok(0),
        "Low" => Ok(-1),
        _ => Err("unknown SAB priority".into()),
    }
}
fn number(v: &Value, key: &str) -> Option<f64> {
    let n = v.get(key)?.as_f64().or_else(|| v.s(key).parse().ok())?;
    n.is_finite().then_some(n)
}
fn slot(q: &Value, id: &str) -> Result<Value> {
    q.at(&["queue"])
        .a("slots")
        .iter()
        .find(|s| s.s("nzo_id").eq_ignore_ascii_case(id))
        .cloned()
        .ok_or("job absent from SAB queue".into())
}
fn queue(iid: i64) -> Result<Vec<Value>> {
    Ok(arr("GET", "/queue?pageSize=1000")?
        .a("records")
        .iter()
        .filter(|r| r.i("movieId") == iid)
        .cloned()
        .collect())
}
struct Snapshot {
    old: Value,
    slot: Value,
    left: f64,
    speed: Option<f64>,
}
impl Snapshot {
    fn slow(&self) -> bool {
        self.speed.is_none_or(|s| self.left / s > SLOW_SECONDS)
    }
}
fn snapshot(iid: i64) -> Result<Snapshot> {
    let rows = queue(iid)?;
    if rows.len() != 1 {
        return Err(format!(
            "need exactly one active movie job; found {}",
            rows.len()
        ));
    }
    let old = rows[0].clone();
    if old.s("protocol") != "usenet" || old.s("downloadId").is_empty() {
        return Err("faster supports an unfinished SAB usenet job only".into());
    }
    let q = sab(&[("limit", "1000")])?;
    let slot = slot(&q, old.s("downloadId"))?;
    let left = number(&slot, "mbleft").unwrap_or(0.0) * 1024.0 * 1024.0;
    if !matches!(slot.s("status"), "Downloading" | "Queued")
        || q.at(&["queue"]).b("paused")
        || left <= 0.0
    {
        return Err("current SAB job is paused, finished or unhealthy; left unchanged".into());
    }
    let speed = number(q.at(&["queue"]), "kbpersec")
        .filter(|s| *s > 0.0)
        .map(|s| s * 1024.0);
    Ok(Snapshot {
        old,
        slot,
        left,
        speed,
    })
}
fn seconds(r: &Value) -> Option<f64> {
    if r.s("status") != "downloading" {
        return None;
    }
    let parts: Vec<f64> = r
        .s("timeleft")
        .split(':')
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    if parts.len() != 3 {
        return None;
    }
    let t = parts[0] * 3600.0 + parts[1] * 60.0 + parts[2];
    (t.is_finite() && t > 0.0 && t < 365.0 * 86400.0).then_some(t)
}
fn eta(left: f64, speed: Option<f64>) -> String {
    speed
        .map(|s| {
            format!(
                "~{} at current speed",
                crate::browse::fmt_age((left / s).ceil() as i64)
            )
        })
        .unwrap_or_else(|| "unknown (no measured download speed)".into())
}
pub(crate) fn print_eta(r: &Value) {
    let label = seconds(r)
        .map(|s| format!("~{}", crate::browse::fmt_age(s as i64)))
        .unwrap_or_else(|| "unknown (queued, paused or no measured speed)".into());
    println!("        download ETA: {label}; import/processing time extra");
}
fn quality(r: &Value) -> &Value {
    r.at(&["quality", "quality"])
}
fn acceptable(r: &Value, old: &Value, iid: i64) -> bool {
    let q = quality(r);
    let source_ok = matches!(q.s("source"), "webdl" | "webrip" | "bluray");
    let floor = quality(old).i("resolution").min(1080).max(1);
    let text = format!("{} {:?}", r.s("title"), r.a("customFormats")).to_lowercase();
    r.i("mappedMovieId") == iid
        && r.s("protocol") == "usenet"
        && r.b("downloadAllowed")
        && !r.s("guid").is_empty()
        && r.i("indexerId") > 0
        && r.i("size") > 0
        && !crate::acquire::is_disc_record(r)
        && r.i("customFormatScore") > JUNK_FORMAT_SCORE
        && source_ok
        && q.i("resolution") >= floor
        && ![
            "upscale",
            "lq",
            "workprint",
            "password",
            "encrypted",
            ".exe",
            ".scr",
            "bd50",
            "bd25",
            "bd66",
            "bd100",
        ]
        .iter()
        .any(|s| text.contains(s))
        && r.a("rejections").iter().all(|v| {
            let reason = v.as_str().unwrap_or("").to_lowercase();
            reason.starts_with("release in queue")
                || reason.contains("larger than maximum")
                || (reason.contains("quality")
                    && ["not wanted", "already", "existing"]
                        .iter()
                        .any(|s| reason.contains(s)))
        })
}
/// Safe faster releases, best first by Radarr's own ranking (ReleaseWeight 0 = best).
fn ranked<'a>(releases: &'a [Value], s: &Snapshot, iid: i64) -> Vec<&'a Value> {
    let mut out: Vec<&Value> = releases
        .iter()
        .filter(|r| s.slow() && acceptable(r, &s.old, iid))
        .filter(|r| {
            r.s("title") != s.old.s("title") && r.f("size") <= s.left * MAX_REMAINING_FRACTION
        })
        .filter(|r| {
            s.speed
                .is_none_or(|speed| (s.left - r.f("size")) / speed >= MIN_SAVING_SECONDS)
        })
        .filter(|r| r.get("releaseWeight").and_then(Value::as_i64).is_some())
        .collect();
    out.sort_by_key(|r| r.i("releaseWeight"));
    out
}
fn select<'a>(releases: &'a [Value], s: &Snapshot, iid: i64) -> Option<&'a Value> {
    ranked(releases, s, iid).into_iter().next()
}
fn releases(iid: i64, timeout: u64) -> Result<Vec<Value>> {
    let v = try_api(
        "radarr",
        "GET",
        &format!("/release?movieId={iid}"),
        None,
        timeout,
    )
    .map_err(|e| format!("{e:?}"))?
    .unwrap_or(Value::Null);
    v.as_array()
        .cloned()
        .ok_or("invalid release response".into())
}
fn choice(s: &Snapshot, r: &Value, iid: i64) {
    println!(
        "  Faster: {}GB {} | ETA {} -> arr radarr faster {iid} --yes",
        fmt_gb(r.i("size")),
        quality(r).s("name"),
        eta(r.f("size"), s.speed)
    );
}
/// Only acquisition searches indexers; status/where use cheap queue ETA + a pointer.
pub(crate) fn report(svc: &str, iid: i64, records: &[Value], acquisition: bool) {
    for r in records {
        print_eta(r);
    }
    if svc != "radarr" {
        return;
    }
    if !acquisition {
        if records
            .iter()
            .any(|r| seconds(r).is_some_and(|t| t > SLOW_SECONDS))
        {
            println!("  faster options: arr radarr faster {iid} --dry-run");
        }
        return;
    }
    let Ok(s) = snapshot(iid) else {
        return;
    };
    println!(
        "  current: {}GB remaining; download ETA {}",
        fmt_gb(s.left as i64),
        eta(s.left, s.speed)
    );
    if !s.slow() {
        return;
    }
    match releases(iid, 30) {
        Ok(rels) => match select(&rels, &s, iid) {
            Some(r) => {
                choice(&s, r, iid);
                println!(
                    "  → tell the requester: full quality takes {}; a {}GB version would take {}. Offer it; if they want it, run the faster command above.",
                    eta(s.left, s.speed), fmt_gb(r.i("size")), eta(r.f("size"), s.speed)
                );
            }
            None => println!(
                "  → tell the requester the ETA ({}); no faster version exists",
                eta(s.left, s.speed)
            ),
        },
        Err(_) => println!("  faster lookup unavailable; retry: arr radarr faster {iid} --dry-run"),
    }
}
pub(crate) fn report_item(svc: &str, iid: i64) {
    let field = if svc.starts_with("sonarr") {
        "seriesId"
    } else {
        "movieId"
    };
    if let Ok(Some(q)) = try_api(svc, "GET", "/queue?pageSize=1000", None, 10) {
        let rows: Vec<_> = q
            .a("records")
            .iter()
            .filter(|r| r.i(field) == iid)
            .cloned()
            .collect();
        report(svc, iid, &rows, false);
    }
}
fn replacement(iid: i64, s: &Snapshot) -> Result<Option<Value>> {
    let rows: Vec<_> = queue(iid)?
        .into_iter()
        .filter(|r| r.s("downloadId") != s.old.s("downloadId"))
        .collect();
    match rows.as_slice() {
        [] => Ok(None),
        [r] if !r.s("downloadId").is_empty() => Ok(Some(r.clone())),
        _ => Err("ambiguous replacement jobs; left untouched".into()),
    }
}
/// SAB counts missing articles as processed, so a dead (DMCA'd) release also
/// shrinks `mbleft`. Proof of a live download is MB actually *received*:
/// processed minus newly missing, with missing a small share of processed.
fn observed_movement(
    baseline: &mut Option<(f64, f64)>,
    left: f64,
    missing: f64,
    status: &str,
) -> Result<bool> {
    let Some((left0, missing0)) = *baseline else {
        *baseline = Some((left, missing));
        return Ok(false);
    };
    let (processed, lost) = (left0 - left, missing - missing0);
    if processed >= MIN_RECEIVED_MB && lost > processed * MAX_MISSING_FRACTION {
        return Err(format!(
            "replacement is missing articles ({lost:.0} of {processed:.0} MB)"
        ));
    }
    Ok(status == "Downloading" && processed - lost >= MIN_RECEIVED_MB)
}
fn wait_moving(iid: i64, s: &Snapshot) -> Result<()> {
    let start = Instant::now();
    let (mut baseline, mut download_id) = (None, String::new());
    while start.elapsed().as_secs() < VERIFY_SECONDS {
        if let Some(r) = replacement(iid, s)? {
            // Gone from the queue within seconds = SAB aborted it as incomplete
            // (DMCA'd/expired articles); a multi-GB release can't finish this fast.
            let slot = slot(&sab(&[("limit", "1000")])?, r.s("downloadId"))
                .map_err(|_| "SAB aborted it: articles missing on usenet".to_string())?;
            if matches!(slot.s("status"), "Paused" | "Failed") {
                return Err("replacement paused or failed".into());
            }
            if download_id != r.s("downloadId") {
                baseline = None;
                download_id = r.s("downloadId").into();
            }
            let first = baseline.is_none();
            let left = number(&slot, "mbleft").ok_or("replacement byte count unavailable")?;
            let missing =
                number(&slot, "mbmissing").ok_or("replacement missing-byte count unavailable")?;
            if observed_movement(&mut baseline, left, missing, slot.s("status"))? {
                return Ok(());
            }
            if first {
                set_priority(slot.s("nzo_id"), 2)?;
            } // Force moves it to the front; no self-switch.
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    Err(format!(
        "replacement did not receive {MIN_RECEIVED_MB} MB within {VERIFY_SECONDS}s"
    ))
}
/// (bytes left, bytes/s) of the replacement over SPEED_SAMPLE_SECONDS, while
/// it has the connection to itself (the original is paused).
fn measured_speed(iid: i64, s: &Snapshot) -> Result<(f64, f64)> {
    let left = |r: &Value| -> Result<f64> {
        let slot = slot(&sab(&[("limit", "1000")])?, r.s("downloadId"))
            .map_err(|_| "SAB aborted it: articles missing on usenet".to_string())?;
        Ok(number(&slot, "mbleft").ok_or("replacement byte count unavailable")? * 1048576.0)
    };
    let r = replacement(iid, s)?.ok_or("replacement left the Radarr queue")?;
    let (start, t0) = (left(&r)?, Instant::now());
    std::thread::sleep(Duration::from_secs(SPEED_SAMPLE_SECONDS));
    let end = left(&r)?;
    if end >= start {
        return Err(format!(
            "replacement stalled during the {SPEED_SAMPLE_SECONDS}s speed sample"
        ));
    }
    Ok((end, (start - end) / t0.elapsed().as_secs_f64()))
}
fn remove_arr(r: &Value, client: bool, blocklist: bool) -> Result<()> {
    arr(
        "DELETE",
        &format!(
            "/queue/{}?removeFromClient={client}&blocklist={blocklist}&skipRedownload=true",
            r.i("id")
        ),
    )
    .map(|_| ())
}
/// `Err((true, _))`: this candidate didn't work out (indexer wouldn't serve it, or
/// it was dead and is now blocklisted); the original is resumed, try the next.
fn swap(iid: i64, s: &Snapshot, release: &Value) -> std::result::Result<(), (bool, String)> {
    let id = s.slot.s("nzo_id");
    let old_priority = priority(&s.slot).map_err(|e| (false, e))?;
    let (mut paused, mut submitted, mut bad) = (false, false, false);
    let prepared = (|| -> Result<()> {
        // Force downloads even while Paused; changing priority can unpause.
        set_priority(id, 0)?;
        sab_action("pause", id)?;
        paused = true;
        crate::acquire::submit_release("radarr", release).map_err(|e| match e {
            arr_api::ApiError::Http { code, detail } => format!(
                "indexer wouldn't serve it ({code}: {})",
                serde_json::from_str::<Value>(&detail)
                    .map(|v| v.s("message").to_string())
                    .unwrap_or(detail)
            ),
            e => format!("submit: {e:?}"),
        })?;
        submitted = true;
        bad = true;
        wait_moving(iid, s)?;
        // The ETA estimate assumed the original's speed; a post served mostly by
        // the backup provider can be far slower. Measure before committing.
        let speed = measured_speed(iid, s)?;
        let (left, old_eta) = (speed.0, s.speed.map(|v| s.left / v));
        if old_eta.is_some_and(|t| left / speed.1 + MIN_SAVING_SECONDS > t) {
            bad = false; // it works, it's just slow here: no blocklist
            return Err(format!(
                "only {:.1} MB/s here (~{} vs ~{} for the original); not faster",
                speed.1 / 1048576.0,
                crate::browse::fmt_age((left / speed.1) as i64),
                crate::browse::fmt_age(old_eta.unwrap() as i64)
            ));
        }
        Ok(())
    })();
    if let Err(e) = prepared {
        let cleanup = replacement(iid, s).and_then(|r| match r {
            Some(r) => remove_arr(&r, true, bad),
            None if !submitted => Ok(()),
            None => Err("replacement not visible in Radarr; check for a delayed grab".into()),
        });
        let resume = set_priority(id, old_priority).and_then(|_| sab_action("resume", id));
        return match (cleanup, resume) {
            (Ok(()), Ok(())) => Err((paused, e)),
            (cleanup, resume) => Err((
                false,
                format!("{e}; replacement cleanup: {cleanup:?}; original resume: {resume:?}"),
            )),
        };
    }
    // Ignore records downloadIgnored for the notifier; plain client removal
    // records no history. From here keep the verified replacement on errors.
    remove_arr(&s.old, false, false).map_err(|e| {
        (
            false,
            format!("ignore old job uncertain: {e}; keep replacement, inspect old {id}"),
        )
    })?;
    sab_action("delete", id).map_err(|e| {
        (
            false,
            format!("replacement moving; old {id} ignored but deletion unconfirmed: {e}"),
        )
    })?;
    println!(
        "  switched to {}GB {} | ETA {}; original cancelled without blocklisting",
        fmt_gb(release.i("size")),
        quality(release).s("name"),
        eta(release.f("size"), s.speed)
    );
    Ok(())
}
pub fn cmd_faster(svc: &str, args: &[String]) {
    if svc != "radarr" {
        die("faster: Radarr/SAB only");
    }
    let (flags, rest) = pop_flags(args, &[("--dry-run", 0), ("--yes", 0)]);
    if rest.len() != 1 || (flags.has("--yes") && flags.has("--dry-run")) {
        die("usage: arr radarr faster '<item>' [--dry-run | --yes]");
    }
    let iid = resolve_id(svc, &rest[0]);
    let result = (|| -> Result<()> {
        let before = snapshot(iid)?;
        if !before.slow() {
            println!(
                "already within 10 minutes; keeping current download (ETA {})",
                eta(before.left, before.speed)
            );
            return Ok(());
        }
        let rels = releases(iid, 300)?;
        // Search can take minutes: recheck the same job and remaining bytes.
        let s = snapshot(iid)?;
        if s.old.s("downloadId") != before.old.s("downloadId") {
            return Err("active job changed; nothing changed".into());
        }
        let candidates = ranked(&rels, &s, iid);
        let Some(best) = candidates.first() else {
            println!(
                "no safe faster alternative; keeping current download (ETA {})",
                eta(s.left, s.speed)
            );
            return Ok(());
        };
        println!(
            "  current ETA {}; choice: {}",
            eta(s.left, s.speed),
            best.s("title")
        );
        choice(&s, best, iid);
        if !flags.has("--yes") {
            println!("  dry-run: nothing changed; --yes confirms the switch");
            return Ok(());
        }
        // Popular titles often have DMCA'd releases: a dead one is blocklisted
        // and the next-best is tried, the original resumed in between.
        let mut tried = std::collections::HashSet::new();
        let fresh = candidates
            .iter()
            .filter(|r| tried.insert(r.s("title").to_string()));
        for r in fresh.take(MAX_TRIES) {
            match swap(iid, &s, r) {
                Ok(()) => return Ok(()),
                Err((true, e)) => {
                    println!("  ✗ {} — {e}; skipped, original resumed", r.s("title"))
                }
                Err((false, e)) => return Err(e),
            }
        }
        println!(
            "none of the {} faster releases tried would download (reasons above); kept the original (ETA {}). Hand-grabbing them won't go better",
            tried.len(),
            eta(s.left, s.speed)
        );
        Ok(())
    })();
    if let Err(e) = result {
        die(&format!("faster: {e}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn fixture() -> (Snapshot, Value) {
        let quality = json!({"quality":{"resolution":2160,"source":"bluray","name":"Remux-2160p"}});
        let s = Snapshot {
            old: json!({"title":"big","quality":quality}),
            slot: Value::Null,
            left: 2000.0 * 1024.0 * 1024.0,
            speed: Some(1024.0 * 1024.0),
        };
        let r = json!({"title":"small","mappedMovieId":1,"protocol":"usenet","downloadAllowed":true,"guid":"g","indexerId":1,"size":800*1024*1024,"quality":quality,"releaseWeight":2});
        (s, r)
    }
    #[test]
    fn queue_rejections_allowed_but_undersized_rejected() {
        let (s, mut r) = fixture();
        let reasons = json!([
            "Release in queue is of equal or higher preference: Bluray-1080p v1",
            "Release in queue has an equal or higher Custom Format score: 0"
        ]);
        r["rejections"] = reasons;
        assert!(acceptable(&r, &s.old, 1));
        r["rejections"].as_array_mut().unwrap().push(json!(
            "2.4 GB is smaller than minimum allowed 6.5 GB (for Neighboring Sounds)"
        ));
        assert!(!acceptable(&r, &s.old, 1));
    }
    #[test]
    fn selection_ranking_and_safety() {
        let (mut s, r) = fixture();
        let mut better = r.clone();
        better["releaseWeight"] = json!(0);
        better["size"] = json!(900 * 1024 * 1024);
        assert_eq!(
            select(&[r.clone(), better], &s, 1)
                .unwrap()
                .i("releaseWeight"),
            0
        );
        for (key, value) in [
            ("size", json!(0)),
            ("mappedMovieId", json!(2)),
            ("protocol", json!("torrent")),
            ("title", json!("Film.BDMV")),
            ("customFormatScore", json!(-10000)),
            ("rejections", json!(["Release is blocklisted"])),
        ] {
            let mut bad = r.clone();
            bad[key] = value;
            assert!(!acceptable(&bad, &s.old, 1));
        }
        s.left /= 2.0;
        assert!(select(std::slice::from_ref(&r), &s, 1).is_none());
        s.left *= 2.0;
        s.speed = None;
        assert!(select(&[r], &s, 1).is_some());
    }
    #[test]
    fn eta_and_movement_require_observed_speed() {
        assert_eq!(
            seconds(&json!({"status":"downloading","timeleft":"01:02:03"})),
            Some(3723.0)
        );
        assert_eq!(
            seconds(&json!({"status":"paused","timeleft":"01:02:03"})),
            None
        );
        assert!(eta(100.0, None).starts_with("unknown"));
        let mut b = None;
        let mut moved = |left, missing, status| observed_movement(&mut b, left, missing, status);
        assert!(!moved(1000.0, 0.0, "Downloading").unwrap()); // baseline
        assert!(!moved(950.0, 0.0, "Downloading").unwrap()); // too little yet
        assert!(!moved(850.0, 0.0, "Paused").unwrap());
        assert!(moved(850.0, 2.0, "Downloading").unwrap());
        // a dead release burns through mbleft while every article goes missing
        let mut b = None;
        observed_movement(&mut b, 1000.0, 0.0, "Downloading").unwrap();
        assert!(observed_movement(&mut b, 800.0, 190.0, "Downloading").is_err());
    }
}
