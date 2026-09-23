//! Deterministic interpretation of Flighty calendar exports into physical flights.
//!
//! The parser never guesses. Every event gets an explicit outcome, and any event that
//! looks like a Flighty export but does not match a known shape makes the whole cycle
//! non-authoritative so that no downstream component mutates the wall from a partial
//! understanding of the calendar.
//!
//! Failure reasons carry the offending Google event id but never event text, so an
//! operator can find the event in their own calendar without names, routes, or
//! reservation details reaching the logs.

use crate::models::{Snapshot, SnapshotAuthority, SourceEvent};
use chrono::{DateTime, Utc};
use regex::Regex;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::LazyLock,
};

/// What the parser concluded about a single calendar event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParseOutcome {
    Flight,
    Cancelled,
    AllDay,
    MissingDeparture,
    Unrecognized,
}

impl ParseOutcome {
    /// The wire spelling used in journal lines and fixtures.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Flight => "flight",
            Self::Cancelled => "cancelled",
            Self::AllDay => "all_day",
            Self::MissingDeparture => "missing_departure",
            Self::Unrecognized => "unrecognized",
        }
    }
}

/// The normalized identity of one physical flight leg.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FlightIdentity {
    pub carrier: String,
    pub number: String,
    pub origin: String,
    pub destination: String,
    pub scheduled_departure: DateTime<Utc>,
}

impl FlightIdentity {
    /// The carrier code and flight number with no separator.
    #[must_use]
    pub fn designator(&self) -> String {
        format!("{}{}", self.carrier, self.number)
    }

    /// The IATA route as plain ASCII, safe for logs and wall labels.
    #[must_use]
    pub fn route(&self) -> String {
        format!("{}-{}", self.origin, self.destination)
    }

    /// A stable key: the same leg keeps its key across delays on the day.
    #[must_use]
    pub fn key(&self) -> String {
        let departure_day = self.scheduled_departure.format("%Y-%m-%d");
        format!("{}:{}:{departure_day}", self.designator(), self.origin)
    }
}

/// One physical flight the wall should track, with every event that asked for it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DesiredFlight {
    pub key: String,
    pub carrier: String,
    pub number: String,
    pub origin: String,
    pub destination: String,
    pub scheduled_departure: DateTime<Utc>,
    /// Sorted, so two cycles that saw the same events compare equal.
    pub source_event_ids: Vec<String>,
}

impl DesiredFlight {
    /// The carrier code and flight number with no separator.
    #[must_use]
    pub fn designator(&self) -> String {
        format!("{}{}", self.carrier, self.number)
    }

    /// The IATA route as plain ASCII, safe for logs and wall labels.
    #[must_use]
    pub fn route(&self) -> String {
        format!("{}-{}", self.origin, self.destination)
    }
}

/// The explicit parse result for one source event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventInterpretation {
    pub event_id: String,
    pub outcome: ParseOutcome,
    pub identity: Option<FlightIdentity>,
    pub updated_at: Option<DateTime<Utc>>,
}

impl EventInterpretation {
    fn of(event_id: &str, outcome: ParseOutcome) -> Self {
        Self {
            event_id: event_id.to_owned(),
            outcome,
            identity: None,
            updated_at: None,
        }
    }
}

/// Desired flights for one poll, or an explicit record of why parsing failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCycle {
    pub authority: SnapshotAuthority,
    pub flights: Vec<DesiredFlight>,
    pub interpretations: Vec<EventInterpretation>,
    pub reason: Option<String>,
}

impl ParsedCycle {
    fn failed(interpretations: Vec<EventInterpretation>, reason: String) -> Self {
        Self {
            authority: SnapshotAuthority::NonAuthoritative,
            flights: Vec::new(),
            interpretations,
            reason: Some(reason),
        }
    }

    /// Whether this cycle may be used to decide writes.
    #[must_use]
    pub const fn is_authoritative(&self) -> bool {
        matches!(self.authority, SnapshotAuthority::Authoritative)
    }
}

/// One event's claim on a flight key, kept with the freshness used to break ties.
struct Contribution<'a> {
    event_id: &'a str,
    identity: &'a FlightIdentity,
    updated_at: Option<DateTime<Utc>>,
}

/// Turn one authoritative snapshot into the set of flights the wall should track.
#[must_use]
pub fn parse_cycle(snapshot: &Snapshot) -> ParsedCycle {
    if !snapshot.is_authoritative() {
        return ParsedCycle {
            authority: snapshot.authority,
            flights: Vec::new(),
            interpretations: Vec::new(),
            reason: snapshot.reason.clone(),
        };
    }

    let interpretations: Vec<EventInterpretation> = snapshot.events.iter().map(interpret).collect();

    if let Some(unrecognized) = interpretations
        .iter()
        .find(|item| item.outcome == ParseOutcome::Unrecognized)
    {
        let reason = format!("parse_event_unrecognized:{}", unrecognized.event_id);
        return ParsedCycle::failed(interpretations, reason);
    }

    let flights = desired_flights(&interpretations);
    if let Some(codeshare) = codeshare_conflict(&flights) {
        let reason = format!("parse_codeshare_ambiguous:{codeshare}");
        return ParsedCycle::failed(interpretations, reason);
    }

    ParsedCycle {
        authority: SnapshotAuthority::Authoritative,
        flights,
        interpretations,
        reason: None,
    }
}

fn interpret(event: &SourceEvent) -> EventInterpretation {
    if event.status.eq_ignore_ascii_case("cancelled") {
        return EventInterpretation::of(&event.event_id, ParseOutcome::Cancelled);
    }

    let summary = normalize(&event.summary);
    if CANCELLATION.is_match(&summary) {
        return EventInterpretation::of(&event.event_id, ParseOutcome::Cancelled);
    }

    let Some(captures) = FLIGHT_SUMMARY.captures(&summary) else {
        return EventInterpretation::of(&event.event_id, ParseOutcome::Unrecognized);
    };

    let number = captures["number"].trim_start_matches('0');
    if number.is_empty() {
        return EventInterpretation::of(&event.event_id, ParseOutcome::Unrecognized);
    }

    let Some(starts_at) = event.starts_at else {
        let missing = if is_all_day(event) {
            ParseOutcome::AllDay
        } else {
            ParseOutcome::MissingDeparture
        };
        return EventInterpretation::of(&event.event_id, missing);
    };

    EventInterpretation {
        event_id: event.event_id.clone(),
        outcome: ParseOutcome::Flight,
        identity: Some(FlightIdentity {
            carrier: captures["carrier"].to_uppercase(),
            number: number.to_owned(),
            origin: captures["origin"].to_uppercase(),
            destination: captures["destination"].to_uppercase(),
            scheduled_departure: starts_at,
        }),
        updated_at: event.updated_at,
    }
}

fn desired_flights(interpretations: &[EventInterpretation]) -> Vec<DesiredFlight> {
    let grouped: BTreeMap<String, Vec<Contribution<'_>>> = interpretations
        .iter()
        .filter(|item| item.outcome == ParseOutcome::Flight)
        .filter_map(|item| {
            item.identity.as_ref().map(|identity| Contribution {
                event_id: &item.event_id,
                identity,
                updated_at: item.updated_at,
            })
        })
        .fold(BTreeMap::new(), |mut grouped, contribution| {
            grouped
                .entry(contribution.identity.key())
                .or_insert_with(Vec::new)
                .push(contribution);
            grouped
        });

    let mut flights: Vec<DesiredFlight> = grouped
        .into_iter()
        .filter_map(|(key, contributions)| merge(key, &contributions))
        .collect();
    flights.sort_by(|a, b| {
        a.scheduled_departure
            .cmp(&b.scheduled_departure)
            .then_with(|| a.key.cmp(&b.key))
    });
    flights
}

/// Collapse every event that named one flight, preferring the freshest departure.
///
/// Returns `None` only for an empty group, which the grouping above never produces.
fn merge(key: String, contributions: &[Contribution<'_>]) -> Option<DesiredFlight> {
    // `None` orders before every `Some`, so an event without `updated` loses every tie.
    // The event id breaks exact ties deterministically.
    let freshest = contributions
        .iter()
        .max_by_key(|item| (item.updated_at, item.event_id))?;
    let identity = freshest.identity;

    let source_event_ids = contributions
        .iter()
        .map(|item| item.event_id.to_owned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    Some(DesiredFlight {
        key,
        carrier: identity.carrier.clone(),
        number: identity.number.clone(),
        origin: identity.origin.clone(),
        destination: identity.destination.clone(),
        scheduled_departure: identity.scheduled_departure,
        source_event_ids,
    })
}

/// The conflicting designators when one leg is claimed by two flight numbers.
fn codeshare_conflict(flights: &[DesiredFlight]) -> Option<String> {
    let by_leg = flights.iter().fold(
        BTreeMap::<(&str, &str, DateTime<Utc>), BTreeSet<String>>::new(),
        |mut by_leg, flight| {
            by_leg
                .entry((
                    &flight.origin,
                    &flight.destination,
                    flight.scheduled_departure,
                ))
                .or_default()
                .insert(flight.designator());
            by_leg
        },
    );
    by_leg
        .into_values()
        .find(|designators| designators.len() > 1)
        .map(|designators| designators.into_iter().collect::<Vec<_>>().join(","))
}

fn is_all_day(event: &SourceEvent) -> bool {
    event
        .fields
        .get("start")
        .and_then(Value::as_object)
        .and_then(|start| start.get("date"))
        .is_some_and(Value::is_string)
}

/// Fold the export's non-breaking and zero-width characters into plain spacing.
fn normalize(text: &str) -> String {
    text.chars()
        .filter_map(translate)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The export's typographic spaces become plain spaces; its zero-width marks vanish.
const fn translate(character: char) -> Option<char> {
    match character {
        // no-break space, used between carrier code and flight number
        '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2007}' | '\u{2009}' | '\u{202F}' => Some(' '),
        // zero-width space, used either side of the route arrow
        '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{FEFF}' => None,
        other => Some(other),
    }
}

static CANCELLATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bcancell?ed\b").expect("valid regex"));

static FLIGHT_SUMMARY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)
        ^
        (?:[^:]{1,120}:\s*)?            # optional traveller label, deliberately discarded
        (?:✈\s*)?                       # optional plane glyph
        (?P<origin>[A-Z]{3})
        \s*(?:→|➡|➙|->)\s*
        (?P<destination>[A-Z]{3})
        \s*[•·|]\s*                     # bullet between route and designator
        (?P<carrier>[A-Z0-9]{2,3})
        \s+                             # a separator is required; without one the event is ambiguous
        (?P<number>[0-9]{1,4})
        \s*$
        ",
    )
    .expect("valid regex")
});

#[cfg(test)]
mod tests {
    //! Characterization tests for Flighty calendar parsing, driven by sanitized fixtures.
    //! Characterization tests for Flighty calendar parsing, driven by sanitized fixtures.
    //!
    //! Snapshots are built straight from raw events here rather than through
    //! `CalendarReader`, so the parser is exercised on its own; `source_event` below
    //! applies the same raw-event mapping the reader does.

    use super::*;
    use crate::models::{Snapshot, SnapshotAuthority, SourceEvent};
    use chrono::{DateTime, TimeZone, Utc};
    use serde_json::{Value, json};
    use std::{collections::BTreeMap, path::Path};

    // The real export separates the carrier code from the flight number with U+00A0 and
    // brackets the route arrow with U+200B. Every special character below is built as an
    // escape so the literals stay plain ASCII and no invisible character hides in this file.
    const NBSP: char = '\u{00A0}';
    const ZWSP: char = '\u{200B}';
    const PLANE: char = '\u{2708}';
    const ARROW: char = '\u{2192}';
    const BULLET: char = '\u{2022}';

    fn friend_designator() -> String {
        format!("VY{NBSP}8721")
    }

    fn friend_summary() -> String {
        format!(
            "<redacted-name>: {PLANE} DUB{ZWSP}{ARROW}{ZWSP}BCN {BULLET} {}",
            friend_designator()
        )
    }

    fn observed_at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 21, 6, 0, 0).unwrap()
    }

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    fn load_fixture_events(name: &str) -> Vec<Value> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/google_calendar")
            .join(name);
        let payload: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(payload["authority"], "authoritative");
        payload["events"].as_array().unwrap().clone()
    }

    /// Mirror `calendar._source_event`: the parser must see production shapes.
    fn source_event(raw: &Value) -> SourceEvent {
        let boundary = |field: &str| -> Option<DateTime<Utc>> {
            let value = raw.get(field)?;
            let date_time = value.get("dateTime")?.as_str()?;
            Some(
                DateTime::parse_from_rfc3339(date_time)
                    .unwrap()
                    .with_timezone(&Utc),
            )
        };
        let fields: BTreeMap<String, Value> = raw
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        SourceEvent {
            event_id: raw["id"].as_str().unwrap().to_owned(),
            summary: raw
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            starts_at: boundary("start"),
            ends_at: boundary("end"),
            status: raw
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("confirmed")
                .to_owned(),
            updated_at: raw
                .get("updated")
                .and_then(Value::as_str)
                .map(|s| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)),
            observed_at: observed_at(),
            fields,
        }
    }

    fn snapshot_of(events: &[Value]) -> Snapshot {
        Snapshot::authoritative(observed_at(), events.iter().map(source_event).collect())
    }

    struct FriendEvent {
        event_id: &'static str,
        summary: Option<String>,
        start: &'static str,
    }

    impl FriendEvent {
        fn new(event_id: &'static str) -> Self {
            Self {
                event_id,
                summary: None,
                start: "2026-09-21T09:55:00+01:00",
            }
        }

        fn summary(mut self, summary: String) -> Self {
            self.summary = Some(summary);
            self
        }

        fn start(mut self, start: &'static str) -> Self {
            self.start = start;
            self
        }

        fn build(self) -> Value {
            json!({
                "id": self.event_id,
                "status": "confirmed",
                "summary": self.summary.unwrap_or_else(friend_summary),
                "description": format!("Vueling{NBSP}8721\nDublin to Barcelona\n\nSynced by Flighty\nwww.flighty.app"),
                "start": {"dateTime": self.start, "timeZone": "Europe/Dublin"},
                "end": {"dateTime": "2026-09-21T13:35:00+02:00", "timeZone": "Europe/Madrid"},
                "updated": "2026-09-20T18:00:00Z",
            })
        }
    }

    fn friend_event(event_id: &'static str) -> Value {
        FriendEvent::new(event_id).build()
    }

    fn with_designator(designator: &str) -> String {
        friend_summary().replace(&friend_designator(), designator)
    }

    fn outcomes(snapshot: &Snapshot) -> BTreeMap<String, ParseOutcome> {
        parse_cycle(snapshot)
            .interpretations
            .into_iter()
            .map(|item| (item.event_id, item.outcome))
            .collect()
    }

    fn cycle_of(events: &[Value]) -> ParsedCycle {
        parse_cycle(&snapshot_of(events))
    }

    fn first_flight(events: &[Value]) -> DesiredFlight {
        cycle_of(events)
            .flights
            .into_iter()
            .next()
            .expect("at least one flight")
    }

    fn designators(cycle: &ParsedCycle) -> Vec<String> {
        cycle
            .flights
            .iter()
            .map(DesiredFlight::designator)
            .collect()
    }

    #[test]
    fn live_fixture_parses_into_two_distinct_flights() {
        let cycle = cycle_of(&load_fixture_events("friend-flight.json"));

        assert_eq!(cycle.authority, SnapshotAuthority::Authoritative);
        assert_eq!(cycle.reason, None);
        assert_eq!(designators(&cycle), ["VY8721", "BA5"]);
        assert_eq!(
            cycle
                .flights
                .iter()
                .map(DesiredFlight::route)
                .collect::<Vec<_>>(),
            ["DUB-BCN", "LHR-HND"]
        );
        assert_eq!(
            cycle.flights[0].scheduled_departure,
            utc(2026, 9, 21, 8, 55)
        );
        assert_eq!(
            cycle.flights[1].scheduled_departure,
            utc(2026, 10, 24, 11, 40)
        );
        assert!(
            cycle
                .interpretations
                .iter()
                .all(|item| item.outcome == ParseOutcome::Flight)
        );
    }

    #[test]
    fn parsed_flights_carry_no_friend_or_reservation_text() {
        let cycle = cycle_of(&load_fixture_events("friend-flight.json"));

        let rendered = format!("{:?}", cycle.flights);

        assert!(!rendered.contains("redacted-name"));
        assert!(!rendered.contains("Vueling"));
        assert!(!rendered.contains("Dublin"));
        assert!(!rendered.to_lowercase().contains("flighty"));
        assert!(!rendered.contains(NBSP));
        assert!(!rendered.contains(ZWSP));
    }

    #[test]
    fn flight_number_is_normalized_without_padding_or_separators() {
        let padded = FriendEvent::new("event-padded")
            .summary(with_designator(&format!("VY{NBSP}0005")))
            .build();

        let cycle = cycle_of(&[padded]);

        assert_eq!(cycle.flights[0].carrier, "VY");
        assert_eq!(cycle.flights[0].number, "5");
        assert_eq!(cycle.flights[0].designator(), "VY5");
    }

    #[test]
    fn two_friends_on_one_flight_aggregate_into_a_single_entry() {
        let cycle = cycle_of(&[friend_event("event-a"), friend_event("event-b")]);

        assert_eq!(cycle.flights.len(), 1);
        assert_eq!(cycle.flights[0].source_event_ids, ["event-a", "event-b"]);
    }

    #[test]
    fn removing_one_of_two_source_events_retains_the_flight() {
        let both = first_flight(&[friend_event("event-a"), friend_event("event-b")]);
        let remaining = first_flight(&[friend_event("event-a")]);

        assert_eq!(remaining.key, both.key);
        assert_eq!(remaining.source_event_ids, ["event-a"]);
    }

    #[test]
    fn daylight_saving_transition_is_resolved_by_the_events_own_offset() {
        // 01:30 local occurs twice in Dublin on 25 October 2026; only the offset disambiguates.
        let before = first_flight(&[FriendEvent::new("event-before")
            .start("2026-10-25T01:30:00+01:00")
            .build()]);
        let after = first_flight(&[FriendEvent::new("event-after")
            .start("2026-10-25T01:30:00+00:00")
            .build()]);

        assert_eq!(before.scheduled_departure, utc(2026, 10, 25, 0, 30));
        assert_eq!(after.scheduled_departure, utc(2026, 10, 25, 1, 30));
        assert_eq!(before.key, after.key);
    }

    #[test]
    fn departure_key_uses_the_utc_day_not_the_local_calendar_day() {
        let late = first_flight(&[FriendEvent::new("event-late")
            .start("2026-09-22T00:30:00+01:00")
            .build()]);

        assert_eq!(late.scheduled_departure, utc(2026, 9, 21, 23, 30));
        assert_eq!(late.key, "VY8721:DUB:2026-09-21");
    }

    #[test]
    fn cancelled_event_contributes_no_source_reference() {
        let cancelled = load_fixture_events("cancelled-flight.json");
        let mut events = vec![friend_event("event-live")];
        events.extend(cancelled.iter().cloned());

        let cycle = cycle_of(&events);

        assert_eq!(cycle.authority, SnapshotAuthority::Authoritative);
        assert_eq!(
            cycle
                .flights
                .iter()
                .map(|f| f.source_event_ids.clone())
                .collect::<Vec<_>>(),
            [vec!["event-live".to_owned()]]
        );
        assert_eq!(
            outcomes(&snapshot_of(&cancelled))["event-b8cdee8f30dc"],
            ParseOutcome::Cancelled
        );
    }

    #[test]
    fn cancelled_flighty_event_does_not_fail_the_cycle_when_unparsable() {
        let marked = FriendEvent::new("event-cancelled")
            .summary(format!("<redacted-name>: {PLANE} Cancelled flight"))
            .build();

        let cycle = cycle_of(&[marked]);

        assert_eq!(cycle.authority, SnapshotAuthority::Authoritative);
        assert!(cycle.flights.is_empty());
        assert_eq!(cycle.interpretations[0].outcome, ParseOutcome::Cancelled);
    }

    #[test]
    fn reschedule_within_the_day_updates_one_flight_key() {
        let original = first_flight(&[friend_event("event-a")]);
        let delayed = first_flight(&[FriendEvent::new("event-a")
            .start("2026-09-21T14:20:00+01:00")
            .build()]);

        assert_eq!(delayed.key, original.key);
        assert_eq!(
            delayed.scheduled_departure,
            original.scheduled_departure
                + chrono::Duration::hours(4)
                + chrono::Duration::minutes(25)
        );
    }

    #[test]
    fn reschedule_to_another_day_produces_a_new_flight_key() {
        let original = first_flight(&[friend_event("event-a")]);
        let moved = first_flight(&[FriendEvent::new("event-a")
            .start("2026-09-23T09:55:00+01:00")
            .build()]);

        assert_ne!(moved.key, original.key);
    }

    #[test]
    fn codeshare_duplicate_fails_the_cycle_instead_of_guessing_two_flights() {
        let codeshare = FriendEvent::new("event-codeshare")
            .summary(with_designator(&format!("IB{NBSP}5432")))
            .build();

        let cycle = cycle_of(&[friend_event("event-a"), codeshare]);

        assert_eq!(cycle.authority, SnapshotAuthority::NonAuthoritative);
        assert!(cycle.flights.is_empty());
        let reason = cycle.reason.expect("a reason");
        assert!(reason.starts_with("parse_codeshare_ambiguous:"), "{reason}");
        assert_eq!(reason, "parse_codeshare_ambiguous:IB5432,VY8721");
    }

    #[test]
    fn malformed_flight_number_fails_the_whole_cycle_closed() {
        let malformed = FriendEvent::new("event-bad")
            .summary(with_designator(&format!("VY{NBSP}87X1")))
            .build();

        let cycle = cycle_of(&[friend_event("event-good"), malformed]);

        assert_eq!(cycle.authority, SnapshotAuthority::NonAuthoritative);
        assert!(cycle.flights.is_empty());
        assert_eq!(
            cycle.reason.as_deref(),
            Some("parse_event_unrecognized:event-bad")
        );
    }

    #[test]
    fn failed_cycle_reason_contains_no_event_text() {
        let malformed = FriendEvent::new("event-bad")
            .summary(format!(
                "<redacted-name>: {PLANE} secret routing note {BULLET} VY{NBSP}87X1"
            ))
            .build();

        let cycle = cycle_of(&[malformed]);

        let reason = cycle.reason.expect("a reason");
        assert!(!reason.contains("secret routing note"));
        assert!(!reason.contains("redacted-name"));
    }

    #[test]
    fn all_day_event_is_skipped_without_failing_the_cycle() {
        let mut all_day = friend_event("event-all-day");
        all_day["start"] = json!({"date": "2026-09-21"});
        all_day["end"] = json!({"date": "2026-09-22"});

        let cycle = cycle_of(&[all_day]);

        assert_eq!(cycle.authority, SnapshotAuthority::Authoritative);
        assert!(cycle.flights.is_empty());
        assert_eq!(cycle.interpretations[0].outcome, ParseOutcome::AllDay);
    }

    #[test]
    fn event_without_any_start_is_skipped_without_failing_the_cycle() {
        let mut undated = friend_event("event-undated");
        undated.as_object_mut().unwrap().remove("start");

        let cycle = cycle_of(&[undated]);

        assert_eq!(cycle.authority, SnapshotAuthority::Authoritative);
        assert!(cycle.flights.is_empty());
        assert_eq!(
            cycle.interpretations[0].outcome,
            ParseOutcome::MissingDeparture
        );
    }

    #[test]
    fn missing_route_context_fails_the_cycle_closed() {
        let routeless = FriendEvent::new("event-routeless")
            .summary(format!("<redacted-name>: {PLANE} {}", friend_designator()))
            .build();

        let cycle = cycle_of(&[routeless]);

        assert_eq!(cycle.authority, SnapshotAuthority::NonAuthoritative);
        assert_eq!(
            cycle.reason.as_deref(),
            Some("parse_event_unrecognized:event-routeless")
        );
    }

    #[test]
    fn every_event_on_the_calendar_must_parse_as_a_flight() {
        // The calendar is dedicated to flights, so there is no "unrelated event" category:
        // anything that does not parse fails the cycle loudly rather than being skipped.
        let unrelated = json!({
            "id": "event-dentist",
            "status": "confirmed",
            "summary": "Dentist",
            "start": {"dateTime": "2026-09-21T09:00:00+01:00", "timeZone": "Europe/Dublin"},
            "end": {"dateTime": "2026-09-21T09:30:00+01:00", "timeZone": "Europe/Dublin"},
        });

        let cycle = cycle_of(&[friend_event("event-a"), unrelated.clone()]);

        assert_eq!(cycle.authority, SnapshotAuthority::NonAuthoritative);
        assert_eq!(
            cycle.reason.as_deref(),
            Some("parse_event_unrecognized:event-dentist")
        );
        assert!(cycle.flights.is_empty());
        assert_eq!(
            outcomes(&snapshot_of(&[unrelated]))["event-dentist"],
            ParseOutcome::Unrecognized
        );
    }

    #[test]
    fn a_flight_without_any_flighty_marker_still_parses() {
        // A hand-written event in the same shape, no plane glyph, no Flighty footer.
        let plain = json!({
            "id": "event-plain",
            "status": "confirmed",
            "summary": "DUB -> BCN | VY 8721",
            "start": {"dateTime": "2026-09-21T09:55:00+01:00", "timeZone": "Europe/Dublin"},
            "end": {"dateTime": "2026-09-21T13:20:00+02:00", "timeZone": "Europe/Madrid"},
        });

        let cycle = cycle_of(&[plain]);

        assert_eq!(cycle.authority, SnapshotAuthority::Authoritative);
        assert_eq!(designators(&cycle), ["VY8721"]);
    }

    #[test]
    fn non_authoritative_snapshot_never_yields_flights() {
        let failed = Snapshot::failed(observed_at(), "calendar_request_failed:TimeoutError");

        let cycle = parse_cycle(&failed);

        assert_eq!(cycle.authority, SnapshotAuthority::NonAuthoritative);
        assert!(cycle.flights.is_empty());
        assert!(cycle.interpretations.is_empty());
        assert_eq!(
            cycle.reason.as_deref(),
            Some("calendar_request_failed:TimeoutError")
        );
    }

    #[test]
    fn flight_order_is_deterministic_regardless_of_event_order() {
        let mut later = FriendEvent::new("event-later")
            .summary(with_designator(&format!("BA{NBSP}5")))
            .build();
        later["start"] =
            json!({"dateTime": "2026-10-24T12:40:00+01:00", "timeZone": "Europe/London"});
        let earlier = friend_event("event-earlier");

        let forward = cycle_of(&[earlier.clone(), later.clone()]);
        let reverse = cycle_of(&[later, earlier]);

        let keys = |cycle: &ParsedCycle| {
            cycle
                .flights
                .iter()
                .map(|f| f.key.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(&forward), keys(&reverse));
        assert_eq!(designators(&forward), ["VY8721", "BA5"]);
    }

    #[test]
    fn freshest_event_wins_when_two_disagree_on_departure() {
        // Same leg, same UTC day, but one event was updated later and moved the departure.
        let stale = json!({
            "id": "event-stale",
            "status": "confirmed",
            "summary": friend_summary(),
            "start": {"dateTime": "2026-09-21T09:55:00+01:00"},
            "updated": "2026-09-20T10:00:00Z",
        });
        let fresh = json!({
            "id": "event-fresh",
            "status": "confirmed",
            "summary": friend_summary(),
            "start": {"dateTime": "2026-09-21T11:10:00+01:00"},
            "updated": "2026-09-20T18:00:00Z",
        });

        let flight = first_flight(&[stale, fresh]);

        assert_eq!(flight.scheduled_departure, utc(2026, 9, 21, 10, 10));
        assert_eq!(flight.source_event_ids, ["event-fresh", "event-stale"]);
    }
}
