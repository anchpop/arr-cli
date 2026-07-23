# Sonarr Anime Numbering / Absolute-Number Repair

Use this when an anime series is incomplete or import-blocked because release filenames use absolute/session numbering while Sonarr's `episodeNumber` order differs from the release order. Cowboy Bebop is a concrete example: Sonarr's season order can put `Session #1: Asteroid Blues` at `S1E13` while release files are named `E01 Asteroid Blues`; importing by `SxxExx` or by naive absolute fallback can silently attach files to the wrong metadata slot.

## Symptoms

- Queue rows are `completed/importBlocked` with messages like “matched to series by ID; automatic import is not possible.”
- Raw queue `episode` fields point at the wrong title for a release filename, or the filename title disagrees with Sonarr's episode title.
- `arr sonarr-anime status '<show>'` reports missing episodes even though matching-looking files exist in completed downloads.
- Previously imported files have titles/filenames that do not match the Sonarr episode they are attached to.

## Safe Workflow

1. Export Sonarr episode metadata and episode files:
   ```sh
   arr sonarr-anime raw GET '/episode?seriesId=<id>' > /tmp/<show>-episodes.json
   arr sonarr-anime raw GET '/episodefile?seriesId=<id>' > /tmp/<show>-files.json
   jq -r --slurpfile files /tmp/<show>-files.json \
     '.[] | select(.seasonNumber==1) | . as $e |
      (if .hasFile then ($files[0][] | select(.id==$e.episodeFileId) | .relativePath) else "" end) as $rel |
      [$e.episodeNumber,$e.absoluteEpisodeNumber,$e.title,($e.hasFile|tostring),$rel] | @tsv' \
      /tmp/<show>-episodes.json | sort -n -k1
   ```

2. Inspect completed download candidates with raw manual import data rather than trusting queue summaries:
   ```sh
   arr sonarr-anime raw GET '/manualimport?folder=%2Fdata%2Fdownloads%2Fusenet%2Fcomplete%2Fsonarr-anime&filterExistingFiles=false' \
     | jq -r '.[] | select(.path|test("<ShowRegex>";"i")) |
              [.path, (.quality.quality.name // "NOQUAL"), ([.rejections[]?.reason]|join(" | "))] | @tsv'
   ```

3. Build an explicit mapping from file number/title to Sonarr episode **ID**. Prefer title agreement over numeric agreement when numbering schemes disagree. For absolute-numbered releases, map `E##` to `absoluteEpisodeNumber`, then verify the target Sonarr title matches the filename title.

4. If Sonarr refuses a correct file because the filename uses the release's numbering but Sonarr expects a different `episodeNumber`, create a temporary copy with a Sonarr-compatible filename and import with `--map se`. Example pattern:
   ```sh
   repair=/data/downloads/usenet/complete/sonarr-anime/hermes-<show>-repair
   mkdir -p "$repair"
   cp '<real file for absolute/session 09>' "$repair/Show.S01E05.Real.Title.1080p.mkv"  # if Sonarr episodeNumber 5 == absolute 9
   chmod 775 "$repair"
   chmod 664 "$repair"/*.mkv
   arr sonarr-anime import "$repair" --series <id> --match 'S01E05.Real.Title' --season 1 --map se --mode copy --dry-run
   arr sonarr-anime import "$repair" --series <id> --match 'S01E05.Real.Title' --season 1 --map se --mode copy
   ```

5. For a missing episode where indexers have a usable release but Sonarr rejects it because another wrong queue row “already meets cutoff,” grab the specific NZB/torrent via Prowlarr/SAB with a restrictive exact release title, wait for completion, then import through a renamed temporary copy into Sonarr's expected episode slot.

6. Re-run the mapping table and ensure every Sonarr episode has the expected title/file alignment. Do not stop at the headline `100%`; verify filename/title alignment for any slots you touched.

7. Clean up temporary repair directories and stale completed/importBlocked queue records only after import/history verification:
   ```sh
   arr sonarr-anime raw GET '/queue?includeUnknownSeriesItems=true&includeSeries=true&includeEpisode=true&pageSize=1000'
   curl -fsS -X DELETE "http://localhost:8990/api/v3/queue/<queueId>?removeFromClient=true&blocklist=false" \
     -H "X-Api-Key: $SONARR_ANIME_API_KEY"
   ```
   Use `blocklist=false` when the item was already successfully imported and you are only clearing stale queue rows.

8. Trigger Jellyfin refresh when done:
   ```sh
   curl -fsS -X POST 'http://localhost:8096/Library/Refresh' -H "X-Emby-Token: $JELLYFIN_API_KEY"
   ```

## Pitfalls

- **Do not trust `arr import --map abs` blindly.** It can import the correct absolute file into the correct absolute episode, but if the file had already been attached to the wrong Sonarr slot, later copy imports may replace or delete the wrong file. Always review the full episode/file map after each batch.
- **Sonarr's `downloadFolderImported` can say success even while the series count is unchanged.** This often means it replaced an already-present episode file rather than filling a missing slot.
- **Temporary repair dirs must be readable by Sonarr.** Files created by Hermes under completed-download paths may have directory mode `770` and group `hermes`, causing Sonarr to report `File doesn't exist`. Set the repair directory to at least `775` and files to `664` before importing.
- **A filename's `S01E12` may mean release/absolute episode 12, not Sonarr's episodeNumber 12.** If Sonarr episodeNumber 12 represents a different absolute/session number, rename the temp copy to Sonarr's expected slot and preserve the real title in the filename for human review.
- **Do not delete original library files until you have a fallback.** When replacing a mis-imported set, create temporary copies/backups first; remove them only after final verification.
