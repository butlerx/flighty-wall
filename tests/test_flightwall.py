"""Contract tests for the FlightWall client, driven by the captured fixtures.

Every request/response shape here comes from ``tests/fixtures/flightwall/``; nothing is
invented. The transport is a fake so no test touches the network.
"""

from __future__ import annotations

import copy
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import TYPE_CHECKING, cast

import orjson
import pytest

from flighty_wall.flightwall import (
    FINGERPRINT_MODEL,
    MAX_TRACKED_FLIGHTS,
    FlightWallClient,
    FlightWallCredentials,
    TrackedFlight,
    TransportError,
    WallSnapshot,
    WriteOutcome,
)
from flighty_wall.models import SnapshotAuthority

if TYPE_CHECKING:
    from collections.abc import Callable

FIXTURES = Path(__file__).parent / "fixtures" / "flightwall"
NOW = datetime(2026, 9, 22, 12, 0, tzinfo=UTC)
CREDENTIALS = FlightWallCredentials(api_key="k" * 43, user_id="fw_ios_" + "u" * 22)

JsonObject = dict[str, object]


def fixture(name: str) -> JsonObject:
    return cast("JsonObject", orjson.loads((FIXTURES / name).read_bytes()))


def fixture_document(name: str, side: str = "response") -> JsonObject:
    body = cast("JsonObject", cast("JsonObject", fixture(name)[side])["body"])
    return copy.deepcopy(cast("JsonObject", body["json"]))


Step = tuple[int, object] | Exception
Call = tuple[str, str, dict[str, str], object]


def _no_steps() -> list[Step]:
    return []


def _no_calls() -> list[Call]:
    return []


@dataclass
class FakeTransport:
    """Scripted transport: each call pops the next (status, body) or raises."""

    responses: list[Step] = field(default_factory=_no_steps)
    calls: list[Call] = field(default_factory=_no_calls)

    def request(
        self,
        method: str,
        path: str,
        *,
        headers: dict[str, str],
        body: object | None,
    ) -> tuple[int, object]:
        self.calls.append((method, path, dict(headers), copy.deepcopy(body)))
        if not self.responses:
            raise AssertionError(f"unexpected {method} {path}")
        step = self.responses.pop(0)
        if isinstance(step, Exception):
            raise step
        return step


def client(transport: FakeTransport) -> FlightWallClient:
    return FlightWallClient(transport, CREDENTIALS, now=lambda: NOW)


def test_read_captured_configuration_is_authoritative() -> None:
    transport = FakeTransport([(200, fixture_document("get-configuration.json"))])

    snapshot = client(transport).read()

    assert snapshot.authority is SnapshotAuthority.AUTHORITATIVE
    assert snapshot.observed_at == NOW
    assert snapshot.fingerprint.model == FINGERPRINT_MODEL
    assert [flight.flight_number for flight in snapshot.tracked_flights] == ["EI61"]
    assert snapshot.tracked_flights[0].show_metrics is True
    method, path, headers, body = transport.calls[0]
    assert (method, path, body) == ("GET", "/configuration", None)
    assert headers["x-api-key"] == CREDENTIALS.api_key
    assert headers["x-user-id"] == CREDENTIALS.user_id
    assert headers["user-agent"].startswith("TheFlightWall/")


def test_read_with_no_tracked_flights_is_authoritative_and_empty() -> None:
    document = fixture_document("get-configuration.json")
    cast("JsonObject", document["request_config"])["tracked_flights"] = []
    transport = FakeTransport([(200, document)])

    snapshot = client(transport).read()

    assert snapshot.authority is SnapshotAuthority.AUTHORITATIVE
    assert snapshot.tracked_flights == ()


def _tracked(document: JsonObject) -> list[JsonObject]:
    return cast("list[JsonObject]", cast("JsonObject", document["request_config"])["tracked_flights"])


def _change_model(document: JsonObject) -> None:
    cast("JsonObject", document["display_config"])["model"] = "mini-v2"


def _drop_request_config(document: JsonObject) -> None:
    del document["request_config"]


def _add_top_level_key(document: JsonObject) -> None:
    document["surprise"] = 1


def _add_entry_key(document: JsonObject) -> None:
    _tracked(document)[0]["id"] = "abc"


def _drop_entry_key(document: JsonObject) -> None:
    del _tracked(document)[0]["created_at"]


@pytest.mark.parametrize(
    ("mutate", "reason_fragment"),
    [
        (_change_model, "model"),
        (_drop_request_config, "top-level"),
        (_add_top_level_key, "top-level"),
        (_add_entry_key, "tracked_flights"),
        (_drop_entry_key, "tracked_flights"),
    ],
)
def test_fingerprint_drift_is_non_authoritative(
    mutate: Callable[[JsonObject], None],
    reason_fragment: str,
) -> None:
    document = fixture_document("get-configuration.json")
    mutate(document)
    transport = FakeTransport([(200, document)])

    snapshot = client(transport).read()

    assert snapshot.authority is SnapshotAuthority.NON_AUTHORITATIVE
    assert snapshot.reason is not None
    assert reason_fragment in snapshot.reason


def test_read_401_is_a_credential_error_that_never_echoes_the_key() -> None:
    transport = FakeTransport(
        [(401, {"success": False, "errors": [{"code": 1102, "message": "Invalid API key"}]})]
    )

    snapshot = client(transport).read()

    assert snapshot.authority is SnapshotAuthority.NON_AUTHORITATIVE
    assert snapshot.reason == "flightwall_credentials_rejected:1102"
    assert CREDENTIALS.api_key not in repr(snapshot)


def test_read_cloudflare_403_is_fatal_not_retryable() -> None:
    transport = FakeTransport([(403, {"cloudflare_error": True, "error_code": 1010, "retryable": False})])

    snapshot = client(transport).read()

    assert snapshot.authority is SnapshotAuthority.NON_AUTHORITATIVE
    assert snapshot.reason == "flightwall_blocked:cloudflare_1010"


@pytest.mark.parametrize(
    ("status", "reason"), [(429, "flightwall_rate_limited"), (503, "flightwall_server_error:503")]
)
def test_read_retryable_statuses_are_named(status: int, reason: str) -> None:
    transport = FakeTransport([(status, {})])

    snapshot = client(transport).read()

    assert snapshot.authority is SnapshotAuthority.NON_AUTHORITATIVE
    assert snapshot.reason == reason


def test_read_transport_failure_is_non_authoritative() -> None:
    transport = FakeTransport([TransportError("connection reset")])

    snapshot = client(transport).read()

    assert snapshot.authority is SnapshotAuthority.NON_AUTHORITATIVE
    assert snapshot.reason == "flightwall_request_failed:TransportError"


def test_replace_posts_the_whole_document_touching_only_tracked_flights() -> None:
    before = fixture_document("get-configuration.json")
    captured_post = fixture_document("post-configuration-add.json", side="request")
    after = fixture_document("post-configuration-add.json")
    transport = FakeTransport([(200, before), (200, after), (200, copy.deepcopy(after))])
    wall = client(transport)
    snapshot = wall.read()

    result = wall.replace_tracked_flights(
        snapshot,
        (
            snapshot.tracked_flights[0],
            TrackedFlight.new("BA5", created_at=NOW),
        ),
    )

    assert result.outcome is WriteOutcome.APPLIED
    assert [f.flight_number for f in result.snapshot.tracked_flights] == ["EI61", "BA5"]
    method, path, _, body = transport.calls[1]
    assert (method, path) == ("POST", "/configuration")
    sent = cast("JsonObject", body)
    # Byte-for-byte the same as the app's own POST, apart from the values only the app knows.
    expected = captured_post
    expected["userId"] = CREDENTIALS.user_id
    sent_flights = cast("list[JsonObject]", cast("JsonObject", sent["request_config"])["tracked_flights"])
    expected_flights = cast(
        "list[JsonObject]", cast("JsonObject", expected["request_config"])["tracked_flights"]
    )
    for sent_flight, expected_flight in zip(sent_flights, expected_flights, strict=True):
        sent_flight["created_at"] = expected_flight["created_at"]
    assert sent == expected
    # And the daemon's re-read after the write is the third call.
    assert transport.calls[2][0:2] == ("GET", "/configuration")


def test_replace_preserves_every_non_tracked_byte_of_the_document() -> None:
    before = fixture_document("get-configuration.json")
    transport = FakeTransport([(200, before), (200, copy.deepcopy(before)), (200, copy.deepcopy(before))])
    wall = client(transport)
    snapshot = wall.read()

    wall.replace_tracked_flights(snapshot, ())

    sent = cast("JsonObject", transport.calls[1][3])
    original = fixture_document("get-configuration.json")
    for key in ("display_config", "version"):
        assert sent[key] == original[key]
    sent_request_config = cast("JsonObject", sent["request_config"])
    original_request_config = cast("JsonObject", original["request_config"])
    for key in original_request_config:
        if key != "tracked_flights":
            assert sent_request_config[key] == original_request_config[key]
    assert sent_request_config["tracked_flights"] == []


def test_replace_refuses_more_than_five_before_any_request() -> None:
    transport = FakeTransport([(200, fixture_document("get-configuration.json"))])
    wall = client(transport)
    snapshot = wall.read()
    six = tuple(TrackedFlight.new(f"BA{i}", created_at=NOW) for i in range(1, MAX_TRACKED_FLIGHTS + 2))

    with pytest.raises(ValueError, match="five"):
        wall.replace_tracked_flights(snapshot, six)

    assert len(transport.calls) == 1


def test_replace_refuses_a_non_authoritative_snapshot() -> None:
    transport = FakeTransport([(503, {})])
    wall = client(transport)
    snapshot = wall.read()

    with pytest.raises(ValueError, match="authoritative"):
        wall.replace_tracked_flights(snapshot, ())

    assert len(transport.calls) == 1


def test_replace_timeout_is_unknown_and_is_not_retried() -> None:
    before = fixture_document("get-configuration.json")
    transport = FakeTransport([(200, before), TransportError("timed out"), (200, copy.deepcopy(before))])
    wall = client(transport)
    snapshot = wall.read()

    result = wall.replace_tracked_flights(snapshot, ())

    assert result.outcome is WriteOutcome.UNKNOWN
    assert result.reason == "flightwall_request_failed:TransportError"
    # Exactly one POST, then the re-read so the caller can compare.
    assert [call[0] for call in transport.calls] == ["GET", "POST", "GET"]
    assert result.snapshot.authority is SnapshotAuthority.AUTHORITATIVE


def test_replace_rejected_status_is_rejected_with_no_re_read() -> None:
    before = fixture_document("get-configuration.json")
    transport = FakeTransport(
        [(200, before), (401, {"success": False, "errors": [{"code": 1102, "message": "Invalid API key"}]})]
    )
    wall = client(transport)
    snapshot = wall.read()

    result = wall.replace_tracked_flights(snapshot, ())

    assert result.outcome is WriteOutcome.REJECTED
    assert result.reason == "flightwall_credentials_rejected:1102"
    assert [call[0] for call in transport.calls] == ["GET", "POST"]


def test_tracked_flight_new_matches_the_app_shape() -> None:
    flight = TrackedFlight.new("VY8721", created_at=NOW)

    assert flight.as_payload() == {
        "flight_number": "VY8721",
        "created_at": "2026-09-22T12:00:00.000Z",
        "show_distance_travelled": True,
        "show_metrics": True,
    }


def test_wall_snapshot_repr_contains_no_document() -> None:
    snapshot = WallSnapshot.non_authoritative(NOW, "flightwall_server_error:500")
    assert "display_config" not in repr(snapshot)
