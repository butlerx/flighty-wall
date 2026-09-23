//! `FlightWall` client for the one contract the capture proved: a whole-document configuration.
//!
//! Everything here mirrors `docs/flightwall-api.md`. There is one resource,
//! `/configuration`; `GET` reads it and `POST` replaces it. Tracked flights are a list
//! inside it, keyed by `flight_number` and capped at five by the app, not the server. There
//! are no per-entry identifiers, no conditional writes, and no display mode, so the client
//! offers exactly two operations and refuses anything the captured contract did not show.

use crate::{
    config::{self, ConfigError},
    models::SnapshotAuthority,
};
use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
    time::Duration,
};

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

// ---------------------------------------------------------------------------------------
// Transport: one HTTP round-trip, expressed without any HTTP-library types so tests can
// script it.
// ---------------------------------------------------------------------------------------

/// The request did not complete; the outcome of a write is unknown.
///
/// `kind` is a short classifier (`timeout`, `connect`, `redirect_refused`) that ends up in
/// the reason string. It never carries a URL, a header, or a body.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("transport failed: {kind}")]
pub struct TransportError {
    kind: String,
}

impl TransportError {
    #[must_use]
    pub fn new(kind: impl Into<String>) -> Self {
        Self { kind: kind.into() }
    }

    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }
}

/// The only two verbs the contract has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    Get,
    Post,
}

impl Method {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
        }
    }
}

/// Request headers by lower-case name. Every name the client sends is a literal.
pub type Headers = BTreeMap<&'static str, String>;

/// One HTTP round-trip.
pub trait Transport {
    /// Send one request and return the status code and decoded JSON body.
    ///
    /// An empty or undecodable body decodes to `{}`; the status code still tells the caller
    /// what happened.
    ///
    /// # Errors
    ///
    /// [`TransportError`] when the request did not complete: no status was received.
    fn request(
        &self,
        method: Method,
        path: &str,
        headers: &Headers,
        body: Option<&Value>,
    ) -> Result<(u16, Value), TransportError>;
}

impl<T: Transport + ?Sized> Transport for &T {
    fn request(
        &self,
        method: Method,
        path: &str,
        headers: &Headers,
        body: Option<&Value>,
    ) -> Result<(u16, Value), TransportError> {
        (**self).request(method, path, headers, body)
    }
}

// ---------------------------------------------------------------------------------------
// Domain values.
// ---------------------------------------------------------------------------------------

/// The per-install key pair the app sends. Only `api_key` authorizes anything.
#[derive(Clone, PartialEq, Eq)]
pub struct FlightWallCredentials {
    api_key: String,
    user_id: String,
}

impl FlightWallCredentials {
    #[must_use]
    pub fn new(api_key: impl Into<String>, user_id: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            user_id: user_id.into(),
        }
    }

    #[must_use]
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    #[must_use]
    pub fn user_id(&self) -> &str {
        &self.user_id
    }
}

impl fmt::Debug for FlightWallCredentials {
    /// Never show the key pair, even in panics.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FlightWallCredentials(<redacted>)")
    }
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

// ---------------------------------------------------------------------------------------
// Client.
// ---------------------------------------------------------------------------------------

/// Read the configuration; replace its tracked flights. Nothing else.
pub struct FlightWallClient<T: Transport> {
    transport: T,
    credentials: FlightWallCredentials,
    clock: Box<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    headers: Headers,
}

impl<T: Transport> FlightWallClient<T> {
    /// A client that stamps snapshots with the system clock.
    pub fn new(transport: T, credentials: FlightWallCredentials, user_agent: &str) -> Self {
        Self::with_clock(transport, credentials, user_agent, Utc::now)
    }

    /// A client with an injected clock, for tests and replay.
    pub fn with_clock(
        transport: T,
        credentials: FlightWallCredentials,
        user_agent: &str,
        clock: impl Fn() -> DateTime<Utc> + Send + Sync + 'static,
    ) -> Self {
        let headers = Headers::from([
            ("accept", "application/json".to_owned()),
            ("user-agent", user_agent.to_owned()),
            ("x-api-key", credentials.api_key.clone()),
            ("x-user-id", credentials.user_id.clone()),
        ]);
        Self {
            transport,
            credentials,
            clock: Box::new(clock),
            headers,
        }
    }

    /// Read the configuration. Authoritative only if it parses and matches the fingerprint.
    pub fn read(&self) -> WallSnapshot {
        let observed_at = (self.clock)();
        match self
            .transport
            .request(Method::Get, CONFIGURATION_PATH, &self.headers, None)
        {
            Err(error) => WallSnapshot::non_authoritative(observed_at, request_failed(&error)),
            Ok((status, body)) => match classify_status(status, &body) {
                Some(failure) => WallSnapshot::non_authoritative(observed_at, failure),
                None => snapshot_from_document(body, observed_at),
            },
        }
    }

    /// POST the snapshot's document with only `tracked_flights` changed, then re-read.
    ///
    /// The document is copied from the snapshot the caller planned against, so the owner's
    /// display and area settings go back exactly as they were read. Writes are last-writer-
    /// wins on the server; keeping the read-to-write window to this one call is the only
    /// mitigation the contract allows.
    ///
    /// # Errors
    ///
    /// [`WriteRefused`] before any request is made: the snapshot is not authoritative, or
    /// more than [`MAX_TRACKED_FLIGHTS`] flights were asked for.
    pub fn replace_tracked_flights(
        &self,
        snapshot: &WallSnapshot,
        flights: &[TrackedFlight],
    ) -> Result<WriteResult, WriteRefused> {
        let document = snapshot
            .document
            .as_ref()
            .filter(|_| snapshot.is_authoritative())
            .ok_or(WriteRefused::NonAuthoritative)?;
        if flights.len() > MAX_TRACKED_FLIGHTS {
            return Err(WriteRefused::TooMany(flights.len()));
        }

        let document = self.document_for_write(document, flights);
        let mut headers = self.headers.clone();
        headers.insert("content-type", "application/json".to_owned());

        let result = match self.transport.request(
            Method::Post,
            CONFIGURATION_PATH,
            &headers,
            Some(&document),
        ) {
            // A full body that reached the server applies even if the response never arrived.
            Err(error) => WriteResult {
                outcome: WriteOutcome::Unknown,
                snapshot: self.read(),
                reason: Some(request_failed(&error)),
            },
            Ok((status, body)) => match classify_status(status, &body) {
                Some(failure) => WriteResult {
                    outcome: WriteOutcome::Rejected,
                    snapshot: snapshot.clone(),
                    reason: Some(failure),
                },
                None => WriteResult {
                    outcome: WriteOutcome::Applied,
                    snapshot: self.read(),
                    reason: None,
                },
            },
        };
        Ok(result)
    }

    /// The read document with `meta` dropped, `tracked_flights` replaced, and `userId` added.
    fn document_for_write(&self, document: &Value, flights: &[TrackedFlight]) -> Value {
        let mut document = document.clone();
        let Some(root) = document.as_object_mut() else {
            return document;
        };
        root.remove("meta");

        let payloads = Value::Array(flights.iter().map(TrackedFlight::as_payload).collect());
        let request_config = root
            .entry("request_config")
            .or_insert_with(|| Value::Object(Map::new()));
        if !request_config.is_object() {
            *request_config = Value::Object(Map::new());
        }
        if let Some(request_config) = request_config.as_object_mut() {
            request_config.insert("tracked_flights".to_owned(), payloads);
        }

        root.insert(
            "userId".to_owned(),
            Value::String(self.credentials.user_id.clone()),
        );
        document
    }
}

// ---------------------------------------------------------------------------------------
// Production transport.
// ---------------------------------------------------------------------------------------

/// The production transport: HTTPS, normal certificate validation, HTTP/1.1, no redirects.
pub struct UreqTransport {
    agent: ureq::Agent,
    base_url: String,
}

impl UreqTransport {
    /// A transport pinned to one host.
    #[must_use]
    pub fn new(host: &str, timeout: Duration) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .max_redirects(0)
            .http_status_as_error(false)
            .build();
        Self {
            agent: config.new_agent(),
            base_url: format!("https://{host}"),
        }
    }
}

impl fmt::Debug for UreqTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UreqTransport")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl Transport for UreqTransport {
    fn request(
        &self,
        method: Method,
        path: &str,
        headers: &Headers,
        body: Option<&Value>,
    ) -> Result<(u16, Value), TransportError> {
        let url = format!("{}{path}", self.base_url);
        let response = match method {
            Method::Get => {
                let request = headers
                    .iter()
                    .fold(self.agent.get(&url), |request, (name, value)| {
                        request.header(*name, value)
                    });
                request.call()
            }
            Method::Post => {
                let request = headers
                    .iter()
                    .fold(self.agent.post(&url), |request, (name, value)| {
                        request.header(*name, value)
                    });
                let bytes = body
                    .map(serde_json::to_vec)
                    .transpose()
                    .map_err(|_| TransportError::new("encode"))?
                    .unwrap_or_default();
                request.send(&bytes[..])
            }
        }
        .map_err(|error| TransportError::new(transport_kind(&error)))?;

        if response.status().is_redirection() {
            return Err(TransportError::new("redirect_refused"));
        }
        let status = response.status().as_u16();
        let bytes = response
            .into_body()
            .read_to_vec()
            .map_err(|_| TransportError::new("body"))?;
        let decoded = if bytes.is_empty() {
            Value::Object(Map::new())
        } else {
            serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::Object(Map::new()))
        };
        Ok((status, decoded))
    }
}

/// A short, log-safe classifier for a transport failure.
fn transport_kind(error: &ureq::Error) -> &'static str {
    match error {
        ureq::Error::Timeout(_) => "timeout",
        ureq::Error::ConnectionFailed => "connect",
        ureq::Error::HostNotFound => "dns",
        ureq::Error::Io(_) => "io",
        ureq::Error::TooManyRedirects | ureq::Error::RedirectFailed => "redirect_refused",
        ureq::Error::BodyExceedsLimit(_) => "body_too_large",
        _ => "request",
    }
}

// ---------------------------------------------------------------------------------------
// Credentials file.
// ---------------------------------------------------------------------------------------

/// Why the `FlightWall` credential file could not be used.
#[derive(Debug, thiserror::Error)]
pub enum CredentialsError {
    #[error(transparent)]
    NotPrivate(#[from] ConfigError),
    #[error("flightwall credential file cannot be read {path}: {source}")]
    Unreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("flightwall credential file is not valid TOML: {0}")]
    InvalidToml(PathBuf),
    #[error("flightwall credential file is missing {field}: {path}")]
    MissingField { path: PathBuf, field: &'static str },
}

/// Read the per-install key pair from a private TOML file with `api_key` and `user_id`.
///
/// # Errors
///
/// [`CredentialsError`] when the file is missing, not private, not TOML, or lacks either
/// key. Messages name the path and the field, never the values.
pub fn load_credentials(path: &Path) -> Result<FlightWallCredentials, CredentialsError> {
    config::require_private_file(path)?;
    let body = std::fs::read_to_string(path).map_err(|source| CredentialsError::Unreadable {
        path: path.to_owned(),
        source,
    })?;
    let table: toml::Table = body
        .parse()
        .map_err(|_| CredentialsError::InvalidToml(path.to_owned()))?;

    let field = |name: &'static str| {
        table
            .get(name)
            .and_then(toml::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or(CredentialsError::MissingField {
                path: path.to_owned(),
                field: name,
            })
    };
    Ok(FlightWallCredentials::new(
        field("api_key")?,
        field("user_id")?,
    ))
}

// ---------------------------------------------------------------------------------------
// Pure helpers.
// ---------------------------------------------------------------------------------------

fn snapshot_from_document(body: Value, observed_at: DateTime<Utc>) -> WallSnapshot {
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
fn classify_status(status: u16, body: &Value) -> Option<String> {
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

fn request_failed(error: &TransportError) -> String {
    WallFailure::RequestFailed.with_detail(error.kind())
}

/// Format the way the app does: millisecond precision, trailing `Z`.
fn rfc3339_millis(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%dT%H:%M:%S.%3fZ").to_string()
}

#[cfg(test)]
mod tests {
    //! Contract tests for the `FlightWall` client, driven by the captured fixtures.
    //! Contract tests for the `FlightWall` client, driven by the captured fixtures.
    //!
    //! Every request/response shape here comes from `tests/fixtures/flightwall/`; nothing is
    //! invented. The transport is a fake so no test touches the network.

    use super::*;
    use crate::models::SnapshotAuthority;
    use chrono::{DateTime, TimeZone, Utc};
    use serde_json::{Value, json};
    use std::{cell::RefCell, collections::VecDeque, fs, os::unix::fs::PermissionsExt, path::Path};
    use tempfile::TempDir;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 22, 12, 0, 0).unwrap()
    }

    fn credentials() -> FlightWallCredentials {
        FlightWallCredentials::new("k".repeat(43), format!("fw_ios_{}", "u".repeat(22)))
    }

    fn fixture(name: &str) -> Value {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/flightwall")
            .join(name);
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    fn fixture_document(name: &str, side: &str) -> Value {
        fixture(name)[side]["body"]["json"].clone()
    }

    fn response_document(name: &str) -> Value {
        fixture_document(name, "response")
    }

    // ---------------------------------------------------------------------------------------
    // Scripted transport: each call pops the next (status, body) or fails.
    // ---------------------------------------------------------------------------------------

    enum Step {
        Reply(u16, Value),
        Fail(&'static str),
    }

    #[derive(Debug, Clone)]
    struct Call {
        method: Method,
        path: String,
        headers: Headers,
        body: Option<Value>,
    }

    struct FakeTransport {
        responses: RefCell<VecDeque<Step>>,
        calls: RefCell<Vec<Call>>,
    }

    impl FakeTransport {
        fn scripted(steps: impl IntoIterator<Item = Step>) -> Self {
            Self {
                responses: RefCell::new(steps.into_iter().collect()),
                calls: RefCell::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.borrow().clone()
        }

        fn methods(&self) -> Vec<Method> {
            self.calls.borrow().iter().map(|call| call.method).collect()
        }
    }

    impl Transport for FakeTransport {
        fn request(
            &self,
            method: Method,
            path: &str,
            headers: &Headers,
            body: Option<&Value>,
        ) -> Result<(u16, Value), TransportError> {
            self.calls.borrow_mut().push(Call {
                method,
                path: path.to_owned(),
                headers: headers.clone(),
                body: body.cloned(),
            });
            match self.responses.borrow_mut().pop_front() {
                None => panic!("unexpected {method:?} {path}"),
                Some(Step::Fail(kind)) => Err(TransportError::new(kind)),
                Some(Step::Reply(status, body)) => Ok((status, body)),
            }
        }
    }

    fn client(transport: &FakeTransport) -> FlightWallClient<&FakeTransport> {
        FlightWallClient::with_clock(transport, credentials(), "TheFlightWall/1 test", now)
    }

    fn ok(body: Value) -> Step {
        Step::Reply(200, body)
    }

    fn invalid_key() -> Step {
        Step::Reply(
            401,
            json!({"success": false, "errors": [{"code": 1102, "message": "Invalid API key"}]}),
        )
    }

    // ---------------------------------------------------------------------------------------
    // Reads.
    // ---------------------------------------------------------------------------------------

    #[test]
    fn read_captured_configuration_is_authoritative() {
        let transport = FakeTransport::scripted([ok(response_document("get-configuration.json"))]);

        let snapshot = client(&transport).read();

        assert_eq!(snapshot.authority, SnapshotAuthority::Authoritative);
        assert_eq!(snapshot.observed_at, now());
        assert_eq!(
            snapshot.fingerprint.model.as_deref(),
            Some(FINGERPRINT_MODEL)
        );
        assert_eq!(snapshot.flight_numbers(), ["EI61"]);
        assert!(snapshot.tracked_flights[0].show_metrics);
        let call = &transport.calls()[0];
        assert_eq!(
            (call.method, call.path.as_str(), &call.body),
            (Method::Get, "/configuration", &None)
        );
        assert_eq!(call.headers["x-api-key"], credentials().api_key());
        assert_eq!(call.headers["x-user-id"], credentials().user_id());
        assert!(call.headers["user-agent"].starts_with("TheFlightWall/"));
    }

    #[test]
    fn read_with_no_tracked_flights_is_authoritative_and_empty() {
        let mut document = response_document("get-configuration.json");
        document["request_config"]["tracked_flights"] = json!([]);
        let transport = FakeTransport::scripted([ok(document)]);

        let snapshot = client(&transport).read();

        assert_eq!(snapshot.authority, SnapshotAuthority::Authoritative);
        assert!(snapshot.tracked_flights.is_empty());
    }

    /// A named mutation of the captured document and the drift fragment it must produce.
    type DriftCase = (&'static str, fn(&mut Value), &'static str);

    #[test]
    fn fingerprint_drift_is_non_authoritative() {
        let cases: [DriftCase; 5] = [
            (
                "model",
                |d| d["display_config"]["model"] = json!("mini-v2"),
                "model",
            ),
            (
                "drop request_config",
                |d| {
                    d.as_object_mut().unwrap().remove("request_config");
                },
                "top-level",
            ),
            (
                "add top-level key",
                |d| d["surprise"] = json!(1),
                "top-level",
            ),
            (
                "add entry key",
                |d| d["request_config"]["tracked_flights"][0]["id"] = json!("abc"),
                "tracked_flights",
            ),
            (
                "drop entry key",
                |d| {
                    d["request_config"]["tracked_flights"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("created_at");
                },
                "tracked_flights",
            ),
        ];
        for (label, mutate, fragment) in cases {
            let mut document = response_document("get-configuration.json");
            mutate(&mut document);
            let transport = FakeTransport::scripted([ok(document)]);

            let snapshot = client(&transport).read();

            assert_eq!(
                snapshot.authority,
                SnapshotAuthority::NonAuthoritative,
                "{label}"
            );
            let reason = snapshot
                .reason
                .as_deref()
                .unwrap_or_else(|| panic!("{label}: no reason"));
            assert!(reason.contains(fragment), "{label}: {reason}");
            assert!(
                reason.starts_with("flightwall_contract_drift:"),
                "{label}: {reason}"
            );
        }
    }

    #[test]
    fn read_401_is_a_credential_error_that_never_echoes_the_key() {
        let transport = FakeTransport::scripted([invalid_key()]);

        let snapshot = client(&transport).read();

        assert_eq!(snapshot.authority, SnapshotAuthority::NonAuthoritative);
        assert_eq!(
            snapshot.reason.as_deref(),
            Some("flightwall_credentials_rejected:1102")
        );
        assert!(!format!("{snapshot:?}").contains(credentials().api_key()));
    }

    #[test]
    fn read_cloudflare_403_is_fatal_not_retryable() {
        let transport = FakeTransport::scripted([Step::Reply(
            403,
            json!({"cloudflare_error": true, "error_code": 1010, "retryable": false}),
        )]);

        let snapshot = client(&transport).read();

        assert_eq!(snapshot.authority, SnapshotAuthority::NonAuthoritative);
        assert_eq!(
            snapshot.reason.as_deref(),
            Some("flightwall_blocked:cloudflare_1010")
        );
    }

    #[test]
    fn read_other_statuses_are_named() {
        for (status, reason) in [
            (429, "flightwall_rate_limited"),
            (503, "flightwall_server_error:503"),
            (403, "flightwall_forbidden"),
            (418, "flightwall_unexpected_status:418"),
        ] {
            let transport = FakeTransport::scripted([Step::Reply(status, json!({}))]);

            let snapshot = client(&transport).read();

            assert_eq!(
                snapshot.authority,
                SnapshotAuthority::NonAuthoritative,
                "{status}"
            );
            assert_eq!(snapshot.reason.as_deref(), Some(reason), "{status}");
        }
    }

    #[test]
    fn read_transport_failure_is_non_authoritative() {
        let transport = FakeTransport::scripted([Step::Fail("connect")]);

        let snapshot = client(&transport).read();

        assert_eq!(snapshot.authority, SnapshotAuthority::NonAuthoritative);
        assert_eq!(
            snapshot.reason.as_deref(),
            Some("flightwall_request_failed:connect")
        );
    }

    #[test]
    fn read_non_object_body_is_non_authoritative() {
        let transport = FakeTransport::scripted([ok(json!([1, 2, 3]))]);

        let snapshot = client(&transport).read();

        assert_eq!(
            snapshot.reason.as_deref(),
            Some("flightwall_response_not_object")
        );
    }

    // ---------------------------------------------------------------------------------------
    // Writes.
    // ---------------------------------------------------------------------------------------

    #[test]
    fn replace_posts_the_whole_document_touching_only_tracked_flights() {
        let before = response_document("get-configuration.json");
        let mut expected = fixture_document("post-configuration-add.json", "request");
        let after = response_document("post-configuration-add.json");
        let transport = FakeTransport::scripted([ok(before), ok(after.clone()), ok(after)]);
        let wall = client(&transport);
        let snapshot = wall.read();

        let result = wall
            .replace_tracked_flights(
                &snapshot,
                &[
                    snapshot.tracked_flights[0].clone(),
                    TrackedFlight::new("BA5", now()),
                ],
            )
            .unwrap();

        assert_eq!(result.outcome, WriteOutcome::Applied);
        assert_eq!(result.snapshot.flight_numbers(), ["EI61", "BA5"]);
        let post = &transport.calls()[1];
        assert_eq!(
            (post.method, post.path.as_str()),
            (Method::Post, "/configuration")
        );
        assert_eq!(post.headers["content-type"], "application/json");
        let mut sent = post.body.clone().expect("a POST body");
        // Byte-for-byte the same as the app's own POST, apart from the values only the app knows.
        expected["userId"] = json!(credentials().user_id());
        let expected_flights = expected["request_config"]["tracked_flights"]
            .as_array()
            .unwrap()
            .clone();
        let sent_flights = sent["request_config"]["tracked_flights"]
            .as_array_mut()
            .unwrap();
        assert_eq!(sent_flights.len(), expected_flights.len());
        for (sent_flight, expected_flight) in sent_flights.iter_mut().zip(&expected_flights) {
            sent_flight["created_at"] = expected_flight["created_at"].clone();
        }
        assert_eq!(sent, expected);
        // And the daemon's re-read after the write is the third call.
        assert_eq!(
            transport.methods(),
            [Method::Get, Method::Post, Method::Get]
        );
    }

    #[test]
    fn replace_preserves_every_non_tracked_byte_of_the_document() {
        let before = response_document("get-configuration.json");
        let transport =
            FakeTransport::scripted([ok(before.clone()), ok(before.clone()), ok(before)]);
        let wall = client(&transport);
        let snapshot = wall.read();

        wall.replace_tracked_flights(&snapshot, &[]).unwrap();

        let sent = transport.calls()[1].body.clone().expect("a POST body");
        let original = response_document("get-configuration.json");
        for key in ["display_config", "version"] {
            assert_eq!(sent[key], original[key], "{key}");
        }
        for (key, value) in original["request_config"].as_object().unwrap() {
            if key != "tracked_flights" {
                assert_eq!(&sent["request_config"][key], value, "request_config.{key}");
            }
        }
        assert_eq!(sent["request_config"]["tracked_flights"], json!([]));
        assert!(sent.get("meta").is_none());
    }

    #[test]
    fn replace_refuses_more_than_five_before_any_request() {
        let transport = FakeTransport::scripted([ok(response_document("get-configuration.json"))]);
        let wall = client(&transport);
        let snapshot = wall.read();
        let six: Vec<TrackedFlight> = (1..=MAX_TRACKED_FLIGHTS + 1)
            .map(|i| TrackedFlight::new(format!("BA{i}"), now()))
            .collect();

        let refused = wall.replace_tracked_flights(&snapshot, &six).unwrap_err();

        assert_eq!(refused, WriteRefused::TooMany(6));
        assert!(refused.to_string().contains("at most 5"));
        assert_eq!(transport.calls().len(), 1);
    }

    #[test]
    fn replace_refuses_a_non_authoritative_snapshot() {
        let transport = FakeTransport::scripted([Step::Reply(503, json!({}))]);
        let wall = client(&transport);
        let snapshot = wall.read();

        let refused = wall.replace_tracked_flights(&snapshot, &[]).unwrap_err();

        assert_eq!(refused, WriteRefused::NonAuthoritative);
        assert_eq!(transport.calls().len(), 1);
    }

    #[test]
    fn replace_timeout_is_unknown_and_is_not_retried() {
        let before = response_document("get-configuration.json");
        let transport =
            FakeTransport::scripted([ok(before.clone()), Step::Fail("timeout"), ok(before)]);
        let wall = client(&transport);
        let snapshot = wall.read();

        let result = wall.replace_tracked_flights(&snapshot, &[]).unwrap();

        assert_eq!(result.outcome, WriteOutcome::Unknown);
        assert_eq!(
            result.reason.as_deref(),
            Some("flightwall_request_failed:timeout")
        );
        // Exactly one POST, then the re-read so the caller can compare.
        assert_eq!(
            transport.methods(),
            [Method::Get, Method::Post, Method::Get]
        );
        assert_eq!(result.snapshot.authority, SnapshotAuthority::Authoritative);
    }

    #[test]
    fn replace_rejected_status_is_rejected_with_no_re_read() {
        let before = response_document("get-configuration.json");
        let transport = FakeTransport::scripted([ok(before), invalid_key()]);
        let wall = client(&transport);
        let snapshot = wall.read();

        let result = wall.replace_tracked_flights(&snapshot, &[]).unwrap();

        assert_eq!(result.outcome, WriteOutcome::Rejected);
        assert_eq!(
            result.reason.as_deref(),
            Some("flightwall_credentials_rejected:1102")
        );
        assert_eq!(transport.methods(), [Method::Get, Method::Post]);
        // The caller gets back the snapshot it planned against, unchanged.
        assert_eq!(result.snapshot, snapshot);
    }

    // ---------------------------------------------------------------------------------------
    // Values.
    // ---------------------------------------------------------------------------------------

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

    #[test]
    fn wall_snapshot_debug_contains_no_document() {
        let failed = WallSnapshot::non_authoritative(now(), "flightwall_server_error:500");
        assert!(!format!("{failed:?}").contains("display_config"));

        let transport = FakeTransport::scripted([ok(response_document("get-configuration.json"))]);
        let live = client(&transport).read();
        let rendered = format!("{live:?}");
        assert!(!rendered.contains("display_config"));
        assert!(!rendered.contains("radius_request"));
        assert!(rendered.contains("EI61"));
    }

    #[test]
    fn credentials_debug_is_redacted() {
        assert_eq!(
            format!("{:?}", credentials()),
            "FlightWallCredentials(<redacted>)"
        );
    }

    // ---------------------------------------------------------------------------------------
    // Credentials file.
    // ---------------------------------------------------------------------------------------

    fn write_credentials(dir: &TempDir, body: &str, mode: u32) -> std::path::PathBuf {
        let path = dir.path().join("flightwall.toml");
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn load_credentials_reads_a_private_toml_file() {
        let dir = TempDir::new().unwrap();
        let path = write_credentials(
            &dir,
            "api_key = \" key-value \"\nuser_id = \"fw_ios_x\"\n",
            0o600,
        );

        let loaded = load_credentials(&path).unwrap();

        assert_eq!(loaded.api_key(), "key-value");
        assert_eq!(loaded.user_id(), "fw_ios_x");
    }

    #[test]
    fn load_credentials_rejects_a_world_readable_file() {
        let dir = TempDir::new().unwrap();
        let path = write_credentials(&dir, "api_key = \"k\"\nuser_id = \"u\"\n", 0o644);

        let error = load_credentials(&path).unwrap_err();

        assert!(error.to_string().contains("0600"), "{error}");
    }

    #[test]
    fn load_credentials_names_the_missing_field_not_the_values() {
        let dir = TempDir::new().unwrap();
        let path = write_credentials(&dir, "api_key = \"very-secret\"\nuser_id = \"  \"\n", 0o600);

        let error = load_credentials(&path).unwrap_err();

        let message = error.to_string();
        assert!(message.contains("user_id"), "{message}");
        assert!(!message.contains("very-secret"), "{message}");
    }

    #[test]
    fn load_credentials_rejects_non_toml() {
        let dir = TempDir::new().unwrap();
        let path = write_credentials(&dir, "{\"api_key\": \"k\"}", 0o600);

        let error = load_credentials(&path).unwrap_err();

        assert!(error.to_string().contains("not valid TOML"), "{error}");
    }
}
