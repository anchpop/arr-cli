# Jellyfin Playback Failure: Corrupt/Undecodable Episode Replacement

Use this when a user says a specific Jellyfin episode/show "isn't loading", spins forever, or starts a transcode but does not play, while the server and library are otherwise reachable.

## Pattern observed

- Jellyfin showed the series and episode correctly, and Sonarr reported the season on disk.
- The active Jellyfin session showed the user paused/spinning on the episode with `PlayMethod: Transcode` and transcode reasons like `ContainerNotSupported`, `VideoCodecNotSupported`, and `AudioCodecNotSupported`.
- The source file was an old AVI whose video stream was detected as `rawvideo`; an ffmpeg decode smoke test emitted many `Packet too small` / `Invalid buffer size` errors and failed.
- Replacing the single episode with a clean MKV/REMUX fixed the issue; Jellyfin then needed a library/item refresh or the client needed to reopen the episode to drop the stale item path.

## Diagnostic commands

Find the active user/session and current item:

```sh
curl -fsS "http://localhost:8096/Sessions?api_key=$JELLYFIN_API_KEY" \
  | jq '.[] | {UserName,Client,DeviceName,NowPlayingItem,PlayState,TranscodingInfo,LastActivityDate}'
```

Verify Jellyfin sees the series/episodes and paths:

```sh
SERIES_ID=$(curl -fsS "http://localhost:8096/Items?api_key=$JELLYFIN_API_KEY&Recursive=true&SearchTerm=<title>&Limit=20" \
  | jq -r '.Items[] | select(.Type=="Series" and .Name=="<exact title>") | .Id' | head -1)

curl -fsS "http://localhost:8096/Shows/$SERIES_ID/Episodes?api_key=$JELLYFIN_API_KEY&Fields=Path,MediaSources&Limit=200" \
  | jq -r '.Items[] | [.ParentIndexNumber,.IndexNumber,.Name,(.Path//""),((.MediaSources//[])|length)] | @tsv'
```

Run a decode smoke test against the exact file path from Jellyfin:

```sh
nix shell nixpkgs#ffmpeg -c ffmpeg -v error -t 10 -i "/data/media/tv/<Show>/Season N/<episode file>" -f null - \
  && echo 'decode: OK' || echo 'decode: FAIL'
```

If the test fails with decode errors, treat the media file as bad even if Jellyfin/Sonarr counts are green.

## Replacement workflow

1. Search for a replacement of the exact episode/release:
   ```sh
   arr prowlarr search '<Show> S01E07' --limit 20
   ```
2. Grab one clearly matching replacement; use a restrictive match and dry-run first:
   ```sh
   arr prowlarr grab '<Show> S01E07' --indexer '<Indexer>' --match '<distinct.release.name>' --cat sonarr --dry-run
   arr prowlarr grab '<Show> S01E07' --indexer '<Indexer>' --match '<distinct.release.name>' --cat sonarr
   arr sab prio '<distinct.release.name>' --top
   ```
3. If Sonarr imports automatically, verify the file path changed. If it blocks with `Not a quality revision upgrade for existing episode file(s)` while the old file is known bad, manually import the completed replacement into the explicit episode:
   ```sh
   arr sonarr import '/data/downloads/usenet/complete/sonarr/<release-folder>' \
     --series '<Show>' --match 'S01E07' --season 1 --map se --mode move --dry-run
   arr sonarr import '/data/downloads/usenet/complete/sonarr/<release-folder>' \
     --series '<Show>' --match 'S01E07' --season 1 --map se --mode move --wait --timeout 300
   ```
4. Verify the new file decodes cleanly:
   ```sh
   nix shell nixpkgs#ffmpeg -c ffmpeg -v error -t 10 -i '<new library file>' -f null - && echo OK
   ```
5. Trigger a Jellyfin refresh and verify Jellyfin's episode path now points to the replacement:
   ```sh
   curl -fsS -X POST "http://localhost:8096/Library/Refresh?api_key=$JELLYFIN_API_KEY" >/dev/null
   ```

## Load-related pitfall

If playback is generally sluggish, check system load and active ffmpeg jobs. A background media-encoder job can consume most CPU and make transcodes feel stuck even when the target file is valid. If permitted and appropriate, temporarily stop `media-encoder.service` to free CPU for live playback, then continue diagnosing the episode itself.

Do not stop after finding high load: in the observed case, high load was real, but the target episode file was also corrupt/undecodable. Verify both system load and the exact source file.
