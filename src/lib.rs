//! lh-contributor-client — LandHunter contributor SDK.
//!
//! Talks to the lh-tracker Cloudflare Worker coordinator:
//! register → lease → verify operator signature → execute chunk → stage
//! blobs → submit result (or report failure). Contracts are ed25519-signed
//! by the operator; clients verify before executing anything.

pub mod api;
pub mod chunk;
pub mod config;
pub mod crypto;
pub mod error;
pub mod rate;
pub mod staging;

/// Client version reported to the coordinator on register.
pub fn client_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
