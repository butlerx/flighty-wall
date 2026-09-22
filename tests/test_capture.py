"""Tests for the HAR capture sanitizer.

The FlightWall hosts and paths below are placeholders. Nothing here asserts what the
real API looks like: these tests pin the sanitizer's guarantees so that when a real
capture arrives, the owner can trust what lands on disk.
"""

from __future__ import annotations

import re
import stat
from typing import TYPE_CHECKING, Any, cast

import orjson
import pytest
from click.testing import CliRunner, Result

from flighty_wall.capture import (
    MAX_BODY_BYTES,
    CaptureEntry,
    CaptureError,
    manifest,
    observed_hosts,
    sanitize_har,
)
from flighty_wall.cli import cli

if TYPE_CHECKING:
    from pathlib import Path

HOST = "api.flightwall.invalid"
FAKE_BEARER = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJkZXZpY2UifQ.rT8pQ2wLxZ0nKf4hVbCmJs7YdEuA1gXoNi"
DEVICE_ID = "4f3c2a19-7b6e-4d51-9a8f-2c1b0e5d7a63"


def har_entry(
    *,
    method: str = "GET",
    url: str = f"https://{HOST}/v1/flights",
    status: int = 200,
    request_body: object = None,
    response_body: object = None,
    headers: dict[str, str] | None = None,
) -> dict[str, Any]:
    header_list = [
        {"name": name, "value": value}
        for name, value in (headers or {"Authorization": f"Bearer {FAKE_BEARER}"}).items()
    ]
    request: dict[str, Any] = {
        "method": method,
        "url": url,
        "headers": header_list,
        "queryString": [{"name": "limit", "value": "5"}, {"name": "device", "value": DEVICE_ID}],
    }
    if request_body is not None:
        request["postData"] = {
            "mimeType": "application/json",
            "text": orjson.dumps(request_body).decode("utf-8"),
        }
    response: dict[str, Any] = {
        "status": status,
        "headers": [{"name": "Content-Type", "value": "application/json"}],
    }
    if response_body is not None:
        response["content"] = {
            "mimeType": "application/json",
            "text": orjson.dumps(response_body).decode("utf-8"),
        }
    return {"request": request, "response": response}


def har(*entries: dict[str, Any]) -> dict[str, Any]:
    return {"log": {"version": "1.2", "entries": list(entries)}}


def section(entry: CaptureEntry, name: str) -> dict[str, Any]:
    """Narrow one half of a sanitized payload so assertions can index into it."""
    return cast("dict[str, Any]", entry.payload[name])


def invoke(arguments: list[str]) -> Result:
    """Run a command, re-raising anything click swallowed so a bug is not read as exit 1."""
    result = CliRunner().invoke(cli, arguments)
    if result.exception is not None and not isinstance(result.exception, SystemExit):
        raise result.exception
    return result


def test_keeps_the_contract_shape() -> None:
    entries = sanitize_har(har(har_entry(response_body={"flights": [{"designator": "VY8721"}]})))

    (entry,) = entries
    assert entry.method == "GET"
    assert entry.host == HOST
    assert entry.path == "/v1/flights"
    assert entry.status == 200
    assert entry.payload["response"] == {
        "status": 200,
        "header_names": ["content-type"],
        "body": {"mime_type": "application/json", "json": {"flights": [{"designator": "VY8721"}]}},
    }


def test_header_values_never_survive() -> None:
    entries = sanitize_har(har(har_entry()))

    rendered = repr(entries[0].payload)
    assert FAKE_BEARER not in rendered
    assert "eyJ" not in rendered
    assert section(entries[0], "request")["header_names"] == ["authorization"]


def test_query_values_never_survive_but_keys_do() -> None:
    entries = sanitize_har(har(har_entry()))

    request = section(entries[0], "request")
    assert request["query_keys"] == ["device", "limit"]
    assert DEVICE_ID not in repr(request)


def test_path_identifiers_are_scrubbed() -> None:
    entries = sanitize_har(har(har_entry(url=f"https://{HOST}/v1/devices/{DEVICE_ID}/flights")))

    assert DEVICE_ID not in entries[0].path
    assert entries[0].path == "/v1/devices/<redacted-uuid>/flights"


def test_body_secrets_are_scrubbed_by_pattern_and_by_key() -> None:
    entries = sanitize_har(
        har(
            har_entry(
                method="POST",
                request_body={"refresh_token": "r0Tk9x", "email": "me@example.invalid"},
                response_body={"access_token": "s3cr3t", "expires_in": 3600},
            )
        )
    )

    rendered = repr(entries[0].payload)
    assert "r0Tk9x" not in rendered
    assert "s3cr3t" not in rendered
    assert "me@example.invalid" not in rendered
    # The keys stay so the discovery document can describe the real request shape.
    assert "refresh_token" in rendered
    assert "expires_in" in rendered


def test_home_coordinates_are_scrubbed_from_area_mode_bodies() -> None:
    entries = sanitize_har(
        har(har_entry(response_body={"mode": "area", "lat": 53.349805, "radius_km": 25.0}))
    )

    body = section(entries[0], "response")["body"]["json"]
    assert body["lat"] == "<redacted>"
    assert body["radius_km"] == 25.0
    assert body["mode"] == "area"


def test_friend_names_are_scrubbed_with_supplied_terms() -> None:
    entries = sanitize_har(
        har(har_entry(response_body={"label": "Alice Smith"})),
        sensitive_terms=("Alice Smith",),
    )

    assert "Alice" not in repr(entries[0].payload)


def test_host_allowlist_filters_unrelated_traffic() -> None:
    document = har(
        har_entry(url=f"https://{HOST}/v1/flights"),
        har_entry(url="https://telemetry.vendor.invalid/collect"),
    )

    kept = sanitize_har(document, hosts=(HOST,))

    assert [entry.host for entry in kept] == [HOST]


def test_host_allowlist_is_case_insensitive() -> None:
    kept = sanitize_har(har(har_entry()), hosts=(HOST.upper(),))

    assert len(kept) == 1


def test_omitting_the_allowlist_keeps_every_host() -> None:
    document = har(har_entry(), har_entry(url="https://telemetry.vendor.invalid/collect"))

    assert len(sanitize_har(document)) == 2


def test_observed_hosts_lists_every_host_for_choosing_an_allowlist() -> None:
    document = har(
        har_entry(url="https://telemetry.vendor.invalid/collect"),
        har_entry(url=f"https://{HOST}/v1/flights"),
    )

    assert observed_hosts(document) == (HOST, "telemetry.vendor.invalid")


def test_index_survives_host_filtering_so_order_stays_traceable() -> None:
    document = har(
        har_entry(url="https://telemetry.vendor.invalid/collect"),
        har_entry(url=f"https://{HOST}/v1/flights"),
    )

    (entry,) = sanitize_har(document, hosts=(HOST,))

    assert entry.index == 2


def test_absent_body_is_recorded_as_null() -> None:
    entries = sanitize_har(har(har_entry(status=204)))

    assert section(entries[0], "response")["body"] is None


def test_present_but_empty_body_is_distinguished_from_an_absent_one() -> None:
    document = har(har_entry())
    document["log"]["entries"][0]["response"]["content"] = {"mimeType": "application/json", "text": ""}

    body = section(sanitize_har(document)[0], "response")["body"]

    assert body == {"omitted": "empty", "mime_type": "application/json"}


def test_non_json_body_is_recorded_by_size_only() -> None:
    document = har(har_entry())
    document["log"]["entries"][0]["response"]["content"] = {
        "mimeType": "text/html",
        "text": "<html>login</html>",
    }

    body = section(sanitize_har(document)[0], "response")["body"]

    assert body == {"omitted": "non_json", "mime_type": "text/html", "bytes": 18}


def test_oversized_body_is_recorded_by_size_only() -> None:
    document = har(har_entry())
    document["log"]["entries"][0]["response"]["content"] = {
        "mimeType": "application/json",
        "text": "x" * (MAX_BODY_BYTES + 1),
    }

    body = section(sanitize_har(document)[0], "response")["body"]

    assert body["omitted"] == "oversized"
    assert body["bytes"] == MAX_BODY_BYTES + 1


def test_filenames_sort_in_capture_order() -> None:
    document = har(
        har_entry(url=f"https://{HOST}/v1/flights"),
        har_entry(method="POST", url=f"https://{HOST}/v1/flights"),
        har_entry(method="DELETE", url=f"https://{HOST}/v1/flights/abc"),
    )

    names = [entry.filename for entry in sanitize_har(document)]

    assert names == [
        "001-get-v1-flights.json",
        "002-post-v1-flights.json",
        "003-delete-v1-flights-abc.json",
    ]


def test_root_path_still_produces_a_filename() -> None:
    entries = sanitize_har(har(har_entry(url=f"https://{HOST}/")))

    assert entries[0].filename == "001-get-root.json"


def test_manifest_indexes_the_written_fixtures() -> None:
    entries = sanitize_har(har(har_entry(), har_entry(method="POST")))

    summary = manifest(entries)
    listed = cast("list[dict[str, Any]]", summary["entries"])

    assert summary["entry_count"] == 2
    assert summary["hosts"] == [HOST]
    assert listed[0]["file"] == "001-get-v1-flights.json"
    assert listed[1]["method"] == "POST"


def test_missing_log_object_fails_loudly() -> None:
    with pytest.raises(CaptureError, match="missing 'log' object"):
        sanitize_har({"entries": []})


def test_missing_entries_array_fails_loudly() -> None:
    with pytest.raises(CaptureError, match=re.escape("missing 'log.entries' array")):
        sanitize_har({"log": {"version": "1.2"}})


def test_malformed_entry_fails_loudly_rather_than_being_skipped() -> None:
    # A half-parsed capture would look like a short contract; that is worse than an error.
    with pytest.raises(CaptureError, match="malformed entry"):
        sanitize_har({"log": {"entries": ["not-an-object"]}})


def test_entry_without_a_request_or_response_is_dropped() -> None:
    document = har(har_entry())
    document["log"]["entries"].append({"request": {"method": "GET", "url": "https://x.invalid/"}})

    assert len(sanitize_har(document)) == 1


def write_har(tmp_path: Path, document: dict[str, Any]) -> Path:
    path = tmp_path / "capture.har"
    path.write_bytes(orjson.dumps(document))
    return path


def test_cli_writes_private_sanitized_fixtures(tmp_path: Path) -> None:
    input_path = write_har(
        tmp_path,
        har(har_entry(response_body={"flights": [{"designator": "VY8721", "token": "s3cr3t"}]})),
    )
    output_dir = tmp_path / "fixtures"

    result = invoke(
        [
            "sanitize-capture",
            "--input",
            str(input_path),
            "--output-dir",
            str(output_dir),
            "--host",
            HOST,
            "--redact-term",
            "Alice Smith",
        ]
    )

    fixture = output_dir / "001-get-v1-flights.json"
    rendered = fixture.read_text(encoding="utf-8")
    assert result.exit_code == 0
    assert "VY8721" in rendered
    assert "s3cr3t" not in rendered
    assert FAKE_BEARER not in rendered
    assert stat.S_IMODE(fixture.stat().st_mode) == 0o600
    assert stat.S_IMODE((output_dir / "manifest.json").stat().st_mode) == 0o600


def test_cli_reports_hosts_when_the_allowlist_matches_nothing(tmp_path: Path) -> None:
    input_path = write_har(tmp_path, har(har_entry()))
    output_dir = tmp_path / "fixtures"

    result = invoke(
        [
            "sanitize-capture",
            "--input",
            str(input_path),
            "--output-dir",
            str(output_dir),
            "--host",
            "wrong.invalid",
        ]
    )

    assert result.exit_code == 1
    assert HOST in result.output
    assert not output_dir.exists()


def test_cli_rejects_a_missing_capture_file(tmp_path: Path) -> None:
    result = invoke(
        [
            "sanitize-capture",
            "--input",
            str(tmp_path / "absent.har"),
            "--output-dir",
            str(tmp_path / "fixtures"),
        ]
    )

    assert result.exit_code == 2


def test_cli_rejects_a_file_that_is_not_json(tmp_path: Path) -> None:
    input_path = tmp_path / "capture.har"
    input_path.write_text("not json at all", encoding="utf-8")

    result = invoke(["sanitize-capture", "--input", str(input_path), "--output-dir", str(tmp_path / "out")])

    assert result.exit_code == 2


def test_cli_rejects_json_that_is_not_a_har_archive(tmp_path: Path) -> None:
    input_path = tmp_path / "capture.har"
    input_path.write_bytes(orjson.dumps([1, 2, 3]))

    result = invoke(["sanitize-capture", "--input", str(input_path), "--output-dir", str(tmp_path / "out")])

    assert result.exit_code == 2


def test_cli_rejects_a_har_archive_with_a_malformed_entry(tmp_path: Path) -> None:
    input_path = write_har(tmp_path, {"log": {"entries": ["not-an-object"]}})
    output_dir = tmp_path / "fixtures"

    result = invoke(["sanitize-capture", "--input", str(input_path), "--output-dir", str(output_dir)])

    assert result.exit_code == 2
    assert not output_dir.exists()
