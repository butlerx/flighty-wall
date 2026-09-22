"""Configuration loading and local secret-file validation."""

from __future__ import annotations

import os
import stat
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Any, cast


class ConfigError(ValueError):
    """Raised when service configuration is missing or unsafe."""


@dataclass(frozen=True, slots=True)
class AppConfig:
    """Validated non-secret service settings and credential locations."""

    calendar_id: str
    google_credentials_path: Path
    state_path: Path
    poll_interval_seconds: int = 120
    lookahead_days: int = 7
    dry_run: bool = True
    max_pages: int = 10
    max_events: int = 500
    max_field_chars: int = 8_192
    max_snapshot_bytes: int = 1_048_576


def require_private_file(path: Path) -> None:
    """Require a regular file with no group or world permissions."""
    try:
        file_stat = path.stat()
    except FileNotFoundError as error:
        raise ConfigError(f"credential file does not exist: {path}") from error

    if not path.is_file():
        raise ConfigError(f"credential path is not a regular file: {path}")

    file_mode = stat.S_IMODE(file_stat.st_mode)
    if file_mode & 0o077:
        raise ConfigError(f"credential file must be mode 0600: {path}")


def load_config(path: str | os.PathLike[str]) -> AppConfig:
    """Load and validate the service's TOML configuration."""
    config_path = Path(path).expanduser()
    try:
        with config_path.open("rb") as config_file:
            raw = tomllib.load(config_file)
    except FileNotFoundError as error:
        raise ConfigError(f"configuration file does not exist: {config_path}") from error
    except tomllib.TOMLDecodeError as error:
        raise ConfigError(f"invalid TOML in {config_path}: {error}") from error

    google = _table(raw, "google")
    service = _table(raw, "service", required=False)
    storage = _table(raw, "storage")
    limits = _table(raw, "calendar_limits", required=False)

    calendar_id = _required_string(google, "calendar_id")
    if calendar_id.casefold() == "primary":
        raise ConfigError("google.calendar_id must name the dedicated calendar, not primary")

    credentials_path = _path_value(google, "credentials_path")
    state_path = _path_value(storage, "state_path")
    _validate_state_parent(state_path)
    require_private_file(credentials_path)

    config = AppConfig(
        calendar_id=calendar_id,
        google_credentials_path=credentials_path,
        state_path=state_path,
        poll_interval_seconds=_integer(service, "poll_interval_seconds", 120),
        lookahead_days=_integer(service, "lookahead_days", 7),
        dry_run=_boolean(service, "dry_run", default=True),
        max_pages=_integer(limits, "max_pages", 10),
        max_events=_integer(limits, "max_events", 500),
        max_field_chars=_integer(limits, "max_field_chars", 8_192),
        max_snapshot_bytes=_integer(limits, "max_snapshot_bytes", 1_048_576),
    )
    _validate_ranges(config)
    return config


def _table(raw: dict[str, Any], name: str, *, required: bool = True) -> dict[str, Any]:
    value = raw.get(name)
    if value is None and not required:
        return {}
    if not isinstance(value, dict):
        raise ConfigError(f"missing or invalid [{name}] table")
    return cast("dict[str, Any]", value)


def _required_string(table: dict[str, Any], key: str) -> str:
    value = table.get(key)
    if not isinstance(value, str) or not value.strip():
        raise ConfigError(f"{key} must be a non-empty string")
    return value.strip()


def _path_value(table: dict[str, Any], key: str) -> Path:
    return Path(_required_string(table, key)).expanduser()


def _integer(table: dict[str, Any], key: str, default: int) -> int:
    value = table.get(key, default)
    if isinstance(value, bool):
        raise ConfigError(f"{key} must be an integer")
    if not isinstance(value, int):
        raise ConfigError(f"{key} must be an integer")
    return value


def _boolean(table: dict[str, Any], key: str, *, default: bool) -> bool:
    value = table.get(key, default)
    if not isinstance(value, bool):
        raise ConfigError(f"{key} must be true or false")
    return value


def _validate_state_parent(state_path: Path) -> None:
    parent = state_path.parent
    if parent.exists() and not parent.is_dir():
        raise ConfigError(f"state directory is not a directory: {parent}")

    existing_parent = parent
    while not existing_parent.exists() and existing_parent != existing_parent.parent:
        existing_parent = existing_parent.parent
    try:
        is_writable_directory = existing_parent.is_dir() and os.access(existing_parent, os.W_OK)
    except OSError as error:
        raise ConfigError(f"cannot inspect state directory: {parent}") from error
    if not is_writable_directory:
        raise ConfigError(f"state directory is not writable: {parent}")


def _validate_ranges(config: AppConfig) -> None:
    ranges = {
        "poll_interval_seconds": (config.poll_interval_seconds, 30, 86_400),
        "lookahead_days": (config.lookahead_days, 1, 30),
        "max_pages": (config.max_pages, 1, 100),
        "max_events": (config.max_events, 1, 10_000),
        "max_field_chars": (config.max_field_chars, 256, 1_000_000),
        "max_snapshot_bytes": (config.max_snapshot_bytes, 1_024, 100_000_000),
    }
    for name, (value, minimum, maximum) in ranges.items():
        if value < minimum or value > maximum:
            raise ConfigError(f"{name} must be between {minimum} and {maximum}")
