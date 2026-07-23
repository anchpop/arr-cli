# Sonarr misnamed single-episode force import

Use this when a real completed single-episode download is import-blocked because its internal video filename points at a different episode, or the queue item is no longer linked to a series.

## Identity hierarchy

Do not trust the inner filename alone. Establish the intended episode from several independent signals:

1. Sonarr grab history: query recent `/history` records by distinctive release-title terms and inspect the episode ID, season/episode, series ID, and `sourceTitle` recorded at grab time.
2. Release metadata: read an included NFO/SFV and compare title, plot, dimensions, runtime, and language with the release title.
3. Media probe: use `ffprobe` to reject samples and confirm the main file has a plausible runtime/streams.
4. Current episode state: query `/episode?seriesId=<id>&includeEpisodeFile=true` and confirm the intended episode is missing while any episode suggested by the misleading inner filename is already present or otherwise inconsistent.

The queue's current `downloadId` is not always the one recorded on the useful grab event: retries can create a fresh SAB job while retaining the same release title. Search history by both identifiers. Prefer exact `sourceTitle` equality once the queue title is known, and select only the mapping fields you need:

```sh
arr sonarr raw GET '/history?page=1&pageSize=1000&sortKey=date&sortDirection=descending&includeSeries=true&includeEpisode=true' \
  | jq '.records[] | select((.downloadId//"")=="<queue-download-id>" or (.sourceTitle//"")=="<exact-release-title>") | {date,eventType,sourceTitle,seriesId,episodeId,downloadId}'
```

Do not dump the full history `data` object into logs or chat: its `downloadUrl` may embed a Prowlarr API key. A grab-history `tvdbId`/`seriesId` plus `episodeId`, followed by `/episode/<id>` showing `monitored=true` and `hasFile=false`, is strong evidence for the intended mapping even when the queue itself has null `series` and `episode` fields.

A folder named for S11E14 containing a full-length `s17e04.avi` plus an NFO explicitly describing S11E14 is a filename mismatch, not permission to import S17E04.

## Safe import pattern

The import helper maps from the selected video's filename. `--season` does not override an explicit wrong `SxxEyy` token. If the dry run maps to the wrong episode:

1. Rename or stage only the verified main video with the intended Sonarr-compatible `SxxEyy` token. Preserve unrelated files and never select a `sample`.
2. Dry-run with a restrictive `--match`:

```sh
arr sonarr import '<completed-folder>' \
  --series <series-id> --match '<renamed-main-file>' \
  --season <season> --map se --mode move --dry-run
```

3. Review the exact mapping, then rerun without `--dry-run`.
4. Wait for the returned Manual Import command ID and verify it completed.
5. Verify all three: target episode `hasFile=true`, recent `downloadFolderImported` history names the target episode, and the imported path exists under `/data/media`.

## Clearing the stale queue row

After a successful manual import, run `arr sonarr stuck --fix` again. Linked rows may now be classified as `satisfied` and can be cleared with `--yes`.

An originally unlinked/title-mismatch row may remain import-blocked even though its file was successfully imported. In that case, only after the target episode and imported file are verified, remove the stale row without blocklisting the good release:

```sh
arr sonarr queue-rm '<distinct release words>'       # dry-run
arr sonarr queue-rm '<distinct release words>' --yes # removeFromClient=true, blocklist=false
```

Do not blocklist content merely because Sonarr could not parse/link it. Blocklist only genuinely dead, junk, or wrong releases. Finish by rerunning `arr sonarr stuck`.