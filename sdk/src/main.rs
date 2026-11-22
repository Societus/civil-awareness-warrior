//! CLI entry: a thin shell over the library's [`Runner`].
//!
//! Subcommands:
//! - `register` — mint an identity and save it to the work dir
//! - `run` — drive the contributor loop (see `lh_contributor_client::runner`
//!   for the loop itself; flags map 1:1 onto `RunOptions` fields)

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use lh_contributor_client::api::{ApiClient, ChunkType, ALL_CHUNK_TYPES};
use lh_contributor_client::config::{save_credentials, Config};
use lh_contributor_client::runner::{register_request_with_llm, RunOptions, Runner};
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "lh-contributor",
    version,
    about = "LandHunter contributor client: lease chunks from lh-tracker, execute, stage blobs, submit results."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Register a new client identity and save it to the work dir.
    Register,
    /// Run the contributor loop (poll → execute → upload → submit).
    Run {
        /// Exit after this slot's first chunk settles (result or fail).
        #[arg(long)]
        once: bool,
        /// Override LH_CONCURRENCY (1..=8).
        #[arg(long)]
        concurrency: Option<u32>,
        /// Comma-separated chunk types to lease (default: all six).
        #[arg(long)]
        capabilities: Option<String>,
        /// Stage blobs locally and print manifests; never submit results.
        #[arg(long)]
        dry_run: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Register => cmd_register().await,
        Cmd::Run {
            once,
            concurrency,
            capabilities,
            dry_run,
        } => cmd_run(once, concurrency, capabilities, dry_run).await,
    }
}

async fn cmd_register() -> Result<()> {
    let cfg = Config::from_env().context("config")?;
    let api = ApiClient::new(
        &cfg.coordinator_url,
        Duration::from_secs(cfg.http_timeout_secs),
    )?;
    // Declare the local-LLM capability when the lane is configured, so the
    // tracker may lease classify chunks to this identity.
    let creds = api
        .register(&register_request_with_llm(
            cfg.llm.as_ref().map(|c| &c.capability),
        ))
        .await
        .context("register failed")?;
    let path = save_credentials(&cfg.work_dir, &cfg.coordinator_url, &creds)?;
    println!("registered client_id = {}", creds.client_id);
    println!("auth_token saved to {} (0600, not echoed)", path.display());
    Ok(())
}

async fn cmd_run(
    once: bool,
    concurrency: Option<u32>,
    capabilities: Option<String>,
    dry_run: bool,
) -> Result<()> {
    let cfg = Config::from_env().context("config")?;
    let caps = match &capabilities {
        Some(csv) => {
            let mut out = Vec::new();
            for part in csv.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                let t = ChunkType::parse(part)
                    .ok_or_else(|| anyhow::anyhow!("unknown chunk type {part:?}"))?;
                if !out.contains(&t) {
                    out.push(t);
                }
            }
            if out.is_empty() {
                bail!("--capabilities parsed to an empty set");
            }
            out
        }
        None => ALL_CHUNK_TYPES.to_vec(),
    };
    Runner::new(
        cfg,
        RunOptions {
            once,
            concurrency,
            capabilities: caps,
            dry_run,
            shutdown_rx: None,
        },
    )?
    .run()
    .await
}
