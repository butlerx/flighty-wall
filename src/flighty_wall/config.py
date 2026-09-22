"""Configuration loading and local secret-file validation."""

from __future__ import annotations

import os
import stat
import tomllib
from pathlib import Path
from typing import Annotated, Any

from pydantic import BaseModel, BeforeValidator, ConfigDict, Field, ValidationError, field_validator


class ConfigError(ValueError):
    """Raised when service configuration is missing or unsafe."""


def _as_user_path(value: object) -> object:
    """Turn a configured string into an expanded path before strict validation runs.

    Strict mode refuses to coerce a `str` into a `Path`, but TOML has no path type, so the
    conversion has to happen here. Anything that is neither a string nor a path is passed
    through untouched so the strict check reports it rather than coercing it silently.
    """
    if isinstance(value, str):
        if not value.strip():
            raise ValueError("must be a non-empty path")
        return Path(value.strip()).expanduser()
    if isinstance(value, Path):
        return value.expanduser()
    return value


UserPath = Annotated[Path, BeforeValidator(_as_user_path)]
"""A filesystem path written as a string in TOML, with a leading `~` expanded."""

_STRICT = ConfigDict(frozen=True, strict=True, extra="forbid", str_strip_whitespace=True)
"""Reject unknown keys and type coercion, so a typo fails loudly instead of taking a default."""


class Google(BaseModel):
    """Which calendar to read, and which service-account key opens it."""

    model_config = _STRICT

    calendar_id: str = Field(min_length=1)
    credentials_path: UserPath

    @field_validator("calendar_id")
    @classmethod
    def _not_primary(cls, value: str) -> str:
        if value.casefold() == "primary":
            raise ValueError("must name the dedicated calendar, not primary")
        return value


class Service(BaseModel):
    """Daemon pacing and the dry-run switch that keeps writes off by default."""

    model_config = _STRICT

    poll_interval_seconds: int = Field(default=120, ge=30, le=86_400)
    lookahead_days: int = Field(default=7, ge=1, le=90)
    dry_run: bool = True


class Storage(BaseModel):
    """Where the daemon keeps its own state."""

    model_config = _STRICT

    state_path: UserPath


class FlightWall(BaseModel):
    """Which wall API to talk to and where its per-install key pair lives.

    The contract is recorded in ``docs/flightwall-api-discovery.md``. The host is pinned
    to the one the capture observed; anything else is a misconfiguration, not a feature.
    """

    model_config = _STRICT

    credentials_path: UserPath
    host: str = Field(default="api.theflightwall.com", min_length=1)
    timeout_seconds: float = Field(default=15.0, gt=0, le=120)
    user_agent: str = Field(default="TheFlightWall/1 CFNetwork/3860.700.1 Darwin/25.6.0", min_length=1)

    @field_validator("host")
    @classmethod
    def _bare_hostname(cls, value: str) -> str:
        if "/" in value or ":" in value or value != value.strip().lower():
            raise ValueError("must be a bare lowercase hostname, no scheme, port, or path")
        return value


class Limits(BaseModel):
    """Hard caps that make an oversized calendar response non-authoritative."""

    model_config = _STRICT

    max_pages: int = Field(default=10, ge=1, le=100)
    max_events: int = Field(default=500, ge=1, le=10_000)
    max_field_chars: int = Field(default=8_192, ge=256, le=1_000_000)
    max_snapshot_bytes: int = Field(default=1_048_576, ge=1_024, le=100_000_000)


class AppConfig(BaseModel):
    """Validated non-secret service settings and credential locations."""

    model_config = _STRICT

    google: Google
    # These defaults are safe to share: every model here is frozen, and pydantic copies a
    # model default per instance rather than aliasing it.
    service: Service = Service()
    storage: Storage
    flightwall: FlightWall | None = None
    calendar_limits: Limits = Limits()


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
    raw = _read_toml(config_path)

    try:
        config = AppConfig.model_validate(raw)
    except ValidationError as error:
        raise ConfigError(f"invalid configuration in {config_path}: {_describe(error)}") from error

    # Both checks touch the filesystem, so they stay outside the model: validation has to
    # stay pure enough to run against untrusted input without probing the host.
    _validate_state_parent(config.storage.state_path)
    require_private_file(config.google.credentials_path)
    if config.flightwall is not None:
        require_private_file(config.flightwall.credentials_path)
    return config


def _read_toml(config_path: Path) -> dict[str, Any]:
    try:
        with config_path.open("rb") as config_file:
            return tomllib.load(config_file)
    except FileNotFoundError as error:
        raise ConfigError(f"configuration file does not exist: {config_path}") from error
    except tomllib.TOMLDecodeError as error:
        raise ConfigError(f"invalid TOML in {config_path}: {error}") from error


def _describe(error: ValidationError) -> str:
    """Render a validation failure as `table.key: reason`, without echoing the value."""
    return "; ".join(
        f"{'.'.join(str(part) for part in item['loc'])}: {item['msg']}" for item in error.errors()
    )


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
