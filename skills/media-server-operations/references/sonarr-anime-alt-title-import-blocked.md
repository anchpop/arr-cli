# Sonarr Anime alternate-title import block: Ling Cage / 灵笼 pattern

## Trigger

Use this pattern when Sonarr Anime grabbed or tracked a release but leaves it at `completed/importBlocked` with a title mismatch, especially Chinese/Japanese anime releases whose release title includes an alternate/localized title, e.g. `Ling Cage S2 - 07 ... | 灵笼` while the Sonarr series is `Ling Cage`.

The durable lesson is not the specific title: Sonarr may have the right download attached by history/queue but still refuse automatic import because the release title cannot be parsed as the library series. Explicit manual import is correct only after verifying the files and mapping.

## Worked pattern

1. Verify season state and queue reason:

```sh
arr sonarr-anime seasons '<series>'
arr sonarr-anime queue '<series>'
arr sonarr-anime raw GET '/queue?includeUnknownSeriesItems=true&includeSeries=true&includeEpisode=true&pageSize=1000' \
  | jq -r '.records[] | select((.title|test("<distinct title>";"i"))) | [.title,.status,.trackedDownloadState,.trackedDownloadStatus,(.outputPath//""),(.statusMessages|tostring)] | @tsv'
```

2. Confirm completed source files exist and that in-progress files are not copied/imported prematurely. In this Ling Cage session, torrent output filenames eventually appeared as:

```text
[FSP DN] Ling Cage S2E01 [1080P B-Global].mkv
[FSP DN] Ling Cage S2E02 [1080P BGlobal].mkv
...
[FSP DN] Ling Cage S2E12 [1080P BGlobal].mkv
```

3. If the shared completed-download directory is large or still contains active downloads, avoid pointing `arr sonarr-anime import` at the whole shared root repeatedly. Sonarr's `/manualimport?folder=...` scan can be slow/hang when the folder contains many unrelated files or files still being written. Instead, create a small temporary staging folder containing only verified completed files for the target import:

```sh
work=/data/downloads/torrent/sonarr-anime/hermes-<series>-s<N>-import
mkdir -p "$work"
chmod 775 "$work"
# Copy only completed files selected from queue outputPath or verified filenames.
cp -f "/data/downloads/torrent/sonarr-anime/<episode-file>.mkv" "$work/"
chmod 664 "$work"/*
```

Use copies for torrent-sourced files when the source must remain intact for seeding. Hard links may fail under the service/sandbox policy; falling back to copies is fine.

4. Dry-run explicit SxxExx mapping, then import:

```sh
arr sonarr-anime import "$work" \
  --series '<series>' \
  --season <N> \
  --map se \
  --mode copy \
  --dry-run

arr sonarr-anime import "$work" \
  --series '<series>' \
  --season <N> \
  --map se \
  --mode copy \
  --wait --timeout 300
```

5. Verify the season count, then remove the temporary staging copies:

```sh
arr sonarr-anime seasons '<series>'
rm -rf "$work"
```

## Pitfalls

- Do not assume `0MB left` means all files are import-safe; inspect `status`/`trackedDownloadState` and source file presence. Some clients report queued/downloading rows with `0MB left` while the output is not ready.
- If a per-episode import from the shared directory times out, the issue may be scanning the shared folder rather than a bad episode. Stage the exact completed files into a small temporary folder and retry.
- If the final on-disk count is green, the Sonarr queue may still show stale `completed/importBlocked` rows for the original download because manual import bypassed the automatic queue parser. Verify season/files/history before deciding whether queue cleanup is needed.
- Preserve torrent source data with `--mode copy`; use `--mode move` for usenet/completed folders only when source deletion is desired.
- For mixed naming variants (`BGlobal` vs `B-Global`), select files by actual paths or queue `outputPath` rather than assuming a uniform filename template.
