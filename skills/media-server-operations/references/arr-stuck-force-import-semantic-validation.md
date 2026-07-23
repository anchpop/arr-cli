# Semantic validation before applying `arr stuck --fix` force-imports

## Why the dry-run mapping is not sufficient by itself

`arr <svc> stuck --fix` can derive an episode mapping from the *inner media filename* that conflicts with the tracked queue item and the episode named in the release. A syntactically clean mapping such as:

```text
Doctor Who - The Caves Of Androzani Part 3.mp4 -> S1E3
```

is still obviously wrong when the completed folder is `Doctor.Who.S21E21...` and the content title is **The Caves of Androzani (3)**. Same-named/remade series make this especially dangerous: a subsequent broad `EpisodeSearch` can grab `Doctor.Who.S01E03.The.Unquiet.Dead` for classic Doctor Who's S01E03.

## Required pre-apply check

Before `stuck --fix --yes` applies any manual/force import, inspect every proposed mapping and confirm all three identities agree:

1. **Queue/grab mapping** — raw queue `series.id`, `episode.id`, season/episode, and title.
2. **Release/content identity** — completed-folder name, inner filename, and (when numbering is odd) the episode title in release metadata/NFO.
3. **Sonarr target** — `/episode/<id>` or `/episode?seriesId=...` title, `hasFile`, and current `episodeFile.relativePath`.

Reject the plan when the mapped episode title is incompatible with the release title, even if the parser reports a unique mapping. Do not let `--fix --yes` replace an existing episode file in that case.

For classic/modern shows sharing the same title, prefer an exact known release (`Forest of Fear`, not merely `Doctor Who S01E03`) and inspect release-title candidates before grabbing. After a search, immediately inspect the new queue row's title and mapped series/episode; blocklist a cross-series result before it downloads/imports.

## If a bad import already happened

1. Stop and inspect history using safe projected fields (`eventType`, `sourceTitle`, `episodeId`, `droppedPath`, `importedPath`).
2. Confirm the content's real intended episode and whether that episode already has a correct file.
3. Remove the wrongly imported episode file through Sonarr so its DB stays consistent.
4. Search/grab an exact replacement for the displaced episode, guarding against same-title/remake collisions.
5. Verify both episodes separately with `/episode?...&includeEpisodeFile=true`, then verify queue/history and filesystem paths.
6. Clear the stale original queue row **without blocklisting** only when the release itself was valid content and the episode it truly represented is already satisfied.

## Active download-client processing can resurrect a queue row

A queue removal may report success while SAB is still `Verifying`/`Repairing`; Sonarr can recreate the same queue row on its next client poll. Always re-run `stuck` and inspect the download-client job. If repair/verifying metrics are changing, treat it as active work and leave it alone unless the client reaches a terminal failure. If it is terminally dead, remove it from the client, then remove/blocklist the Sonarr row and re-query both sides.