# Bazarr / Subliminal Subtitle Backfill Pattern

Use this when a user reports missing English subtitles for specific movies or shows in Jellyfin.

## What worked

1. Identify the exact library paths first; user titles may be slightly wrong or localized.
   - Example mismatches seen:
     - User said "Baby Assassins Everywhere" but library title was `Baby Assassins Everyday!`.
     - User said `Les Fugitifs`; library/Radarr folder was `The Fugitives (1986)`.
     - User typo/truncated title `Come to My Place, I Live at a Girlfriend'`; library folder was `Come to My Place, I Live at a Girlfriend's (1981)`.
2. Check for existing sidecar subtitles before doing work:
   ```sh
   find "$dir" -maxdepth 1 -type f \( -iname '*.srt' -o -iname '*.ass' -o -iname '*.vtt' \) -printf '%f\n' | sort
   ```
3. If Bazarr is installed but Hermes cannot access `/data/.bazarr` or the Bazarr API key, use Bazarr's purpose/config as context but do not try to read secrets. Prefer its UI/API only when an accessible API key exists.
4. Use `subliminal` via Nix for direct, reversible sidecar backfill:
   ```sh
   nix shell nixpkgs#python313Packages.subliminal -c subliminal download \
     -l en \
     -p opensubtitles -p tvsubtitles \
     -r metadata -r hash \
     -m 10 \
     "/data/media/tv/Show Name/Season 1"
   ```
   For movies:
   ```sh
   nix shell nixpkgs#python313Packages.subliminal -c subliminal download \
     -l en -p opensubtitles -p bsplayer -p tvsubtitles \
     -r metadata -r hash -m 20 \
     "/data/media/movies/Movie (Year)/Movie.File.mkv"
   ```
5. If a whole-season run times out, run one episode first to prove matching, then re-run the directory with fewer providers (for example only `opensubtitles` + `tvsubtitles`) and a long timeout.
6. For obscure/localized movies, retry with `-n` using original-language and English titles, but if all providers return `Downloaded 0 subtitle`, report that specific title as unavailable rather than claiming success.
7. Verify by counting videos vs sidecars and sizes:
   ```sh
   videos=$(find "$dir" -maxdepth 1 -type f \( -iname '*.mkv' -o -iname '*.mp4' -o -iname '*.avi' -o -iname '*.m2ts' \) | wc -l)
   subs=$(find "$dir" -maxdepth 1 -type f \( -iname '*.srt' -o -iname '*.ass' -o -iname '*.vtt' \) | wc -l)
   echo "videos=$videos subtitles=$subs"
   find "$dir" -maxdepth 1 -type f \( -iname '*.srt' -o -iname '*.ass' -o -iname '*.vtt' \) -printf '%f %s bytes\n' | sort
   ```
8. Trigger Jellyfin refresh when done:
   ```sh
   curl -fsS -X POST "http://localhost:8096/Library/Refresh?api_key=$JELLYFIN_API_KEY" >/dev/null
   ```

## Pitfalls

- Do not read or expose subtitle-provider credentials. If Bazarr's configured subscription exists but its API key/config is not available to Hermes, use direct safe tooling and report any remaining item that needs Bazarr UI/subscription intervention.
- OpenSubtitles/Bazarr/subliminal providers can return transient provider errors while still downloading from another provider. Capture the final downloaded count, not every noisy provider warning.
- The legacy `opensubtitlescom` Subliminal provider can emit huge/debuggy output or bad-request errors on broad searches. For routine backfills, start with `opensubtitles`, `tvsubtitles`, and optionally `bsplayer`; add `opensubtitlescom` only when needed.
- `.m2ts` movie files can still accept sidecar subtitles named after the stream file, e.g. `00002.en.srt`; verify the sidecar exists rather than assuming the generic file name is wrong.
- If a title has 0 subtitles after multiple title variants, be explicit: “everything else was added; this one returned no English subtitle result.”