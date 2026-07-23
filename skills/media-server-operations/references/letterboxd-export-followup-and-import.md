# Letterboxd Export Follow-up and Import Workflow

Use this when Andre asks whether a trusted user has provided a Letterboxd export, asks to send them the same instructions another person received, or asks to import the export into the Jellyfin Letterboxd plugin.

## Check before re-sending instructions

1. Check persistent/user memory for the person's Letterboxd username/profile.
2. Search past sessions for whether they already supplied a username or export.
3. Check Hermes' document cache for an export zip before asking again:

```sh
find /var/lib/hermes/.hermes/cache/documents -maxdepth 1 -type f \
  -iname '*letterboxd*.zip' -printf '%TY-%Tm-%Td %TH:%TM %p\n' | sort
find /var/lib/hermes/.hermes/cache/documents -maxdepth 1 -type f \
  \( -iname '*alex*' -o -iname '*alexandria*' -o -iname '*alexandriasage*' \) \
  -printf '%TY-%Tm-%Td %TH:%TM %p\n' | sort
```

Adapt the second command for the target person's known username/handle.

If no export is present, send the export instructions; if an export is present, import first and only message if needed.

## DM instructions template

Use the `discord-dm <discord_user_id> '<message>'` helper for allowlisted Discord users when a direct message is requested.

Important shell pitfall: do **not** put Markdown backticks inside a double-quoted shell argument (`"... `.zip` ..."`), because the shell treats backticks as command substitution and can mangle the message. Either use single quotes around the whole message, avoid backticks, or write the message to a temporary file and pass it safely.

Template:

```text
Hey <Name> — Andre asked me to send you the Letterboxd export instructions so I can import your history into Jellyfin.

1. Go to: https://letterboxd.com/settings/data/
2. Log in if needed.
3. Open the Import & Export / Data settings page.
4. Click Export your data.
5. Letterboxd should generate/download a .zip file.
6. Send/upload that .zip to me here, and I’ll import it into the Jellyfin Letterboxd plugin for you.

The export is better than scraping because it should include your full history, not just the recent RSS feed.
```

If a quoting mistake sent a malformed DM, immediately send a short correction that clarifies the `.zip` step; do not pretend the original message was perfect.

## Import command pattern

The importer lives in the Jellyfin Letterboxd plugin repo:

```sh
cd /data/hermes/hermes-plugins/jellyfin-letterboxd
python scripts/import_letterboxd_export.py <export.zip> \
  --user 'DisplayName:letterboxdUsername:jellyfinUserName:discordUserId' \
  --merge-cache /data/.nixflix/jellyfin/plugins/Jellyfin.Plugin.Letterboxd/letterboxd-cache.json
```

If the system Python lacks `requests`/`beautifulsoup4`, run the importer in the same Nix/Python environment used for the plugin backfill tooling rather than rewriting the workflow.

After import:

1. Trigger plugin sync: `POST /Letterboxd/Sync` with the local Jellyfin API key.
2. Verify `/Letterboxd/Users` shows the expected user and count.
3. Verify `/Letterboxd/Film/<tmdbId>` or `/Letterboxd/Search?query=<known title>` returns at least one imported entry.
4. Report clearly whether the live cache was updated and verified, or whether the task is blocked waiting on the user to upload the zip.

## Reporting pattern

Be explicit about the state:

- “No export uploaded yet; instructions sent; import is blocked until the zip arrives.”
- “Export found at `<path>`; imported N entries, M with TMDb IDs; sync preserved the import; verified via API.”

Do not claim import completion just because instructions were sent.