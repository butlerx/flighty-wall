"""Command-line entry points for safe setup and diagnostics."""

from __future__ import annotations

import os
import tempfile
import threading
from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path

import click
import orjson

from .auth import build_calendar_gateway
from .calendar import CalendarGateway, CalendarLimits, CalendarReader, sanitize_event_payload
from .capture import CaptureError, manifest, observed_hosts, sanitize_har
from .config import AppConfig, ConfigError, FlightWall, load_config
from .flightwall import FlightWallClient, HttpxTransport, Transport, load_credentials
from .models import SnapshotAuthority
from .redaction import as_mapping
from .service import (
    CycleReport,
    CycleStatus,
    LockBusyError,
    configure_logging,
    host_lock,
    install_stop_signals,
    lock_path_for,
    run_cycle,
    run_forever,
)
from .state import StateStore

GatewayFactory = Callable[[Path], CalendarGateway]
TransportFactory = Callable[[FlightWall], Transport]
Clock = Callable[[], datetime]

MAX_INSPECTION_WINDOW_DAYS = 365
"""Inspection reads are diagnostic only, so they may look wider than the daemon window."""

MappingJson = dict[str, object]


def _utc_now() -> datetime:
    return datetime.now(UTC)


def _httpx_transport(settings: FlightWall) -> Transport:
    return HttpxTransport(settings.host, timeout_seconds=settings.timeout_seconds)


@dataclass(frozen=True, slots=True)
class Deps:
    """Injection seam for the effects a command cannot fake: Google, the wall, and the clock."""

    gateway_factory: GatewayFactory = field(default=build_calendar_gateway)
    transport_factory: TransportFactory = field(default=_httpx_transport)
    now: Clock = field(default=_utc_now)


@click.group()
@click.pass_context
def cli(ctx: click.Context) -> None:
    """Sync Flighty Friends' flights to a FlightWall."""
    ctx.obj = ctx.obj or Deps()


@cli.command("inspect-calendar")
@click.option("--config", "config_path", type=click.Path(path_type=Path), required=True)
@click.option("--output", "output_path", type=click.Path(path_type=Path), required=True)
@click.option(
    "--redact-term",
    "sensitive_terms",
    multiple=True,
    help="name or other literal text to replace in the fixture; repeat as needed",
)
@click.option(
    "--lookahead-days",
    type=click.IntRange(0, MAX_INSPECTION_WINDOW_DAYS),
    default=None,
    help="read this many days ahead instead of service.lookahead_days",
)
@click.option(
    "--lookback-days",
    type=click.IntRange(0, MAX_INSPECTION_WINDOW_DAYS),
    default=0,
    help="also read this many days of past events",
)
@click.pass_context
def inspect_calendar(
    ctx: click.Context,
    *,
    config_path: Path,
    output_path: Path,
    sensitive_terms: tuple[str, ...],
    lookahead_days: int | None,
    lookback_days: int,
) -> None:
    """Read the configured calendar and write a sanitized fixture."""
    deps = ctx.ensure_object(Deps)
    try:
        config = load_config(config_path)
        reader = _reader(
            config,
            deps.gateway_factory,
            lookahead_days=lookahead_days,
            lookback_days=lookback_days,
        )
    except ConfigError as error:
        click.echo(f"configuration error: {error}", err=True)
        ctx.exit(2)

    snapshot = reader.read_snapshot(deps.now())
    if snapshot.authority is not SnapshotAuthority.AUTHORITATIVE:
        click.echo(f"calendar inspection failed: {snapshot.reason}", err=True)
        ctx.exit(2)

    payload: MappingJson = {
        "authority": snapshot.authority.value,
        "captured_at": _rfc3339(snapshot.observed_at),
        "events": [
            sanitize_event_payload(event.fields, sensitive_terms=sensitive_terms) for event in snapshot.events
        ],
    }
    _atomic_private_json(output_path, payload)
    click.echo(f"wrote {len(snapshot.events)} sanitized event(s) to {output_path}")


@cli.command("sanitize-capture")
@click.option(
    "--input",
    "input_path",
    type=click.Path(path_type=Path),
    required=True,
    help="HAR file exported by the proxy",
)
@click.option("--output-dir", type=click.Path(path_type=Path), required=True)
@click.option(
    "--host",
    "hosts",
    multiple=True,
    help="keep only entries for this host; repeat as needed, omit to keep every host",
)
@click.option(
    "--redact-term",
    "sensitive_terms",
    multiple=True,
    help="name, device label, or other literal text to replace; repeat as needed",
)
@click.pass_context
def sanitize_capture(
    ctx: click.Context,
    *,
    input_path: Path,
    output_dir: Path,
    hosts: tuple[str, ...],
    sensitive_terms: tuple[str, ...],
) -> None:
    """Turn a HAR capture of your own FlightWall app traffic into committable fixtures."""
    try:
        document = orjson.loads(input_path.read_bytes())
    except FileNotFoundError:
        click.echo(f"capture file does not exist: {input_path}", err=True)
        ctx.exit(2)
    except orjson.JSONDecodeError as error:
        click.echo(f"capture file is not valid JSON: {error}", err=True)
        ctx.exit(2)

    parsed = as_mapping(document)
    if parsed is None:
        click.echo("capture file is not a HAR archive: top level is not an object", err=True)
        ctx.exit(2)

    try:
        entries = sanitize_har(parsed, sensitive_terms=sensitive_terms, hosts=hosts)
        every_host = observed_hosts(parsed)
    except CaptureError as error:
        click.echo(f"capture error: {error}", err=True)
        ctx.exit(2)

    if not entries:
        click.echo(f"no entries matched. hosts in this capture: {', '.join(every_host) or 'none'}")
        ctx.exit(1)

    for entry in entries:
        _atomic_private_json(output_dir / entry.filename, entry.payload)
        click.echo(f"{entry.method:<6} {entry.status:<4} {entry.host}{entry.path} -> {entry.filename}")

    _atomic_private_json(
        output_dir / "manifest.json",
        manifest(entries, sensitive_terms=sensitive_terms),
    )
    click.echo(f"wrote {len(entries)} sanitized entr(ies) and manifest.json to {output_dir}")
    click.echo("review every file by hand before committing, then delete the raw capture")


@cli.command("probe-wall")
@click.option("--config", "config_path", type=click.Path(path_type=Path), required=True)
@click.pass_context
def probe_wall(ctx: click.Context, *, config_path: Path) -> None:
    """Read the wall's configuration once and report the tracked flights. Never writes."""
    deps = ctx.ensure_object(Deps)
    try:
        config = load_config(config_path)
        wall = _wall(config, deps)
    except ConfigError as error:
        click.echo(f"configuration error: {error}", err=True)
        ctx.exit(2)

    snapshot = wall.read()
    if snapshot.authority is not SnapshotAuthority.AUTHORITATIVE:
        click.echo(f"wall read failed: {snapshot.reason}", err=True)
        ctx.exit(2)

    click.echo(f"observed_at: {_rfc3339(snapshot.observed_at)}")
    click.echo(f"model: {snapshot.fingerprint.model}")
    click.echo(f"tracked_flights: {len(snapshot.tracked_flights)}")
    for flight in snapshot.tracked_flights:
        click.echo(f"  {flight.flight_number}  added {flight.created_at}")


EXIT_NOT_AUTHORITATIVE = 3
"""A source could not be read authoritatively; nothing was written."""
EXIT_WRITE_FAILED = 4
"""The wall rejected the write or its outcome is unknown; see the summary line."""
EXIT_BUSY = 5
"""Another mutating process holds the host lock."""

_NON_AUTHORITATIVE = frozenset(
    {
        CycleStatus.CALENDAR_NOT_AUTHORITATIVE,
        CycleStatus.PARSE_NOT_AUTHORITATIVE,
        CycleStatus.WALL_NOT_AUTHORITATIVE,
    }
)
_WRITE_FAILED = frozenset({CycleStatus.REJECTED, CycleStatus.UNKNOWN})


@cli.command("sync")
@click.option("--config", "config_path", type=click.Path(path_type=Path), required=True)
@click.option(
    "--apply",
    "apply_flag",
    is_flag=True,
    help="write to the wall even if service.dry_run is true; without it this is a dry run",
)
@click.pass_context
def sync(ctx: click.Context, *, config_path: Path, apply_flag: bool) -> None:
    """Run one cycle and print what it did or would do."""
    deps = ctx.ensure_object(Deps)
    try:
        config = load_config(config_path)
        wall = _wall(config, deps)
        calendar = _reader(config, deps.gateway_factory)
    except ConfigError as error:
        click.echo(f"configuration error: {error}", err=True)
        ctx.exit(2)

    apply = apply_flag or not config.service.dry_run
    lock = lock_path_for(config.storage.state_path)
    try:
        with host_lock(lock), StateStore(config.storage.state_path) as store:
            report = run_cycle(calendar=calendar, wall=wall, store=store, now=deps.now(), apply=apply)
    except LockBusyError as error:
        click.echo(f"busy: {error}", err=True)
        ctx.exit(EXIT_BUSY)

    _print_report(report, apply=apply)
    ctx.exit(_exit_code(report))


@cli.command("run")
@click.option("--config", "config_path", type=click.Path(path_type=Path), required=True)
@click.option("--log-level", default="INFO", show_default=True, help="DEBUG, INFO, WARNING, or ERROR")
@click.pass_context
def run(ctx: click.Context, *, config_path: Path, log_level: str) -> None:
    """Run cycles on the configured interval until SIGTERM or SIGINT."""
    deps = ctx.ensure_object(Deps)
    configure_logging(log_level)
    try:
        config = load_config(config_path)
        wall = _wall(config, deps)
        calendar = _reader(config, deps.gateway_factory)
    except ConfigError as error:
        click.echo(f"configuration error: {error}", err=True)
        ctx.exit(2)

    apply = not config.service.dry_run
    stop = threading.Event()
    install_stop_signals(stop)
    lock = lock_path_for(config.storage.state_path)
    try:
        with host_lock(lock), StateStore(config.storage.state_path) as store:
            click.echo(
                f"flighty-wall starting: interval={config.service.poll_interval_seconds}s "
                f"lookahead={config.service.lookahead_days}d mode={'apply' if apply else 'dry-run'}",
                err=True,
            )
            cycles = run_forever(
                lambda: run_cycle(calendar=calendar, wall=wall, store=store, now=deps.now(), apply=apply),
                interval_seconds=config.service.poll_interval_seconds,
                stop=stop,
            )
    except LockBusyError as error:
        click.echo(f"busy: {error}", err=True)
        ctx.exit(EXIT_BUSY)
    click.echo(f"flighty-wall stopped after {cycles} cycle(s)", err=True)


def _print_report(report: CycleReport, *, apply: bool) -> None:
    click.echo(report.summary())
    if report.plan is not None and report.plan.changed and not apply:
        click.echo("dry run: nothing was written. Re-run with --apply, or set service.dry_run = false.")


def _exit_code(report: CycleReport) -> int:
    if report.status in _NON_AUTHORITATIVE:
        return EXIT_NOT_AUTHORITATIVE
    if report.status in _WRITE_FAILED:
        return EXIT_WRITE_FAILED
    return 0


def _wall(config: AppConfig, deps: Deps) -> FlightWallClient:
    if config.flightwall is None:
        raise ConfigError("no [flightwall] table: add one before probing or syncing the wall")
    try:
        credentials = load_credentials(config.flightwall.credentials_path)
    except ValueError as error:
        raise ConfigError(str(error)) from error
    return FlightWallClient(
        deps.transport_factory(config.flightwall),
        credentials,
        now=deps.now,
        user_agent=config.flightwall.user_agent,
    )


def _reader(
    config: AppConfig,
    gateway_factory: GatewayFactory,
    *,
    lookahead_days: int | None = None,
    lookback_days: int = 0,
) -> CalendarReader:
    limits = config.calendar_limits
    return CalendarReader(
        gateway=gateway_factory(config.google.credentials_path),
        calendar_id=config.google.calendar_id,
        lookahead_days=config.service.lookahead_days if lookahead_days is None else lookahead_days,
        lookback_days=lookback_days,
        limits=CalendarLimits(
            max_pages=limits.max_pages,
            max_events=limits.max_events,
            max_field_chars=limits.max_field_chars,
            max_snapshot_bytes=limits.max_snapshot_bytes,
        ),
    )


def _atomic_private_json(path: Path, payload: MappingJson) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(
        dir=path.parent,
        prefix=f".{path.name}.",
        suffix=".tmp",
        text=False,
    )
    temporary_path = Path(temporary_name)
    try:
        os.fchmod(descriptor, 0o600)
        encoded = orjson.dumps(
            payload,
            option=orjson.OPT_INDENT_2 | orjson.OPT_SORT_KEYS | orjson.OPT_APPEND_NEWLINE,
        )
        with os.fdopen(descriptor, "wb") as output:
            output.write(encoded)
            output.flush()
            os.fsync(output.fileno())
        temporary_path.replace(path)
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
