# Jellyfin / Sonarr episode has the wrong default audio language

Use this when a user says some episodes are "not in English" or are playing in the wrong dub, especially when the file may already contain an English audio stream but Jellyfin is choosing another default track.

## Pattern

1. Resolve the exact series path from Sonarr/Jellyfin rather than guessing.
2. Audit all episode media files with `ffprobe`, inspecting audio/subtitle language tags and `disposition.default`:

```sh
nix shell nixpkgs#ffmpeg -c python - <<'PY'
import os, json, subprocess, glob
for f in sorted(glob.glob('/data/media/tv/<Series>/Season <N>/*.mkv')):
    p = subprocess.run([
        'ffprobe','-v','error',
        '-show_entries','stream=index,codec_type,codec_name:stream_tags=language,title:stream_disposition=default',
        '-of','json',f], capture_output=True, text=True, check=True)
    print('\nFILE:', os.path.basename(f))
    for s in json.loads(p.stdout).get('streams', []):
        if s.get('codec_type') in ('audio','subtitle'):
            tags = s.get('tags') or {}; disp = s.get('disposition') or {}
            print(f"  #{s.get('index')} {s.get('codec_type')} {s.get('codec_name')} lang={tags.get('language','')} title={tags.get('title','')} default={disp.get('default',0)}")
PY
```

3. If the correct English audio exists but is not default, remux only; do not transcode or replace the release unnecessarily. Keep all streams unless there is a clear reason to drop one.

Example for a file where audio stream 0 is French/default and audio stream 1 is English/non-default:

```sh
DIR='/data/media/tv/<Series>/Season <N>'
SRC="$DIR/<episode>.mkv"
TMP="$DIR/.<episode>.english-default.tmp.mkv"

nix shell nixpkgs#ffmpeg -c ffmpeg -y -v error -i "$SRC" \
  -map 0 -c copy \
  -disposition:a:0 0 -disposition:a:1 default \
  -disposition:s:0 0 -disposition:s:1 0 -disposition:s:2 0 -disposition:s:3 0 -disposition:s:4 0 \
  "$TMP"
```

Adjust the `-disposition` indexes to the actual stream layout. Use `-map 0 -c copy` so this is a metadata/container remux, not a quality-changing encode.

4. Verify the temp file's stream dispositions before replacing the library file:

```sh
nix shell nixpkgs#ffmpeg -c ffprobe -v error \
  -show_entries stream=index,codec_type,codec_name:stream_tags=language,title:stream_disposition=default \
  -of compact=p=0:nk=1 "$TMP"
```

5. Replace atomically enough for this library workflow: move the original aside, move the temp into place, set readable permissions, and only remove the backup after verification:

```sh
BAK="$SRC.bak-wrong-default-audio"
mv "$SRC" "$BAK"
mv "$TMP" "$SRC"
chmod 664 "$SRC"
# after final verification:
rm -f "$BAK"
```

6. Re-audit every episode in the affected season and verify all default audio tracks are English. Then trigger Jellyfin refresh:

```sh
curl -fsS -X POST "http://localhost:8096/Library/Refresh?api_key=$JELLYFIN_API_KEY" >/dev/null
```

## Reporting

Say exactly which episode(s) had the wrong default language, whether English audio was already present, what changed, and that Jellyfin refresh was triggered. If a season is missing entirely or unmonitored in Sonarr, mention that separately from the audio fix.
