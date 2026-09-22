# flighty-wall

Sync Flighty Friends flights from a dedicated Google Calendar to a FlightWall Mini.

> **Status:** Calendar intake, Flighty event parsing, and both fixture sanitizers are done and verified against the live calendar. The FlightWall contract was captured on 2026-09-22 from the owner's Mac (`mise run check`: 103 tests, 94% coverage). The wall client is next; nothing writes to the wall yet.

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
| FlightWall client | next |
| Reconciliation, systemd daemon | not started |

The reviewed plan, the per-unit record of what landed, and the remaining work are in
`docs/plans/2026-09-21-001-feat-flighty-flightwall-sync-plan.md`. The capture protocol and the
capability gate that decides whether the wall integration can proceed are in
`docs/flightwall-api-discovery.md`.
