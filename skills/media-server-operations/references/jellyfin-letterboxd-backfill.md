# Jellyfin Letterboxd Backfill Notes

Use this when the Letterboxd plugin's RSS-backed cache has only ~50 entries per profile and the user wants older Letterboxd opinions available in Jellyfin/Hermes.

## Durable findings

- Letterboxd RSS is a recent-activity feed; it can expose only ~50 film activity entries per user. This is not enough for historical friend-opinion lookup.
- Public review pages can expose older review history under URLs like:
  - `https://letterboxd.com/<username>/films/reviews/page/<n>/`
- Plain `curl`/`requests` may receive Cloudflare 403 / `Just a moment...` on review pages even when RSS works. A real browser automation path (for example Selenium + Firefox/geckodriver) can load the same public pages normally.
- `/films/reviews/page/<n>/` backfills **reviewed entries only**, not every diary/logged/watched film. For all watched/logged films, prefer a user-provided Letterboxd export zip over scraping.
- Letterboxd exports are the best historical-backfill path when available: parse `watched.csv`, `diary.csv`, `ratings.csv`, and `reviews.csv` locally, then use RSS only for incremental updates.
- Review-page or export backfill should resolve TMDb IDs from linked film pages when possible so existing plugin endpoints can match Jellyfin/Radarr movies by `tmdbMovieId`. Export CSVs may use different short URLs for the same movie across files, so collapse export rows by title+year (or TMDb once resolved), not by Letterboxd URL alone.

## Plugin cache preservation pitfall

Before importing backfilled entries, make sure scheduled RSS sync merges RSS entries into the existing cache instead of replacing the cache wholesale. Otherwise the next scheduled sync will collapse the cache back to RSS-only history.

Suggested merge behavior:

1. Read existing `letterboxd-cache.json`.
2. For each configured user, fetch current RSS entries.
3. Merge existing/backfilled entries with RSS entries by stable key:
   - Prefer `letterboxdUsername + tmdbMovieId`.
   - Fall back to `letterboxdUsername + title + year`.
   - Avoid using Letterboxd URL as the primary key for exports: `watched.csv`, `diary.csv`, and `reviews.csv` can provide different short URLs for the same film.
4. When inserting existing cache entries into the merge dictionary, de-dupe them too; otherwise old URL-keyed duplicates can survive until the next manual cleanup.
5. Let RSS refresh current metadata, but preserve richer existing review text when RSS only has a synthetic/short summary.
6. Sort merged entries by watched/published date descending.

## Operational workflow

### Preferred: user-provided Letterboxd export

1. Ask the user to download their export from Letterboxd settings/data and upload the zip.
2. Extract/read the zip locally and inspect row counts/columns for `profile.csv`, `watched.csv`, `diary.csv`, `ratings.csv`, and `reviews.csv`.
3. Build one cache entry per film by merging rows across CSVs by `title + year` first; keep rating, watched date, rewatch flag, review text, likes from `likes/films.csv`, and a representative Letterboxd URL.
4. Resolve TMDb IDs from the film URLs and cache URL→TMDb lookups to avoid repeated network calls.
5. Add/update the user in Jellyfin's live Letterboxd plugin configuration through `/Plugins/{id}/Configuration` (POST the full config); do not hand-edit Jellyfin XML config files.
6. Back up `letterboxd-cache.json`, merge the generated entries into the live cache, then run `/Letterboxd/Sync` to prove RSS merging preserves the backfill instead of wiping it.
7. Verify with `/Letterboxd/Users` counts and `/Letterboxd/Film/<tmdbId>` for at least one known imported title.

#### Exact protocol when a trusted user uploads a Letterboxd export zip

Use this for Alex (`alexandriasage` / Jellyfin `alexandria` / Discord `162299305300852737`) and any other trusted user once their username/profile is known.

1. Locate the uploaded zip in Hermes' document cache, usually under `/var/lib/hermes/.hermes/cache/documents/doc_*_letterboxd-<username>-*.zip`. Use file/search tools; do not ask the user to paste zip contents.
2. Inspect the archive before importing:
   - `unzip -l <zip> | sed -n '1,80p'`
   - verify expected CSVs: `profile.csv`, `watched.csv`, `diary.csv`, `ratings.csv`, `reviews.csv`, optionally `likes/films.csv`.
3. Back up live cache before writing:
   - `cp /data/.nixflix/jellyfin/plugins/Jellyfin.Plugin.Letterboxd/letterboxd-cache.json /data/.nixflix/jellyfin/plugins/Jellyfin.Plugin.Letterboxd/letterboxd-cache.json.bak.$(date -u +%Y%m%dT%H%M%SZ)`
4. Run the importer from the plugin repo using the known identity mapping:
   - `cd /data/hermes/hermes-plugins/jellyfin-letterboxd`
   - `nix shell nixpkgs#python3 nixpkgs#python3Packages.requests nixpkgs#python3Packages.beautifulsoup4 --command python scripts/import_letterboxd_export.py <zip> --user 'Alex:alexandriasage:alexandria:162299305300852737' --merge-cache /data/.nixflix/jellyfin/plugins/Jellyfin.Plugin.Letterboxd/letterboxd-cache.json`
   - For other users, replace the `--user` tuple: `DisplayName:letterboxdUsername:jellyfinUserName:discordUserId`.
5. Trigger plugin sync and Jellyfin refresh, keeping API keys out of chat:
   - `curl -fsS -X POST -H "X-Emby-Token: $JELLYFIN_API_KEY" http://localhost:8096/Letterboxd/Sync`
   - `curl -fsS -X POST "http://localhost:8096/Library/Refresh?api_key=$JELLYFIN_API_KEY"`
6. Verify live state:
   - `/Letterboxd/Users` includes the user and their imported entry count is well above RSS-only when the export is full.
   - `/Letterboxd/Film/<tmdbId>` returns at least one known imported title from that user's export.
   - Re-run `/Letterboxd/Sync` or inspect its result to ensure RSS merge preserves the backfilled entries instead of collapsing to ~50.
7. Report clearly whether the live Jellyfin cache was updated and verified, plus parsed unique film count, TMDb-resolved count, reviews/likes count if printed, and backup path. If no zip is present yet, say import is blocked on upload and do not claim success.

### Fallback: public review-page scraping

1. Scrape public review pages politely with browser automation and delays. Expect Cloudflare challenges; retry with a fresh browser session rather than hammering the same one.
2. Save raw per-user review JSON under a scratch/backfill directory before touching live Jellyfin state.
3. Resolve TMDb IDs from film pages and save a reusable film→TMDb cache to avoid repeated lookups.
4. Back up the live plugin cache before editing.
5. Merge into the live plugin cache only if Hermes has write access. If write fails with `Permission denied`, report that the backfill data was produced but not installed; do not claim Jellyfin is updated.
6. After cache merge, verify via plugin API endpoints and counts per user. Then run a normal `/Letterboxd/Sync` to ensure the merge-preservation code does not wipe backfilled entries.

## Verification signals

- Raw backfill file counts by Letterboxd username.
- Count of rows with non-null `tmdbMovieId`.
- Live cache counts before/after merge.
- Plugin API query for a backfilled film returns the expected user review.
- A subsequent RSS sync preserves counts above 50 where applicable.

## Communication caveat

Say clearly whether the result is:

- Tooling built only.
- Backfill data generated but not installed.
- Live Jellyfin cache updated and verified.

Do not conflate these states.