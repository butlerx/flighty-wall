//! Bounded Google Calendar reads and privacy-safe fixture sanitization.
//!
//! [`reader`] consumes every page of a bounded window or returns a non-authoritative
//! snapshot. Google's discovery client has no Rust port, so [`google`] speaks the Calendar
//! v3 REST surface directly, authorised by a bearer token that [`auth`] mints from a
//! service-account key.

pub mod auth;
pub mod google;
pub mod reader;
#[cfg(test)]
mod test_support;

pub use auth::{AuthError, ServiceAccountTokenSource};
pub use google::{GoogleCalendarGateway, JsonHttp, TokenSource, UreqJsonHttp};
pub use reader::{CalendarGateway, CalendarLimits, CalendarReader, GatewayError};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::redaction::Rules;

pub const CALENDAR_READONLY_SCOPE: &str = "https://www.googleapis.com/auth/calendar.readonly";
pub const TOKEN_URI: &str = "https://oauth2.googleapis.com/token";

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
    use serde_json::{Value, json};

    use super::test_support::{timed_event, timed_event_with};
    use super::*;

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
}
