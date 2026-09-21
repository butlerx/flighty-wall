"""Tests for the safe calendar inspection command."""

from __future__ import annotations

import json
import stat
from collections.abc import Mapping
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from flighty_wall.calendar import CalendarGateway
from flighty_wall.cli import run


class FixtureGateway:
    def __init__(self, page: Mapping[str, Any] | Exception) -> None:
        self.page = page

    def list_events_page(
        self,
        *,
        calendar_id: str,
        time_min: datetime,
        time_max: datetime,
        page_token: str | None,
    ) -> Mapping[str, Any]:
        del calendar_id, time_min, time_max, page_token
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

    exit_code = run(
        [
            "inspect-calendar",
            "--config",
            str(config_path),
            "--output",
            str(output_path),
            "--redact-term",
            "Alice Smith",
        ],
        gateway_factory=lambda _: gateway,
        now=lambda: datetime(2026, 9, 21, 12, 0, tzinfo=UTC),
    )

    fixture = json.loads(output_path.read_text(encoding="utf-8"))
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

    exit_code = run(
        ["inspect-calendar", "--config", str(config_path), "--output", str(output_path)],
        gateway_factory=lambda _: FixtureGateway(TimeoutError("secret response")),
        now=lambda: datetime(2026, 9, 21, 12, 0, tzinfo=UTC),
    )

    assert exit_code == 2
    assert not output_path.exists()


def test_gateway_factory_matches_calendar_protocol(tmp_path: Path) -> None:
    gateway: CalendarGateway = FixtureGateway({"items": []})
    assert gateway is not None
