---
name: media-subtitle-maintenance
description: Audit and repair English subtitle availability across Jellyfin/Sonarr/Radarr media libraries, including foreign-audio titles, anime, series, and optional all-title subtitle passes.
---

# Media Subtitle Maintenance

Use this for requests like:

- “Add English subtitles for these movies/series.”
- “Scan all foreign titles and make sure they have English subtitles.”
- “Run the subtitle pass again, including English-audio titles.”
- “Why did Jellyfin not show subtitles for this title?”

This skill covers Jellyfin-side subtitle availability via sidecar or embedded English subtitle tracks. Prefer local verification with `ffprobe`/filesystem checks over assumptions from Radarr/Sonarr metadata alone.

## Core workflow

1. **Resolve the exact media item first.**
   - Use Radarr/Sonarr/Sonarr-anime metadata and filesystem paths.
   - Watch for alternate titles: e.g. a user may say “Baby Assassins: Nice Days” while Radarr has it as “Baby Assassins 3 (2024)”.
   - For wrong-version complaints, verify TMDb/original title/year before deleting or replacing anything.

2. **Check current subtitle state — use the `arr` CLI first.**
   - Per-season dub/sub coverage in one command: `arr sonarr coverage '<show>' --tracks` (or `arr sonarr-anime ...`); per-file details: `arr <svc> tracks '<item>' --missing-subs eng` / `--missing-audio eng`. These already combine ffprobe embedded-stream inspection with sidecar detection — prefer them over hand-rolled loops.
   - **Run this unprompted whenever a show/movie is requested or checked** (especially anime/foreign-language titles) and report dub/sub gaps in the same reply — the user should not have to follow up with “subs and dubs?”.
   - Raw `ffprobe`/sidecar globbing (below) is the fallback for cases the CLI doesn't cover: external sidecars are exact-stem language files like `.en.srt`, `.eng.srt`, `.en.ass`, `.eng.ass`, `.en.vtt`, `.eng.vtt`.
   - For Jellyfin, embedded `eng` subtitle streams count as available English subtitles even with no sidecar file.

3. **Download English subtitles when missing — Bazarr first, subliminal as fallback.**
   - Bazarr holds the OpenSubtitles VIP membership and covers the MAIN Sonarr + Radarr: `arr sonarr coverage '<show>' --tracks --fix-subs`, or directly `arr bazarr search --series '<show>'` / `--movie '<title>'`; check provider health/backlog with `arr bazarr status` / `arr bazarr wanted`.
   - Bazarr does NOT cover `sonarr-anime` — anime subs must come from a subbed/dual-audio release (`arr sonarr-anime releases <id> --season N --audio eng --timeout 600`; anime indexer searches are slow, use the raised timeout) or a fansub subtitle pack (see the anime pitfall below).
   - Use `subliminal` from Nix when Bazarr doesn't apply or comes up empty:

     ```bash
     nix shell nixpkgs#ffmpeg-headless nixpkgs#python313Packages.subliminal -c \
       subliminal download -l en -p opensubtitles -p opensubtitlescom -p bsplayer -p tvsubtitles -r metadata -r hash -m 10 '<media-file-or-season-dir>'
     ```

     When files already carry embedded English subs but Jellyfin-visible sidecars are wanted, add `--force-embedded-subtitles` (or `--force` to also replace existing sidecars).
   - Prefer targeted runs for specific user-reported titles before broad scans.
   - For long library-wide runs, run in background with completion notification and keep TSV/log files under `/data/hermes/subtitle-audit/`.

4. **Refresh Jellyfin and verify.**
   - Trigger a Jellyfin library refresh after adding sidecars.
   - Recheck actual file/stream state; do not report success based only on subtitle-provider output.

## Broad audit patterns

Use two passes for robust library audits:

1. **Audio-tag pass:** scan all videos, treat tagged non-English audio with no English subtitle as candidates. Log audio-unknown files instead of guessing.
2. **Metadata pass:** use Radarr/Sonarr/Sonarr-anime `originalLanguage` to catch files whose audio streams are untagged. This is especially important for anime and remuxes.

If the user asks to include English-audio titles, run an **all-title English subtitle pass** that skips only files that already have English subtitles; do not skip English audio.

## Forced-sidecar repair from embedded non-English forced streams

If a release has embedded forced subtitles in a non-English language (for example French `Forced`) but Jellyfin needs an English forced sidecar for foreign-language dialogue, use the non-English forced stream as the timing/source scaffold. Extract it with `ffmpeg -map 0:<subtitle_stream_index>`, translate or map the cues into English, save beside the media as `<stem>.eng.forced.srt`, `chmod 664`, validate with `ffprobe -f srt`, refresh Jellyfin, and verify `MediaStreams` shows `Language=eng`, `IsExternal=true`, `IsForced=true`. See `references/forced-subtitles-from-foreign-forced-streams.md` for the worked pattern.

## Critical pitfalls

- **Do not use unquoted glob arrays to detect sidecars.** A bug from a prior session treated literal non-matching patterns as real files, producing false `HAVE_EN_SUB` results. Always check `[[ -f "$candidate" ]]`, `find -type f`, or enable `nullglob` correctly and verify the array contains real paths.
- **Do not trust a title string alone.** Confirm exact movie/year/TMDb. `Santa Claus Is a Stinker (1985)` was a play/incorrect item; the intended film was `Santa Claus Is a Stinker (1982)` / original title `Le père Noël est une ordure`.
- **Provider no-match is not necessarily final.** Try original titles, alternate titles, and exact filenames. Some obscure films still may have no downloadable English subtitle from configured providers.
- **Anime/fansub subtitles may live outside normal subtitle providers.** If `subliminal`/provider search fails for anime or donghua, search release pages (Nyaa, fansub group notes) for subtitle-only packs or “subs and fonts” folders. Install language-tagged sidecars with exact media stems (`.eng.ass`/`.eng.srt`) and verify timing with duration/last-dialogue checks before reporting success.
- **Embedded subtitle language tags matter.** If a file has an embedded subtitle track but Jellyfin shows unknown/no English subtitle, inspect `ffprobe` language tags and remux with `-map 0 -c copy -metadata:s:s:0 language=eng` rather than downloading a duplicate sidecar.
- **Avoid concurrent full-library subtitle jobs.** Queue the next pass after the current PID exits; subtitle providers can be slow and rate/error-prone.
- **SIGTERM/exit -15 means incomplete.** Summarize partial counts clearly and restart/continue if the user still wants the pass.

## Verification snippets

Check audio/subtitle streams:

```bash
nix shell nixpkgs#ffmpeg-headless -c ffprobe -v error \
  -show_entries stream=index,codec_type:stream_tags=language,title \
  -of json '<file>' | jq -r '.streams[] | select(.codec_type=="audio" or .codec_type=="subtitle") | [.index,.codec_type,(.tags.language//""),(.tags.title//"")] | @tsv'
```

Count report statuses:

```bash
cut -f1 /data/hermes/subtitle-audit/<report>.tsv | sort | uniq -c | sort -nr
```

## Supporting references

- `references/2026-06-subtitle-audit.md` records the session-derived script patterns, false-positive sidecar bug, and title-resolution examples that motivated this skill.
- `references/ling-cage-fansub-sidecar-repair.md` captures a concrete anime/donghua workflow: provider search failed, a fansub subtitle/font pack was found via a release page, `.eng.ass` sidecars were installed, one embedded subtitle stream was retagged, and Jellyfin was verified by path/display-name search.
