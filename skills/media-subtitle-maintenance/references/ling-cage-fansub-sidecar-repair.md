# Ling Cage / Spirit Cage subtitle repair pattern

Session-derived pattern for a show whose Sonarr path/title is `Ling Cage` but Jellyfin may display it as `Spirit Cage: Incarnation 2`.

## Problem shape

- Season 1 files existed as raw/WEB-DL `.mp4` files with no embedded subtitles and no sidecars:
  - `/data/media/anime/Ling Cage/Season 1/Ling.Cage.S01E01...StarfallWeb.mp4` through `S01E12`.
- Season 2 files already had embedded ASS subtitles from Falling Star Pavilion, but one episode (`S2E03`) had the subtitle stream language untagged, so Jellyfin did not consistently present it as English.
- `subliminal` collected the videos but timed out / did not produce sidecars for season 1. This is a provider-no-match situation, not proof subtitles do not exist.

## Useful source

Web search found a Nyaa release page for `[Cortyx] Ling Cage S1 (1080p)` whose description includes a Google Drive link labelled `Just subs and fonts`:

- Nyaa release: `nyaa.si/view/1983002`
- Google Drive folder id observed in-session: `1zayFsEkB0h54MZ26mI13h5tanPF4G-BP`

The Drive folder contained `[Cortyx] Ling Cage S1 (1080p)` with episode `.ass` files plus fonts/chapters. Download with `gdown`:

```bash
mkdir -p /data/hermes/ling-cage-subs
cd /data/hermes/ling-cage-subs
nix shell nixpkgs#python313Packages.gdown -c \
  gdown --folder 'https://drive.google.com/drive/folders/1zayFsEkB0h54MZ26mI13h5tanPF4G-BP?usp=sharing' \
  --remaining-ok --fuzzy
```

## Installing S1 sidecars

Map Cortyx `01`..`12` to the StarfallWeb `S01E01`..`S01E12` files. Do **not** accidentally map Cortyx `06_5 - Middle Chapter` into normal episode 7.

A duration sanity check is useful before installing:

```bash
MEDIA='/data/media/anime/Ling Cage/Season 1'
SUBS='/data/hermes/ling-cage-subs/[Cortyx] Ling Cage/[Cortyx] Ling Cage S1 (1080p)'
for ep in $(seq -w 1 12); do
  f=$(find "$MEDIA" -maxdepth 1 -type f -name "*S01E$ep*.mp4" | head -1)
  s=$(find "$SUBS" -maxdepth 1 -type f -name "* - $ep *.ass" | head -1)
  vd=$(nix shell nixpkgs#ffmpeg-headless -c ffprobe -v error -show_entries format=duration -of default=nk=1:nw=1 "$f")
  last=$(grep -a '^Dialogue:' "$s" | awk -F, '{print $3}' | tail -1)
  printf '%s\t%.1f\t%s\t%s\n' "$ep" "$vd" "$last" "$(basename "$s")"
done
```

Install sidecars using Jellyfin language-aware exact stems:

```bash
MEDIA='/data/media/anime/Ling Cage/Season 1'
SUBS='/data/hermes/ling-cage-subs/[Cortyx] Ling Cage/[Cortyx] Ling Cage S1 (1080p)'
for n in $(seq 1 12); do
  ep=$(printf '%02d' "$n")
  video=$(find "$MEDIA" -maxdepth 1 -type f -name "*S01E$ep*.mp4" | head -1)
  sub=$(find "$SUBS" -maxdepth 1 -type f -name "* - $ep *.ass" | head -1)
  cp -f "$sub" "${video%.*}.eng.ass"
  chmod 664 "${video%.*}.eng.ass"
done
```

## Retagging an embedded subtitle language

If one MKV has an embedded subtitle track but `ffprobe` shows no language tag, remux without re-encoding and set the subtitle language:

```bash
F='/data/media/anime/Ling Cage/Season 2/[FSP DN] Ling Cage S2E03 [1080P BGlobal].mkv'
TMP='/data/media/anime/Ling Cage/Season 2/.s2e03.engsubtag.tmp.mkv'
BAK='/data/media/anime/Ling Cage/Season 2/[FSP DN] Ling Cage S2E03 [1080P BGlobal].pre-engsubtag.bak.mkv'
nix shell nixpkgs#ffmpeg-headless -c ffmpeg -hide_banner -y -i "$F" \
  -map 0 -c copy -metadata:s:s:0 language=eng "$TMP"
mv -f "$F" "$BAK"
mv -f "$TMP" "$F"
chmod 664 "$F"
mkdir -p /data/downloads/hermes-backups/ling-cage
mv -f "$BAK" /data/downloads/hermes-backups/ling-cage/
```

## Verification

1. Refresh Jellyfin:

```bash
curl -fsS -X POST "http://localhost:8096/Library/Refresh?api_key=$JELLYFIN_API_KEY" >/dev/null
```

2. Search by path/title, not just exact display name. Jellyfin may show the series as `Spirit Cage: Incarnation 2` even though the filesystem path is `/data/media/anime/Ling Cage`.

```bash
curl -fsS "http://localhost:8096/Items?api_key=$JELLYFIN_API_KEY&Recursive=true&SearchTerm=Cage&Limit=100&Fields=Path" \
  | jq -r '.Items[] | select((.Path//"")|contains("/data/media/anime/Ling Cage")) | [.Type,.Name,.Id,(.Path//"")] | @tsv'
```

3. Use the resulting series id with `/Shows/<id>/Episodes?Fields=MediaStreams,Path` and verify every episode has subtitle language `eng`.
