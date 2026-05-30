//! infra-github — GitHub adapter implementing [`ports::RemoteTaskProvider`]
//! and [`ports::RemoteProjectProvider`].
//!
//! Issues are the underlying remote task object. Promotion creates an issue in
//! the task's filing repo (today always its logical repo, until RFC 0002 lets
//! the two diverge); push updates it; pull fetches its current state.
//! Projects v2 boards layer on top: a project's Status field, draft issues,
//! and membership are
//! GraphQL-only (GitHub sunset the REST projects API).
//!
//! # Internals
//!
//! Protocol-specific code is split into submodules so REST and GraphQL stay
//! distinct:
//!
//! - [`rest`] — issue CRUD via `octocrab`'s REST handlers. Owns the
//!   issue-model mapping, URL parsing, and the `state_reason` enum mapping.
//! - [`graphql`] — the Projects v2 surface via `octocrab.graphql()`: status
//!   schema fetch, draft create/update/convert, item attach, status writes,
//!   and the per-project delta poll.
//!
//! [`GithubAdapter`] composes both clients and routes each port method to
//! whichever protocol supports it.

mod graphql;
mod provider;
mod rest;

#[cfg(test)]
mod graphql_tests;
#[cfg(test)]
mod tests;

pub use provider::GithubAdapter;
