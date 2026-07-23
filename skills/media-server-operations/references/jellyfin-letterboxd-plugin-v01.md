# Jellyfin Letterboxd plugin v0.1 pattern

Session-derived reference for building out a custom Jellyfin plugin that surfaces server members' public Letterboxd activity.

## When this applies

Use when a user asks for Letterboxd/social movie-opinion integration with Jellyfin or the media server. A Jellyfin plugin makes sense when the desired surface is Jellyfin-owned data/endpoints or a future Jellyfin/Homepage UI; a Hermes plugin/Discord workflow is lower-friction for conversational Q&A, but the server already has a custom Jellyfin plugin path for this integration.

## Source and deployment paths

- Source repo: `/data/hermes/hermes-plugins/`
- Plugin subdir: `/data/hermes/hermes-plugins/jellyfin-letterboxd/`
- Deployed plugin dir: `/data/.nixflix/jellyfin/plugins/Jellyfin.Plugin.Letterboxd/`
- Keep repo remote on SSH: `git@github.com:anchpop/hermes-plugins.git`. HTTPS push prompts for credentials and fails in the Hermes service environment.

## Known-good v0.1 shape

The v0.1 implementation added:

- `PluginConfiguration.Users` with explicit opt-in mappings from display/Jellyfin/Discord user to Letterboxd username. Keep the C# list default empty; Jellyfin XML deserialization appends onto collection defaults, so non-empty defaults duplicate users after restart. Use the live Jellyfin plugin configuration API and/or `jellyfin-letterboxd/config.example.json` for the current user mapping.
- `LetterboxdService` that fetches `https://letterboxd.com/<username>/rss/`, parses RSS XML, and caches JSON in the plugin data folder.
- Parsed fields: title, year, TMDb movie ID, rating, liked, rewatch, watched date, publish date, Letterboxd URL, cleaned review text.
- Authenticated REST endpoints:
  - `GET /Letterboxd/Users`
  - `GET /Letterboxd/Recent?limit=20`
  - `GET /Letterboxd/Film/{tmdbMovieId}`
  - `GET /Letterboxd/Search?query=<title>&limit=20`
  - `POST /Letterboxd/Sync`
- `IScheduledTask` named `Sync Letterboxd profiles`, key `LetterboxdSync`, default 12-hour interval.
- `IPluginServiceRegistrator` registering `AddHttpClient()` and `LetterboxdService`.

## Build/deploy/verify commands

```sh
cd /data/hermes/hermes-plugins/jellyfin-letterboxd
export HOME="$PWD/.home"
export DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_NOLOGO=1
nix --extra-experimental-features 'nix-command flakes' shell nixpkgs#dotnet-sdk_9 -c \
  dotnet build -c Release -o ./out
bash ./deploy.sh
```

Verify plugin load and endpoints:

```sh
curl -fsS -H "X-Emby-Token: $JELLYFIN_API_KEY" http://localhost:8096/Plugins \
  | jq -r '.[] | select(.Name=="Letterboxd") | "\(.Name) v\(.Version) [\(.Status)]"'

curl -fsS -X POST -H "X-Emby-Token: $JELLYFIN_API_KEY" \
  http://localhost:8096/Letterboxd/Sync | jq .

curl -fsS -H "X-Emby-Token: $JELLYFIN_API_KEY" \
  'http://localhost:8096/Letterboxd/Search?query=Ratatouille&limit=2' | jq .

curl -fsS -H "X-Emby-Token: $JELLYFIN_API_KEY" http://localhost:8096/ScheduledTasks \
  | jq '.[] | select(.Key=="LetterboxdSync") | {Name,Key,State,LastExecutionResult,Triggers}'
```

Expected v0.1 verification example:

- Plugin: `Letterboxd v0.1.0.0 [Active]`
- Sync result: one configured user, 50 entries, no errors
- Scheduled task: `Sync Letterboxd profiles`, key `LetterboxdSync`, `Idle`, last result `Completed`

## Adding Letterboxd users

Do not edit Jellyfin's XML config file directly from the Hermes file tool; the deployed Jellyfin config directory may be outside the writable sandbox. Use Jellyfin's plugin configuration API instead, then sync and verify.

1. Verify the public RSS feed exists:
   ```sh
   curl -fsSL 'https://letterboxd.com/<username>/rss/' | head
   ```
2. Fetch the current plugin id/config:
   ```sh
   PLUGIN_ID=$(curl -fsS -H "X-Emby-Token: $JELLYFIN_API_KEY" http://localhost:8096/Plugins \
     | jq -r '.[] | select(.Name=="Letterboxd") | .Id')
   curl -fsS -H "X-Emby-Token: $JELLYFIN_API_KEY" \
     "http://localhost:8096/Plugins/$PLUGIN_ID/Configuration" | jq .
   ```
3. POST the full updated config JSON (including all existing users; do not send a partial list):
   ```sh
   curl -fsS -X POST -H "X-Emby-Token: $JELLYFIN_API_KEY" -H 'Content-Type: application/json' \
     --data @/path/to/letterboxd-config-update.json \
     "http://localhost:8096/Plugins/$PLUGIN_ID/Configuration"
   ```
4. Build/deploy/restart if plugin code changed, then verify the live config after restart. If the C# `Users` default list is non-empty, Jellyfin XML deserialization can append saved users onto defaults and duplicate entries. Keep code defaults empty and store current mappings in live config / `config.example.json`.
5. Run sync and verify per-user counts/errors:
   ```sh
   curl -fsS -X POST -H "X-Emby-Token: $JELLYFIN_API_KEY" http://localhost:8096/Letterboxd/Sync | jq .
   jq -r '.users[] | "\(.displayName): \(.letterboxdUsername) entries=\(.entries|length) error=\(.error // "none")"' \
     /data/.nixflix/jellyfin/plugins/Jellyfin.Plugin.Letterboxd/letterboxd-cache.json
   ```

## Pitfalls

- Jellyfin serializes API responses from this plugin with PascalCase property names by default (`UserDisplayName`, `Rating`, etc.), while the JSON cache uses camelCase (`users`, `entries`). Use matching `jq` keys depending on whether reading the API or cache file.
- `TaskTriggerInfo.Type` should be `TaskTriggerInfoType.IntervalTrigger`, not a nonexistent `TaskTriggerInfo.TriggerInterval` constant.
- Public Letterboxd RSS gives TMDb IDs (`tmdb:movieId`), which is the reliable join key to Jellyfin/Radarr movie identity.
- Letterboxd RSS can include multiple watches/reviews for the same film; keep entries distinct by URL if rewatch history matters.
- This is backend infrastructure only. Surfacing directly on Jellyfin movie pages still needs separate UI work; a standalone page/Homepage widget or Discord/Hermes Q&A may be easier than patching the Jellyfin web client.
