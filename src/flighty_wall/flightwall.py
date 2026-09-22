"""FlightWall client for the one contract the capture proved: a whole-document configuration.

Everything here mirrors ``docs/flightwall-api-discovery.md`` §4. There is one resource,
``/configuration``; ``GET`` reads it and ``POST`` replaces it. Tracked flights are a list
inside it, keyed by ``flight_number`` and capped at five by the app, not the server. There
are no per-entry identifiers, no conditional writes, and no display mode, so the client
offers exactly two operations and refuses anything the captured contract did not show.
"""

from __future__ import annotations

import copy
import stat
import tomllib
from dataclasses import dataclass, field
from datetime import UTC, datetime
from enum import StrEnum
from http import HTTPStatus
from typing import TYPE_CHECKING, Protocol, cast

import httpx
import orjson

from .models import SnapshotAuthority
from .redaction import as_mapping

if TYPE_CHECKING:
    from collections.abc import Callable, Mapping, Sequence
    from pathlib import Path

CONFIGURATION_PATH = "/configuration"
MAX_TRACKED_FLIGHTS = 5
"""The app's limit. The server stored ten when asked; what the wall then shows is undefined."""

FINGERPRINT_MODEL = "mini-v1"
DOCUMENT_KEYS = frozenset({"display_config", "request_config", "version"})
DOCUMENT_KEYS_AFTER_WRITE = DOCUMENT_KEYS | {"meta"}
TRACKED_FLIGHT_KEYS = frozenset({"flight_number", "created_at", "show_distance_travelled", "show_metrics"})
"""Any change to these three sets means the contract moved; the client then refuses to write."""

DEFAULT_USER_AGENT = "TheFlightWall/1 CFNetwork/3860.700.1 Darwin/25.6.0"
"""Cloudflare returns 403 error 1010 for anything that does not look like the app."""

JsonObject = dict[str, object]


class TransportError(RuntimeError):
    """The request did not complete; the outcome of a write is unknown."""


class Transport(Protocol):
    """One HTTP round-trip, expressed without any httpx types so tests can script it."""

    def request(
        self,
        method: str,
        path: str,
        *,
        headers: dict[str, str],
        body: object | None,
    ) -> tuple[int, object]:
        """Send one request and return the status code and decoded JSON body."""
        ...


@dataclass(frozen=True, slots=True, repr=False)
class FlightWallCredentials:
    """The per-install key pair the app sends. Only ``api_key`` authorizes anything."""

    api_key: str
    user_id: str

    def __repr__(self) -> str:
        """Never show the key pair, even in tracebacks."""
        return "FlightWallCredentials(<redacted>)"


@dataclass(frozen=True, slots=True)
class TrackedFlight:
    """One entry in ``request_config.tracked_flights``, exactly as the app writes it."""

    flight_number: str
    created_at: str
    show_distance_travelled: bool = True
    show_metrics: bool = True

    @classmethod
    def new(cls, flight_number: str, *, created_at: datetime) -> TrackedFlight:
        """Build an entry the way the app does for a flight added right now."""
        return cls(flight_number=flight_number, created_at=_rfc3339_millis(created_at))

    def as_payload(self) -> JsonObject:
        """Return the JSON object the wall expects for this entry."""
        return {
            "flight_number": self.flight_number,
            "created_at": self.created_at,
            "show_distance_travelled": self.show_distance_travelled,
            "show_metrics": self.show_metrics,
        }


@dataclass(frozen=True, slots=True)
class Fingerprint:
    """The three shape facts that identify the captured contract."""

    model: str | None
    top_level_keys: frozenset[str]
    tracked_flight_keys: frozenset[str]

    def drift(self) -> str | None:
        """Return why this document is not the captured contract, or None if it is."""
        if self.model != FINGERPRINT_MODEL:
            return WallFailure.CONTRACT_DRIFT.with_detail(f"model={self.model!r}")
        if self.top_level_keys not in (DOCUMENT_KEYS, DOCUMENT_KEYS_AFTER_WRITE):
            unexpected = sorted(self.top_level_keys ^ DOCUMENT_KEYS)
            return WallFailure.CONTRACT_DRIFT.with_detail(f"top-level={unexpected}")
        if self.tracked_flight_keys and self.tracked_flight_keys != TRACKED_FLIGHT_KEYS:
            unexpected = sorted(self.tracked_flight_keys ^ TRACKED_FLIGHT_KEYS)
            return WallFailure.CONTRACT_DRIFT.with_detail(f"tracked_flights={unexpected}")
        return None


@dataclass(frozen=True, slots=True, repr=False)
class WallSnapshot:
    """A complete read of the configuration, or an explicit record of why it failed.

    ``document`` is the raw configuration and carries the owner's home coordinates; it stays
    out of ``repr`` and out of logs. It exists only so a write can send it back unchanged.
    """

    authority: SnapshotAuthority
    observed_at: datetime
    tracked_flights: tuple[TrackedFlight, ...] = ()
    fingerprint: Fingerprint = field(default_factory=lambda: Fingerprint(None, frozenset(), frozenset()))
    reason: str | None = None
    document: Mapping[str, object] | None = field(default=None, compare=False)

    @classmethod
    def non_authoritative(cls, observed_at: datetime, reason: str) -> WallSnapshot:
        """Build the failure record for a read that cannot drive any write."""
        return cls(authority=SnapshotAuthority.NON_AUTHORITATIVE, observed_at=observed_at, reason=reason)

    @property
    def flight_numbers(self) -> tuple[str, ...]:
        """Return the tracked flight numbers in wall order."""
        return tuple(flight.flight_number for flight in self.tracked_flights)

    def __repr__(self) -> str:
        """Show the outcome and flight numbers, never the document."""
        return (
            f"WallSnapshot(authority={self.authority.value!r}, observed_at={self.observed_at.isoformat()!r}, "
            f"tracked={list(self.flight_numbers)!r}, reason={self.reason!r})"
        )


class WriteOutcome(StrEnum):
    """What the client knows about a write after one POST."""

    APPLIED = "applied"
    """The server answered 200 and echoed the document."""
    REJECTED = "rejected"
    """The server answered with an error; nothing changed."""
    UNKNOWN = "unknown"
    """The request did not complete. A full body may have applied; compare the re-read."""


class WallFailure(StrEnum):
    """Why a read or write could not be trusted. Rendered as ``<value>:<detail>`` in reasons.

    Kept as strings so ``WallSnapshot.reason`` matches the calendar side and reads well in
    the journal; the enum stops typos and lists the vocabulary in one place.
    """

    CREDENTIALS_REJECTED = "flightwall_credentials_rejected"
    """401. Detail is the server's ``errors[0].code`` (1101 missing, 1102 invalid)."""
    BLOCKED = "flightwall_blocked"
    """403 from Cloudflare. Detail is ``cloudflare_<error_code>``; never retry."""
    FORBIDDEN = "flightwall_forbidden"
    RATE_LIMITED = "flightwall_rate_limited"
    SERVER_ERROR = "flightwall_server_error"
    UNEXPECTED_STATUS = "flightwall_unexpected_status"
    REQUEST_FAILED = "flightwall_request_failed"
    """Transport-level failure. Detail is the exception class; a write may still have applied."""
    RESPONSE_NOT_OBJECT = "flightwall_response_not_object"
    CONTRACT_DRIFT = "flightwall_contract_drift"
    """The document does not match the captured fingerprint. Detail names the field."""

    def with_detail(self, detail: object) -> str:
        """Render as the reason string carried on snapshots and results."""
        return f"{self.value}:{detail}"


@dataclass(frozen=True, slots=True)
class WriteResult:
    """The outcome of one replace, plus the fresh read the caller must reconcile against."""

    outcome: WriteOutcome
    snapshot: WallSnapshot
    reason: str | None = None


class FlightWallClient:
    """Read the configuration; replace its tracked flights. Nothing else."""

    def __init__(
        self,
        transport: Transport,
        credentials: FlightWallCredentials,
        *,
        now: Callable[[], datetime],
        user_agent: str = DEFAULT_USER_AGENT,
    ) -> None:
        self._transport = transport
        self._credentials = credentials
        self._now = now
        self._headers = {
            "accept": "application/json",
            "user-agent": user_agent,
            "x-api-key": credentials.api_key,
            "x-user-id": credentials.user_id,
        }

    def read(self) -> WallSnapshot:
        """Read the configuration. Authoritative only if it parses and matches the fingerprint."""
        observed_at = self._now()
        try:
            status, body = self._transport.request(
                "GET", CONFIGURATION_PATH, headers=self._headers, body=None
            )
        except TransportError as error:
            return WallSnapshot.non_authoritative(observed_at, _request_failed(error))
        failure = _classify_status(status, body)
        if failure is not None:
            return WallSnapshot.non_authoritative(observed_at, failure)
        return _snapshot_from_document(body, observed_at)

    def replace_tracked_flights(
        self,
        snapshot: WallSnapshot,
        flights: Sequence[TrackedFlight],
    ) -> WriteResult:
        """POST the snapshot's document with only ``tracked_flights`` changed, then re-read.

        The document is copied from the snapshot the caller planned against, so the owner's
        display and area settings go back exactly as they were read. Writes are last-writer-
        wins on the server; keeping the read-to-write window to this one call is the only
        mitigation the contract allows.
        """
        if snapshot.authority is not SnapshotAuthority.AUTHORITATIVE or snapshot.document is None:
            raise ValueError("refusing to write against a non-authoritative snapshot")
        if len(flights) > MAX_TRACKED_FLIGHTS:
            raise ValueError(f"the wall tracks at most five flights; refusing to send {len(flights)}")

        document = copy.deepcopy(dict(snapshot.document))
        document.pop("meta", None)
        request_config = dict(as_mapping(document.get("request_config")) or {})
        request_config["tracked_flights"] = [flight.as_payload() for flight in flights]
        document["request_config"] = request_config
        document["userId"] = self._credentials.user_id

        headers = {**self._headers, "content-type": "application/json"}
        try:
            status, body = self._transport.request("POST", CONFIGURATION_PATH, headers=headers, body=document)
        except TransportError as error:
            # A full body that reached the server applies even if the response never arrived.
            return WriteResult(WriteOutcome.UNKNOWN, self.read(), _request_failed(error))
        failure = _classify_status(status, body)
        if failure is not None:
            return WriteResult(WriteOutcome.REJECTED, snapshot, failure)
        return WriteResult(WriteOutcome.APPLIED, self.read())


class HttpxTransport:
    """The production transport: HTTPS, normal certificate validation, no redirects."""

    def __init__(self, host: str, *, timeout_seconds: float) -> None:
        self._client = httpx.Client(
            base_url=f"https://{host}",
            timeout=timeout_seconds,
            follow_redirects=False,
            http2=False,
        )

    def request(
        self,
        method: str,
        path: str,
        *,
        headers: dict[str, str],
        body: object | None,
    ) -> tuple[int, object]:
        """Send one request; any transport-level failure becomes ``TransportError``."""
        content = orjson.dumps(body) if body is not None else None
        try:
            response = self._client.request(method, path, headers=headers, content=content)
        except httpx.HTTPError as error:
            raise TransportError(type(error).__name__) from error
        if response.is_redirect:
            raise TransportError("redirect refused")
        try:
            decoded: object = orjson.loads(response.content) if response.content else {}
        except orjson.JSONDecodeError:
            decoded = {}
        return response.status_code, decoded

    def close(self) -> None:
        """Release the connection pool."""
        self._client.close()


def load_credentials(path: Path) -> FlightWallCredentials:
    """Read the per-install key pair from a private TOML file with ``api_key`` and ``user_id``."""
    try:
        file_stat = path.stat()
    except FileNotFoundError as error:
        raise ValueError(f"flightwall credential file does not exist: {path}") from error
    if not path.is_file():
        raise ValueError(f"flightwall credential path is not a regular file: {path}")
    if stat.S_IMODE(file_stat.st_mode) & 0o077:
        raise ValueError(f"flightwall credential file must be mode 0600: {path}")
    try:
        with path.open("rb") as handle:
            raw = tomllib.load(handle)
    except tomllib.TOMLDecodeError as error:
        raise ValueError(f"flightwall credential file is not valid TOML: {path}") from error
    api_key = raw.get("api_key")
    user_id = raw.get("user_id")
    if not isinstance(api_key, str) or not api_key.strip():
        raise ValueError(f"flightwall credential file is missing api_key: {path}")
    if not isinstance(user_id, str) or not user_id.strip():
        raise ValueError(f"flightwall credential file is missing user_id: {path}")
    return FlightWallCredentials(api_key=api_key.strip(), user_id=user_id.strip())


def _snapshot_from_document(body: object, observed_at: datetime) -> WallSnapshot:
    document = as_mapping(body)
    if document is None:
        return WallSnapshot.non_authoritative(observed_at, WallFailure.RESPONSE_NOT_OBJECT.value)
    display_config = as_mapping(document.get("display_config")) or {}
    request_config = as_mapping(document.get("request_config"))
    if request_config is None:
        return _drift(observed_at, "top-level=['request_config']")
    raw_flights = request_config.get("tracked_flights")
    if not isinstance(raw_flights, list):
        return _drift(observed_at, "tracked_flights=not-a-list")

    flights: list[TrackedFlight] = []
    entry_keys: set[str] = set()
    for raw in cast("list[object]", raw_flights):
        entry = as_mapping(raw)
        if entry is None:
            return _drift(observed_at, "tracked_flights=not-an-object")
        entry_keys |= set(entry.keys())
        parsed = _tracked_flight(entry)
        if parsed is None:
            return _drift(observed_at, "tracked_flights=field-types")
        flights.append(parsed)

    model = display_config.get("model")
    fingerprint = Fingerprint(
        model=model if isinstance(model, str) else None,
        top_level_keys=frozenset(document.keys()),
        tracked_flight_keys=frozenset(entry_keys),
    )
    drift = fingerprint.drift()
    if drift is not None:
        return WallSnapshot.non_authoritative(observed_at, drift)
    return WallSnapshot(
        authority=SnapshotAuthority.AUTHORITATIVE,
        observed_at=observed_at,
        tracked_flights=tuple(flights),
        fingerprint=fingerprint,
        document=document,
    )


def _drift(observed_at: datetime, detail: str) -> WallSnapshot:
    return WallSnapshot.non_authoritative(observed_at, WallFailure.CONTRACT_DRIFT.with_detail(detail))


def _tracked_flight(entry: Mapping[str, object]) -> TrackedFlight | None:
    number = entry.get("flight_number")
    created = entry.get("created_at")
    distance = entry.get("show_distance_travelled")
    metrics = entry.get("show_metrics")
    if not (isinstance(number, str) and isinstance(created, str)):
        return None
    if not (isinstance(distance, bool) and isinstance(metrics, bool)):
        return None
    return TrackedFlight(number, created, distance, metrics)


def _classify_status(status: int, body: object) -> str | None:
    """Turn a non-200 answer into a ``WallFailure`` reason; never includes the key."""
    if status == HTTPStatus.OK:
        return None
    mapping = as_mapping(body) or {}
    if status == HTTPStatus.UNAUTHORIZED:
        return WallFailure.CREDENTIALS_REJECTED.with_detail(_first_error_code(mapping))
    if status == HTTPStatus.FORBIDDEN:
        if mapping.get("cloudflare_error") == True:  # noqa: E712 - JSON true only, not truthy
            return WallFailure.BLOCKED.with_detail(f"cloudflare_{mapping.get('error_code', 'unknown')}")
        return WallFailure.FORBIDDEN.value
    if status == HTTPStatus.TOO_MANY_REQUESTS:
        return WallFailure.RATE_LIMITED.value
    if status >= HTTPStatus.INTERNAL_SERVER_ERROR:
        return WallFailure.SERVER_ERROR.with_detail(status)
    return WallFailure.UNEXPECTED_STATUS.with_detail(status)


def _first_error_code(mapping: Mapping[str, object]) -> str:
    errors = mapping.get("errors")
    if isinstance(errors, list) and errors:
        first = as_mapping(cast("list[object]", errors)[0]) or {}
        code = first.get("code")
        if isinstance(code, int):
            return str(code)
    return "unknown"


def _request_failed(error: Exception) -> str:
    return WallFailure.REQUEST_FAILED.with_detail(type(error).__name__)


def _rfc3339_millis(value: datetime) -> str:
    """Format the way the app does: millisecond precision, trailing ``Z``."""
    return value.astimezone(UTC).strftime("%Y-%m-%dT%H:%M:%S.") + f"{value.microsecond // 1000:03d}Z"
