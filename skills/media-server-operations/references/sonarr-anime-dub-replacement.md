# Sonarr Anime Dub Replacement Pattern

Session pattern: user wanted an English dub of an anime already present in Sonarr/Jellyfin, and was willing to replace existing subbed/Japanese files episode-by-episode.

## Key lessons

- Sonarr cannot reliably request "dub" as a user-facing semantic. Treat dub selection as release-title targeting: search for explicit `English Dub`, `Dual Audio`, `Funimation Dub`, `Animax Dub`, etc.
- For anime, release numbering may use **absolute episode numbers** even when Sonarr stores seasons. Use `arr sonarr-anime import ... --map abs` after a dry run; verify the mapping converts absolute numbers to the intended Sonarr seasons/episodes.
- Dub coverage may be partial. Do not assume one release covers the whole show. Search multiple title/dub variants and explain gaps.
  - Example: an Animax dub release covered mostly absolute episodes 104–202, mapping into Sonarr S3/S4 only, with missing absolute episodes noted by the release.
  - Example: a Funimation dub release covered absolute episodes 1–78, mapping to S1E1–S1E51 and S2E1–S2E27; later S2 episodes remained old files.
- Sonarr language labels may remain misleading (`Japanese` or `Unknown`) when files lack embedded audio language tags or the import parser infers language from prior metadata. Verify via release title and, when useful, `ffprobe` audio streams; report that Sonarr/Jellyfin may not show the dub language cleanly.
- If Sonarr marks the completed download as `completed/importBlocked` with "Series title mismatch", manual import is appropriate only after inspecting the files and dry-run mapping.
- Use `--mode move` for replacement imports from completed downloads. Sonarr will keep one file per episode and replace the existing managed episode file for mapped episodes.
- After manual import, remove the stale queue row with `DELETE /queue/<id>?removeFromClient=true&blocklist=false` only after imported files are moved and any leftover source folder contains no needed media. Then trigger Jellyfin refresh.

## Command pattern

Search for dub releases:

```sh
arr prowlarr search '<title> English Dub' --limit 50
arr prowlarr search '<title> Funimation Dub' --limit 50
arr prowlarr search '<alternate/original title> English Dub' --limit 50
```

Grab a single well-labeled release, preferring usenet if available:

```sh
arr prowlarr grab '<release query>' --match '<distinct dub words>' --indexer 'AnimeTosho' --cat sonarr-anime --dry-run
arr prowlarr grab '<release query>' --match '<distinct dub words>' --indexer 'AnimeTosho' --cat sonarr-anime
arr sab prio '<distinct release words>' --top
```

When completed/import-blocked, inspect raw queue and files:

```sh
arr sonarr-anime raw GET '/queue?includeUnknownSeriesItems=true&includeSeries=true&includeEpisode=true&pageSize=1000' \
  | jq '.records[] | select(.title|test("<release words>";"i")) | {id,title,status,trackedDownloadState,statusMessages,outputPath,downloadId}'
find '<outputPath>' -maxdepth 1 -type f -printf '%f\n' | sort
```

Dry-run absolute-number import, then run it:

```sh
arr sonarr-anime import '<outputPath>' \
  --series '<Sonarr title>' \
  --match '<distinct release filename prefix>' \
  --map abs \
  --mode move \
  --dry-run

arr sonarr-anime import '<outputPath>' \
  --series '<Sonarr title>' \
  --match '<distinct release filename prefix>' \
  --map abs \
  --mode move
```

Verify command, counts, and queue cleanup:

```sh
arr sonarr-anime raw GET '/command/<id>' | jq '{id,name,status,message}'
arr sonarr-anime seasons '<Sonarr title>'
arr sonarr-anime files '<Sonarr title>' --full
arr sonarr-anime raw DELETE '/queue/<queueId>?removeFromClient=true&blocklist=false'
curl -fsS -X POST "http://localhost:8096/Library/Refresh?api_key=$JELLYFIN_API_KEY" >/dev/null && echo refreshed
```

## Reporting to users

Be explicit about replacement scope:

- Which seasons/episodes were replaced.
- Which episodes remain old/subbed/Japanese because no dub files were found.
- Whether the dub release is Funimation, Animax, dual-audio, etc.
- Whether Sonarr/Jellyfin language metadata may look wrong despite the release being a dub.
