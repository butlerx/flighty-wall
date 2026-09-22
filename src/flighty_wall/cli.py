"""Command-line entry points for safe setup and diagnostics."""

from __future__ import annotations

import argparse
import json
import os
import sys
import tempfile
from collections.abc import Callable, Sequence
from datetime import UTC, datetime
from pathlib import Path

from .auth import build_calendar_gateway
from .calendar import CalendarGateway, CalendarLimits, CalendarReader, sanitize_event_payload
from .config import AppConfig, ConfigError, load_config
from .models import SnapshotAuthority

GatewayFactory = Callable[[Path], CalendarGateway]
Clock = Callable[[], datetime]

MAX_INSPECTION_WINDOW_DAYS = 365
"""Inspection reads are diagnostic only, so they may look wider than the daemon window."""


def run(
    argv: Sequence[str] | None = None,
    *,
    gateway_factory: GatewayFactory = build_calendar_gateway,
    now: Clock = lambda: datetime.now(UTC),
) -> int:
    """Run a command and return its process exit code."""

    parser = _parser()
    arguments = parser.parse_args(argv)

    if arguments.command == "inspect-calendar":
        return _inspect_calendar(
            config_path=arguments.config,
            output_path=arguments.output,
            sensitive_terms=tuple(arguments.redact_term),
            lookahead_days=arguments.lookahead_days,
            lookback_days=arguments.lookback_days,
            gateway_factory=gateway_factory,
            now=now,
        )

    parser.error(f"unsupported command: {arguments.command}")
    return 2


def main() -> None:
    """Console-script entry point."""

    raise SystemExit(run())


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="flighty-wall")
    commands = parser.add_subparsers(dest="command", required=True)

    inspect = commands.add_parser(
        "inspect-calendar",
        help="read the configured calendar and write a sanitized fixture",
    )
    inspect.add_argument("--config", type=Path, required=True)
    inspect.add_argument("--output", type=Path, required=True)
    inspect.add_argument(
        "--redact-term",
        action="append",
        default=[],
        help="name or other literal text to replace in the fixture; repeat as needed",
    )
    inspect.add_argument(
        "--lookahead-days",
        type=_inspection_window,
        default=None,
        help=(
            f"read this many days ahead instead of service.lookahead_days "
            f"(0-{MAX_INSPECTION_WINDOW_DAYS})"
        ),
    )
    inspect.add_argument(
        "--lookback-days",
        type=_inspection_window,
        default=0,
        help=f"also read this many days of past events (0-{MAX_INSPECTION_WINDOW_DAYS})",
    )
    return parser


def _inspection_window(raw: str) -> int:
    try:
        days = int(raw)
    except ValueError:
        raise argparse.ArgumentTypeError(f"expected an integer, got {raw!r}") from None
    if not 0 <= days <= MAX_INSPECTION_WINDOW_DAYS:
        raise argparse.ArgumentTypeError(
            f"must be between 0 and {MAX_INSPECTION_WINDOW_DAYS} days, got {days}"
        )
    return days


def _inspect_calendar(
    *,
    config_path: Path,
    output_path: Path,
    sensitive_terms: tuple[str, ...],
    lookahead_days: int | None,
    lookback_days: int,
    gateway_factory: GatewayFactory,
    now: Clock,
) -> int:
    try:
        config = load_config(config_path)
        reader = _reader(
            config,
            gateway_factory,
            lookahead_days=lookahead_days,
            lookback_days=lookback_days,
        )
    except ConfigError as error:
        print(f"configuration error: {error}", file=sys.stderr)
        return 2

    snapshot = reader.read_snapshot(now())
    if snapshot.authority is not SnapshotAuthority.AUTHORITATIVE:
        print(f"calendar inspection failed: {snapshot.reason}", file=sys.stderr)
        return 2

    payload: dict[str, object] = {
        "authority": snapshot.authority.value,
        "captured_at": _rfc3339(snapshot.observed_at),
        "events": [
            sanitize_event_payload(event.fields, sensitive_terms=sensitive_terms)
            for event in snapshot.events
        ],
    }
    _atomic_private_json(output_path, payload)
    print(f"wrote {len(snapshot.events)} sanitized event(s) to {output_path}")
    return 0


def _reader(
    config: AppConfig,
    gateway_factory: GatewayFactory,
    *,
    lookahead_days: int | None = None,
    lookback_days: int = 0,
) -> CalendarReader:
    return CalendarReader(
        gateway=gateway_factory(config.google_credentials_path),
        calendar_id=config.calendar_id,
        lookahead_days=config.lookahead_days if lookahead_days is None else lookahead_days,
        lookback_days=lookback_days,
        limits=CalendarLimits(
            max_pages=config.max_pages,
            max_events=config.max_events,
            max_field_chars=config.max_field_chars,
            max_snapshot_bytes=config.max_snapshot_bytes,
        ),
    )


def _atomic_private_json(path: Path, payload: MappingJson) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(
        dir=path.parent,
        prefix=f".{path.name}.",
        suffix=".tmp",
        text=True,
    )
    temporary_path = Path(temporary_name)
    try:
        os.fchmod(descriptor, 0o600)
        with os.fdopen(descriptor, "w", encoding="utf-8") as output:
            json.dump(payload, output, indent=2, sort_keys=True)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary_path, path)
        path.chmod(0o600)
    except BaseException:
        os.close(descriptor) if _descriptor_is_open(descriptor) else None
        temporary_path.unlink(missing_ok=True)
        raise


def _descriptor_is_open(descriptor: int) -> bool:
    try:
        os.fstat(descriptor)
    except OSError:
        return False
    return True


def _rfc3339(value: datetime) -> str:
    return value.astimezone(UTC).isoformat().replace("+00:00", "Z")


MappingJson = dict[str, object]
