//! liru-bot library crate (Rust port of the Python lichess-bot).
//!
//! Module layout mirrors the Python `lib/` directory:
//!
//! - [`timer`]            — duration helpers and a `Timer` struct
//! - [`blocklist`]        — local & online blocklists
//! - [`config`]            — YAML loading, defaults, validation
//! - [`lichess_types`]    — typed wrappers over the Lichess JSON API
//! - [`model`]             — `Game`, `Challenge`, `Player` domain objects
//! - [`lichess`]           — HTTP/streaming client for lichess.org
//! - [`conversation`]     — chat / `!command` handler
//! - [`engine_wrapper`]   — UCI/XBoard/homemade engine integration
//! - [`matchmaking`]      — outgoing bot challenges
//! - [`lichess_bot`]      — main event loop, signal handling, logging setup

pub mod blocklist;
pub mod config;
pub mod conversation;
pub mod egtb;
// In-process clrsrc engine backend (B1-B6). Only compiled with `--features
// embedded`; the default subprocess build never references clrsrc.
#[cfg(feature = "embedded")]
pub mod embedded_engine;
pub mod engine_wrapper;
pub mod exp_overlay;
pub mod homemade;
pub mod lichess;
pub mod lichess_bot;
pub mod lichess_types;
pub mod matchmaking;
pub mod model;
pub mod online_book;
pub mod opponent_db;
pub mod polyglot;
pub mod timer;

pub use lichess_bot::start_program;
pub use lichess_types::UserProfileType;

/// Crate version embedded into log lines and HTTP user-agent.
///
/// In Python this lives in `lib/versioning.yml`. For Rust we read the version
/// from Cargo at compile time so the two stay in sync via a single source.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
