//! Ownership-safe reconciliation between wanted flights and the wall's tracked list.
//!
//! Three facts from the capture shape every rule here (`docs/flightwall-api.md`): the
//! wall has no ownership signal, so the journal is the only record of what the daemon
//! added; writes are whole-document last-writer-wins, so a plan is a complete desired
//! list, not a diff; and the server enforces no cap, so the daemon holds the line at
//! five itself.
//!
//! [`plan`] is pure. [`apply_plan`] is the only writer and journals its intent first.
//! [`recover_pending_write`] runs at startup to settle an intent the last run never
//! resolved.

use crate::{
    flightwall::{
        MAX_TRACKED_FLIGHTS, TrackedFlight, WallSnapshot, WriteOutcome, WriteRefused, WriteResult,
    },
    state::{OwnedFlight, StateError, StateStore},
};
use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, BTreeSet};

/// What startup recovery concluded about a journaled write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecoveryOutcome {
    Applied,
    NotApplied,
    /// The wall could not be read authoritatively; the intent stays journaled.
    Deferred,
}

impl RecoveryOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::NotApplied => "not_applied",
            Self::Deferred => "deferred",
        }
    }
}

/// One flight the calendar wants on the wall, and the key that asked for it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Wanted {
    pub flight_number: String,
    pub source_key: String,
}

impl Wanted {
    #[must_use]
    pub fn new(flight_number: impl Into<String>, source_key: impl Into<String>) -> Self {
        Self {
            flight_number: flight_number.into(),
            source_key: source_key.into(),
        }
    }
}

/// The complete desired `tracked_flights` list, and how it differs from the wall.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// What the wall should hold after the write, in wall order with additions appended.
    pub desired: Vec<String>,
    /// Wanted numbers not on the wall that fit under the cap.
    pub additions: Vec<String>,
    /// Owned numbers on the wall that nothing wants any more.
    pub removals: Vec<String>,
    /// Numbers on the wall the daemon does not own. Always kept.
    pub manual: Vec<String>,
    /// Wanted numbers that are already manual entries. Not added, not adopted.
    pub suppressed: Vec<String>,
    /// Wanted numbers that did not fit. Reported, never forced.
    pub unplaceable: Vec<String>,
    /// Owned numbers already gone from the wall (the server drops landed flights).
    pub released: Vec<String>,
    /// `flight_number` to calendar key for every wanted flight, for the journal.
    pub wanted_keys: BTreeMap<String, String>,
}

impl Plan {
    /// Whether a write is needed at all.
    #[must_use]
    pub fn changed(&self) -> bool {
        !self.additions.is_empty() || !self.removals.is_empty()
    }
}

/// Why a plan could not be computed. Both are caller bugs, not wall conditions.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("refusing to plan against a non-authoritative wall snapshot")]
    NonAuthoritative,
    #[error("duplicate flight numbers in the wanted list")]
    DuplicateWanted,
}

/// Why a plan could not be applied.
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Refused(#[from] WriteRefused),
}

/// The one client method reconciliation needs.
pub trait Writer {
    /// Replace the wall's list with `flights` and return the outcome plus a re-read.
    ///
    /// # Errors
    ///
    /// [`WriteRefused`] when the client would not attempt the write.
    fn replace_tracked_flights(
        &self,
        snapshot: &WallSnapshot,
        flights: &[TrackedFlight],
    ) -> Result<WriteResult, WriteRefused>;
}

impl<T: crate::flightwall::Transport> Writer for crate::flightwall::FlightWallClient<T> {
    fn replace_tracked_flights(
        &self,
        snapshot: &WallSnapshot,
        flights: &[TrackedFlight],
    ) -> Result<WriteResult, WriteRefused> {
        Self::replace_tracked_flights(self, snapshot, flights)
    }
}

impl<W: Writer + ?Sized> Writer for &W {
    fn replace_tracked_flights(
        &self,
        snapshot: &WallSnapshot,
        flights: &[TrackedFlight],
    ) -> Result<WriteResult, WriteRefused> {
        (**self).replace_tracked_flights(snapshot, flights)
    }
}

/// Compute the desired list. Pure; errors rather than planning against bad input.
///
/// # Errors
///
/// [`PlanError`] for a non-authoritative snapshot or duplicate wanted numbers.
pub fn plan(
    wanted: &[Wanted],
    snapshot: &WallSnapshot,
    owned: &BTreeMap<String, OwnedFlight>,
) -> Result<Plan, PlanError> {
    plan_with_cap(wanted, snapshot, owned, MAX_TRACKED_FLIGHTS)
}

/// [`plan`] with an explicit cap, for tests.
///
/// # Errors
///
/// As [`plan`].
pub fn plan_with_cap(
    wanted: &[Wanted],
    snapshot: &WallSnapshot,
    owned: &BTreeMap<String, OwnedFlight>,
    cap: usize,
) -> Result<Plan, PlanError> {
    if !snapshot.is_authoritative() {
        return Err(PlanError::NonAuthoritative);
    }
    let wanted_numbers: Vec<&str> = wanted.iter().map(|w| w.flight_number.as_str()).collect();
    if wanted_numbers.iter().collect::<BTreeSet<_>>().len() != wanted_numbers.len() {
        return Err(PlanError::DuplicateWanted);
    }

    let wanted_keys: BTreeMap<String, String> = wanted
        .iter()
        .map(|w| (w.flight_number.clone(), w.source_key.clone()))
        .collect();
    let on_wall = snapshot.flight_numbers();
    let on_wall_set: BTreeSet<&str> = on_wall.iter().copied().collect();

    let mut desired: Vec<String> = Vec::new();
    let mut manual: Vec<String> = Vec::new();
    let mut removals: Vec<String> = Vec::new();
    for number in &on_wall {
        if !owned.contains_key(*number) {
            manual.push((*number).to_owned());
            desired.push((*number).to_owned());
        } else if wanted_keys.contains_key(*number) {
            desired.push((*number).to_owned());
        } else {
            removals.push((*number).to_owned());
        }
    }

    let suppressed: Vec<String> = wanted_numbers
        .iter()
        .filter(|n| on_wall_set.contains(**n) && !owned.contains_key(**n))
        .map(|n| (*n).to_owned())
        .collect();
    let released: Vec<String> = owned
        .keys()
        .filter(|n| !on_wall_set.contains(n.as_str()))
        .cloned()
        .collect();

    let mut additions: Vec<String> = Vec::new();
    let mut unplaceable: Vec<String> = Vec::new();
    for number in wanted_numbers {
        if on_wall_set.contains(number) {
            continue;
        }
        if desired.len() < cap {
            desired.push(number.to_owned());
            additions.push(number.to_owned());
        } else {
            unplaceable.push(number.to_owned());
        }
    }

    Ok(Plan {
        desired,
        additions,
        removals,
        manual,
        suppressed,
        unplaceable,
        released,
        wanted_keys,
    })
}

/// Write the plan if it changes anything; journal intent first, ownership after.
///
/// Returns `Ok(None)` when no write was needed. Ownership of entries that have already
/// vanished from the wall is released either way.
///
/// # Errors
///
/// [`ApplyError`] when the journal cannot be written or the client refuses the write.
pub fn apply_plan(
    client: &impl Writer,
    store: &StateStore,
    snapshot: &WallSnapshot,
    the_plan: &Plan,
    now: DateTime<Utc>,
) -> Result<Option<WriteResult>, ApplyError> {
    if !the_plan.changed() {
        if !the_plan.released.is_empty() {
            store.transaction(|tx| {
                the_plan
                    .released
                    .iter()
                    .try_for_each(|number| tx.release_owned(number))
            })?;
        }
        return Ok(None);
    }

    let started_at = rfc3339(now);
    let before: Vec<String> = snapshot
        .flight_numbers()
        .into_iter()
        .map(str::to_owned)
        .collect();
    store.transaction(|tx| tx.begin_pending_write(&the_plan.desired, &before, &started_at))?;

    let flights = flights_for(the_plan, snapshot, now);
    let result = client.replace_tracked_flights(snapshot, &flights)?;

    match result.outcome {
        WriteOutcome::Applied => settle(
            store,
            &the_plan.desired,
            &the_plan.wanted_keys,
            &before,
            true,
            &started_at,
        )?,
        WriteOutcome::Rejected => settle(
            store,
            &the_plan.desired,
            &the_plan.wanted_keys,
            &before,
            false,
            &started_at,
        )?,
        // UNKNOWN: a full body that reached the server applies. Only the re-read can say.
        WriteOutcome::Unknown => {
            recover_pending_write(store, &result.snapshot, Some(&the_plan.wanted_keys))?;
        }
    }
    Ok(Some(result))
}

/// Settle an intent the last run journaled but never resolved.
///
/// If the wall now holds exactly the desired list, the write applied and ownership is
/// recorded. If it does not, the write did not apply (or the owner has since intervened)
/// and only the intent is cleared; the next cycle re-plans from scratch. A
/// non-authoritative read defers the decision.
///
/// # Errors
///
/// [`StateError`] when the journal cannot be read or written.
pub fn recover_pending_write(
    store: &StateStore,
    snapshot: &WallSnapshot,
    wanted_keys: Option<&BTreeMap<String, String>>,
) -> Result<Option<RecoveryOutcome>, StateError> {
    let Some(pending) = store.pending_write()? else {
        return Ok(None);
    };
    if !snapshot.is_authoritative() {
        return Ok(Some(RecoveryOutcome::Deferred));
    }
    let applied = snapshot.flight_numbers() == pending.desired;
    let empty = BTreeMap::new();
    settle(
        store,
        &pending.desired,
        wanted_keys.unwrap_or(&empty),
        &pending.before,
        applied,
        &pending.started_at,
    )?;
    Ok(Some(if applied {
        RecoveryOutcome::Applied
    } else {
        RecoveryOutcome::NotApplied
    }))
}

/// Clear the intent; if the write applied, own what was added and release what was dropped.
fn settle(
    store: &StateStore,
    desired: &[String],
    wanted_keys: &BTreeMap<String, String>,
    before: &[String],
    applied: bool,
    now: &str,
) -> Result<(), StateError> {
    store.transaction(|tx| {
        tx.resolve_pending_write()?;
        if !applied {
            return Ok(());
        }
        let desired_set: BTreeSet<&str> = desired.iter().map(String::as_str).collect();
        let before_set: BTreeSet<&str> = before.iter().map(String::as_str).collect();
        for number in desired {
            if !before_set.contains(number.as_str()) {
                let source_key = wanted_keys.get(number).map_or("", String::as_str);
                tx.record_owned(number, now, source_key)?;
            }
        }
        for number in tx.owned_flight_numbers()? {
            if !desired_set.contains(number.as_str()) {
                tx.release_owned(&number)?;
            }
        }
        Ok(())
    })
}

/// Existing entries go back exactly as read; new ones are stamped the way the app does.
fn flights_for(the_plan: &Plan, snapshot: &WallSnapshot, now: DateTime<Utc>) -> Vec<TrackedFlight> {
    let existing: BTreeMap<&str, &TrackedFlight> = snapshot
        .tracked_flights
        .iter()
        .map(|flight| (flight.flight_number.as_str(), flight))
        .collect();
    the_plan
        .desired
        .iter()
        .map(|number| {
            existing
                .get(number.as_str())
                .map_or_else(|| TrackedFlight::new(number.clone(), now), |f| (*f).clone())
        })
        .collect()
}

fn rfc3339(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    //! Tests for the reconciliation planner and the journal it drives.

    use super::*;
    use crate::flightwall::Fingerprint;
    use chrono::TimeZone;
    use serde_json::json;
    use std::cell::RefCell;
    use tempfile::TempDir;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 22, 12, 0, 0).unwrap()
    }

    fn fingerprint() -> Fingerprint {
        Fingerprint {
            model: Some("mini-v1".into()),
            top_level_keys: ["display_config", "request_config", "version"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            tracked_flight_keys: BTreeSet::new(),
        }
    }

    pub(crate) fn wall(numbers: &[&str]) -> WallSnapshot {
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

    fn owned(numbers: &[&str]) -> BTreeMap<String, OwnedFlight> {
        numbers
            .iter()
            .map(|n| {
                (
                    (*n).to_owned(),
                    OwnedFlight {
                        flight_number: (*n).to_owned(),
                        first_added_at: "2026-09-01T00:00:00Z".to_owned(),
                        source_key: format!("{n}:XXX:2026-10-01"),
                    },
                )
            })
            .collect()
    }

    fn wanted(numbers: &[&str]) -> Vec<Wanted> {
        numbers
            .iter()
            .map(|n| Wanted::new(*n, format!("{n}:XXX:2026-10-01")))
            .collect()
    }

    fn strs(items: &[String]) -> Vec<&str> {
        items.iter().map(String::as_str).collect()
    }

    // --- plan(): pure ------------------------------------------------------------------

    #[test]
    fn unchanged_inputs_plan_no_write() {
        let result = plan(&wanted(&["BA5"]), &wall(&["BA5"]), &owned(&["BA5"])).unwrap();

        assert_eq!(result.desired, ["BA5"]);
        assert!(!result.changed());
        assert!(result.additions.is_empty());
        assert!(result.removals.is_empty());
    }

    #[test]
    fn first_add_on_an_empty_wall() {
        let result = plan(&wanted(&["VY8721"]), &wall(&[]), &owned(&[])).unwrap();

        assert_eq!(result.desired, ["VY8721"]);
        assert_eq!(result.additions, ["VY8721"]);
        assert!(result.changed());
    }

    #[test]
    fn manual_entries_are_kept_and_never_removed() {
        // EI61 is on the wall and not in the journal: the owner put it there.
        let result = plan(&wanted(&["BA5"]), &wall(&["EI61"]), &owned(&[])).unwrap();

        assert_eq!(result.desired, ["EI61", "BA5"]);
        assert!(result.removals.is_empty());
        assert_eq!(result.manual, ["EI61"]);
    }

    #[test]
    fn owned_entry_no_longer_wanted_is_removed() {
        let result = plan(&wanted(&[]), &wall(&["EI61", "BA5"]), &owned(&["BA5"])).unwrap();

        assert_eq!(result.desired, ["EI61"]);
        assert_eq!(result.removals, ["BA5"]);
    }

    #[test]
    fn a_friend_flying_a_manual_number_is_suppressed_not_adopted() {
        let result = plan(&wanted(&["EI61"]), &wall(&["EI61"]), &owned(&[])).unwrap();

        assert_eq!(result.desired, ["EI61"]);
        assert!(result.additions.is_empty());
        assert_eq!(result.suppressed, ["EI61"]);
        assert!(!result.changed());
    }

    #[test]
    fn owned_entry_that_vanished_is_released_without_a_write() {
        // The server removes landed flights on its own. Nothing to write; the journal is stale.
        let result = plan(&wanted(&[]), &wall(&[]), &owned(&["EI61"])).unwrap();

        assert!(!result.changed());
        assert_eq!(result.released, ["EI61"]);
    }

    #[test]
    fn owned_entry_that_vanished_but_is_still_wanted_is_re_added() {
        let result = plan(&wanted(&["BA5"]), &wall(&[]), &owned(&["BA5"])).unwrap();

        assert_eq!(result.additions, ["BA5"]);
    }

    #[test]
    fn capacity_is_respected_and_overflow_is_reported() {
        let result = plan(
            &wanted(&["A1", "A2", "A3"]),
            &wall(&["M1", "M2", "M3"]),
            &owned(&[]),
        )
        .unwrap();

        assert_eq!(result.desired.len(), 5);
        assert_eq!(strs(&result.desired[..3]), ["M1", "M2", "M3"]);
        assert_eq!(result.additions, ["A1", "A2"]);
        assert_eq!(result.unplaceable, ["A3"]);
    }

    #[test]
    fn a_stale_owned_entry_frees_a_slot_in_the_same_write() {
        // Five on the wall, one of them owned and no longer wanted, one new flight wanted.
        let result = plan(
            &wanted(&["NEW"]),
            &wall(&["M1", "M2", "M3", "M4", "OLD"]),
            &owned(&["OLD"]),
        )
        .unwrap();

        assert_eq!(result.removals, ["OLD"]);
        assert_eq!(result.additions, ["NEW"]);
        assert_eq!(result.desired, ["M1", "M2", "M3", "M4", "NEW"]);
    }

    #[test]
    fn manual_entries_are_never_evicted_even_when_the_wall_is_full_of_them() {
        let result = plan(
            &wanted(&["NEW"]),
            &wall(&["M1", "M2", "M3", "M4", "M5"]),
            &owned(&[]),
        )
        .unwrap();

        assert_eq!(result.desired, ["M1", "M2", "M3", "M4", "M5"]);
        assert_eq!(result.unplaceable, ["NEW"]);
        assert!(!result.changed());
    }

    #[test]
    fn wall_order_is_preserved_and_new_flights_append() {
        let result = plan(&wanted(&["B", "A"]), &wall(&["M"]), &owned(&[])).unwrap();

        assert_eq!(result.desired, ["M", "B", "A"]);
    }

    #[test]
    fn non_authoritative_wall_yields_no_plan() {
        let failed = WallSnapshot::non_authoritative(now(), "x");
        assert_eq!(
            plan(&wanted(&["BA5"]), &failed, &owned(&[])),
            Err(PlanError::NonAuthoritative)
        );
    }

    #[test]
    fn duplicate_wanted_numbers_are_rejected() {
        assert_eq!(
            plan(&wanted(&["BA5", "BA5"]), &wall(&[]), &owned(&[])),
            Err(PlanError::DuplicateWanted)
        );
    }

    // --- apply_plan(): the one writer ---------------------------------------------------

    /// A `FlightWallClient` stand-in that records the flights it was asked to write.
    struct ScriptedClient {
        result: WriteResult,
        writes: RefCell<Vec<Vec<TrackedFlight>>>,
    }

    impl ScriptedClient {
        fn new(outcome: WriteOutcome, after: WallSnapshot, reason: Option<&str>) -> Self {
            Self {
                result: WriteResult {
                    outcome,
                    snapshot: after,
                    reason: reason.map(str::to_owned),
                },
                writes: RefCell::new(Vec::new()),
            }
        }

        fn writes(&self) -> Vec<Vec<TrackedFlight>> {
            self.writes.borrow().clone()
        }
    }

    impl Writer for ScriptedClient {
        fn replace_tracked_flights(
            &self,
            _snapshot: &WallSnapshot,
            flights: &[TrackedFlight],
        ) -> Result<WriteResult, WriteRefused> {
            self.writes.borrow_mut().push(flights.to_vec());
            Ok(self.result.clone())
        }
    }

    fn store(dir: &TempDir) -> StateStore {
        StateStore::open(dir.path().join("state").join("state.sqlite3")).unwrap()
    }

    fn owned_numbers(journal: &StateStore) -> Vec<String> {
        journal.owned_flights().unwrap().into_keys().collect()
    }

    #[test]
    fn apply_with_no_change_does_not_write() {
        let dir = TempDir::new().unwrap();
        let journal = store(&dir);
        let client = ScriptedClient::new(WriteOutcome::Applied, wall(&["BA5"]), None);
        let unchanged = plan(&wanted(&["BA5"]), &wall(&["BA5"]), &owned(&["BA5"])).unwrap();

        let result = apply_plan(&client, &journal, &wall(&["BA5"]), &unchanged, now()).unwrap();

        assert!(result.is_none());
        assert!(client.writes().is_empty());
        assert_eq!(journal.pending_write().unwrap(), None);
    }

    #[test]
    fn apply_journals_intent_then_writes_then_records_ownership() {
        let dir = TempDir::new().unwrap();
        let journal = store(&dir);
        let before = wall(&["EI61"]);
        let client = ScriptedClient::new(WriteOutcome::Applied, wall(&["EI61", "BA5"]), None);
        let the_plan = plan(&wanted(&["BA5"]), &before, &owned(&[])).unwrap();

        let result = apply_plan(&client, &journal, &before, &the_plan, now())
            .unwrap()
            .expect("a write happened");

        assert_eq!(result.outcome, WriteOutcome::Applied);
        let written = &client.writes()[0];
        let numbers: Vec<&str> = written.iter().map(|f| f.flight_number.as_str()).collect();
        assert_eq!(numbers, ["EI61", "BA5"]);
        // The manual entry is sent back exactly as read; the new one is stamped now.
        assert_eq!(written[0], before.tracked_flights[0]);
        assert_eq!(written[1].created_at, "2026-09-22T12:00:00.000Z");
        assert_eq!(owned_numbers(&journal), ["BA5"]);
        assert_eq!(journal.pending_write().unwrap(), None);
    }

    #[test]
    fn apply_releases_ownership_of_removed_and_vanished_entries() {
        let dir = TempDir::new().unwrap();
        let journal = store(&dir);
        journal
            .transaction(|tx| {
                tx.record_owned("OLD", "t", "k")?;
                tx.record_owned("GONE", "t", "k")
            })
            .unwrap();
        let before = wall(&["EI61", "OLD"]);
        let client = ScriptedClient::new(WriteOutcome::Applied, wall(&["EI61"]), None);
        let the_plan = plan(&wanted(&[]), &before, &journal.owned_flights().unwrap()).unwrap();

        apply_plan(&client, &journal, &before, &the_plan, now()).unwrap();

        assert!(owned_numbers(&journal).is_empty());
    }

    #[test]
    fn apply_rejected_write_leaves_journal_unchanged() {
        let dir = TempDir::new().unwrap();
        let journal = store(&dir);
        let before = wall(&["EI61"]);
        let client = ScriptedClient::new(
            WriteOutcome::Rejected,
            before.clone(),
            Some("flightwall_credentials_rejected:1102"),
        );
        let the_plan = plan(&wanted(&["BA5"]), &before, &owned(&[])).unwrap();

        let result = apply_plan(&client, &journal, &before, &the_plan, now())
            .unwrap()
            .expect("a write was attempted");

        assert_eq!(result.outcome, WriteOutcome::Rejected);
        assert!(owned_numbers(&journal).is_empty());
        assert_eq!(journal.pending_write().unwrap(), None);
    }

    #[test]
    fn apply_unknown_outcome_that_applied_is_journaled_from_the_re_read() {
        let dir = TempDir::new().unwrap();
        let journal = store(&dir);
        let before = wall(&["EI61"]);
        let client = ScriptedClient::new(
            WriteOutcome::Unknown,
            wall(&["EI61", "BA5"]),
            Some("flightwall_request_failed:X"),
        );
        let the_plan = plan(&wanted(&["BA5"]), &before, &owned(&[])).unwrap();

        apply_plan(&client, &journal, &before, &the_plan, now()).unwrap();

        assert_eq!(owned_numbers(&journal), ["BA5"]);
        assert_eq!(journal.pending_write().unwrap(), None);
    }

    #[test]
    fn apply_unknown_outcome_that_did_not_apply_leaves_the_journal_alone() {
        let dir = TempDir::new().unwrap();
        let journal = store(&dir);
        let before = wall(&["EI61"]);
        let client = ScriptedClient::new(
            WriteOutcome::Unknown,
            wall(&["EI61"]),
            Some("flightwall_request_failed:X"),
        );
        let the_plan = plan(&wanted(&["BA5"]), &before, &owned(&[])).unwrap();

        apply_plan(&client, &journal, &before, &the_plan, now()).unwrap();

        assert!(owned_numbers(&journal).is_empty());
        assert_eq!(journal.pending_write().unwrap(), None);
    }

    #[test]
    fn apply_unknown_outcome_with_a_non_authoritative_re_read_keeps_the_intent() {
        let dir = TempDir::new().unwrap();
        let journal = store(&dir);
        let before = wall(&["EI61"]);
        let unreadable = WallSnapshot::non_authoritative(now(), "flightwall_server_error:503");
        let client = ScriptedClient::new(
            WriteOutcome::Unknown,
            unreadable,
            Some("flightwall_request_failed:X"),
        );
        let the_plan = plan(&wanted(&["BA5"]), &before, &owned(&[])).unwrap();

        apply_plan(&client, &journal, &before, &the_plan, now()).unwrap();

        let pending = journal.pending_write().unwrap().expect("intent kept");
        assert_eq!(pending.desired, ["EI61", "BA5"]);
    }

    // --- recover_pending_write(): startup -----------------------------------------------

    #[test]
    fn recovery_with_no_pending_write_is_a_no_op() {
        let dir = TempDir::new().unwrap();
        let journal = store(&dir);

        assert_eq!(
            recover_pending_write(&journal, &wall(&["EI61"]), None).unwrap(),
            None
        );
    }

    #[test]
    fn recovery_when_the_wall_matches_the_intent_records_ownership() {
        let dir = TempDir::new().unwrap();
        let journal = store(&dir);
        journal
            .transaction(|tx| {
                tx.begin_pending_write(&["EI61".into(), "BA5".into()], &["EI61".into()], "t")
            })
            .unwrap();

        let outcome = recover_pending_write(&journal, &wall(&["EI61", "BA5"]), None).unwrap();

        assert_eq!(outcome, Some(RecoveryOutcome::Applied));
        assert_eq!(owned_numbers(&journal), ["BA5"]);
        assert_eq!(journal.pending_write().unwrap(), None);
    }

    #[test]
    fn recovery_when_the_wall_does_not_match_clears_the_intent_only() {
        let dir = TempDir::new().unwrap();
        let journal = store(&dir);
        journal
            .transaction(|tx| {
                tx.begin_pending_write(&["EI61".into(), "BA5".into()], &["EI61".into()], "t")
            })
            .unwrap();

        let outcome = recover_pending_write(&journal, &wall(&["EI61"]), None).unwrap();

        assert_eq!(outcome, Some(RecoveryOutcome::NotApplied));
        assert!(owned_numbers(&journal).is_empty());
        assert_eq!(journal.pending_write().unwrap(), None);
    }

    #[test]
    fn recovery_against_a_non_authoritative_wall_keeps_the_intent() {
        let dir = TempDir::new().unwrap();
        let journal = store(&dir);
        journal
            .transaction(|tx| tx.begin_pending_write(&["EI61".into(), "BA5".into()], &[], "t"))
            .unwrap();

        let outcome =
            recover_pending_write(&journal, &WallSnapshot::non_authoritative(now(), "x"), None)
                .unwrap();

        assert_eq!(outcome, Some(RecoveryOutcome::Deferred));
        assert!(journal.pending_write().unwrap().is_some());
    }
}
