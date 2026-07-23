# Developing the arr stack

This repo is the source of truth for the media-stack *tooling layer*: the `arr`
CLI (`arr.py`) and the Hermes operating skills (`skills/<name>/SKILL.md`,
symlinked into `~/.hermes/skills/media/` by the nixos `arrRepo` activation).
Always commit **and** push after editing — nothing else version-controls these
files, and a fresh `/data` re-clones from GitHub.

## The one-commit rule

A tool change and the skill prose describing it belong in the same commit.
The skills are the tools' operating manual; every time they drift apart, the
agent faithfully executes stale instructions (it once re-derived a whole
add-verification workflow by hand because the skill predated `add`'s
wait-and-report default). When you change `arr.py` behavior, grep `skills/`
for the old behavior before committing.

## Audience split

- `skills/*/SKILL.md` — operator doctrine for the agent: current behavior,
  current defaults, what to do. No changelog dates, no design rationale beyond
  the one-line whys that guide judgment, no implementation internals.
- This file — maintainer context: why things are shaped this way, where the
  moving parts live, history worth keeping.
- Machine setup / service wiring — `CLAUDE.md` on the box and
  `configuration.nix` in the nixos-config repo (its comments carry the deep
  rationale per subsystem).

Skill prose style (per Andre): calm positive framing, state what works and
why, minimal NEVER/DO-NOT emphasis — the models have good judgment and
respond better to reasons than prohibitions.

## Map of the machinery (where behavior actually lives)

- **`arr` CLI** (`arr.py`, this repo): one-shot `add`/`grab` (wait for first
  grab, report it, promote to download-queue front), `audit` (disk-vs-DB,
  quarantine to `/data/hermes/quarantine`), `watch --once` (cron memory in
  `~/.local/state/arr-watch.json`), `stuck --fix`, requester/require-* tag
  stamping, TRaSH-bypass notices on `--override`/`--via-sab`/prowlarr grabs.
- **download-notifier** (`download-notifier.py`, nixos-config repo): requester
  DM embeds; ffprobe language verification at ready-time (require-* tags) with
  Bazarr kick + recheck loop that live-edits the 🔎 line; failure escalation
  (10 failed attempts, or 12h failed with nothing in flight); queue-wide stuck
  sweep every 5 min (failed / import-blocked incl. unattributed grabs, >15 min
  → one batched wake). State: `/var/lib/download-notifier/state.db`.
- **Hermes gateway webhooks** (`localhost:8644`, HMAC secret in sops
  `langgap_webhook_secret`): `language-gap`, `download-failed`, `queue-stuck` —
  event-driven agent wakes with triage-first prompts. Subscriptions are dynamic
  state in `~/.hermes` (recreate with `hermes webhook subscribe` if wiped);
  `platform_toolsets.webhook = ["all"]` in the hermes settings is what gives
  webhook sessions a terminal.
- **Quality profiles**: recyclarr (nixos-config) syncs TRaSH — inlined
  templates, not `include:`. The anime target carries the Unwanted formats
  (Upscaled/LQ/BR-DISK/…) and `Anime Dual Audio` at +101 explicitly via
  `custom_formats` + `assign_scores_to` (the `custom_format_groups` mechanism
  synced formats but not profile scores there).
- **SAB**: 5.0.4 via overlay (5.0.3 queue deadlock; drop when nixpkgs ≥5.0.4),
  `pre_check` pinned off. Doomed multi-set packs serialize post-processing and
  survive restarts — the kill is data-dir removal + par2 pkill + history
  delete (see the SAB stall playbook memory for the full story).

## History worth remembering

- 2026-07-20/21: audit + proactive warnings; watch --once; webhook wakes;
  cron-PATH fix (`systemd.services.hermes-agent.path`); SAB 5.0.3 deadlock
  root-caused (pre_check + idle-jobs crash). Chat-dumping watchdog crons
  retired in favor of notifier sweeps + webhook wakes.
- 2026-07-22: add/grab wait+promote defaults; anime profile custom formats
  (a fake "[AI UPSCALE]" pack ground postproc for a day — hence the Upscaled
  block and the bypass notices); skills repo-homed and pared to operator
  doctrine.
