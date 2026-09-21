"""Contract-independent domain values shared by service components."""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass
from datetime import datetime
from enum import StrEnum


class SnapshotAuthority(StrEnum):
    AUTHORITATIVE = "authoritative"
    NON_AUTHORITATIVE = "non_authoritative"
    PROVISIONAL = "provisional"


@dataclass(frozen=True, slots=True)
class SourceEvent:
    event_id: str
    summary: str
    starts_at: datetime | None
    ends_at: datetime | None
    status: str
    updated_at: datetime | None
    observed_at: datetime
    fields: Mapping[str, object]


@dataclass(frozen=True, slots=True)
class Snapshot:
    authority: SnapshotAuthority
    observed_at: datetime
    events: tuple[SourceEvent, ...] = ()
    reason: str | None = None
