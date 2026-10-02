# arr-cli

Rust CLI + daemon over the Sonarr / Radarr / Prowlarr / SABnzbd / qBittorrent /
Jellyfin / Jellyseerr / Bazarr APIs for the beef.baby media box.

A Cargo workspace (rewritten from Python 2026-07-26, byte-parity output):

- `crates/arr-api` — shared clients: env-file key loading, per-service HTTP,
  `JsonExt` helpers
- `crates/arr` — the `arr` command (a NixOS wrapper execs
  `target/release/arr`)
- `crates/arr-notifierd` — the download-notifier daemon (live Discord progress
  DMs; the `download-notifier` systemd unit execs `target/release/arr-notifierd`)

This repo is the **source of truth** for both binaries. Edit, then build —
**no nixos-rebuild**:

    cd /data/arr && cargo build --release                  # andrep
    CARGO_HOME=/data/hermes/.cargo cargo build --release   # hermes

A daemon change also needs `systemctl restart download-notifier` (hermes has a
polkit grant). Output strings, flags and exit codes have parsers — Hermes'
skills (`skills/` here) and its crons. Adding new lines/verbs/flags is always
fine; before rewording or removing *existing* output, grep those consumers for
the string (see "Output compatibility" in `DEVELOPMENT.md`).

**After any change, commit AND push** — nothing else version-controls it:

    git -C /data/arr add -A && git -C /data/arr commit -m "..." && git -C /data/arr push

Adding a *new* service API key still needs a nixos rebuild (key goes in the
`arr-cli.env` sops template). Run `arr --help` for the command list.

`arr.py` is the frozen legacy Python CLI, kept only as the wrapper's bootstrap
fallback for a fresh `/data` re-clone (before the first `cargo build`) and as
the porting reference. Don't add features to it. The old
`download-notifier.py` lives in git history (nixos-config repo, removed at
cutover; its port is `crates/arr-notifierd`).

`tests/parity.sh` diffs Python-vs-Rust output on read-only commands — useful
if `arr.py` and the Rust CLI ever need to be compared again.

## Audiobooks — `arr abook`

abook.link is a logged-in SMF spotting forum, not an indexer API. Keep acquisition
inspectable with four commands:

```sh
arr abook search 'Harry Potter Stone'
arr abook reveal 60298
arr abook nzb 'abook.link - f08fd8f0943a4570a4'
arr abook grab '<NZB URL from results>' --name 'Author - Title (Narrator)' --password '<password from reveal>'
arr sab queue 'Author - Title'
arr sab history 'Author - Title'
```

`reveal` presses **Say Thanks on Andre's forum account** when the opening post
requires it; it never posts replies. It prints metadata, revealed content,
search strings and the optional password. `nzb` tries NZBIndex → Binsearch →
NZBKing, stopping at the first results. Use `--site nzbking` for an explicit
provider, especially for old spots; shorten a missing subject to its distinctive
token. Up to 25 candidates are printed, with download URLs and next commands.
`grab` requires a clean `--name`, uploads the fetched NZB to SAB's `audiobooks`
category, and prints the job id. Omit `--password` for unpassworded archives.
Provider-qualified `nzbindex:<id>` and `nzbking:<id>` also work; Binsearch uses
its full result URL. Confirm the candidate matches the topic before submitting.

Set `ABOOK_USERNAME` / `ABOOK_PASSWORD` in the environment or existing
`ARR_ENV_FILE` (default `/run/secrets/rendered/arr-cli.env`). Cookies are cached
per user in `$XDG_CACHE_HOME/arr/abook/cookies.json` (or `~/.cache/arr/abook/`),
with directory mode 0700 and file mode 0600; expired sessions reauthenticate.
No credentials are sent to the NZB providers.

The category should unpack into `/data/media/audiobooks` for Jellyfin's Books
library. SAB's **systemd writable paths** must include that destination too;
otherwise a completed download fails post-processing with `Read-only file
system`. Library layout and Jellyfin visibility should be checked after unpacking.
Audiobooks do not use the download-notifier; the caller notifies the requester.

Validation: `cargo test --workspace`, `cargo clippy --workspace --all-targets`,
and `cargo build --release`. HTML parser tests use small sanitized fixtures in
`crates/arr-api/src/abook.rs`; live forum/NZB/SAB requests are manual integration
tests so routine tests never press Thanks or queue downloads.
