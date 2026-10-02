//! Step-wise abook.link forum access and public NZB search. Never posts replies.
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::time::Duration;

use scraper::{ElementRef, Html, Selector};
use serde_json::Value;
use url::Url;

use crate::{env_key, http::form_encode};

const FORUM: &str = "https://abook.link/book/index.php";
pub type Result<T> = std::result::Result<T, String>;
fn sel(s: &str) -> Selector {
    Selector::parse(s).unwrap()
}
fn text(e: ElementRef<'_>) -> String {
    e.text()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
fn first(e: ElementRef<'_>, s: &str) -> String {
    e.select(&sel(s)).next().map(text).unwrap_or_default()
}
fn http_error(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(n, _) => format!("HTTP {n}"),
        ureq::Error::Transport(e) => format!("network request failed: {}", e.kind()),
    }
}
fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(45))
        .build()
}
fn body(r: ureq::Response) -> Result<String> {
    r.into_string().map_err(|e| e.to_string())
}

pub struct Forum {
    agent: ureq::Agent,
    cache: PathBuf,
}
impl Forum {
    pub fn new() -> Result<Self> {
        let root = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
            .ok_or("HOME or XDG_CACHE_HOME is required for the forum session")?;
        let dir = root.join("arr/abook");
        fs::create_dir_all(&dir).map_err(|e| format!("session cache: {e}"))?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
        let cache = dir.join("cookies.json");
        let store = File::open(&cache)
            .ok()
            .and_then(|f| cookie_store::serde::json::load(BufReader::new(f)).ok())
            .unwrap_or_default();
        Ok(Self {
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(45))
                .redirects(0)
                .cookie_store(store)
                .build(),
            cache,
        })
    }
    fn save(&self) -> Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&self.cache)
            .map_err(|e| e.to_string())?;
        f.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
        cookie_store::serde::json::save(&self.agent.cookie_store(), &mut f)
            .map_err(|e| e.to_string())
    }
    // Follow only forum redirects: credentials/cookies never travel to another origin.
    fn request(&self, url: &str, form: Option<&[(&str, &str)]>) -> Result<String> {
        let mut url = forum_url(url)?;
        for n in 0..5 {
            let r = if n == 0 {
                match form {
                    Some(p) => self.agent.post(&url).send_form(p),
                    None => self.agent.get(&url).call(),
                }
            } else {
                self.agent.get(&url).call()
            }
            .map_err(http_error)?;
            if (300..400).contains(&r.status()) {
                url = forum_url(
                    r.header("Location")
                        .ok_or("forum redirect has no destination")?,
                )?;
            } else {
                self.save()?;
                return body(r);
            }
        }
        Err("too many forum redirects".into())
    }
    fn login(&self) -> Result<()> {
        let page = self.request(&format!("{FORUM}?action=login"), None)?;
        let doc = Html::parse_document(&page);
        let form = doc
            .select(&sel("form#frmLogin"))
            .next()
            .ok_or("login form missing (site changed or blocked)")?;
        let action = forum_url(form.value().attr("action").ok_or("login action missing")?)?;
        let mut fields: Vec<(String, String)> = form
            .select(&sel("input[type=hidden]"))
            .filter_map(|e| {
                Some((
                    e.value().attr("name")?.into(),
                    e.value().attr("value").unwrap_or("").into(),
                ))
            })
            .collect();
        fields.retain(|(k, _)| k != "hash_passwrd");
        fields.extend([
            ("user".into(), env_key("ABOOK_USERNAME", "ABOOK_USERNAME")),
            (
                "passwrd".into(),
                env_key("ABOOK_PASSWORD", "ABOOK_PASSWORD"),
            ),
            ("cookielength".into(), "-1".into()),
            ("hash_passwrd".into(), "".into()),
        ]);
        let fields: Vec<_> = fields
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let page = self.request(&action, Some(&fields))?;
        if !logged_in(&page) {
            return Err("abook login failed; check ABOOK_USERNAME / ABOOK_PASSWORD (or sign in on the website to check account restrictions)".into());
        }
        Ok(())
    }
    fn authenticated(&self, url: &str, form: Option<&[(&str, &str)]>) -> Result<String> {
        let page = self.request(url, form)?;
        if logged_in(&page) {
            return Ok(page);
        }
        self.login()?;
        let page = self.request(url, form)?;
        if !logged_in(&page) {
            return Err("abook session expired or access denied after login".into());
        }
        Ok(page)
    }
    pub fn search(&self, query: &str) -> Result<Vec<Topic>> {
        let page = self.authenticated(
            &format!("{FORUM}?action=search2"),
            Some(&[("search", query), ("subject_only", "1"), ("nograve", "1")]),
        )?;
        let hits = parse_topics(&page);
        if hits.is_empty() && !page.contains("No results found") && !page.contains("No results") {
            return Err("forum returned no recognizable results; it may be rate-limiting searches. Wait a few seconds, or search on the website".into());
        }
        Ok(hits)
    }
    pub fn reveal(&self, input: &str) -> Result<Reveal> {
        let id = topic_id(input).ok_or("expected a numeric topic id or an abook.link topic URL")?;
        let url = format!("{FORUM}?topic={id}.0");
        let mut page = self.authenticated(&url, None)?;
        let doc = Html::parse_document(&page);
        // Only thank the opening post, and only when it says content is locked.
        let post = doc
            .select(&sel(".post .inner"))
            .next()
            .ok_or("topic has no readable opening post")?;
        let mut thanked = false;
        if text(post).contains("You must thank this post") {
            let msg = post
                .value()
                .attr("id")
                .and_then(|s| s.strip_prefix("msg_"))
                .ok_or("opening post id missing")?;
            let button = doc
                .select(&sel("a.thank_you_button_link"))
                .filter_map(|e| e.value().attr("href"))
                .find(|href| href.contains(&format!(";msg={msg};")))
                .ok_or("locked post has no Thanks button; open it on the website")?;
            let action = forum_url(button)?;
            if !action.contains("action=thank;") || !action.contains(&format!("topic={id};")) {
                return Err("unexpected Thanks action; inspect the topic on the website".into());
            }
            self.request(&action, None)?;
            page = self.authenticated(&url, None)?;
            thanked = true;
        }
        parse_reveal(&page, thanked)
    }
}
fn logged_in(page: &str) -> bool {
    page.contains("action=logout")
}
fn forum_url(input: &str) -> Result<String> {
    let url = Url::parse(FORUM)
        .unwrap()
        .join(input)
        .map_err(|e| e.to_string())?;
    if url.scheme() != "https"
        || url.host_str() != Some("abook.link")
        || url.port().is_some()
        || url.path() != "/book/index.php"
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("refusing a non-abook forum URL".into());
    }
    Ok(url.into())
}
pub fn topic_id(input: &str) -> Option<String> {
    if !input.is_empty() && input.bytes().all(|c| c.is_ascii_digit()) {
        return Some(input.into());
    }
    let url = forum_url(input).ok()?;
    let part = url.split("topic=").nth(1)?;
    let id: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
    (!id.is_empty()).then_some(id)
}
#[derive(Debug)]
pub struct Topic {
    pub id: String,
    pub title: String,
    pub board: String,
    pub date: String,
}
fn parse_topics(page: &str) -> Vec<Topic> {
    let doc = Html::parse_document(page);
    doc.select(&sel(".topic_details"))
        .filter_map(|row| {
            let link = row.select(&sel("h5 a[href*='topic=']")).next()?;
            Some(Topic {
                id: topic_id(link.value().attr("href")?)?,
                title: text(link),
                board: first(row, "h5 a[href*='board=']"),
                date: first(row, "em"),
            })
        })
        .collect()
}
#[derive(Debug)]
pub struct Reveal {
    pub title: String,
    pub metadata: Vec<(String, String)>,
    pub searches: Vec<String>,
    pub password: Option<String>,
    pub hidden: String,
    pub thanked: bool,
}
fn lines(e: ElementRef<'_>) -> Vec<String> {
    let mut s = String::new();
    for n in e.descendants() {
        match n.value() {
            scraper::Node::Text(t) => s.push_str(t),
            scraper::Node::Element(el)
                if matches!(el.name(), "br" | "div" | "code" | "h6" | "p") =>
            {
                s.push('\n')
            }
            _ => {}
        }
    }
    s.lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect()
}
fn parse_reveal(page: &str, thanked: bool) -> Result<Reveal> {
    let doc = Html::parse_document(page);
    let post = doc
        .select(&sel(".post .inner"))
        .next()
        .ok_or("topic has no readable opening post (access denied or removed)")?;
    if text(post).contains("You must thank this post") {
        return Err(
            "Thanks did not reveal the opening post; inspect account access on the website".into(),
        );
    }
    let hidden = post.select(&sel(".unhiddenbox")).next().unwrap_or(post);
    let all = lines(post);
    let title = first(doc.root_element(), "title");
    let mut metadata: Vec<(String, String)> = all
        .iter()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            [
                "Title", "Author", "Read By", "Narrator", "Reader", "Subject",
            ]
            .contains(&k.trim())
            .then(|| (k.trim().into(), v.trim().into()))
        })
        .collect();
    let mut seen = std::collections::HashSet::new();
    metadata.retain(|item| seen.insert(item.clone()));
    let mut searches = Vec::new();
    let mut password = None;
    let mut pending = None;
    let codes: Vec<String> = hidden
        .select(&sel("code"))
        .filter(|e| !e.text().any(|t| t.contains('\n')))
        .map(text)
        .collect();
    for line in lines(hidden) {
        let low = line.to_lowercase();
        if low.starts_with("password")
            || low.starts_with("archive password")
            || low.starts_with("search:")
            || low.starts_with("search string:")
        {
            pending = Some(low.contains("password"));
            if let Some((_, v)) = line.split_once(':') {
                if !v.trim().is_empty() {
                    if pending == Some(true) {
                        password = Some(v.trim().into());
                    } else {
                        searches.push(v.trim().into());
                    }
                    pending = None;
                }
            }
            continue;
        }
        if line.starts_with("Code:")
            || line == "Hidden content:"
            || line == "[Copy]"
            || line == "[Select]"
        {
            continue;
        }
        // Archived spots sometimes put the entire original post in one code block.
        // Its Search/Password labels still work; never treat the description as a subject.
        if let Some(is_password) = pending.take() {
            if is_password {
                password = Some(line);
            } else {
                searches.push(line);
            }
        } else if codes.contains(&line) {
            searches.push(line);
        }
    }
    searches.dedup();
    Ok(Reveal {
        title,
        metadata,
        searches,
        password,
        hidden: lines(hidden).join("\n"),
        thanked,
    })
}

#[derive(Debug)]
pub struct Candidate {
    pub url: String,
    pub subject: String,
    pub details: String,
}
pub const SITES: [&str; 3] = ["nzbindex", "binsearch", "nzbking"];
pub fn nzb_search(site: &str, query: &str) -> Result<Vec<Candidate>> {
    // NZBKing defaults to OR between words; quotes request all terms.
    let literal = format!("\"{}\"", query.replace('"', ""));
    let q = form_encode(&[("q", if site == "nzbking" { &literal } else { query })]);
    let url = match site {
        "nzbindex" => format!("https://nzbindex.com/api/search?{q}&max=25"),
        "binsearch" => format!("https://binsearch.info/search?{q}"),
        "nzbking" => format!("https://www.nzbking.com/?{q}"),
        _ => return Err("site must be nzbindex, binsearch, or nzbking".into()),
    };
    let page = body(agent().get(&url).call().map_err(http_error)?)?;
    if site == "nzbindex" {
        let data: Value = serde_json::from_str(&page)
            .map_err(|_| "NZBIndex did not return search JSON (blocked or site changed)")?;
        if data["error"].as_bool() == Some(true) {
            return Err(format!("NZBIndex: {}", data["errorMessage"]));
        }
        return data["data"]["content"]
            .as_array()
            .ok_or("NZBIndex result format changed".into())
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| {
                        let id = r["id"].as_str()?;
                        Some(Candidate {
                            url: format!("https://nzbindex.com/download/{id}.nzb"),
                            subject: r["name"].as_str()?.into(),
                            details: format!(
                                "{} bytes | age: {} days | {} files | complete: {}",
                                r["size"],
                                std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap()
                                    .as_secs()
                                    .saturating_sub(r["posted"].as_u64().unwrap_or(0))
                                    / 86400,
                                r["fileCount"],
                                r["complete"]
                            ),
                        })
                    })
                    .collect()
            });
    }
    parse_nzb_html(site, &page, query)
}
fn parse_nzb_html(site: &str, page: &str, query: &str) -> Result<Vec<Candidate>> {
    let doc = Html::parse_document(page);
    let mut out = Vec::new();
    if site == "binsearch" {
        for row in doc.select(&sel("table.result-table tr")) {
            let Some(box_) = row.select(&sel("input[type=checkbox]")).next() else {
                continue;
            };
            let Some(id) = box_.value().attr("name") else {
                continue;
            };
            let q = form_encode(&[("q", query), (id, "on")]);
            out.push(Candidate {
                url: format!("https://binsearch.info/nzb?{q}"),
                subject: first(row, "a[href^='/details/']"),
                details: text(row),
            });
        }
        if out.is_empty() && !page.contains("No results") && !page.contains("result-table") {
            return Err(
                "Binsearch returned no recognizable result page (blocked or site changed)".into(),
            );
        }
    } else {
        for row in doc.select(&sel(".search-result")) {
            let Some(link) = row.select(&sel("a[href^='/nzb:']")).next() else {
                continue;
            };
            let subject = row.select(&sel(".search-subject")).next().unwrap();
            let subject = subject.text().next().unwrap_or("").trim().into();
            out.push(Candidate {
                url: format!(
                    "https://www.nzbking.com{}",
                    link.value().attr("href").unwrap()
                ),
                subject,
                details: text(row),
            });
        }
        if out.is_empty()
            && !page.contains("Search results:")
            && !page.contains("No results")
            && !page.contains("Nothing found")
        {
            return Err(
                "NZBKing returned no recognizable result page (blocked or site changed)".into(),
            );
        }
    }
    Ok(out.into_iter().take(25).collect())
}
/// Explicit provider ids avoid unstable list positions: nzbindex:<uuid>, nzbking:<id>.
pub fn nzb_url(input: &str) -> Result<String> {
    let url = if let Some(id) = input.strip_prefix("nzbindex:") {
        format!("https://nzbindex.com/download/{id}.nzb")
    } else if let Some(id) = input.strip_prefix("nzbking:") {
        format!("https://www.nzbking.com/nzb:{id}/")
    } else {
        input.into()
    };
    let u =
        Url::parse(&url).map_err(|_| "use a candidate NZB URL, nzbindex:<id>, or nzbking:<id>")?;
    let allowed = match u.host_str() {
        Some("nzbindex.com") => u.path().starts_with("/download/"),
        Some("binsearch.info") => u.path() == "/nzb",
        Some("www.nzbking.com" | "nzbking.com") => u.path().starts_with("/nzb:"),
        _ => false,
    };
    if u.scheme() != "https"
        || !allowed
        || !u.username().is_empty()
        || u.password().is_some()
        || u.port().is_some()
    {
        return Err(
            "use an HTTPS download URL from nzbindex.com, binsearch.info, or nzbking.com".into(),
        );
    }
    Ok(url)
}
pub fn fetch_nzb(input: &str) -> Result<Vec<u8>> {
    let url = nzb_url(input)?;
    let r = agent().get(&url).call().map_err(http_error)?;
    let mut bytes = Vec::new();
    r.into_reader()
        .take(32 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > 32 * 1024 * 1024 {
        return Err("NZB exceeds 32 MiB; not submitted to SAB".into());
    }
    let s = String::from_utf8_lossy(&bytes);
    if !s.contains("<nzb") || !s.contains("<segment") || s.contains("<html") {
        return Err("download was not an NZB with article segments (possibly a challenge/error page); not submitted to SAB".into());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn topic_urls() {
        assert_eq!(topic_id("3285").as_deref(), Some("3285"));
        assert_eq!(
            topic_id("https://abook.link/book/index.php?topic=3285.msg4518#msg4518").as_deref(),
            Some("3285")
        );
        assert!(topic_id("https://evil.example/?topic=3285").is_none());
        assert!(forum_url("https://abook.link.evil.example/book/index.php").is_err());
    }
    #[test]
    fn result_metadata() {
        let rows = parse_topics(
            r#"<div class="topic_details"><h5><a href="?board=12">Kids</a><a href="https://abook.link/book/index.php?topic=3285.0">Harry <strong>Potter</strong></a></h5><em>January 2012</em></div>"#,
        );
        assert_eq!(rows[0].id, "3285");
        assert_eq!(rows[0].title, "Harry Potter");
        assert_eq!(rows[0].board, "Kids");
    }
    #[test]
    fn current_and_archive_reveal() {
        let s = r#"<title>Book</title><div class="post"><div class="inner">Author: <span>Writer</span><br>Read By: <span>Reader</span><div class="unhiddenbox"><h6>Hidden content:</h6><br>Search:<br><div class="codeheader">Code: [Copy]</div><code>obfuscated.subject</code><br>Password:<br><div class="codeheader">Code: [Copy]</div><code>secret</code></div></div></div>"#;
        let r = parse_reveal(s, true).unwrap();
        assert_eq!(r.searches, vec!["obfuscated.subject"]);
        assert_eq!(r.password.as_deref(), Some("secret"));
        assert_eq!(r.metadata[0].1, "Writer");
        let s = r#"<div class="post"><div class="inner"><div class="unhiddenbox">Description<br><div>Code:</div><code>archive.subject</code></div></div></div>"#;
        let r = parse_reveal(s, false).unwrap();
        assert_eq!(r.searches, vec!["archive.subject"]);
        assert!(r.password.is_none());
    }
    #[test]
    fn archived_multiline_code_extracts_labels_not_description() {
        let page = r#"<title>Archive</title><div class="post"><div class="inner"><div class="unhiddenbox"><code>General Information
Title: A Book
Author: A Writer
Long description without a search string.
Search:
Code: [Select]
abook.to - exact-token
Password:
Code: [Select]
archive-password
</code></div></div></div>"#;
        let result = parse_reveal(page, false).unwrap();
        assert_eq!(result.searches, vec!["abook.to - exact-token"]);
        assert_eq!(result.password.as_deref(), Some("archive-password"));
        assert_eq!(result.metadata[0], ("Title".into(), "A Book".into()));
    }
    #[test]
    fn locked_and_missing_content_are_distinct() {
        assert!(parse_reveal(r#"<div class="post"><div class="inner">You must thank this post to see the content.</div></div>"#, true).is_err());
        let r = parse_reveal(
            r#"<div class="post"><div class="inner">No release supplied.</div></div>"#,
            false,
        )
        .unwrap();
        assert!(r.searches.is_empty());
        assert_eq!(r.hidden, "No release supplied.");
    }
    #[test]
    fn provider_ids_and_download_paths() {
        assert_eq!(
            nzb_url("nzbking:abc123").unwrap(),
            "https://www.nzbking.com/nzb:abc123/"
        );
        assert_eq!(
            nzb_url("nzbindex:abc123").unwrap(),
            "https://nzbindex.com/download/abc123.nzb"
        );
        assert!(nzb_url("https://nzbindex.com/download/../../private").is_err());
        assert!(nzb_url("https://user:password@nzbindex.com/download/x.nzb").is_err());
    }
    #[test]
    fn invalid_pages_fail_closed() {
        assert!(parse_reveal("<html>Login</html>", false).is_err());
        assert!(nzb_url("http://localhost/private").is_err());
        assert!(nzb_url("https://abook.link/book/index.php?action=post").is_err());
    }
    #[test]
    fn public_provider_rows() {
        let s = r#"<table class="result-table"><tr><td><input type="checkbox" name="abc"><a href="/details/abc">Subject</a><span>12MB</span></td><td>10 days</td></tr></table>"#;
        let rows = parse_nzb_html("binsearch", s, "a & b").unwrap();
        assert_eq!(rows[0].subject, "Subject");
        assert!(rows[0].url.contains("abc=on"));
        assert!(rows[0].url.contains("q=a+%26+b"));
        let s = r#"<div class="search-result"><div class="search-subject">Subject<br><a href="/nzb:abc/">NZB</a>parts: 5/5 size: 1MB</div><div class="search-age">2d</div></div>"#;
        let rows = parse_nzb_html("nzbking", s, "").unwrap();
        assert_eq!(rows[0].subject, "Subject");
        assert!(rows[0].details.contains("5/5"));
    }
}
