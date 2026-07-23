# Letterboxd + Jellyfin recommendation/request workflow

Use this when asked to recommend movies for one trusted user based on another user's Letterboxd taste and the target user's Jellyfin watch history, then request missing titles.

## Data sources

- Letterboxd plugin API/cache is the preferred source when the user says a profile has been imported. Verify cache counts instead of scraping only the public profile.
  - API: `GET /Letterboxd/Users`, `GET /Letterboxd/Search?query=<title>`
  - Cache: `/data/.nixflix/jellyfin/plugins/Jellyfin.Plugin.Letterboxd/letterboxd-cache.json`
  - Cache keys are camelCase (`displayName`, `entries`, `tmdbMovieId`); plugin API responses are PascalCase (`DisplayName`, `Entries`, `TmdbMovieId`).
- Jellyfin watch history/preferences for the target user:
  - Find the user ID via `/Users?api_key=$JELLYFIN_API_KEY`.
  - Fetch played movies with `Users/<id>/Items?Recursive=true&IncludeItemTypes=Movie&Filters=IsPlayed&Fields=Genres,ProductionYear,CommunityRating,Overview,UserData,ProviderIds`.
- Use TMDb IDs as the join key between Letterboxd, Radarr, and Jellyfin whenever available.

## Recommendation pattern

1. Confirm the Letterboxd profile is imported and get its entry count. If the plugin has full imported data, do not rely on the public Letterboxd profile page alone; that only exposes favorites/recent activity.
2. Summarize the target user's actual Jellyfin watched movies by genre/era/tone before ranking candidates.
3. Candidate score should combine:
   - explicit favorites from the source profile,
   - high ratings / liked entries from imported Letterboxd data,
   - fit to the target user's watched history,
   - whether the title is already in the Jellyfin/Radarr library and not already watched by the target user.
4. Present a short ranked list with plain-English rationale; distinguish “already in library” from “good but needs request.”

## Requesting the recommended movies

When the user asks to request the list:

1. Check Radarr first for each title; some exact title searches may fail when Radarr has a longer canonical title, so also query `/movie` and match by TMDb ID or known canonical title.
2. For missing movies, use Radarr lookup by `title year`, then `POST /movie` with:
   - `qualityProfileId`: current preferred movie profile (often `UHD Bluray + WEB` here),
   - `rootFolderPath`: `/data/media/movies`,
   - `monitored: true`,
   - `minimumAvailability: released`,
   - `addOptions.searchForMovie: true`.
3. After adding, verify with `arr radarr status <title>` and inspect `/queue?includeMovie=true&pageSize=1000` for active downloads/import blocks.
4. Trigger a Jellyfin library refresh after additions/imports.
5. Report two states separately:
   - “ON DISK / watchable now” from Radarr status,
   - “still downloading/upgrading” from Radarr queue.
   It is common for Radarr to show a file on disk while a better release is still downloading.

## Pitfalls

- Do not claim only Nick's Letterboxd favorites were available if the plugin cache has the full import; check the plugin/cache first.
- `arr radarr status` can miss some movies by abbreviated title even when they exist under a longer canonical title. Use TMDb IDs or raw `/movie` matching for exact verification.
- Radarr may immediately show `ON DISK` after adding because an import completed quickly or a previous file existed; still check the queue because upgrades can continue in parallel.
