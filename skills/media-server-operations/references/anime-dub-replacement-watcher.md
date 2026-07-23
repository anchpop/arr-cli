# Anime Dub Replacement Watcher Pattern

Use this when a show is already complete in Sonarr/Sonarr Anime but the user asks to replace it with a specific dub / dual-audio release.

## Scenario

- Existing Sonarr Anime series is green / complete, but `ffprobe` shows only some episodes have English audio.
- Prowlarr has a full-season `Dual Audio` / `English Dubbed` release, often as a torrent batch.
- Sonarr may not automatically replace/import because the existing files already satisfy the normal cutoff, the release is a batch, or the download title does not map cleanly.

## Workflow

1. **Inventory current audio first.**
   - Get episode paths from `/episode?seriesId=<id>&includeEpisodeFile=true`.
   - Run `ffprobe` over audio streams and summarize which episodes have `eng` audio.
   - Report exact episode ranges rather than relying on Sonarr language labels.

2. **Search explicit dub terms across Prowlarr.**
   - Search variants such as `<title> English dub`, `<title> dual audio`, and `<title> <year> dual audio`.
   - Prefer exact title/year matches and avoid similarly named anime (e.g. `Talentless Nana`, `Nanatsu no Taizai`).
   - Dry-run the grab with a restrictive `--match` and, when needed, `--indexer` to avoid duplicate mirrors.

3. **Prefer reliable, importable releases over the largest release.**
   - A huge BD remux may be correct but slow/wasteful.
   - A smaller HEVC/WEB/BDRip dual-audio batch can be the better operational choice if it covers all episodes.
   - If a usenet release sits indefinitely in SAB `Grabbing` with `0.00MB`, remove that stuck SAB queue item and try another mirror/indexer rather than waiting forever.

4. **If using a torrent, use copy import.**
   - Keep torrent source files intact for seeding: `--mode copy`.
   - After enough files are present, dry-run manual import against the torrent content path:
     ```sh
     arr sonarr-anime import '<torrent content path>' \
       --series '<series>' --match 'S01E' --season 1 --map se --mode copy --dry-run
     ```
   - Do not import until the dry run maps the full expected count exactly.

5. **Use a silent watcher for long downloads.**
   - The watcher should stay silent while qBittorrent is downloading normally.
   - On completion, it should:
     1. confirm the expected folder exists,
     2. dry-run manual import and require the full episode count,
     3. import with `--mode copy` for torrents,
     4. re-run `ffprobe` on final Sonarr episode paths and require English audio on every episode,
     5. trigger Jellyfin refresh,
     6. print only success or an actionable blocker.

6. **Watch for stale queue rows.**
   - If a failed/stuck SAB job is removed and a torrent replacement is queued, Sonarr may briefly show a stale queue row for the removed SAB `downloadId`/title.
   - Cross-check qBittorrent/SAB directly before assuming the visible Sonarr queue row describes the active replacement.

## Reporting

- Say that the replacement is **queued/downloading** until import and `ffprobe` verification complete.
- Only say “all episodes have English dub” after verifying the final imported library files, not just after grabbing a dual-audio release.
