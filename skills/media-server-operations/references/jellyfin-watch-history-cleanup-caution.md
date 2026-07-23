# Jellyfin watch-history cleanup caution

When generating library cleanup candidates based on Jellyfin playback data, avoid interpreting current DB rows too strongly.

## Observed pattern

A direct Jellyfin DB check can show current episode items with zero matching watched/progress rows even for shows the user knows they watched. In one session, Doctor Who, Agatha Christie's Poirot, The Goes Wrong Show, and The Creep Tapes all appeared as `0` current watched/progress episode rows despite the user reporting substantial viewing.

This means the result should be interpreted as:

> No current Jellyfin playback data is attached to these episode items.

It should **not** be interpreted as:

> Nobody watched these episodes.

## Likely causes to consider

- Jellyfin item IDs changed after restore/rescan/re-identification.
- Historical `UserData` rows may be keyed by provider/custom data keys rather than current item IDs.
- A reboot or service restart may coincide with a library rescan that detached old progress from current library items.
- The user may have watched under a now-missing user or old library identity.

## Reporting guidance

- Say "no current Jellyfin playback data found" rather than "unwatched".
- Mark output as cleanup candidates requiring review, not delete-ready.
- If the user corrects the result, trust their viewing history and treat Jellyfin data as incomplete.
- Exclude or flag known false positives before recommending deletion.

## Probe pattern

When checking the DB, remember Jellyfin TV episode type names are fully qualified, e.g. `MediaBrowser.Controller.Entities.TV.Episode`, not simply `Episode`.

Also consider joining against provider/custom keys in addition to `UserData.ItemId`, but do not assume that absence across these joins proves absence of historical viewing.
