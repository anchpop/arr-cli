# Radarr movie request with English subtitles when profile rejects usable releases

Use this pattern when a user asks for a specific movie plus English subtitles, Radarr lookup is unambiguous, but the active quality profile rejects all usable releases (for example because WEB-DL/1080p/720p is not wanted in the current profile).

## Pattern

1. Add the exact movie identity by TMDB/IMDb/year, using the normal movie root/profile and `searchForMovie=true`.
2. Inspect Radarr releases and prefer a release whose title or metadata gives subtitle confidence. Useful clues:
   - `sub ENG`, `Eng Sub`, `English subtitles` in the release title.
   - Original-title release names for foreign films can be correct even if the English title is not present, e.g. `Cha.no.aji` for `The Taste of Tea`.
   - Usenet WEB-DL/WEBRip releases often include embedded SRT even if Radarr rejects them for profile reasons.
3. Query raw releases for the chosen row and save `downloadUrl`:

```sh
arr radarr raw GET '/release?movieId=<id>' \
  | jq -r '.[] | select(.title=="<exact release title>") | [.title,.quality.quality.name,.size,.downloadUrl] | @tsv'
```

4. If the release is clearly the intended movie/year and profile rejection is the only blocker, add it directly to SAB with category `radarr` and a distinctive name, then prioritize it:

```sh
url=$(arr radarr raw GET '/release?movieId=<id>' | jq -r '.[] | select(.title=="<exact release title>") | .downloadUrl' | head -1)
arr sab add "$url" --cat radarr --name '<exact release title>'
arr sab prio '<distinct release words>' --top
```

5. Actively watch until Radarr imports; do not stop at SAB completion. Verify `arr radarr status`, `arr radarr history`, and the final media path.
6. Verify English subtitles on the imported file with ffprobe, not just the release title:

```sh
nix shell nixpkgs#ffmpeg -c ffprobe -v error \
  -show_entries stream=index,codec_type,codec_name:stream_tags=language,title \
  -of json "/data/media/movies/<Movie> (<Year>)/<file>.mkv" \
  | jq -r '.streams[] | [.index,.codec_type,.codec_name,(.tags.language//""),(.tags.title//"")] | @tsv'
```

7. Trigger Jellyfin refresh and verify Jellyfin sees the movie and subtitle stream:

```sh
curl -fsS -X POST "http://localhost:8096/Library/Refresh?api_key=$JELLYFIN_API_KEY" >/dev/null
curl -fsS "http://localhost:8096/Items?api_key=$JELLYFIN_API_KEY&Recursive=true&SearchTerm=<url-encoded-title>&Limit=20&Fields=Path,MediaStreams" \
  | jq -r '.Items[] | select(.Type=="Movie" and .ProductionYear==<year>) | [.Name,.ProductionYear,.Path,(.MediaStreams[]? | select(.Type=="Subtitle") | ((.Language//"")+":"+(.Title//.DisplayTitle//"")))] | @tsv'
```

## Reporting

Only tell the user subtitles are confirmed after the imported file has an English subtitle stream or English sidecar. If the file has not imported yet, say subtitle verification is pending.
