//! One cycle: read the calendar, parse it, read the wall, plan, and (if asked) apply.

use chrono::{DateTime, Utc};

use crate::calendar::{CalendarGateway, CalendarReader};
use crate::flightwall::{WallSnapshot, WriteOutcome, WriteRefused, WriteResult};
use crate::models::Snapshot;
use crate::parser::{ParsedCycle, parse_cycle};
use crate::reconcile::{
    ApplyError, Plan, PlanError, RecoveryOutcome, Wanted, Writer, apply_plan, plan,
    recover_pending_write,
};
use crate::state::{StateError, StateStore};

/// How far one cycle got, and therefore what its report means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CycleStatus {
    CalendarNotAuthoritative,
    ParseNotAuthoritative,
    /// Calendar and parse were fine; the report carries provisional local intent only.
    WallNotAuthoritative,
    NoChange,
    /// A write was planned and not sent.
    DryRun,
    Applied,
    Rejected,
    /// The write did not complete; the journal was settled from a re-read where possible.
    Unknown,
}

impl CycleStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CalendarNotAuthoritative => "calendar_not_authoritative",
            Self::ParseNotAuthoritative => "parse_not_authoritative",
            Self::WallNotAuthoritative => "wall_not_authoritative",
            Self::NoChange => "no_change",
            Self::DryRun => "dry_run",
            Self::Applied => "applied",
            Self::Rejected => "rejected",
            Self::Unknown => "unknown",
        }
    }

    /// A source could not be read authoritatively; nothing was written.
    #[must_use]
    pub const fn is_non_authoritative(self) -> bool {
        matches!(
            self,
            Self::CalendarNotAuthoritative
                | Self::ParseNotAuthoritative
                | Self::WallNotAuthoritative
        )
    }

    /// The wall rejected the write or its outcome is unknown.
    #[must_use]
    pub const fn is_write_failure(self) -> bool {
        matches!(self, Self::Rejected | Self::Unknown)
    }
}

/// Everything one cycle observed and decided. Safe to log: no document, no descriptions.
#[derive(Debug, Clone)]
pub struct CycleReport {
    pub status: CycleStatus,
    pub started_at: DateTime<Utc>,
    pub calendar: Snapshot,
    pub parsed: Option<ParsedCycle>,
    pub wall: Option<WallSnapshot>,
    pub plan: Option<Plan>,
    pub write: Option<WriteResult>,
    pub recovery: Option<RecoveryOutcome>,
}

impl CycleReport {
    /// A report that stopped at `status` with nothing past the calendar read filled in.
    #[must_use]
    pub fn new(status: CycleStatus, started_at: DateTime<Utc>, calendar: Snapshot) -> Self {
        Self {
            status,
            started_at,
            calendar,
            parsed: None,
            wall: None,
            plan: None,
            write: None,
            recovery: None,
        }
    }

    /// Flight numbers the calendar wants, in parse order.
    #[must_use]
    pub fn wanted(&self) -> Vec<String> {
        self.parsed
            .as_ref()
            .map(|parsed| {
                parsed
                    .flights
                    .iter()
                    .map(crate::parser::DesiredFlight::designator)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// One journal-safe line: counts, designators, and the reason for any stop.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut parts = vec![format!("status={}", self.status.as_str())];
        if let Some(recovery) = self.recovery {
            parts.push(format!("recovery={}", recovery.as_str()));
        }
        parts.extend(self.source_parts());
        parts.extend(self.plan_parts());
        if let Some(reason) = self.write.as_ref().and_then(|w| w.reason.as_deref()) {
            parts.push(format!("write_reason={reason}"));
        }
        parts.join(" ")
    }

    fn source_parts(&self) -> Vec<String> {
        let mut parts = vec![format!("calendar_events={}", self.calendar.events.len())];
        if let Some(reason) = &self.calendar.reason {
            parts.push(format!("calendar_reason={reason}"));
        }
        if let Some(parsed) = &self.parsed {
            parts.push(format!("wanted={}", list(&self.wanted())));
            if let Some(reason) = &parsed.reason {
                parts.push(format!("parse_reason={reason}"));
            }
        }
        if let Some(wall) = &self.wall {
            parts.push(format!("wall={}", list(&wall.flight_numbers())));
            if let Some(reason) = &wall.reason {
                parts.push(format!("wall_reason={reason}"));
            }
        }
        parts
    }

    fn plan_parts(&self) -> Vec<String> {
        let Some(plan) = &self.plan else {
            return Vec::new();
        };
        let mut parts = vec![format!(
            "add={} remove={}",
            list(&plan.additions),
            list(&plan.removals)
        )];
        for (label, values) in [
            ("suppressed", &plan.suppressed),
            ("unplaceable", &plan.unplaceable),
            ("released", &plan.released),
        ] {
            if !values.is_empty() {
                parts.push(format!("{label}={}", list(values)));
            }
        }
        parts
    }
}

/// Render as `['A', 'B']`: the journal format the existing runbooks and greps expect.
fn list<S: AsRef<str>>(items: &[S]) -> String {
    let inner: Vec<String> = items.iter().map(|s| format!("'{}'", s.as_ref())).collect();
    format!("[{}]", inner.join(", "))
}

/// The two client methods a cycle needs.
pub trait Wall: Writer {
    fn read(&self) -> WallSnapshot;
}

impl<T: crate::flightwall::Transport> Wall for crate::flightwall::FlightWallClient<T> {
    fn read(&self) -> WallSnapshot {
        Self::read(self)
    }
}

impl<W: Wall + ?Sized> Wall for &W {
    fn read(&self) -> WallSnapshot {
        (**self).read()
    }
}

/// Why a cycle could not complete. Everything else is a status, not an error.
#[derive(Debug, thiserror::Error)]
pub enum CycleError {
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error(transparent)]
    Refused(#[from] WriteRefused),
}

impl From<ApplyError> for CycleError {
    fn from(error: ApplyError) -> Self {
        match error {
            ApplyError::State(e) => Self::State(e),
            ApplyError::Refused(e) => Self::Refused(e),
        }
    }
}

/// Run one full cycle. `apply = false` plans but never writes.
///
/// # Errors
///
/// [`CycleError`] when the journal fails or the planner is handed impossible input.
/// A non-authoritative read is not an error; it is a [`CycleStatus`].
pub fn run_cycle<G: CalendarGateway>(
    calendar: &CalendarReader<G>,
    wall: &impl Wall,
    store: &StateStore,
    now: DateTime<Utc>,
    apply: bool,
) -> Result<CycleReport, CycleError> {
    let calendar_snapshot = calendar.read_snapshot(now);
    if !calendar_snapshot.is_authoritative() {
        return Ok(CycleReport::new(
            CycleStatus::CalendarNotAuthoritative,
            now,
            calendar_snapshot,
        ));
    }

    let parsed = parse_cycle(&calendar_snapshot);
    if !parsed.is_authoritative() {
        let mut report =
            CycleReport::new(CycleStatus::ParseNotAuthoritative, now, calendar_snapshot);
        report.parsed = Some(parsed);
        return Ok(report);
    }

    let mut wall_snapshot = wall.read();
    let recovery = recover_pending_write(store, &wall_snapshot, None)?;
    if recovery.is_some() && wall_snapshot.is_authoritative() {
        // The journal changed; read the wall again so the plan sees the settled state.
        wall_snapshot = wall.read();
    }
    let mut report = CycleReport::new(CycleStatus::WallNotAuthoritative, now, calendar_snapshot);
    report.recovery = recovery;
    if !wall_snapshot.is_authoritative() {
        report.parsed = Some(parsed);
        report.wall = Some(wall_snapshot);
        return Ok(report);
    }

    let the_plan = plan(&wanted(&parsed), &wall_snapshot, &store.owned_flights()?)?;
    report.parsed = Some(parsed);

    if !apply {
        report.status = if the_plan.changed() {
            CycleStatus::DryRun
        } else {
            CycleStatus::NoChange
        };
        report.wall = Some(wall_snapshot);
        report.plan = Some(the_plan);
        return Ok(report);
    }

    let write = apply_plan(wall, store, &wall_snapshot, &the_plan, now)?;
    report.status = match write.as_ref().map(|w| w.outcome) {
        None => CycleStatus::NoChange,
        Some(WriteOutcome::Applied) => CycleStatus::Applied,
        Some(WriteOutcome::Rejected) => CycleStatus::Rejected,
        Some(WriteOutcome::Unknown) => CycleStatus::Unknown,
    };
    report.wall = Some(wall_snapshot);
    report.plan = Some(the_plan);
    report.write = write;
    Ok(report)
}

/// One `Wanted` per flight number, keeping the earliest departure.
///
/// The wall keys entries by `flight_number` alone and has no date, so a number that flies
/// twice in the window (`AA577` on the 28th and again on the 9th) is one entry.
/// `parsed.flights` is sorted by departure, so the first occurrence is the soonest, and the
/// later leg becomes wanted on its own once the earlier one has landed and the server has
/// dropped it.
fn wanted(parsed: &ParsedCycle) -> Vec<Wanted> {
    let mut seen = std::collections::BTreeSet::new();
    parsed
        .flights
        .iter()
        .filter(|flight| seen.insert(flight.designator()))
        .map(|flight| Wanted::new(flight.designator(), flight.key.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::super::test_support::now;
    use super::*;
    use crate::calendar::{CalendarLimits, GatewayError};
    use crate::flightwall::{Fingerprint, TrackedFlight, TransportError, WriteResult};

    // --- fakes -------------------------------------------------------------------------

    /// A calendar event in the shape Flighty exports (see the `google_calendar` fixture README).
    fn flighty_event(
        event_id: &str,
        friend: &str,
        carrier: &str,
        number: &str,
        route: &str,
    ) -> Value {
        let (origin, destination) = route.split_once('-').unwrap();
        json!({
            "id": event_id,
            "status": "confirmed",
            "summary": format!("{friend}: \u{2708} {origin}\u{200b}\u{2192}\u{200b}{destination} \u{2022} {carrier}\u{a0}{number}"),
            "description": format!("{carrier} {number}\n{origin} to {destination}\n\u{2197} 10:00 IST\n\u{2198} 13:00 CET"),
            "location": origin,
            "start": {"dateTime": "2026-10-24T10:00:00+01:00", "timeZone": "Europe/Dublin"},
            "end": {"dateTime": "2026-10-24T13:00:00+02:00", "timeZone": "Europe/Madrid"},
            "updated": "2026-09-20T09:00:00Z",
        })
    }

    struct FixtureGateway(Result<Value, GatewayError>);

    impl CalendarGateway for FixtureGateway {
        fn list_events_page(
            &self,
            _calendar_id: &str,
            _time_min: DateTime<Utc>,
            _time_max: DateTime<Utc>,
            _page_token: Option<&str>,
        ) -> Result<Value, GatewayError> {
            self.0.clone()
        }
    }

    fn limits() -> CalendarLimits {
        CalendarLimits {
            max_pages: 10,
            max_events: 500,
            max_field_chars: 8192,
            max_snapshot_bytes: 1 << 20,
        }
    }

    fn calendar(events: &[Value]) -> CalendarReader<FixtureGateway> {
        CalendarReader::new(
            FixtureGateway(Ok(json!({"items": events}))),
            "friends@example.invalid",
            60,
            0,
            limits(),
        )
    }

    fn sample_calendar(friend: &str) -> CalendarReader<FixtureGateway> {
        calendar(&[flighty_event("e1", friend, "VY", "8721", "DUB-BCN")])
    }

    fn broken_calendar() -> CalendarReader<FixtureGateway> {
        CalendarReader::new(
            FixtureGateway(Err(GatewayError::Transport(TransportError::new("timeout")))),
            "friends@example.invalid",
            60,
            0,
            limits(),
        )
    }

    fn fingerprint() -> Fingerprint {
        Fingerprint {
            model: Some("mini-v1".into()),
            top_level_keys: ["display_config", "request_config", "version"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            tracked_flight_keys: std::collections::BTreeSet::default(),
        }
    }

    fn wall_snapshot(numbers: &[&str]) -> WallSnapshot {
        WallSnapshot::authoritative(
            now(),
            numbers
                .iter()
                .map(|n| TrackedFlight {
                    flight_number: (*n).to_owned(),
                    created_at: "2026-09-01T00:00:00.000Z".to_owned(),
                    show_distance_travelled: true,
                    show_metrics: true,
                })
                .collect(),
            fingerprint(),
            json!({
                "display_config": {"model": "mini-v1"},
                "request_config": {"tracked_flights": []},
                "version": 2,
            }),
        )
    }

    /// Stands in for `FlightWallClient`: scripted reads, recorded writes.
    struct FakeWall {
        reads: RefCell<VecDeque<WallSnapshot>>,
        write_outcome: WriteOutcome,
        writes: RefCell<Vec<Vec<String>>>,
    }

    impl FakeWall {
        fn new(reads: Vec<WallSnapshot>) -> Self {
            Self::with_outcome(reads, WriteOutcome::Applied)
        }

        fn with_outcome(reads: Vec<WallSnapshot>, write_outcome: WriteOutcome) -> Self {
            Self {
                reads: RefCell::new(reads.into()),
                write_outcome,
                writes: RefCell::new(Vec::new()),
            }
        }

        fn writes(&self) -> Vec<Vec<String>> {
            self.writes.borrow().clone()
        }
    }

    impl Wall for FakeWall {
        /// Pop scripted reads until one is left, then repeat it forever.
        fn read(&self) -> WallSnapshot {
            let mut reads = self.reads.borrow_mut();
            if reads.len() > 1 {
                reads.pop_front().unwrap()
            } else {
                reads.front().cloned().expect("at least one scripted read")
            }
        }
    }

    impl Writer for FakeWall {
        fn replace_tracked_flights(
            &self,
            _snapshot: &WallSnapshot,
            flights: &[TrackedFlight],
        ) -> Result<WriteResult, WriteRefused> {
            let numbers: Vec<String> = flights.iter().map(|f| f.flight_number.clone()).collect();
            self.writes.borrow_mut().push(numbers.clone());
            let after = if self.write_outcome == WriteOutcome::Rejected {
                self.reads.borrow().front().cloned().unwrap()
            } else {
                let refs: Vec<&str> = numbers.iter().map(String::as_str).collect();
                wall_snapshot(&refs)
            };
            Ok(WriteResult {
                outcome: self.write_outcome,
                snapshot: after,
                reason: (self.write_outcome != WriteOutcome::Applied).then(|| "x".to_owned()),
            })
        }
    }

    fn store(dir: &TempDir) -> StateStore {
        StateStore::open(dir.path().join("state").join("state.sqlite3")).unwrap()
    }

    fn owned_numbers(journal: &StateStore) -> Vec<String> {
        journal.owned_flights().unwrap().into_keys().collect()
    }

    fn cycle(
        dir: &TempDir,
        cal: &CalendarReader<FixtureGateway>,
        wall: &FakeWall,
        apply: bool,
    ) -> (CycleReport, StateStore) {
        let journal = store(dir);
        let report = run_cycle(cal, wall, &journal, now(), apply).unwrap();
        (report, journal)
    }

    // --- run_cycle ---------------------------------------------------------------------

    #[test]
    fn dry_run_plans_the_add_and_writes_nothing() {
        let dir = TempDir::new().unwrap();
        let wall = FakeWall::new(vec![wall_snapshot(&["EI61"])]);
        let (report, journal) = cycle(&dir, &sample_calendar("Alice"), &wall, false);

        assert_eq!(report.status, CycleStatus::DryRun);
        assert_eq!(report.wanted(), ["VY8721"]);
        assert_eq!(report.plan.as_ref().unwrap().additions, ["VY8721"]);
        assert!(wall.writes().is_empty());
        assert!(owned_numbers(&journal).is_empty());
        assert!(
            report.summary().contains("add=['VY8721']"),
            "{}",
            report.summary()
        );
    }

    #[test]
    fn apply_writes_once_and_records_ownership() {
        let dir = TempDir::new().unwrap();
        let wall = FakeWall::new(vec![wall_snapshot(&["EI61"])]);
        let (report, journal) = cycle(&dir, &sample_calendar("Alice"), &wall, true);

        assert_eq!(report.status, CycleStatus::Applied);
        assert_eq!(wall.writes(), [["EI61", "VY8721"]]);
        assert_eq!(owned_numbers(&journal), ["VY8721"]);
    }

    #[test]
    fn unchanged_cycle_makes_no_write() {
        let dir = TempDir::new().unwrap();
        let journal = store(&dir);
        journal
            .transaction(|tx| tx.record_owned("VY8721", "t", "k"))
            .unwrap();
        let wall = FakeWall::new(vec![wall_snapshot(&["EI61", "VY8721"])]);
        let cal = sample_calendar("Alice");

        for _ in 0..10 {
            let report = run_cycle(&cal, &wall, &journal, now(), true).unwrap();
            assert_eq!(report.status, CycleStatus::NoChange);
        }

        assert!(wall.writes().is_empty());
    }

    #[test]
    fn calendar_failure_stops_before_the_wall_is_read() {
        let dir = TempDir::new().unwrap();
        let wall = FakeWall::new(vec![wall_snapshot(&["EI61"])]);
        let (report, _) = cycle(&dir, &broken_calendar(), &wall, true);

        assert_eq!(report.status, CycleStatus::CalendarNotAuthoritative);
        assert!(report.plan.is_none());
        assert!(wall.writes().is_empty());
        assert!(report.summary().contains("calendar_reason="));
    }

    #[test]
    fn ambiguous_flighty_event_stops_before_the_wall_is_read() {
        let dir = TempDir::new().unwrap();
        let mut odd = flighty_event("e1", "Alice", "VY", "8721", "DUB-BCN");
        odd["summary"] = json!("Alice: \u{2708} something new Flighty invented");
        let wall = FakeWall::new(vec![wall_snapshot(&["EI61"])]);
        let (report, _) = cycle(&dir, &calendar(&[odd]), &wall, true);

        assert_eq!(report.status, CycleStatus::ParseNotAuthoritative);
        assert!(wall.writes().is_empty());
    }

    #[test]
    fn wall_failure_yields_provisional_intent_and_no_write() {
        let dir = TempDir::new().unwrap();
        let wall = FakeWall::new(vec![WallSnapshot::non_authoritative(
            now(),
            "flightwall_server_error:503",
        )]);
        let (report, _) = cycle(&dir, &sample_calendar("Alice"), &wall, true);

        assert_eq!(report.status, CycleStatus::WallNotAuthoritative);
        assert_eq!(report.wanted(), ["VY8721"]); // local intent is still visible
        assert!(report.plan.is_none()); // but nothing remote-dependent is claimed
        assert!(wall.writes().is_empty());
    }

    #[test]
    fn rejected_write_is_reported_and_owns_nothing() {
        let dir = TempDir::new().unwrap();
        let wall = FakeWall::with_outcome(vec![wall_snapshot(&["EI61"])], WriteOutcome::Rejected);
        let (report, journal) = cycle(&dir, &sample_calendar("Alice"), &wall, true);

        assert_eq!(report.status, CycleStatus::Rejected);
        assert!(owned_numbers(&journal).is_empty());
    }

    #[test]
    fn startup_recovery_settles_a_pending_write_before_planning() {
        let dir = TempDir::new().unwrap();
        let journal = store(&dir);
        journal
            .transaction(|tx| {
                tx.begin_pending_write(&["EI61".into(), "VY8721".into()], &["EI61".into()], "t")
            })
            .unwrap();
        // The wall shows the pending write applied; the second read is what the plan uses.
        let wall = FakeWall::new(vec![
            wall_snapshot(&["EI61", "VY8721"]),
            wall_snapshot(&["EI61", "VY8721"]),
        ]);

        let report = run_cycle(&sample_calendar("Alice"), &wall, &journal, now(), true).unwrap();

        assert_eq!(report.recovery, Some(RecoveryOutcome::Applied));
        assert_eq!(report.status, CycleStatus::NoChange);
        assert_eq!(owned_numbers(&journal), ["VY8721"]);
        assert_eq!(journal.pending_write().unwrap(), None);
        assert!(
            report
                .summary()
                .starts_with("status=no_change recovery=applied")
        );
    }

    #[test]
    fn summary_never_contains_the_friend_name_or_description() {
        let dir = TempDir::new().unwrap();
        let wall = FakeWall::new(vec![wall_snapshot(&[])]);
        let (report, _) = cycle(&dir, &sample_calendar("Alice Smith"), &wall, false);

        let text = report.summary();
        assert!(!text.contains("Alice"));
        assert!(!text.contains("IST"));
        assert!(text.contains("VY8721"));
    }

    #[test]
    fn same_flight_number_twice_in_the_window_is_wanted_once() {
        // Flighty exports every leg. AA577 DFW->DEN on two different dates is two calendar
        // events, two parser keys, but one wall entry: the wall has no date field.
        let dir = TempDir::new().unwrap();
        let first = flighty_event("e1", "Alice", "AA", "577", "DFW-DEN");
        let mut second = flighty_event("e2", "Bob", "AA", "577", "DFW-DEN");
        second["start"] =
            json!({"dateTime": "2026-11-05T10:00:00-06:00", "timeZone": "America/Chicago"});
        second["end"] =
            json!({"dateTime": "2026-11-05T12:00:00-07:00", "timeZone": "America/Denver"});
        let wall = FakeWall::new(vec![wall_snapshot(&[])]);

        let (report, journal) = cycle(&dir, &calendar(&[first, second]), &wall, true);

        assert_eq!(report.status, CycleStatus::Applied);
        assert_eq!(report.wanted(), ["AA577", "AA577"]); // both legs parsed
        assert_eq!(wall.writes(), [["AA577"]]); // one entry written
        assert_eq!(owned_numbers(&journal), ["AA577"]);
    }
}
