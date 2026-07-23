# Radarr direct-to-SAB grab: download URL and verification gates

Use this when Radarr's profile chooses an undesirable BR-DISK/complete Blu-ray or rejects a clearly suitable encoded release.

## Why this matters

Radarr release objects commonly expose both:

- `guid`: an indexer details-page identifier/URL
- `downloadUrl`: a Prowlarr-proxied URL that returns the actual NZB payload

`arr sab add` can accept a details-page URL without immediately proving it fetched an NZB. The job may then fail asynchronously with `URL Fetching failed; Maximum retries`. Therefore, an “added” response is not a success gate.

## Safe sequence

```sh
movie_id=<RADARR_MOVIE_ID>
release_title='<EXACT RELEASE TITLE>'

# Search/refresh release cache and select one exact object.
release=$(arr radarr raw GET "/release?movieId=$movie_id" \
  | jq --arg title "$release_title" '[.[] | select(.title==$title)][0]')

# Inspect identity without printing credentials embedded in downloadUrl.
printf '%s' "$release" | jq '{title,size,quality,indexerId,rejections}'

# Send the actual NZB URL, not .guid.
download_url=$(printf '%s' "$release" | jq -r '.downloadUrl')
arr sab add "$download_url" --cat radarr --name "$release_title"
```

Do not print or paste `downloadUrl` into chat/log summaries; it may contain a Prowlarr API key.

## Verification gates

1. **SAB fetch succeeded:** after a short delay, `arr sab queue '<distinct title>'` shows the intended release in `Checking`, `Downloading`, or another healthy non-failed state. Check SAB history if it disappears.
2. **Radarr attached it:** query Radarr `/queue` and verify the intended movie/release has a download ID, `trackedDownloadStatus=ok`, and a healthy tracked state.
3. **Search race settled:** poll again after the original add-triggered movie search completes. Remove/blocklist late-arriving `BR-DISK`, `COMPLETE BLURAY`, or duplicate mirror rows; automatic searches can enqueue these after an earlier bad row was already removed.
4. **Import completed:** Radarr reports `hasFile=true` and import history exists.
5. **Actually available:** Jellyfin sees the movie after refresh. Until then, report “queued/downloading/importing,” not “handled” or “available.”

## Queue cleanup

Match the exact Radarr queue row and its download ID before deletion:

```sh
arr radarr raw DELETE "/queue/<QUEUE_ID>?removeFromClient=true&blocklist=true"
```

Use `blocklist=true` for a specifically unwanted complete-disc candidate so another search does not choose it again. Use `blocklist=false` only for an accidental duplicate of an otherwise acceptable release.

## Failure signatures

- SAB history: `URL Fetching failed; Maximum retries` → likely passed `guid`/details URL instead of `downloadUrl`; select the live release object again and submit `.downloadUrl`.
- Radarr queue warning: download failed and “wasn't grabbed by Radarr” → direct job failed before Radarr could handle it; inspect SAB history, then re-submit correctly.
- One BR-DISK removed but another appears → add-triggered search/mirror race; wait for search completion and stabilize the queue before reporting.
