//! Fixtures shared by the service submodules' tests.

use chrono::{DateTime, TimeZone, Utc};

pub(crate) fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 22, 12, 0, 0).unwrap()
}
