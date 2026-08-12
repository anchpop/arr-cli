//! Discord embed rendering: progress bars ▰▱, per-episode charts ✅📥⏳❌🔁,
//! state colours. Rendered strings are what requesters see — change them
//! deliberately, and keep every state reachable from the selftest.

use std::collections::BTreeMap;

use arr_api::JsonExt;
use serde_json::{json, Value};

use crate::config::BAR_WIDTH;
use crate::util::py_round;

pub fn render_bar(pct: i64) -> String {
    let pct = pct.clamp(0, 100);
    let filled = py_round(pct as f64 / 100.0 * BAR_WIDTH as f64);
    "▰".repeat(filled as usize) + &"▱".repeat((BAR_WIDTH - filled).max(0) as usize)
}

pub fn fmt_eta(timeleft: &str) -> String {
    // Sonarr/Radarr give "HH:MM:SS" (or huge) — turn into a friendly hint.
    if timeleft.is_empty() || timeleft.starts_with("00:00:00") {
        return String::new();
    }
    let parts: Vec<&str> = timeleft.split(':').collect();
    let (h, m) = match (
        parts.first().and_then(|p| p.parse::<i64>().ok()),
        parts.get(1).and_then(|p| p.parse::<i64>().ok()),
    ) {
        (Some(h), Some(m)) => (h, m),
        _ => return String::new(),
    };
    if h >= 24 {
        return " · a while left".to_string();
    }
    if h != 0 {
        return format!(" · ~{}h {}m left", h, m);
    }
    if m != 0 {
        return format!(" · ~{} min left", m);
    }
    " · almost done".to_string()
}

const MAX_ATTEMPT_LINES: usize = 6;

fn attempt_line(a: &Value, timeleft: &str, importing: bool, active: bool) -> String {
    let pct = a.i("pct");
    if a.s("status") == "failed" {
        return format!("{}  ✗ failed at {}%", render_bar(pct), pct);
    }
    if active && importing {
        return format!("{}  📥 importing…", render_bar(100));
    }
    format!(
        "{}  {}%{}",
        render_bar(pct),
        pct,
        if active { fmt_eta(timeleft) } else { String::new() }
    )
}

/// Render attempts, collapsing older failed ones if there are many.
fn bars_block(attempts: &[Value], timeleft: &str, importing: bool) -> String {
    let (shown, prefix) = if attempts.len() <= MAX_ATTEMPT_LINES {
        (attempts, String::new())
    } else {
        let hidden = attempts.len() - (MAX_ATTEMPT_LINES - 1);
        (
            &attempts[hidden..],
            format!("…+{} earlier failed attempt(s)\n", hidden),
        )
    };
    let lines: Vec<String> = shown
        .iter()
        .enumerate()
        .map(|(i, a)| attempt_line(a, timeleft, importing, i == shown.len() - 1))
        .collect();
    prefix + &lines.join("\n")
}

// Discord embed colours per state (downloading=blurple, adding=amber,
// failed=red, ready=green).
fn embed_color(state: &str) -> i64 {
    match state {
        "downloading" => 0x5865F2,
        "adding" => 0xFAA61A,
        "failed" => 0xED4245,
        _ => 0x57F287, // ready
    }
}

// Per-episode status glyphs for the multi-file (season) chart. "upgrading"
// (🔁) means the episode is already on disk and watchable — the queue entry
// is only a quality upgrade — so it counts as done everywhere.
fn ep_emoji(st: &str) -> &'static str {
    match st {
        "done" => "✅",
        "upgrading" => "🔁",
        "importing" => "📥",
        "downloading" => "⏳",
        "failed" => "❌",
        _ => "▫️",
    }
}

/// An episode the requester could watch right now.
pub fn watchable(st: &str) -> bool {
    st == "done" || st == "upgrading"
}

/// A season currently downloading as ONE download (season pack): rendered as
/// its own byte bar instead of N copies of the same glyph.
pub struct SeasonLine {
    pub pct: i64,
    pub importing: bool,
}

pub struct Chart {
    pub lines: Vec<String>,
    pub done: i64,
    pub total: i64,
}

/// ep_status: {"<season>x<ep>": status} -> (lines grouped by season, done, total).
/// Seasons present in `season_bars` render as a byte bar; the rest as glyphs.
pub fn render_chart(
    ep_status: &BTreeMap<String, String>,
    season_bars: Option<&BTreeMap<i64, SeasonLine>>,
) -> Chart {
    let mut by_season: BTreeMap<i64, Vec<(i64, String)>> = BTreeMap::new();
    for (k, st) in ep_status {
        let mut it = k.split('x');
        let s: i64 = it.next().unwrap_or("0").parse().unwrap_or(0);
        let e: i64 = it.next().unwrap_or("0").parse().unwrap_or(0);
        by_season.entry(s).or_default().push((e, st.clone()));
    }
    let mut lines = Vec::new();
    for (s, mut eps) in by_season {
        if let Some(sl) = season_bars.and_then(|m| m.get(&s)) {
            let tail = if sl.importing {
                "📥 importing…".to_string()
            } else {
                format!("{}% — season pack", sl.pct)
            };
            lines.push(format!("S{}: {}  {}", s, render_bar(sl.pct), tail));
            continue;
        }
        eps.sort();
        let glyphs: String = eps.iter().map(|(_, st)| ep_emoji(st)).collect();
        lines.push(format!("S{}: {}", s, glyphs));
    }
    let done = ep_status.values().filter(|st| watchable(st)).count() as i64;
    Chart {
        lines,
        done,
        total: ep_status.len() as i64,
    }
}

/// Return the Discord message body (an embed) for the given state. We edit
/// this same embed in place; the poster thumbnail (a TMDB url from the arr)
/// makes it look like a proper 'now downloading' card.
///
/// For a multi-file (season) download, pass `chart`: we render a season
/// completion bar + a per-episode status grid (✅📥⏳❌).
#[allow(clippy::too_many_arguments)]
pub fn build_embed(
    state: &str,
    title: &str,
    year: Option<i64>,
    poster: Option<&str>,
    attempts: &[Value],
    timeleft: &str,
    importing: bool,
    chart: Option<&Chart>,
    verify: Option<&str>,
) -> Value {
    let mut desc;
    if let Some(c) = chart {
        let pct = if c.total != 0 {
            py_round(c.done as f64 / c.total as f64 * 100.0)
        } else {
            0
        };
        let head = match state {
            "downloading" => "⏳ Downloading",
            "adding" => "📥 Adding to Jellyfin",
            "ready" => "✅ Ready to watch on Jellyfin",
            _ => "❌ Download issues", // failed
        };
        let eta = if state == "downloading" {
            fmt_eta(timeleft)
        } else {
            String::new()
        };
        desc = format!(
            "{} · **{}/{}** episodes{}\n{}  {}%\n{}",
            head,
            c.done,
            c.total,
            eta,
            render_bar(pct),
            pct,
            c.lines.join("\n")
        );
    } else {
        let n = attempts.len();
        desc = match state {
            "downloading" => {
                let head = if n > 1 {
                    format!("⏳ Downloading…  ·  attempt {}", n)
                } else {
                    "⏳ Downloading…".to_string()
                };
                format!("{}\n{}", head, bars_block(attempts, timeleft, importing))
            }
            "failed" => format!(
                "❌ Download failed.\n{}\n_Looking for another release…_",
                bars_block(attempts, "", false)
            ),
            "adding" => {
                let tail = if n > 1 {
                    format!("  ·  after {} tries", n)
                } else {
                    String::new()
                };
                format!("📥 Downloaded — adding to Jellyfin…{}", tail)
            }
            _ => {
                // ready
                let tail = if n > 1 {
                    format!("  ·  after {} tries", n)
                } else {
                    String::new()
                };
                format!("✅ Ready to watch on Jellyfin!{}", tail)
            }
        };
    }
    if state == "ready" {
        if let Some(v) = verify {
            desc.push('\n');
            desc.push_str(v);
        }
    }
    let full_title = match year {
        Some(y) if y != 0 => format!("{} ({})", title, y),
        _ => title.to_string(),
    };
    let mut embed = json!({
        "title": full_title,
        "description": desc,
        "color": embed_color(state),
    });
    if let Some(p) = poster {
        if !p.is_empty() {
            embed["thumbnail"] = json!({ "url": p });
        }
    }
    // content:"" so editing a pre-embed (plain-text) message clears the old text.
    json!({ "content": "", "embeds": [embed] })
}
