"""Shared redaction of secrets and personal data before anything reaches disk.

Both the calendar fixture writer and the FlightWall capture sanitizer scrub through
this module so a pattern added for one source protects the other. Every rule here is
deliberately over-broad: losing a little fixture fidelity is cheaper than committing a
token, a home location, or a Friend's name.
"""

from __future__ import annotations

import re
from collections.abc import Mapping, Sequence
from typing import cast

EMAIL = re.compile(r"[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}", re.IGNORECASE)
URL = re.compile(r"[A-Z][A-Z0-9+.-]*://\S+", re.IGNORECASE)
UUID = re.compile(
    r"\b[0-9A-F]{8}-[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{12}\b",
    re.IGNORECASE,
)
BOOKING = re.compile(
    r"(?im)\b(confirmation|reservation|booking)(?:\s+(?:code|number))?\s*[:#-]?\s*[A-Z0-9-]+"
)
SEAT = re.compile(r"(?im)\bseat\s*[:#-]?\s*[A-Z0-9-]+")
JWT = re.compile(r"\beyJ[A-Za-z0-9_-]{6,}\.[A-Za-z0-9_-]{6,}\.[A-Za-z0-9_-]{6,}")
CREDENTIAL = re.compile(
    r"(?i)\b(bearer|basic|token|secret|password|api[_-]?key)\b\s*[:=]?\s*[A-Za-z0-9._~+/=-]{8,}"
)
OPAQUE_TOKEN = re.compile(r"\b[A-Za-z0-9_-]{32,}\b")
COORDINATE = re.compile(r"-?\d{1,3}\.\d{4,}")

_REPLACEMENTS: tuple[tuple[re.Pattern[str], str], ...] = (
    (EMAIL, "<redacted-email>"),
    (URL, "<redacted-url>"),
    (JWT, "<redacted-token>"),
    (UUID, "<redacted-uuid>"),
    (CREDENTIAL, r"\1 <redacted-credential>"),
    (OPAQUE_TOKEN, "<redacted-token>"),
    (BOOKING, r"\1: <redacted>"),
    (SEAT, "Seat: <redacted>"),
    (COORDINATE, "<redacted-coordinate>"),
)


def scrub_text(value: str, sensitive_terms: Sequence[str] = ()) -> str:
    """Replace every secret or personal pattern, then any caller-supplied literals."""
    redacted = value
    for pattern, replacement in _REPLACEMENTS:
        redacted = pattern.sub(replacement, redacted)
    for term in sensitive_terms:
        if term:
            redacted = re.sub(re.escape(term), "<redacted-name>", redacted, flags=re.IGNORECASE)
    return redacted


REDACTED_BY_KEY = "<redacted>"
"""Placeholder that keeps a field visible in a fixture while discarding its contents."""


def scrub_value(
    value: object,
    sensitive_terms: Sequence[str] = (),
    *,
    dropped_keys: frozenset[str] = frozenset(),
    redacted_keys: frozenset[str] = frozenset(),
) -> object:
    """Scrub a decoded JSON value, dropping any key named in `dropped_keys`."""
    if isinstance(value, str):
        return scrub_text(value, sensitive_terms)
    mapping = as_mapping(value)
    if mapping is not None:
        return scrub_mapping(
            mapping,
            sensitive_terms,
            dropped_keys=dropped_keys,
            redacted_keys=redacted_keys,
        )
    if isinstance(value, list):
        items = cast("list[object]", value)
        return [
            scrub_value(item, sensitive_terms, dropped_keys=dropped_keys, redacted_keys=redacted_keys)
            for item in items
        ]
    if isinstance(value, float):
        return _scrub_float(value)
    return value


def scrub_mapping(
    value: Mapping[str, object],
    sensitive_terms: Sequence[str] = (),
    *,
    dropped_keys: frozenset[str] = frozenset(),
    redacted_keys: frozenset[str] = frozenset(),
) -> dict[str, object]:
    """Scrub every value in a mapping, dropping keys that must never be committed.

    A key listed in `redacted_keys` keeps its name but loses its value entirely, including
    any nested structure. Pattern matching cannot recognise a short opaque secret, so for
    those fields the key name is the only reliable signal and shape fidelity is forfeited.
    """
    result: dict[str, object] = {}
    for key, item in value.items():
        name = str(key)
        if name in dropped_keys:
            continue
        if name.casefold() in redacted_keys:
            result[name] = REDACTED_BY_KEY
            continue
        result[name] = scrub_value(
            item,
            sensitive_terms,
            dropped_keys=dropped_keys,
            redacted_keys=redacted_keys,
        )
    return result


def as_mapping(value: object) -> Mapping[str, object] | None:
    """Return the value as a string-keyed mapping, or None if it is not a mapping."""
    if not isinstance(value, Mapping):
        return None
    return cast("Mapping[str, object]", value)


def _scrub_float(value: float) -> object:
    """Blunt a precise coordinate while leaving ordinary numbers intact."""
    text = repr(value)
    if COORDINATE.fullmatch(text):
        return "<redacted-coordinate>"
    return value
