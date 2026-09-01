# Shelfmark book downloads over the REST API (2026-08-31)

> **Prefer `arr book add '<title|isbn>'`** — it runs this whole flow (plus
> Jellyfin dedup, edition resolve, and a usenet probe) as one command. This
> doc is the raw API for when the command's diagnosis isn't enough.

Shelfmark (`http://localhost:8084`, podman container, v1.3.13) fronts Anna's
Archive / Libgen / Z-Library for the books usenet can't find. Auth is OFF
(`AUTH_METHOD: "none"`) — the full UI and API work from localhost with no
credentials, so drive it directly with curl. First proven end-to-end on
*Contextual Design: Design for Life* 2nd ed. (Holtzblatt/Beyer), which both
Bindery ISBNs + title searches had missed on usenet.

## The happy path

```sh
# 1. Metadata search (Hardcover provider; title or ISBN both work)
curl -s 'http://localhost:8084/api/metadata/search?query=9780128011362'
#    → books[].provider ("hardcover") + provider_id

# 2. Release search — live Anna's Archive query, allow ~30s
curl -s "http://localhost:8084/api/releases?provider=hardcover&book_id=<id>&title=<title>"
#    → releases[].source_id (an AA md5), format, size

# 3. Queue the download
curl -s -X POST -H 'Content-Type: application/json' \
  -d '{"source":"direct_download","source_id":"<md5>","title":"<title>","format":"epub"}' \
  http://localhost:8084/api/releases/download

# 4. Poll until complete (states: queued→resolving→downloading→complete/error)
curl -s http://localhost:8084/api/status
```

Shelfmark organizes the finished file itself (since 2026-09-01:
`FILE_ORGANIZATION=organize`, template `{Author}/{Title} ({Year})/{Title} -
{Author}` — matches the existing library layout, per-book folder so cover
art works). Do NOT move/rename its output by hand, and do not build extra
normalization tooling — the template in Shelfmark's Downloads settings is
the one knob. The file lands under `/data/media/books` as `bindery:media` and
Jellyfin's Books library sees it (realtime monitor; `arr jellyfin refresh`
nudges, `arr jellyfin has '<title>'` confirms). AA slow-partner speed is
~1 MB/s — a 95 MB epub takes ~20 min; that's normal, not stuck.

Books never touch the arrs, so the download-notifier will NOT DM the
requester — do that yourself once `arr jellyfin has` confirms it.

**Canonical config, not experiment residue:** `direct_download` ENABLED with
AA mirrors `.gl/.pk/.gd` is the intended steady state (set deliberately
2026-08-31, verified working). Leave it enabled; "restore the original
values" hygiene from earlier troubleshooting does not apply to this.

## Failure modes actually hit (and their fixes)

- **`"No metadata provider configured"`** from step 1 → the Hardcover
  provider got disabled/re-inited. Its key lives ONLY in
  `/data/.shelfmark/plugins/hardcover.json` (not sops) — if it's gone, Andre
  re-enters it in Settings → Metadata Providers.
- **`"Unable to reach download source. Network restricted or mirrors are
  blocked."`** from step 2 → the Anna's Archive domains rotted again (they
  rotate under legal pressure; `.org`/`.se`/`.li` all died in 2026,
  current are `.gl`/`.pk`/`.gd`). Find the live domains (AA's Wikipedia
  article tracks them), then:
  `curl -X PUT -H 'Content-Type: application/json' -d '{"AA_BASE_URL":"https://annas-archive.gl","AA_MIRROR_URLS":["https://annas-archive.gl","https://annas-archive.pk","https://annas-archive.gd"]}' http://localhost:8084/api/settings/mirrors`
  **Settings changes need a container restart to take effect** (the process
  caches config; the API's `requiresRestart:false` is wrong about this).
  Hermes has a polkit grant for exactly this:
  `systemctl restart podman-shelfmark.service` (added 2026-08-31).
- **Same error even with live mirrors, on Shelfmark < 1.3.13** → every AA
  domain now puts `/search` behind a DDoS-Guard JS challenge; only ≥ 1.3.13
  falls back to the built-in bypasser for search. The image tag is pinned in
  configuration.nix — a bump is Andre's (rebuild).
- **Step 2 returns `releases: 0` instantly with `sources_searched: []`** →
  the `direct_download` release source is disabled
  (`GET /api/release-sources` shows `enabled`). Re-enable:
  `curl -X PUT -H 'Content-Type: application/json' -d '{"DIRECT_DOWNLOAD_ENABLED":true}' http://localhost:8084/api/settings/download_sources`
  — then the restart caveat above applies.
