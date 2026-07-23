# Ambiguous Sonarr title correction: same-title / accent variants

Use this when a requested show title maps to the wrong series in Sonarr, especially short titles with many remakes or accent variants.

## Pattern

A bare title search/status can silently point at the wrong existing Sonarr object. Example: `Gloria` matched an existing Korean `Gloria (2010)` entry with 0 files, while the intended request was Netflix Portugal `Glória (2021)` (TVDB 388219). Prowlarr releases for the intended show were listed as unaccented `Gloria.S01...`, so searching only the accented title missed useful releases.

## Safer workflow

1. Inspect the existing Sonarr match with year/network/original language, not just title:
   - `arr sonarr status '<title>'`
   - `arr sonarr get '<title>'`
2. If the title is ambiguous, run a Sonarr lookup and compare candidates:
   - `arr sonarr raw GET '/series/lookup?term=<title>' | jq '.[] | {title,year,tvdbId,tmdbId,imdbId,network,overview:(.overview[0:140])}'`
3. Search Prowlarr using both accented and unaccented variants plus year/season:
   - `arr prowlarr search 'Gloria 2021' --limit 10`
   - `arr prowlarr search 'Glória 2021' --limit 10`
   - `arr prowlarr search 'Gloria S01 2021' --limit 10`
4. Add the intended Sonarr object by explicit TVDB/lookup result rather than relying on fuzzy title matching. Set root/profile/season monitoring and use `addOptions.searchForMissingEpisodes=true`.
5. Leave the wrong existing entry alone unless the user asked for cleanup or you have verified it is unwanted. Report it as a separate stale/wrong entry that can be removed.
6. Verify queue mapping through raw queue fields (`series.title`, season/episode, `trackedDownloadStatus`, `trackedDownloadState`) so a complete-season release is attached to every intended episode.

## Why this matters

For same-title remakes and accent variants, Sonarr's local title match, TheTVDB lookup title, and indexer release title can all differ. The durable technique is to anchor on external IDs/year/network, then use broad release-search variants for availability.