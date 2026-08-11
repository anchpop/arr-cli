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
