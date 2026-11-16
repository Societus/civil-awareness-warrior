//! Minimal GUI-style embedder: drives the warrior loop through the library
//! API with an external stop channel and a pollable progress handle.
//!
//! Compile: `cargo build --examples`
//! Run: same `LH_*` env as the CLI, plus a coordinator worth talking to.
//! The poller prints a one-line dashboard every 5 s; the "stop button"
//! fires after 30 s.

use anyhow::Result;
use lh_contributor_client::api::ApiClient;
use lh_contributor_client::config::Config;
use lh_contributor_client::{RunOptions, Runner};
use std::time::Duration;
use tokio::sync::watch;

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Config::from_env().map_err(|e| anyhow::anyhow!("config: {e}"))?;

    // Library surface also covers the pieces the runner wires together:
    // registration, contract verification, executors, staging.
    let api = ApiClient::new(
        &cfg.coordinator_url,
        Duration::from_secs(cfg.http_timeout_secs),
    )?;
    let health = api.health().await;
    println!(
        "embedding lh-contributor-client v{} (coordinator health check: {})",
        lh_contributor_client::client_version(),
        if health.is_ok() {
            "ok"
        } else {
            "unreachable (runner will retry)"
        },
    );

    // GUI stop button → external shutdown channel.
    let (stop_tx, stop_rx) = watch::channel(false);

    let runner = Runner::new(
        cfg,
        RunOptions {
            shutdown_rx: Some(stop_rx),
            ..RunOptions::default()
        },
    )?;

    // Pollable progress: clone the handle, poll on the UI tick.
    let stats = runner.stats();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let s = stats.snapshot();
            println!(
                "up {:>5.0}s | completed {} failed {} verified {} | {:>7} B staged | slots: {:?}",
                s.uptime.as_secs_f32(),
                s.chunks_completed,
                s.chunks_failed,
                s.chunks_verified,
                s.bytes_staged,
                s.slots,
            );
        }
    });

    // Press the stop button after 30 s.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        let _ = stop_tx.send(true);
    });

    runner.run().await
}
