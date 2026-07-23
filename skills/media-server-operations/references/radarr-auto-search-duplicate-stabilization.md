# Radarr Auto-Search Duplicate Stabilization

Use this when adding a movie triggers an undesirable release (especially `BR-DISK` / `COMPLETE BLURAY`) and you manually choose a better encoded release.

## Why one deletion may not be enough

`arr radarr add` starts a background movie search. While that search is still completing, Radarr can enqueue multiple mirrors or additional accepted candidates several seconds apart. Deleting the first bad queue row does not prove the search is settled. A replacement added directly to SAB can therefore coexist with one or more late-arriving disc images or duplicate encodes.

## Safe sequence

1. Add by explicit TMDb ID when title/year lookup is ambiguous. Note that TMDb's display year may differ from the year used in release names.
2. Wait briefly and inspect the raw Radarr queue, not only compact `arr radarr queue` output:
   ```sh
   arr radarr raw GET '/queue?includeUnknownMovieItems=true&includeMovie=true&pageSize=1000' \
     | jq '.records[] | select(.movie.id==<MOVIE_ID>) | {id,title,status,downloadId,sizeleft}'
   ```
3. For each unwanted disc image or duplicate, delete the exact queue row with client removal and blocklisting:
   ```sh
   arr radarr raw DELETE '/queue/<QUEUE_ID>?removeFromClient=true&blocklist=true'
   ```
4. Select one normal encode/WEB-DL/remux. If Radarr's release push is unreliable, add the chosen release URL directly to SAB with category `radarr`; Radarr should attach it by category/history:
   ```sh
   arr sab add '<downloadUrl>' --cat radarr --name '<exact release title>'
   ```
5. Re-check both Radarr and SAB after several short intervals. Remove every late-arriving unwanted candidate by its exact Radarr queue ID. Do not use broad client deletion that could remove the chosen replacement.
6. Only after the queue has stabilized to one intended release, optionally promote that exact release:
   ```sh
   arr sab prio '<distinct exact-release substring>' --top
   ```
7. Verify again after promotion; promotion does not prevent a still-running Radarr search from enqueueing another candidate.

## Completion criteria

Before reporting success, confirm:

- Radarr has exactly one queue row for the movie.
- SAB has exactly the intended release and no late-arriving mirrors/disc images.
- The intended row reports a healthy tracked state (`ok`, downloading/queued).
- If title years differ, explain succinctly: catalog year versus release-name year.

Do not claim the movie is available until Radarr imports it. Report `queued` or `downloading` accurately.