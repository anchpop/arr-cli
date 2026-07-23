# Seerr request repair case notes: June 2026 anime/TV cleanup

These are durable patterns from a Seerr/Sonarr/Radarr cleanup session, not a task log. Use them as examples when future unfulfilled-request reports disagree with Arr state.

## Interpret Seerr request status at the requested-scope level

- For TV requests, inspect `.seasons[]` on the Seerr request. A request can be fulfilled (`status: 5` on the requested seasons) while `media.status` remains partial because future/unrequested seasons are incomplete.
- Concrete examples:
  - `Sgt. Frog`: requested S1 was fulfilled (`S1 51/51`), while the series/media stayed partial because later seasons were not all complete.
  - `Love Island USA`: requested S7/S8 showed available in Seerr, while S8 was ongoing/future and the whole series remained partial. Do not call that request broken.
  - `JUJUTSU KAISEN`: request was S1 only; after `S1 24/24`, Seerr request/media became available even though S2/S3 were empty.

## Safe repair sequence for a batch of requests

1. Pull Seerr request detail for each ID and preserve both request `status` and season statuses.
2. Pull Sonarr/Radarr season/movie state and missing monitored episodes.
3. Search/grab only when mappings are obvious and the Arr app accepts the release/profile.
4. For anime season searches, expect slow per-episode crawling and duplicate NZB attempts. Report `in progress` if the search/download is still running rather than overstating completion.
5. After imports, trigger Jellyfin refresh plus Seerr Sonarr/Radarr scan and availability sync, then re-read Seerr detail.

## Examples of fixes and blockers

- `Baby Assassins Everyday!`: missing E08/E10/E12 had exact usenet releases; per-episode search/grab completed and S1 became `12/12`, then Seerr request became available.
- `JUJUTSU KAISEN`: missing S1E17 was enough to keep request partial. A targeted episode search fixed S1; S2/S3 were irrelevant to that Seerr request.
- `High School D×D`: requested S1 was `2/12`; S2/S3/S4 were already available. A season search can run a long time because Sonarr iterates many per-episode searches; leave a background watcher and report partial progress.
- `Panty & Stocking with Garterbelt`: season search may queue many overlapping/duplicate releases. Watch SAB warnings and queue state; don't assume 19 downloaded reports mean 19 useful imports until season counts update.
- `Judge Fayard Called the Sheriff` and `Art Museum by the Zoo`: Radarr searches returning `found 0 release(s)` are true blockers; do not invent alternate matches.

## Background watcher pattern

When Arr/SAB work is legitimately long-running, start a tracked background watcher with `notify_on_complete=true` that periodically prints compact status:

```bash
for i in $(seq 1 90); do
  sleep 20
  arr sab status | sed -n '1,8p'
  arr sonarr-anime seasons 'High School D×D' | grep 'S1' || true
  arr sonarr-anime seasons 'Panty and Stocking with Garterbelt' | grep 'S1' || true
  hs=$(arr sonarr-anime raw GET '/command/<id>' | jq -r '.status')
  ps=$(arr sonarr-anime raw GET '/command/<id>' | jq -r '.status')
  # break when commands are done and SAB has no remaining bytes
  # ...
done
```

Use this for bounded searches/imports; do not use untracked shell backgrounding.
