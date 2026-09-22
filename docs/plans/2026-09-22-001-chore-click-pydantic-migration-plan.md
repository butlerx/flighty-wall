---
title: "chore: Adopt click and pydantic at the CLI and config boundaries"
type: chore
status: done
date: 2026-09-22
completed: 2026-09-22
---

# chore: Adopt click and pydantic at the CLI and config boundaries

**Done.** Kept as the record of what landed and which conventions it fixed. There is no
remaining work in this plan; the one deferred item moved to U7 of the origin plan.

## What landed

| Phase | Commit | Result |
| --- | --- | --- |
| A — click replaces argparse | `097d5a0` | `cli.py` is a click group; `Deps` frozen dataclass through `ctx.obj`; `[project.scripts]` points at `flighty_wall.cli:cli` |
| B — pydantic replaces hand-rolled config | `3e3122c` | nested `AppConfig` with `strict=True`, `extra="forbid"`, range bounds in `Field`; six validation helpers and `_validate_ranges` deleted |
| Tooling groundwork | `9ba2b5a`, `3767bc7`, `6a16c27`, `64f0f73` | `uv.lock`, `mise.toml` tasks, `prek` hooks, ruff `ALL`, mypy/pyright strict, `deptry`, coverage floor 80, CI via `jdx/mise-action`, Apache-2.0 |

`mise run check` passed on each commit: 102 tests, 94% coverage.

## Conventions this fixed

These are now load-bearing; later units follow them (also listed under *Established Code and
Patterns* in the origin plan).

- **Libraries at the edges, stdlib in the middle.** `click` owns argument parsing and terminal
  output. `pydantic` owns TOML → typed object. `models.py` dataclasses stay frozen dataclasses.
- **Filesystem checks stay outside the model.** `require_private_file` and the state-parent
  probe run after `model_validate`, so a config can be built in tests without touching disk.
- **Exit code 2** for config errors, usage errors, and non-authoritative results. Nothing is
  written on exit 2.
- **New settings are a new nested table**, never a flat key, so `extra="forbid"` keeps catching
  typos.

## Deviations from the plan as written

- The limits model is `Limits`, not `CalendarLimits`, because `cli.py` also imports
  `calendar.CalendarLimits` and two identically named types in one module would be resolved
  wrongly later.
- Paths use a `BeforeValidator`, not `AfterValidator`: under `strict=True` pydantic refuses to
  coerce `str` → `Path` at all, so `~` expansion has to run before validation. Non-string,
  non-path input passes through untouched and is still reported.
- `Field(default=120, ...)`, not `Field(120, ...)`: pyright does not read the positional form
  as a default and reports every defaulted field as a missing constructor argument.

## Moved elsewhere

- **Logging** — introduce `logging` when the daemon lands, not before. Now specified in U7 of
  `docs/plans/2026-09-21-001-feat-flighty-flightwall-sync-plan.md`.
- **`GoogleEvent(BaseModel, extra="allow")`** to replace the `isinstance` chain in
  `calendar.py` — listed under *Deferred to Follow-Up Work* in the origin plan; only worth
  doing once the parser contract has been stable for a while.
