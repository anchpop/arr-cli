//! die(), size formatting, flag parsing, id resolution — the helpers every
//! command uses. Output formats must stay byte-compatible with arr.py: the
//! Hermes skills and cron watchdogs parse these strings.

use std::collections::{BTreeSet, HashMap};

use serde_json::Value;

use crate::http::api;
use crate::json::JsonExt;

pub fn die(msg: &str) -> ! {
    eprintln!("arr: {}", msg);
    std::process::exit(1);
}

/// Bytes -> GiB rounded to 1 decimal (Python: round(n/2**30, 1)).
pub fn gb(n: i64) -> f64 {
    (n as f64 / 1073741824.0 * 10.0).round() / 10.0
}

/// Renders like Python's str(round(..., 1)): "17.8", "17.0".
pub fn fmt_gb(n: i64) -> String {
    format!("{:.1}", gb(n))
}

/// Bytes -> MiB rounded to int.
pub fn mb(n: i64) -> i64 {
    (n as f64 / 1048576.0).round() as i64
}

/// Parsed flags from pop_flags. nargs=0 flags are presence-only; nargs=1
/// flags carry a value.
#[derive(Debug, Default)]
pub struct Flags {
    map: HashMap<String, Option<String>>,
}

impl Flags {
    pub fn has(&self, flag: &str) -> bool {
        self.map.contains_key(flag)
    }
    pub fn val(&self, flag: &str) -> Option<&str> {
        self.map.get(flag).and_then(|v| v.as_deref())
    }
    pub fn val_or<'a>(&'a self, flag: &str, default: &'a str) -> &'a str {
        self.val(flag).unwrap_or(default)
    }
}

/// Pull flags out of args. spec: (flag, nargs 0|1). Returns (flags, rest).
/// A value-flag at end-of-args dies (Python raised IndexError; a clean error
/// beats a traceback and nothing parsed argv[i+1] behavior differently).
pub fn pop_flags(args: &[String], spec: &[(&str, usize)]) -> (Flags, Vec<String>) {
    let spec: HashMap<&str, usize> = spec.iter().copied().collect();
    let mut flags = Flags::default();
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match spec.get(a) {
            Some(0) => {
                flags.map.insert(a.to_string(), None);
                i += 1;
            }
            Some(_) => {
                let v = args
                    .get(i + 1)
                    .unwrap_or_else(|| die(&format!("{} needs a value", a)));
                flags.map.insert(a.to_string(), Some(v.clone()));
                i += 2;
            }
            None => {
                rest.push(args[i].clone());
                i += 1;
            }
        }
    }
    (flags, rest)
}

/// numeric -> id; title substring -> unique id (or list candidates & die).
pub fn resolve_id(svc: &str, q: &str) -> i64 {
    if !q.is_empty() && q.chars().all(|c| c.is_ascii_digit()) {
        return q.parse().unwrap();
    }
    let coll = if crate::is_series(svc) { "series" } else { "movie" };
    let items = api(svc, "GET", &format!("/{}", coll), None);
    let ql = q.to_lowercase();
    let empty = vec![];
    let items = items.as_ref().and_then(Value::as_array).unwrap_or(&empty);
    let hits: Vec<&Value> = items
        .iter()
        .filter(|it| {
            let mut names: Vec<&str> = vec![it.s("title"), it.s("titleSlug")];
            names.extend(it.a("alternateTitles").iter().map(|a| a.s("title")));
            names.iter().any(|n| n.to_lowercase().contains(&ql))
        })
        .collect();
    match hits.len() {
        0 => die(&format!("no {} matching \"{}\"", coll, q)),
        1 => hits[0].i("id"),
        n => {
            eprintln!("ambiguous \"{}\" — {} matches:", q, n);
            for h in &hits {
                let year = h.get("year").map(|y| y.to_string()).unwrap_or("None".into());
                eprintln!("  {}\t{} ({})", h.i("id"), h.s("title"), year);
            }
            die("narrow the query or pass a numeric id")
        }
    }
}

/// '3-8' / '3,5' / '3-5,7' -> sorted set of ints.
pub fn parse_seasons(spec: &str) -> BTreeSet<i64> {
    let mut out = BTreeSet::new();
    for part in spec.split(',') {
        let part = part.trim();
        if let Some((a, b)) = part.split_once('-') {
            let (a, b) = (
                a.trim().parse::<i64>().unwrap_or_else(|_| die(&format!("bad season spec '{}'", spec))),
                b.trim().parse::<i64>().unwrap_or_else(|_| die(&format!("bad season spec '{}'", spec))),
            );
            out.extend(a..=b);
        } else if !part.is_empty() {
            out.insert(part.parse().unwrap_or_else(|_| die(&format!("bad season spec '{}'", spec))));
        }
    }
    out
}
