# Seerr unfulfilled audit: easy fixes and Radarr manual-import pitfalls

Use this note when continuing from a Seerr requested-but-not-available audit and applying obvious fixes.

## Pattern: after fixing arr state, force Seerr to resync

Seerr can continue reporting requests as `partial`/`processing` after Sonarr/Radarr are fixed. After verifying imports in arr, trigger Seerr jobs and re-query requests before reporting final counts:

```sh
for job in radarr-scan sonarr-scan availability-sync jellyfin-recently-added-scan; do
  curl -fsS -X POST -H "X-Api-Key: $SEERR_API_KEY" \
    "http://localhost:5055/api/v1/settings/jobs/$job/run" | jq '{id,name,running}'
done

# Wait until availability-sync is false, then refetch request list.
curl -fsS -H "X-Api-Key: $SEERR_API_KEY" \
  'http://localhost:5055/api/v1/request?take=200&skip=0' > /tmp/seerr_after_jobs.json
```

## Safe easy-fix candidates

Good candidates to act on without further user prompting:

- A monitored season has only a few missing episodes, and `arr sonarr releases '<title>' --episode <id>` shows clean accepted releases. Grab them, wait for SAB, verify `seasons` and history.
- A completed Sonarr queue item is clearly the exact intended episode but import-blocked by filename/scene numbering. Use explicit manual import payload or the `arr sonarr import` helper after a dry run.
- A Radarr movie has clean accepted releases and is missing; run a normal `arr radarr grab '<title>'`, wait for download/import, then verify `arr radarr status`.

Avoid auto-fixing when:

- Anime/TVDB ordering differs from scene/absolute order; build an explicit title/episode map first.
- Same-title movie/show releases are ambiguous by year/remake, or release titles point to a different title.
- Indexer results are only CAM/low-quality unless that is acceptable for the request.

## Radarr manual-import gotchas

Radarr can show a completed download as `completed/importBlocked` with:

> Found matching movie via grab history, but release was matched to movie by ID. Manual Import required.

Workflow:

1. Inspect raw queue and output path:
   ```sh
   arr radarr raw GET '/queue?includeUnknownMovieItems=true&includeMovie=true&pageSize=1000' \
     | jq '.records[] | select(.title|test("<release words>";"i")) | {id,title,movieId:.movie.id,outputPath,statusMessages}'
   ```
2. Make sure the Radarr movie folder exists and is readable by Radarr. If Hermes creates it, set group-readable/traversable permissions:
   ```sh
   mkdir -p '/data/media/movies/<Movie> (<Year>)'
   chmod 775 '/data/media/movies/<Movie> (<Year>)'
   ```
   Without the target directory, Radarr manualimport may HTTP 500 with `DirectoryNotFoundException` for the movie path.
3. Query manual import without `movieId` if querying with `movieId` returns empty unexpectedly:
   ```sh
   arr radarr raw GET "/manualimport?folder=$ENCODED_FOLDER&filterExistingFiles=false"
   ```
4. If posting `ManualImport` reports success but `hasFile` remains false and the source file is still in completed downloads, do not trust the success message alone. Verify:
   ```sh
   arr radarr raw GET '/movie/<id>' | jq '{title,hasFile,movieFileId,sizeOnDisk,path}'
   stat '<source-file>' || true
   ```
5. If the import command falsely succeeded but did not move the file, move the verified movie file into the Radarr movie folder with a sane name, chmod it readable, then refresh the movie:
   ```sh
   mv '<completed-download>.mkv' '/data/media/movies/<Movie> (<Year>)/<Movie> (<Year>) <quality> <group>.mkv'
   chmod 664 '/data/media/movies/<Movie> (<Year>)/'*.mkv
   curl -fsS -X POST 'http://localhost:7878/api/v3/command' \
     -H "X-Api-Key: $RADARR_API_KEY" -H 'Content-Type: application/json' \
     --data '{"name":"RefreshMovie","movieIds":[<id>]}'
   ```
6. Only remove the stale queue row after `hasFile=true` / `ON DISK` is verified:
   ```sh
   curl -fsS -X DELETE \
     "http://localhost:7878/api/v3/queue/<queueId>?removeFromClient=true&blocklist=false" \
     -H "X-Api-Key: $RADARR_API_KEY"
   ```

## Verification

For every claimed fix, verify at least:

- Sonarr/Radarr status shows full season or movie `ON DISK`.
- History shows import, or file exists under the library path and arr DB has `hasFile=true`.
- Jellyfin refresh was triggered.
- Seerr scan/availability jobs were run and the final Seerr count was re-read.
