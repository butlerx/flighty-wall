//! Run cycles on an interval until told to stop. Never overlaps two cycles; never sleeps
//! past a stop.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::cycle::{CycleError, CycleReport};

/// A flag the signal handler sets and the loop polls.
#[derive(Debug, Clone, Default)]
pub struct StopSignal(Arc<AtomicBool>);

impl StopSignal {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    #[must_use]
    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Sleep up to `timeout`, waking early if the flag is set. Returns whether it was.
    ///
    /// Polls at 100ms: a signal delivered mid-sleep is honoured within that, which is far
    /// inside systemd's `TimeoutStopSec`.
    #[must_use]
    pub fn wait(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while !self.is_set() {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            std::thread::sleep((deadline - now).min(Duration::from_millis(100)));
        }
        true
    }

    fn flag(&self) -> &Arc<AtomicBool> {
        &self.0
    }
}

/// Run cycles until `stop` is set. Never overlaps two cycles; never sleeps past a stop.
///
/// `monotonic` and `wait` are injected so the loop's timing is testable: in production
/// they are [`Instant`] and [`StopSignal::wait`].
pub fn run_forever(
    mut cycle: impl FnMut() -> Result<CycleReport, CycleError>,
    interval: Duration,
    stop: &StopSignal,
    mut monotonic: impl FnMut() -> Duration,
    mut wait: impl FnMut(Duration) -> bool,
) -> u64 {
    let mut cycles = 0;
    while !stop.is_set() {
        let started = monotonic();
        match cycle() {
            Ok(report) => log::info!("cycle {}", report.summary()),
            Err(error) => log::error!("cycle failed: {error}"),
        }
        cycles += 1;
        let elapsed = monotonic().saturating_sub(started);
        if elapsed >= interval {
            log::warn!(
                "cycle took {:.1}s, longer than the {:.0}s interval; starting the next at once",
                elapsed.as_secs_f64(),
                interval.as_secs_f64()
            );
            continue;
        }
        // The branch above guarantees `elapsed < interval`; saturating keeps that panic-free.
        wait(interval.saturating_sub(elapsed));
    }
    cycles
}

/// Set `stop` on SIGTERM or SIGINT so the loop exits at the next safe point.
///
/// # Errors
///
/// [`std::io::Error`] if the handler cannot be registered.
pub fn install_stop_signals(stop: &StopSignal) -> std::io::Result<()> {
    for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        signal_hook::flag::register(signal, Arc::clone(stop.flag()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::super::cycle::CycleStatus;
    use super::super::test_support::now;
    use super::*;
    use crate::models::Snapshot;
    use crate::reconcile::PlanError;

    fn stub_report() -> CycleReport {
        CycleReport::new(
            CycleStatus::NoChange,
            now(),
            Snapshot::authoritative(now(), Vec::new()),
        )
    }

    fn secs(values: &[f64]) -> VecDeque<Duration> {
        values.iter().map(|s| Duration::from_secs_f64(*s)).collect()
    }

    #[test]
    fn loop_runs_until_stopped_and_waits_the_remaining_interval() {
        let stop = StopSignal::new();
        let mut clock = secs(&[0.0, 2.0, 120.0, 122.0, 240.0, 241.0]);
        let waits = RefCell::new(Vec::new());
        let reports = RefCell::new(0);

        let cycles = run_forever(
            || {
                *reports.borrow_mut() += 1;
                Ok(stub_report())
            },
            Duration::from_secs(120),
            &stop,
            || clock.pop_front().unwrap(),
            |timeout| {
                waits.borrow_mut().push(timeout);
                if waits.borrow().len() == 2 {
                    stop.set();
                }
                false
            },
        );

        assert_eq!(cycles, 2);
        assert_eq!(*reports.borrow(), 2);
        assert_eq!(
            *waits.borrow(),
            [Duration::from_secs(118), Duration::from_secs(118)]
        );
    }

    #[test]
    fn loop_does_not_overlap_a_slow_cycle_and_starts_the_next_at_once() {
        let stop = StopSignal::new();
        let mut clock = secs(&[0.0, 200.0, 200.0, 201.0]);
        let waits = RefCell::new(Vec::new());

        let cycles = run_forever(
            || Ok(stub_report()),
            Duration::from_secs(120),
            &stop,
            || clock.pop_front().unwrap(),
            |timeout| {
                waits.borrow_mut().push(timeout);
                stop.set();
                true
            },
        );

        // First cycle overran: no wait, straight into the second. Second waited, then stop.
        assert_eq!(cycles, 2);
        assert_eq!(*waits.borrow(), [Duration::from_secs(119)]);
    }

    #[test]
    fn loop_survives_a_cycle_that_errors() {
        let stop = StopSignal::new();
        let calls = RefCell::new(0);

        let cycles = run_forever(
            || {
                *calls.borrow_mut() += 1;
                if *calls.borrow() == 1 {
                    return Err(CycleError::Plan(PlanError::DuplicateWanted));
                }
                stop.set();
                Ok(stub_report())
            },
            Duration::from_millis(10),
            &stop,
            || Duration::ZERO,
            |timeout| stop.wait(timeout),
        );

        assert_eq!(cycles, 2);
    }

    #[test]
    fn stop_signal_wait_returns_early_when_set() {
        let stop = StopSignal::new();
        stop.set();
        let started = Instant::now();
        assert!(stop.wait(Duration::from_secs(5)));
        assert!(started.elapsed() < Duration::from_secs(1));

        let fresh = StopSignal::new();
        assert!(!fresh.wait(Duration::from_millis(20)));
    }
}
