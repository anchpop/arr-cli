---
name: requested-show-coverage-policy
description: Use when handling requests to add, get, or verify TV shows in Sonarr/Sonarr Anime; covers per-season coverage checks, proactive dub/sub + duplicate-file reporting, fixing partial monitored seasons, and asking before grabbing whole missing/unmonitored seasons.
version: 2.1.0
author: Hermes Agent
license: MIT
metadata:
  hermes:
    tags: [media-server, sonarr, sonarr-anime, tv, requests]
    related_skills: [media-server-operations, media-subtitle-maintenance]
---

# Requested Show Coverage Policy

Use alongside `media-server-operations` for requests like "add this show," "get this show," or "make sure we have this" — including shows already present, and multi-title requests mixing new and existing. The guiding idea: the first reply should cover the show's whole health — coverage, dub/sub state, library cleanliness — so the user doesn't have to come back with "subs and dubs?" or "the first episode is wrong."

## Workflow for a Requested Show

1. Pick the instance: `sonarr-anime` for anime, `sonarr` for everything else.
2. Start with `add` — it's the one-shot entry point and does the checking for you:
   ```sh
   arr sonarr-anime add '<show>' --requester <discordId>
   ```
   When the request carries a language condition ("with English subs", "in Japanese"), add `--require-subs eng` / `--require-audio jpn`. The download-notifier then verifies the requirement at ready-time, shows "🔎 eng subs ✓" in the requester's DM, and escalates failures by itself — so no watcher cron is needed for this.
   For an already-present show, `add` prints per-season coverage plus warnings worth acting on in the same turn (step 4).
3. Reading the season table:
   - A complete monitored season needs nothing.
   - A partially downloaded monitored season is fixable right away: `arr <svc> coverage <id> --fix`, then verify queue/history.
   - A wholly missing or unmonitored season is the user's call — ask before grabbing, unless they clearly asked for everything ("all", "complete").
   - Missing unmonitored specials (S0) are worth a separate mention; they don't make the main series incomplete.
4. A `⚠ unmanaged video file(s)` warning means Jellyfin is showing ghost duplicates right now, so it's worth resolving before replying. `arr <svc> audit <id>` gives a per-file verdict:
   - `DUPLICATE of tracked:` → `arr <svc> audit <id> --quarantine --yes` moves the ghosts (and their sidecar subs) to `/data/hermes/quarantine/`, rescans, and refreshes Jellyfin. Nothing is deleted — restoring is moving the files back. One glance first: a dual-audio "duplicate" beside a sub-only tracked file may actually be the better file, which is worth surfacing.
   - `UNIMPORTED — maps to SxxEyy which has NO tracked file` → that's missing content; import it (`arr <svc> import ... --map abs`).
   - `unmatched` → identify it (`arr <svc> parse "<name>"`) before acting.
5. For anime and foreign-language shows, dub/sub coverage is part of the answer even when nobody asked:
   ```sh
   arr sonarr-anime coverage <id> --tracks
   ```
   Report eng audio and eng subs per season with the missing episode numbers. Missing subs on the main sonarr chain into Bazarr with `--fix-subs`; on `sonarr-anime`, Bazarr doesn't apply, so gaps there mean finding a dual-audio/subbed release (`arr sonarr-anime releases <id> --season N --audio eng --timeout 600` — anime indexer searches run long).
6. After starting a fix, verify from a couple of angles before reporting:
   ```sh
   arr sonarr status '<show>'
   arr sonarr queue '<distinct show terms>' || true
   arr sonarr history '<show>' | head -40
   ```

## Communication Pattern

The first reply reads like a health report:

- "Already present and complete: 24/24. English dub 20/24 (missing E02, E04, E08, E10), English subs 21/24 (missing E16, E18, E19) — searching for dual-audio replacements now."
- "S3 is partially missing, so I triggered a search for the missing monitored episodes."
- "Found 3 leftover duplicate files Jellyfin was showing as ghost episodes — quarantined them and refreshed Jellyfin."
- "S4 is entirely missing and unmonitored — want me to grab it?"
- "Only specials are missing/unmonitored; the main series is complete."

## Things That Have Bitten Before

- Aggregate `N/N` and percent figures hide partial seasons — the per-season lines from `add`/`status`/`coverage` are the real signal, warnings included.
- "Complete 24/24" with four missing dubs left an anime requester unsatisfied; `coverage --tracks` before replying would have answered the question they were about to ask.
- The ⚠ unmanaged-file warning is the "first episode is wrong" complaint in embryo — cheaper to fix before anyone notices.
- Files the audit marks UNIMPORTED are content, not clutter; importing keeps them, quarantining would lose real episodes.
- Grabbing whole unmonitored seasons unasked has surprised users; a quick question first has never hurt.
- "Queued" and "downloaded" read the same in a report but aren't — saying which is which avoids walking claims back later.

## Verification Checklist

- [ ] Checked the show in the right Sonarr instance via `arr ... add` (or `status`/`coverage` for a pure check).
- [ ] Acted on any ⚠ unmanaged-file warning with `arr ... audit`.
- [ ] For anime/foreign-language shows, included dub/sub state (`coverage --tracks`) in the first reply.
- [ ] Fixed partial monitored seasons and verified via queue/history.
- [ ] Asked before grabbing whole missing/unmonitored seasons (unless "all/complete" was explicit).
- [ ] Mentioned missing unmonitored specials separately from main-season coverage.
