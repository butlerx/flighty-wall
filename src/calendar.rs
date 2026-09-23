//! Bounded Google Calendar reads and privacy-safe fixture sanitization.
//!
//! The reader consumes every page of a bounded window or returns a non-authoritative
//! snapshot. Google's discovery client has no Rust port, so the gateway speaks the
//! Calendar v3 REST surface directly: one `GET events.list` per page, authorised by a
//! service-account bearer token minted from a signed JWT.

use crate::{
    config::{self, ConfigError},
    flightwall::TransportError,
    models::{Snapshot, SourceEvent},
    redaction::Rules,
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    time::Duration,
};

pub const CALENDAR_READONLY_SCOPE: &str = "https://www.googleapis.com/auth/calendar.readonly";
pub const TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const EVENTS_URL_PREFIX: &str = "https://www.googleapis.com/calendar/v3/calendars/";

/// Refresh a bearer token this long before Google says it expires.
const TOKEN_REFRESH_MARGIN: ChronoDuration = ChronoDuration::seconds(60);
/// Google caps service-account assertions at one hour.
const ASSERTION_LIFETIME: ChronoDuration = ChronoDuration::seconds(3600);

/// Calendar keys that carry identity or private links and never belong in a fixture.
pub const DROPPED_FIXTURE_KEYS: [&str; 7] = [
    "attachments",
    "attendees",
    "conferenceData",
    "etag",
    "hangoutLink",
    "htmlLink",
    "iCalUID",
];

// ---------------------------------------------------------------------------------------
// Reader.
// ---------------------------------------------------------------------------------------

/// Hard caps that make an oversized calendar response non-authoritative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CalendarLimits {
    pub max_pages: usize,
    pub max_events: usize,
    pub max_field_chars: usize,
    pub max_snapshot_bytes: usize,
}

impl From<&config::Limits> for CalendarLimits {
    fn from(limits: &config::Limits) -> Self {
        // Every field is validated `>= 1` at load, so the conversion cannot fail in practice;
        // clamping to zero on a negative just makes the reader refuse everything.
        let clamp = |value: i64| usize::try_from(value).unwrap_or(0);
        Self {
            max_pages: clamp(limits.max_pages),
            max_events: clamp(limits.max_events),
            max_field_chars: clamp(limits.max_field_chars),
            max_snapshot_bytes: clamp(limits.max_snapshot_bytes),
        }
    }
}

/// Why one page could not be fetched. `kind()` is the log-safe token in the reason.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GatewayError {
    #[error("could not obtain a bearer token: {0}")]
    Token(String),
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("calendar API answered {0}")]
    Status(u16),
}

impl GatewayError {
    /// A short classifier with no URL, header, or body in it.
    #[must_use]
    pub fn kind(&self) -> String {
        match self {
            Self::Token(_) => "auth".to_owned(),
            Self::Transport(error) => error.kind().to_owned(),
            Self::Status(status) => format!("http_{status}"),
        }
    }
}

/// One page of calendar reads, expressed without any Google types.
pub trait CalendarGateway {
    /// Read one page of events in `[time_min, time_max]`.
    ///
    /// # Errors
    ///
    /// [`GatewayError`] when the page could not be fetched at all. A page that arrives but
    /// is malformed is returned as-is; the reader validates it.
    fn list_events_page(
        &self,
        calendar_id: &str,
        time_min: DateTime<Utc>,
        time_max: DateTime<Utc>,
        page_token: Option<&str>,
    ) -> Result<Value, GatewayError>;
}

impl<G: CalendarGateway + ?Sized> CalendarGateway for &G {
    fn list_events_page(
        &self,
        calendar_id: &str,
        time_min: DateTime<Utc>,
        time_max: DateTime<Utc>,
        page_token: Option<&str>,
    ) -> Result<Value, GatewayError> {
        (**self).list_events_page(calendar_id, time_min, time_max, page_token)
    }
}

/// Read a complete bounded window or return a non-authoritative snapshot.
#[derive(Debug)]
pub struct CalendarReader<G: CalendarGateway> {
    gateway: G,
    calendar_id: String,
    lookahead: ChronoDuration,
    lookback: ChronoDuration,
    limits: CalendarLimits,
}

impl<G: CalendarGateway> CalendarReader<G> {
    /// A reader over `[now - lookback_days, now + lookahead_days]`.
    pub fn new(
        gateway: G,
        calendar_id: impl Into<String>,
        lookahead_days: u32,
        lookback_days: u32,
        limits: CalendarLimits,
    ) -> Self {
        Self {
            gateway,
            calendar_id: calendar_id.into(),
            lookahead: ChronoDuration::days(i64::from(lookahead_days)),
            lookback: ChronoDuration::days(i64::from(lookback_days)),
            limits,
        }
    }

    /// An authoritative snapshot, or a failed one if the window is incomplete.
    pub fn read_snapshot(&self, now: DateTime<Utc>) -> Snapshot {
        let time_min = now - self.lookback;
        let time_max = now + self.lookahead;
        let mut accumulator = PageAccumulator::new(self.limits, now);
        let mut page_token: Option<String> = None;

        for _ in 0..self.limits.max_pages {
            let page = match self.gateway.list_events_page(
                &self.calendar_id,
                time_min,
                time_max,
                page_token.as_deref(),
            ) {
                Ok(page) => page,
                Err(error) => {
                    return Snapshot::failed(
                        now,
                        format!("calendar_request_failed:{}", error.kind()),
                    );
                }
            };
            match accumulator.absorb(&page) {
                Err(error) => return Snapshot::failed(now, error.0),
                Ok(None) => return Snapshot::authoritative(now, accumulator.into_events()),
                Ok(Some(next)) => page_token = Some(next),
            }
        }
        Snapshot::failed(now, "calendar_limit_exceeded:max_pages")
    }
}

/// A Calendar response that cannot be treated as authoritative. The string is the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CalendarDataError(String);

impl CalendarDataError {
    fn new(reason: impl Into<String>) -> Self {
        Self(reason.into())
    }
}

/// Collect events across pages, rejecting the whole cycle on any bound breach.
struct PageAccumulator {
    limits: CalendarLimits,
    observed_at: DateTime<Utc>,
    byte_count: usize,
    events: Vec<SourceEvent>,
    /// Position in `events` and the raw object, by event id, for duplicate detection.
    seen: HashMap<String, (usize, Value)>,
}

impl PageAccumulator {
    fn new(limits: CalendarLimits, observed_at: DateTime<Utc>) -> Self {
        Self {
            limits,
            observed_at,
            byte_count: 0,
            events: Vec::new(),
            seen: HashMap::new(),
        }
    }

    /// Add one response page and return the next page token, if any.
    fn absorb(&mut self, page: &Value) -> Result<Option<String>, CalendarDataError> {
        self.charge_bounds(page)?;
        let items = match page.get("items") {
            None => &[][..],
            Some(Value::Array(items)) => items.as_slice(),
            Some(_) => return Err(CalendarDataError::new("calendar_response_invalid:items")),
        };
        for raw in items {
            self.add_event(raw)?;
        }
        next_token(page)
    }

    /// Every accepted event in first-seen order.
    fn into_events(self) -> Vec<SourceEvent> {
        self.events
    }

    fn charge_bounds(&mut self, page: &Value) -> Result<(), CalendarDataError> {
        self.byte_count += serde_json::to_vec(page).map_or(0, |bytes| bytes.len());
        if self.byte_count > self.limits.max_snapshot_bytes {
            return Err(CalendarDataError::new(
                "calendar_limit_exceeded:max_snapshot_bytes",
            ));
        }
        if longest_string(page) > self.limits.max_field_chars {
            return Err(CalendarDataError::new(
                "calendar_limit_exceeded:max_field_chars",
            ));
        }
        Ok(())
    }

    fn add_event(&mut self, raw: &Value) -> Result<(), CalendarDataError> {
        let raw_event = raw
            .as_object()
            .ok_or_else(|| CalendarDataError::new("calendar_response_invalid:event"))?;
        let event = source_event(raw_event, self.observed_at)?;

        match self.seen.get(&event.event_id) {
            Some((_, prior)) if prior != raw => {
                return Err(CalendarDataError::new(
                    "calendar_response_invalid:duplicate_event",
                ));
            }
            Some((index, _)) => self.events[*index] = event,
            None => {
                self.seen
                    .insert(event.event_id.clone(), (self.events.len(), raw.clone()));
                self.events.push(event);
                if self.events.len() > self.limits.max_events {
                    return Err(CalendarDataError::new("calendar_limit_exceeded:max_events"));
                }
            }
        }
        Ok(())
    }
}

fn next_token(page: &Value) -> Result<Option<String>, CalendarDataError> {
    match page.get("nextPageToken") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(token)) => Ok(Some(token.clone())),
        Some(_) => Err(CalendarDataError::new(
            "calendar_response_invalid:next_page_token",
        )),
    }
}

fn source_event(
    raw: &Map<String, Value>,
    observed_at: DateTime<Utc>,
) -> Result<SourceEvent, CalendarDataError> {
    let event_id = raw
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| CalendarDataError::new("calendar_response_invalid:event_id"))?;

    let text_or = |key: &str, default: &str| -> Result<String, CalendarDataError> {
        match raw.get(key) {
            None => Ok(default.to_owned()),
            Some(Value::String(text)) => Ok(text.clone()),
            Some(_) => Err(CalendarDataError::new(format!(
                "calendar_response_invalid:event_fields:{event_id}"
            ))),
        }
    };
    let summary = text_or("summary", "")?;
    let status = text_or("status", "confirmed")?;

    Ok(SourceEvent {
        event_id: event_id.to_owned(),
        summary,
        starts_at: event_boundary(raw.get("start"), event_id, "start")?,
        ends_at: event_boundary(raw.get("end"), event_id, "end")?,
        status,
        updated_at: optional_datetime(raw.get("updated"), event_id, "updated")?,
        observed_at,
        fields: raw.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
    })
}

fn event_boundary(
    value: Option<&Value>,
    event_id: &str,
    field: &str,
) -> Result<Option<DateTime<Utc>>, CalendarDataError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let boundary = value.as_object().ok_or_else(|| {
        CalendarDataError::new(format!("calendar_response_invalid:{field}:{event_id}"))
    })?;
    match boundary.get("dateTime") {
        // An all-day event has `date` and no `dateTime`; the parser decides what that means.
        None if boundary.get("date").is_some_and(Value::is_string) => Ok(None),
        date_time => optional_datetime(date_time, event_id, field),
    }
}

fn optional_datetime(
    value: Option<&Value>,
    event_id: &str,
    field: &str,
) -> Result<Option<DateTime<Utc>>, CalendarDataError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        // RFC 3339 requires an offset, so a naive timestamp is rejected.
        Some(Value::String(text)) => DateTime::parse_from_rfc3339(text)
            .map(|parsed| Some(parsed.with_timezone(&Utc)))
            .map_err(|_| {
                CalendarDataError::new(format!("calendar_response_invalid:{field}:{event_id}"))
            }),
        Some(_) => Err(CalendarDataError::new(format!(
            "calendar_response_invalid:{field}:{event_id}"
        ))),
    }
}

fn longest_string(value: &Value) -> usize {
    match value {
        Value::String(text) => text.chars().count(),
        Value::Object(map) => map
            .iter()
            .map(|(key, item)| key.chars().count().max(longest_string(item)))
            .max()
            .unwrap_or(0),
        Value::Array(items) => items.iter().map(longest_string).max().unwrap_or(0),
        _ => 0,
    }
}

// ---------------------------------------------------------------------------------------
// Google gateway.
// ---------------------------------------------------------------------------------------

/// Two shapes of HTTPS round-trip, expressed without any HTTP-library types.
pub trait JsonHttp {
    /// `GET url?query` with a bearer token; returns status and decoded JSON.
    ///
    /// # Errors
    ///
    /// [`TransportError`] when no status was received.
    fn get_json(
        &self,
        url: &str,
        query: &[(&str, &str)],
        bearer: &str,
    ) -> Result<(u16, Value), TransportError>;

    /// `POST url` with a URL-encoded form body; returns status and decoded JSON.
    ///
    /// # Errors
    ///
    /// [`TransportError`] when no status was received.
    fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<(u16, Value), TransportError>;
}

impl<H: JsonHttp + ?Sized> JsonHttp for &H {
    fn get_json(
        &self,
        url: &str,
        query: &[(&str, &str)],
        bearer: &str,
    ) -> Result<(u16, Value), TransportError> {
        (**self).get_json(url, query, bearer)
    }

    fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<(u16, Value), TransportError> {
        (**self).post_form(url, form)
    }
}

/// Something that can produce a bearer token for the Calendar API.
pub trait TokenSource {
    /// A bearer token valid for at least the next request.
    ///
    /// # Errors
    ///
    /// A short, log-safe reason the token could not be obtained.
    fn bearer_token(&self) -> Result<String, String>;
}

impl<S: TokenSource + ?Sized> TokenSource for &S {
    fn bearer_token(&self) -> Result<String, String> {
        (**self).bearer_token()
    }
}

/// The Calendar v3 `events.list` call, one page at a time.
#[derive(Debug)]
pub struct GoogleCalendarGateway<H: JsonHttp, S: TokenSource> {
    http: H,
    tokens: S,
}

impl<H: JsonHttp, S: TokenSource> GoogleCalendarGateway<H, S> {
    pub const fn new(http: H, tokens: S) -> Self {
        Self { http, tokens }
    }
}

impl<H: JsonHttp, S: TokenSource> CalendarGateway for GoogleCalendarGateway<H, S> {
    /// Read one page of single, time-ordered events including cancellations.
    fn list_events_page(
        &self,
        calendar_id: &str,
        time_min: DateTime<Utc>,
        time_max: DateTime<Utc>,
        page_token: Option<&str>,
    ) -> Result<Value, GatewayError> {
        let bearer = self.tokens.bearer_token().map_err(GatewayError::Token)?;
        let url = format!("{EVENTS_URL_PREFIX}{}/events", percent_encode(calendar_id));
        let time_min = rfc3339(time_min);
        let time_max = rfc3339(time_max);
        let mut query = vec![
            ("timeMin", time_min.as_str()),
            ("timeMax", time_max.as_str()),
            ("singleEvents", "true"),
            ("orderBy", "startTime"),
            ("showDeleted", "true"),
        ];
        if let Some(token) = page_token {
            query.push(("pageToken", token));
        }
        let (status, body) = self.http.get_json(&url, &query, &bearer)?;
        if status == 200 {
            Ok(body)
        } else {
            Err(GatewayError::Status(status))
        }
    }
}

/// Percent-encode a calendar id for the path segment. Google ids are `local@domain`;
/// `@` is the only character in them that needs escaping.
fn percent_encode(calendar_id: &str) -> String {
    calendar_id.replace('@', "%40")
}

fn rfc3339(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

// ---------------------------------------------------------------------------------------
// Service-account tokens.
// ---------------------------------------------------------------------------------------

/// Why a service-account key could not be turned into a bearer token.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error(transparent)]
    NotPrivate(#[from] ConfigError),
    #[error("cannot read service-account key {path}: {source}")]
    Unreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("service-account key is not the expected JSON shape: {0}")]
    InvalidKey(PathBuf),
    #[error("service-account private key is not a usable RSA PEM: {0}")]
    InvalidPem(PathBuf),
}

/// The fields of a Google service-account JSON key the token flow needs.
#[derive(Debug, Clone, Deserialize)]
struct ServiceAccountKey {
    client_email: String,
    private_key: String,
    #[serde(default = "default_token_uri")]
    token_uri: String,
}

fn default_token_uri() -> String {
    TOKEN_URI.to_owned()
}

#[derive(Debug, Clone)]
struct CachedToken {
    value: String,
    expires_at: DateTime<Utc>,
}

/// The JWT claims Google's service-account flow expects.
#[derive(Debug, serde::Serialize)]
struct Claims<'a> {
    iss: &'a str,
    scope: &'a str,
    aud: &'a str,
    iat: i64,
    exp: i64,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: i64,
}

/// Mints and caches a Calendar-read-only bearer token from a service-account key.
pub struct ServiceAccountTokenSource<H: JsonHttp> {
    http: H,
    client_email: String,
    token_uri: String,
    signing_key: jsonwebtoken::EncodingKey,
    clock: Box<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    cache: RefCell<Option<CachedToken>>,
}

impl<H: JsonHttp> fmt::Debug for ServiceAccountTokenSource<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceAccountTokenSource")
            .field("client_email", &self.client_email)
            .field("token_uri", &self.token_uri)
            .finish_non_exhaustive()
    }
}

impl<H: JsonHttp> ServiceAccountTokenSource<H> {
    /// Load a private service-account key and prepare to sign with it.
    ///
    /// # Errors
    ///
    /// [`AuthError`] when the key file is missing, not private, not JSON, or its PEM is not
    /// an RSA private key. Messages name the path, never the key.
    pub fn from_key_file(credentials_path: &Path, http: H) -> Result<Self, AuthError> {
        Self::from_key_file_with_clock(credentials_path, http, Utc::now)
    }

    /// As [`Self::from_key_file`], with an injected clock for tests.
    ///
    /// # Errors
    ///
    /// As [`Self::from_key_file`].
    pub fn from_key_file_with_clock(
        credentials_path: &Path,
        http: H,
        clock: impl Fn() -> DateTime<Utc> + Send + Sync + 'static,
    ) -> Result<Self, AuthError> {
        config::require_private_file(credentials_path)?;
        let body = std::fs::read(credentials_path).map_err(|source| AuthError::Unreadable {
            path: credentials_path.to_owned(),
            source,
        })?;
        let key: ServiceAccountKey = serde_json::from_slice(&body)
            .map_err(|_| AuthError::InvalidKey(credentials_path.to_owned()))?;
        let signing_key = jsonwebtoken::EncodingKey::from_rsa_pem(key.private_key.as_bytes())
            .map_err(|_| AuthError::InvalidPem(credentials_path.to_owned()))?;
        Ok(Self {
            http,
            client_email: key.client_email,
            token_uri: key.token_uri,
            signing_key,
            clock: Box::new(clock),
            cache: RefCell::new(None),
        })
    }

    fn fetch(&self, now: DateTime<Utc>) -> Result<CachedToken, String> {
        let claims = Claims {
            iss: &self.client_email,
            scope: CALENDAR_READONLY_SCOPE,
            aud: &self.token_uri,
            iat: now.timestamp(),
            exp: (now + ASSERTION_LIFETIME).timestamp(),
        };
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        let assertion = jsonwebtoken::encode(&header, &claims, &self.signing_key)
            .map_err(|_| "sign".to_owned())?;

        let form = [
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", assertion.as_str()),
        ];
        let (status, body) = self
            .http
            .post_form(&self.token_uri, &form)
            .map_err(|error| error.kind().to_owned())?;
        if status != 200 {
            return Err(format!("http_{status}"));
        }
        let response: TokenResponse =
            serde_json::from_value(body).map_err(|_| "token_response".to_owned())?;
        Ok(CachedToken {
            value: response.access_token,
            expires_at: now + ChronoDuration::seconds(response.expires_in),
        })
    }
}

impl<H: JsonHttp> TokenSource for ServiceAccountTokenSource<H> {
    fn bearer_token(&self) -> Result<String, String> {
        let now = (self.clock)();
        if let Some(cached) = self.cache.borrow().as_ref() {
            if cached.expires_at - TOKEN_REFRESH_MARGIN > now {
                return Ok(cached.value.clone());
            }
        }
        let fresh = self.fetch(now)?;
        let value = fresh.value.clone();
        *self.cache.borrow_mut() = Some(fresh);
        Ok(value)
    }
}

// ---------------------------------------------------------------------------------------
// Production HTTP.
// ---------------------------------------------------------------------------------------

/// The production [`JsonHttp`]: HTTPS, normal certificate validation, no redirects.
pub struct UreqJsonHttp {
    agent: ureq::Agent,
}

impl UreqJsonHttp {
    #[must_use]
    pub fn new(timeout: Duration) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .max_redirects(0)
            .http_status_as_error(false)
            .build();
        Self {
            agent: config.new_agent(),
        }
    }
}

impl fmt::Debug for UreqJsonHttp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UreqJsonHttp").finish_non_exhaustive()
    }
}

impl JsonHttp for UreqJsonHttp {
    fn get_json(
        &self,
        url: &str,
        query: &[(&str, &str)],
        bearer: &str,
    ) -> Result<(u16, Value), TransportError> {
        let request = self
            .agent
            .get(url)
            .header("accept", "application/json")
            .header("authorization", &format!("Bearer {bearer}"))
            .query_pairs(query.iter().copied());
        decode(request.call())
    }

    fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<(u16, Value), TransportError> {
        let request = self.agent.post(url).header("accept", "application/json");
        decode(request.send_form(form.iter().copied()))
    }
}

fn decode(
    result: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> Result<(u16, Value), TransportError> {
    let response = result.map_err(|error| TransportError::new(ureq_kind(&error)))?;
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

fn ureq_kind(error: &ureq::Error) -> &'static str {
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
// Fixture sanitization.
// ---------------------------------------------------------------------------------------

/// A structurally useful fixture with direct identifiers and PII removed.
#[must_use]
pub fn sanitize_event_payload(
    event: &Map<String, Value>,
    sensitive_terms: &[&str],
) -> Map<String, Value> {
    let rules = Rules::new()
        .with_terms(sensitive_terms)
        .dropping_keys(DROPPED_FIXTURE_KEYS);
    let mut sanitized = rules.scrub_object(event);
    if let Some(raw_id) = event.get("id").and_then(Value::as_str) {
        let digest = hex::encode(Sha256::digest(raw_id.as_bytes()));
        sanitized.insert(
            "id".to_owned(),
            Value::String(format!("event-{}", &digest[..12])),
        );
    }
    sanitized
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::*;
    use crate::flightwall::TransportError;
    use crate::models::SnapshotAuthority;
    use chrono::{DateTime, TimeZone, Utc};
    use serde_json::{Map, Value, json};
    use tempfile::TempDir;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    fn now() -> DateTime<Utc> {
        utc(2026, 9, 21, 12, 0)
    }

    // ---------------------------------------------------------------------------------------
    // Fake gateway: pages keyed by page token.
    // ---------------------------------------------------------------------------------------

    enum Page {
        Body(Value),
        Fail(GatewayError),
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Call {
        calendar_id: String,
        time_min: DateTime<Utc>,
        time_max: DateTime<Utc>,
        page_token: Option<String>,
    }

    struct FakeGateway {
        pages: HashMap<Option<String>, Page>,
        calls: RefCell<Vec<Call>>,
    }

    impl FakeGateway {
        fn new(pages: impl IntoIterator<Item = (Option<&'static str>, Page)>) -> Self {
            Self {
                pages: pages
                    .into_iter()
                    .map(|(token, page)| (token.map(str::to_owned), page))
                    .collect(),
                calls: RefCell::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.borrow().clone()
        }
    }

    impl CalendarGateway for FakeGateway {
        fn list_events_page(
            &self,
            calendar_id: &str,
            time_min: DateTime<Utc>,
            time_max: DateTime<Utc>,
            page_token: Option<&str>,
        ) -> Result<Value, GatewayError> {
            self.calls.borrow_mut().push(Call {
                calendar_id: calendar_id.to_owned(),
                time_min,
                time_max,
                page_token: page_token.map(str::to_owned),
            });
            match self.pages.get(&page_token.map(str::to_owned)) {
                None => panic!("unexpected page token {page_token:?}"),
                Some(Page::Body(body)) => Ok(body.clone()),
                Some(Page::Fail(error)) => Err(error.clone()),
            }
        }
    }

    fn timed_event(event_id: &str) -> Value {
        timed_event_with(event_id, "AA123 · Friend", "2026-09-22T08:00:00-04:00")
    }

    fn timed_event_with(event_id: &str, summary: &str, start: &str) -> Value {
        json!({
            "id": event_id,
            "status": "confirmed",
            "summary": summary,
            "description": "Flight AA123 from JFK to ORD",
            "start": {"dateTime": start, "timeZone": "America/New_York"},
            "end": {"dateTime": "2026-09-22T10:30:00-05:00", "timeZone": "America/Chicago"},
            "updated": "2026-09-21T11:00:00Z",
        })
    }

    fn items(events: &[Value]) -> Page {
        Page::Body(json!({"items": events}))
    }

    fn items_then(events: &[Value], next: &str) -> Page {
        Page::Body(json!({"items": events, "nextPageToken": next}))
    }

    #[derive(Clone, Copy)]
    struct Overrides {
        max_pages: usize,
        max_events: usize,
        max_field_chars: usize,
        lookahead_days: u32,
        lookback_days: u32,
    }

    impl Default for Overrides {
        fn default() -> Self {
            Self {
                max_pages: 10,
                max_events: 500,
                max_field_chars: 8_192,
                lookahead_days: 7,
                lookback_days: 0,
            }
        }
    }

    fn reader(gateway: &FakeGateway, overrides: Overrides) -> CalendarReader<&FakeGateway> {
        CalendarReader::new(
            gateway,
            "friends@example.invalid",
            overrides.lookahead_days,
            overrides.lookback_days,
            CalendarLimits {
                max_pages: overrides.max_pages,
                max_events: overrides.max_events,
                max_field_chars: overrides.max_field_chars,
                max_snapshot_bytes: 1_048_576,
            },
        )
    }

    fn default_reader(gateway: &FakeGateway) -> CalendarReader<&FakeGateway> {
        reader(gateway, Overrides::default())
    }

    // ---------------------------------------------------------------------------------------
    // Reader.
    // ---------------------------------------------------------------------------------------

    #[test]
    fn reader_window_defaults_to_now_forward() {
        let gateway = FakeGateway::new([(None, items(&[]))]);

        default_reader(&gateway).read_snapshot(now());

        let call = &gateway.calls()[0];
        assert_eq!(call.calendar_id, "friends@example.invalid");
        assert_eq!(call.time_min, now());
        assert_eq!(call.time_max, utc(2026, 9, 28, 12, 0));
    }

    #[test]
    fn reader_lookback_extends_window_into_the_past() {
        let gateway = FakeGateway::new([(None, items(&[]))]);

        reader(
            &gateway,
            Overrides {
                lookahead_days: 60,
                lookback_days: 3,
                ..Overrides::default()
            },
        )
        .read_snapshot(now());

        let call = &gateway.calls()[0];
        assert_eq!(call.time_min, utc(2026, 9, 18, 12, 0));
        assert_eq!(call.time_max, utc(2026, 11, 20, 12, 0));
    }

    #[test]
    fn reader_consumes_every_page_before_marking_snapshot_authoritative() {
        let gateway = FakeGateway::new([
            (None, items_then(&[timed_event("event-1")], "page-2")),
            (Some("page-2"), items(&[timed_event("event-2")])),
        ]);

        let snapshot = default_reader(&gateway).read_snapshot(now());

        assert_eq!(snapshot.authority, SnapshotAuthority::Authoritative);
        let ids: Vec<&str> = snapshot
            .events
            .iter()
            .map(|e| e.event_id.as_str())
            .collect();
        assert_eq!(ids, ["event-1", "event-2"]);
        assert_eq!(snapshot.events[0].starts_at, Some(utc(2026, 9, 22, 12, 0)));
        assert_eq!(snapshot.events[0].ends_at, Some(utc(2026, 9, 22, 15, 30)));
        assert_eq!(snapshot.events[0].updated_at, Some(utc(2026, 9, 21, 11, 0)));
        assert_eq!(snapshot.events[0].observed_at, now());
        let tokens: Vec<Option<String>> =
            gateway.calls().into_iter().map(|c| c.page_token).collect();
        assert_eq!(tokens, [None, Some("page-2".to_owned())]);
    }

    #[test]
    fn second_page_failure_discards_partial_events() {
        let gateway = FakeGateway::new([
            (None, items_then(&[timed_event("event-1")], "page-2")),
            (
                Some("page-2"),
                Page::Fail(GatewayError::Transport(TransportError::new("timeout"))),
            ),
        ]);

        let snapshot = default_reader(&gateway).read_snapshot(now());

        assert_eq!(snapshot.authority, SnapshotAuthority::NonAuthoritative);
        assert!(snapshot.events.is_empty());
        assert_eq!(
            snapshot.reason.as_deref(),
            Some("calendar_request_failed:timeout")
        );
    }

    #[test]
    fn gateway_error_kinds_are_log_safe() {
        for (error, kind) in [
            (GatewayError::Token("http_401".into()), "auth"),
            (GatewayError::Status(403), "http_403"),
            (GatewayError::Transport(TransportError::new("dns")), "dns"),
        ] {
            let gateway = FakeGateway::new([(None, Page::Fail(error))]);
            let snapshot = default_reader(&gateway).read_snapshot(now());
            assert_eq!(
                snapshot.reason.as_deref(),
                Some(format!("calendar_request_failed:{kind}").as_str())
            );
        }
    }

    #[test]
    fn reader_rejects_more_pages_than_configured() {
        let gateway = FakeGateway::new([
            (None, items_then(&[], "page-2")),
            (Some("page-2"), items_then(&[], "page-3")),
        ]);

        let snapshot = reader(
            &gateway,
            Overrides {
                max_pages: 1,
                ..Overrides::default()
            },
        )
        .read_snapshot(now());

        assert_eq!(snapshot.authority, SnapshotAuthority::NonAuthoritative);
        assert_eq!(
            snapshot.reason.as_deref(),
            Some("calendar_limit_exceeded:max_pages")
        );
        // The cap is on pages fetched: one page in, no second request.
        assert_eq!(gateway.calls().len(), 1);
    }

    #[test]
    fn reader_rejects_event_and_field_limits() {
        let events_gateway =
            FakeGateway::new([(None, items(&[timed_event("1"), timed_event("2")]))]);
        let fields_gateway = FakeGateway::new([(
            None,
            items(&[timed_event_with(
                "1",
                &"A".repeat(40),
                "2026-09-22T08:00:00-04:00",
            )]),
        )]);

        let too_many = reader(
            &events_gateway,
            Overrides {
                max_events: 1,
                ..Overrides::default()
            },
        )
        .read_snapshot(now());
        let oversized = reader(
            &fields_gateway,
            Overrides {
                max_field_chars: 20,
                ..Overrides::default()
            },
        )
        .read_snapshot(now());

        assert_eq!(too_many.authority, SnapshotAuthority::NonAuthoritative);
        assert_eq!(
            too_many.reason.as_deref(),
            Some("calendar_limit_exceeded:max_events")
        );
        assert_eq!(oversized.authority, SnapshotAuthority::NonAuthoritative);
        assert_eq!(
            oversized.reason.as_deref(),
            Some("calendar_limit_exceeded:max_field_chars")
        );
    }

    #[test]
    fn reader_retains_all_day_event_for_fail_closed_parser() {
        let gateway = FakeGateway::new([(
            None,
            items(&[json!({
                "id": "all-day",
                "status": "confirmed",
                "summary": "AA123",
                "start": {"date": "2026-09-22"},
                "end": {"date": "2026-09-23"},
            })]),
        )]);

        let snapshot = default_reader(&gateway).read_snapshot(now());

        assert_eq!(snapshot.authority, SnapshotAuthority::Authoritative);
        assert_eq!(snapshot.events[0].starts_at, None);
        assert_eq!(snapshot.events[0].ends_at, None);
        assert_eq!(snapshot.events[0].fields["start"]["date"], "2026-09-22");
    }

    #[test]
    fn reader_rejects_malformed_pages_by_name() {
        let cases = [
            (
                json!({"items": "not-a-list"}),
                "calendar_response_invalid:items",
            ),
            (json!({"items": [42]}), "calendar_response_invalid:event"),
            (
                json!({"items": [{"summary": "no id"}]}),
                "calendar_response_invalid:event_id",
            ),
            (
                json!({"items": [{"id": "e", "summary": 7}]}),
                "calendar_response_invalid:event_fields:e",
            ),
            (
                json!({"items": [{"id": "e", "start": "2026-09-22T08:00:00Z"}]}),
                "calendar_response_invalid:start:e",
            ),
            (
                json!({"items": [{"id": "e", "start": {"dateTime": "2026-09-22T08:00:00"}}]}),
                "calendar_response_invalid:start:e",
            ),
            (
                json!({"items": [{"id": "e", "updated": 5}]}),
                "calendar_response_invalid:updated:e",
            ),
            (
                json!({"items": [], "nextPageToken": 9}),
                "calendar_response_invalid:next_page_token",
            ),
        ];
        for (page, reason) in cases {
            let gateway = FakeGateway::new([(None, Page::Body(page.clone()))]);
            let snapshot = default_reader(&gateway).read_snapshot(now());
            assert_eq!(snapshot.reason.as_deref(), Some(reason), "{page}");
        }
    }

    #[test]
    fn reader_rejects_a_duplicate_event_that_changed_between_pages() {
        let same = timed_event("dup");
        let changed = timed_event_with("dup", "AA123 · moved", "2026-09-22T09:00:00-04:00");
        let gateway = FakeGateway::new([
            (None, items_then(std::slice::from_ref(&same), "page-2")),
            (Some("page-2"), items(&[changed])),
        ]);

        let snapshot = default_reader(&gateway).read_snapshot(now());

        assert_eq!(
            snapshot.reason.as_deref(),
            Some("calendar_response_invalid:duplicate_event")
        );

        // An identical repeat is tolerated and counted once.
        let gateway = FakeGateway::new([
            (None, items_then(std::slice::from_ref(&same), "page-2")),
            (Some("page-2"), items(&[same])),
        ]);
        let snapshot = default_reader(&gateway).read_snapshot(now());
        assert_eq!(snapshot.authority, SnapshotAuthority::Authoritative);
        assert_eq!(snapshot.events.len(), 1);
    }

    // ---------------------------------------------------------------------------------------
    // Sanitizer.
    // ---------------------------------------------------------------------------------------

    fn object(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other => panic!("expected an object, got {other}"),
        }
    }

    #[test]
    fn sanitizer_preserves_structure_and_flight_number_but_removes_pii() {
        let mut event = timed_event_with(
            "private-google-id",
            "Alice Smith · AA123",
            "2026-09-22T08:00:00-04:00",
        );
        event["description"] = json!(
            "Confirmation: ABC123\nSeat: 12A\nalice@example.com\nhttps://calendar.example/private-link"
        );
        event["creator"] = json!({"email": "alice@example.com", "displayName": "Alice Smith"});
        event["organizer"] = json!({"email": "owner@example.com"});
        event["attendees"] = json!([{"email": "friend@example.com"}]);

        let sanitized = sanitize_event_payload(&object(event), &["Alice Smith"]);
        let rendered = Value::Object(sanitized.clone()).to_string();

        let id = sanitized["id"].as_str().unwrap();
        assert!(id.starts_with("event-"));
        assert_eq!(id.len(), "event-".len() + 12);
        assert!(sanitized["summary"].as_str().unwrap().contains("AA123"));
        assert!(!rendered.contains("Alice Smith"));
        assert!(!rendered.contains("ABC123"));
        assert!(!rendered.contains("12A"));
        assert!(!rendered.contains("@example.com"));
        assert!(!rendered.contains("private-link"));
        assert!(!sanitized.contains_key("attendees"));
    }

    #[test]
    fn sanitizer_redacts_flighty_deeplinks_and_calendar_uids() {
        let mut event = timed_event("private-google-id");
        event["description"] = json!(
            "Ryanair 8721\nDublin to Barcelona\nView in Flighty flighty://flight/651cbaa3-2a8b-4580-b88c-c7d2a0164f9e"
        );
        event["iCalUID"] = json!("9BDF5975-05CA-4689-B3BD-48501DC930D4");
        event["etag"] = json!("\"3579963474324702\"");

        let sanitized = sanitize_event_payload(&object(event), &[]);
        let rendered = Value::Object(sanitized.clone()).to_string();

        assert!(!rendered.contains("651cbaa3"));
        assert!(!rendered.contains("flighty://"));
        assert!(!rendered.contains("9BDF5975"));
        assert!(!rendered.contains("3579963474324702"));
        let description = sanitized["description"].as_str().unwrap();
        assert!(description.contains("Ryanair 8721"));
        assert!(description.contains("Dublin to Barcelona"));
    }

    #[test]
    fn sanitizer_id_is_stable_for_the_same_source_id() {
        let a = sanitize_event_payload(&object(timed_event("same")), &[]);
        let b = sanitize_event_payload(&object(timed_event("same")), &[]);
        let c = sanitize_event_payload(&object(timed_event("other")), &[]);
        assert_eq!(a["id"], b["id"]);
        assert_ne!(a["id"], c["id"]);
    }

    // ---------------------------------------------------------------------------------------
    // Google gateway + service-account token flow, against a scripted HTTP.
    // ---------------------------------------------------------------------------------------

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum HttpCall {
        Get {
            url: String,
            query: Vec<(String, String)>,
            bearer: String,
        },
        PostForm {
            url: String,
            form: Vec<(String, String)>,
        },
    }

    struct FakeHttp {
        get_responses: RefCell<Vec<(u16, Value)>>,
        post_responses: RefCell<Vec<(u16, Value)>>,
        calls: RefCell<Vec<HttpCall>>,
    }

    impl FakeHttp {
        fn new(gets: Vec<(u16, Value)>, posts: Vec<(u16, Value)>) -> Self {
            Self {
                get_responses: RefCell::new(gets.into_iter().rev().collect()),
                post_responses: RefCell::new(posts.into_iter().rev().collect()),
                calls: RefCell::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<HttpCall> {
            self.calls.borrow().clone()
        }
    }

    fn owned(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    impl JsonHttp for FakeHttp {
        fn get_json(
            &self,
            url: &str,
            query: &[(&str, &str)],
            bearer: &str,
        ) -> Result<(u16, Value), TransportError> {
            self.calls.borrow_mut().push(HttpCall::Get {
                url: url.to_owned(),
                query: owned(query),
                bearer: bearer.to_owned(),
            });
            self.get_responses
                .borrow_mut()
                .pop()
                .ok_or_else(|| TransportError::new("unscripted_get"))
        }

        fn post_form(
            &self,
            url: &str,
            form: &[(&str, &str)],
        ) -> Result<(u16, Value), TransportError> {
            self.calls.borrow_mut().push(HttpCall::PostForm {
                url: url.to_owned(),
                form: owned(form),
            });
            self.post_responses
                .borrow_mut()
                .pop()
                .ok_or_else(|| TransportError::new("unscripted_post"))
        }
    }

    struct FixedToken(&'static str);

    impl TokenSource for FixedToken {
        fn bearer_token(&self) -> Result<String, String> {
            Ok(self.0.to_owned())
        }
    }

    struct NoToken;

    impl TokenSource for NoToken {
        fn bearer_token(&self) -> Result<String, String> {
            Err("http_401".to_owned())
        }
    }

    #[test]
    fn google_gateway_builds_the_events_list_request() {
        let http = FakeHttp::new(vec![(200, json!({"items": []}))], vec![]);
        let gateway = GoogleCalendarGateway::new(&http, FixedToken("tok"));

        let page = gateway
            .list_events_page(
                "friends@example.invalid",
                now(),
                utc(2026, 9, 28, 12, 0),
                Some("p2"),
            )
            .unwrap();

        assert_eq!(page, json!({"items": []}));
        let [HttpCall::Get { url, query, bearer }] = &http.calls()[..] else {
            panic!("expected one GET");
        };
        assert_eq!(
            url,
            "https://www.googleapis.com/calendar/v3/calendars/friends%40example.invalid/events"
        );
        assert_eq!(bearer, "tok");
        assert_eq!(
            *query,
            owned(&[
                ("timeMin", "2026-09-21T12:00:00Z"),
                ("timeMax", "2026-09-28T12:00:00Z"),
                ("singleEvents", "true"),
                ("orderBy", "startTime"),
                ("showDeleted", "true"),
                ("pageToken", "p2"),
            ])
        );
    }

    #[test]
    fn google_gateway_maps_non_200_and_token_failures() {
        let http = FakeHttp::new(vec![(403, json!({}))], vec![]);
        let gateway = GoogleCalendarGateway::new(&http, FixedToken("tok"));
        assert_eq!(
            gateway.list_events_page("c", now(), now(), None),
            Err(GatewayError::Status(403))
        );

        let http = FakeHttp::new(vec![], vec![]);
        let gateway = GoogleCalendarGateway::new(&http, NoToken);
        assert_eq!(
            gateway.list_events_page("c", now(), now(), None),
            Err(GatewayError::Token("http_401".to_owned()))
        );
        assert!(http.calls().is_empty(), "no request without a token");
    }

    fn key_file(dir: &TempDir, mode: u32) -> std::path::PathBuf {
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/google_auth/service-account.json");
        let path = dir.path().join("service-account.json");
        fs::copy(source, &path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn service_account_token_source_signs_a_jwt_and_caches_the_token() {
        let dir = TempDir::new().unwrap();
        let http = FakeHttp::new(
            vec![],
            vec![(
                200,
                json!({"access_token": "access-token-stub", "expires_in": 3600, "token_type": "Bearer"}),
            )],
        );
        let source =
            ServiceAccountTokenSource::from_key_file_with_clock(&key_file(&dir, 0o600), &http, now)
                .unwrap();

        let first = source.bearer_token().unwrap();
        let second = source.bearer_token().unwrap();

        assert_eq!(first, "access-token-stub");
        assert_eq!(second, "access-token-stub");
        let calls = http.calls();
        assert_eq!(calls.len(), 1, "second call served from cache");
        let HttpCall::PostForm { url, form } = &calls[0] else {
            panic!("expected a form POST");
        };
        assert_eq!(url, TOKEN_URI);
        assert_eq!(
            form[0],
            (
                "grant_type".to_owned(),
                "urn:ietf:params:oauth:grant-type:jwt-bearer".to_owned()
            )
        );
        assert_eq!(form[1].0, "assertion");

        // The assertion is a real RS256 JWT with the claims Google expects.
        let jwt = &form[1].1;
        let payload = jwt.split('.').nth(1).expect("three-part JWT");
        let claims: Value = serde_json::from_slice(&base64_url_decode(payload)).unwrap();
        assert_eq!(claims["iss"], "sync@test-project.iam.gserviceaccount.com");
        assert_eq!(claims["scope"], CALENDAR_READONLY_SCOPE);
        assert_eq!(claims["aud"], TOKEN_URI);
        assert_eq!(claims["iat"], now().timestamp());
        assert_eq!(claims["exp"], now().timestamp() + 3600);
        let header: Value =
            serde_json::from_slice(&base64_url_decode(jwt.split('.').next().unwrap())).unwrap();
        assert_eq!(header["alg"], "RS256");
    }

    #[test]
    fn service_account_token_source_refreshes_inside_the_margin() {
        let dir = TempDir::new().unwrap();
        let http = FakeHttp::new(
            vec![],
            vec![
                (200, json!({"access_token": "first", "expires_in": 30})),
                (200, json!({"access_token": "second", "expires_in": 3600})),
            ],
        );
        // 30s lifetime is inside the 60s refresh margin, so the cache is never trusted.
        let source =
            ServiceAccountTokenSource::from_key_file_with_clock(&key_file(&dir, 0o600), &http, now)
                .unwrap();

        assert_eq!(source.bearer_token().unwrap(), "first");
        assert_eq!(source.bearer_token().unwrap(), "second");
    }

    #[test]
    fn service_account_token_source_reports_failures_without_the_key() {
        let dir = TempDir::new().unwrap();
        let http = FakeHttp::new(vec![], vec![(401, json!({"error": "invalid_grant"}))]);
        let source =
            ServiceAccountTokenSource::from_key_file_with_clock(&key_file(&dir, 0o600), &http, now)
                .unwrap();

        assert_eq!(source.bearer_token(), Err("http_401".to_owned()));

        let http = FakeHttp::new(vec![], vec![(200, json!({"nope": true}))]);
        let source =
            ServiceAccountTokenSource::from_key_file_with_clock(&key_file(&dir, 0o600), &http, now)
                .unwrap();
        assert_eq!(source.bearer_token(), Err("token_response".to_owned()));
    }

    #[test]
    fn service_account_token_source_requires_a_private_key_file() {
        let dir = TempDir::new().unwrap();
        let error = ServiceAccountTokenSource::from_key_file(
            &key_file(&dir, 0o644),
            FakeHttp::new(vec![], vec![]),
        )
        .unwrap_err();
        assert!(error.to_string().contains("0600"), "{error}");

        let bad = dir.path().join("bad.json");
        fs::write(
            &bad,
            "{\"client_email\": \"x\", \"private_key\": \"not a pem\"}",
        )
        .unwrap();
        fs::set_permissions(&bad, fs::Permissions::from_mode(0o600)).unwrap();
        let error = ServiceAccountTokenSource::from_key_file(&bad, FakeHttp::new(vec![], vec![]))
            .unwrap_err();
        assert!(error.to_string().contains("RSA PEM"), "{error}");
    }

    /// Minimal base64url decoder for inspecting the JWT; no padding, URL alphabet.
    #[allow(clippy::cast_possible_truncation)] // every shift leaves at most 8 bits
    fn base64_url_decode(input: &str) -> Vec<u8> {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let value = |c: u8| -> u32 {
            let position = ALPHABET
                .iter()
                .position(|&a| a == c)
                .expect("base64url alphabet");
            u32::try_from(position).expect("position < 64")
        };
        let mut out = Vec::new();
        let bytes = input.as_bytes();
        for chunk in bytes.chunks(4) {
            let mut acc: u32 = 0;
            for (i, &c) in chunk.iter().enumerate() {
                acc |= value(c) << (18 - 6 * i);
            }
            let n = chunk.len();
            if n >= 2 {
                out.push((acc >> 16) as u8);
            }
            if n >= 3 {
                out.push((acc >> 8) as u8);
            }
            if n == 4 {
                out.push(acc as u8);
            }
        }
        out
    }
}
