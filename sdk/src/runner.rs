//! The contributor run loop, embeddable as a library.
//!
//! `Runner` owns everything the CLI's `run` command used to inline: the
//! per-slot lease poll → ed25519 verify → execute (with heartbeat) →
//! stage → submit/fail cycle, the emergency re-register path, and shutdown
//! handling. Hosts that embed this crate (the CLI, a future GUI) construct
//! a `Runner`, keep its [`StatsHandle`] for progress display, and await
//! [`Runner::run`].
//!
//! Shutdown wiring: the runner has one internal `watch` channel. When
//! [`RunOptions::shutdown_rx`] is supplied (GUI stop button), the runner
//! bridges it onto the internal channel; otherwise it installs a Ctrl-C
//! handler, which is the CLI behavior.

use crate::api::{
    unix_ms, ApiClient, ChunkType, Credentials, FailRequest, FailureType, LeasePoll,
    RegisterRequest, ResultSubmit, ALL_CHUNK_TYPES,
};
use crate::chunk::{execute, ExecContext, LeaseContext};
use crate::config::{save_credentials, Config, StagingConfig};
use crate::crypto::verify_lease_contract;
use crate::llm::{LlmEndpoint, LlmLane};
use crate::rate::RateLimiter;
use crate::staging::{build_stager, BlobStager};
use crate::stats::{SlotActivity, StatsHandle};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use reqwest::redirect::Policy;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;

/// Tuning for one [`Runner`] invocation. Mirrors the CLI flags; GUI hosts
/// set the same fields programmatically.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Exit each slot after its first chunk settles (result, fail, or
    /// dry-run manifest). CLI: `--once`.
    pub once: bool,
    /// Override `LH_CONCURRENCY` / `Config::concurrency` (clamped 1..=8).
    /// CLI: `--concurrency`.
    pub concurrency: Option<u32>,
    /// Chunk types to lease. Must be non-empty. CLI: `--capabilities`.
    pub capabilities: Vec<ChunkType>,
    /// Stage blobs locally and write manifests without submitting. CLI:
    /// `--dry-run`.
    pub dry_run: bool,
    /// External stop signal; the runner shuts down when it fires. `None`
    /// installs a Ctrl-C handler instead (CLI behavior).
    pub shutdown_rx: Option<watch::Receiver<bool>>,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            once: false,
            concurrency: None,
            capabilities: ALL_CHUNK_TYPES.to_vec(),
            dry_run: false,
            shutdown_rx: None,
        }
    }
}

/// The register request this client sends by default: all six chunk types,
/// platform string `os-arch-rust`, crate version, no llm capability. Hosts
/// with a configured local-model lane should use
/// [`register_request_with_llm`] instead so the tracker may lease classify
/// chunks.
pub fn register_request() -> RegisterRequest {
    register_request_with_llm(None)
}

/// Register request carrying the local-LLM capability when configured.
/// Declaring it is what makes classify chunks leasable to this client.
pub fn register_request_with_llm(llm: Option<&crate::api::LlmCapability>) -> RegisterRequest {
    RegisterRequest {
        platform: format!("{}-{}-rust", std::env::consts::OS, std::env::consts::ARCH),
        version: crate::client_version().to_string(),
        capabilities: ALL_CHUNK_TYPES.to_vec(),
        llm: llm.cloned(),
    }
}

/// Library-driven contributor loop. Build with [`Runner::new`], hand
/// [`Runner::stats`] to the UI, then await [`Runner::run`].
pub struct Runner {
    api: ApiClient,
    cfg: Config,
    capabilities: Vec<ChunkType>,
    dry_run: bool,
    once: bool,
    external_shutdown: Option<watch::Receiver<bool>>,
    stats: StatsHandle,
    shutdown_tx: watch::Sender<bool>,
}

impl Runner {
    /// Validate options + config and construct the runner. Offline: no
    /// network I/O happens here (registration and the health check run at
    /// the start of [`Runner::run`], matching the CLI's ordering).
    pub fn new(mut cfg: Config, opts: RunOptions) -> Result<Runner> {
        if let Some(c) = opts.concurrency {
            cfg.concurrency = c.clamp(1, 8);
        }
        if opts.capabilities.is_empty() {
            bail!("capabilities set is empty");
        }

        // Real runs need a real staging target; local staging is dry-run-only
        // (submitted blob keys must exist in R2 for verification to fetch them).
        if !opts.dry_run {
            match cfg.staging {
                StagingConfig::PresignTemplate { .. } | StagingConfig::R2 { .. } => {}
                StagingConfig::Local { .. } => bail!(
                    "real runs stage blobs to R2; configure LH_R2_* or \
                     LH_UPLOAD_PRESIGN_TEMPLATE (or use --dry-run for local staging)"
                ),
            }
        }

        let api = ApiClient::new(
            &cfg.coordinator_url,
            Duration::from_secs(cfg.http_timeout_secs),
        )?;
        let (shutdown_tx, _) = watch::channel(false);
        let stats = StatsHandle::new(cfg.concurrency as usize);
        Ok(Runner {
            api,
            cfg,
            capabilities: opts.capabilities,
            dry_run: opts.dry_run,
            once: opts.once,
            external_shutdown: opts.shutdown_rx,
            stats,
            shutdown_tx,
        })
    }

    /// Live progress handle; clone it and poll `snapshot()` from the UI.
    pub fn stats(&self) -> StatsHandle {
        self.stats.clone()
    }

    /// Ask the loop to stop: finish in-flight leases (reported as
    /// `aborted_by_user` so chunks requeue) and return from `run()`.
    /// Equivalent to Ctrl-C in the CLI.
    pub fn request_shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    /// Drive all worker slots to completion. Registers an identity when the
    /// config has none, health-checks the coordinator, then runs the loop.
    pub async fn run(self) -> Result<()> {
        let Runner {
            api,
            cfg,
            capabilities,
            dry_run,
            once,
            external_shutdown,
            stats,
            shutdown_tx,
        } = self;

        // Core loop step 1: register (or load saved client_id + auth_token).
        // The llm capability rides along when the local-model lane is
        // configured — declaring it is what unlocks classify chunks.
        let llm_capability = cfg.llm.as_ref().map(|c| c.capability.clone());
        let register_req = register_request_with_llm(llm_capability.as_ref());
        let creds = match cfg.credentials.clone() {
            Some(c) => c,
            None => {
                tracing::info!("no saved credentials; registering a new client identity");
                let c = api
                    .register(&register_req)
                    .await
                    .context("register failed")?;
                let path = save_credentials(&cfg.work_dir, &cfg.coordinator_url, &c)?;
                tracing::info!(client_id = %c.client_id, path = %path.display(), "registered");
                c
            }
        };

        // Pre-flight: coordinator reachable?
        match api.health().await {
            Ok(h) => tracing::info!(time = h.time, "coordinator healthy"),
            Err(e) => bail!("coordinator health check failed: {e}"),
        }

        // Local-LLM lane (classify grading). Built once; classify leases
        // without it are impossible in practice (the tracker gates on the
        // register declaration) and handled as typed failures if they slip
        // through anyway.
        let llm_lane = match &cfg.llm {
            Some(llm_cfg) => match LlmEndpoint::new(
                &llm_cfg.base_url,
                llm_cfg.api_key.clone(),
                &llm_cfg.model,
                Duration::from_secs(llm_cfg.timeout_secs),
            ) {
                Ok(endpoint) => {
                    tracing::info!(
                        base_url = %llm_cfg.base_url,
                        model_family = %llm_cfg.capability.model_family,
                        endpoint_kind = %llm_cfg.capability.endpoint_kind.as_str(),
                        "local-LLM grading lane active (classify chunks enabled)"
                    );
                    Some(LlmLane {
                        endpoint,
                        capability: llm_cfg.capability.clone(),
                    })
                }
                Err(e) => bail!("LLM lane configured but unusable: {e}"),
            },
            None => {
                tracing::info!("no LLM lane configured; classify chunks will not be leased");
                None
            }
        };

        let source_http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.http_timeout_secs))
            // Redirect policy: Policy::none() — the contracted source URL is
            // the final destination; any 3xx is rejected rather than followed,
            // so a compromised source cannot redirect the client off-host
            // (SSRF / host-drift). Operators must use final URLs in contracts.
            .redirect(Policy::none())
            .build()
            .context("source http client")?;
        let state = Arc::new(RunState {
            api,
            cfg,
            creds: RwLock::new(creds),
            source_http,
            capabilities,
            dry_run,
            llm_lane,
            shutdown_tx,
            consecutive_verify_failures: AtomicU32::new(0),
            re_register_used: AtomicBool::new(false),
            stats,
        });

        // Shutdown wiring: bridge the external stop signal if given, else
        // install the Ctrl-C handler (CLI behavior). Either way the internal
        // watch channel is what the slot loops listen to.
        {
            let shutdown_tx = state.shutdown_tx.clone();
            match external_shutdown {
                Some(mut rx) => {
                    tokio::spawn(async move {
                        while !*rx.borrow() {
                            if rx.changed().await.is_err() {
                                // Stop channel dropped without firing: keep
                                // running (request_shutdown still works).
                                return;
                            }
                        }
                        tracing::warn!("shutdown signal received; finishing in-flight work");
                        let _ = shutdown_tx.send(true);
                    });
                }
                None => {
                    tokio::spawn(async move {
                        if tokio::signal::ctrl_c().await.is_ok() {
                            tracing::warn!("shutdown signal received; finishing in-flight work");
                            let _ = shutdown_tx.send(true);
                        }
                    });
                }
            }
        }

        let concurrency = state.cfg.concurrency as usize;
        let shutdown_rx = state.shutdown_tx.subscribe();
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
}

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
    /// Local-LLM grading lane (classify chunks); None = not configured.
    llm_lane: Option<LlmLane>,
    shutdown_tx: watch::Sender<bool>,
    /// Consecutive contract-verification failures; abort the run beyond 3
    /// (a persistently bad signature is tampering or a wrong public key —
    /// neither heals by hammering the coordinator).
    consecutive_verify_failures: AtomicU32,
    /// Set after one emergency re-register (401 = dev D1 wipe / rotated
    /// token); a second 401 is fatal.
    re_register_used: AtomicBool,
    stats: StatsHandle,
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
            state.stats.set_slot(slot as usize, SlotActivity::Stopped);
            return Ok(());
        }
        let settled = lease_round(slot, &state, shutdown_rx).await?;
        if (settled || *shutdown_rx.borrow()) && once {
            state.stats.set_slot(slot as usize, SlotActivity::Stopped);
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
    state.stats.set_slot(slot as usize, SlotActivity::Polling);

    // 1. Poll.
    let creds_snapshot = state.creds.read().expect("creds lock").clone();
    let lease = match state
        .api
        .lease(&creds_snapshot, slot, &state.capabilities)
        .await
    {
        Ok(LeasePoll::Backoff { backoff_sec }) => {
            tracing::debug!(slot, backoff_sec, "no work; backing off");
            sleep_or_shutdown(backoff_sleep(backoff_sec), shutdown_rx).await;
            return Ok(false);
        }
        Ok(LeasePoll::Lease(lease)) => lease,
        Err(crate::error::ApiError::RateLimited { retry_after_sec }) => {
            let wait = retry_after_sec.unwrap_or(30).min(300);
            tracing::warn!(slot, wait, "coordinator rate limited; waiting");
            sleep_or_shutdown(Duration::from_secs(wait), shutdown_rx).await;
            return Ok(false);
        }
        Err(crate::error::ApiError::Unauthorized)
            if !state.re_register_used.load(Ordering::Relaxed) =>
        {
            // Identity vanished server-side (dev D1 wipe, token rotation):
            // re-register once, persist, keep going.
            state.re_register_used.store(true, Ordering::Relaxed);
            tracing::warn!("lease auth rejected; re-registering once");
            let creds = state
                .api
                .register(&register_request_with_llm(
                    state.cfg.llm.as_ref().map(|c| &c.capability),
                ))
                .await
                .context("emergency re-register failed")?;
            let path = save_credentials(&state.cfg.work_dir, &state.cfg.coordinator_url, &creds)?;
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
    state.stats.set_slot(
        slot as usize,
        SlotActivity::Verifying {
            chunk_id: lease.chunk_id.clone(),
            chunk_type: lease.contract.core.chunk_type,
        },
    );

    // 2. Verify the operator signature BEFORE executing anything.
    let now_ms = unix_ms();
    if let Err(reason) = verify_lease_contract(&state.cfg.public_key_hex, &lease, now_ms) {
        let n = state
            .consecutive_verify_failures
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        tracing::error!(lease_id = %lease.lease_id, %reason, "contract verification FAILED");
        state.stats.record_failed();
        report(
            state,
            &lease.lease_id,
            FailureType::ClientError,
            &format!("contract verification failed: {reason}"),
        )
        .await;
        if n >= 3 {
            bail!(
                "contract verification failed {n} times consecutively; {} — aborting",
                verify_failure_hint(
                    state.cfg.pinned_public_key_hex.as_deref(),
                    &state.cfg.public_key_hex
                )
            );
        }
        return Ok(true);
    }
    state
        .consecutive_verify_failures
        .store(0, Ordering::Relaxed);
    state.stats.record_verified();
    state.stats.set_slot(
        slot as usize,
        SlotActivity::Executing {
            chunk_id: lease.chunk_id.clone(),
            chunk_type: lease.contract.core.chunk_type,
        },
    );

    // 3. Execute with heartbeat.
    let started_at = now_ms;
    let (lost_tx, lost_rx) = watch::channel(false);
    let hb_api = state.api.clone();
    let hb_creds = creds_snapshot.clone();
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
                .heartbeat(&hb_creds, &hb_lease_id, Some("executing"))
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
        (
            lease.rate_budget.requests_per_min,
            lease.rate_budget.bytes_per_min,
        ),
        state.cfg.rate_limit,
    ));
    let stager = match build_stager(&state.cfg.staging, state.source_http.clone()) {
        Ok(s) => Arc::new(CountingStager {
            inner: s,
            stats: state.stats.clone(),
        }),
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
        user_agent: format!("lh-contributor/{} (+lh-tracker)", crate::client_version()),
        brave_api_key: state.cfg.brave_api_key.clone(),
        llm: state.llm_lane.clone(),
        search_proxy: Some(state.api.search_proxy(&creds_snapshot)),
        lease_lost: lost_rx,
        dry_run: state.dry_run,
    };

    let executed = execute(&exec_ctx, &lease.contract).await;
    heartbeat_task.abort();

    state.stats.set_slot(
        slot as usize,
        SlotActivity::Submitting {
            chunk_id: lease.chunk_id.clone(),
        },
    );

    // 4. Submit result / report failure.
    match executed {
        Ok(outcome) => {
            let completed_at = unix_ms();
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
                auth_token: creds_snapshot.auth_token.clone(),
                started_at,
                completed_at,
                manifest: outcome.manifest,
                evidence_blobs: outcome.evidence_blobs,
            };
            match state.api.submit_result(&submit).await {
                Ok(receipt) => {
                    state.stats.record_completed();
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
            state.stats.record_failed();
            report(
                state,
                &lease.lease_id,
                failure.failure_type,
                &failure.details,
            )
            .await;
            Ok(true)
        }
    }
}

/// RT2 F2: a stored key override that no longer matches the app's pin is
/// the one failure mode where "check LH_PUBLIC_KEY_HEX" is actively wrong
/// advice — the stored Settings key wins the resolution chain and survives
/// every app update, so three consecutive verification failures mean the
/// volunteer's own stale override is the prime suspect. The embedder sets
/// `Config.pinned_public_key_hex` (and only when a stored override is in
/// play); without it the generic env-var hint stays.
fn verify_failure_hint(pinned: Option<&str>, effective: &str) -> &'static str {
    match pinned {
        Some(pin) if pin != effective => {
            "your stored advanced key differs from the app's pinned key — clear it under Settings"
        }
        _ => "check LH_PUBLIC_KEY_HEX or suspect tampering",
    }
}

async fn report(state: &Arc<RunState>, lease_id: &str, failure_type: FailureType, details: &str) {
    let (client_id, auth_token) = {
        let creds = state.creds.read().expect("creds lock");
        (creds.client_id.clone(), creds.auth_token.clone())
    };
    let req = FailRequest {
        lease_id: lease_id.to_string(),
        client_id,
        auth_token,
        failure_type,
        details: Some(details.to_string()),
    };
    if let Err(e) = state.api.report_fail(&req).await {
        tracing::error!(lease_id, error = %e, "fail report itself failed");
    }
}

/// [`BlobStager`] decorator that feeds the `bytes_staged` counter. Counts
/// stored (compressed) bytes on successful puts only.
struct CountingStager {
    inner: Arc<dyn BlobStager>,
    stats: StatsHandle,
}

#[async_trait]
impl BlobStager for CountingStager {
    async fn put_object(
        &self,
        key: &str,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> Result<(), crate::error::StageError> {
        let stored_len = bytes.len() as u64;
        self.inner.put_object(key, content_type, bytes).await?;
        self.stats.record_bytes_staged(stored_len);
        Ok(())
    }

    async fn get_object(&self, key: &str) -> Result<Vec<u8>, crate::error::StageError> {
        self.inner.get_object(key).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::StageError;
    use crate::staging::put_zstd;
    use std::path::PathBuf;

    fn test_config(staging: StagingConfig) -> Config {
        Config {
            coordinator_url: "http://coordinator.invalid".to_string(),
            public_key_hex: "ab".repeat(32),
            pinned_public_key_hex: None,
            credentials: None,
            concurrency: 2,
            work_dir: PathBuf::from("/tmp/lh-runner-test"),
            rate_limit: None,
            staging,
            brave_api_key: None,
            http_timeout_secs: 60,
            llm: None,
        }
    }

    #[test]
    fn verify_failure_hint_names_the_stored_override_only_on_divergence() {
        let pin = "ab".repeat(32);
        assert_eq!(
            verify_failure_hint(Some(&pin), &pin),
            "check LH_PUBLIC_KEY_HEX or suspect tampering",
            "stored key matches the pin — the generic hint stays accurate"
        );
        assert_eq!(
            verify_failure_hint(Some(&pin), &"cd".repeat(32)),
            "your stored advanced key differs from the app's pinned key — clear it under Settings",
            "stale stored override is the RT2 F2 bricking shape"
        );
        assert_eq!(
            verify_failure_hint(None, &"cd".repeat(32)),
            "check LH_PUBLIC_KEY_HEX or suspect tampering",
            "no pin known (CLI / env config) — generic hint"
        );
    }

    #[test]
    fn real_run_refuses_local_staging() {
        let opts = RunOptions {
            dry_run: false,
            ..RunOptions::default()
        };
        let err = match Runner::new(
            test_config(StagingConfig::Local {
                root: PathBuf::from("/tmp/x"),
            }),
            opts,
        ) {
            Err(e) => e,
            Ok(_) => panic!("local staging must be refused for real runs"),
        };
        assert!(err.to_string().contains("real runs stage blobs to R2"));
    }

    #[test]
    fn dry_run_allows_local_staging_and_concurrency_clamps() {
        let opts = RunOptions {
            dry_run: true,
            concurrency: Some(99),
            ..RunOptions::default()
        };
        let runner = Runner::new(
            test_config(StagingConfig::Local {
                root: PathBuf::from("/tmp/x"),
            }),
            opts,
        )
        .expect("dry run with local staging is fine");
        let snap = runner.stats().snapshot();
        assert_eq!(snap.slots.len(), 8, "concurrency clamps to 8 slots");
    }

    #[test]
    fn empty_capabilities_rejected() {
        let opts = RunOptions {
            capabilities: Vec::new(),
            ..RunOptions::default()
        };
        assert!(Runner::new(
            test_config(StagingConfig::Local {
                root: PathBuf::from("/tmp/x")
            }),
            opts
        )
        .is_err());
    }

    #[test]
    fn default_options_cover_all_five_chunk_types() {
        assert_eq!(RunOptions::default().capabilities, ALL_CHUNK_TYPES.to_vec());
    }

    /// In-memory stager proving the counting decorator only counts
    /// successful puts of stored bytes.
    struct VecStager {
        fail_keys_containing: &'static str,
        stored: Mutex<Vec<(String, u64)>>,
    }

    use std::sync::Mutex;

    #[async_trait]
    impl BlobStager for VecStager {
        async fn put_object(
            &self,
            key: &str,
            _content_type: &str,
            bytes: Vec<u8>,
        ) -> Result<(), StageError> {
            if key.contains(self.fail_keys_containing) {
                return Err(StageError::Http {
                    status: 500,
                    body: "boom".to_string(),
                });
            }
            self.stored
                .lock()
                .unwrap()
                .push((key.to_string(), bytes.len() as u64));
            Ok(())
        }

        async fn get_object(&self, _key: &str) -> Result<Vec<u8>, StageError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn counting_stager_counts_stored_bytes_on_success_only() {
        let stats = StatsHandle::new(1);
        let inner: Arc<dyn BlobStager> = Arc::new(VecStager {
            fail_keys_containing: "bad",
            stored: Mutex::new(Vec::new()),
        });
        let stager = CountingStager {
            inner,
            stats: stats.clone(),
        };

        stager
            .put_object("stage/pages/0000.zst", "application/zstd", vec![0u8; 100])
            .await
            .expect("put ok");
        assert!(
            stager
                .put_object("stage/bad.zst", "application/zstd", vec![0u8; 50])
                .await
                .is_err(),
            "failed puts must error"
        );

        let snap = stats.snapshot();
        assert_eq!(snap.bytes_staged, 100, "only the successful put counts");
    }

    /// End-to-end through the real zstd helper: compressed size is what the
    /// counter sees, not the raw payload size.
    #[tokio::test]
    async fn counting_stager_via_put_zstd_counts_compressed_len() {
        let stats = StatsHandle::new(1);
        let inner: Arc<dyn BlobStager> = Arc::new(VecStager {
            fail_keys_containing: "\0never",
            stored: Mutex::new(Vec::new()),
        });
        let blob = put_zstd(inner.as_ref(), "stage/", "page", &[7u8; 4096])
            .await
            .expect("zstd put ok");

        let stager = CountingStager {
            inner,
            stats: stats.clone(),
        };
        stager
            .put_object(
                &blob.key,
                "application/zstd",
                vec![1u8; blob.stored_len as usize],
            )
            .await
            .expect("put ok");

        assert_eq!(stats.snapshot().bytes_staged, blob.stored_len);
    }
}
