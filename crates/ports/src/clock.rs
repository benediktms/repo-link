//! Clock port and system implementation.

use domain_core::Timestamp;

// ---------- Clock --------------------------------------------------------

/// Injected to keep reconciliation deterministic in tests.
pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}
