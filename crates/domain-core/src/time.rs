use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Wall-clock instant. UTC-only to dodge timezone foot-guns at the boundary;
/// adapters convert on the way in/out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(DateTime<Utc>);

impl Timestamp {
    pub fn now() -> Self {
        Self(Utc::now())
    }

    /// The Unix epoch — a "since the beginning of time" sentinel. Used by the
    /// project poller as the initial per-project watermark so the first poll
    /// covers everything, then narrows as items are seen.
    pub fn epoch() -> Self {
        Self(DateTime::<Utc>::UNIX_EPOCH)
    }

    pub fn from_utc(dt: DateTime<Utc>) -> Self {
        Self(dt)
    }

    pub fn into_inner(self) -> DateTime<Utc> {
        self.0
    }

    /// This instant shifted back by one second. The project poller uses it to
    /// pull its watermark to `max_seen - 1s` before issuing the next strict
    /// `updated:>` delta query, so same-second siblings of the newest item it
    /// saw are re-included rather than silently skipped (over-fetching one
    /// second is idempotent and strictly safer than under-fetching).
    pub fn minus_one_second(self) -> Self {
        Self(self.0 - chrono::Duration::seconds(1))
    }

    pub fn as_inner(&self) -> &DateTime<Utc> {
        &self.0
    }
}

impl From<DateTime<Utc>> for Timestamp {
    fn from(dt: DateTime<Utc>) -> Self {
        Self(dt)
    }
}

impl From<Timestamp> for DateTime<Utc> {
    fn from(ts: Timestamp) -> Self {
        ts.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_ordering_matches_chrono() {
        let a = Timestamp::now();
        let b = Timestamp::from_utc(a.into_inner() + chrono::Duration::seconds(1));
        assert!(b > a);
    }
}
