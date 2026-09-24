//! Server reboot with a Jellyfin viewer acknowledgement.

use arr_api::{die, jf_api};
use serde_json::Value;
use std::process::Command;

fn viewers(sessions: &Value) -> Option<Vec<String>> {
    Some(
        sessions
            .as_array()?
            .iter()
            .filter(|session| session["NowPlayingItem"].is_object())
            .map(|session| {
                let item = &session["NowPlayingItem"];
                let name = item["Name"].as_str().unwrap_or("Unknown title");
                let title = if item["Type"] == "Episode" {
                    format!(
                        "{} S{:02}E{:02} \"{}\"",
                        item["SeriesName"].as_str().unwrap_or("Unknown series"),
                        item["ParentIndexNumber"].as_u64().unwrap_or(0),
                        item["IndexNumber"].as_u64().unwrap_or(0),
                        name
                    )
                } else if let Some(year) = item["ProductionYear"].as_u64() {
                    format!("{name} ({year})")
                } else {
                    name.to_string()
                };
                let paused = if session["PlayState"]["IsPaused"] == true {
                    " (paused)"
                } else {
                    ""
                };
                format!(
                    "{} is watching {}{} — {} / {}",
                    session["UserName"].as_str().unwrap_or("Unknown user"),
                    title,
                    paused,
                    session["Client"].as_str().unwrap_or("Unknown client"),
                    session["DeviceName"].as_str().unwrap_or("Unknown device")
                )
            })
            .collect(),
    )
}

fn current_viewers() -> Option<Vec<String>> {
    let mut streams = jf_api("/Sessions", &[], 10, "GET", true)
        .as_ref()
        .and_then(viewers)?;
    streams.sort(); // Session ordering isn't a change in who's watching.
    Some(streams)
}

fn report_viewers(streams: &Option<Vec<String>>) {
    match streams {
        Some(streams) => {
            println!("Jellyfin reports {} active stream(s):", streams.len());
            for stream in streams {
                println!("  {stream}");
            }
        }
        None => println!("Jellyfin didn't answer with a session list; can't tell who's watching."),
    }
}

fn systemctl(args: &[&str]) {
    let status = Command::new("systemctl")
        .args(args)
        .status()
        .unwrap_or_else(|err| die(&format!("could not run systemctl: {err}")));
    if !status.success() {
        die(&format!("systemctl {} failed: {status}", args.join(" ")));
    }
}

fn request_reboot() {
    println!("Requesting server reboot...");
    systemctl(&["start", "--no-block", "hermes-reboot.service"]);
    println!("Reboot request accepted (hermes-reboot.service queued).");
}

fn mode(args: &[String]) -> Option<&str> {
    match args {
        [] => Some(""),
        [flag]
            if matches!(
                flag.as_str(),
                "--force" | "--when-free" | "--wait" | "--cancel"
            ) =>
        {
            Some(flag)
        }
        _ => None,
    }
}

pub fn cmd_reboot(args: &[String]) {
    let mode = mode(args).unwrap_or_else(|| {
        die("usage: arr reboot [--force | --when-free | --cancel] (one mode only)")
    });
    if mode == "--cancel" {
        systemctl(&["stop", "hermes-reboot-when-free.service"]);
        println!("Pending reboot-when-free cancelled.");
        return;
    }
    if mode == "--wait" {
        let mut previous = None;
        loop {
            let streams = current_viewers();
            if previous.as_ref() != Some(&streams) {
                report_viewers(&streams);
                previous = Some(streams.clone());
            }
            if streams.as_ref().is_some_and(|s| s.is_empty()) {
                request_reboot();
                return;
            }
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    }
    let streams = current_viewers();
    report_viewers(&streams);
    if streams.as_ref().is_some_and(|s| s.is_empty()) {
        request_reboot();
    } else if mode == "--when-free" {
        systemctl(&["start", "--no-block", "hermes-reboot-when-free.service"]);
        println!("The server will reboot once Jellyfin reports no streams remain.");
        println!("Check progress: systemctl status hermes-reboot-when-free");
        println!("Cancel: arr reboot --cancel");
    } else if mode == "--force" {
        println!("Forcing reboot; any streams above will be interrupted.");
        request_reboot();
    } else {
        die("Rebooting may interrupt viewers. Re-run with 'arr reboot --when-free' to wait, or 'arr reboot --force' to reboot anyway.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn formats_viewers_and_skips_idle_sessions() {
        let sessions = json!([
            {"UserName": "Alice", "NowPlayingItem": {
                "Type": "Episode", "SeriesName": "Example", "ParentIndexNumber": 1,
                "IndexNumber": 2, "Name": "Pilot"
            }, "PlayState": {"IsPaused": true}, "Client": "Jellyfin Web", "DeviceName": "Firefox"},
            {"UserName": "Bob", "NowPlayingItem": {"Type": "Movie", "Name": "Arrival", "ProductionYear": 2016},
             "Client": "Jellyfin", "DeviceName": "TV"},
            {"UserName": "Idle", "NowPlayingItem": null}
        ]);
        assert_eq!(
            viewers(&sessions).unwrap(),
            vec![
                "Alice is watching Example S01E02 \"Pilot\" (paused) — Jellyfin Web / Firefox",
                "Bob is watching Arrival (2016) — Jellyfin / TV"
            ]
        );
    }

    #[test]
    fn accepts_only_one_known_mode() {
        assert_eq!(mode(&[]), Some(""));
        for flag in ["--force", "--when-free", "--wait", "--cancel"] {
            let args = vec![flag.to_string()];
            assert_eq!(mode(&args), Some(flag));
            assert_eq!(mode(&[flag.to_string(), flag.to_string()]), None);
        }
        assert_eq!(mode(&["--force".into(), "--when-free".into()]), None);
        assert_eq!(mode(&["--unknown".into()]), None);
    }

    #[test]
    fn distinguishes_empty_sessions_from_invalid_response() {
        assert_eq!(viewers(&json!([])), Some(vec![]));
        assert_eq!(viewers(&json!({"error": "unavailable"})), None);
        assert_eq!(viewers(&Value::Null), None);
    }
}
