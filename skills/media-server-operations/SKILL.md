---
name: media-server-operations
description: "Use when administering a Sonarr/Radarr/SABnzbd/Jellyfin-style media server: investigating missing requests, stuck imports, download queues, library scans, and safe media-library fixes."
version: 2.1.0
author: Hermes Agent
license: MIT
metadata:
  hermes:
    tags: [media-server, sonarr, radarr, sabnzbd, jellyfin, arr, imports]
    related_skills: []
---

# Media Server Operations

## Overview

Use this skill for hands-on administration of the media server stack: Sonarr/Radarr/Prowlarr/SABnzbd/qBittorrent/Jellyfin, especially when a requested title didn't appear in the library. The goal is to answer with verified operational state and, when safe, fix the issue rather than just explain it.

The local `arr` CLI already knows the service endpoints and API keys here, so it's the natural first tool for Sonarr/Radarr/SABnzbd work. Direct REST calls are there for when the CLI output is too summarized or missing a diagnostic field.

When an automation starts needing non-trivial Sonarr/Radarr/SAB/qBittorrent logic, the best home for that logic is the `arr` CLI itself — knowledge that lands there stays reusable, whereas one-off cron scripts tend to rot. (This skill lives in the same repo as the CLI, `/data/arr` — when a tool change makes part of this document stale, update both in the same commit.)

## When to Use

- A user asks why a requested movie/show didn't download or appear in Jellyfin.
- Sonarr/Radarr shows completed downloads stuck in queue or import blocked.
- SABnzbd/qBittorrent completed an item but the library still reports 0 files on disk.
- You need to inspect or manually import files from completed downloads into `/data/media`.
- You need to distinguish: not found, failed download, still downloading, import blocked, permissions issue, wrong series/movie match, or library scan delay.

Media *generation* (music/image/video creation) has its own skills.

## Download Watchers and Notifications

`arr watch --once` has cron memory built in, so a watcher needs no wrapper script or stamp files — the whole job is a bounded cron whose command is the bare CLI call:

```sh
arr radarr watch 'The True Don Quixote' 'The Man Who Killed Don Quixote' \
  --until in-jellyfin --verify-subs eng --max-age 72 --quiet --once
```

What `--once` provides (state in `~/.local/state/arr-watch.json`): READY prints exactly once and later firings stay silent; STUCK/STALLED/VERIFY-FAIL alert once and re-arm only if the item recovers and breaks again; transient API errors are retried and then treated as silent-pending rather than becoming false alerts. Multiple titles fit in one call (quote each); the exit code is the worst of them: 0 ready, 1 pending, 2 verify-fail, 3 stuck, 4 stalled. `--verify-audio` / `--verify-subs` run the post-import ffprobe check (embedded + sidecar) themselves, and `--until in-jellyfin` nudges and confirms the Jellyfin scan, so no separate refresh step is needed. Give the cron a bounded repeat count and remove it once READY lands. Older `references/*watch*.md` files describing bespoke scripts with stamp files predate `--once`.

Worth choosing the notification channel thoughtfully, because cron output arrives in chat wrapped in `Cronjob Response: … job_id …` boilerplate that reads as spam when repeated — while the download-notifier speaks through a single polished embed it keeps editing:

- "Tell them when it's downloaded" is fully covered by tagging the item `--requester <discordId>`: the download-notifier DMs that person a live progress embed and a Jellyfin-confirmed "ready to watch" ping. A cron for this would duplicate it in a worse format.
- "With English subtitles" / "in Japanese" requests are covered by stamping the requirement: `arr radarr add '<title>' --requester <id> --require-subs eng [--require-audio jpn]` (or `arr <svc> tag` for an existing item). The notifier ffprobes the imported files at ready-time and appends "🔎 eng subs ✓" (or "⚠ 1/3") to the embed. Failures escalate on their own: the notifier kicks Bazarr for main-instance sub gaps, re-probes every ~5 minutes for 48h (the ⚠ flips to ✓ in the user's DM as soon as files improve), and wakes this agent through the `language-gap` gateway webhook for gaps that need judgment (dubs, anime subs, Bazarr coming up empty). When that webhook wakes you, fix the gap it describes — a dual-audio/subbed release, an alternate-title Bazarr search — and verify with `arr <svc> tracks`; the notifier's recheck loop handles telling the requester by editing their embed.
- Stuck and failing downloads are also the notifier's job now: it wakes this agent through the `download-failed` webhook (a tracked download failing repeatedly or stalling) and the `queue-stuck` webhook (anything — including unattributed grabs — sitting failed/import-blocked for 15+ minutes, batched). When one of those wakes you, triage first: check current state, fix only what actually needs help (`stuck --fix` safe repairs, blocklist dead/junk releases, force-import real content), and reply with a short report. The old stuck-downloads-watchdog cron is gone — there's no need to recreate anything like it.
- A watch cron earns its place for the little that remains outside the notifier: multi-item verification sweeps or follow-ups with no requester attribution. `--quiet --once` keeps it to at most one failure alert and one READY across its whole life.

## Fast Diagnostic Workflow

### 1. Check the arr object first

For shows:

```sh
arr sonarr status '<title>'
arr sonarr get '<title>'
arr sonarr seasons '<title>'
arr sonarr queue | grep -i '<distinct title words>' || true
arr sonarr history '<title>' | head -80
```

For movies, the equivalent Radarr commands:

```sh
arr radarr status '<title>'
arr radarr get '<title>'
arr radarr queue | grep -i '<distinct title words>' || true
arr radarr history '<title>' | head -80
```

What to look for:

- `0/N eps on disk` or missing movie file: not imported yet.
- History `grabbed` events but no import events: downloaded or attempted, but not in the library.
- Queue state `completed/importBlocked`: finished in the download client but blocked from automatic import.
- Queue state still downloading: answer with progress and ETA.

### 2. Pull full queue diagnostics when summarized output says import blocked

The compact `arr ... queue` output often hides the useful reason. Query the raw API and inspect `trackedDownloadState`, `trackedDownloadStatus`, `statusMessages`, `outputPath`, and mapped episode/movie fields.

For Sonarr season packs, repeated `completed/importBlocked` rows for the same download after some episodes imported — with messages like `Single episode file contains all episodes in seasons` or `Episode file already imported` — usually mean a partially imported pack. Inventory the exact missing episodes before manual import; if the remaining pack files are ambiguous, it's fine to ignore the stale queue rows (keeping client data) and run a fresh season search for the missing monitored episodes. See `references/sonarr-season-pack-partial-import-recovery.md`.

For Sonarr:

```sh
arr sonarr raw GET '/queue?includeUnknownSeriesItems=true&includeSeries=true&includeEpisode=true&pageSize=1000' \
  | jq '.records[] | select(.title|test("<TitleRegex>";"i")) | {id,title,status,trackedDownloadStatus,trackedDownloadState,statusMessages,errorMessage,downloadId,outputPath,episode:.episode|{seasonNumber,episodeNumber,title},series:.series.title}'
```

For Radarr:

```sh
arr radarr raw GET '/queue?includeUnknownMovieItems=true&includeMovie=true&pageSize=1000' \
  | jq '.records[] | select(.title|test("<TitleRegex>";"i")) | {id,title,status,trackedDownloadStatus,trackedDownloadState,statusMessages,errorMessage,downloadId,outputPath,movie:.movie.title}'
```

### 3. Check completed download files before importing

Confirm the expected files exist under the queue `outputPath` or completed-download root. For a stuck Sonarr season, it's worth verifying the set of episode files matches expectations before a move import.

## Manual Import Pattern for Sonarr Import-Blocked Episodes

When Sonarr downloaded episodes but declines automatic import, manual import with a restrictive match filter and a dry run first is the reliable path.

### Dub/language replacement for anime

When a user asks for a specific anime dub, Sonarr usually can't request that directly — "dub" lives in release titles/tags. Search Prowlarr by terms like `English Dub`, `Dual Audio`, `Funimation Dub`, `Animax Dub`, and alternate/original titles. If the release completes as `completed/importBlocked` on a series-title mismatch, inspect the files and dry-run the mapping before importing.

Things this workflow has learned the hard way:

- Anime dub releases often use absolute numbering; `--map abs` plus a dry-run mapping review catches most surprises.
- Blu-ray-box releases can confuse Sonarr while still queued: the release title may show one episode while the queue object maps to another. Raw queue mappings are worth a look before declaring an anime queue healthy, especially for absolute-numbered `[Group] Title E## Episode Name` releases.
- If you suspect the wrong season/OVA got imported under the right episode numbers, file contents settle it faster than filenames. A non-destructive check: extract embedded ASS subtitle headers (`nix shell nixpkgs#ffmpeg -c ffmpeg -v error -y -i <file> -map 0:s:0 /tmp/check.ass`) and read `Title:` / `Video File:` plus a few title-card dialogue lines.
- Some anime have Sonarr `episodeNumber` order differing from absolute/session release order. When the filename title and Sonarr episode title disagree, build an explicit table of `episodeNumber`, `absoluteEpisodeNumber`, title, `hasFile`, and `episodeFile.relativePath`, map by title/absolute number, and use temporary Sonarr-compatible renamed copies where needed. See `references/sonarr-anime-numbering-repair.md`.
- Alternate/localized titles (a release ending in `| 灵笼` against a series named `Ling Cage`) can be correctly grabbed yet import-blocked. If the shared completed-download folder is large or has active torrent writes, staging just the verified files into a small temp folder (dir `775`, files `664`), dry-running `--map se`, and importing with `--mode copy` keeps things clean. See `references/sonarr-anime-alt-title-import-blocked.md`.
- Movie releases sometimes get tracked as TV episodes (`Movie`, `Knockin on Heaven's Door`, OVA titles). A movie release attached to an episode queue row is best removed with `removeFromClient=true` and usually `blocklist=true` so it can't import into the wrong slot.
- One dub release may cover only part of a show. Search multiple dub variants and report exactly which seasons/episodes were replaced and which remain subbed.
- Sonarr's language labels stay `Japanese`/`Unknown` when files lack embedded tags, so the release title, notes, and `ffprobe` are better evidence of a dub than Sonarr's language field.
- `--mode move` replaces the existing managed file per mapped episode — when the user hasn't already authorized replacing their subbed library, check first.

See `references/sonarr-anime-dub-replacement.md` for the full pattern, and `references/anime-dub-replacement-watcher.md` for replacing an already-complete series with a dual-audio batch (verify current audio with `ffprobe`, grab restrictively, dry-run the full import, `--mode copy` for torrents, re-verify audio on the final paths).

Typical command for SxxExx releases:

```sh
arr sonarr import /data/downloads/usenet/complete/sonarr \
  --series '<series title or id>' \
  --match '<distinct.release.prefix>' \
  --season <N> \
  --map se \
  --mode move \
  --dry-run
```

Review every dry-run mapping — each file should map to its intended episode exactly once (`Release.S01E10.mkv -> S1E10`) — then rerun without `--dry-run`.

`--mode move` keeps completed downloads from lingering on disk; `--mode copy` is for when preserving the source matters (seeding torrents, staged files).

### The `--match` habit

Always pass `--match` when importing from a shared completed-download directory — without it the import helper considers every file in the folder. A distinctive substring that selects only the target release family is enough.

## Jellyfin User Password Reset PINs

When someone asks for a Jellyfin password-reset PIN, verify the account exists first:

```sh
curl -fsS "http://localhost:8096/Users?api_key=$JELLYFIN_API_KEY" \
  | jq -r '.[] | [.Name,.Id,.HasPassword,.HasConfiguredPassword] | @tsv'
```

If a reset attempt appears in logs for a username that isn't in `/Users`, the honest answer is that no account/PIN exists for that name — ask for the correct username. To initiate a reset for a known user:

```sh
curl -fsS -X POST 'http://localhost:8096/Users/ForgotPassword' \
  -H 'Content-Type: application/json' \
  --data '{"EnteredUsername":"<username>"}' | jq .
```

A successful response may include `Action: "PinCode"`, `PinFile`, and an expiration. Check that the named `passwordreset*.json` actually exists before quoting a PIN — depending on path/sandbox behavior the file may not be readable from Hermes, in which case say so and fall back to admin-reset guidance.

## Custom Jellyfin Plugin Operations

When a feature belongs inside Jellyfin itself (like exposing server-member Letterboxd opinions by TMDb ID), the custom-plugin workflow beats a one-off script. Source of truth is `/data/hermes/hermes-plugins/` (Letterboxd plugin in `jellyfin-letterboxd/`). Build with `dotnet-sdk_9`, deploy with the plugin's `deploy.sh`, verify via `/Plugins`, then exercise the endpoints with the local API key.

For Letterboxd users, update live config through `/Plugins/{id}/Configuration` (POST the full user list, run `/Letterboxd/Sync`, verify per-user cache counts) rather than editing the XML — and keep the C# `PluginConfiguration.Users` default empty, since XML deserialization appends saved users onto non-empty defaults and duplicates them. See `references/jellyfin-letterboxd-plugin-v01.md`.

For historical backfills, remember RSS is recent-only (~50 entries): a user-provided Letterboxd export zip is complete, local, and avoids scraping fragility. Make the RSS sync merge with the existing cache before importing backfill data, and de-dupe by TMDb ID or title+year (export rows vary in URL for the same film). See `references/jellyfin-letterboxd-backfill.md`.

For recommendation asks that join one user's Letterboxd with another's Jellyfin history, the imported plugin cache/API is the data source; join by TMDb ID, exclude already-watched, check what's on disk, and add missing titles through Radarr when asked. See `references/letterboxd-jellyfin-recommend-and-request.md`.

When Andre asks whether someone sent their Letterboxd export, check memory/session history and the document cache for a `letterboxd*.zip` before re-sending instructions. Plain-text DMs work best — backticks inside double-quoted `discord-dm` arguments get eaten by shell command substitution. See `references/letterboxd-export-followup-and-import.md`.

## Verification After a Fix

After queue or import work, verify from at least two angles:

```sh
arr sonarr status '<title>'
arr sonarr history '<title>' | head -40
```

Then confirm files exist in the library path (e.g. `/data/media/tv/<Series>/Season <N>/`).

A fixed season shows imported episodes in history and the expected on-disk count in status. Sonarr may count specials in the total (`16/19 eps on disk` with season 1 complete and S0 unmonitored) — worth explaining when the headline fraction looks off.

Jellyfin may lag until a library scan runs; trigger a refresh via the API when you have a key, or mention the pending scan.

When verifying episodes through Jellyfin's API, the dedicated Shows endpoint is more reliable than a generic `Items?ParentId=` query, and including `Path` pays off when a user reports a wrong episode: Jellyfin indexes unmanaged files sitting beside Sonarr-managed ones, so Sonarr can be green while Jellyfin shows two entries for one episode. `arr audit` (next section) automates the whole diagnosis-and-quarantine loop.

```sh
SERIES_ID=$(curl -fsS "http://localhost:8096/Items?api_key=$JELLYFIN_API_KEY&Recursive=true&SearchTerm=<title>&Limit=10" \
  | jq -r '.Items[] | select(.Type=="Series" and .Name=="<exact title>") | .Id' | head -1)
curl -fsS "http://localhost:8096/Shows/$SERIES_ID/Episodes?api_key=$JELLYFIN_API_KEY&Fields=Path&Limit=200" \
  | jq -r '.TotalRecordCount'
```

If an exact-title search finds no series, retry broad/romanized — Jellyfin may display an anime under a localized metadata title while the filesystem and Sonarr use the romanized one:

```sh
curl -fsS "http://localhost:8096/Items?api_key=$JELLYFIN_API_KEY&Recursive=true&SearchTerm=<distinct-word>&Limit=50&Fields=Path" \
  | jq -r '.Items[] | [.Type,.Name,.Id,(.Path//"")] | @tsv'
```

If a scan was just triggered and the API shows only the series shell, check the scheduled-task state and retry `/Shows/<id>/Episodes` before concluding the scan failed.

For anime with alternate/season titles, Sonarr can import a release from an alternate season title into the wrong season while still reporting the season complete — so the mapped files and source titles are worth a look beyond the aggregate count:

```sh
arr sonarr-anime raw GET '/episode?seriesId=<id>&includeEpisodeFile=true' \
  | jq -r 'sort_by(.seasonNumber,.episodeNumber)[] | [.seasonNumber,.episodeNumber,.absoluteEpisodeNumber,.title,(.episodeFile.relativePath // "MISSING")] | @tsv'
arr sonarr-anime raw GET '/history/series?seriesId=<id>&pageSize=1000&includeSeries=true&includeEpisode=true' \
  | jq -r '.[] | [.date,.eventType,.sourceTitle,(.data.droppedPath//""),(.data.importedPath//"")] | @tsv'
```

A `Zan`/`Zoku`/OVA release filling plain season-1 slots is a real library-quality problem even with green counts; replace with verified correct releases mapped explicitly.

## Jellyfin Playback Failure on a Specific Episode

When a show "isn't loading," the file itself is the place to look, past service health and green on-disk counts: check the active Jellyfin session for the exact `NowPlayingItem.Path`, inspect `PlayState`/`TranscodingInfo`, and run an ffmpeg decode smoke test. Old AVI files and odd codecs appear fine in Jellyfin yet fail decode/transcode (`Packet too small`, `Invalid buffer size`); replacing the exact episode with a clean release fixes it. High system load can coexist with a corrupt file — pausing background encoders helps live playback, but still test the file.

After importing a replacement, check audio/subtitle streams and default dispositions before calling it fixed — replacements can carry commentary tracks, wrong-language defaults, or misleading tags. `ffprobe` on `codec_type`, `language`, `title`, `disposition.default` tells you; a no-transcode remux that only changes dispositions repairs a wrong default. See `references/jellyfin-playback-corrupt-episode-replacement.md`.

## Wrong Default Audio Language in Jellyfin

"Not in English" often means the file has English audio with a non-English track marked default. Audit stream language tags and default dispositions across the season; when English audio is present, a no-transcode remux that preserves all streams and flips the disposition is the gentlest fix. Verify the temp file before replacing, re-audit, refresh Jellyfin. See `references/jellyfin-audio-language-default-repair.md`.

## Adding or Checking a Requested TV Show

When a user asks to "add", "get", or "make sure we have" a show, check whether it already exists in Sonarr/Sonarr Anime first — and for present shows, per-season coverage tells the real story rather than the aggregate status line.

For an existing show:

- A partially downloaded monitored season (`0 < onDisk < total`) is fixable right away: trigger a search for the missing monitored episodes and verify queue/history.
- Whole seasons that are unmonitored/not-requested are the user's call — ask before grabbing, unless the request clearly meant complete coverage ("all", "everything").
- Missing unmonitored specials (S0) are worth mentioning separately; the main series isn't incomplete because of them.

Useful checks:

```sh
arr sonarr add '<show>'          # already-present shows get coverage + unmanaged-file warnings + a tracks prompt
arr sonarr status '<show>' || true
arr sonarr seasons '<show>'
arr sonarr episodes '<show>' --missing --monitored
```

For anime, run these against `sonarr-anime`.

A good first reply covers the show's whole health (see `requested-show-coverage-policy`): when `add`/`status`/`coverage` print a `⚠ unmanaged video file(s)` warning, resolving it in the same turn via `arr <svc> audit` spares the user from discovering ghost duplicates in Jellyfin themselves; and for anime/foreign-language shows, `arr <svc> coverage <id> --tracks` answers the dub/sub question before it gets asked.

## Duplicate Episodes / Wrong First Episode / Unmanaged Files (`arr audit`)

Complaints shaped like "the first episode is wrong" or "there are two copies of E01" are almost always unmanaged files: videos in the library folder that Sonarr/Radarr don't track (migration leftovers, manual copies, failed imports). Jellyfin scans the folder directly, so they show up beside the managed file as duplicates.

`arr audit` is the purpose-built loop for this — it replaces the old hand-rolled `curl /episodefile` + filesystem-diff + `cp` dance:

```sh
arr sonarr-anime audit '<show>'          # verdict per unmanaged file
arr sonarr-anime audit '<show>' --quarantine --yes   # move dups out, rescan, refresh Jellyfin
arr sonarr-anime audit --all             # library-wide sweep (also: sonarr, radarr)
```

The per-file verdicts point at the right fix:

- `DUPLICATE of tracked:` — quarantine it. Files (plus sidecar subs) move to `/data/hermes/quarantine/<svc>-<slug>-<date>/`; nothing is deleted, restoring is just moving back, and the command triggers the arr rescan + Jellyfin refresh itself. One glance first: if the duplicate is dual-audio and the tracked file isn't, the "duplicate" may be the better file — worth surfacing.
- `UNIMPORTED — maps to SxxEyy which has NO tracked file` — real missing content; import it (`arr <svc> import <folder> --series <id> --match <token> --map abs`).
- `unmatched` — identify it (`arr <svc> parse "<name>"`) before acting.
- `JELLYFIN VERSION` (radarr, `Title (Year) - Label.mkv`) — intentional multi-version naming; leave it (excluded from `--quarantine` by default).

Afterwards `arr <svc> audit <id>` reports clean and `arr jellyfin has '<title>'` shows one entry per episode.

## Adding a Requested TV Show with English Subtitles

One command covers the whole request:

```sh
arr sonarr add '<show>' --requester <discordId> --require-subs eng
```

Its output already contains everything the first reply needs: the disambiguated match (it refuses ambiguous titles and lists candidates), the profile it picked, what the search grabbed (it waits up to 60s and reports the release, promoted to the front of the download queue), or per-season coverage if the show was already present. When the output shows a grab, reply with that — status/history/queue/SAB checks would just re-derive what the add already said. When it reports "nothing grabbed yet," say the search is still running; the download-notifier DMs the requester when a grab starts, so polling on their behalf adds nothing.

Everything downstream is automatic for a tagged add: live progress embed, Jellyfin-confirmed ready ping, ffprobe subtitle verification at ready-time (the `--require-subs` tag), Bazarr kick + Hermes wake if subs are missing. "Subtitles verified" before files exist isn't claimable — the honest phrasing is that verification happens automatically after import.

Still-useful judgment around the edges:

- Choose reasonable language options rather than asking: a foreign-language show usually wants original audio + English subs (`--require-subs eng`); "in Japanese"/"in Korean" means `--require-audio` for that language plus English subs unless they said otherwise; anime requesters here usually want the English dub (the profile already prefers dual-audio releases, so `--require-audio eng` just makes the verification explicit). State the assumption in the reply — "getting it in Korean with English subs" — so it's trivially correctable.
- Terse multi-title requests ("andor and utopia"): act on the obvious matches, report meaningful ambiguity afterward rather than stalling to ask.
- A slightly-wrong title plus a strong disambiguator usually identifies the intended show — "17 Again (Korean drama)" is 18 Again (2020), TVDB 377956 — state the correction and proceed. `arr sonarr lookup '<show>'` helps when add refuses an ambiguous match; then re-add with `--tvdb`.
- For anime whose releases all get rejected by the profile, a clearly-labeled manual grab (`Dual Audio`, `Eng Sub`) into `sonarr-anime` gets further than "not available" — the manual paths print their own bypass caveat; vet names for upscale/LQ markers.
- If `/Shows/<id>/Episodes` returns 0 right after a refresh while recursive item search sees the files, the Shows endpoint is stale — cross-check with `Items?Recursive=true&IncludeItemTypes=Episode&Fields=Path,MediaStreams` before concluding the scan failed.

## Missing External Subtitles / Bazarr Backfill

For missing-subtitle reports, resolve the exact media paths first (titles may be localized or abbreviated), check existing sidecars, then reach for Bazarr — it's where the OpenSubtitles membership, language profiles, and arr mappings live. `subliminal` via Nix is a narrow per-file fallback when Bazarr can't help, and a direct subliminal run doesn't use the user's membership unless credentials were explicitly provided.

Fallback pattern:

```sh
nix shell nixpkgs#python313Packages.subliminal -c subliminal download \
  -l en -p opensubtitles -p tvsubtitles \
  -r metadata -r hash -m 10 \
  "/data/media/tv/<Show>/Season 1"
```

For obscure/localized films, retry with `-n` using original and English titles. If providers return `Downloaded 0 subtitle`, report the title as unavailable — that's a real and common outcome. See `references/bazarr-subliminal-subtitle-backfill.md`.

## Targeting Anime Dubs / Audio-Language Variants

Since 2026-07-22 the anime profile scores `Anime Dual Audio` at +101 (above the release-group tiers) and carries the TRaSH Unwanted formats (Upscaled, LQ, BR-DISK…), so a plain `arr sonarr-anime grab` now prefers dual-audio copies and avoids garbage by itself — start there, and reach for manual targeting only when the profile search comes up empty.

The TRaSH-synced profiles are the house standard: they encode a lot of accumulated release wisdom, so deviating from them (`--override`, `--via-sab`, direct `prowlarr grab`) deserves a concrete reason — usually "the profile found nothing and the user wants a copy anyway." On those manual paths the profile's protection doesn't apply, so vet release names yourself: fake upscales (`AI UPSCALE`, upscale groups like `bluury`) are garbage-tier no matter how appealing "Dual Audio 1080p HEVC" looks in the listing — one such pack ground SAB post-processing for a day — and BR-DISK/LQ-group markers are equally disqualifying.

A specific-dub request is a best-effort release-targeting task: Sonarr tracks language metadata, but "dub" lives in release titles.

```sh
arr sonarr-anime status '<show>' || arr sonarr status '<show>'
arr sonarr-anime lookup '<show>'
arr sonarr-anime raw GET '/episodefile?seriesId=<id>' \
  | jq -r 'group_by(.seasonNumber)[] | "S\(.[0].seasonNumber): " + (map(.languages[].name) | unique | join(", ")) + " (" + (length|tostring) + " files)"'
arr prowlarr search '<show> English dub' --limit 20
arr prowlarr search '<alternate title> English dub' --limit 20
```

When a result is explicitly labeled (`English Dub`, `Dual Audio`, `Dubbed`), grab just that release with a restrictive match and the right category:

```sh
arr prowlarr grab '<show> English dub' --match '<distinct dub release words>' --cat sonarr-anime --dry-run
arr prowlarr grab '<show> English dub' --match '<distinct dub release words>' --cat sonarr-anime
arr sab prio '<distinct release words>' --top
```

Be clear with the user about the limitation: normal requests can't reliably select a dub unless release metadata exposes it. When replacing existing files in an already-complete series, the grab is the halfway point — dry-run the full manual import and verify final audio streams with `ffprobe` before reporting the dub complete. See `references/anime-dub-targeting.md` (worked Sergeant Frog example) and `references/anime-dub-replacement-watcher.md`.

## Fast-Track a Download Already in SAB

Fresh `add`/`grab` downloads are promoted to the queue front automatically. For something *already* sitting in SAB behind a backlog, `arr sab prio '<distinct title words>' --top` promotes it — run it against more than one naming pattern, since queues hold both `Cowboy Bebop` and `Cowboy.Bebop.S01E...` forms and a single spaced-title match misses the dotted ones. Verify the intended rows show `prio=Force`. For follow-through that outlasts the turn, `arr <svc> watch '<title>' --quiet --once` in a bounded cron.

## External Direct-Download Links

For an external share link, do metadata triage before any download decision. Transfer.it links are Mega-backed: `https://bt7.api.mega.co.nz/cs` with actions `xi` (transfer info) and `f` (node listing) yields title, file count, and sizes without fetching anything. See `references/transferit-mega-metadata-probe.md`.

The policy boundary: if metadata or context shows the link is unofficial third-party copyrighted media (fan-uploaded 35mm scans of commercial films, say), stop after triage — no download, import, or library add. Offering the legitimate alternatives (organizing files already on the server, subtitles, normal arr workflows) keeps things helpful.

## Release sizes

Large BluRay rips and remuxes are good grabs here. The box runs an automatic media-encoder that re-encodes after import (movies → AV1 with film-grain synthesis, shows → HEVC), so a 30GB remux shrinks substantially on its own — and the high-quality source gives the encoder its best input. The quality profiles (TRaSH: UHD Bluray + WEB, Remux-1080p for anime) select for these on purpose, so a big grab is usually the profile working as intended. When someone's waiting on a slow large download, an ETA usually serves them better than a swap to a smaller release.

The exception is raw disc structures (`BR-DISK`, `COMPLETE BLURAY`, ISO/BDMV/VIDEO_TS folders): they don't auto-import and the encoder skips disc folders, so they're worth replacing with a single-file release — a remux is ideal. `references/radarr-anime-movie-brdisk-avoidance.md` predates the encoder; its size-avoidance advice really only applies to disc structures.

For a disc-structure queue item: remove and usually blocklist that specific item, then pick a single-file release with a restrictive Prowlarr dry run. Watch for duplicate mirrors and stale qBittorrent rows when replacing something SAB/Radarr already had active.

## Movie Request with English Subtitles

One command, same as shows:

```sh
arr radarr add '<movie>' --requester <discordId> --require-subs eng
```

The add output reports the grab (or coverage if present); the notifier handles progress, the Jellyfin-confirmed ready ping, and the ffprobe subtitle verification, escalating automatically if subs are missing. Judgment that still matters:

- Choose reasonable language options: foreign films default to original audio + English subs (`--require-subs eng`); an explicit "in French"/"original language" ask adds `--require-audio` for it. Say which combination you went with so the requester can correct it cheaply.
- Verify identity for foreign films (`arr radarr lookup`, original titles, year, TMDB) — same-title substitution is the classic failure; add with `--tmdb` when in doubt.
- If the movie exists in the wrong language, remove only the file (`arr radarr delete '<movie>' --file-only --yes`) so Radarr accepts a replacement instead of rejecting candidates with "existing file meets cutoff", keep it monitored, then search.
- When the profile rejects otherwise-suitable releases, one exact well-matched release straight to SAB (category `radarr`) is more precise than a broad `--override`. The bypass caveat applies — vet the name.

See `references/radarr-movie-english-subtitle-direct-sab.md` for the direct-SAB mechanics.

## Missing Movie / Availability Workflow

A movie in Radarr but absent from Jellyfin is one of four cases: no indexed releases, failed download, still downloading, or wrong movie object.

1. Check state and history:
   ```sh
   arr radarr status '<title>'
   arr radarr history '<title>' | tail -60
   arr radarr queue | grep -i '<distinct title words>' || true
   ```
2. If you just added it with `searchForMovie=true`, give the add-triggered search a few seconds before issuing a separate grab — doubling up enqueues the same release twice. If a duplicate appears, remove just the extra queue row (`DELETE /queue/<id>?removeFromClient=true&blocklist=false`) after matching its `downloadId`.
3. Check `arr radarr releases '<title>'`; if empty, search Prowlarr with English title, original title, romanizations, and year. For festival/obscure films, verify identity via `originalTitle`/IDs and search by director and romanized terms too — and if the user's link is just a trailer, say so.
4. If nothing is indexed, trigger one normal search (`arr radarr grab '<title>'`) and re-check before reporting "not currently available."
5. `downloadFailed` history deserves the exact failure — SAB repair failures like `Repair failed, not enough repair blocks` mean grabbing a different, non-blocklisted release.
6. For ambiguous titles, confirm `year`/`tmdbId`/`imdbId`/`originalTitle` via lookup — same-title films are easy to substitute by accident.

See `references/radarr-missing-movie-availability.md`.

## Wrong/Ambiguous Show Match Workflow

For ambiguous titles (remakes, year variants, diacritics), verify year, network, original language, and IDs before trusting a `status` match. If the existing Sonarr object is wrong but the intended series exists in lookup, add the intended one explicitly and search both accented and unaccented release-title variants. The wrong existing entry can stay unless cleanup is requested or clearly safe. See `references/ambiguous-sonarr-title-correction.md`.

## Wrong Movie / Jellyfin Mismatch Workflow

"Jellyfin shows the wrong movie" is usually a library problem, not a metadata one:

1. Search Radarr for the intended title and `/data/media/movies` for distinctive terms from both the intended and wrong titles.
2. If Radarr lacks the intended movie but a similarly named wrong folder exists, add the right one by TMDB lookup (`POST /movie` with `tmdbId`, profile, root, `searchForMovie=true`).
3. Delete the wrong folder once you've confirmed it's truly the wrong film.
4. Check queue/history; for a large incoming download, prioritize it and let the notifier/watch handle follow-through rather than declaring victory before import.

## Safe Sonarr Season Pruning

Season deletions go through Sonarr so the DB stays consistent and nothing silently re-downloads: verify the inventory, compute the exact episode-file IDs and size, unmonitor the seasons being removed (`arr sonarr monitor '<series>' s1,s2` to keep 1–2), then `DELETE /episodefile/<id>` per file. Verify kept seasons on disk, removed ones at `0/N`, and clear only empty season dirs. See `references/sonarr-safe-season-pruning.md`.

## Completed Download Cleanup Pitfall

Old SAB completed folders may be owned by a dynamic service UID (shows as e.g. `systemd-oom`), so Hermes can rename top-level folders but not delete contents. When SAB history no longer knows the items, report it as "removed from arr/queue; leftover files need privileged cleanup" with folder count/size — that's the accurate state.

## Common Import-Blocked Causes

### Sonarr history shows `grabbed`, but SAB has no job and nothing imported

Old `grabbed` events with 0 files on disk, no queue rows, and no SAB history usually mean a lost/expired download-client job, not unavailability.

```sh
arr sonarr status '<title>'
arr sonarr history '<title>' | head -80
arr sab queue '<distinct title words>' || true
curl -fsS "http://localhost:8085/api?mode=history&output=json&apikey=$SABNZBD_API_KEY&limit=1000" \
  | jq -r '.history.slots[] | [.completed,.status,.name,.fail_message,.nzo_id] | @tsv' \
  | grep -i '<distinct title words>' || true
arr sonarr raw GET '/blocklist?page=1&pageSize=1000&sortKey=date&sortDirection=descending' \
  | jq -r '.records[] | select((.series.title//""|test("<TitleRegex>";"i")) or (.sourceTitle//""|test("<TitleRegex>";"i"))) | [.date,.sourceTitle,.reason,(.message//"")] | @tsv'
```

If releases look acceptable and nothing's blocklisted, re-run the season/episode search — that's usually all it takes. Note SAB can show `Checking`/`Downloading` with a misleading aggregate `0.00MB left` while individual rows still have real `mbleft`; per-job rows tell the truth. Finish by verifying the import, not the new `grabbed` line.

### qBittorrent `missingFiles` state

`qBittorrent is reporting missing files` is often recoverable: query by hash, compare `content_path` against the filesystem, and if the data exists, force a recheck before hunting replacement releases:

```sh
curl -fsS 'http://127.0.0.1:8080/api/v2/torrents/info?hashes=<HASH1>|<HASH2>' \
  | jq '.[] | {name,hash,state,progress,amount_left,save_path,content_path}'

missing=$(curl -fsS 'http://127.0.0.1:8080/api/v2/torrents/info?filter=errored' \
  | jq -r '.[] | select(.state=="missingFiles") | .hash' | paste -sd '|' -)
[ -n "$missing" ] && curl -fsS -X POST --data-urlencode "hashes=$missing" \
  'http://127.0.0.1:8080/api/v2/torrents/recheck'
```

States should move to `checkingUP`/`queuedUP`/`stalledUP`; a recheck may then surface a second-stage `importBlocked` to handle with the manual-import workflow.

### The catalogue of blockers

1. **Matched by grab history/ID, not parseable.** Sonarr knows the item belongs to the series (it grabbed it) but can't parse the title for auto-import. Manual import with explicit mapping, after checking the files.
2. **Title/alternate-title mismatch.** Inspect `statusMessages` and parse output; manual import when the mapping is unambiguous.
3. **Permissions/path.** Download done, arr can't read it. If you stage Hermes-owned repair folders for manual import, `chmod 775` the dir and `664` the files so the service user can traverse — otherwise Sonarr reports `File doesn't exist` for files Hermes can see.
4. **Quality/profile rejection after download.** Read `statusMessages` before overriding.
5. **Jellyfin scan delay.** Files imported fine; the scan hasn't run.
6. **Radarr disc-structure (BR-DISK/ISO) import blocked.** The `arr import` helper is Sonarr-only, so for movies: remove/blocklist the disc-structure item and grab a single-file release — a large remux is ideal (see Release sizes).
7. **Radarr rejects a usable release on language/profile rules.** When the release is clearly the intended film: `arr radarr raw GET '/release?movieId=<id>'` right after a search, pick one release by exact title/size, and feed its `downloadUrl` to SAB (`arr sab add "$url" --cat radarr --name '<release title>'`). Category tracking attaches the download to Radarr. (`POST /release` wants the full guid URL and can still miss its cache, so the direct SAB add is the dependable route.) Clean up any duplicate queue row by `downloadId`.
8. **Wrong similarly-named movie in Jellyfin.** Verify the actual folder/file (e.g. `Groove (2000)` vs `The Emperor's New Groove`), remove the confirmed-wrong folder, add the correct TMDB movie, search.

## Communication Style for User-Facing Replies

Concise and operational reads best:

- Did it download, import, or fail?
- The exact blocker, in plain English.
- What you changed and how you verified it.
- Any remaining wait state (e.g. Jellyfin scan pending).
- Internal debugging tangents translate into one plain sentence, or drop out entirely.

Watch-history / cleanup reports deserve extra care: a missing Jellyfin `UserData` row means "no current playback data found," which is weaker than "unwatched" — reboots, restores, and re-identification can detach history from current item IDs. When a user says they watched something the data doesn't show, the data is the suspect. Present cleanup lists as candidates for review, not delete-ready.

## References

- `references/sonarr-import-blocked-manual-import.md` — a completed Sonarr season stuck in `completed/importBlocked`, fixed with explicit manual import.
- `references/sonarr-season-pack-partial-import-recovery.md` — recover a partially imported season pack: exact missing-episode inventory, selective manual import, stale-queue ignore, fresh search.
- `references/radarr-missing-movie-and-wrong-match-cleanup.md` — wrong similarly named movie folders, disc-structure import blocks, alternate-title rescue searches.
- `references/jellyfin-watch-history-cleanup-caution.md` — missing playback rows can be detached history, not proof of unwatched.
- `references/wrong-movie-and-download-cleanup.md` — wrong movie match workflow + stale completed-download permission pitfall.
- `references/radarr-missing-movie-availability.md` — no releases vs failed repair vs blocklist vs wrong same-title object.
- `references/radarr-movie-english-subtitle-direct-sab.md` — subtitle-verified movie fulfillment via direct SAB add when the profile rejects suitable releases.
- `references/ambiguous-sonarr-title-correction.md` — same-title/accent/year ambiguity anchored on TVDB/year/network.
- `references/sonarr-safe-season-pruning.md` — delete selected seasons through Sonarr, keeping DB state consistent.
- `references/sonarr-anime-dub-replacement.md` — manual anime dub imports: absolute mapping, partial coverage, language-metadata caveats.
- `references/anime-dub-targeting.md` — worked anime dub targeting example.
- `references/bazarr-subliminal-subtitle-backfill.md` — subtitle backfill with Bazarr context or subliminal, provider pitfalls, verification.
- `references/subtitle-backfill-long-running-jobs.md` — broad subtitle scans: state verification, TSV counts, unavailable-provider tails.
- `references/transferit-mega-metadata-probe.md` — metadata-only triage for Transfer.it/Mega links + the copyrighted-media boundary.
- `references/jellyfin-letterboxd-plugin-v01.md` — operate the custom Letterboxd plugin.
- `references/jellyfin-letterboxd-backfill.md` — historical Letterboxd backfill that survives RSS syncs.
- `references/letterboxd-export-followup-and-import.md` — export follow-up flow + shell-quoting caveat for DMs.
- `references/seerr-unfulfilled-request-audit.md` — audit Seerr requested-but-missing items.
- `references/seerr-unfulfilled-easy-fixes-and-radarr-manual-import.md` — applying the safe fixes from that audit.
- `references/sonarr-anime-numbering-repair.md` — absolute/session numbering vs Sonarr episode order repairs.
- `references/sonarr-anime-alt-title-import-blocked.md` — staged imports for alternate/localized-title blocks.
- `references/jellyfin-playback-corrupt-episode-replacement.md` — decode-test and replace a failing episode.
- `references/jellyfin-audio-language-default-repair.md` — wrong-default-audio repair via disposition-only remux.
- `references/jellyfin-unmanaged-duplicate-episodes.md` — unmanaged-duplicate diagnosis (automated by `arr audit`).

## Verification Checklist

- [ ] Checked arr status/get/history/queue for the requested title.
- [ ] If import blocked, inspected raw queue `statusMessages`.
- [ ] Verified completed files exist before importing.
- [ ] Ran manual import with a restrictive `--match` and `--dry-run` first.
- [ ] Verified arr on-disk count and import history after changes.
- [ ] Verified files are present in `/data/media` when claiming the fix is complete.
- [ ] Told the user if Jellyfin still needs a library scan.
