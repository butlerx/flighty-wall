# FlightWall contract discovery

**Status: not captured. U5 and U6 are blocked.**

This document is the U4 deliverable. It records what the vendor documents publicly, the
protocol for capturing the rest safely, and the capability gate that U5 and U6 depend on.
Every findings table below is empty because the capture requires the owner's own device
and account. Nothing in this repository may assume a FlightWall endpoint, field, or
identifier format that is not recorded here from an observed request.

The rule for this unit, from the plan: **do not invent endpoints.** An invented contract
produces a client that fails against the physical wall instead of in a test.

---

## 1. What the vendor documents publicly

Read 2026-09-22. These are the only facts available without a capture, and none of them is
sufficient to write a client.

| Fact | Source | Consequence for this project |
| --- | --- | --- |
| Individual flights are tracked by "flight number, callsign, or tail number" | theflightwall.com FAQ | The parser produces a designator (`VY8721`); which of the three forms the wall accepts, and whether it wants a space or zero-padding, is unknown. U3 already found Flighty does not zero-pad (`BA 5`). |
| The wall is controlled by a mobile app through a cloud account: "all connected users can view and control it from any network" | theflightwall.com FAQ | Control is remote and server-mediated, so a daemon on a Linux box can plausibly reach it — but only through whatever the app uses. There is no documented local interface. |
| "Both the Mini and WideScreen display up to 5 flights at a time" | theflightwall.com FAQ | Hard capacity limit of 5. Reconciliation must handle being at capacity as a normal condition, not an error. This is a requirement-critical constraint, already known before capture. |
| No public API, webhook, Home Assistant integration, or developer documentation is offered | theflightwall.com FAQ, searched 2026-09-22 | Capture is the only route. There is nothing to request access to short of contacting the vendor. |
| `AxisNimble/TheFlightWall_OSS` is a DIY ESP32 build with no companion app and no local HTTP API; it tracks an area only, sourcing data from OpenSky and FlightAware AeroAPI, and is configured by editing `UserConfiguration.h` before flashing | github.com/AxisNimble/TheFlightWall_OSS | **This does not substitute for the capture.** It shares a name and a look, not a contract. It cannot be used to infer the commercial Mini's protocol, and it offers no individual-flight or mode API to copy. |

### Questions the public documentation does not answer

These must be settled by the capture. The first one changes the design.

1. **Are area tracking and flight tracking mutually exclusive?** The plan assumes they are,
   and that the daemon must lease flight mode and restore area mode afterwards. But
   "display up to 5 flights at a time" suggests the wall may hold a list that area mode and
   tracked flights share. If they coexist, mode leasing is unnecessary and the design
   simplifies; if they do not, the lease and restore logic in U6 is required.
2. **What happens to a tracked flight after it lands?** Does the wall drop it, hold it, or
   error on a stale flight? This decides whether the daemon must remove its own entries.
3. **How is a manually added flight distinguished from one added over the API?** R7 requires
   never touching a manual entry. If the wall does not distinguish them, the daemon must
   maintain its own ownership records and can only ever remove identifiers it recorded
   adding — which is the design the plan already assumes, but it must be confirmed.
4. **What identifier does the wall return for an added flight, and is it stable?** A
   conditional remove needs a stable ID and ideally a revision or ETag.

---

## 2. Capture protocol

Run this once. It needs about 90 minutes, most of it the controlled sequences in §3.

### Safety rules

These are constraints on the capture itself, not advice:

- Capture only your own device, your own account, and your own wall.
- Use a throwaway or ephemeral device where possible (an owned Android emulator is ideal —
  it can be discarded wholesale afterwards).
- Bind the proxy to `127.0.0.1` or a single LAN address. Never expose it.
- Disable raw flow persistence, or write flows to a path under `captures/` (gitignored) and
  delete them the moment fixtures are produced.
- Rotate anything captured that can be rotated: sign out of the app afterwards, and change
  the account password if the capture included a login.
- Remove the interception CA from the device trust store when finished. A CA left installed
  is a standing vulnerability on that device.
- No capture CA, and no disabled TLS verification, may reach any non-capture code path.

### Setup

1. Start an HTTPS inspection proxy bound to localhost or a single LAN address, with flow
   persistence off or pointed at `captures/`.
2. Point the device or emulator at the proxy and install the proxy's CA in the device trust
   store.
3. Open the FlightWall app and confirm traffic appears. Record the app version and platform
   now, while it is in front of you — it belongs in the provenance table in §6.
4. If the app refuses to connect, it is pinning certificates. Stop and go to §5.

### Teardown

1. Export the HAR, then run the sanitizer (see `tests/fixtures/flightwall/README.md`).
2. Read every produced fixture by hand.
3. Delete the raw HAR and any proxy flow files.
4. Remove the CA from the device trust store; discard the emulator image if you used one.
5. Sign out of the app on the capture device, and rotate the account password if a login was
   captured.

---

## 3. Controlled capture sequences

Record each sequence separately so the fixture for each operation is unambiguous. Use two
real future flights you are willing to put on the wall — Friends' flights a month out are
ideal, because they will not go active mid-capture.

| # | Sequence | What it must prove |
| --- | --- | --- |
| 1 | Cold start: launch, authenticate, connect to the wall | Auth shape, token lifetime, which host serves what, whether device selection is a separate call |
| 2 | Read the full tracked-flight list on an empty wall | The authoritative empty read, and whether the response distinguishes "empty" from "unknown" |
| 3 | Read the current mode | Mode representation, and whether mode and flight list are one resource or two |
| 4 | Add one flight, then read the list | Request field semantics, the identifier returned, and whether the read reflects the write immediately |
| 5 | Add a second flight, then read | That two flights coexist, and the list ordering |
| 6 | Add a flight manually in the app, then read | Whether a manual entry is distinguishable from an API-added one |
| 7 | Remove exactly one flight by identifier, then read | Conditional delete semantics, and that the other flight survives |
| 8 | Remove an identifier that is already gone | The error shape for a lost race, which reconciliation must treat as success |
| 9 | Fill to capacity, then add a sixth | Capacity behaviour: rejection, eviction, or silent drop. Eviction of a manual entry would be a gate failure |
| 10 | Reschedule or replace a tracked flight | Whether the wall keys on the designator or on the instance, which decides whether U3's day-based key matches |
| 11 | Switch to flight tracking mode, then back to area mode | Whether the modes are exclusive, and whether restoring area mode needs its original parameters |
| 12 | Interrupt a mutation mid-flight (kill the proxy during an add) | Recovery: is the operation idempotent, and can the daemon tell whether it applied? |
| 13 | Let a tracked flight go active, then land | Active-status and provenance fields, and post-landing behaviour |
| 14 | Leave the app idle until the token expires, then act | Refresh flow and the error shape for an expired token |

Sequences 9 and 12 are the ones most likely to fail the gate. Do not skip them.

---

## 4. Findings

Empty pending capture. Fill these in from observed requests only.

### 4.1 Hosts and transport

| Host | Purpose | Notes |
| --- | --- | --- |
| _not captured_ | | |

### 4.2 Operations

| Operation | Method | Path shape | Required headers (names) | Request fields | Response fields | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| Authenticate | | | | | | _not captured_ |
| Read flight list | | | | | | _not captured_ |
| Add flight | | | | | | _not captured_ |
| Remove flight | | | | | | _not captured_ |
| Read mode | | | | | | _not captured_ |
| Set mode | | | | | | _not captured_ |

### 4.3 Identity, revisions, and provenance

| Question | Finding |
| --- | --- |
| Identifier for a tracked flight | _not captured_ |
| Is that identifier stable across reads? | _not captured_ |
| Revision, ETag, or version field | _not captured_ |
| Actor or source field (who added this entry) | _not captured_ |
| Active-status field | _not captured_ |
| Mode provenance (who last set the mode) | _not captured_ |

### 4.4 Pagination and completeness

| Question | Finding |
| --- | --- |
| Is the list paginated? | _not captured_ |
| How is the last page signalled? | _not captured_ |
| Can a partial read be distinguished from a complete one? | _not captured_ |

### 4.5 Capacity

| Question | Finding |
| --- | --- |
| Documented cap | 5 flights (vendor FAQ, §1) |
| Observed behaviour when adding beyond the cap | _not captured_ |
| Does the wall ever evict an existing entry? | _not captured_ |

### 4.6 Errors observed

| Condition | Status | Body shape | How the daemon must treat it |
| --- | --- | --- | --- |
| _not captured_ | | | |

### 4.7 Contract fingerprint

A value the client can check at startup to detect that the contract has changed under it —
an API version header, a build identifier, or a schema version.

| Candidate | Where it appears | Finding |
| --- | --- | --- |
| _not captured_ | | |

---

## 5. If the capture is not possible

If the app pins certificates or otherwise refuses to run behind a proxy, do **not** work
around the pinning by patching the app to weaken TLS — that is both fragile and a security
regression on the capture device. Instead, in order of preference:

1. Inspect the owned APK for contract metadata: hostnames, path templates, and API version
   strings in resources or string tables. This can populate §4.1 and part of §4.2 without a
   live capture, but it cannot prove response shapes or error behaviour, so the gate below
   still fails.
2. Ask the vendor for API access or documentation. The FAQ offers none, but a direct request
   costs nothing and would replace this entire unit.
3. Return to the user with a scope decision. The honest options at that point are a
   manual-entry-only workflow, or abandoning the wall integration and keeping U1–U3 as a
   Flighty normalisation tool.

---

## 6. Fixture provenance

One row per committed fixture. A fixture without a row cannot be trusted later, because
there is no way to tell whether the contract has since changed.

| Fixture | Captured | App version | Platform | Firmware | Fields removed beyond sanitizer defaults |
| --- | --- | --- | --- | --- | --- |
| _none committed_ | | | | | |

---

## 7. Capability gate

U5 and U6 may begin only when every row below reads **pass**. A failure stops
implementation and returns to the user for a scope decision; it does not license a weaker
guarantee. In particular, a wall that cannot distinguish a manual entry from a synced one,
or that evicts entries at capacity, breaks R7 — and R7 is the reason this project preserves
manual entries at all.

| # | Capability required | Why it matters | Result |
| --- | --- | --- | --- |
| 1 | A complete, authoritative read of the tracked-flight list, with partial reads detectable | The daemon must never mutate on an incomplete read. Same fail-closed rule as the calendar side | **not captured → blocks U5/U6** |
| 2 | A stable remote identifier per tracked flight | Required to remove exactly what the daemon added and nothing else | **not captured → blocks U5/U6** |
| 3 | At least two simultaneous tracked flights | Friends fly concurrently; one-at-a-time would not meet the requirement | **not captured → blocks U5/U6** |
| 4 | Conditional remove that cannot delete the wrong entry | A manual entry deleted by the daemon is the worst failure this project can cause | **not captured → blocks U5/U6** |
| 5 | Manual entries distinguishable, or safely inferable from owned-ID records | R7 | **not captured → blocks U5/U6** |
| 6 | Recoverable state after an uncertain mutation | A daemon must be able to restart mid-operation without duplicating or orphaning entries | **not captured → blocks U5/U6** |
| 7 | Defined behaviour at the 5-flight capacity, with no eviction of entries the daemon does not own | R7 again, under load | **not captured → blocks U5/U6** |
| 8 | Reschedule semantics that match or can be mapped onto the U3 flight key | U3 keys on `DESIGNATOR:ORIGIN:UTC-date`; if the wall keys differently, the mapping must be explicit | **not captured → blocks U5/U6** |
| 9 | Mode read and set, with area mode restorable to its prior parameters | The wall must return to how the owner had it | **not captured → blocks U5/U6** |
| 10 | No committed or retained artifact contains a usable token, device secret, CA key, personal location, or account identifier | The capture must not leave a trail | pass for the tooling; re-check after the real capture |

Row 10 is the only row that can pass before the capture: the sanitizer and its tests exist,
and `tests/fixtures/flightwall/` is empty rather than populated with guesses. It must be
re-checked by hand once real fixtures are produced.
