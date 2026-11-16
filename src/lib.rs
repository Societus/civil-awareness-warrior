//! lh-contributor-client — LandHunter contributor SDK.
//!
//! Talks to the lh-tracker Cloudflare Worker coordinator:
//! register → lease → verify operator signature → execute chunk → stage
//! blobs → submit result (or report failure). Contracts are ed25519-signed
//! by the operator; clients verify before executing anything.
//!
//! Embedding: the CLI (`src/main.rs`) is a thin shell; everything it does
//! is available as library API — [`runner::Runner`] drives the loop,
//! [`stats::StatsHandle`] is a pollable progress handle for GUIs,
//! [`api::ApiClient`] exposes registration/lease/heartbeat/result, and the
//! `chunk`, `crypto`, `config` and `staging` modules hold the executors,
//! ed25519 verification, config/credential loading and blob transports.
//! See `examples/embed.rs`.

pub mod api;
pub mod chunk;
pub mod config;
pub mod crypto;
pub mod error;
pub mod rate;
pub mod runner;
pub mod staging;
pub mod stats;

pub use runner::{register_request, RunOptions, Runner};
pub use stats::{SlotActivity, StatsHandle, StatsSnapshot};

/// Client version reported to the coordinator on register.
pub fn client_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
