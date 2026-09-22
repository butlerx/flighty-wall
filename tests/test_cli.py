"""Tests for the setup and diagnostic commands."""

from __future__ import annotations

import stat
from datetime import UTC, datetime
from pathlib import Path
from typing import TYPE_CHECKING, Any

import orjson
from click.testing import CliRunner

from flighty_wall.cli import Deps, cli

if TYPE_CHECKING:
    from collections.abc import Mapping

    from flighty_wall.calendar import CalendarGateway


class FixtureGateway:
    def __init__(self, page: Mapping[str, Any] | Exception) -> None:
        self.page = page
        self.calls: list[dict[str, object]] = []

    def list_events_page(
        self,
        *,
        calendar_id: str,
        time_min: datetime,
        time_max: datetime,
        page_token: str | None,
    ) -> Mapping[str, Any]:
        del calendar_id, page_token
        self.calls.append({"time_min": time_min, "time_max": time_max})
        if isinstance(self.page, Exception):
            raise self.page
        return self.page


def write_config(tmp_path: Path) -> Path:
    state_dir = tmp_path / "state"
    state_dir.mkdir(mode=0o700)
    credentials = tmp_path / "service-account.json"
    credentials.write_text("{}", encoding="utf-8")
    credentials.chmod(0o600)
    config_path = tmp_path / "config.toml"
    config_path.write_text(
        f"""
[google]
calendar_id = "friends@example.invalid"
credentials_path = "{credentials}"

[storage]
state_path = "{state_dir / "state.sqlite3"}"
""".strip()
        + "\n",
        encoding="utf-8",
    )
    return config_path


def invoke(arguments: list[str], *, gateway: FixtureGateway, now: datetime) -> int:
    """Run a command with both effects faked, and return its exit code."""
    result = CliRunner().invoke(
        cli,
        arguments,
        obj=Deps(gateway_factory=lambda _: gateway, now=lambda: now),
    )
    if result.exception is not None and not isinstance(result.exception, SystemExit):
        raise result.exception
    return result.exit_code


def test_inspect_calendar_writes_private_sanitized_fixture(tmp_path: Path) -> None:
    config_path = write_config(tmp_path)
    output_path = tmp_path / "fixture.json"
    gateway = FixtureGateway(
        {
            "items": [
                {
                    "id": "private-event-id",
                    "status": "confirmed",
                    "summary": "Alice Smith · AA123",
                    "description": "Confirmation: ABC123\nSeat: 12A",
                    "start": {"dateTime": "2026-09-22T08:00:00-04:00"},
                    "end": {"dateTime": "2026-09-22T10:30:00-05:00"},
                    "updated": "2026-09-21T11:00:00Z",
                }
            ]
        }
    )

    exit_code = invoke(
        [
            "inspect-calendar",
            "--config",
            str(config_path),
            "--output",
            str(output_path),
            "--redact-term",
            "Alice Smith",
        ],
        gateway=gateway,
        now=datetime(2026, 9, 21, 12, 0, tzinfo=UTC),
    )

    fixture = orjson.loads(output_path.read_bytes())
    rendered = repr(fixture)
    assert exit_code == 0
    assert fixture["authority"] == "authoritative"
    assert fixture["captured_at"] == "2026-09-21T12:00:00Z"
    assert len(fixture["events"]) == 1
    assert "AA123" in rendered
    assert "Alice Smith" not in rendered
    assert "ABC123" not in rendered
    assert "12A" not in rendered
    assert stat.S_IMODE(output_path.stat().st_mode) == 0o600


def test_inspect_calendar_failure_does_not_write_fixture(tmp_path: Path) -> None:
    config_path = write_config(tmp_path)
    output_path = tmp_path / "fixture.json"

    exit_code = invoke(
        ["inspect-calendar", "--config", str(config_path), "--output", str(output_path)],
        gateway=FixtureGateway(TimeoutError("secret response")),
        now=datetime(2026, 9, 21, 12, 0, tzinfo=UTC),
    )

    assert exit_code == 2
    assert not output_path.exists()


def test_inspect_calendar_window_flags_widen_the_read(tmp_path: Path) -> None:
    config_path = write_config(tmp_path)
    output_path = tmp_path / "fixture.json"
    gateway = FixtureGateway({"items": []})

    exit_code = invoke(
        [
            "inspect-calendar",
            "--config",
            str(config_path),
            "--output",
            str(output_path),
            "--lookahead-days",
            "60",
            "--lookback-days",
            "3",
        ],
        gateway=gateway,
        now=datetime(2026, 9, 21, 12, 0, tzinfo=UTC),
    )

    assert exit_code == 0
    assert gateway.calls[0]["time_min"] == datetime(2026, 9, 18, 12, 0, tzinfo=UTC)
    assert gateway.calls[0]["time_max"] == datetime(2026, 11, 20, 12, 0, tzinfo=UTC)


def test_inspect_calendar_rejects_window_outside_inspection_bounds(tmp_path: Path) -> None:
    config_path = write_config(tmp_path)
    output_path = tmp_path / "fixture.json"

    exit_code = invoke(
        [
            "inspect-calendar",
            "--config",
            str(config_path),
            "--output",
            str(output_path),
            "--lookahead-days",
            "400",
        ],
        gateway=FixtureGateway({"items": []}),
        now=datetime(2026, 9, 21, 12, 0, tzinfo=UTC),
    )

    assert exit_code == 2
    assert not output_path.exists()


def test_gateway_factory_matches_calendar_protocol(tmp_path: Path) -> None:
    gateway: CalendarGateway = FixtureGateway({"items": []})
    assert gateway is not None


# --- probe-wall ------------------------------------------------------------------------------


class ScriptedTransport:
    """Return one canned (status, body) for every request and record the calls."""

    def __init__(self, status: int, body: object) -> None:
        self.status = status
        self.body = body
        self.calls: list[tuple[str, str]] = []

    def request(
        self,
        method: str,
        path: str,
        *,
        headers: dict[str, str],
        body: object | None,
    ) -> tuple[int, object]:
        del headers, body
        self.calls.append((method, path))
        return self.status, self.body


def write_config_with_wall(tmp_path: Path) -> Path:
    config_path = write_config(tmp_path)
    wall_credentials = tmp_path / "flightwall.toml"
    wall_credentials.write_text('api_key = "k"\nuser_id = "u"\n', encoding="utf-8")
    wall_credentials.chmod(0o600)
    with config_path.open("a", encoding="utf-8") as handle:
        handle.write(f'\n[flightwall]\ncredentials_path = "{wall_credentials}"\n')
    return config_path


def invoke_wall(arguments: list[str], *, transport: ScriptedTransport) -> tuple[int, str, str]:
    result = CliRunner().invoke(
        cli,
        arguments,
        obj=Deps(transport_factory=lambda _: transport, now=lambda: datetime(2026, 9, 22, tzinfo=UTC)),
    )
    if result.exception is not None and not isinstance(result.exception, SystemExit):
        raise result.exception
    return result.exit_code, result.stdout, result.stderr


def wall_document() -> dict[str, object]:
    fixture = Path(__file__).parent / "fixtures" / "flightwall" / "get-configuration.json"
    parsed: Any = orjson.loads(fixture.read_bytes())
    return dict(parsed["response"]["body"]["json"])


def test_probe_wall_reads_once_and_prints_tracked_flights(tmp_path: Path) -> None:
    transport = ScriptedTransport(200, wall_document())

    code, out, _ = invoke_wall(
        ["probe-wall", "--config", str(write_config_with_wall(tmp_path))], transport=transport
    )

    assert code == 0
    assert transport.calls == [("GET", "/configuration")]
    assert "model: mini-v1" in out
    assert "tracked_flights: 1" in out
    assert "EI61" in out


def test_probe_wall_without_flightwall_table_is_a_config_error(tmp_path: Path) -> None:
    transport = ScriptedTransport(200, wall_document())

    code, _, err = invoke_wall(["probe-wall", "--config", str(write_config(tmp_path))], transport=transport)

    assert code == 2
    assert "[flightwall]" in err
    assert transport.calls == []


def test_probe_wall_reports_a_rejected_key_without_echoing_it(tmp_path: Path) -> None:
    transport = ScriptedTransport(
        401, {"success": False, "errors": [{"code": 1102, "message": "Invalid API key"}]}
    )

    code, _, err = invoke_wall(
        ["probe-wall", "--config", str(write_config_with_wall(tmp_path))], transport=transport
    )

    assert code == 2
    assert "flightwall_credentials_rejected:1102" in err
