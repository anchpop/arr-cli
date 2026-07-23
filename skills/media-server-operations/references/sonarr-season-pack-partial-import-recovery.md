# Sonarr season-pack partial import recovery

Use this when a season pack completes but Sonarr imports only some episodes, then leaves multiple `completed/importBlocked` queue rows for the same download.

## Symptom

- `arr sonarr seasons '<show>'` shows a season partially complete, e.g. `S2 5/8 on disk` while other seasons are complete.
- `arr sonarr queue` shows repeated rows for the same season-pack title as `completed/importBlocked`.
- Raw queue status messages may say:
  - `One or more episodes expected in this release were not imported or missing from the release`
  - `Episode file already imported ...`
  - `Single episode file contains all episodes in seasons. Review file name or manually import`
- The completed folder contains oddly named files that do not parse cleanly, e.g. `Nathan.for.1.You.mkv`, `Nathan.for.2.You.mkv`, `Nathan.for.You.mkv`.

## Recovery pattern

1. Inventory exactly what is missing before changing anything:

```sh
arr sonarr seasons '<show>'
arr sonarr raw GET '/episode?seriesId=<id>&includeEpisodeFile=true' \
  | jq -r 'sort_by(.seasonNumber,.episodeNumber)[] | [.seasonNumber,.episodeNumber,.title,(if .hasFile then "ON_DISK" else "MISSING" end),(.episodeFile.relativePath // "")] | @tsv'
```

2. Inspect the raw queue messages and output path:

```sh
arr sonarr raw GET '/queue?includeUnknownSeriesItems=true&includeSeries=true&includeEpisode=true&pageSize=1000' \
  | jq '.records[] | select((.series.title//"")=="<show>") | {id,title,status,trackedDownloadStatus,trackedDownloadState,statusMessages,downloadId,outputPath,episode:{seasonNumber:.episode.seasonNumber,episodeNumber:.episode.episodeNumber,title:.episode.title}}'
```

3. List the completed files and, if useful, compare duration/size with already-imported files. If `ffprobe` is not on PATH, use `nix shell nixpkgs#ffmpeg -c ...`; do not record that as a durable failure.

4. Try a restrictive dry-run manual import against the completed folder. For weird names, match one file at a time so the helper cannot accidentally import an already-present or ambiguous file:

```sh
arr sonarr import '<completed-folder>' \
  --series '<show>' \
  --match '<exact-distinct-file-stem>' \
  --season <N> \
  --map se \
  --mode move \
  --dry-run
```

Only run without `--dry-run` if the single-file mapping points to the intended missing episode.

5. If remaining season-pack files are ambiguous or do not map to the missing episodes, ignore/remove the stale Sonarr queue rows without deleting the download client files, then rerun a normal season search for the missing monitored episodes:

```sh
arr sonarr raw DELETE '/queue/<queueId>?removeFromClient=false&blocklist=false'
arr sonarr grab '<show>' --season <N>
arr sonarr wait <commandId> --timeout 300
arr sab prio '<distinct show/season terms>' --top
```

6. Watch until import, then verify:

```sh
arr sonarr seasons '<show>'
arr sonarr raw GET '/episode?seriesId=<id>&includeEpisodeFile=true' \
  | jq -r 'sort_by(.seasonNumber,.episodeNumber)[] | select(.seasonNumber==<N>) | [.seasonNumber,.episodeNumber,.title,(if .hasFile then "ON_DISK" else "MISSING" end),(.episodeFile.relativePath // "")] | @tsv'
arr sonarr history '<show>' | head -40
curl -fsS -X POST "http://localhost:8096/Library/Refresh?api_key=$JELLYFIN_API_KEY" >/dev/null && echo refreshed
```

## Pitfalls

- Do not trust the headline `N/total` if specials are included. For normal-show completeness, compare monitored seasons/episodes directly.
- Do not blindly import all files from the season-pack folder; Sonarr may have already imported some of them or may map odd filenames to the wrong episode.
- When a season pack has already partially imported and still has warning rows, a fresh season search can be the cleanest way to fill only the genuinely missing episodes after stale queue rows are ignored.
