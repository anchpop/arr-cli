# Radarr/Jellyfin missing movie and wrong-match cleanup

Use this as a concrete pattern when a user says a movie is not in Jellyfin, or that Jellyfin shows the wrong movie for a requested title.

## Wrong movie indexed under the expected request

Example: user expected **The Emperor's New Groove (2000)**, but Jellyfin had `/data/media/movies/Groove (2000)/Groove.2000...mkv` — a completely different movie.

Pattern:

1. Search Radarr first for the expected title. If Radarr has no object, use `/movie/lookup?term=<title>` to get the correct TMDB object.
2. Search `/data/media/movies` for partial title words. If only a similarly named folder exists (e.g. `Groove (2000)`), treat it as a wrong-match candidate, not as evidence the correct movie is present.
3. If the folder is truly the wrong movie, remove the wrong library folder, add the correct movie to Radarr with the normal root folder/quality profile, and set `addOptions.searchForMovie=true`.
4. Verify Radarr queue/history shows a correct release for the intended movie, and tell the user it will appear after import/Jellyfin scan.

Representative commands:

```sh
arr radarr raw GET '/movie/lookup?term=The%20Emperor%27s%20New%20Groove' \
  | jq -r '.[] | [.tmdbId,.title,.year] | @tsv'

lookup=$(arr radarr raw GET '/movie/lookup?term=The%20Emperor%27s%20New%20Groove' \
  | jq 'map(select(.tmdbId==11688))[0]')
printf '%s' "$lookup" \
  | jq '. + {qualityProfileId:1, rootFolderPath:"/data/media/movies", monitored:true, minimumAvailability:"released", addOptions:{searchForMovie:true}}' \
  > /tmp/movie-add.json
arr radarr raw POST '/movie' "$(cat /tmp/movie-add.json)"
```

## Radarr import-blocked disc image / ID-matched release

Example: **Serie Noire** downloaded as a Blu-ray disc ISO. Radarr queue said `completed/importBlocked` with:

> Found matching movie via grab history, but release was matched to movie by ID. Manual Import required.

For Radarr, the local `arr import` helper is Sonarr-only. If the completed item is an awkward disc image (ISO/BR-DISK) and the user just wants the movie available, it may be faster and safer to remove/blocklist the stuck queue item and grab a normal encode.

Pattern:

```sh
arr radarr raw GET '/queue?includeUnknownMovieItems=true&includeMovie=true&pageSize=1000' \
  | jq '.records[] | select(.movie.title=="Serie Noire") | {id,downloadId,title,status,trackedDownloadState,statusMessages,outputPath}'

# If replacing the stuck item is preferable:
arr radarr raw DELETE '/queue/<queueId>?removeFromClient=true&blocklist=true'
arr radarr raw GET '/release?movieId=<movieId>' \
  | jq -r '.[] | [.title,.quality.quality.name,.size,(.rejections|join("; "))] | @tsv'

release=$(arr radarr raw GET '/release?movieId=<movieId>' \
  | jq 'map(select(.title=="<chosen normal encode>"))[0]')
arr radarr raw POST '/release' "$release"
```

## Alternate-title/manual Prowlarr search rescue

Example: **Judge Fayard Called the Sheriff** had no Radarr releases by English title, but Prowlarr found it via partial French title and a misspelled release (`Le Juge Fayard Dit Le Shriff`).

Pattern:

1. Inspect Radarr metadata for `originalTitle` and `alternateTitles`.
2. Search Prowlarr with several shorter/original-language variants, not only the English title.
3. Use a dry run before grabbing a Prowlarr result directly to SAB with `--cat radarr`.

```sh
arr radarr get 'Judge Fayard Called the Sheriff' \
  | jq '{title,originalTitle,alternateTitles:[.alternateTitles[].title]}'

for q in 'Judge Fayard Called the Sheriff' 'Le juge Fayard dit Le Shériff' 'Le juge Fayard' 'Fayard Sheriff'; do
  arr prowlarr search "$q" --limit 10
done

arr prowlarr grab 'Le Juge Fayard' --match 'Compress' --cat radarr --dry-run
arr prowlarr grab 'Le Juge Fayard' --match 'Compress' --cat radarr
```

## Follow-up watcher pattern

For multi-item downloads that may import after the chat ends, create a small `no_agent` cron watcher that stays silent until all target Radarr objects have `movieFile != null`, then posts the imported paths. This is better than promising to check later manually.
