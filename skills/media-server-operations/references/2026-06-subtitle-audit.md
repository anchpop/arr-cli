# 2026-06 Subtitle Audit Notes

Session-derived details for future media subtitle maintenance.

## What happened

A user first requested English subtitles for several foreign titles, then expanded the task to the whole Movies/TV/Anime library, then requested a second pass that also includes English-audio titles.

Important examples:

- `Baby Assassins: Nice Days` was not the series `Baby Assassins Everyday!`; Radarr/Jellyfin had the movie as `Baby Assassins 3 (2024)` with file `Baby Assassins Nice Days (2024) ...mp4`.
- `Santa Claus Is a Stinker (1985)` was the wrong item/play. The intended film was the 1982 movie with original title `Le père Noël est une ordure`; after replacement, `ffprobe` verified French audio plus embedded English/French subtitles.
- `Come to My Place, I Live at a Girlfriend's (1981)` / `Viens chez moi, j'habite chez une copine` exposed a verification bug: the report said it had English subs, but `ffprobe` showed only untagged audio and no subtitles.

## Sidecar detection bug to avoid

Bad pattern:

```bash
local matches=("$dir/$stem".en.srt "$dir/$stem".eng.srt ...)
((${#matches[@]} > 0))
```

Without correct `nullglob` handling, literal non-matching strings remain in the array, causing every file to appear to have an English sidecar.

Safer pattern:

```bash
for candidate in \
  "$dir/$stem.en.srt" "$dir/$stem.eng.srt" \
  "$dir/$stem.en.ass" "$dir/$stem.eng.ass" \
  "$dir/$stem.en.vtt" "$dir/$stem.eng.vtt"; do
  [[ -f "$candidate" ]] && return 0
done
find "$dir" -maxdepth 1 -type f \( \
  -iname "$stem.en.*.srt" -o -iname "$stem.eng.*.srt" -o \
  -iname "$stem.en.*.ass" -o -iname "$stem.eng.*.ass" -o \
  -iname "$stem.en.*.vtt" -o -iname "$stem.eng.*.vtt" \
\) -print -quit 2>/dev/null | grep -q .
```

## Pass structure that worked

1. Audio-tag pass over `/data/media/movies`, `/data/media/tv`, `/data/media/anime`:
   - skip English-audio files,
   - treat tagged non-English audio as foreign,
   - log unknown/untagged audio rather than guessing.

2. Metadata pass using Arr APIs:
   - Radarr `/movie`, Sonarr `/series`, Sonarr-anime `/series`,
   - `originalLanguage.name` identifies non-English originals even when streams are untagged,
   - still skip files with English audio unless the user asks otherwise.

3. All-title pass:
   - use when the user wants English subtitles even for English-audio titles,
   - skip only if English sidecar or embedded English subtitle is already present.

## Operational notes

- Long runs should use `terminal(background=true, notify_on_complete=true)` and write reports/logs under `/data/hermes/subtitle-audit/`.
- If a pass is terminated with exit `-15`, it was incomplete. Count the partial TSV rows and restart or continue rather than reporting final completion.
- Use status counts from TSV reports, not truncated background output, for summaries.
- Trigger Jellyfin refresh at the end of each successful pass.
