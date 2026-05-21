use crate::Timestamp;

/// Identity-bearing root of a domain consistency boundary.
///
/// Kept intentionally narrow: aggregates carry an `Id` and a last-touched
/// `Timestamp`. State transitions live on the concrete type, not the trait —
/// transitions are domain-specific and shouldn't be hoisted into a base
/// abstraction that callers would have to pattern-match through.
pub trait Aggregate {
    type Id: Copy + Eq + std::hash::Hash;

    fn id(&self) -> Self::Id;
    fn updated_at(&self) -> Timestamp;
}
