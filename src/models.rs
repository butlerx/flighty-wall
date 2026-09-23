//! Contract-independent domain values shared by service components.

use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::BTreeMap;

/// How far a snapshot may be trusted. Only authoritative reads may drive mutations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SnapshotAuthority {
    Authoritative,
    NonAuthoritative,
}

impl SnapshotAuthority {
    /// The wire spelling used in journal lines and fixtures.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Authoritative => "authoritative",
            Self::NonAuthoritative => "non_authoritative",
        }
    }
}

impl std::fmt::Display for SnapshotAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One calendar event as read, with observation time kept apart from event freshness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceEvent {
    pub event_id: String,
    pub summary: String,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub status: String,
    pub updated_at: Option<DateTime<Utc>>,
    pub observed_at: DateTime<Utc>,
    /// The raw event fields the parser may still need, keyed by their calendar name.
    pub fields: BTreeMap<String, Value>,
}

/// A complete read of a source, or an explicit record of why the read failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub authority: SnapshotAuthority,
    pub observed_at: DateTime<Utc>,
    pub events: Vec<SourceEvent>,
    pub reason: Option<String>,
}

impl Snapshot {
    /// A trusted, complete read.
    #[must_use]
    pub fn authoritative(observed_at: DateTime<Utc>, events: Vec<SourceEvent>) -> Self {
        Self {
            authority: SnapshotAuthority::Authoritative,
            observed_at,
            events,
            reason: None,
        }
    }

    /// A read that must not drive mutations, with the reason it fell short.
    #[must_use]
    pub fn failed(observed_at: DateTime<Utc>, reason: impl Into<String>) -> Self {
        Self {
            authority: SnapshotAuthority::NonAuthoritative,
            observed_at,
            events: Vec::new(),
            reason: Some(reason.into()),
        }
    }

    /// Whether this snapshot may be used to decide writes.
    #[must_use]
    pub const fn is_authoritative(&self) -> bool {
        matches!(self.authority, SnapshotAuthority::Authoritative)
    }
}
