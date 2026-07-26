//! arr — Sonarr/Radarr/Prowlarr/SABnzbd CLI (Rust port of arr.py).
//!
//! Dispatch mirrors arr.py's main(): `arr <svc> <cmd> ...` for the arr
//! services, plus the sab/jellyfin/seerr/bazarr command families and the
//! top-level `arr delete` / `arr queue` conveniences. Output strings, flags
//! and exit codes are parity-critical: Hermes' skills and cron watchdogs
//! parse them.

use arr_api::die;

mod usage;

fn main() {
    // Python gets SIG_DFL for SIGPIPE by default via signal(); Rust ignores it,
    // which turns `arr ... | head` into a broken-pipe panic. Restore default.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || matches!(argv[0].as_str(), "-h" | "--help" | "help") {
        println!("{}", usage::USAGE);
        std::process::exit(if argv.is_empty() { 1 } else { 0 });
    }
    let svc = argv[0].as_str();
    let rest: Vec<String> = argv[1..].to_vec();
    match svc {
        "sab" => dispatch_family("sab", &rest, "queue|status|add|prio|history|cleanup"),
        "jellyfin" => dispatch_family("jellyfin", &rest, "unwatched|has|refresh"),
        "seerr" => dispatch_family("seerr", &rest, "requests|request|unfulfilled"),
        "bazarr" => dispatch_family("bazarr", &rest, "status|wanted|search|raw"),
        "delete" => todo_cmd("delete-auto"),
        "queue" => todo_cmd("queue-overview"),
        _ => {
            if arr_api::svc_cfg(svc).is_none() {
                die(&format!(
                    "unknown service '{}' (want sonarr|sonarr-anime|radarr|prowlarr|sab|jellyfin|seerr|bazarr)",
                    svc
                ));
            }
            if rest.is_empty() {
                println!("{}", usage::USAGE);
                std::process::exit(1);
            }
            dispatch_svc(svc, rest[0].as_str(), &rest[1..]);
        }
    }
}

fn dispatch_family(family: &str, rest: &[String], cmds: &str) {
    if rest.is_empty() {
        die(&format!("{}: {}", family, cmds));
    }
    let cmd = rest[0].as_str();
    let _args = &rest[1..];
    match (family, cmd) {
        _ => die(&format!("unknown {} command '{}'", family, cmd)),
    }
}

fn dispatch_svc(svc: &str, cmd: &str, args: &[String]) {
    let _ = (svc, args);
    match cmd {
        _ => die(&format!("unknown command '{}' (try: arr --help)", cmd)),
    }
}

fn todo_cmd(name: &str) -> ! {
    die(&format!("{}: not yet ported", name));
}
