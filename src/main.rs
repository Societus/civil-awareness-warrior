//! CLI entry: register → poll → execute → upload → submit.
//!
//! The core loop (per worker slot, `slot_id` 0..=7 so the server's
//! per-client-per-slot claim dedupe holds):
//! 1. `POST /v1/lease` with capabilities → backoff? sleep and retry.
//! 2. Verify the contract's ed25519 signature BEFORE executing anything.
//! 3. Execute the chunk-type handler (declarative fetch/probe/walk/search).
//! 4. Heartbeat every `heartbeat_interval_sec` while working.
//! 5. Upload blobs to the lease's staging prefix (R2 / presign / local).
//! 6. `POST /v1/result` with manifest + blob keys — or `POST /v1/fail`.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use lh_contributor_client::api::{
    ApiClient, ChunkType, Credentials, FailRequest, FailureType, LeasePoll, RegisterRequest,
    ResultSubmit, ALL_CHUNK_TYPES,
};
use lh_contributor_client::chunk::{execute, ExecContext, LeaseContext};
use lh_contributor_client::config::{save_credentials, Config, StagingConfig};
use lh_contributor_client::crypto::verify_lease_contract;
use lh_contributor_client::rate::RateLimiter;
use lh_contributor_client::staging::build_stager;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;

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
        /// Comma-separated chunk types to lease (default: all five).
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
    let api = ApiClient::new(&cfg.coordinator_url, Duration::from_secs(cfg.http_timeout_secs))?;
    let creds = api
        .register(&register_request())
        .await
        .context("register failed")?;
    let path = save_credentials(&cfg.work_dir, &creds)?;
    println!("registered client_id = {}", creds.client_id);
    println!(
        "auth_token saved to {} (0600, not echoed)",
        path.display()
    );
    Ok(())
}

fn register_request() -> RegisterRequest {
    RegisterRequest {
        platform: format!("{}-{}-rust", std::env::consts::OS, std::env::consts::ARCH),
        version: lh_contributor_client::client_version().to_string(),
        capabilities: ALL_CHUNK_TYPES.to_vec(),
    }
}

// ---------------------------------------------------------------------------
// Run loop
// ---------------------------------------------------------------------------

/// Atomic run state shared by all slots.
struct RunState {
    api: ApiClient,
    cfg: Config,
    /// RwLock: the emergency re-register path replaces the identity once.
    creds: RwLock<Credentials>,
    /// Pre-built reqwest client for SOURCE fetches (county GIS etc.), so the
    /// coordinator connection pool stays separate.
    source_http: reqwest::Client,
    capabilities: Vec<ChunkType>,
    dry_run: bool,
    shutdown_tx: watch::Sender<bool>,
    /// Consecutive contract-verification failures; abort the run beyond 3
    /// (a persistently bad signature is tampering or a wrong public key —
    /// neither heals by hammering the coordinator).
    consecutive_verify_failures: AtomicU32,
    /// Set after one emergency re-register (401 = dev D1 wipe / rotated
    /// token); a second 401 is fatal.
    re_register_used: AtomicBool,
}

async fn cmd_run(
    once: bool,
    concurrency: Option<u32>,
    capabilities: Option<String>,
    dry_run: bool,
) -> Result<()> {
    let mut cfg = Config::from_env().context("config")?;
    if let Some(c) = concurrency {
        cfg.concurrency = c.clamp(1, 8);
    }
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

    // Real runs need a real staging target; local staging is dry-run-only
    // (submitted blob keys must exist in R2 for verification to fetch them).
    if !dry_run {
        match cfg.staging {
            StagingConfig::PresignTemplate { .. } | StagingConfig::R2 { .. } => {}
            StagingConfig::Local { .. } => bail!(
                "real runs stage blobs to R2; configure LH_R2_* or \
                 LH_UPLOAD_PRESIGN_TEMPLATE (or use --dry-run for local staging)"
            ),
        }
    }

    let api = ApiClient::new(&cfg.coordinator_url, Duration::from_secs(cfg.http_timeout_secs))?;

    // Core loop step 1: register (or load saved client_id + auth_token).
    let creds = match cfg.credentials.clone() {
        Some(c) => c,
        None => {
            tracing::info!("no saved credentials; registering a new client identity");
            let c = api.register(&register_request()).await.context("register failed")?;
            let path = save_credentials(&cfg.work_dir, &c)?;
            tracing::info!(client_id = %c.client_id, path = %path.display(), "registered");
            c
        }
    };

    // Pre-flight: coordinator reachable?
    match api.health().await {
        Ok(h) => tracing::info!(time = h.time, "coordinator healthy"),
        Err(e) => bail!("coordinator health check failed: {e}"),
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let source_http = reqwest::Client::builder()
        .timeout(Duration::from_secs(cfg.http_timeout_secs))
        .build()
        .context("source http client")?;
    let state = Arc::new(RunState {
        api,
        cfg: cfg.clone(),
        creds: RwLock::new(creds),
        source_http,
        capabilities: caps,
        dry_run,
        shutdown_tx,
        consecutive_verify_failures: AtomicU32::new(0),
        re_register_used: AtomicBool::new(false),
    });

    // Ctrl-C: stop polling; in-flight leases report aborted_by_user so the
    // chunks requeue for someone else.
    {
        let shutdown_tx = state.shutdown_tx.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                tracing::warn!("shutdown signal received; finishing in-flight work");
                let _ = shutdown_tx.send(true);
            }
        });
    }

    let concurrency = cfg.concurrency as usize;
    let mut handles = Vec::with_capacity(concurrency);
    for slot in 0..concurrency {
        let state = Arc::clone(&state);
        let mut shutdown_rx = shutdown_rx.clone();
        handles.push(tokio::spawn(async move {
            slot_loop(slot as u8, state, &mut shutdown_rx, once).await
        }));
    }

    let mut fatal: Option<anyhow::Error> = None;
    for handle in handles {
        match handle.await.context("slot task panicked")? {
            Ok(()) => {}
            Err(e) => fatal = fatal.or(Some(e)),
        }
    }
    if let Some(e) = fatal {
        return Err(e);
    }
    tracing::info!("run complete");
    Ok(())
}

/// Sleep that wakes early on shutdown. Returns true if shut down.
async fn sleep_or_shutdown(d: Duration, shutdown_rx: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => false,
        _ = shutdown_rx.changed() => *shutdown_rx.borrow(),
    }
}

/// Politeness jitter: ±20% around the server-suggested backoff, capped at
/// 5 minutes, so N clients don't thundering-herd on lease expiry.
fn backoff_sleep(sec: u64) -> Duration {
    let sec = sec.min(300);
    let spread = (sec / 5).max(1);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let delta = nanos % (2 * spread + 1);
    Duration::from_secs(sec.saturating_sub(spread).saturating_add(delta))
}

async fn slot_loop(
    slot: u8,
    state: Arc<RunState>,
    shutdown_rx: &mut watch::Receiver<bool>,
    once: bool,
) -> Result<()> {
    loop {
        if *shutdown_rx.borrow() {
            return Ok(());
        }
        let settled = lease_round(slot, &state, shutdown_rx).await?;
        if (settled || *shutdown_rx.borrow()) && once {
            return Ok(());
        }
    }
}

/// One lease attempt: poll → verify → execute → submit/fail.
/// Returns true when a chunk settled (result submitted, fail reported, or
/// dry-run completed).
async fn lease_round(
    slot: u8,
    state: &Arc<RunState>,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<bool> {
    // 1. Poll.
    let creds_snapshot = state.creds.read().expect("creds lock").clone();
    let lease = match state.api.lease(&creds_snapshot, slot, &state.capabilities).await {
        Ok(LeasePoll::Backoff { backoff_sec }) => {
            tracing::debug!(slot, backoff_sec, "no work; backing off");
            sleep_or_shutdown(backoff_sleep(backoff_sec), shutdown_rx).await;
            return Ok(false);
        }
        Ok(LeasePoll::Lease(lease)) => lease,
        Err(lh_contributor_client::error::ApiError::RateLimited { retry_after_sec }) => {
            let wait = retry_after_sec.unwrap_or(30).min(300);
            tracing::warn!(slot, wait, "coordinator rate limited; waiting");
            sleep_or_shutdown(Duration::from_secs(wait), shutdown_rx).await;
            return Ok(false);
        }
        Err(lh_contributor_client::error::ApiError::Unauthorized)
            if !state.re_register_used.load(Ordering::Relaxed) =>
        {
            // Identity vanished server-side (dev D1 wipe, token rotation):
            // re-register once, persist, keep going.
            state.re_register_used.store(true, Ordering::Relaxed);
            tracing::warn!("lease auth rejected; re-registering once");
            let creds = state
                .api
                .register(&register_request())
                .await
                .context("emergency re-register failed")?;
            let path = save_credentials(&state.cfg.work_dir, &creds)?;
            tracing::info!(client_id = %creds.client_id, path = %path.display(), "re-registered");
            *state.creds.write().expect("creds lock") = creds;
            return Ok(false);
        }
        Err(e) => {
            tracing::warn!(slot, error = %e, "lease poll failed; waiting 30s");
            sleep_or_shutdown(Duration::from_secs(30), shutdown_rx).await;
            return Ok(false);
        }
    };

    let lease = *lease;
    tracing::info!(
        slot,
        lease_id = %lease.lease_id,
        chunk_id = %lease.chunk_id,
        chunk_type = %lease.contract.core.chunk_type,
        target = %lease.contract.core.target_key,
        "leased chunk"
    );

    // 2. Verify the operator signature BEFORE executing anything.
    let now_ms = lh_contributor_client::api::unix_ms();
    if let Err(reason) = verify_lease_contract(&state.cfg.public_key_hex, &lease, now_ms) {
        let n = state.consecutive_verify_failures.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::error!(lease_id = %lease.lease_id, %reason, "contract verification FAILED");
        report(
            state,
            &lease.lease_id,
            FailureType::ClientError,
            &format!("contract verification failed: {reason}"),
        )
        .await;
        if n >= 3 {
            bail!("contract verification failed {n} times consecutively; check LH_PUBLIC_KEY_HEX or suspect tampering — aborting");
        }
        return Ok(true);
    }
    state.consecutive_verify_failures.store(0, Ordering::Relaxed);

    // 3. Execute with heartbeat.
    let started_at = now_ms;
    let (lost_tx, lost_rx) = watch::channel(false);
    let hb_api = state.api.clone();
    let hb_client_id = creds_snapshot.client_id.clone();
    let hb_lease_id = lease.lease_id.clone();
    let hb_interval = Duration::from_secs(lease.heartbeat_interval_sec.max(15));
    let mut hb_shutdown = state.shutdown_tx.subscribe();
    let heartbeat_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(hb_interval) => {}
                _ = hb_shutdown.changed() => {
                    if *hb_shutdown.borrow() { break; }
                }
            }
            match hb_api
                .heartbeat(&hb_client_id, &hb_lease_id, Some("executing"))
                .await
            {
                Ok(hb) => {
                    tracing::debug!(lease_id = %hb_lease_id, expires_at = hb.lease_expires_at, "heartbeat ok");
                }
                Err(e) if e.lease_is_lost() => {
                    tracing::error!(lease_id = %hb_lease_id, error = %e, "LEASE LOST (expired or settled elsewhere)");
                    let _ = lost_tx.send(true);
                    break;
                }
                Err(e) => {
                    tracing::warn!(lease_id = %hb_lease_id, error = %e, "heartbeat transport error; will retry");
                }
            }
        }
    });

    let limiter = Arc::new(RateLimiter::combined(
        (lease.rate_budget.requests_per_min, lease.rate_budget.bytes_per_min),
        state.cfg.rate_limit,
    ));
    let stager = match build_stager(&state.cfg.staging, state.source_http.clone()) {
        Ok(s) => s,
        Err(e) => {
            heartbeat_task.abort();
            return Err(anyhow::anyhow!("stager construction failed: {e}"));
        }
    };

    let exec_ctx = ExecContext {
        http: state.source_http.clone(),
        limiter,
        stager,
        lease: LeaseContext::from_lease(&lease),
        user_agent: format!(
            "lh-contributor/{} (+lh-tracker)",
            lh_contributor_client::client_version()
        ),
        brave_api_key: state.cfg.brave_api_key.clone(),
        lease_lost: lost_rx,
        dry_run: state.dry_run,
    };

    let executed = execute(&exec_ctx, &lease.contract).await;
    heartbeat_task.abort();

    // 4. Submit result / report failure.
    match executed {
        Ok(outcome) => {
            let completed_at = lh_contributor_client::api::unix_ms();
            if state.dry_run {
                let dir = state.cfg.work_dir.join("manifests");
                tokio::fs::create_dir_all(&dir).await?;
                let path = dir.join(format!("{}.json", lease.chunk_id));
                let json = serde_json::to_string_pretty(&outcome.manifest)?;
                tokio::fs::write(&path, &json).await?;
                tracing::info!(
                    chunk_id = %lease.chunk_id,
                    path = %path.display(),
                    blobs = outcome.evidence_blobs.len(),
                    "DRY RUN: manifest written (no result submitted)"
                );
                return Ok(true);
            }
            let submit = ResultSubmit {
                chunk_id: lease.chunk_id.clone(),
                client_id: creds_snapshot.client_id.clone(),
                started_at,
                completed_at,
                manifest: outcome.manifest,
                evidence_blobs: outcome.evidence_blobs,
            };
            match state.api.submit_result(&submit).await {
                Ok(receipt) => {
                    tracing::info!(
                        chunk_id = %lease.chunk_id,
                        result_id = %receipt.result_id,
                        status = %receipt.status,
                        "result staged for verification"
                    );
                    Ok(true)
                }
                Err(e) => {
                    // A lost lease cannot be fail-reported (the fail endpoint
                    // 404/409s on settled leases); anything else is a client
                    // error worth reporting so the chunk requeues.
                    if !e.lease_is_lost() {
                        report(
                            state,
                            &lease.lease_id,
                            FailureType::ClientError,
                            &format!("result submission failed after successful execution: {e}"),
                        )
                        .await;
                    }
                    tracing::error!(chunk_id = %lease.chunk_id, error = %e, "result submission failed");
                    Ok(true)
                }
            }
        }
        Err(failure) => {
            tracing::warn!(
                chunk_id = %lease.chunk_id,
                failure_type = %failure.failure_type,
                details = %failure.details,
                "chunk failed; reporting"
            );
            report(state, &lease.lease_id, failure.failure_type, &failure.details).await;
            Ok(true)
        }
    }
}

async fn report(state: &Arc<RunState>, lease_id: &str, failure_type: FailureType, details: &str) {
    let req = FailRequest {
        lease_id: lease_id.to_string(),
        client_id: state.creds.read().expect("creds lock").client_id.clone(),
        failure_type,
        details: Some(details.to_string()),
    };
    if let Err(e) = state.api.report_fail(&req).await {
        tracing::error!(lease_id, error = %e, "fail report itself failed");
    }
}
