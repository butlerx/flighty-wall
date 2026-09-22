"""Contract-independent domain values shared by service components."""

from __future__ import annotations

from dataclasses import dataclass
from enum import StrEnum
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections.abc import Mapping
    from datetime import datetime


class SnapshotAuthority(StrEnum):
    """How far a snapshot may be trusted. Only authoritative reads may drive mutations."""

    AUTHORITATIVE = "authoritative"
    NON_AUTHORITATIVE = "non_authoritative"


@dataclass(frozen=True, slots=True)
class SourceEvent:
    """One calendar event as read, with observation time kept apart from event freshness."""

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
    """A complete read of a source, or an explicit record of why the read failed."""

    authority: SnapshotAuthority
    observed_at: datetime
    events: tuple[SourceEvent, ...] = ()
    reason: str | None = None
