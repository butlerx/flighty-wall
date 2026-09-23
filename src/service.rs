//! The synchronization engine: one cycle, the loop that repeats it, and the lock that
//! guards it.
//!
//! One engine sits behind every entry point so the dry run, the one-shot apply, and the
//! daemon cannot drift. A cycle is read → parse → read wall → plan → (apply). Any
//! non-authoritative input ends the cycle before the plan is computed; nothing is written
//! from a partial view.

pub mod cycle;
pub mod lock;
pub mod run_loop;
#[cfg(test)]
mod test_support;

pub use cycle::{CycleError, CycleReport, CycleStatus, Wall, run_cycle};
pub use lock::{HostLock, LockError, lock_path_for};
pub use run_loop::{StopSignal, install_stop_signals, run_forever};
