//! Read a complete bounded window or return a non-authoritative snapshot.
//!
//! Every page is charged against [`CalendarLimits`]; any breach, any malformed event, and
//! any gateway failure makes the whole cycle non-authoritative rather than yielding a
//! partial view.

use crate::{
    config,
    flightwall::TransportError,
    models::{Snapshot, SourceEvent},
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde_json::{Map, Value};
use std::collections::HashMap;

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

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use chrono::{DateTime, Utc};
    use serde_json::{Value, json};

    use super::super::test_support::{now, timed_event, timed_event_with, utc};
    use super::*;
    use crate::flightwall::TransportError;
    use crate::models::SnapshotAuthority;

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
}
