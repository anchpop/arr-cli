# Sonarr completed/importBlocked manual import example

## Scenario

A user requested a show, and Sonarr showed the series as monitored but with `0/N` episodes on disk. The history contained `grabbed` events for each episode, and the queue showed every episode as `completed/importBlocked`.

Raw queue diagnostics for each release contained the key message:

> Found matching series via grab history, but release was matched to series by ID. Automatic import is not possible. See the FAQ for details.

This means the download completed and Sonarr knows which series it belongs to, but Sonarr refuses to automatically import because the release was matched through grab history/ID rather than normal title parsing.

## Diagnostic command pattern

```sh
arr sonarr status '<series>'
arr sonarr get '<series>'
arr sonarr seasons '<series>'
arr sonarr queue | grep -i '<distinct words>' || true
arr sonarr history '<series>' | head -80

arr sonarr raw GET '/queue?includeUnknownSeriesItems=true&includeSeries=true&includeEpisode=true&pageSize=1000' \
  | jq '.records[] | select(.title|test("<TitleRegex>";"i")) | {id,title,status,trackedDownloadStatus,trackedDownloadState,statusMessages,errorMessage,downloadId,outputPath,episode:.episode|{seasonNumber,episodeNumber,title},series:.series.title}'
```

Also verify the expected `.mkv` files exist in the completed download folder before importing.

## Fix command pattern

Run a dry-run manual import with explicit series, season, SxxExx mapping, and a restrictive match substring:

```sh
arr sonarr import /data/downloads/usenet/complete/sonarr \
  --series '<series>' \
  --match '<distinct.release.prefix>' \
  --season 1 \
  --map se \
  --mode move \
  --dry-run
```

If every file maps correctly, rerun without `--dry-run`:

```sh
arr sonarr import /data/downloads/usenet/complete/sonarr \
  --series '<series>' \
  --match '<distinct.release.prefix>' \
  --season 1 \
  --map se \
  --mode move
```

## Verification pattern

```sh
arr sonarr status '<series>'
arr sonarr history '<series>' | head -40
```

Expected outcome: Sonarr reports the season's episodes on disk, history has `downloadFolderImported` events, and files appear in `/data/media/tv/<Series>/Season <N>/`.

If the status fraction still looks odd, check whether Sonarr includes unmonitored specials in the total; e.g. `16/19 eps on disk` can still mean season 1 is complete when season 0 specials account for the extra 3.
