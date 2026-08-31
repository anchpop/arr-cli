//! `arr book` — book acquisition over the Bindery/Shelfmark/Jellyfin stack.
//!
//! Books have two channels: Bindery (usenet via Prowlarr — thin for
//! academic/technical titles, a miss there is expected) and Shelfmark
//! (Anna's Archive/Libgen — the reliable fallback, no auth on localhost).
//! `arr book add` runs the whole pipeline: Jellyfin dedup check → edition
//! resolve → usenet probe (informational) → Shelfmark release search →
//! queue → wait → Jellyfin visibility confirm. Books never touch the arrs,
//! so the download-notifier does NOT DM requesters — the caller must.
//!
//! Output strings grow parsers (Hermes' book skill); evolve additively.

use std::time::Duration;

use serde_json::{json, Value};

use arr_api::{bindery_api, die, jf_api, pop_flags, shelfmark_api, JsonExt};

use crate::integrations::jf_search_items;

/// Format preference when the caller doesn't pin one (mirrors Shelfmark's own
/// supported-format ordering: reflowable text first, scans last).
const FORMAT_PREF: &[&str] = &["epub", "azw3", "mobi", "fb2", "pdf", "djvu", "cbz", "cbr"];

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn looks_like_isbn(q: &str) -> bool {
    let digits: String = q.chars().filter(|c| c.is_ascii_digit()).collect();
    (digits.len() == 10 || digits.len() == 13) && q.chars().all(|c| c.is_ascii_digit() || c == '-')
}

/// Search Shelfmark's metadata provider; returns the `books` array.
fn shelfmark_metadata_search(query: &str) -> Vec<Value> {
    let r = shelfmark_api("GET", "/metadata/search", &[("query", query)], None, 60, false);
    let r = r.unwrap_or(Value::Null);
    if !r.s("error").is_empty() {
        // "No metadata provider configured" etc. — config problem, not a miss.
        die(&format!(
            "shelfmark metadata search: {} (check Settings → Metadata Providers; \
             the Hardcover key lives in /data/.shelfmark/plugins/hardcover.json, \
             recovery copy in sops shelfmark/hardcover_api_key)",
            r.s("error")
        ));
    }
    r.a("books").to_vec()
}

/// Quick usenet availability probe via Bindery's live indexer search.
/// Purely informational: `arr book add` acquires through Shelfmark either way;
/// a hit here just means Bindery could also manage this title (monitored, its
/// author-dir layout) if added through its UI.
fn usenet_probe(query: &str) -> Option<usize> {
    let r = bindery_api("GET", "/indexer/search", &[("q", query)], 60, true)?;
    match r {
        Value::Array(a) => Some(a.len()),
        other => Some(other.a("items").len()),
    }
}

fn jf_book_hits(term: &str) -> Vec<Value> {
    let hits: Vec<Value> = jf_search_items(term, 10)
        .into_iter()
        .filter(|it| matches!(it.s("Type"), "Book" | "AudioBook"))
        .collect();
    if !hits.is_empty() {
        return hits;
    }
    // Jellyfin's searchTerm won't match "Title: Subtitle" against an item
    // named just "Title" (observed with Contextual Design) — retry the prefix.
    match term.split_once(':') {
        Some((prefix, _)) if !prefix.trim().is_empty() => jf_search_items(prefix.trim(), 10)
            .into_iter()
            .filter(|it| matches!(it.s("Type"), "Book" | "AudioBook"))
            .collect(),
        _ => hits,
    }
}

/// Candidate detail lookup — the search payload carries null ISBNs; only the
/// per-book endpoint has them.
fn book_detail_isbns(provider: &str, id: &str) -> Vec<String> {
    let path = format!("/metadata/book/{}/{}", provider, id);
    match shelfmark_api("GET", &path, &[], None, 30, true) {
        Some(d) => [d.s("isbn_13"), d.s("isbn_10")]
            .iter()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect(),
        None => Vec::new(),
    }
}

/// arr book add '<title|isbn>' [--author X] [--format epub] [--book-id ID]
///              [--no-wait] [--timeout SECS]
pub fn cmd_book_add(args: &[String]) {
    let (flags, rest) = pop_flags(
        args,
        &[
            ("--author", 1),
            ("--format", 1),
            ("--book-id", 1),
            ("--no-wait", 0),
            ("--timeout", 1),
        ],
    );
    if rest.is_empty() {
        die("book add: need a title or ISBN");
    }
    let query = rest.join(" ");
    let timeout: u64 = flags
        .val_or("--timeout", "1800")
        .parse()
        .unwrap_or_else(|_| die("bad --timeout"));

    // 0. Already readable? (Jellyfin is the source of truth for "we have it" —
    //    a Bindery-eye `ls` of the library lies about permissions.)
    let existing = jf_book_hits(&query);
    if !existing.is_empty() {
        println!("already in Jellyfin:");
        for it in &existing {
            println!("  [{}] {}", it.s("Type"), it.s("Name"));
            if !it.s("Path").is_empty() {
                println!("      {}", it.s("Path"));
            }
        }
        return;
    }

    // 1. Resolve the edition via Shelfmark's metadata provider. A bare ISBN
    //    that the provider can't match gets a second chance through Bindery's
    //    OpenLibrary isbn lookup (title+author then re-queries the provider).
    let mut books = shelfmark_metadata_search(&query);
    if books.is_empty() && looks_like_isbn(&query) {
        if let Some(b) = bindery_api("GET", "/book/lookup", &[("isbn", &query)], 30, true) {
            let title = b.s("title").trim().to_string();
            if !title.is_empty() {
                println!("isbn {} resolves to '{}' — searching by title", query, title);
                books = shelfmark_metadata_search(&title);
            }
        }
    }
    let book = match flags.val("--book-id") {
        Some(id) => books
            .iter()
            .find(|b| b.s("provider_id") == id)
            .cloned()
            .unwrap_or_else(|| die(&format!("no candidate with provider_id {}", id))),
        None => match books.len() {
            0 => {
                let hint = match usenet_probe(&query) {
                    Some(n) if n > 0 => format!(
                        "but usenet has {} candidate release(s) — try Bindery (:8787)",
                        n
                    ),
                    _ => "and usenet has nothing either".to_string(),
                };
                eprintln!("no metadata match for \"{}\" — {}", query, hint);
                std::process::exit(1);
            }
            1 => books[0].clone(),
            // An ISBN query names ONE edition-family: auto-pick the candidate
            // whose detail ISBNs contain it, if exactly one does. (Editions in
            // the same family carry different ISBNs, so a non-match still falls
            // through to the candidate list rather than failing.)
            n if looks_like_isbn(&query) => {
                let matches: Vec<&Value> = books
                    .iter()
                    .take(8)
                    .filter(|b| {
                        book_detail_isbns(b.s("provider"), b.s("provider_id"))
                            .iter()
                            .any(|i| i == &query)
                    })
                    .collect();
                if let [one] = matches[..] {
                    one.clone()
                } else {
                    eprintln!("ambiguous \"{}\" — {} candidates:", query, n);
                    for b in &books {
                        let authors: Vec<String> = b
                            .a("authors")
                            .iter()
                            .map(|a| a.as_str().unwrap_or("").into())
                            .collect();
                        eprintln!(
                            "  {}\t{} — {}",
                            b.s("provider_id"),
                            b.s("title"),
                            authors.join(", ")
                        );
                    }
                    die("narrow the query or pass --book-id <provider_id>");
                }
            }
            n => {
                eprintln!("ambiguous \"{}\" — {} candidates:", query, n);
                for b in &books {
                    let authors: Vec<String> =
                        b.a("authors").iter().map(|a| a.as_str().unwrap_or("").into()).collect();
                    eprintln!(
                        "  {}\t{} — {}",
                        b.s("provider_id"),
                        b.s("title"),
                        authors.join(", ")
                    );
                }
                die("narrow the query or pass --book-id <provider_id>");
            }
        },
    };
    let provider = book.s("provider").to_string();
    let book_id = book.s("provider_id").to_string();
    let title = book.s("title").to_string();
    let authors: Vec<String> =
        book.a("authors").iter().map(|a| a.as_str().unwrap_or("").into()).collect();
    println!("matched: {} — {} [{} {}]", title, authors.join(", "), provider, book_id);

    // Dedup again under the RESOLVED title — an ISBN query can't match a
    // Jellyfin item by name, so the first check alone would re-download.
    if title != query {
        let existing = jf_book_hits(&title);
        if !existing.is_empty() {
            println!("already in Jellyfin:");
            for it in &existing {
                println!("  [{}] {}", it.s("Type"), it.s("Name"));
                if !it.s("Path").is_empty() {
                    println!("      {}", it.s("Path"));
                }
            }
            return;
        }
    }

    // 2. Usenet probe (informational — see usenet_probe docs).
    let probe_q = format!("{} {}", title, authors.first().cloned().unwrap_or_default());
    match usenet_probe(probe_q.trim()) {
        Some(0) => println!("usenet: nothing (normal for academic/technical titles)"),
        Some(n) => println!(
            "usenet: {} candidate release(s) — Bindery could also manage this one",
            n
        ),
        None => println!("usenet: probe unavailable (bindery down?)"),
    }

    // 3. Shelfmark release search (drives Anna's Archive live; can take ~30s).
    println!("searching Anna's Archive/Libgen...");
    let rel = shelfmark_api(
        "GET",
        "/releases",
        &[("provider", &provider), ("book_id", &book_id), ("title", &title)],
        None,
        180,
        false,
    )
    .unwrap_or(Value::Null);
    if !rel.s("error").is_empty() {
        // Shelfmark's error text IS the diagnosis; add the local playbook.
        eprintln!("release search failed: {}", rel.s("error"));
        eprintln!(
            "(\"Unable to reach download source\" = the AA mirrors rotted — set new \
             domains via PUT /api/settings/mirrors and restart podman-shelfmark; \
             empty sources_searched = direct_download source disabled)"
        );
        std::process::exit(2);
    }
    let releases = rel.a("releases");
    if releases.is_empty() {
        eprintln!("no releases found for \"{}\" on Anna's Archive/Libgen", title);
        std::process::exit(1);
    }

    // Pick: pinned --format first, else the FORMAT_PREF ladder.
    let pick = |fmt: &str| releases.iter().find(|r| r.s("format").eq_ignore_ascii_case(fmt));
    let chosen = match flags.val("--format") {
        Some(f) => pick(f).unwrap_or_else(|| {
            let have: Vec<&str> = releases.iter().map(|r| r.s("format")).collect();
            die(&format!("no {} release (available: {})", f, have.join(", ")))
        }),
        None => FORMAT_PREF
            .iter()
            .find_map(|f| pick(f))
            .unwrap_or(&releases[0]),
    };
    println!(
        "grabbing: {} | {} | {}",
        chosen.s("format"),
        chosen.s("size"),
        chosen.s("title")
    );

    // 4. Queue it.
    let payload = json!({
        "source": chosen.s("source"),
        "source_id": chosen.s("source_id"),
        "title": title,
        "format": chosen.s("format"),
        "size": chosen.s("size"),
    });
    let q = shelfmark_api("POST", "/releases/download", &[], Some(&payload), 60, false)
        .unwrap_or(Value::Null);
    println!("queued (status: {})", q.s("status"));
    let dl_id = chosen.s("source_id").to_string();

    if flags.has("--no-wait") {
        println!("not waiting — `arr book status` follows it");
        return;
    }

    // 5. Wait for the download (AA slow-partner pace is ~1 MB/s; a 100MB epub
    //    legitimately takes ~20 min — silence here is not a stall).
    let deadline = now_secs() + timeout as f64;
    let mut last_pct = -1.0_f64;
    loop {
        if now_secs() > deadline {
            println!(
                "still downloading after {}s — check later with `arr book status`",
                timeout
            );
            std::process::exit(3);
        }
        std::thread::sleep(Duration::from_secs(10));
        let st = shelfmark_api("GET", "/status", &[], None, 30, true).unwrap_or(Value::Null);
        if st.at(&["complete", &dl_id]).is_object() {
            let path = st.at(&["complete", &dl_id]).s("download_path").to_string();
            println!("download complete{}", if path.is_empty() { String::new() } else { format!(": {}", path.replace("/books/", "/data/media/books/")) });
            break;
        }
        if st.at(&["error", &dl_id]).is_object() {
            eprintln!(
                "download FAILED: {}",
                st.at(&["error", &dl_id]).s("status_message")
            );
            std::process::exit(2);
        }
        let dl = st.at(&["downloading", &dl_id]);
        if dl.is_object() {
            let pct = dl.f("progress");
            if pct - last_pct >= 10.0 {
                println!("  downloading... {:.0}%", pct);
                last_pct = pct;
            }
        }
    }

    // 6. Confirm it's actually readable (scanned into Jellyfin), like the
    //    notifier does for movies/shows: imported-on-disk isn't "ready".
    jf_api("/Library/Refresh", &[], 60, "POST", true);
    let jf_deadline = now_secs() + 180.0;
    while now_secs() < jf_deadline {
        let hits = jf_book_hits(&title);
        if let Some(first) = hits.first() {
            println!("visible in Jellyfin: {} ({})", first.s("Name"), first.s("Type"));
            println!(
                "NB books skip the download-notifier — DM the requester yourself \
                 (web reader: https://watch.beef.baby, OPDS feed: /opds)"
            );
            return;
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    println!("downloaded but not yet visible in Jellyfin — `arr jellyfin refresh --wait '{}'`", title);
    std::process::exit(2);
}

/// arr book status — Shelfmark queue + Bindery queue rollup.
pub fn cmd_book_status(_args: &[String]) {
    let st = shelfmark_api("GET", "/status", &[], None, 30, false).unwrap_or(Value::Null);
    let mut any = false;
    for state in ["downloading", "resolving", "locating", "queued", "error", "complete"] {
        let grp = st.at(&[state]);
        let Some(obj) = grp.as_object() else { continue };
        for (_, it) in obj {
            any = true;
            let pct = if state == "downloading" {
                format!(" {:.0}%", it.f("progress"))
            } else {
                String::new()
            };
            let note = if state == "error" {
                format!("  ({})", it.s("status_message"))
            } else {
                String::new()
            };
            println!("  [shelfmark {}{}] {}{}", state, pct, it.s("title"), note);
        }
    }
    // Bindery's /queue is history-inclusive; only in-flight states are news.
    match bindery_api("GET", "/queue", &[], 30, true) {
        Some(q) => {
            for it in q.a("items") {
                if matches!(it.s("status"), "imported" | "failed") {
                    continue;
                }
                any = true;
                println!(
                    "  [bindery {}] {}",
                    it.s("status"),
                    if it.s("title").is_empty() { it.s("sourceTitle") } else { it.s("title") }
                );
            }
        }
        None => println!("  (bindery queue unavailable)"),
    }
    if !any {
        println!("no book downloads in flight");
    }
}

pub fn dispatch(cmd: &str, args: &[String]) {
    match cmd {
        "add" => cmd_book_add(args),
        "status" => cmd_book_status(args),
        c => die(&format!("unknown book command '{}'", c)),
    }
}
