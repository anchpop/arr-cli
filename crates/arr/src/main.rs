//! arr — Sonarr/Radarr/Prowlarr/SABnzbd CLI (Rust port of arr.py).
//!
//! Dispatch mirrors arr.py's main(): `arr <svc> <cmd> ...` for the arr
//! services, plus the sab/jellyfin/seerr/bazarr command families and the
//! top-level `arr delete` / `arr queue` conveniences. Output strings, flags
//! and exit codes have parsers (Hermes' skills and cron watchdogs) — evolve
//! them additively; grep `skills/` before rewording existing lines.

use arr_api::die;

mod acquire;
mod browse;
mod disk;
mod integrations;
mod locate;
mod policy;
mod usage;

fn main() {
    // Python gets SIG_DFL for SIGPIPE by default; Rust ignores it, which turns
    // `arr ... | head` into a broken-pipe panic. Restore default.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || matches!(argv[0].as_str(), "-h" | "--help" | "help") {
        println!("{}", usage::usage());
        std::process::exit(if argv.is_empty() { 1 } else { 0 });
    }
    let svc = argv[0].as_str();
    let rest: Vec<String> = argv[1..].to_vec();
    match svc {
        "sab" => {
            if rest.is_empty() {
                die("sab: queue|status|add|prio|history|cleanup");
            }
            let args = &rest[1..];
            match rest[0].as_str() {
                "queue" => disk::sab_queue(args),
                "status" => disk::sab_status(args),
                "add" => disk::sab_add(args),
                "prio" => disk::sab_prio(args),
                "history" => disk::sab_history(args),
                "cleanup" => disk::sab_cleanup(args),
                c => die(&format!("unknown sab command '{}'", c)),
            }
        }
        "jellyfin" => {
            if rest.is_empty() {
                die("jellyfin: unwatched|has|refresh");
            }
            let args = &rest[1..];
            match rest[0].as_str() {
                "unwatched" => disk::cmd_jf_unwatched(args),
                "has" => integrations::cmd_jf_has(args),
                "refresh" => integrations::cmd_jf_refresh(args),
                c => die(&format!("unknown jellyfin command '{}'", c)),
            }
        }
        "seerr" => {
            if rest.is_empty() {
                die("seerr: requests|request|unfulfilled");
            }
            let args = &rest[1..];
            match rest[0].as_str() {
                "requests" => integrations::seerr_requests(args),
                "request" => integrations::seerr_request(args),
                "unfulfilled" => integrations::seerr_unfulfilled(args),
                c => die(&format!("unknown seerr command '{}'", c)),
            }
        }
        "bazarr" => {
            if rest.is_empty() {
                die("bazarr: status|wanted|search|raw");
            }
            let args = &rest[1..];
            match rest[0].as_str() {
                "status" => integrations::bazarr_status(args),
                "wanted" => integrations::bazarr_wanted(args),
                "search" => integrations::bazarr_search(args),
                "raw" => integrations::bazarr_raw(args),
                c => die(&format!("unknown bazarr command '{}'", c)),
            }
        }
        "delete" => disk::cmd_delete_auto(&rest),
        "queue" => acquire::cmd_queue_overview(&rest),
        "where" => locate::cmd_where(&rest),
        _ => {
            if arr_api::svc_cfg(svc).is_none() {
                // Service-optional dispatch: `arr status 'Lycoris Recoil'`.
                // Nobody thinks in services, and guessing the wrong one returns
                // "no match" — indistinguishable from "not in the library".
                if locate::is_item_command(svc) {
                    return locate::dispatch(svc, &rest);
                }
                die(&format!(
                    "unknown service '{}' (want sonarr|sonarr-anime|radarr|prowlarr|sab|jellyfin|seerr|bazarr)",
                    svc
                ));
            }
            if rest.is_empty() {
                println!("{}", usage::usage());
                std::process::exit(1);
            }
            run_svc_command(svc, rest[0].as_str(), &rest[1..]);
        }
    }
}

/// The per-service command table. Called with an explicit service prefix, and
/// by locate::dispatch after resolving a bare title to its owning service.
pub fn run_svc_command(svc: &str, cmd: &str, args: &[String]) {
    match cmd {
                "status" => browse::cmd_status(svc, args),
                "get" => browse::cmd_get(svc, args),
                "seasons" => browse::cmd_seasons(svc, args),
                "releases" => browse::cmd_releases(svc, args),
                "grab" => acquire::cmd_grab(svc, args),
                "monitor" => browse::cmd_monitor(svc, args),
                "queue" => browse::cmd_queue(svc, args),
                "queue-rm" => acquire::cmd_queue_rm(svc, args),
                "stuck" => acquire::cmd_stuck(svc, args),
                "episodes" => browse::cmd_episodes(svc, args),
                "wait" => browse::cmd_wait(svc, args),
                "searches" => browse::cmd_searches(svc, args),
                "cancel" => browse::cmd_cancel(svc, args),
                "history" => browse::cmd_history(svc, args),
                "wanted" => browse::cmd_wanted(svc, args),
                "parse" => browse::cmd_parse(svc, args),
                "search" => browse::cmd_search(svc, args),
                "import" => acquire::cmd_import(svc, args),
                "files" => disk::cmd_files(svc, args),
                "delete" => disk::cmd_delete(svc, args),
                "audit" => disk::cmd_audit(svc, args),
                "availability" => disk::cmd_availability(svc, args),
                "lookup" => disk::cmd_lookup(svc, args),
                "info" => disk::cmd_info(svc, args),
                "tag" => browse::cmd_tag(svc, args),
                "raw" => browse::cmd_raw(svc, args),
                "add" => policy::cmd_add(svc, args),
                "coverage" => policy::cmd_coverage(svc, args),
                "tracks" => policy::cmd_tracks(svc, args),
                "watch" => policy::cmd_watch(svc, args),
                "replace" => policy::cmd_replace(svc, args),
                c => die(&format!("unknown command '{}' (try: arr --help)", c)),
    }
}
