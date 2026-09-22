"""Turn an authorized HTTPS capture into sanitized, replay-safe FlightWall fixtures.

The FlightWall backend is undocumented, so the contract has to be observed from the
owner's own device before any code talks to it. A raw capture is full of credentials,
device identifiers, and home coordinates, so nothing from it is ever committed
directly: this module keeps the contract shape and throws the secrets away.

Header values are dropped wholesale rather than scrubbed, because an unrecognized
authorization scheme is exactly the case a pattern-based scrubber would miss.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING, cast
from urllib.parse import urlsplit

import orjson

from .redaction import as_mapping, scrub_text, scrub_value

if TYPE_CHECKING:
    from collections.abc import Mapping, Sequence

MAX_BODY_BYTES = 262_144
"""Bodies larger than this are recorded by size only; a fixture needs shape, not bulk."""

SENSITIVE_BODY_KEYS = frozenset(
    {
        # Credentials, which are frequently too short for any pattern to recognise.
        "access_token",
        "api_key",
        "apikey",
        "authorization",
        "client_secret",
        "id_token",
        "password",
        "refresh_token",
        "secret",
        "signature",
        "token",
        # Identifiers that tie a fixture back to the owner's account or hardware.
        "account_id",
        "accountid",
        "device_id",
        "deviceid",
        "device_token",
        "email",
        "phone",
        "push_token",
        "serial",
        "serial_number",
        "session",
        "session_id",
        "user_id",
        "userid",
        # Area-tracking configuration is the owner's home location.
        "lat",
        "latitude",
        "lng",
        "lon",
        "longitude",
    }
)
"""Body keys whose values are discarded by name, because their contents are never safe."""


class CaptureError(ValueError):
    """Raised when a capture file cannot be read as a HAR archive."""


@dataclass(frozen=True, slots=True)
class CaptureEntry:
    """One sanitized request/response pair, safe to commit as a fixture."""

    index: int
    method: str
    host: str
    path: str
    status: int
    payload: dict[str, object]

    @property
    def filename(self) -> str:
        """Return a stable, sortable fixture filename for this entry."""
        slug = "-".join(part for part in self.path.split("/") if part) or "root"
        safe = "".join(char if char.isalnum() or char == "-" else "-" for char in slug).strip("-")
        return f"{self.index:03d}-{self.method.lower()}-{safe.lower()[:60]}.json"


def sanitize_har(
    document: Mapping[str, object],
    *,
    sensitive_terms: Sequence[str] = (),
    hosts: Sequence[str] = (),
) -> tuple[CaptureEntry, ...]:
    """Sanitize every HAR entry, optionally keeping only an explicit host allowlist."""
    allowed = {host.casefold() for host in hosts}
    return tuple(
        entry
        for index, raw_entry in enumerate(_entries(document), start=1)
        if (entry := _entry(index, raw_entry, sensitive_terms)) is not None
        and (not allowed or entry.host.casefold() in allowed)
    )


def observed_hosts(document: Mapping[str, object]) -> tuple[str, ...]:
    """List every host the capture touched, so an allowlist can be chosen deliberately."""
    hosts = {
        urlsplit(url).hostname or "" for raw_entry in _entries(document) if (url := _request_url(raw_entry))
    }
    return tuple(sorted(host for host in hosts if host))


def _entries(document: Mapping[str, object]) -> list[Mapping[str, object]]:
    log = as_mapping(document.get("log"))
    if log is None:
        raise CaptureError("capture is not a HAR archive: missing 'log' object")
    raw_entries = log.get("entries")
    if not isinstance(raw_entries, list):
        raise CaptureError("capture is not a HAR archive: missing 'log.entries' array")
    items = cast("list[object]", raw_entries)
    entries = [as_mapping(item) for item in items]
    if any(entry is None for entry in entries):
        raise CaptureError("capture contains a malformed entry")
    return [entry for entry in entries if entry is not None]


def _request_url(raw_entry: Mapping[str, object]) -> str:
    request = as_mapping(raw_entry.get("request"))
    if request is None:
        return ""
    url = request.get("url")
    return url if isinstance(url, str) else ""


def _entry(
    index: int,
    raw_entry: Mapping[str, object],
    sensitive_terms: Sequence[str],
) -> CaptureEntry | None:
    request = as_mapping(raw_entry.get("request"))
    response = as_mapping(raw_entry.get("response"))
    if request is None or response is None:
        return None

    url = urlsplit(_request_url(raw_entry))
    method = _text(request.get("method"), "GET").upper()
    status = response.get("status")
    # Paths routinely embed an account or device id, so they are scrubbed like a body.
    path = scrub_text(url.path, sensitive_terms)
    payload: dict[str, object] = {
        "request": {
            "method": method,
            "host": url.hostname or "",
            "path": path,
            "query_keys": _query_keys(request),
            "header_names": _header_names(request),
            "body": _body(request, sensitive_terms),
        },
        "response": {
            "status": status if isinstance(status, int) else 0,
            "header_names": _header_names(response),
            "body": _body(response, sensitive_terms),
        },
    }
    return CaptureEntry(
        index=index,
        method=method,
        host=url.hostname or "",
        path=path,
        status=status if isinstance(status, int) else 0,
        payload=payload,
    )


def _header_names(message: Mapping[str, object]) -> list[str]:
    """Return header names only. Values may hold tokens under any scheme."""
    raw_headers = message.get("headers")
    if not isinstance(raw_headers, list):
        return []
    items = cast("list[object]", raw_headers)
    names: set[str] = set()
    for item in items:
        header = as_mapping(item)
        if header is None:
            continue
        name = header.get("name")
        if isinstance(name, str) and name:
            names.add(name.lower())
    return sorted(names)


def _query_keys(request: Mapping[str, object]) -> list[str]:
    """Return query parameter names only. Values routinely carry ids and tokens."""
    raw_query = request.get("queryString")
    if not isinstance(raw_query, list):
        return []
    items = cast("list[object]", raw_query)
    keys: set[str] = set()
    for item in items:
        parameter = as_mapping(item)
        if parameter is None:
            continue
        name = parameter.get("name")
        if isinstance(name, str) and name:
            keys.add(name)
    return sorted(keys)


def _body(message: Mapping[str, object], sensitive_terms: Sequence[str]) -> object:
    container = as_mapping(message.get("postData")) or as_mapping(message.get("content"))
    if container is None:
        return None

    text = container.get("text")
    mime = _text(container.get("mimeType"), "")
    if not isinstance(text, str) or not text:
        return {"omitted": "empty", "mime_type": mime}

    encoded = text.encode("utf-8", "replace")
    if len(encoded) > MAX_BODY_BYTES:
        return {"omitted": "oversized", "mime_type": mime, "bytes": len(encoded)}

    try:
        decoded = orjson.loads(encoded)
    except orjson.JSONDecodeError:
        return {"omitted": "non_json", "mime_type": mime, "bytes": len(encoded)}

    return {
        "mime_type": mime,
        "json": scrub_value(decoded, sensitive_terms, redacted_keys=SENSITIVE_BODY_KEYS),
    }


def _text(value: object, default: str) -> str:
    return value if isinstance(value, str) else default


def manifest(entries: Sequence[CaptureEntry], *, sensitive_terms: Sequence[str] = ()) -> dict[str, object]:
    """Describe the sanitized capture so the discovery document can cite real requests."""
    return {
        "entry_count": len(entries),
        "hosts": sorted({entry.host for entry in entries if entry.host}),
        "entries": [
            {
                "file": entry.filename,
                "method": entry.method,
                "host": entry.host,
                "path": scrub_text(entry.path, sensitive_terms),
                "status": entry.status,
            }
            for entry in entries
        ],
    }
