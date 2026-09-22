---
title: "chore: Adopt click and pydantic at the CLI and config boundaries"
type: chore
status: active
date: 2026-09-22
---

# chore: Adopt click and pydantic at the CLI and config boundaries

## Overview

The repository already follows current Python packaging and tooling practice:
`src/` layout, PEP 621 `pyproject.toml`, `uv.lock`, `ruff` with `select = ALL`,
`mypy --strict`, `pyright strict`, `deptry`, a coverage floor, `prek` hooks, a
`mise`-driven task runner, and a CI workflow that runs the same hooks. The
tooling work is done; this plan covers the two remaining library migrations
and one deferred logging change.

Estimated effort: about 1.5 hours across two independent PRs, plus a third
small PR when the daemon lands.

## Already Complete

Recorded so the next reader does not redo it.

- Python 3.11 pinned in `.python-version`; venv resynced and tests pass on
  3.11.16 (previously ran on 3.14, so the minimum version was never tested).
- `prek` hooks installed (`pre-commit`, `commit-msg`); `tombi`, `yamlfmt`,
  `actionlint`, `zizmor`, `ruff`, `mypy`, `pyright`, `uv-lock` all run from
  `.pre-commit-config.yaml`.
- `mise.toml` + `mise.lock` pin `prek`, `tombi`, `uv`, `zizmor`. File tasks
  in `.mise/tasks/`: `sync`, `hooks`, `check`, `lint`, `lint:fix`, `test`,
  `deps`.
- `.github/workflows/ci.yml`: `lint` job via `jdx/mise-action`, `test`
  matrix 3.11 to 3.14 using `UV_PYTHON` to override the pin.
- `pyproject.toml`: coverage `fail_under = 80` and `omit = ["*/__main__.py"]`;
  dead mypy overrides and `benchmarks` ruff ignores removed; `max-complexity`
  40 to 10 (nothing fired); PEP 639 `license = "Apache-2.0"`,
  `license-files`, `authors`, `classifiers`, `[project.urls]`;
  `setuptools>=77`.
- `LICENSE` (Apache-2.0) and `NOTICE` added.
- `pyrightconfig.json` deleted; `[tool.pyright]` already covered it.
- `__version__` read from `importlib.metadata`, so the version lives only in
  `pyproject.toml`.

## Design Principles

- **Libraries at the edges, stdlib in the middle.** `click` owns argument
  parsing and terminal output. `pydantic` owns parsing the TOML into a typed
  object. `models.py` dataclasses stay as plain frozen dataclasses: they are
  internal values, not I/O.
- **Filesystem checks stay outside the model.** `require_private_file` and
  `_validate_state_parent` are side-effecting probes of the host. They run
  after `model_validate`, not inside a validator, so a config model can be
  constructed in tests without touching disk.
- **Preserve exit codes.** Config errors and inspection failures currently
  exit 2. Usage errors from `click` also exit 2. Keep both.

## Phase A: click (about 40 minutes)

One PR. Tests must pass at every step.

### A1. Add the dependency

```bash
uv add "click>=8.1,<9"
```

### A2. Rewrite `src/flighty_wall/cli.py`

Replace `argparse` with a `click.group` and one command.

| argparse today | click |
| --- | --- |
| `parser.add_subparsers(...)` + `add_parser("inspect-calendar")` | `@click.group() def cli()` + `@cli.command("inspect-calendar")` |
| `--config`, `--output` with `type=Path, required=True` | `@click.option(..., type=click.Path(path_type=Path), required=True)` |
| `--redact-term` with `action="append", default=[]` | `@click.option(..., multiple=True)` (arrives as a tuple; drop the `tuple()` call) |
| `--lookahead-days`, `--lookback-days` with `type=_inspection_window` | `type=click.IntRange(0, MAX_INSPECTION_WINDOW_DAYS)`; delete `_inspection_window` |
| `print(..., file=sys.stderr)` | `click.echo(..., err=True)` |
| `print(...)` | `click.echo(...)` |
| `return 2` | `ctx.exit(2)` |

### A3. Dependency injection through `ctx.obj`

`run()` currently takes `gateway_factory` and `now` as keyword arguments so
tests can inject fakes. Replace with a small frozen dataclass and
`@click.pass_obj`:

```python
@dataclass(frozen=True, slots=True)
class Deps:
    gateway_factory: GatewayFactory = build_calendar_gateway
    now: Clock = lambda: datetime.now(UTC)

@click.group()
@click.pass_context
def cli(ctx: click.Context) -> None:
    ctx.obj = ctx.obj or Deps()
```

Delete `run()`.

### A4. Entry points

- `pyproject.toml` `[project.scripts]`: `flighty-wall = "flighty_wall.cli:cli"`
- `src/flighty_wall/__main__.py`: `from .cli import cli; cli()`

### A5. Tests

`tests/test_cli.py`: switch the four CLI tests to
`CliRunner().invoke(cli, [...], obj=Deps(gateway_factory=..., now=...))` and
assert on `result.exit_code`. The out-of-range test drops
`pytest.raises(SystemExit)` in favour of `result.exit_code == 2`.

### A6. Cleanup

Remove the `"src/flighty_wall/cli.py" = ["T201"]` per-file-ignore from
`pyproject.toml` once no `print` remains.

## Phase B: pydantic (about 45 minutes)

One PR, independent of Phase A.

### B1. Add the dependency and mypy plugin

```bash
uv add "pydantic>=2.7,<3"
```

`pyproject.toml` `[tool.mypy]`: add `plugins = ["pydantic.mypy"]`.

The ruff `runtime-evaluated-base-classes = ["pydantic.BaseModel"]` entry is
already present.

### B2. Replace the config model

`src/flighty_wall/config.py`: delete the `AppConfig` dataclass and the
helpers `_table`, `_required_string`, `_path_value`, `_integer`, `_boolean`,
`_validate_ranges`. Replace with nested frozen models that mirror the TOML
tables:

```python
from typing import Annotated
from pydantic import AfterValidator, BaseModel, ConfigDict, Field, field_validator

UserPath = Annotated[Path, AfterValidator(Path.expanduser)]

_STRICT = ConfigDict(frozen=True, strict=True, extra="forbid", str_strip_whitespace=True)

class Google(BaseModel):
    model_config = _STRICT
    calendar_id: str = Field(min_length=1)
    credentials_path: UserPath

    @field_validator("calendar_id")
    @classmethod
    def _not_primary(cls, value: str) -> str:
        if value.casefold() == "primary":
            raise ValueError("google.calendar_id must name the dedicated calendar, not primary")
        return value

class Service(BaseModel):
    model_config = _STRICT
    poll_interval_seconds: int = Field(120, ge=30, le=86_400)
    lookahead_days: int = Field(7, ge=1, le=30)
    dry_run: bool = True

class Storage(BaseModel):
    model_config = _STRICT
    state_path: UserPath

class CalendarLimits(BaseModel):
    model_config = _STRICT
    max_pages: int = Field(10, ge=1, le=100)
    max_events: int = Field(500, ge=1, le=10_000)
    max_field_chars: int = Field(8_192, ge=256, le=1_000_000)
    max_snapshot_bytes: int = Field(1_048_576, ge=1_024, le=100_000_000)

class AppConfig(BaseModel):
    model_config = _STRICT
    google: Google
    service: Service = Service()
    storage: Storage
    calendar_limits: CalendarLimits = CalendarLimits()
```

Behaviour notes:

- `strict=True` keeps the current rejection of `true` for an `int` field.
  `str` to `Path` is still allowed in strict mode.
- `extra="forbid"` is new: a typo'd key such as `lookahead_day` now fails
  instead of being silently ignored. This is the desired change.
- Range bounds move from `_validate_ranges` into `Field(ge=, le=)`.

### B3. Rewire `load_config`

```python
def load_config(path: str | os.PathLike[str]) -> AppConfig:
    config_path = Path(path).expanduser()
    try:
        with config_path.open("rb") as config_file:
            raw = tomllib.load(config_file)
    except FileNotFoundError as error:
        raise ConfigError(f"configuration file does not exist: {config_path}") from error
    except tomllib.TOMLDecodeError as error:
        raise ConfigError(f"invalid TOML in {config_path}: {error}") from error

    try:
        config = AppConfig.model_validate(raw)
    except ValidationError as error:
        raise ConfigError(str(error)) from error

    _validate_state_parent(config.storage.state_path)
    require_private_file(config.google.credentials_path)
    return config
```

`require_private_file` and `_validate_state_parent` are unchanged.

### B4. Update callers

`src/flighty_wall/cli.py` `_reader`: six attribute reads change from flat to
nested (`config.calendar_id` becomes `config.google.calendar_id`,
`config.max_pages` becomes `config.calendar_limits.max_pages`, and so on).
Check `grep -rn 'config\.' src` for any other consumers added since this plan
was written.

### B5. Tests

`tests/test_config.py`: the `match=` strings (`"poll_interval_seconds"`,
`"dedicated calendar"`, `"state directory"`, `"0600"`) survive because
pydantic error messages include the field path. Update attribute paths in
`test_load_config_applies_safe_defaults` and the repr test. Add one test that
an unknown key raises `ConfigError` (covers `extra="forbid"`).

If ruff's `N805` fires on `_not_primary`, add
`classmethod-decorators = ["pydantic.field_validator"]` under
`[tool.ruff.lint.pep8-naming]`.

## Out of Scope

- `models.py` dataclasses (`SourceEvent`, `Snapshot`, `SnapshotAuthority`):
  internal values, stay as dataclasses.
- `calendar.py` Google event parsing: its `isinstance` chain encodes the
  deliberate "ambiguous means non-authoritative" rule. A
  `GoogleEvent(BaseModel, extra="allow")` could replace it later, but only
  once the parser contract is stable.
- Switching build backend (setuptools works; no gain).

## Deferred: logging (when the daemon lands)

`cli.py` writes status with `print` / `click.echo`. That is correct for a
one-shot diagnostic command. When the long-running poll loop is added:

1. Add a `logging` module with `logging.getLogger(__name__)` per module.
2. Configure once in the daemon entry point (stderr handler, level from
   config, `systemd` picks it up via journal).
3. Follow the privacy rule already in the origin plan: log event IDs,
   normalized flight identifiers, action types, error classes. Never log
   credentials, reservation codes, seat numbers, descriptions, or Friend
   names by default.

The `inspect-calendar` command keeps `click.echo`; it is a user-facing CLI,
not a service.

## Verification

Each phase ends with:

```bash
mise run check
```

which runs `prek` (ruff, mypy, pyright, tombi, yamlfmt, actionlint, zizmor),
`pytest --cov` (floor 80%), and `deptry`.
