# Seerr Unfulfilled Request Audit Pattern

Use this when asked to review “everything requested but not fulfilled” and apply easy media-server fixes.

## Scope

Audit Seerr requests where either the request status is not completed or the media status is not available. Then classify into safe/easy fixes versus items that need manual judgment.

## Data collection

1. Pull Seerr requests and current arr libraries:
   ```sh
   curl -fsS -H "X-Api-Key: $SEERR_API_KEY" \
     'http://localhost:5055/api/v1/request?take=300&skip=0' > /tmp/requests.json
   curl -fsS -H "X-Api-Key: $SONARR_API_KEY" 'http://localhost:8989/api/v3/series' > /tmp/sonarr.json
   curl -fsS -H "X-Api-Key: $SONARR_ANIME_API_KEY" 'http://localhost:8990/api/v3/series' > /tmp/anime.json
   curl -fsS -H "X-Api-Key: $RADARR_API_KEY" 'http://localhost:7878/api/v3/movie' > /tmp/radarr.json
   ```
2. Join Seerr media IDs to Sonarr/Radarr by `tvdbId` / `tmdbId` and list status, on-disk counts, monitored state, and queue state.
3. Inspect stuck queues:
   ```sh
   arr sonarr stuck
   arr sonarr-anime stuck
   arr radarr stuck
   arr sab status
   ```

## Easy fixes to try

- **Completed/importPending or importBlocked with exact SxxEyy mapping:** run `arr ... import ... --dry-run` with a restrictive `--match`, verify every mapping, then import.
- **Seerr failed because item was never added to arr:** add the item directly to Sonarr/Radarr from lookup metadata and enable an immediate search. For Sonarr Anime, use the local anime instance/root folder when the title is anime:
  ```sh
  lookup=$(arr sonarr-anime raw GET '/series/lookup?term=tvdb:<tvdbId>' | jq '.[0]')
  payload=$(echo "$lookup" | jq '. + {
    qualityProfileId: 1,
    languageProfileId: 1,
    rootFolderPath: "/data/media/anime",
    monitored: true,
    seasonFolder: true,
    addOptions: {monitor:"all", searchForMissingEpisodes:true}
  } | .seasons |= map(.monitored = (if .seasonNumber == 0 then false else true end))')
  curl -fsS -X POST 'http://localhost:8990/api/v3/series' \
    -H "X-Api-Key: $SONARR_ANIME_API_KEY" \
    -H 'Content-Type: application/json' \
    --data-binary "$payload"
  ```
  Verify with `arr sonarr-anime status '<title>'` and `arr sonarr-anime queue '<title words>'`.
- **Single missing episode with an accepted release:** `arr sonarr grab '<series>' --episode <episodeId>`, then watch SAB and history. If SAB fails, do not keep forcing blocklisted or wrong-language alternatives without user approval.
- **Radarr failed repair:** search Radarr releases and Prowlarr by English title, original title, year, IMDb/TMDb IDs, and romanizations. If no alternate releases exist, report that clearly.

## Pitfalls

- Seerr may still show a request as `failed`/`unknown` immediately after adding the title directly to Sonarr; report the operational state in Sonarr/Radarr rather than treating Seerr as the only source of truth.
- `arr raw` expects inline JSON and does not accept an `@file` body. For large POST bodies, use `curl --data-binary @file` directly against the arr API.
- Do not force-import anime releases when scene/absolute numbering disagrees with Sonarr episode metadata. Example: a Cowboy Bebop `E01 Asteroid Blues` release may map strangely if Sonarr has alternate ordering; inspect `/episode?seriesId=...` and manual import candidates before touching files.
- Some “partial” Seerr rows are metadata/status artifacts: the arr library may show all known episodes on disk (for example `50/50`) while Seerr still says partial. Separate those from genuinely missing content.

## Verification

After each fix:

```sh
arr <service> status '<title>'
arr <service> history '<title>' | head -20
arr <service> queue '<distinct title words>' || true
curl -fsS -X POST "http://localhost:8096/Library/Refresh?api_key=$JELLYFIN_API_KEY" >/dev/null
```

Report: fixed/imported, actively queued/downloading, failed with no safe alternate, or skipped due to risky mapping.