//! Reusable Ronin client runtime shared by terminal and desktop frontends.
//!
//! UI-independent API transport, authentication, configuration, context,
//! permissions, session persistence, and agent orchestration live here. The
//! `ronin` binary is only argument parsing and terminal presentation.

pub mod auth;
pub mod client;
pub mod config;
pub mod context;
pub mod permissions;
pub mod run;
pub mod storage;
#[cfg(feature = "cli-ui")]
pub mod terminal;
#[cfg(feature = "cli-ui")]
pub mod tui;
pub mod update;

#[cfg(test)]
mod compat_tests;
