# Long-running subtitle backfill follow-up

Use this when a missing-English-subtitles request expands from a few named titles into a broad library scan/backfill.

## Lessons

- Do not rely only on an in-session background-process notification for multi-hour subtitle scans. Before telling the user the job is done, verify live process state, latest logs, and report TSV counts.
- If you queue a second pass behind a first one, explicitly report that it is queued/waiting rather than implying it has run.
- Create or use a durable completion notifier/watcher for jobs that may outlive the chat turn. A simple watcher can wait for the relevant PIDs to exit and then summarize the newest report counts.
- Include `.m2ts` in video-extension checks for movie libraries; Blu-ray remux/BDMV-derived items can have `00002.m2ts`-style filenames with matching `00002.en.srt` sidecars.
- Sidecar verification should compare each video stem against language-tagged subtitle stems (`.en.srt`, `.eng.srt`, `.en.ass`, `.eng.ass`, `.en.vtt`, `.eng.vtt`), not just count total subtitles in a folder. Avoid brittle shell glob checks that can falsely mark files missing.
- For user-named titles, resolve the exact library object/path first: localized titles and aliases matter (`Les Fugitifs` may be stored as `The Fugitives`; requested movie wording may point at a similarly named series or sequel).
- When subliminal/provider searches return `Downloaded 0 subtitle` after retries using both localized and original titles, report that title as still unavailable rather than calling the request complete.

## Verification pattern

1. Check live processes for the scripts/providers involved:
   ```sh
   ps -eo pid,ppid,stat,etime,cmd | grep -E 'subtitle|subliminal' | grep -v grep || true
   ```
2. Inspect newest report/log files under the subtitle audit directory:
   ```sh
   find /data/hermes/subtitle-audit -maxdepth 1 -type f -printf '%T@ %p\n' | sort -n | tail
   awk -F'\t' '{c[$1]++} END{for(k in c) print c[k], k}' /data/hermes/subtitle-audit/<report>.tsv | sort -nr
   tail -40 /data/hermes/subtitle-audit/<run>.log
   ```
3. Verify each requested title by exact media path and per-video sidecar presence. Include `.m2ts` along with common container extensions.
4. Trigger a Jellyfin library refresh only after sidecars are present, then say whether Jellyfin may still need time to rescan.

## Communication

If the user asks “how did it go?” after a delayed subtitle job, lead with an apology if a follow-up was promised but not delivered, then give the operational state:

- finished vs still running vs queued;
- counts from the latest report;
- per-requested-title status;
- titles still not fixed and why;
- what notification/watchdog is now in place, if any.
