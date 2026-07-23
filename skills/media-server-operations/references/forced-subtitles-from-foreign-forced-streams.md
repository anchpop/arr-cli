# Forced English sidecars from non-English forced streams

Use this pattern when a show/movie has an embedded forced subtitle stream in a non-English language (for example French `Forced`) but Jellyfin needs an English forced sidecar for foreign-language dialogue.

## When this applies

- The video has English audio and full English `.en.srt`, but no `.eng.forced.srt` sidecar.
- `ffprobe` shows an embedded non-English forced subtitle stream, often tagged like `lang=fre`, `title=Forced`, `forced=1`.
- Full English subtitles may contain placeholder `_` cues for dialogue that was already translated in the original broadcast or release; do not assume full `.en.srt` can be filtered into a clean forced track.

## Workflow

1. Inspect subtitle streams:

```bash
nix shell nixpkgs#ffmpeg-headless -c ffprobe -v error \
  -select_streams s \
  -show_entries stream=index,codec_name:stream_tags=language,title:stream_disposition=forced \
  -of json '<media.mkv>' | jq -r \
  '.streams[] | [.index,.codec_name,(.tags.language//""),(.tags.title//""),(.disposition.forced//0)] | @tsv'
```

2. Extract the embedded forced stream as a timing/source-text scaffold:

```bash
nix shell nixpkgs#ffmpeg-headless -c ffmpeg -v error -y \
  -i '<media.mkv>' -map 0:<subtitle_stream_index> '<stem>.fre.forced.srt'
```

3. Convert that scaffold into an English forced sidecar named exactly beside the media file:

```text
<media stem>.eng.forced.srt
```

Use the extracted forced stream timings as the source of truth. Translate the cue text to English only if no trustworthy English forced source is available. If using machine translation, manually sanity-check domain terms and character/place names (e.g. Grey Worm, Unsullied, Khaleesi, Lord of Light, bloodriders, Meereen).

4. Skip subtitle-credit cues from the embedded stream, such as `Adaptation:` and `Sous-titrage:`.

5. Set library-readable mode and validate:

```bash
chmod 664 '<stem>.eng.forced.srt'
nix shell nixpkgs#ffmpeg-headless -c ffprobe -v error -f srt \
  -show_entries packet=pts_time -of csv=p=0 '<stem>.eng.forced.srt' | wc -l
```

6. Refresh Jellyfin and verify via the API that the new subtitle is visible as external English forced:

```bash
curl -fsS -X POST "http://localhost:8096/Library/Refresh?api_key=$JELLYFIN_API_KEY" >/dev/null
# Then inspect MediaStreams for Language=eng, IsExternal=true, IsForced=true.
```

## Pitfalls

- Do not trust a full English `.en.srt` by itself for foreign dialogue. Some full subtitles use `_` placeholders during Valyrian/Dothraki/etc. scenes because the translation is expected to come from a forced/burned track.
- Do not mark every episode as missing just because it lacks a forced sidecar. Some episodes may have no forced foreign-language stream and need no sidecar.
- External forum/share links for forced subtitle packs may disappear or return HTML/malware-advisory pages. If that happens, using the embedded forced stream as a timing scaffold is a durable fallback.
- If a file has only non-English forced subtitles and no authoritative English forced source, report that the resulting sidecar is best-effort unless you have manually verified wording against the episode/release.
