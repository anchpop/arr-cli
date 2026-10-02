//! Audiobook acquisition is deliberately four independently inspectable steps.
use arr_api::{
    abook::{self, Forum, SITES},
    die, pop_flags, JsonExt,
};

const HELP: &str = "arr abook search '<query>'\narr abook reveal <topic-id|url>\narr abook nzb '<search string>' [--site nzbindex|binsearch|nzbking]\narr abook grab <nzb-url|nzbindex:id|nzbking:id> --name 'Author - Title' [--password P]\n\nreveal presses Say Thanks on Andre's forum account when needed; never posts replies.\ngrab submits to SAB category audiobooks. Follow with arr sab queue / history.";
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
fn must<T>(r: Result<T, String>) -> T {
    r.unwrap_or_else(|e| die(&format!("abook: {e}")))
}
pub fn dispatch(args: &[String]) {
    if args.is_empty() || matches!(args[0].as_str(), "--help" | "-h" | "help") {
        println!("{HELP}");
        return;
    }
    let (flags, rest) = pop_flags(
        &args[1..],
        &[("--site", 1), ("--name", 1), ("--password", 1)],
    );
    match args[0].as_str() {
        "search" => {
            if rest.is_empty() {
                die("abook search: need a query");
            }
            let query = rest.join(" ");
            let rows = must(must(Forum::new()).search(&query));
            println!(
                "abook: {} topic(s) on this result page for {}",
                rows.len(),
                quote(&query)
            );
            for r in rows {
                println!(
                    "{} | {}\n  board: {} | date: {}\n  next: arr abook reveal {}",
                    r.id, r.title, r.board, r.date, r.id
                );
            }
            println!("Narrow the query if needed; inspect the board (Test Area is not a real release). Already-thanked state is checked by reveal.");
        }
        "reveal" => {
            if rest.len() != 1 {
                die("abook reveal: need one topic id or URL");
            }
            let r = must(must(Forum::new()).reveal(&rest[0]));
            println!(
                "{}\nthanks: {}",
                r.title,
                if r.thanked {
                    "pressed Say Thanks on Andre's account"
                } else {
                    "content already visible; no Thanks action needed"
                }
            );
            for (k, v) in r.metadata {
                println!("{k}: {v}");
            }
            println!(
                "password: {}",
                r.password.as_deref().unwrap_or("no password stated")
            );
            println!(
                "Revealed content (check context before selecting a release):\n{}",
                r.hidden
            );
            if r.searches.is_empty() {
                die("topic has no recognizable search string; inspect the revealed content above or open the topic on the website");
            }
            for s in r.searches {
                println!("search string: {s}\nnext: arr abook nzb {}", quote(&s));
            }
        }
        "nzb" => {
            if rest.is_empty() {
                die("abook nzb: need a revealed search string");
            }
            let query = rest.join(" ");
            let sites = if let Some(site) = flags.val("--site") {
                if !SITES.contains(&site) {
                    die("--site must be nzbindex, binsearch, or nzbking");
                }
                vec![site]
            } else {
                SITES.to_vec()
            };
            for site in sites {
                println!("searching {site}: {}", quote(&query));
                match abook::nzb_search(site, &query) {
                    Ok(rows) if !rows.is_empty() => {
                        println!(
                            "{} candidate(s) from {site} (up to 25; refine the string for more):",
                            rows.len()
                        );
                        for (i, r) in rows.iter().enumerate() {
                            println!("{}. {}\n  {}\n  NZB: {}\n  next: arr abook grab {} --name 'Author - Title'",i+1,r.subject,r.details,r.url,quote(&r.url));
                        }
                        println!("Replace 'Author - Title' with the clean library name. If reveal printed a password, append --password 'PASSWORD'. Check subject/size/completeness before grabbing; a search match is not identity verification.");
                        println!(
                            "Other sites: arr abook nzb {} --site {}",
                            quote(&query),
                            if site == "nzbking" {
                                "binsearch"
                            } else {
                                "nzbking"
                            }
                        );
                        return;
                    }
                    Ok(_) => println!("{site}: no NZB results"),
                    Err(e) => println!("{site}: {e}"),
                }
            }
            die("no NZB candidates; try shortening the string or --site nzbking for old posts. No download was submitted.");
        }
        "grab" => {
            if rest.len() != 1 {
                die("abook grab: need one candidate NZB URL or provider:id");
            }
            let name = flags.val("--name").unwrap_or_else(|| {
                die("abook grab requires --name 'Author - Title' (the library folder name)")
            });
            if name.trim().is_empty() || name.contains(['/', '\\', '\r', '\n']) {
                die("--name must be a clean nonempty folder name without slashes or newlines");
            }
            let nzb = must(abook::fetch_nzb(&rest[0]));
            let r = must(arr_api::http::sab_add_nzb(
                &nzb,
                "audiobooks",
                name,
                flags.val("--password"),
            ));
            println!(
                "Added to SAB: {name}\ncategory: audiobooks\narchive password: {}",
                if flags.val("--password").is_some() {
                    "supplied"
                } else {
                    "none"
                }
            );
            for id in r.a("nzo_ids") {
                println!("nzo_id: {}", id.as_str().unwrap_or("unknown"));
            }
            println!("next: arr sab queue {}\nthen: arr sab history {}\nDestination: /data/media/audiobooks (check unpacking and library visibility before reporting ready).",quote(name),quote(name));
        }
        _ => die(HELP),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shell_quotes() {
        assert_eq!(quote("Sorcerer's"), "'Sorcerer'\\''s'");
    }
}
