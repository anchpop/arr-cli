// Auto-extracted from arr.py USAGE — keep in sync when commands change.
const USAGE_TPL: &str = r#"arr — Sonarr/Radarr/Prowlarr/SABnzbd CLI

  arr <service> <command> [args]
  arr <command> <title> [args]    service optional — see below
  service: sonarr | sonarr-anime | radarr | prowlarr | sab | jellyfin | seerr | bazarr
  (sonarr-anime = the dedicated anime Sonarr on :8990; all sonarr commands work
   against it, e.g. `arr sonarr-anime status`, `arr sonarr-anime seasons <show>`)

THE SERVICE PREFIX IS OPTIONAL. `arr status 'Lycoris Recoil'` resolves the
title across radarr + sonarr + sonarr-anime and runs against whichever holds
it, printing the service it picked. Use this by default: guessing the wrong
instance returns "no match", which reads exactly like "not in the library" —
the usual reason a lookup turns into a six-command hunt. Anime lives on
sonarr-anime, so a bare `arr sonarr status <anime>` is the classic false miss.
  works for: status get seasons releases grab monitor episodes history files
             audit availability info tag coverage tracks
  put the TITLE FIRST, flags after it. A numeric id still needs its service
  (ids are only unique within one instance). Ambiguous titles list candidates.

  arr where <title>               WHERE IS THIS? one call, whole pipeline:
        which service holds it, what's on disk (per-season for shows), any
        search still running, what's in the arr + SAB queues, the Seerr request
        state, whether Jellyfin can see it, and the command that moves it
        forward. Start here for "is it downloading?" / "why isn't X there?"
        instead of checking each service.

Commands (sonarr & radarr unless noted):
  status [query]                  list items (optionally filter by title)
        a single-match sonarr status also prints per-season coverage flags,
        so "do we have X?" surfaces partially-downloaded seasons by itself
  add <title> [--year Y] [--tvdb/--tmdb ID] [--seasons all|s1,s2|none]
        [--quality NAME] [--root PATH] [--no-search] [--requester <discordId>]
        [--require-subs L] [--require-audio L] [--no-wait] [--dry-run]
        add by title. REFUSES ambiguous matches (lists candidates — narrow by
        --year/--tvdb/--tmdb; the wrong-Gloria guard). If already in the
        library, prints its per-season coverage instead of adding. Defaults:
        monitor all regular seasons, library-majority quality profile, search
        immediately, then wait up to 60s and report what got grabbed — one
        command answers "is it downloading?" (--no-wait skips the wait).
        Fresh grabs are promoted to the front of the download queue (an
        interactive add means someone is waiting; backlog churn can't starve it).
        --requester wires up the download-notifier DM; --require-* makes the
        notifier's ready DM verify subs/dub languages via ffprobe.
  coverage <id|query> [--fix] [--tracks] [--lang LANG] [--fix-subs] [--dry-run]  (sonarr)
  coverage --all [--fix] [--quiet] [--limit N]
        per-season gaps vs AIRED episodes. --fix searches missing MONITORED
        eps (partial season -> EpisodeSearch, empty -> SeasonSearch);
        unmonitored seasons are only reported — ask the requester first.
        --tracks adds per-season DUB/SUB coverage for --lang (default eng) via
        ffprobe; --fix-subs also asks Bazarr to fetch missing subs (main sonarr
        only — the anime instance isn't Bazarr-covered).
        --all sweeps every monitored series (cron this; --quiet = silent when clean)
  tracks <id|query> [--season N] [--missing-audio LANG] [--missing-subs LANG] [--json]
        audio/sub tracks per file via ffprobe + sidecar .srt detection.
        `tracks NANA --missing-audio eng` = which eps lack an English dub;
        '*' marks the default track (catches wrong-default-audio bugs)
  watch <id|query> [more targets...] [--season N] [--until on-disk|in-jellyfin]
        [--verify-audio L] [--verify-subs L] [--max-age H] [--quiet] [--once]
        one-shot readiness check for cron watchdogs — silent while a download
        progresses (--quiet). exit: 0 ready, 1 pending, 2 verify-fail, 3 stuck,
        4 stalled (multi-target: worst code). --once = announce READY a single
        time, alert STUCK/STALLED/VERIFY-FAIL once (re-arm on recovery), retry
        transient API errors instead of false-alerting; state in
        ~/.local/state/arr-watch.json. Replaces per-title watch-*.sh scripts
        AND their stamp files — put the bare command in the cron, no wrapper.
  replace <old id|query> <correct title...> [--tmdb ID] [--year Y] [--yes]  (radarr)
        delete wrong movie+file, add the right one, carry requester tag, search.
        dry-run unless --yes (the German-Les-Visiteurs fix)
  get <id|query>                  full JSON for one item
  seasons <id|query>              (sonarr) per-season monitored + on-disk
  releases <id|query> [--season N|--episode EPID] [--audio eng|dual] [--timeout S]
        (--audio: dub heuristic on release names — Dual Audio/English Dub/Multi;
         --timeout: raise past the 300s default — anime searches can exceed it)
  grab <id|query> [--season N|--episode EPID] [--override|--via-sab] [--dry-run]
        [--wait] [--no-wait] [--monitor]
        no flags: search & let the arr decide (respects quality profile), then
        wait up to 60s and report what got grabbed (--no-wait skips; --timeout
        adjusts). The wait narrates the search command (id + live progress) and
        a zero-grab timeout ends with a diagnosis — search still running vs
        finished empty, indexer backoff state — plus searches/cancel follow-ups
        (add's wait works the same).
        Fresh grabs — and anything already downloading for this item —
        are promoted to the front of the queue, so this doubles as "I want this
        NOW" on something already in flight.
        Converges on the two states where a bare search achieves nothing:
        NOT IN THE LIBRARY -> hands off to `add` (same intent: monitor, search,
        wait, promote), so you don't have to know which verb applies; and
        UNMONITORED -> the arrs' *Search commands skip unmonitored items, so a
        search "succeeds" and grabs nothing. grab reports that and stops;
        --monitor turns monitoring on first (ADDITIVE — unlike `monitor <id> s2`,
        which unmonitors every other season) and then searches.
        --override: force-push every candidate release (bypass rejections).
        --via-sab: send candidate NZBs straight to SAB (bypass search cache).
        --wait: also block until the search command itself finishes
        (--timeout SECS, def 300)
        --requester <discordId>: stamp a requester:<id> tag so the download-notifier
              DMs that person a live progress bar (use when grabbing for someone)
  tag <id|query> [--requester <discordId>] [--require-subs LANG]
        [--require-audio LANG] [--remove]   (sonarr/radarr)
        requester-*: download-notifier DMs that person a live progress bar.
        require-*: the notifier's ✅ ready DM VERIFIES the language via ffprobe
        ("🔎 eng subs ✓" / "⚠ 1/3") — the fix for "with English subs" asks;
        no watcher cron needed. add accepts the same --require-* flags.
  episodes <id|query> [--season N] [--missing] [--monitored] [--json]   (sonarr)
        list episodes WITH ids (for grab/import by --episode). --missing = no file
  wait <commandId> [--timeout SECS]   block until a search/grab/import/refresh ends
  searches [id|query]             active/recent search commands: id, age, live
        progress message, ⚠ when one has been grinding >15m. THE follow-up
        when grab/add reports nothing landed — is the search alive, and where?
  cancel <commandId...>           cancel QUEUED commands. A STARTED command
        runs to completion (the arrs 409 on cancelling it) — but searches run
        concurrently, so fire a narrower one alongside a grinder instead:
        `grab <id> --season N`
  monitor <id|query> <spec>       sonarr: all|none|s1,s2,...   radarr: on|off
  info <id|query>                 concise identity (title/year/ids/origTitle/altTitles/state)
  lookup <term>                   TMDB/TVDB metadata search (disambiguate 1990 vs 2003)
  availability <id|query>         is it obtainable? searches the item's own titles + ids
  files <id|query> [--full]       files on disk (path/size/quality)
  audit <id|query> [--quarantine] [--yes] [--dest DIR] [--json]
  audit --all [--quiet]
        disk-vs-DB audit: video files the arr does NOT track (the cause of
        duplicate episodes / "wrong first episode" in Jellyfin). --quarantine
        moves them + sidecars to /data/hermes/quarantine (dry-run unless
        --yes; restore = move back), then rescans arr + Jellyfin.
        status/coverage/add warn about these automatically for single items;
        --all sweeps the whole library (exit 1 if offenders found — cron it).
  delete <id|query> ...           DELETE via API (dry-run unless --yes):
        takes MANY titles/ids at once and also cancels each item's active
        downloads (removeFromClient), so "delete X Y Z" fully stops wanting them.
        --keep-downloads leaves the queue alone; --blocklist blocklists the
        cancelled releases.
        sonarr: --seasons X-Y [--keep-monitored] (surgical) | --all (whole series)
        radarr: --file-only (drop file, keep movie) | (default: movie + file)

Top-level (no service prefix):
  where <title>                   the whole pipeline for one title (see top)
  queue [--json]                  whole-picture rollup across SAB + every arr
        queue: total jobs, TB left, ETA, per-category/state/quality counts,
        free space vs. queued, and a raw-disc warning. (Per-service `arr <svc>
        queue` is the filterable item listing.)
  delete <title|id> ...           removes each item whether it's a movie or a
        show and cancels its downloads; mixed lists fine. [--yes]
        [--keep-downloads] [--blocklist]

  parse "<release title>"         show how the arr maps a title (series+episode)
  import <folder> --series <id|query> [--match SUBSTR] [--season N]
        [--map auto|abs|se] [--mode copy|move] [--batch N] [--dry-run] [--wait]  (sonarr)
        force-import files into episodes, bypassing name matching (maps by
        absolute/SxxExx number). --match filters filenames (USE IT in a shared
        download folder, or every file gets mapped onto this series).
        Chunks --batch files per API call (default 10) so big imports can't time out.
  import <folder> --movie <id|query> [--match SUBSTR] [--mode copy|move]
        [--dry-run] [--wait]   (radarr) force-import file(s) into a movie
  queue [query] [--disc|--encrypted|--quality NAME|--status STATE] [--ids]   list queue items
        filter by title/quality/state; --ids prints just ids to pipe into queue-rm
  queue-rm <id…|pat> [--disc|--encrypted|--quality NAME|--status STATE]
        [--blocklist] [--keep-files] [--research] [--yes]   remove queue record(s)
        via API (dry-run unless --yes); default removes from the download client.
        --research re-searches the affected movies/shows. A raw-disc sweep:
        `arr radarr queue-rm --disc --blocklist --research --yes`
  stuck [query] [--json|--quiet] [--fix [--yes]]   queue items needing intervention
        --fix: auto-plan force-imports for import-blocked items (applies w/ --yes);
        failed items get a suggested queue-rm+re-grab line
  history [id|query] | wanted
  search <query> [--group X] [--indexer Y] [--limit N] [--audio eng|dual] [--json]  (prowlarr)
  grab <query> [--indexer X] [--group/--match Y] [--cat C] [--all] [--dry-run]
        (prowlarr) grab a matched release to the right client by protocol:
        usenet -> SABnzbd, torrent -> qBittorrent. Refuses >1 match w/o --all.
  raw <METHOD> <path> [json]      arbitrary API call

SABnzbd:
  arr sab queue [pat] | status | add <url> [--cat C] | prio <pat> [--top]
      | history [pat] | cleanup <pat> [--yes]   (cleanup deletes via SAB, files too)

Jellyfin / Seerr:
  arr jellyfin unwatched [--min-seasons N]   series on disk no user has watched
  arr jellyfin has <title>                   is it visible in the library NOW?
  arr jellyfin refresh [--wait <title>] [--timeout S]   trigger scan (+poll until visible)
  arr seerr requests [--pending] [query]     who requested what (+ media status)
  arr seerr request <id>                     full request detail
  arr seerr unfulfilled [--fix] [--json] [--quiet]   requests vs ACTUAL disk state
        (cross-checked against the arrs; flags Seerr-says-partial-but-complete
        divergences; --fix triggers arr-side searches for real gaps)

Bazarr (subtitle manager — covers MAIN sonarr + radarr; NOT sonarr-anime):
  arr bazarr status                     provider health/throttles + wanted backlog
  arr bazarr wanted [pat] [--tv|--movies]   items Bazarr still owes subs for
  arr bazarr search --series <q> | --movie <q>   trigger its subtitle search
  arr bazarr raw <METHOD> <path> [urlencoded-params]
  Prefer Bazarr for subtitles (it holds the OpenSubtitles membership) over
  hand-rolled subliminal runs.

Keys come from $ARR_API_KEY_<SVC> / $ARR_API_KEY_{SAB,JELLYFIN,SEERR} or the
sops-rendered env file (%s). No sudo required."#;

pub fn usage() -> String {
    USAGE_TPL.replace("%s", &arr_api::env::env_file_path())
}
