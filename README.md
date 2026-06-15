# arr-cli

Thin Python CLI over the Sonarr / Radarr / Prowlarr / SABnzbd / qBittorrent /
Jellyfin / Jellyseerr APIs for the beef.baby media box.

This repo is the **source of truth** for the `arr` command: the script lives at
`/data/arr/arr.py`; a NixOS wrapper runs it directly, so edits take effect
immediately — **no rebuild**. Both `andrep` and the `hermes` agent can edit it.

**After any change, commit AND push** — nothing else version-controls it:

    git -C /data/arr add -A && git -C /data/arr commit -m "..." && git -C /data/arr push

Adding a *new* service API key still needs a nixos rebuild (key goes in the
`arr-cli.env` sops template). Run `arr --help` for the command list.
