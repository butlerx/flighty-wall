# Capturing the FlightWall contract

How the contract in `docs/flightwall-api.md` was observed, and how to re-observe it when the
client reports `flightwall_contract_drift` or the vendor ships a new app.

Everything here targets the owner's own device, account, and wall. Nothing else.

---

## 1. Where to capture from

**The Mac.** `TheFlightWall.app` is the iOS build and runs natively on Apple Silicon
(`/Applications/TheFlightWall.app`, `Wrapper/TheFlightWall.app/Info.plist` reports
`LSRequiresIPhoneOS`). It is already signed in to the wall. `NSAllowsArbitraryLoads = true` and
there is no pinning code in `main.jsbundle`, so an HTTPS inspection proxy with a locally trusted
CA sees everything. The 2026-09-22 capture had zero TLS errors.

The app is not AppleScript-scriptable, but with Accessibility granted to the terminal it can be
driven through System Events: enumerate `entire contents of window 1`, match `AXButton` by
`description`, `perform action "AXPress"`. Typing into the Add field needs a coordinate click on
the `AXTextField`, then `keystroke`, then Return.

**Fallback: Android.** Install `com.axisnimble.theflightwall` from Google Play, replug the wall to
show the setup QR code, scan it (the vendor FAQ confirms several devices can control one wall),
proxy the phone, install the CA in the user trust store. Not needed so far.

---

## 2. Safety rules

- Capture only your own device, account, and wall.
- The proxy binds to `127.0.0.1`. Never expose it.
- Raw flows go to `captures/` (gitignored). Delete them once fixtures are produced.
- Remove the interception CA when finished. A CA left trusted is a standing vulnerability.
- No capture CA and no disabled TLS verification may reach any non-capture code path. The
  client uses normal certificate validation and refuses redirects.
- The captured `x-api-key` is a live credential. It survives sign-out. Treat the flow file and
  the credential file it produces like a password.

---

## 3. Procedure

```bash
mise install                       # pulls mitmproxy alongside the other pinned tools
mise run capture:start             # CA into login keychain, system proxy on, mitmdump headless
```

`capture:start` prompts for `sudo` once (macOS requires admin rights to change the system
proxy). It runs `mitmdump` in the foreground with flows streaming to `captures/flightwall.flow`;
**Ctrl-C writes `captures/flightwall.har`**. Pass `--web` for the mitmweb UI on
http://127.0.0.1:8081 instead.

Then, in the app:

1. Quit and relaunch `TheFlightWall.app` so it picks up the proxy. That is the cold-start
   sequence. If it shows a connection error, the app is now pinning: Ctrl-C, `capture:stop`, and
   go to §5.
2. Open the wall → the "Area Tracking" button is a tab switcher → **Flight Tracking**. This
   screen loads `GET /configuration`.
3. Add a flight, **Save**. Add a second, Save. Remove one, Save. Each Save is one
   `POST /configuration`. Use flights you are willing to see on the wall briefly.

Then:

```bash
# Ctrl-C the proxy
mise run capture:stop                                   # proxy off, CA removed (sudo prompt)
mise run capture:sanitize -- --host api.theflightwall.com
mise run capture:credentials -- flightwall-credentials.toml   # the daemon's key pair, 0600
mise run capture:stop --purge                           # delete the raw flow and HAR
```

Run `capture:sanitize` once with no `--host` to list every host the capture touched. Rename
the numbered fixtures to the operation they prove, read each one by hand, and scan them against
the raw HAR for the two API keys, the user id, `radius_request.id`, and the coordinates before
committing. Update the fixture table in `docs/flightwall-api.md` §11 with the capture date and
app version.

If the proxy exits before Ctrl-C, convert the flow file by hand:

```bash
mise exec -- mitmdump -nr captures/flightwall.flow --set hardump=captures/flightwall.har -q
```

Sign out of the app and back in afterwards. This does **not** rotate the key pair (it is
per-install), but it does end any session the capture might have seen.

---

## 4. Probing without the app

Once the key pair is in a `0600` TOML file, the contract can be exercised from the shell with
plain HTTP — this is how §9 of the API document was filled in after the app capture. Keep to
reads unless you are willing to change the wall, and restore `tracked_flights` to what it was
when you are done. The pattern:

```python
import json, os, tomllib, urllib.request
creds = tomllib.load(open("flightwall-credentials.toml", "rb"))
headers = {
    "x-api-key": creds["api_key"], "x-user-id": creds["user_id"],
    "accept": "application/json",
    "user-agent": "TheFlightWall/1 CFNetwork/3860.700.1 Darwin/25.6.0",   # required, §1 of the API doc
}
req = urllib.request.Request("https://api.theflightwall.com/configuration", headers=headers)
document = json.load(urllib.request.urlopen(req))
```

Print only shapes and flight numbers. The document carries the owner's home coordinates.

---

## 5. If the app starts pinning

Do not patch the app to weaken TLS. In order:

1. Inspect the owned bundle for hostnames and path templates (`strings -n 8 main.jsbundle`).
   This recovers §1 and the path list in §10 of the API document, but not shapes or errors.
2. Try the Android fallback in §1; pinning is per-platform.
3. Ask the vendor for API access. The FAQ offers none, but a direct request costs nothing.
4. If none of that works, the daemon can only keep running against the contract it already
   knows, and the fingerprint check will stop it the day that contract changes.

---

## 6. What the 2026-09-22 capture taught

Worth knowing before the next one:

- The first sanitizer pass leaked the POST body's `userId` — a 29-character opaque string that
  no length- or charset-based pattern catches. Only a key-name rule does. Scan every fixture
  against the raw source before committing; the sanitizer is the second line of defence.
- The app stages edits locally and only POSTs on **Save**. Adding a flight without saving
  produces no traffic.
- The app polls `/plus/sync` every ~30 s. Filter it out or it dominates the flow file.
- Landed flights disappear from `tracked_flights` on their own. If the test flight you added
  departs mid-capture, the server will remove it for you.
