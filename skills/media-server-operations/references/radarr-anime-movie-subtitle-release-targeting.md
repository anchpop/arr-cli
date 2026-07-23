# Radarr Anime Movie Subtitle Release Targeting

Use this pattern when a user requests an anime/foreign-language movie with a specific language/subtitle requirement, e.g. Japanese audio with English subtitles.

## Lesson

A normal Radarr add/search can immediately queue a large BR-DISK/BD50 release that technically satisfies the quality profile but is a poor fit for a casual watch request and can prevent better encoded releases from being selected (`Quality for release in queue already meets cutoff`). Treat the language/subtitle requirement as a verification task and actively choose a sane encoded release when Radarr's automatic pick is a disc image or likely hard to import.

## Workflow

1. Add/identify the exact movie by TMDB/IMDb/year/original title.
2. Inspect Radarr queue right after add/search. If the active item is BR-DISK/BD50/complete Blu-ray/ISO-sized and the request is simply to watch the movie, remove and usually blocklist that queue item.
3. Inspect releases for titles that advertise the needed streams/subs (`JP`, `Japanese`, `Dual Audio`, `Sub(...EN...)`, `English Subtitles`, etc.). Prefer normal encoded MKV releases over BR-DISK.
4. If Radarr rejects a useful release only because the previous bad queue item met cutoff or the profile dislikes the size/language label, grab exactly one release through Prowlarr/SAB/qBittorrent using restrictive `--match`/`--indexer` dry-run first.
5. After a manual direct grab, re-check Radarr queue. If a stale bad/stalled torrent remains attached to the movie, remove just that Radarr queue row and, if appropriate, delete the abandoned torrent from the download client so `arr radarr watch --verify-subs` does not keep reporting STUCK for the wrong item.
6. Promote the intended SAB job if it is behind other downloads.
7. Use a bounded no-agent watcher that stays silent while downloading/importing, then verifies the final Radarr file with both `--verify-audio <lang>` and `--verify-subs eng`, refreshes Jellyfin, and reports once.

## Pitfalls

- Do not claim subtitles are satisfied from the release title alone. Verify the final imported file with ffprobe/`arr radarr watch --verify-subs eng`.
- Radarr queue matching can show multiple items for the same movie after manual intervention. Clean up the obsolete item or the watcher may report stuck despite the intended replacement downloading normally.
- A torrent advertised with English subtitles may be stalled/no-seed. If a usenet encode is available, it can be a better follow-through path even when smaller than the profile's normal minimum; verify after import rather than relying on Radarr's profile decision.
