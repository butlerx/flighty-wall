"""Google service-account authentication for the dedicated calendar."""

from __future__ import annotations

from importlib import import_module
from pathlib import Path
from typing import Protocol, cast

from google.oauth2 import service_account

from .calendar import CalendarService, GoogleCalendarGateway
from .config import require_private_file

CALENDAR_READONLY_SCOPE = "https://www.googleapis.com/auth/calendar.readonly"


class _CredentialsFactory(Protocol):
    def from_service_account_file(self, filename: str, *, scopes: list[str]) -> object: ...


class _ServiceBuilder(Protocol):
    def __call__(
        self,
        service_name: str,
        version: str,
        *,
        credentials: object,
        cache_discovery: bool,
    ) -> CalendarService: ...


class _DiscoveryModule(Protocol):
    build: _ServiceBuilder


def build_calendar_gateway(credentials_path: Path) -> GoogleCalendarGateway:
    """Build a Calendar gateway with a private service-account key."""

    require_private_file(credentials_path)
    credentials_factory = cast(_CredentialsFactory, service_account.Credentials)
    credentials = credentials_factory.from_service_account_file(
        str(credentials_path), scopes=[CALENDAR_READONLY_SCOPE]
    )
    discovery_module = cast(_DiscoveryModule, import_module("googleapiclient.discovery"))
    service = discovery_module.build(
        "calendar", "v3", credentials=credentials, cache_discovery=False
    )
    return GoogleCalendarGateway(service)
