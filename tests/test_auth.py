"""Tests for calendar-isolated Google service-account authentication."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import Mock, patch

from flighty_wall.auth import CALENDAR_READONLY_SCOPE, build_calendar_gateway


def test_build_calendar_gateway_uses_service_account_and_readonly_scope(tmp_path: Path) -> None:
    credentials_path = tmp_path / "service-account.json"
    credentials_path.write_text("{}", encoding="utf-8")
    credentials_path.chmod(0o600)
    credentials = Mock(name="credentials")
    service = Mock(name="calendar_service")
    discovery_module = Mock(name="discovery_module")
    discovery_module.build.return_value = service

    with (
        patch(
            "flighty_wall.auth.service_account.Credentials.from_service_account_file",
            return_value=credentials,
        ) as from_file,
        patch("flighty_wall.auth.import_module", return_value=discovery_module) as import_module,
    ):
        gateway = build_calendar_gateway(credentials_path)

    from_file.assert_called_once_with(str(credentials_path), scopes=[CALENDAR_READONLY_SCOPE])
    import_module.assert_called_once_with("googleapiclient.discovery")
    discovery_module.build.assert_called_once_with(
        "calendar", "v3", credentials=credentials, cache_discovery=False
    )
    assert gateway.service is service
