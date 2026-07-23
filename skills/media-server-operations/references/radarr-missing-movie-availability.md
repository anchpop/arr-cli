# Radarr Missing Movie / Availability Investigation

Use this pattern when a user asks why requested movies are not available in Jellyfin.

## Diagnostic sequence

1. Check Radarr object state for each requested title:
   ```sh
   arr radarr status '<movie title>'
   arr radarr get '<movie title>' | jq '{id,title,year,tmdbId,imdbId,originalTitle,alternateTitles,path,monitored,hasFile,minimumAvailability,qualityProfileId,statistics}'
   arr radarr history '<movie title>' | tail -60
   arr radarr queue | grep -i '<distinct title words>' || true
   ```
2. Check Radarr release decisions:
   ```sh
   arr radarr releases '<movie title>'
   ```
   `found 0 release(s)` means Radarr's configured indexers did not find an acceptable release for that exact movie object.
3. If releases are absent, search Prowlarr manually with alternate names, original title, romanization, local-language title, and year:
   ```sh
   arr prowlarr search '<English title> <year>' --limit 10
   arr prowlarr search '<original title> <year>' --limit 10
   arr prowlarr search '<romanized title> <year>' --limit 10
   ```
4. If history has `downloadFailed`, inspect raw history for the actual failure reason:
   ```sh
   arr radarr raw GET '/history/movie?movieId=<id>&pageSize=50&sortKey=date&sortDirection=descending' \
     | jq '.[] | {date,eventType,sourceTitle,quality:.quality.quality.name,downloadId,data}'
   ```

## Common outcomes

- **No releases in Radarr and Prowlarr**: answer that the movie is monitored but no configured indexer currently has a matching release. Include the alternate titles searched if helpful.
- **Download failed with SAB repair message**: e.g. `Repair failed, not enough repair blocks`. The NZB was incomplete on the provider. Search for another release and re-grab if Radarr offers one.
- **Release blocklisted**: the failed release may appear with `Release is blocklisted`; choose a different release rather than re-grabbing the same broken NZB.
- **Wrong year / same English title**: Radarr may have the requested title but a different movie object than the user intended. Use `arr radarr raw GET '/movie/lookup?term=...'` and compare `year`, `tmdbId`, `imdbId`, `originalTitle`, and overview before grabbing. Tell the user clearly if the available releases are for a different film.

## Example lessons from prior session

- `The Little Thief (1988)` had a failed NZB: `Repair failed, not enough repair blocks`; another `La petite voleuse (1988).1080p.bluray` release was available and could be queued.
- `She's on Duty (2005)` had no releases under English, romanized, or Korean title searches.
- `Reversal of Fortune` was ambiguous: Radarr had the Korean 2003 movie (`역전에 산다`) with no releases, while the 1990 Jeremy Irons film had many releases. Do not grab the 1990 movie unless the user confirms that is what they meant or the requested Radarr object is corrected.
