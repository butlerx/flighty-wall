# flighty-wall

Sync Flighty Friends flights from a dedicated Google Calendar to a FlightWall Mini.

> Current status: Google Calendar ingestion and sanitized fixture capture work. FlightWall writes remain disabled until the owner's Android app contract passes the safety checks in the implementation plan.

## Requirements

- Python 3.11 or newer
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

1. Install the project:

   ```bash
   python3 -m venv .venv
   .venv/bin/python -m pip install -e '.[dev]'
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
   .venv/bin/flighty-wall inspect-calendar \
     --config config.toml \
     --output tests/fixtures/google_calendar/friend-flight.json \
     --redact-term "Friend Name"
   ```

5. Open the generated fixture and verify it contains no names, emails, booking codes, seat numbers, private URLs, or raw Google event IDs before committing it.

The fixture is written with mode `0600`. `config.toml`, credential files, runtime databases, and raw captures are excluded by `.gitignore`.

## Development checks

```bash
.venv/bin/ruff check src tests
.venv/bin/ruff format --check src tests
.venv/bin/pyright --project pyrightconfig.json
.venv/bin/mypy src tests
.venv/bin/python -m pytest
```

## Plan

See `docs/plans/2026-09-21-001-feat-flighty-flightwall-sync-plan.md` for the reviewed implementation plan, external-contract gates, and remaining work.
