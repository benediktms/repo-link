use chrono::{TimeZone, Utc};
use domain_core::Timestamp;
use ports::Clock;

// ---------- Clock --------------------------------------------------------

pub struct FixedClock {
    instant: Timestamp,
}

impl FixedClock {
    pub fn new_epoch() -> Self {
        Self {
            instant: Timestamp::from_utc(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()),
        }
    }

    pub fn at(instant: Timestamp) -> Self {
        Self { instant }
    }
}

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        self.instant
    }
}
