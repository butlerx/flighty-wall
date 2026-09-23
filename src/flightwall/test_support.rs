//! Fixtures shared by the flightwall submodules' tests.
//!
//! Every request/response shape comes from `tests/fixtures/flightwall/`; nothing is
//! invented.

use super::credentials::FlightWallCredentials;
use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;
use std::{fs, path::Path};

pub(crate) fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 22, 12, 0, 0).unwrap()
}

pub(crate) fn credentials() -> FlightWallCredentials {
    FlightWallCredentials::new("k".repeat(43), format!("fw_ios_{}", "u".repeat(22)))
}

pub(crate) fn fixture(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/flightwall")
        .join(name);
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

pub(crate) fn fixture_document(name: &str, side: &str) -> Value {
    fixture(name)[side]["body"]["json"].clone()
}

pub(crate) fn response_document(name: &str) -> Value {
    fixture_document(name, "response")
}
