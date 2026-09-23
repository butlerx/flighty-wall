//! Shared redaction of secrets and personal data before anything reaches disk.
//!
//! Both the calendar fixture writer and the `FlightWall` capture sanitizer scrub through
//! this module so a pattern added for one source protects the other. Every rule here is
//! deliberately over-broad: losing a little fixture fidelity is cheaper than committing a
//! token, a home location, or a Friend's name.

use regex::{Regex, RegexBuilder};
use serde_json::{Map, Value};
use std::{collections::HashSet, sync::LazyLock};

/// Placeholder that keeps a field visible in a fixture while discarding its contents.
pub const REDACTED_BY_KEY: &str = "<redacted>";

const REDACTED_COORDINATE: &str = "<redacted-coordinate>";

pub static EMAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}").expect("valid regex")
});
pub static URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)[A-Z][A-Z0-9+.-]*://\S+").expect("valid regex"));
pub static UUID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[0-9A-F]{8}-[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{12}\b")
        .expect("valid regex")
});
pub static BOOKING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(confirmation|reservation|booking)(?:\s+(?:code|number))?\s*[:#-]?\s*[A-Z0-9-]+",
    )
    .expect("valid regex")
});
pub static SEAT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bseat\s*[:#-]?\s*[A-Z0-9-]+").expect("valid regex"));
pub static JWT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\beyJ[A-Za-z0-9_-]{6,}\.[A-Za-z0-9_-]{6,}\.[A-Za-z0-9_-]{6,}")
        .expect("valid regex")
});
pub static CREDENTIAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(bearer|basic|token|secret|password|api[_-]?key)\b\s*[:=]?\s*[A-Za-z0-9._~+/=-]{8,}",
    )
    .expect("valid regex")
});
pub static OPAQUE_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[A-Za-z0-9_-]{32,}\b").expect("valid regex"));
pub static COORDINATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-?\d{1,3}\.\d{4,}").expect("valid regex"));

/// A float is a coordinate only when its whole rendering matches, not a substring.
static WHOLE_COORDINATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^-?\d{1,3}\.\d{4,}$").expect("valid regex"));

/// Pattern rules in application order. JWTs go before UUIDs and opaque tokens so a
/// dotted token is replaced whole rather than being split into unrelated fragments.
fn replacements() -> [(&'static Regex, &'static str); 9] {
    [
        (&EMAIL, "<redacted-email>"),
        (&URL, "<redacted-url>"),
        (&JWT, "<redacted-token>"),
        (&UUID, "<redacted-uuid>"),
        (&CREDENTIAL, "${1} <redacted-credential>"),
        (&OPAQUE_TOKEN, "<redacted-token>"),
        (&BOOKING, "${1}: <redacted>"),
        (&SEAT, "Seat: <redacted>"),
        (&COORDINATE, REDACTED_COORDINATE),
    ]
}

/// What to scrub beyond the built-in patterns.
///
/// Built once per fixture run so the caller-supplied term patterns are compiled a single
/// time rather than once per string leaf in the payload.
#[derive(Debug, Default, Clone)]
pub struct Rules {
    terms: Vec<Regex>,
    dropped_keys: HashSet<String>,
    redacted_keys: HashSet<String>,
}

impl Rules {
    /// No extra terms, no key rules: only the built-in patterns apply.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Literal strings to replace with `<redacted-name>`, case-insensitively.
    ///
    /// Empty terms are ignored: they would otherwise match everywhere and destroy the
    /// fixture. A term too large to compile (past the regex size limit) is also dropped;
    /// `regex::escape` guarantees the syntax itself is valid.
    #[must_use]
    pub fn with_terms<I, S>(mut self, terms: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.terms.extend(
            terms
                .into_iter()
                .filter(|term| !term.as_ref().is_empty())
                .filter_map(|term| {
                    RegexBuilder::new(&regex::escape(term.as_ref()))
                        .case_insensitive(true)
                        .build()
                        .ok()
                }),
        );
        self
    }

    /// Keys removed entirely, at every depth. Matched exactly.
    #[must_use]
    pub fn dropping_keys<I, S>(mut self, keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.dropped_keys.extend(keys.into_iter().map(Into::into));
        self
    }

    /// Keys whose value becomes [`REDACTED_BY_KEY`], at every depth. Matched
    /// case-insensitively.
    #[must_use]
    pub fn redacting_keys<I, S>(mut self, keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.redacted_keys
            .extend(keys.into_iter().map(|key| key.as_ref().to_lowercase()));
        self
    }

    /// Replace every secret or personal pattern, then any caller-supplied literals.
    #[must_use]
    pub fn scrub_text(&self, value: &str) -> String {
        let after_patterns = replacements()
            .iter()
            .fold(value.to_owned(), |text, (pattern, replacement)| {
                pattern.replace_all(&text, *replacement).into_owned()
            });
        self.terms.iter().fold(after_patterns, |text, term| {
            term.replace_all(&text, "<redacted-name>").into_owned()
        })
    }

    /// Scrub a decoded JSON value, dropping any key named in the drop list.
    #[must_use]
    pub fn scrub_value(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.scrub_text(text)),
            Value::Object(map) => Value::Object(self.scrub_object(map)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| self.scrub_value(item)).collect())
            }
            Value::Number(number) if number.is_f64() => number
                .as_f64()
                .map_or_else(|| value.clone(), |float| scrub_float(float, value)),
            other => other.clone(),
        }
    }

    /// Scrub every value in an object, dropping keys that must never be committed.
    ///
    /// A key listed for redaction keeps its name but loses its value entirely, including
    /// any nested structure. Pattern matching cannot recognise a short opaque secret, so
    /// for those fields the key name is the only reliable signal and shape fidelity is
    /// forfeited.
    #[must_use]
    pub fn scrub_object(&self, map: &Map<String, Value>) -> Map<String, Value> {
        map.iter()
            .filter(|(key, _)| !self.dropped_keys.contains(key.as_str()))
            .map(|(key, item)| {
                let scrubbed = if self.redacted_keys.contains(&key.to_lowercase()) {
                    Value::String(REDACTED_BY_KEY.to_owned())
                } else {
                    self.scrub_value(item)
                };
                (key.clone(), scrubbed)
            })
            .collect()
    }
}

/// Replace every secret or personal pattern using only the built-in rules.
#[must_use]
pub fn scrub_text(value: &str) -> String {
    Rules::new().scrub_text(value)
}

/// Blunt a precise coordinate while leaving ordinary numbers intact.
///
/// `{:?}` renders `25.0` as `25.0` (never `25`), so a whole
/// number always has too few decimals to look like a coordinate.
fn scrub_float(float: f64, original: &Value) -> Value {
    if WHOLE_COORDINATE.is_match(&format!("{float:?}")) {
        Value::String(REDACTED_COORDINATE.to_owned())
    } else {
        original.clone()
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the shared redaction rules.
    //!
    //! Both fixture writers depend on this module, so every rule is pinned here rather than
    //! re-tested per caller. Each secret below is a fabricated sample, not a real credential.

    use super::*;
    use serde_json::{Value, json};

    const FAKE_JWT: &str =
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r_wW1g";
    const FAKE_OPAQUE: &str = "AKIA6BHTGZ5R4WQ2PLNVX3CDMJ7YKF8ES1U0";

    fn scrub_value(value: &Value) -> Value {
        Rules::new().scrub_value(value)
    }

    #[test]
    fn scrubs_email() {
        assert!(!scrub_text("ping friend.name@example.invalid about it").contains('@'));
    }

    #[test]
    fn scrubs_url_including_private_calendar_and_deeplink() {
        let redacted =
            scrub_text("open flighty://flight/123 or https://calendar.google.com/x/private-abc");
        assert!(!redacted.contains("flighty://"));
        assert!(!redacted.contains("calendar.google.com"));
    }

    #[test]
    fn scrubs_jwt_before_any_other_rule_can_split_it() {
        let redacted = scrub_text(&format!("Authorization: Bearer {FAKE_JWT}"));
        assert!(!redacted.contains("eyJ"));
        assert!(redacted.contains("<redacted-token>"));
    }

    #[test]
    fn scrubs_uuid() {
        let redacted = scrub_text("device 4f3c2a19-7b6e-4d51-9a8f-2c1b0e5d7a63 reported");
        assert!(!redacted.contains("4f3c2a19"));
    }

    #[test]
    fn scrubs_credential_but_keeps_the_scheme_name_visible() {
        // The scheme is the part of the contract worth keeping; the value never is.
        let redacted = scrub_text("api_key=sk-live-9d82hf03mfkq");
        assert!(!redacted.contains("sk-live"));
        assert!(redacted.contains("api_key"));
    }

    #[test]
    fn scrubs_long_opaque_token() {
        assert!(!scrub_text(&format!("session {FAKE_OPAQUE} ok")).contains(FAKE_OPAQUE));
    }

    #[test]
    fn scrubs_booking_and_seat_codes() {
        let redacted = scrub_text("Confirmation: XR7K2Q\nSeat: 14F");
        assert!(!redacted.contains("XR7K2Q"));
        assert!(!redacted.contains("14F"));
    }

    #[test]
    fn scrubs_precise_coordinates_in_text_and_as_floats() {
        let redacted = scrub_text("centre 53.349805,-6.260310");
        assert!(!redacted.contains("53.349805"));
        #[allow(clippy::unreadable_literal)] // a coordinate, not a count
        let scrubbed = scrub_value(&json!(53.349805));
        assert_eq!(scrubbed, json!("<redacted-coordinate>"));
    }

    #[test]
    fn keeps_ordinary_numbers_intact() {
        // Radius, altitude, and flight numbers are contract detail, not personal data.
        assert_eq!(scrub_value(&json!(25.0)), json!(25.0));
        assert_eq!(scrub_value(&json!(8721)), json!(8721));
        assert_eq!(scrub_value(&json!(35.5)), json!(35.5));
    }

    #[test]
    fn scrubs_caller_supplied_terms_case_insensitively() {
        let redacted = Rules::new()
            .with_terms(["alice smith"])
            .scrub_text("Flight for Alice Smith");
        assert!(!redacted.contains("Alice"));
        assert!(redacted.contains("<redacted-name>"));
    }

    #[test]
    fn ignores_empty_sensitive_terms() {
        // An empty --redact-term would otherwise match everywhere and destroy the fixture.
        assert_eq!(
            Rules::new().with_terms([""]).scrub_text("DUB-BCN"),
            "DUB-BCN"
        );
    }

    #[test]
    fn scrub_value_recurses_through_lists_and_mappings() {
        let scrubbed = scrub_value(&json!({"legs": [{"crew": "bob@example.invalid"}]}));
        assert!(!scrubbed.to_string().contains("bob@"));
    }

    #[test]
    fn dropped_keys_are_removed_at_every_depth() {
        let scrubbed = Rules::new()
            .dropping_keys(["attendees"])
            .scrub_value(&json!({"outer": {"attendees": ["someone"], "kept": 1}}));
        assert_eq!(scrubbed, json!({"outer": {"kept": 1}}));
    }

    #[test]
    fn redacted_keys_keep_the_name_and_discard_short_secrets() {
        // A six-character token defeats every length-based pattern, so the key name decides.
        let scrubbed = Rules::new()
            .redacting_keys(["token"])
            .scrub_value(&json!({"token": "abc123", "limit": 5}));
        assert_eq!(scrubbed, json!({"token": REDACTED_BY_KEY, "limit": 5}));
    }

    #[test]
    fn redacted_keys_match_regardless_of_case() {
        let scrubbed = Rules::new()
            .redacting_keys(["authorization"])
            .scrub_value(&json!({"Authorization": "Custom xyz"}));
        assert_eq!(scrubbed["Authorization"], json!(REDACTED_BY_KEY));
    }

    #[test]
    fn redacted_keys_discard_nested_structure_too() {
        let scrubbed = Rules::new()
            .redacting_keys(["session"])
            .scrub_value(&json!({"session": {"id": "s1", "user": {"email": "a@b.invalid"}}}));
        assert_eq!(scrubbed, json!({"session": REDACTED_BY_KEY}));
    }

    #[test]
    fn non_string_scalars_pass_through_untouched() {
        assert_eq!(scrub_value(&json!(null)), json!(null));
        assert_eq!(scrub_value(&json!(true)), json!(true));
        assert_eq!(scrub_value(&json!(-42)), json!(-42));
    }
}
