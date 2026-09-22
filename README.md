# flighty-wall

Sync Flighty Friends flights from a dedicated Google Calendar to a FlightWall Mini.

> **Status:** Feature-complete. Calendar intake, Flighty parsing, the FlightWall client, reconciliation, and the daemon are implemented and tested against captured real data (`mise run check`: 179 tests, 94% coverage). Not yet run against the live wall from Linux — steps 7–9 below are the first deployment.

## Requirements

- Python 3.11 or newer
- [mise](https://mise.jdx.dev/) for development; it installs the pinned tools
- Flighty with Calendar Export
- A dedicated Google Calendar
- A Google Cloud project with the Calendar API enabled
- A Linux host for the finished daemon

## 1. Create the Flighty Friends calendar

1. Open [Google Calendar](https://calendar.google.com/) in a desktop browser.
2. Next to **Other calendars**, select **+ → Create new calendar**.
3. Name it `Flighty Friends`, create it, then open its **Settings and sharing** page.
4. Under **Integrate calendar**, copy the **Calendar ID**. This becomes `google.calendar_id` in `config.toml`.

Keep this calendar private. Do not add public sharing.

## 2. Export Flighty Friends flights

1. Make the new Google calendar available in Apple Calendar on the iPhone running Flighty.
2. Open Flighty's current **Calendar Export** screen and select the `Flighty Friends` calendar.
3. Enable export for Flighty Friends' flights.
4. Confirm that at least one Friend flight appears in the dedicated Google calendar.

Flighty's help page confirms that Friends' flights can be exported with the Friend's name and standard flight information. Menu labels can vary by app version: [Flighty Calendar Export](https://flighty.com/help/calendar-export).

## 3. Create the service-account key

1. Open [Google Cloud Console](https://console.cloud.google.com/), create or select a project, and [enable the Google Calendar API](https://console.cloud.google.com/flows/enableapi?apiid=calendar-json.googleapis.com).
2. Open **IAM & Admin → Service Accounts**, create a service account, then open its **Keys** tab.
3. Select **Add key → Create new key → JSON** and save the downloaded file somewhere outside this repository.
4. Restrict the key file:

   ```bash
   chmod 600 /path/to/google-service-account.json
   ```

Do not paste the JSON key into chat, commit it, or place it in a shared folder.

## 4. Share only the dedicated calendar

1. Read the service account's `client_email` from the downloaded JSON file.
2. Return to the `Flighty Friends` calendar's **Settings and sharing** page.
3. Under **Share with specific people or groups**, add that email with **See all event details** access.
4. Do not grant access to any other calendar and do not enable domain-wide delegation.

Google documents this access model in [Share calendars](https://developers.google.com/workspace/calendar/api/concepts/sharing) and [Service accounts](https://developers.google.com/identity/protocols/oauth2/service-account).

## 5. Configure and inspect

1. Install the tools and the project:

   ```bash
   mise install    # uv, prek, tombi, zizmor
   mise run sync   # .venv with every dependency group
   ```

2. Create local configuration:

   ```bash
   cp config.example.toml config.toml
   ```

3. In `config.toml`, replace:
   - `google.calendar_id` with the Calendar ID from step 1.
   - `google.credentials_path` with the downloaded JSON key's absolute path.

4. Capture a sanitized fixture, repeating `--redact-term` for every Friend name that could appear:

   ```bash
   mise run fixture:calendar -- --redact-term "Friend Name"
   ```

   This runs:

   ```bash
   uv run flighty-wall inspect-calendar \
     --config config.toml \
     --output tests/fixtures/google_calendar/friend-flight.json \
     --lookahead-days 60 --lookback-days 3 \
     --redact-term "Friend Name"
   ```

   `--lookahead-days` and `--lookback-days` widen the read window for this command only, up to 365 days each, so a capture can reach flights outside the daemon's `service.lookahead_days`. Pass them after `--` to override the task's defaults. If the command reports `wrote 0 sanitized event(s)`, the read succeeded and the window simply held no flights — widen it rather than assuming a setup problem.

5. Open the generated fixture and verify it contains no names, emails, booking codes, seat numbers, private URLs, Flighty deeplinks, event UUIDs, or raw Google event IDs before committing it.

The fixture is written with mode `0600`. `config.toml`, credential files, runtime databases, and raw captures are excluded by `.gitignore`.

## 6. Capture the FlightWall contract

**Done 2026-09-22.** The FlightWall backend has no public API, so its contract was observed
from `TheFlightWall.app` running on the owner's Mac, against the owner's own account and
wall, then probed from the shell. The findings, the four committed fixtures, and the closed
capability gate are in `docs/flightwall-api-discovery.md`.

To re-run or extend the capture:

```bash
mise run capture:start                 # trusts a local CA, sets the system proxy, runs mitmdump
# quit and relaunch TheFlightWall.app, do the sequences, Ctrl-C
mise run capture:stop                  # proxy off, CA removed
mise run capture:sanitize -- --host api.theflightwall.com
mise run capture:stop --purge          # delete the raw flow and HAR once fixtures are reviewed
```

Both `capture:start` and `capture:stop` prompt for `sudo` (macOS requires admin rights to
change the system proxy). The sanitizer drops every header and query value, discards body
fields whose keys are never safe, and scrubs tokens, coordinates, and identifiers by pattern
— but it cannot recognise a person's name, so pass one `--redact-term` per Friend name if
any could appear. Read every generated file before committing.

## 7. Write the FlightWall credential file

The daemon reuses the per-install key pair the FlightWall app already has. It does not
expire and survives sign-out (discovery document §4.2). Copy it once from the capture with
the extraction task, which reads `captures/flightwall.flow` and writes a `0600` file without
printing either value:

```bash
mise run capture:credentials -- flightwall-credentials.toml
```

Then in `config.toml`, add:

```toml
[flightwall]
credentials_path = "/path/to/flightwall-credentials.toml"
```

Confirm it works without writing anything:

```bash
uv run flighty-wall probe-wall --config config.toml
```

You should see `model: mini-v1` and your current tracked flights. Now
`mise run capture:stop --purge` to delete the raw capture. Never commit the credential file.

## 8. First dry run, then first apply

`service.dry_run = true` is the default. A dry run reads both sources, plans, and prints
what it *would* write:

```bash
uv run flighty-wall sync --config config.toml
```

Read the `status=` line. `dry_run` with `add=[...]` means a write is planned. `no_change`
means the wall already matches. Anything ending `_not_authoritative` means a source could not
be trusted and nothing would have been written — the `*_reason=` field says why.

When the plan looks right, apply it once:

```bash
uv run flighty-wall sync --config config.toml --apply
```

Then run it again: the second `sync --apply` must report `status=no_change`. That is the
idempotency check. Exit codes: `0` ok, `2` config, `3` a source was not authoritative, `4` the
write was rejected or its outcome unknown, `5` another flighty-wall process holds the lock.

What the daemon will and will not do to your wall:

- It only ever changes `tracked_flights`. Your area, brightness, sleep, and layout settings
  are sent back byte-for-byte as read.
- It only removes flight numbers *it* added, recorded in its own journal. Anything already
  on the wall when it first ran is yours and is never touched, even if a Friend later flies
  the same number.
- It never sends more than five entries. Flights that do not fit are reported as
  `unplaceable`, not forced.
- The server drops landed flights on its own; the daemon notices and forgets them.

## 9. Run it as a service on Linux

```bash
sudo useradd --system --home /var/lib/flighty-wall --shell /usr/sbin/nologin flighty-wall
sudo install -d -m 0700 -o flighty-wall -g flighty-wall /var/lib/flighty-wall /etc/flighty-wall
sudo install -m 0600 -o flighty-wall -g flighty-wall config.toml google-service-account.json \
  flightwall-credentials.toml /etc/flighty-wall/
sudo git clone https://github.com/butlerx/flighty-wall /opt/flighty-wall
sudo -u flighty-wall sh -c 'cd /opt/flighty-wall && uv sync --frozen --no-dev'
sudo install -m 0644 systemd/flighty-wall.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now flighty-wall
journalctl -u flighty-wall -f
```

Set `service.dry_run = false` in `/etc/flighty-wall/config.toml` once the journal shows the
dry-run plans you expect. The unit runs as an unprivileged user with `ProtectSystem=strict`,
no capabilities, and a `@system-service` syscall filter; `systemd-analyze security
flighty-wall` should score well. One summary line is logged per cycle; it names flight
numbers and reasons, never Friends, descriptions, or credentials.

Rotating the Google key: create a new key in the Cloud Console, install it in place of the
old file, `systemctl restart flighty-wall`, delete the old key. Rotating the FlightWall key:
signing out of the app and back in does *not* rotate it (it is per-install); uninstalling and
reinstalling the app does, after which repeat step 7. Back up `/var/lib/flighty-wall` only to
encrypted storage — it holds the ownership journal, which is what protects your manual
entries.

## Development checks

Tools (`uv`, `prek`, `tombi`, `zizmor`) and tasks are defined in `mise.toml` (tools pinned in `mise.lock`); Python is pinned in `.python-version`.

```bash
mise run hooks  # git hooks CI also runs
mise run check  # lint + types + tests + deps, same as CI
```

`mise tasks` lists the individual tasks (`lint`, `lint:fix`, `test`, `deps`). `mise run test -- -k name` and `mise run lint -- ruff-check` pass extra arguments through.

## What is done, what is next

| Step | State |
| --- | --- |
| 1–5 Google calendar, Flighty export, service account, fixture capture | done, verified live |
| 6 FlightWall contract capture | done; capability gate closed |
| FlightWall client, reconciliation, daemon | done, tested against captured fixtures |
| 7–9 credential file, first apply, systemd | **next — the first live run from Linux** |

The reviewed plan, the per-unit record of what landed, and the remaining work are in
`docs/plans/2026-09-21-001-feat-flighty-flightwall-sync-plan.md`. The capture protocol and the
capability gate that decides whether the wall integration can proceed are in
`docs/flightwall-api-discovery.md`.
