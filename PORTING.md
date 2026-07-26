# arr.py → Rust porting conventions

Scope: `/data/arr/arr.py` (CLI) and `/etc/nixos/download-notifier.py` (daemon)
are being ported into this workspace. `arr.py` stays authoritative until the
Rust CLI reaches parity and the nix wrapper is cut over — do not modify it.

## Layout

- `crates/arr-api` — env/key loading, HTTP clients, `JsonExt`, `die`/`pop_flags`/
  `resolve_id`/`parse_seasons` helpers. Read `src/*.rs` before porting anything.
- `crates/arr` — the CLI. One module per command group under `src/`
  (e.g. `browse.rs`, `acquire.rs`, `policy.rs`, `disk.rs`, `integrations.rs`),
  wired into `main.rs`'s `dispatch_svc`/`dispatch_family`.
- `crates/arr-notifierd` — the daemon.

## Parity rules (the point of the exercise)

1. **Output strings are API.** Hermes' skills, crons, and Andre's muscle memory
   parse them. Port print formats byte-for-byte, including emoji, indentation,
   `⚠` warnings, and stderr-vs-stdout choice. When Python does `"%s" % round(x,1)`
   use `format!("{:.1}", ...)`; `gb()`/`fmt_gb()`/`mb()` exist in arr-api.
2. **Exit codes are API.** `die()` = exit 1. `watch` exits 0/1/2/3/4 (worst-wins
   across targets). `delete` without `--yes` is a dry-run that exits 1.
3. **Flags are API.** Port `pop_flags` specs exactly — same flag names, same
   nargs. No clap, no new flags, no renamed flags.
4. **Dict-poking via `JsonExt`** (`v.s("title")`, `v.i("id")`, `v.a("seasons")`,
   `v.at(&["quality","quality","name"])`). Match Python `.get()` default
   behavior — absent fields are ""/0/false/[], never a panic. `unwrap()` on API
   data is a porting bug.
5. **HTTP via arr-api only.** `api()`/`api_t()` die with arr.py's exact error
   strings; `try_api()` is for flows that must survive errors (the daemon; the
   CLI's `--once` watch retry). Timeouts: match each Python call site
   (`api(svc, "GET", path)` default 120s; releases/searches pass longer).
6. **Subprocess calls** (ffprobe, etc.): `std::process::Command`, same argv,
   same parse of stdout.
7. Comments: keep arr.py's load-bearing comments (the WHY ones), drop narration.

## Build & test

    cd /data/arr && nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#gcc \
      --command cargo build

Binaries land in `target/debug/`. Read-only commands can be parity-tested live
against the Python: `diff <(python3 arr.py radarr status X) <(target/debug/arr radarr status X)`.
Mutating commands (grab/delete/import/tag/queue-rm) must NOT be run against the
live services during porting — verify by code review + `--dry-run` paths only.
