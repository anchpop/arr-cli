# Safe Sonarr Season Deletion / Pruning

Use this when the user asks to delete later seasons while keeping earlier seasons.

## Safety-first workflow

1. Confirm the target series and season inventory:
   ```sh
   arr sonarr seasons '<series title>'
   arr sonarr get '<series title>' | jq '{id,title,path,seasons:.seasons[] | {seasonNumber, monitored, stats:.statistics}}'
   ```
2. Calculate exactly which episode files will be deleted and the expected freed size:
   ```sh
   arr sonarr raw GET '/episode?seriesId=<seriesId>&includeEpisodeFile=true' \
     | jq '[.[] | select(.seasonNumber>=<start> and .seasonNumber<=<end> and .hasFile==true) |
            {seasonNumber, episodeNumber, episodeFileId, path:.episodeFile.path, size:.episodeFile.size}]
           | {count:length,
              totalGB:((map(.size // 0)|add)/1000000000),
              bySeason:(group_by(.seasonNumber)|map({season:.[0].seasonNumber,count:length,gb:((map(.size//0)|add)/1000000000)}))}'
   ```
3. Disable monitoring for seasons being removed before deleting, so Sonarr does not re-download them. For keeping only seasons 1 and 2:
   ```sh
   arr sonarr monitor '<series title>' s1,s2
   ```
4. Delete through Sonarr's episode-file API, not raw filesystem deletion, so Sonarr's DB stays consistent:
   ```sh
   ids=$(arr sonarr raw GET '/episode?seriesId=<seriesId>&includeEpisodeFile=true' \
     | jq -r '.[] | select(.seasonNumber>=<start> and .seasonNumber<=<end> and .hasFile==true) | .episodeFileId' \
     | sort -n | uniq)
   for id in $ids; do
     arr sonarr raw DELETE "/episodefile/$id" >/dev/null
   done
   ```
5. Verify Sonarr state and files:
   ```sh
   arr sonarr seasons '<series title>'
   arr sonarr status '<series title>'
   ```
   Also confirm the library directory only contains intended season files.
6. Remove empty season directories only after all files are gone and the directory names are exactly the intended target seasons.

## Communication

Report:
- which seasons were deleted and which were kept,
- episode count and approximate size deleted,
- monitoring state changed to prevent re-download,
- verification result.

## Pitfalls

- Do not delete files directly from `/data/media` first; Sonarr may keep stale episode-file records.
- Do not leave deleted seasons monitored unless the user explicitly wants them re-acquired later.
- Watch out for Season 0 specials: user language like "last 6 seasons" usually means regular seasons, not specials.
