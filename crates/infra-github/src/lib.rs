//! infra-github — GitHub adapter implementing [`ports::RemoteTaskProvider`].
//!
//! Issues are the underlying remote task object. Promotion creates an issue;
//! push updates it; pull fetches its current state. The REST surface is
//! driven by `octocrab`.
//!
//! # Internals
//!
//! Protocol-specific code is split into submodules so REST and GraphQL stay
//! distinct:
//!
//! - [`rest`] — issue CRUD via `octocrab`'s REST handlers (today's full
//!   implementation). Owns the issue-model mapping, URL parsing, and the
//!   `state_reason` enum mapping.
//! - `graphql` *(future)* — sibling module for capabilities only exposed
//!   via GraphQL (Projects v2 status fields, sub-issue parents, custom
//!   fields). The wrapper struct below will compose both clients.

mod provider;
mod rest;

#[cfg(test)]
mod tests;

pub use provider::GithubTaskProvider;
