use crate::Timestamp;

/// Identity-bearing root of a domain consistency boundary.
///
/// Kept intentionally narrow: aggregates carry an `Id`, a `created_at`
/// timestamp (immutable), and an `updated_at` timestamp that the type
/// touches on every successful transition. State transitions live on the
/// concrete type, not the trait — transitions are domain-specific and
/// shouldn't be hoisted into a base abstraction that callers would have
/// to pattern-match through.
pub trait Aggregate {
    type Id: Copy + Eq + std::hash::Hash;

    fn id(&self) -> Self::Id;
    fn created_at(&self) -> Timestamp;
    fn updated_at(&self) -> Timestamp;
}
