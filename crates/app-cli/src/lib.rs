//! repo-link CLI — also installed as `rl`.

mod cli;
mod commands;
mod daemon;
mod dispatch;
mod docs;
mod render;
mod services;

pub use commands::repo::DiscoveredRepo;
pub use dispatch::run;
