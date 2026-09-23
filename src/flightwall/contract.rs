//! The captured contract as values: the document fingerprint, tracked-flight entries, the
//! snapshot a read produces, and the vocabulary of ways a read or write can fail.

use super::transport::TransportError;
use crate::models::SnapshotAuthority;
use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};
use std::{collections::BTreeSet, fmt};

pub const CONFIGURATION_PATH: &str = "/configuration";

/// The app's limit. The server stored ten when asked; what the wall then shows is undefined.
pub const MAX_TRACKED_FLIGHTS: usize = 5;

pub const FINGERPRINT_MODEL: &str = "mini-v1";

// Any change to these three sets means the contract moved; the client then refuses to write.
const DOCUMENT_KEYS: [&str; 3] = ["display_config", "request_config", "version"];
const DOCUMENT_KEYS_AFTER_WRITE: [&str; 4] =
    ["display_config", "meta", "request_config", "version"];
const TRACKED_FLIGHT_KEYS: [&str; 4] = [
    "created_at",
    "flight_number",
    "show_distance_travelled",
    "show_metrics",
];

fn key_set(keys: &[&str]) -> BTreeSet<String> {
    keys.iter().map(|key| (*key).to_owned()).collect()
}

/// One entry in `request_config.tracked_flights`, exactly as the app writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedFlight {
    pub flight_number: String,
    pub created_at: String,
    pub show_distance_travelled: bool,
    pub show_metrics: bool,
}

impl TrackedFlight {
    /// Build an entry the way the app does for a flight added right now.
    #[must_use]
    pub fn new(flight_number: impl Into<String>, created_at: DateTime<Utc>) -> Self {
        Self {
            flight_number: flight_number.into(),
            created_at: rfc3339_millis(created_at),
            show_distance_travelled: true,
            show_metrics: true,
        }
    }

    /// The JSON object the wall expects for this entry.
    #[must_use]
    pub fn as_payload(&self) -> Value {
        json!({
            "flight_number": self.flight_number,
            "created_at": self.created_at,
            "show_distance_travelled": self.show_distance_travelled,
            "show_metrics": self.show_metrics,
        })
    }
}

/// The three shape facts that identify the captured contract.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fingerprint {
    pub model: Option<String>,
    pub top_level_keys: BTreeSet<String>,
    pub tracked_flight_keys: BTreeSet<String>,
}

impl Fingerprint {
    /// Why this document is not the captured contract, or `None` if it is.
    #[must_use]
    pub fn drift(&self) -> Option<String> {
        if self.model.as_deref() != Some(FINGERPRINT_MODEL) {
            let model = self.model.as_deref().unwrap_or("missing");
            return Some(drift_reason(&format!("model={model}")));
        }
        let expected = key_set(&DOCUMENT_KEYS);
        if self.top_level_keys != expected
            && self.top_level_keys != key_set(&DOCUMENT_KEYS_AFTER_WRITE)
        {
            return Some(unexpected_keys(
                "top-level",
                &self.top_level_keys,
                &expected,
            ));
        }
        let expected = key_set(&TRACKED_FLIGHT_KEYS);
        if !self.tracked_flight_keys.is_empty() && self.tracked_flight_keys != expected {
            return Some(unexpected_keys(
                "tracked_flights",
                &self.tracked_flight_keys,
                &expected,
            ));
        }
        None
    }
}

fn unexpected_keys(field: &str, actual: &BTreeSet<String>, expected: &BTreeSet<String>) -> String {
    let unexpected: Vec<&str> = actual
        .symmetric_difference(expected)
        .map(String::as_str)
        .collect();
    drift_reason(&format!("{field}={}", unexpected.join(",")))
}

fn drift_reason(detail: &str) -> String {
    WallFailure::ContractDrift.with_detail(detail)
}

/// A complete read of the configuration, or an explicit record of why it failed.
///
/// `document` is the raw configuration and carries the owner's home coordinates; it stays
/// out of `Debug`, out of equality, and out of logs. It exists only so a write can send it
/// back unchanged.
#[derive(Clone)]
pub struct WallSnapshot {
    pub authority: SnapshotAuthority,
    pub observed_at: DateTime<Utc>,
    pub tracked_flights: Vec<TrackedFlight>,
    pub fingerprint: Fingerprint,
    pub reason: Option<String>,
    document: Option<Value>,
}

impl WallSnapshot {
    /// The failure record for a read that cannot drive any write.
    #[must_use]
    pub fn non_authoritative(observed_at: DateTime<Utc>, reason: impl Into<String>) -> Self {
        Self {
            authority: SnapshotAuthority::NonAuthoritative,
            observed_at,
            tracked_flights: Vec::new(),
            fingerprint: Fingerprint::default(),
            reason: Some(reason.into()),
            document: None,
        }
    }

    /// A read that parsed and matched the fingerprint.
    #[must_use]
    pub fn authoritative(
        observed_at: DateTime<Utc>,
        tracked_flights: Vec<TrackedFlight>,
        fingerprint: Fingerprint,
        document: Value,
    ) -> Self {
        Self {
            authority: SnapshotAuthority::Authoritative,
            observed_at,
            tracked_flights,
            fingerprint,
            reason: None,
            document: Some(document),
        }
    }

    /// Whether this snapshot may be written against.
    #[must_use]
    pub const fn is_authoritative(&self) -> bool {
        matches!(self.authority, SnapshotAuthority::Authoritative)
    }

    /// The raw document, for the write path only: it carries the owner's coordinates.
    pub(super) fn document(&self) -> Option<&Value> {
        self.document.as_ref()
    }

    /// The tracked flight numbers in wall order.
    #[must_use]
    pub fn flight_numbers(&self) -> Vec<&str> {
        self.tracked_flights
            .iter()
            .map(|flight| flight.flight_number.as_str())
            .collect()
    }
}

impl fmt::Debug for WallSnapshot {
    /// Show the outcome and flight numbers, never the document.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WallSnapshot")
            .field("authority", &self.authority)
            .field("observed_at", &self.observed_at)
            .field("tracked", &self.flight_numbers())
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}

impl PartialEq for WallSnapshot {
    /// Two snapshots are equal when they would drive the same decision; the document is
    /// an implementation detail of the write path.
    fn eq(&self, other: &Self) -> bool {
        self.authority == other.authority
            && self.observed_at == other.observed_at
            && self.tracked_flights == other.tracked_flights
            && self.fingerprint == other.fingerprint
            && self.reason == other.reason
    }
}

impl Eq for WallSnapshot {}

/// What the client knows about a write after one POST.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WriteOutcome {
    /// The server answered 200 and echoed the document.
    Applied,
    /// The server answered with an error; nothing changed.
    Rejected,
    /// The request did not complete. A full body may have applied; compare the re-read.
    Unknown,
}

impl WriteOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Rejected => "rejected",
            Self::Unknown => "unknown",
        }
    }
}

/// Why a read or write could not be trusted. Rendered as `<value>:<detail>` in reasons.
///
/// Kept as strings on the wire so `WallSnapshot::reason` matches the calendar side and
/// reads well in the journal; the enum stops typos and lists the vocabulary in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WallFailure {
    /// 401. Detail is the server's `errors[0].code` (1101 missing, 1102 invalid).
    CredentialsRejected,
    /// 403 from Cloudflare. Detail is `cloudflare_<error_code>`; never retry.
    Blocked,
    Forbidden,
    RateLimited,
    ServerError,
    UnexpectedStatus,
    /// Transport-level failure. Detail is the transport's kind; a write may still have applied.
    RequestFailed,
    ResponseNotObject,
    /// The document does not match the captured fingerprint. Detail names the field.
    ContractDrift,
}

impl WallFailure {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CredentialsRejected => "flightwall_credentials_rejected",
            Self::Blocked => "flightwall_blocked",
            Self::Forbidden => "flightwall_forbidden",
            Self::RateLimited => "flightwall_rate_limited",
            Self::ServerError => "flightwall_server_error",
            Self::UnexpectedStatus => "flightwall_unexpected_status",
            Self::RequestFailed => "flightwall_request_failed",
            Self::ResponseNotObject => "flightwall_response_not_object",
            Self::ContractDrift => "flightwall_contract_drift",
        }
    }

    /// Render as the reason string carried on snapshots and results.
    #[must_use]
    pub fn with_detail(self, detail: impl fmt::Display) -> String {
        format!("{}:{detail}", self.as_str())
    }
}

impl fmt::Display for WallFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The outcome of one replace, plus the fresh read the caller must reconcile against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteResult {
    pub outcome: WriteOutcome,
    pub snapshot: WallSnapshot,
    pub reason: Option<String>,
}

/// Why the client would not even attempt a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WriteRefused {
    #[error("refusing to write against a non-authoritative snapshot")]
    NonAuthoritative,
    #[error("the wall tracks at most {MAX_TRACKED_FLIGHTS} flights; refusing to send {0}")]
    TooMany(usize),
}

pub(super) fn snapshot_from_document(body: Value, observed_at: DateTime<Utc>) -> WallSnapshot {
    match inspect(&body) {
        Err(reason) => WallSnapshot::non_authoritative(observed_at, reason),
        Ok((flights, fingerprint)) => {
            WallSnapshot::authoritative(observed_at, flights, fingerprint, body)
        }
    }
}

/// Pull the tracked flights and the fingerprint out of a document, or say why not.
fn inspect(body: &Value) -> Result<(Vec<TrackedFlight>, Fingerprint), String> {
    let document = body
        .as_object()
        .ok_or_else(|| WallFailure::ResponseNotObject.as_str().to_owned())?;
    let request_config = document
        .get("request_config")
        .and_then(Value::as_object)
        .ok_or_else(|| drift_reason("top-level=request_config"))?;
    let raw_flights = request_config
        .get("tracked_flights")
        .and_then(Value::as_array)
        .ok_or_else(|| drift_reason("tracked_flights=not-a-list"))?;

    let (flights, entry_keys) = raw_flights.iter().try_fold(
        (Vec::new(), BTreeSet::new()),
        |(mut flights, mut keys), raw| {
            let entry = raw
                .as_object()
                .ok_or_else(|| drift_reason("tracked_flights=not-an-object"))?;
            keys.extend(entry.keys().cloned());
            flights.push(
                tracked_flight(entry).ok_or_else(|| drift_reason("tracked_flights=field-types"))?,
            );
            Ok::<_, String>((flights, keys))
        },
    )?;

    let fingerprint = Fingerprint {
        model: document
            .get("display_config")
            .and_then(Value::as_object)
            .and_then(|display| display.get("model"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        top_level_keys: document.keys().cloned().collect(),
        tracked_flight_keys: entry_keys,
    };
    match fingerprint.drift() {
        Some(reason) => Err(reason),
        None => Ok((flights, fingerprint)),
    }
}

fn tracked_flight(entry: &Map<String, Value>) -> Option<TrackedFlight> {
    Some(TrackedFlight {
        flight_number: entry.get("flight_number")?.as_str()?.to_owned(),
        created_at: entry.get("created_at")?.as_str()?.to_owned(),
        show_distance_travelled: entry.get("show_distance_travelled")?.as_bool()?,
        show_metrics: entry.get("show_metrics")?.as_bool()?,
    })
}

/// Turn a non-200 answer into a [`WallFailure`] reason; never includes the key.
pub(super) fn classify_status(status: u16, body: &Value) -> Option<String> {
    match status {
        200 => None,
        401 => Some(WallFailure::CredentialsRejected.with_detail(first_error_code(body))),
        403 if body.get("cloudflare_error") == Some(&Value::Bool(true)) => {
            let code = scalar_or_unknown(body.get("error_code"));
            Some(WallFailure::Blocked.with_detail(format!("cloudflare_{code}")))
        }
        403 => Some(WallFailure::Forbidden.as_str().to_owned()),
        429 => Some(WallFailure::RateLimited.as_str().to_owned()),
        500.. => Some(WallFailure::ServerError.with_detail(status)),
        _ => Some(WallFailure::UnexpectedStatus.with_detail(status)),
    }
}

fn first_error_code(body: &Value) -> String {
    body.get("errors")
        .and_then(Value::as_array)
        .and_then(|errors| errors.first())
        .and_then(|first| first.get("code"))
        .and_then(Value::as_i64)
        .map_or_else(|| "unknown".to_owned(), |code| code.to_string())
}

/// Render a JSON number or string bare; anything else is `unknown`.
fn scalar_or_unknown(value: Option<&Value>) -> String {
    match value {
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::String(text)) => text.clone(),
        _ => "unknown".to_owned(),
    }
}

pub(super) fn request_failed(error: &TransportError) -> String {
    WallFailure::RequestFailed.with_detail(error.kind())
}

/// Format the way the app does: millisecond precision, trailing `Z`.
fn rfc3339_millis(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%dT%H:%M:%S.%3fZ").to_string()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::test_support::now;
    use super::*;

    #[test]
    fn tracked_flight_new_matches_the_app_shape() {
        let flight = TrackedFlight::new("VY8721", now());

        assert_eq!(
            flight.as_payload(),
            json!({
                "flight_number": "VY8721",
                "created_at": "2026-09-22T12:00:00.000Z",
                "show_distance_travelled": true,
                "show_metrics": true,
            })
        );
    }

    #[test]
    fn tracked_flight_created_at_truncates_to_milliseconds() {
        let precise = now() + chrono::Duration::microseconds(123_999);
        assert_eq!(
            TrackedFlight::new("X1", precise).created_at,
            "2026-09-22T12:00:00.123Z"
        );
    }
}
