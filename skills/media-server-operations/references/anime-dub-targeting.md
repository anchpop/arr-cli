# Anime Dub / Audio-Language Targeting Example

## Context

A user asked whether Hermes can request a show by dub, specifically the English dub of `Sgt. Frog` / `Sergeant Frog` / `Keroro Gunso`.

## Durable lesson

Normal request workflows do not reliably support choosing a dub. For anime, "dub" is often only visible in release titles (`English Dub`, `Dual Audio`, `Animax English Dub`, etc.) rather than as structured Sonarr metadata. Treat this as manual release targeting:

1. Verify whether the show already exists and which Sonarr instance owns it (`sonarr-anime` for anime).
2. Inspect current file language metadata if available via Sonarr episode files.
3. Search Prowlarr with both English and alternate/romanized titles plus `English dub`.
4. Grab exactly one clearly-labeled release using `--match` and `--dry-run` first.
5. Put the item in the correct category (`sonarr-anime`) and optionally promote it in SAB.
6. Watch queue/import state; expect possible manual import if Sonarr cannot map the release automatically.

## Commands from the Sergeant Frog case

```sh
arr sonarr-anime lookup 'Sergeant Frog'
arr sonarr-anime status 'Sgt. Frog'
arr sonarr-anime seasons 'Sgt. Frog'

arr sonarr-anime raw GET '/episodefile?seriesId=11' \
  | jq -r 'group_by(.seasonNumber)[] | "S\(.[0].seasonNumber): " + (map(.languages[].name) | unique | join(", ")) + " (" + (length|tostring) + " files)"'

arr prowlarr search 'Sgt Frog English dub' --limit 20
arr prowlarr search 'Sergeant Frog English dub' --limit 20

arr prowlarr grab 'Sergeant Frog English dub' \
  --match 'Animax English Dub' \
  --cat sonarr-anime \
  --dry-run

arr prowlarr grab 'Sergeant Frog English dub' \
  --match 'Animax English Dub' \
  --cat sonarr-anime

arr sab prio 'Sergeant Keroro' --top
arr sab queue 'Sergeant Keroro'
arr sonarr-anime queue 'Sergeant'
```

In this case, the useful indexed release was:

```text
[3mperorNero] Sergeant Keroro, Keroro Gunso Animax English Dub Web-DL
```

Existing Sonarr files were marked `Japanese`, so grabbing the labeled English dub release was justified.

## User-facing phrasing

Be concise and clear:

- "Not cleanly through the normal request UI, but I can manually target releases by dub/language when the indexers label them clearly."
- "Sonarr understands language better than dub, and anime releases are messy, so this is best-effort."
- "I found a release explicitly labeled English Dub / Dual Audio and grabbed that one."
