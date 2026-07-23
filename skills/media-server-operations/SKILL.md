---
name: media-server-operations
description: "Use when administering the Sonarr/Radarr/SABnzbd/qBittorrent/Jellyfin media stack: missing or stuck requests, failed downloads, import blocks, duplicate/wrong episodes, release selection, library scans and safe library fixes — including when woken by the queue-stuck / download-failed / language-gap webhooks. Not for media generation (music/image/video creation)."
version: 3.0.0
author: Hermes Agent
license: MIT
metadata:
  hermes:
    tags: [media-server, sonarr, radarr, sabnzbd, jellyfin, arr, imports]
    related_skills: []
---

# Media Server Operations

When safe, you should assist users with their issues with this media server. The most common issues are requests for media, stuck downloads, and subtitle/language issues. This media server has one radarr instance and two sonarr instances (one for regular shows and one for anime).

Most routine operation of the services on this media server can be carried out with the local `arr` CLI.

It's better to use `arr` when possible because it can capture institutional knowledge and patterns, but direct REST calls of course still fine if you find the capabilities offered by `arr` to be insufficient. `arr --help` is the flag reference.

## Cookbook

**Someone asks for a movie or show.**
`arr radarr add '<title>' --requester <theirDiscordId>` — or `arr sonarr add` / `arr sonarr-anime add` for shows (anime belongs on the anime instance). If the request mentions language, add `--require-subs eng` and/or `--require-audio <lang>`; when they didn't spell it out, pick a sensible default (foreign-language title → original audio + English subs) and say what you picked so it's easy to correct. The command refuses ambiguous titles (disambiguate with `arr <svc> lookup`, re-add with `--tvdb`/`--tmdb`), and its output already answers the request: what got grabbed (it waits up to a minute and bumps the grab to the front of the download queue) or, for a show that's already in the library, its per-season coverage and any warnings. Reply from that output. From there the download-notifier DMs the requester a live progress embed, confirms the item in Jellyfin, and ffprobe-verifies any require-* tags on the finished files — following up is its job, not yours.

**"Where's my request?" / something didn't arrive.**
`arr <svc> status '<title>'` — a single match prints per-season gaps and unmanaged-file warnings. `arr seerr unfulfilled` cross-checks every website request against actual disk state. `arr <svc> releases '<title>' --season N` shows what the indexers have and why candidates were rejected (anime indexer searches run long; `--timeout 600`). `arr <svc> history '<title>'` shows grabs and failures. Old obscure content often has only dead releases — after the arr exhausts its candidates, "currently unobtainable" is a real and honest answer.

**A queue item is stuck or failing** (including `queue-stuck` / `download-failed` webhook wakes).
Look before fixing: `arr <svc> stuck` — items that recovered or are progressing need nothing. For the rest, `arr <svc> stuck --fix` plans the safe repairs and applies them with `--yes` (maps import-blocked packs onto episodes, clears stale records, flags junk releases). Dead releases get `arr <svc> queue-rm <id> --blocklist --yes` so the arr grabs an alternative; real content that won't auto-match gets a manual import (below). The notifier keeps requester DMs updated by itself — no need to message anyone or create crons.

**Jellyfin shows a duplicate or wrong episode.**
`arr <svc> audit '<show>'` compares the disk against the arr's records and gives a per-file verdict: DUPLICATE → `--quarantine --yes` moves it (plus sidecar subs) to `/data/hermes/quarantine/`, rescans the arr, refreshes Jellyfin — nothing is deleted, restoring is moving it back; UNIMPORTED → that's missing content, import it rather than quarantine; JELLYFIN VERSION (`Title (Year) - Label.mkv`) → intentional multi-version naming, leave it. One glance before quarantining a dup: if it's dual-audio and the tracked file isn't, the "duplicate" may be the better file — surface that instead.

**Subtitles or dubs missing.**
`arr <svc> coverage '<show>' --tracks` reads the actual files (embedded streams + sidecars) per season. On main sonarr and radarr, Bazarr fetches missing subs (`--fix-subs`, or `arr bazarr search --series/--movie`); check `arr bazarr status` when nothing arrives — a throttled/expired provider is the usual cause. The anime instance is not Bazarr-covered: missing subs or dubs there mean finding a dual-audio or subbed release, though the anime profile already prefers dual-audio so a plain re-grab often does it. Missing audio is never fetchable as a file — it always means a different release.

**Follow up on a download nobody is tagged on.**
Requester-tagged downloads need nothing — the notifier handles progress, ready, and verification. For the rest, one `arr <svc> watch '<title>' [--until in-jellyfin] [--verify-subs eng] --quiet --once` in a bounded cron: each firing prints only news — one alert if it gets stuck/stalled/fails verification, one READY when done, silence otherwise (`--once` makes it remember what it already announced, so repeats stay quiet; an alert re-arms if the item recovers and breaks again). Remove the cron once READY lands.

**An episode won't play, or plays in the wrong language.**
For won't-play: find the exact playing path from the active Jellyfin session, decode-test it with ffmpeg — old AVIs and odd codecs sit green in every dashboard and still fail to decode (`Packet too small`, `Invalid buffer size`); replace that exact file. For wrong-language: the file often has the right audio with the wrong track marked default — a no-transcode remux that only flips dispositions fixes it without quality loss. Verify replacement files' streams before declaring victory; releases arrive with commentary tracks and mislabeled defaults. See `references/jellyfin-playback-corrupt-episode-replacement.md` and `references/jellyfin-audio-language-default-repair.md`.

**Manual import (title mismatches, anime numbering).**
`arr sonarr import <folder> --series <id> --match '<distinct token>' [--season N] --map se|abs --dry-run`, review every mapping, then rerun without `--dry-run`. `--match` matters in shared download folders — without it every file in the folder is considered. `--mode move` cleans up completed downloads; `--mode copy` preserves seeding torrents and staged files. Move-imports replace the existing managed file per episode — a question for the user if they didn't ask for replacement. Anime specifics below.

## Anime import pitfalls

- Dub releases usually carry absolute numbering: `--map abs`, and review the dry-run mapping.
- Sonarr's queue can map a Blu-ray-box release to a different episode than its title suggests — raw queue mappings are worth a look for absolute-numbered `[Group] Title E##` releases.
- When the wrong season/OVA may have imported under the right numbers, file contents beat filenames: extract an embedded subtitle header (`ffmpeg -map 0:s:0`) and read the title lines.
- Some series' Sonarr episode order differs from absolute/session release order — when filename title and Sonarr episode title disagree, build the explicit episode table and map by title/absolute number (`references/sonarr-anime-numbering-repair.md`).
- Localized alternate titles (`… | 灵笼` vs `Ling Cage`) grab fine but import-block; staging verified files into a small temp folder with explicit `--map se` is the clean path (`references/sonarr-anime-alt-title-import-blocked.md`).
- A movie release attached to an episode queue row imports into the wrong slot — remove it with `removeFromClient=true`, usually blocklist too.
- One dub release may cover only part of a show; report exactly which episodes got replaced.
- Sonarr's language labels are unreliable for dubs (untagged files show `Japanese`/`Unknown`) — release names and ffprobe are the evidence.

## This setup

- **Big files are welcome.** A media-encoder re-encodes everything after import (movies → AV1, shows → HEVC), so 30GB remuxes shrink on their own and the quality profiles select them on purpose. A slow big download wants an ETA, not a swap to a smaller encode. The exception is raw disc structures (BR-DISK/ISO/BDMV folders) — they neither import nor re-encode; replace with a single-file release.
- **TRaSH-synced profiles are the house standard.** They encode a lot of release wisdom, including blocking fake upscales (`AI UPSCALE`, upscale groups like `bluury` — garbage-tier no matter how good the name looks). The bypass paths (`--override`, `--via-sab`, direct `prowlarr grab`) skip all of that and print a reminder saying so; on those paths the vetting is yours, and deviating from the profile deserves a concrete reason.
- **The download-notifier is the requester-facing channel.** It tracks Seerr requests and `requester-*`-tagged items: live embed, Jellyfin-confirmed ready ping, language verification, and escalation — it kicks Bazarr itself and wakes you via webhook (`language-gap`, `download-failed`, `queue-stuck`) when judgment is needed. Woken means triage first: check current state, fix what actually needs help, reply with a short report.
- **Jellyfin API quirks:** the Shows endpoint can be stale right after a scan while recursive item search already sees the files; anime may display under a localized title while the filesystem and Sonarr use the romanized one — retry broad with `Fields=Path`.
- **Jellyfin watch history is weak evidence.** Missing `UserData` rows mean "no current playback data," not "unwatched" — reboots, restores, and re-identification detach history. Cleanup lists are candidates for review, never delete-ready; when a user says they watched something the data doesn't show, the data is the suspect.
- **SAB oddities:** aggregate `0.00MB left` can lie while per-job rows show real remaining bytes. Old completed-download folders may be owned by a stale dynamic UID — renameable but not deletable; report "needs privileged cleanup" when SAB history no longer knows them. qBittorrent `missingFiles` is often recoverable: if the data exists on disk, force a recheck before hunting replacements.
- **Jellyfin password-reset PINs:** verify the account exists (`/Users`) before initiating `/Users/ForgotPassword`, and read the `passwordreset*.json` it names before quoting a PIN — if the file isn't readable, say so rather than inventing a code.
- **Letterboxd plugin:** custom Jellyfin plugin, source in `/data/hermes/hermes-plugins/jellyfin-letterboxd/`; operate it via `/Plugins/{id}/Configuration` + `/Letterboxd/Sync`, not by editing its XML. Backfills come from user export zips (RSS is recent-only). See the letterboxd references.
- **External share links:** metadata-triage first (Transfer.it is Mega-backed — `references/transferit-mega-metadata-probe.md`). If a link is unofficial copyrighted media, stop after triage — no download or import; offer the legitimate alternatives instead.
- **Season pruning goes through Sonarr** (`arr sonarr delete --seasons`, or unmonitor + `DELETE /episodefile/<id>`) so the DB stays consistent and nothing silently re-downloads. `references/sonarr-safe-season-pruning.md`.

## References

Worked examples and long-form patterns, in `references/`:

- `sonarr-import-blocked-manual-import.md` — a stuck season fixed with explicit manual import.
- `sonarr-season-pack-partial-import-recovery.md` — partially imported season pack: missing-episode inventory, selective import, fresh search.
- `sonarr-anime-numbering-repair.md` — absolute/session numbering vs Sonarr episode order.
- `sonarr-anime-alt-title-import-blocked.md` — staged imports for localized-title blocks.
- `sonarr-anime-dub-replacement.md` — manual dub imports: mapping, partial coverage, metadata caveats.
- `anime-dub-targeting.md` — worked dub-targeting example.
- `sonarr-safe-season-pruning.md` — season deletion with consistent DB state.
- `radarr-missing-movie-availability.md` — no releases vs failed repair vs blocklist vs wrong same-title object.
- `radarr-missing-movie-and-wrong-match-cleanup.md` — wrong similarly-named folders, alternate-title rescue searches.
- `wrong-movie-and-download-cleanup.md` — wrong-movie workflow + completed-download permission pitfall.
- `radarr-movie-english-subtitle-direct-sab.md` — direct-SAB add when the profile rejects suitable releases.
- `ambiguous-sonarr-title-correction.md` — same-title/accent/year disambiguation.
- `bazarr-subliminal-subtitle-backfill.md` — subtitle backfill, provider pitfalls, verification.
- `subtitle-backfill-long-running-jobs.md` — broad subtitle scans and their reporting.
- `jellyfin-playback-corrupt-episode-replacement.md` — decode-test and replace a failing episode.
- `jellyfin-audio-language-default-repair.md` — wrong-default-audio disposition remux.
- `jellyfin-watch-history-cleanup-caution.md` — detached history vs "unwatched".
- `transferit-mega-metadata-probe.md` — metadata-only triage for share links.
- `jellyfin-letterboxd-plugin-v01.md`, `jellyfin-letterboxd-backfill.md`, `letterboxd-export-followup-and-import.md`, `letterboxd-jellyfin-recommend-and-request.md` — Letterboxd plugin operations.
- `seerr-unfulfilled-request-audit.md`, `seerr-unfulfilled-easy-fixes-and-radarr-manual-import.md` — website-request audits and their fixes.
