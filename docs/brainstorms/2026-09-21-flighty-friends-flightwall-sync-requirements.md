---
date: 2026-09-21
topic: flighty-friends-flightwall-sync
---

# Flighty Friends to FlightWall Sync

## Problem Frame

Flighty already contains current and upcoming flights for the owner's Flighty Friends, while FlightWall Mini can track specific flights only after they are added through its app. Re-entering those flights is repetitive and easy to forget. The goal is a small Linux service that uses Flighty's supported calendar export as the source of truth and keeps the wall focused on active Friends' flights without disrupting manually tracked flights or normal area tracking.

---

## Actors

- A1. Owner: Configures Flighty, Google Calendar, the Linux service, and FlightWall Mini.
- A2. Flighty Friend: Shares flights through Flighty Friends.
- A3. Flighty: Exports Friends' flight events and keeps their details current.
- A4. Linux sync service: Converts calendar events into owned FlightWall tracking entries and display-mode changes.
- A5. FlightWall Mini: Shows nearby aircraft normally and active Friends' flights at the appropriate time.

---

## Key Flows

- F1. Sync an upcoming Friend flight
  - **Trigger:** Flighty creates or updates a Friend's flight in the dedicated Google Calendar.
  - **Actors:** A2, A3, A4, A5
  - **Steps:** The service reads the calendar, identifies the flight and Friend, checks whether it already manages that flight, and adds or updates it on FlightWall.
  - **Outcome:** The flight is ready to appear automatically when it becomes active, without duplicate entries.
  - **Covered by:** R1, R2, R3, R4, R5

- F2. Display an active Friend flight
  - **Trigger:** FlightWall reports that a daemon-managed Friend flight is active.
  - **Actors:** A4, A5
  - **Steps:** The service switches the wall from Area Tracking Mode to Flight Tracking Mode and keeps all active Friend flights available until FlightWall reports that their tracking windows ended.
  - **Outcome:** The wall prioritizes each Friend's flight while FlightWall considers it relevant.
  - **Covered by:** R7, R8, R9, R10

- F3. Return to normal area tracking
  - **Trigger:** FlightWall reports no managed Friend flight active and the daemon still owns the mode transition.
  - **Actors:** A4, A5
  - **Steps:** The service removes only stale entries it previously created and restores Area Tracking Mode unless the owner manually changed the mode.
  - **Outcome:** Nearby aircraft return to the display without overriding manual mode changes or modifying manually added tracked flights.
  - **Covered by:** R7, R8, R9

---

## Requirements

**Calendar source**

- R1. Flighty Friends' flights must be exported into a dedicated Google Calendar that the Linux service can read.
- R2. The service must recognize a calendar event only when it contains enough information to identify a flight unambiguously, including airline/flight number and departure context.
- R3. The service must handle calendar additions, changes, cancellations, and deletions without creating duplicate FlightWall entries.
- R4. The service must process all Friends represented in the dedicated calendar; selecting individual Friends is not required for the first version.

**Safe synchronization**

- R5. Repeated sync runs with unchanged inputs must make no unnecessary changes to FlightWall.
- R6. Upcoming-flight lookahead and polling frequency must be configurable, with conservative defaults suitable for a small always-on Linux box.
- R7. The service must keep durable ownership state so it removes or modifies only FlightWall entries that it created.
- R8. Manually added FlightWall flights and unrelated settings must be preserved.

**Display behavior**

- R9. When FlightWall exposes authoritative active-flight and mode-change state, the service must switch from Area Tracking Mode to Flight Tracking Mode while at least one managed Friend flight is active and return to Area Tracking Mode only when none are active, the daemon initiated the switch, and the owner has not manually overridden it.
- R10. Overlapping active Friends' flights must all remain available to FlightWall rather than one silently replacing another.

**Operations**

- R11. The service must run unattended as a Linux daemon, restart automatically after failure or reboot, and expose useful status and error logs.
- R12. Google and FlightWall credentials must not be committed to source control or printed in logs.
- R13. A dry-run mode must show authoritative proposed actions when both sources are available and clearly label calendar-and-journal-only intent as provisional when FlightWall is unavailable; neither form may modify FlightWall.
- R14. A temporary calendar or FlightWall failure must not erase local ownership state or cause any FlightWall mutation.

---

## Acceptance Examples

- AE1. **Covers R1, R2, R3, R5.** Given a Friend's future flight is exported twice with an updated gate, repeated daemon runs retain one managed FlightWall entry and do not add duplicates.
- AE2. **Covers R7, R8.** Given one manually added flight and one daemon-added flight, when the calendar flight is deleted, only the daemon-added entry is removed.
- AE3. **Covers R9, R10.** Given two Friends' flights overlap and FlightWall exposes authoritative activity and mode state, when the first becomes active the wall enters Flight Tracking Mode, both active flights remain tracked, and the wall returns to Area Tracking Mode only after both are inactive and no manual mode override occurred.
- AE4. **Covers R13, R14.** Given FlightWall is unavailable, a dry run reports locally inferred intent as provisional with remote-dependent actions marked unknown, while a normal run records an error without mutating FlightWall or changing ownership records.

---

## Success Criteria

- A Friend can add or update a flight in Flighty and have it reach FlightWall without manual re-entry.
- The wall shows nearby aircraft between Friends' flights and automatically prioritizes Friends during active tracking windows.
- Manual FlightWall entries survive calendar updates, deletions, daemon restarts, and temporary failures.
- Setup and verification can be completed from documented steps on a Linux host plus a one-time authorized Android capture.

---

## Scope Boundaries

- The first version reads one dedicated Google Calendar.
- The first version handles all Friends exported to that calendar; per-Friend filters and notification rules are deferred.
- The service does not scrape Flighty, access another person's Flighty account, or depend on an undocumented Flighty API.
- The service does not replace FlightWall's aircraft-data providers or firmware.
- A supported FlightWall API is preferred; if none exists, automation is limited to the owner's authenticated app requests and device.
- A dashboard or general-purpose flight-management UI is outside the first version.

---

## Key Decisions

- Google Calendar is the integration boundary because Flighty officially exports Friends' flights there and does not publish a Friends API.
- A dedicated calendar isolates Flighty Friends events and reduces duplicate or unrelated event parsing.
- The daemon preserves manually tracked flights by maintaining explicit ownership of only the entries it creates.
- Area Tracking Mode is the default display; active Friends' flights temporarily take priority.
- Android is available for a one-time authorized network capture because FlightWall's commercial app has no documented public automation API.

---

## Dependencies / Assumptions

- Flighty Calendar Export remains enabled and includes Friends' flights with their names and standard flight information. *Verified live 2026-09-22.*
- The dedicated Google Calendar can be shared read-only with a service account used only by the Linux daemon. *Verified live 2026-09-22.*
- FlightWall's app backend must expose authoritative, complete state plus safe identifiers or conditional-write semantics for add/remove operations and mode changes; implementation stops and returns for a scope decision if those capabilities cannot be proven. *Unverified: waits on the owner's capture.*
- FlightWall remains authoritative for each tracked flight's active window; its published default is approximately 15–30 minutes before takeoff through 30 minutes after landing.
- The FlightWall Mini displays up to five flights at a time (vendor FAQ). Reconciliation must treat being at capacity as a normal condition.

---

## Outstanding Questions

### Resolved during implementation

- [Affects R2, R3] **Stable calendar fields.** `summary` (`"<Friend>: ✈ DUB→BCN • VY 8721"`, with `U+00A0` and `U+200B` normalised), `description`, `start`/`end` with explicit time zones, and `status`. The stable key is `DESIGNATOR:ORIGIN:UTC-departure-date` carrying the set of contributing Google event IDs; codeshares make the cycle non-authoritative because the export carries no codeshare data. Recorded in `tests/fixtures/google_calendar/README.md`.
- [Affects R6] **Defaults.** Poll every 120 seconds, manage the next 7 days; bounded to 30–86 400 seconds and 1–30 days in `config.py`.
- [Affects R11, R12] **Read-only sharing.** The dedicated calendar was shared with the service account's `client_email` as *See all event details* and read live through `calendar.readonly` on 2026-09-22.

### Open — settled only by the FlightWall capture

- [Affects R7, R8, R9] Which authenticated FlightWall app requests list, add, remove, and activate tracked flights and switch display modes? Protocol and capability gate: `docs/flightwall-api-discovery.md`.
- [Affects R9] Are Area Tracking Mode and Flight Tracking Mode mutually exclusive? The vendor's "up to 5 flights at a time" suggests they may share one list. If they coexist, R9 is not applicable and the mode-lease design is dropped.

---

## Status

Planned in `docs/plans/2026-09-21-001-feat-flighty-flightwall-sync-plan.md`. Calendar intake and
parsing (U1–U3) are done and verified against the live calendar. Everything that touches the wall
(U5–U7) waits on the owner running the U4 capture.
