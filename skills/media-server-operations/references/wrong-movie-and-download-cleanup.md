# Wrong movie indexed + undeletable completed-download leftovers

## Situation pattern

A user reports that Jellyfin has the wrong movie for a requested title, or asks to delete a show that is no longer in Sonarr but has leftover files in completed downloads.

## Useful checks

### Wrong movie / intended movie absent

- Search Radarr for the intended title.
- Search the movie library by partial terms, not just the exact intended title. A wrong item may be a different movie sharing one keyword.
- Example pattern: intended title `The Emperor's New Groove` was absent from Radarr and `/data/media/movies/*Emperor*`, but `/data/media/movies/Groove (2000)/...` existed and was the unrelated movie `Groove (2000)`.

### Add intended movie explicitly

Use Radarr lookup to pin the intended TMDB ID, then POST to `/movie` with normal root/profile and `searchForMovie=true`.

```sh
lookup=$(arr radarr raw GET '/movie/lookup?term=The%20Emperor%27s%20New%20Groove' \
  | jq 'map(select(.tmdbId==11688))[0]')
printf '%s' "$lookup" \
  | jq '. + {qualityProfileId:1, rootFolderPath:"/data/media/movies", monitored:true, minimumAvailability:"released", addOptions:{searchForMovie:true}}' \
  > /tmp/movie-add.json
arr radarr raw POST '/movie' "$(cat /tmp/movie-add.json)"
```

Then delete the wrong folder only after confirming it is truly wrong, and verify Radarr queue/history for the intended title.

### Completed-download leftovers not controlled by arr

If a show is not in Sonarr and has zero queue entries, stale completed folders may remain under `/data/downloads/usenet/complete`. Check size/count:

```sh
find /data/downloads/usenet/complete -mindepth 1 -maxdepth 1 -iname '*johnny*test*' | wc -l
find /data/downloads/usenet/complete -mindepth 1 -maxdepth 1 -iname '*johnny*test*' -print0 | xargs -0 du -sch | tail -1
```

If `rm -rf` fails with permission denied, inspect ownership. Old SABnzbd outputs can be owned by a dynamic UID/GID shown as a different system account. Hermes may be able to rename top-level folders but not delete file contents. If SAB history does not include those jobs anymore, report that arr cleanup is complete but host-level privileged cleanup is needed for the leftover bytes.
