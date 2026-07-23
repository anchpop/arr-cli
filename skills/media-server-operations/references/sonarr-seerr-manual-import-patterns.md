# Sonarr/Seerr manual import patterns

These examples are condensed from a media repair session and are meant to guide future repairs, not to preserve task history.

## Seerr per-season status vs whole-media status

For TV requests, inspect both:

- request `status`
- `media.status`
- each requested season's `status`

A request can be fulfilled even when `media.status` remains partial. Example pattern:

```bash
arr seerr request <id> | jq '{id,status,mediaStatus:.media.status,seasons:[.seasons[]?|{seasonNumber,status}]}'
arr sonarr-anime seasons '<show>'
```

If the requested season is `status: 5` and Sonarr shows that season fully on disk, treat that request as fulfilled even if the whole series is partial.

## Alternate release for metadata-stuck torrent

If a torrent is stuck fetching metadata, search Prowlarr broadly and prefer a usenet alternate if available:

```bash
arr prowlarr search 'Oban Star Racers 1080p' --limit 20
arr prowlarr grab 'Oban Star Racers 1080p' --match '2ndfire' --indexer 'AnimeTosho (Usenet)' --cat sonarr-anime
```

After download, manual import with a verified dry-run:

```bash
folder='/data/downloads/usenet/complete/sonarr-anime/<release folder>'
arr sonarr-anime import "$folder" --series 'Ōban Star-Racers' \
  --match '[2ndfire] Oban Star-Racers' --season 1 --map abs --mode move --dry-run
arr sonarr-anime import "$folder" --series 'Ōban Star-Racers' \
  --match '[2ndfire] Oban Star-Racers' --season 1 --map abs --mode move
arr sonarr-anime seasons 'Ōban Star-Racers'
```

Only then clean SAB history/source files.

## Title matches but release number is wrong

Some releases are numbered differently from Sonarr/TVDB. Example pattern: a file named `S02E16.Double.Cross.My.Heart` was actually Sonarr's missing `S2E18 Double Cross My Heart`.

Process:

1. Check the Sonarr episode table and identify the correct missing episode ID.
2. Check the queue/manual import candidate's quality/languages.
3. POST a `ManualImport` command with the correct `episodeIds`, not the filename-parsed episode.

Payload shape:

```json
{
  "name": "ManualImport",
  "importMode": "copy",
  "files": [
    {
      "path": "/data/downloads/.../Release.Name.mkv",
      "seriesId": 101,
      "episodeIds": [6936],
      "quality": {"quality": {"id": 2, "name": "DVD", "source": "dvd", "resolution": 480}, "revision": {"version": 1, "real": 0, "isRepack": false}},
      "languages": [{"id": 1, "name": "English"}],
      "releaseGroup": "iVy"
    }
  ]
}
```

Then verify:

```bash
arr sonarr raw GET '/command/<id>' | jq '{status,message,exception}'
arr sonarr seasons 'Danny Phantom'
arr sonarr raw GET '/episode?seriesId=<seriesId>' | jq -r '.[] | select(.seasonNumber==2 and .episodeNumber==18) | [.id,.title,.hasFile,.episodeFileId] | @tsv'
```

## Anime pack caution

For anime such as Sayonara Zetsubou Sensei, Sonarr may split seasons/specials differently than release groups. Always verify dry-run mappings before import. If S1/S3 are complete but S2 is downloading, report exactly that; do not call the request complete until the remaining season imports.
