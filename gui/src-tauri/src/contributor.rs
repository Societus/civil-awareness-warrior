//! GUI ↔ protocol wiring: settings, run-loop lifecycle, status snapshots.
//!
//! Lifecycle model (the lib has no "pause" primitive — RunOptions only has
//! the shutdown watch channel, so pause == graceful stop):
//!
//! - `begin_run` builds a `Config` + `RunOptions` from settings and spawns
//!   `Runner::run` on Tauri's tokio runtime, holding the `watch::Sender`
//!   in `ActiveRun`. `start_contributor` / `resume_contributor` are thin
//!   command wrappers over it.
//! - `pause_user` / the schedule watcher fire that watch channel. The runner
//!   bridges it to its internal shutdown signal: slots stop leasing,
//!   in-flight chunks run to completion and report, and `run()` returns.
//!   Caveat verified against the lib (src/runner.rs): an in-flight lease's
//!   heartbeat task *breaks* the moment the shutdown signal fires — the
//!   chunk keeps executing and its result still submits, but if execution
//!   outlives the lease's remaining validity the lease lapses and the chunk
//!   requeues for another volunteer (no data loss). A true soft-pause
//!   (keep heartbeating in-flight leases until they settle) needs a lib
//!   primitive and is tracked as a follow-up; this card must not modify the
//!   client lib.
//! - Resume (or the primary button after pause/error) constructs a fresh
//!   `Runner`; the client identity persists in `credentials.json`, so a
//!   resume reuses the registered warrior instead of re-registering
//!   (proven by the fake-coordinator lifecycle test).
//!
//! Schedule: when a run window is enabled, a 15 s watcher politely stops
//! the loop outside the window and auto-resumes it when the window opens
//! again — auto-resume only for schedule-initiated stops; a user-initiated
//! Pause stays paused.
//!
//! Lease targets: `StatsHandle` exposes `{chunk_type, chunk_id}` per slot
//! but not the contract's `target_key` (the lib cannot be modified from
//! here), so a 2 s enrichment task resolves chunk_id → target_key through
//! the public `GET /v1/chunk/:id/contract` endpoint and caches it. When a
//! lookup fails the UI degrades to the chunk id.
//!
//! Local Model lane (t_3067589d): `ensure_llm_ready` runs on every Start/
//! Resume/Register. Auto mode ensures the verified model file + a healthy
//! llama-server sidecar (spawning it if needed, with a hard health budget);
//! Custom mode probes the volunteer's endpoint. The SDK `Config.llm` is set
//! ONLY when enabled AND healthy — so the register call declares the llm
//! capability exactly when the classify lane can actually execute, and
//! `llm: None` (mode None) keeps the app byte-identical to its pre-change
//! behavior. Pause/quit stop the sidecar (SIGTERM→SIGKILL, always reaped);
//! a safety monitor relaunches a crashed sidecar while the loop runs (with
//! a respawn budget) and kills any child left behind when the loop isn't.
//!
//! `AppState` lives behind an `Arc` so the lifecycle logic can be exercised
//! in tests with `app: None` (no Tauri runtime, no tray) against a
//! fake-coordinator test harness — the exact `begin_run`/`pause_user` code
//! paths the tray menu handlers invoke.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{Local, Timelike};
use lh_contributor_client::api::{ApiClient, Credentials, LlmCapability, LlmEndpointKind};
use lh_contributor_client::config::{save_credentials, Config, LlmConfig, StagingConfig};
use lh_contributor_client::stats::SlotActivity;
use lh_contributor_client::{register_request, register_request_with_llm, RunOptions, Runner, StatsHandle};
use serde::{Deserialize, Serialize};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tokio::sync::watch;
use tauri::{AppHandle, Manager};

use crate::settings::{self, FieldError, GuiSettings, LlmMode};
use crate::sidecar::{self, LaneState, LaneStatus, DEFAULT_MODEL};

pub const TRAY_ID: &str = "lh-contributor-tray";

const HEALTH_POLL_SECS: u64 = 20;
const ENRICH_POLL_SECS: u64 = 2;
const RUNTIME_TICK_SECS: u64 = 1;
/// How often the schedule watcher checks the run window. Coarse on purpose
/// — a run-window pause is a politeness gesture, not a hard cutoff.
const SCHEDULE_POLL_SECS: u64 = 15;
/// Persist the cross-session runtime total every N 1 s ticks; pauses, run
/// endings and quit flush eagerly, so a crash loses at most this much.
const RUNTIME_PERSIST_TICKS: u64 = 30;
/// Runner uptime is monotonic, so a 1 s tick can only advance 0–1 s (plus
/// rounding). Anything larger means the snapshot came from a different
/// (newer) Runner than the one `prev` tracked — reject, don't retro-add.
const RUNTIME_MAX_TICK_SECS: u64 = 5;
const COMMUNITY_TTL_MS: i64 = 60_000;
const COMMUNITY_RETRY_MS: i64 = 15_000;
/// Contract lookups attempted per chunk_id before giving up (UI falls back
/// to the chunk id itself).
const LEASE_LOOKUP_MAX_ATTEMPTS: u8 = 3;
/// Ceiling on lease_attempts so a tracker that keeps supplying failing
/// chunk_ids cannot grow this map without bound (cand_9737add). When full we
/// evict the longest-retained entries first; attempt counters are only a
/// backoff hint, so dropping a stale entry loses nothing.
const LEASE_ATTEMPTS_MAX_ENTRIES: usize = 256;
/// Why the current pause exists (UiStatus.pause_reason). 0 = not paused.
pub const PAUSE_USER: u8 = 1;
pub const PAUSE_SCHEDULE: u8 = 2;
/// Local-model chat/completions timeout (SDK default; the classify lane is
/// generous with CPU inference on purpose).
const LLM_TIMEOUT_SECS: u64 = 240;
/// Health budget when Start must bring the sidecar up itself (cold llama.cpp
/// model load on a weak CPU can legitimately take a while).
const SIDECAR_START_HEALTH_SECS: u64 = 120;
/// Health budget for safety-monitor respawns (tighter: the loop is already
/// running and classify leases deserve a prompt return).
const SIDECAR_MONITOR_HEALTH_SECS: u64 = 45;
/// Safety-monitor tick.
const SIDECAR_POLL_SECS: u64 = 10;
/// Max consecutive safety respawns before giving up and surfacing the error
/// (crash-looping llama-server must not burn a machine forever).
const SIDECAR_MAX_RESPAWN: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Starting,
    Running,
    Stopping,
    Paused,
    Error,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Starting => "starting",
            Phase::Running => "running",
            Phase::Stopping => "stopping",
            Phase::Paused => "paused",
            Phase::Error => "error",
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn guard<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A spawned `Runner::run` task plus its control channel. `finished` flips
/// when the future resolves; stats stay readable for the UI afterwards.
struct ActiveRun {
    stats: StatsHandle,
    stop_tx: watch::Sender<bool>,
    stop_requested: bool,
    finished: bool,
    /// Uptime frozen at run() completion. The lib's snapshot().uptime is
    /// computed from an Instant and keeps advancing on a finished Runner;
    /// without this, a paused session's "session uptime" would tick forever.
    final_uptime_secs: u64,
}

// Local Model lane bookkeeping lives in `sidecar::LaneState` (imported).
pub struct AppState {
    data_dir: PathBuf,
    settings: Mutex<GuiSettings>,
    phase: Mutex<Phase>,
    last_error: Mutex<Option<String>>,
    /// 0 = never checked, 1 = healthy, 2 = unreachable.
    coordinator_ok: AtomicU8,
    last_health_ok_ms: AtomicI64,
    active: Mutex<Option<ActiveRun>>,
    /// chunk_id → target_key, filled by the enrichment task.
    lease_targets: Mutex<HashMap<String, String>>,
    lease_attempts: Mutex<HashMap<String, u8>>,
    /// Cross-session runtime total (persisted to runtime.json).
    /// `runtime_total` holds settled tick seconds; the live tail since the
    /// last tick is added in get_status from `runtime_prev_uptime`.
    runtime_total: AtomicU64,
    runtime_prev_uptime: AtomicU64,
    runtime_persist_ticks: AtomicU64,
    /// 60 s-cached copy of the tracker's /public/stats.json (None until the
    /// first successful fetch; failures serve stale).
    community: Mutex<Option<CommunityStats>>,
    community_next_attempt_ms: AtomicI64,
    community_fetching: AtomicBool,
    /// Local Model lane bookkeeping (Settings model-status row + the `llm`
    /// field of the status poll).
    lane: Mutex<LaneState>,
    /// OUR llama-server child (Auto mode). Always stopped+reaped when the
    /// run loop stops — no orphaned processes (the point of this card).
    sidecar: Mutex<Option<Child>>,
    /// A bundled-model download is in flight (swap-guarded).
    download_active: AtomicBool,
    /// Consecutive safety-monitor respawns since the last healthy state.
    respawn_attempts: AtomicU8,
    /// Tray handle access for the Pause⇄Resume label flip. `None` in tests
    /// (no tray exists there — the lifecycle code paths run unchanged).
    app: Option<AppHandle>,
    /// PAUSE_USER / PAUSE_SCHEDULE / 0.
    pause_reason: AtomicU8,
}

impl AppState {
    pub fn new(data_dir: PathBuf, settings: GuiSettings, app: Option<AppHandle>) -> AppState {
        let runtime = settings::load_runtime(&data_dir);
        AppState {
            data_dir,
            settings: Mutex::new(settings),
            phase: Mutex::new(Phase::Idle),
            last_error: Mutex::new(None),
            coordinator_ok: AtomicU8::new(0),
            last_health_ok_ms: AtomicI64::new(0),
            active: Mutex::new(None),
            lease_targets: Mutex::new(HashMap::new()),
            lease_attempts: Mutex::new(HashMap::new()),
            runtime_total: AtomicU64::new(runtime.total_runtime_secs),
            runtime_prev_uptime: AtomicU64::new(0),
            runtime_persist_ticks: AtomicU64::new(0),
            community: Mutex::new(None),
            community_next_attempt_ms: AtomicI64::new(0),
            community_fetching: AtomicBool::new(false),
            lane: Mutex::new(LaneState::default()),
            sidecar: Mutex::new(None),
            download_active: AtomicBool::new(false),
            respawn_attempts: AtomicU8::new(0),
            app,
            pause_reason: AtomicU8::new(0),
        }
    }

    fn phase(&self) -> Phase {
        *guard(&self.phase)
    }

    fn set_phase(&self, p: Phase) {
        *guard(&self.phase) = p;
    }

    fn settings_snapshot(&self) -> GuiSettings {
        guard(&self.settings).clone()
    }

    fn creds_path(&self) -> PathBuf {
        self.data_dir.join("credentials.json")
    }

    fn credentials(&self) -> Option<Credentials> {
        std::fs::read_to_string(self.creds_path())
            .ok()
            .and_then(|t| serde_json::from_str::<Credentials>(&t).ok())
    }

    fn api_client(&self) -> Result<ApiClient, String> {
        let url = self.settings_snapshot().coordinator_url;
        ApiClient::new(&url, Duration::from_secs(10)).map_err(|e| format!("bad coordinator URL: {e}"))
    }

    /// The run-loop-facing capability set: only the enabled chunk types,
    /// in canonical order. Goes straight into `RunOptions::capabilities`
    /// and therefore into every lease poll body.
    fn capabilities_vec(&self) -> Vec<lh_contributor_client::api::ChunkType> {
        self.settings_snapshot().capabilities.enabled_list()
    }

    /// Local wall-clock membership in the run window.
    fn in_window_now(&self) -> bool {
        let s = self.settings_snapshot();
        if !s.schedule.enabled {
            return true;
        }
        let now = Local::now();
        settings::in_window_minutes(now.hour() * 60 + now.minute(), &s.schedule)
    }

    /// Live Runner slot count (None when no unfinished run exists) —
    /// `StatsHandle::new(cfg.concurrency)` means this mirrors the settings.
    /// The shipped watcher decides stop/resume via phase+pause_reason; this
    /// accessor exists for the lifecycle test that asserts the lib actually
    /// runs with the configured concurrency.
    #[allow(dead_code)]
    fn slot_count(&self) -> Option<usize> {
        let active = guard(&self.active);
        active
            .as_ref()
            .filter(|r| !r.finished)
            .map(|r| r.stats.snapshot().slots.len())
    }

    /// Revoke the warrior identity locally. The coordinator has no revoke
    /// endpoint (verified against lh-tracker's routes: register/lease/
    /// heartbeat/result/fail/contract only), so "best-effort server-side"
    /// is what stopping actually achieves: without credentials the client
    /// cannot lease, act on, or heartbeat anything; open leases lapse and
    /// requeue for other volunteers.
    fn unregister_identity(&self) -> Result<String, String> {
        if matches!(
            self.phase(),
            Phase::Running | Phase::Starting | Phase::Stopping
        ) {
            return Err("pause the contributor loop before unregistering".to_string());
        }
        let path = self.creds_path();
        if !path.exists() {
            return Ok("no local credentials file to remove — this client was never registered here".to_string());
        }
        std::fs::remove_file(&path).map_err(|e| format!("could not remove {}: {e}", path.display()))?;
        Ok(format!(
            "identity removed locally (deleted {}) — the coordinator has no revoke endpoint, so this client simply stops acting: any open leases lapse and requeue for other volunteers.",
            path.display()
        ))
    }

    /// User-initiated pause (tray Pause / dashboard button).
    pub fn pause_user(&self) -> Result<(), String> {
        self.stop_loop(PAUSE_USER)
    }

    fn stop_loop(&self, reason: u8) -> Result<(), String> {
        match self.phase() {
            Phase::Running => {
                if let Some(run) = guard(&self.active).as_mut() {
                    run.stop_requested = true;
                    let _ = run.stop_tx.send(true);
                }
                self.set_phase(Phase::Stopping);
                self.pause_reason.store(reason, Ordering::Relaxed);
                self.set_pause_label();
                flush_runtime(self);
                Ok(())
            }
            Phase::Stopping => {
                Err("already stopping — waiting for in-flight leases to settle".to_string())
            }
            _ => Err("contributor loop is not running".to_string()),
        }
    }

    /// Rebuild the tray menu so Pause⇄Resume matches the live phase.
    /// TrayIcon has no menu getter in this tauri version (only set_menu),
    /// so the flip rebuilds the four-item menu; the tray's event handler
    /// keeps dispatching for the replaced menu. A no-op when `app` is None.
    fn set_pause_label(&self) {
        let Some(app) = self.app.as_ref() else {
            return;
        };
        let Some(tray) = app.tray_by_id(TRAY_ID) else {
            return;
        };
        let running = matches!(self.phase(), Phase::Running | Phase::Starting);
        let pause_label = if running { "Pause" } else { "Resume" };
        let built = (|| -> tauri::Result<()> {
            let show = MenuItem::with_id(app, "show", "Show", true, None::<&str>)?;
            let settings_item = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
            let pause = MenuItem::with_id(app, "pause", pause_label, true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(app, &[&show, &settings_item, &pause, &sep, &quit])?;
            tray.set_menu(Some(menu))
        })();
        let _ = built;
    }

    // -----------------------------------------------------------------------
    // Local Model lane (t_3067589d)
    // -----------------------------------------------------------------------

    fn lane_status(&self) -> LaneStatus {
        guard(&self.lane).status
    }

    /// Set lane status + detail line. Progress is only meaningful while
    /// downloading and gets cleared otherwise.
    fn set_lane(&self, status: LaneStatus, detail: Option<String>) {
        let mut lane = guard(&self.lane);
        lane.status = status;
        lane.detail = detail;
        if status != LaneStatus::Downloading {
            lane.progress = None;
        }
    }

    /// Is OUR tracked sidecar child alive? A dead child is reaped right here.
    /// A volunteer's own llama-server is never tracked at all — the app
    /// must not kill what it didn't start.
    fn sidecar_child_alive(&self) -> bool {
        let mut slot = guard(&self.sidecar);
        match slot.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(None) => true,
                _ => {
                    slot.take(); // reaped
                    false
                }
            },
            None => false,
        }
    }

    /// The ModelSpec the persisted settings steer in Auto mode. Unknown or
    /// empty ids are canonicalized to the pinned default on every load/save
    /// (settings::normalize_local_model), so the unwrap_or here is
    /// belt-and-braces; it never steers toward an unpinned model.
    fn active_model(&self) -> &'static sidecar::ModelSpec {
        sidecar::model_for_id(&self.settings_snapshot().local_model.model_id)
            .unwrap_or(&DEFAULT_MODEL)
    }

    /// Stop (SIGTERM→SIGKILL, always reaped) our sidecar child if any, then
    /// re-derive the lane: model on disk → `model_ready` (it comes back with
    /// the next loop start), otherwise honest off/missing state.
    fn stop_sidecar(&self) {
        if let Some(child) = guard(&self.sidecar).take() {
            sidecar::stop_child(child);
        }
        let auto = self.settings_snapshot().local_model.mode == LlmMode::Auto;
        let model_ready = sidecar::model_is_ready(&self.data_dir, self.active_model());
        let mut lane = guard(&self.lane);
        lane.pid = None;
        lane.spawned_by_us = false;
        lane.status = if auto {
            if model_ready {
                LaneStatus::ModelReady
            } else {
                LaneStatus::ModelMissing
            }
        } else {
            LaneStatus::Off
        };
        lane.progress = None;
    }

    /// The gate Start/Resume/Register all pass through. Returns:
    /// `Ok(None)` — mode None (the byte-identical disabled path);
    /// `Ok(Some(config))` — enabled AND healthy, safe for `Config.llm` +
    /// capability declaration; `Err` — enabled but not healthy, carrying the
    /// human-readable refusal (lane bookkeeping already shows why).
    async fn ensure_llm_ready(&self, health_budget: Duration) -> Result<Option<LlmConfig>, String> {
        let lm = self.settings_snapshot().local_model.clone();
        match lm.mode {
            LlmMode::None => {
                // Byte-identical disabled path: no probe, no spawn, no
                // declaration. Lane bookkeeping is reset and a child left by
                // a mode switch is never kept behind.
                if self.sidecar_child_alive() {
                    self.stop_sidecar();
                }
                let mut lane = guard(&self.lane);
                lane.status = LaneStatus::Off;
                lane.detail = None;
                lane.progress = None;
                lane.pid = None;
                lane.spawned_by_us = false;
                Ok(None)
            }
            LlmMode::Custom => {
                let base = sidecar::resolved_endpoint(&lm);
                let stored_key = crate::settings::read_llm_key(&self.data_dir);
                match sidecar::probe_endpoint(&base, stored_key.as_deref()).await {
                    Ok(()) => {
                        self.set_lane(LaneStatus::Ready, None);
                        Ok(self.build_llm_config())
                    }
                    Err(e) => {
                        self.set_lane(LaneStatus::Unhealthy, Some(e.clone()));
                        Err(format!("the custom endpoint {base} did not answer (GET /health, then /v1/models): {e}"))
                    }
                }
            }
            LlmMode::Auto => {
                let spec = self.active_model();
                // 1. Model on disk, verified by a finished download state?
                if !sidecar::model_is_ready(&self.data_dir, spec) {
                    if self.download_active.load(Ordering::Relaxed) {
                        let prog = {
                            let lane = guard(&self.lane);
                            lane.progress
                        };
                        let suffix = prog
                            .map(|(d, t)| if t > 0 { format!(" ({d} / {t} bytes)") } else { String::new() })
                            .unwrap_or_default();
                        return Err(format!(
                            "the bundled model is still downloading{suffix} — Start again once the model row under Settings ▸ Local Model shows Ready"
                        ));
                    }
                    self.set_lane(LaneStatus::ModelMissing, None);
                    return Err(format!(
                        "the bundled model is not downloaded yet — it normally starts automatically when Auto-configure is saved; press Download model under Settings ▸ Local Model, then Start (bundled choice: {})",
                        sidecar::model_caption(spec)
                    ));
                }
                // 2. Already healthy with our live child? Fast path.
                if self.lane_status() == LaneStatus::Ready && self.sidecar_child_alive() {
                    return Ok(self.build_llm_config());
                }
                // 3. An already-running local server answering the endpoint
                //    (Auto mode's endpoint override doubles as "I run my
                //    own llama-server on this port") — adopt it; the app
                //    never claims ownership of a server it didn't start.
                let base = sidecar::resolved_endpoint(&lm);
                if sidecar::probe_endpoint(&base, None).await.is_ok() {
                    let mut lane = guard(&self.lane);
                    lane.status = LaneStatus::Ready;
                    lane.detail = None;
                    lane.progress = None;
                    lane.pid = None;
                    lane.spawned_by_us = false;
                    return Ok(self.build_llm_config());
                }
                // 4. Binary resolvable? Resolution order: user override >
                // verified app download > PATH. Auto-configure never makes
                // the volunteer install llama.cpp by hand: when nothing
                // resolves, the app self-acquires the pinned llama.cpp
                // release for this box (sha256-verified before use,
                // `--version`-self-checked in staging before anything is
                // installed under bin/).
                let override_set = !lm.llama_server_bin.trim().is_empty();
                let mut bin = sidecar::resolve_binary(&lm.llama_server_bin, &self.data_dir);
                if bin.is_none() && override_set {
                    // A deliberately set override is honored strictly — a
                    // broken override is an error, never a silent fallthrough
                    // to an app-managed binary the user opted out of.
                    let msg = format!(
                        "llama-server override {:#?} is not a file — fix the path under Settings ▸ Local Model ▸ Advanced, or clear it and Auto-configure will fetch the pinned llama.cpp release for you",
                        lm.llama_server_bin.trim()
                    );
                    self.set_lane(LaneStatus::BinaryMissing, Some(msg.clone()));
                    return Err(msg);
                }
                if bin.is_none() {
                    let resolved = match sidecar::select_pin_spec(self.auto_backend()) {
                        Ok(spec) => sidecar::ResolvedBinary::from_pin(spec),
                        Err(e) => {
                            self.set_lane(LaneStatus::BinaryMissing, Some(e.clone()));
                            return Err(e);
                        }
                    };
                    self.set_lane(
                        LaneStatus::BinaryDownloading,
                        Some(format!(
                            "fetching llama.cpp {} for this machine ({} · {} MB) — verified against its pinned sha256 and self-checked before install",
                            resolved.tag, resolved.asset, resolved.bytes_hint / 1_000_000
                        )),
                    );
                    match sidecar::download_binary(&self.data_dir, &resolved, |p| {
                        let mut lane = guard(&self.lane);
                        lane.progress = Some((p.done, p.total));
                    })
                    .await
                    {
                        Ok(version) => {
                            let mut lane = guard(&self.lane);
                            lane.progress = None;
                            lane.binary_version = Some(version.clone());
                            lane.binary_backend = Some(resolved.backend);
                            lane.binary_tag = Some(resolved.tag.clone());
                            eprintln!("acquired llama-server {} ({}): {version}", resolved.tag, resolved.backend);
                            bin = sidecar::resolve_binary(&lm.llama_server_bin, &self.data_dir);
                        }
                        Err(e) => {
                            let mut lane = guard(&self.lane);
                            lane.progress = None;
                            lane.detail = Some(e.clone());
                            drop(lane);
                            self.set_lane(LaneStatus::BinaryMissing, Some(e.clone()));
                            return Err(e);
                        }
                    }
                    if bin.is_none() {
                        let msg = "the download verified and installed, but the binary no longer resolves — check Settings ▸ Local Model ▸ Advanced override".to_string();
                        self.set_lane(LaneStatus::BinaryMissing, Some(msg.clone()));
                        return Err(msg);
                    }
                }
                // Capture the version of whatever we will actually run
                // (override/PATH binaries included) so Settings can show it.
                if guard(&self.lane).binary_version.is_none() {
                    if let Some(v) = bin.as_ref().and_then(|p| sidecar::capture_version(p)) {
                        guard(&self.lane).binary_version = Some(v);
                    }
                }
                // 5. Spawn ours with the cap flags; health budget is hard.
                let stale = guard(&self.sidecar).take();
                if let Some(child) = stale {
                    sidecar::stop_child(child);
                }
                let child = match sidecar::spawn_sidecar(&lm, &self.data_dir, spec) {
                    Ok(c) => c,
                    Err(e) => {
                        self.set_lane(LaneStatus::Unhealthy, Some(e.clone()));
                        return Err(e);
                    }
                };
                let pid = child.id();
                *guard(&self.sidecar) = Some(child);
                {
                    let mut lane = guard(&self.lane);
                    lane.status = LaneStatus::Starting;
                    lane.detail = None;
                    lane.progress = None;
                    lane.pid = Some(pid);
                    lane.spawned_by_us = true;
                }
                match sidecar::wait_healthy(&base, health_budget).await {
                    Ok(()) => {
                        self.respawn_attempts.store(0, Ordering::Relaxed);
                        let mut lane = guard(&self.lane);
                        lane.status = LaneStatus::Ready;
                        lane.detail = None;
                        Ok(self.build_llm_config())
                    }
                    Err(e) => {
                        let child = guard(&self.sidecar).take();
                        if let Some(child) = child {
                            sidecar::stop_child(child);
                        }
                        let tail = sidecar::log_tail(&self.data_dir, 8);
                        let detail = format!(
                            "the local model sidecar did not answer {} within {} s: {}{}",
                            base,
                            health_budget.as_secs(),
                            e,
                            if tail.is_empty() { String::new() } else { format!(" — last llama-server.log lines:\n{tail}") }
                        );
                        self.set_lane(LaneStatus::Unhealthy, Some(detail.clone()));
                        Err(detail)
                    }
                }
            }
        }
    }

    /// Matrix backend pick from the hardware advisor's knowledge: the
    /// Vulkan build only for a measured dedicated GPU; CPU otherwise. An
    /// inconclusive probe never buys a confident Vulkan pick.
    fn auto_backend(&self) -> sidecar::BinBackend {
        use crate::hardware::SystemProbe;
        match crate::hardware::RealSystemProbe.probe().gpu {
            crate::hardware::GpuReading::Detected { .. } => sidecar::BinBackend::Vulkan,
            _ => sidecar::BinBackend::Cpu,
        }
    }

    /// The SDK `Config.llm` shape from persisted settings + live lane facts
    /// (`ensure_llm_ready` decides WHEN it is used; health gates it).
    fn build_llm_config(&self) -> Option<LlmConfig> {
        let lm = self.settings_snapshot().local_model.clone();
        let auto_spec = sidecar::model_for_id(&lm.model_id).unwrap_or(&DEFAULT_MODEL);
        match lm.mode {
            LlmMode::None => None,
            LlmMode::Auto => Some(LlmConfig {
                base_url: sidecar::resolved_endpoint(&lm),
                api_key: None,
                model: auto_spec.id.to_string(),
                timeout_secs: LLM_TIMEOUT_SECS,
                capability: LlmCapability {
                    model_family: auto_spec.family.to_string(),
                    quant: Some(auto_spec.quant.to_string()),
                    ctx_tokens: lm.ctx_tokens,
                    // Honest declaration: `local-sidecar` only for a sidecar
                    // WE started; a healthy server the volunteer runs
                    // themselves is declared `custom-local`.
                    endpoint_kind: if guard(&self.lane).spawned_by_us {
                        LlmEndpointKind::LocalSidecar
                    } else {
                        LlmEndpointKind::CustomLocal
                    },
                },
            }),
            LlmMode::Custom => {
                let base = sidecar::resolved_endpoint(&lm);
                let name = lm.model_name.trim().to_string();
                Some(LlmConfig {
                    base_url: base.clone(),
                    api_key: settings::read_llm_key(&self.data_dir),
                    model: if name.is_empty() { "unknown".to_string() } else { name.clone() },
                    timeout_secs: LLM_TIMEOUT_SECS,
                    capability: LlmCapability {
                        model_family: if name.is_empty() { "unknown".to_string() } else { name },
                        quant: None,
                        ctx_tokens: lm.ctx_tokens,
                        endpoint_kind: sidecar::custom_endpoint_kind(&base),
                    },
                })
            }
        }
    }

    /// Resolve LH_* staging env the same way the lib's `Config::from_env`
    /// does; real (non-dry) runs without R2/presign env are refused by
    /// `Runner::new` with its own explanatory error.
    fn staging_from_env(data_dir: &Path) -> StagingConfig {
        if let Ok(t) = std::env::var("LH_UPLOAD_PRESIGN_TEMPLATE") {
            if !t.trim().is_empty() && t.contains("{key}") {
                return StagingConfig::PresignTemplate {
                    url_template: t.trim().to_string(),
                };
            }
        }
        let bucket = std::env::var("LH_R2_BUCKET").ok().filter(|v| !v.trim().is_empty());
        let ak = std::env::var("LH_R2_ACCESS_KEY_ID")
            .ok()
            .filter(|v| !v.trim().is_empty());
        let sk = std::env::var("LH_R2_SECRET_ACCESS_KEY")
            .ok()
            .filter(|v| !v.trim().is_empty());
        if let (Some(bucket), Some(ak), Some(sk)) = (bucket, ak, sk) {
            let endpoint = std::env::var("LH_R2_ENDPOINT")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .or_else(|| {
                    std::env::var("LH_R2_ACCOUNT_ID")
                        .ok()
                        .filter(|v| !v.trim().is_empty())
                        .map(|a| format!("https://{}.r2.cloudflarestorage.com", a.trim()))
                });
            if let Some(endpoint) = endpoint {
                return StagingConfig::R2 {
                    endpoint,
                    bucket,
                    access_key_id: ak,
                    secret_access_key: sk,
                };
            }
        }
        StagingConfig::Local {
            root: data_dir.join("staging"),
        }
    }

    fn staging_mode(settings: &GuiSettings) -> &'static str {
        if settings.dry_run {
            return "staging: local (dry run)";
        }
        if std::env::var("LH_UPLOAD_PRESIGN_TEMPLATE")
            .ok()
            .is_some_and(|t| !t.trim().is_empty() && t.contains("{key}")
                || (!t.trim().is_empty() && !t.contains("{key}")))
        {
            // A malformed template is a config error; say presign so the
            // user sees where the runner will choke.
            return "staging: presigned upload URLs";
        }
        let r2_ok = std::env::var("LH_R2_BUCKET").is_ok()
            && std::env::var("LH_R2_ACCESS_KEY_ID").is_ok()
            && std::env::var("LH_R2_SECRET_ACCESS_KEY").is_ok()
            && (std::env::var("LH_R2_ENDPOINT").is_ok() || std::env::var("LH_R2_ACCOUNT_ID").is_ok());
        if r2_ok {
            "staging: Cloudflare R2"
        } else {
            "staging: not configured (real runs will refuse to start)"
        }
    }

    fn build_config(&self, llm_cfg: Option<LlmConfig>) -> Result<(Config, bool), String> {
        let s = self.settings_snapshot();
        let key = s.resolved_public_key();
        if key.len() != 64 || !key.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(
                "contract public key missing — set LH_PUBLIC_KEY_HEX (64-char hex ed25519) \
                 or enter it under Settings"
                    .to_string(),
            );
        }
        let cfg = Config {
            coordinator_url: s.coordinator_url.clone(),
            public_key_hex: key,
            credentials: self.credentials(),
            concurrency: s.concurrency,
            work_dir: self.data_dir.clone(),
            rate_limit: None,
            staging: Self::staging_from_env(&self.data_dir),
            brave_api_key: std::env::var("LH_BRAVE_API_KEY")
                .ok()
                .filter(|v| !v.trim().is_empty()),
            http_timeout_secs: 60,
            // Local Model lane: Some only when ensure_llm_ready found the
            // mode enabled AND the lane healthy — the capability declaration
            // and classify leases follow exactly that health. Mode None
            // keeps this None, byte-identical to the pre-change app.
            llm: llm_cfg,
        };
        Ok((cfg, s.dry_run))
    }
}

// ---------------------------------------------------------------------------
// Run-loop lifecycle
// ---------------------------------------------------------------------------

/// Spawn the lib run loop for the currently persisted settings. Shared by
/// the Start/Resume commands, the tray Resume item, and the schedule
/// auto-resume — the single code path the pause/resume tests exercise.
pub async fn begin_run(st: &Arc<AppState>) -> Result<(), String> {
    {
        let active = guard(&st.active);
        if active.as_ref().is_some_and(|r| !r.finished) {
            return Err("contributor loop already running".to_string());
        }
    }
    if st.phase() == Phase::Starting {
        return Err("startup already in progress".to_string());
    }

    *guard(&st.last_error) = None;
    st.set_phase(Phase::Starting);
    *guard(&st.active) = None; // drop the previous session's stats view
    guard(&st.lease_targets).clear();
    guard(&st.lease_attempts).clear();
    // New Runner → its uptime starts at 0; reset the accumulation anchor.
    st.runtime_prev_uptime.store(0, Ordering::Relaxed);
    st.pause_reason.store(0, Ordering::Relaxed);
    st.respawn_attempts.store(0, Ordering::Relaxed);

    let fail = |msg: String| {
        // A failed start must not leave a half-spawned sidecar behind.
        if st.sidecar_child_alive() {
            st.stop_sidecar();
        }
        *guard(&st.last_error) = Some(msg.clone());
        st.set_phase(Phase::Error);
        st.set_pause_label();
        msg
    };

    // Local Model gate: Start refuses with a human-readable reason when a
    // mode is enabled but the lane isn't healthy. Mode None returns Ok(None)
    // touching no sidecar path → Config.llm stays None → byte-identical
    // pre-change behavior.
    let llm_cfg = match st.ensure_llm_ready(Duration::from_secs(SIDECAR_START_HEALTH_SECS)).await {
        Ok(v) => v,
        Err(e) => return Err(fail(e)),
    };
    let (cfg, dry_run) = match st.build_config(llm_cfg) {
        Ok(v) => v,
        Err(e) => return Err(fail(e)),
    };
    let capabilities = st.capabilities_vec();
    let (stop_tx, stop_rx) = watch::channel(false);
    let runner = match Runner::new(cfg, RunOptions {
        once: false,
        concurrency: None, // settings.concurrency is already inside Config
        capabilities,
        dry_run,
        shutdown_rx: Some(stop_rx),
    }) {
        Ok(r) => r,
        Err(e) => return Err(fail(format!("{e:#}"))),
    };

    let stats = runner.stats();
    *guard(&st.active) = Some(ActiveRun {
        stats,
        stop_tx,
        stop_requested: false,
        finished: false,
        final_uptime_secs: 0,
    });
    st.set_phase(Phase::Running);
    st.set_pause_label();

    let st = st.clone();
    tauri::async_runtime::spawn(async move {
        let result = runner.run().await;
        // The run loop is over (paused, finished, or errored): reap our
        // sidecar — an enabled app never leaves a llama-server behind when
        // the loop isn't running. The safety monitor is the backstop if
        // this hook is ever skipped.
        st.stop_sidecar();
        let was_user_stop = guard(&st.active)
            .as_ref()
            .map(|r| r.stop_requested)
            .unwrap_or(false);
        let tail_secs = {
            let mut active = guard(&st.active);
            match active.as_mut() {
                Some(run) if !run.finished => {
                    // Freeze the session's final uptime now: later snapshots
                    // of this handle would keep ticking (Instant-based uptime).
                    let cur = run.stats.snapshot().uptime.as_secs();
                    run.finished = true;
                    run.final_uptime_secs = cur;
                    // Seconds between the last 1 s runtime tick and this
                    // completion — fold the tail into the total before flush.
                    cur.saturating_sub(st.runtime_prev_uptime.load(Ordering::Relaxed))
                        .min(RUNTIME_MAX_TICK_SECS)
                }
                Some(run) => {
                    run.finished = true;
                    0
                }
                None => 0,
            }
        };
        if tail_secs > 0 {
            st.runtime_total.fetch_add(tail_secs, Ordering::Relaxed);
        }
        st.runtime_prev_uptime.store(0, Ordering::Relaxed);
        match result {
            Ok(()) => st.set_phase(if was_user_stop { Phase::Paused } else { Phase::Idle }),
            Err(e) => {
                *guard(&st.last_error) = Some(format!("{e:#}"));
                st.set_phase(Phase::Error);
            }
        }
        flush_runtime(&st);
        st.set_pause_label();
    });
    Ok(())
}

pub fn toggle_pause(app: &AppHandle) {
    let st = app.state::<Arc<AppState>>();
    match st.phase() {
        Phase::Running => {
            let _ = st.pause_user();
        }
        Phase::Starting | Phase::Stopping => {}
        _ => {
            // begin_run is async (the sidecar health wait can take minutes);
            // the tray closure can't await — failures surface through phase +
            // last_error, which the UI polls.
            let st2: Arc<AppState> = st.inner().clone();
            tauri::async_runtime::spawn(async move {
                let _ = begin_run(&st2).await;
            });
        }
    }
}

pub fn graceful_shutdown_and_exit(app: &AppHandle) {
    let st = app.state::<Arc<AppState>>();
    if let Some(run) = guard(&st.active).as_mut() {
        run.stop_requested = true;
        let _ = run.stop_tx.send(true);
    }
    st.set_phase(Phase::Stopping);
    flush_runtime(&st);
    // Grace window for the runner to finish in-flight reports before exit.
    let handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(2500));
        handle.exit(0);
    });
}

/// Last-ditch orphan protection: fires on every `RunEvent::ExitRequested`
/// route (tray Quit, `handle.exit(0)`, window shutdown) — including routes
/// that skip graceful shutdown. A sidecar child must never outlive the app.
pub fn emergency_kill_sidecar(app: &AppHandle) {
    if let Some(st) = app.try_state::<Arc<AppState>>() {
        if st.sidecar_child_alive() {
            st.stop_sidecar();
        }
    }
}

// ---------------------------------------------------------------------------
// Background tasks (coordinator health + lease target enrichment + schedule)
// ---------------------------------------------------------------------------

pub fn start_background_tasks(st: &Arc<AppState>) {
    let s = st.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(HEALTH_POLL_SECS)).await;
            poll_health(&s).await;
        }
    });
    let s = st.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(ENRICH_POLL_SECS)).await;
            enrich_leases(&s).await;
        }
    });
    let s = st.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(RUNTIME_TICK_SECS)).await;
            runtime_tick(&s).await;
        }
    });
    let s = st.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(SCHEDULE_POLL_SECS)).await;
            schedule_tick(&s).await;
        }
    });
    // Sidecar safety net (t_3067589d): respawn a dead child while the loop
    // runs (with budget), reap a child outliving the loop.
    let s = st.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(SIDECAR_POLL_SECS)).await;
            sidecar_monitor(&s).await;
        }
    });
}

/// Run-window enforcement. Outside the window a live loop is politely
/// stopped (schedule reason); when the window opens again, only a
/// schedule-stopped loop auto-resumes — a user Pause is honored until the
/// user resumes.
async fn schedule_tick(st: &Arc<AppState>) {
    if !st.settings_snapshot().schedule.enabled {
        return;
    }
    let phase = st.phase();
    let in_window = st.in_window_now();
    if !in_window && phase == Phase::Running {
        let _ = st.stop_loop(PAUSE_SCHEDULE);
    } else if in_window
        && phase == Phase::Paused
        && st.pause_reason.load(Ordering::Relaxed) == PAUSE_SCHEDULE
    {
        let _ = begin_run(st).await;
    }
}

/// Safety-monitor tick: the sidecar lifecycle convergence loop. While the
/// run loop is Running it keeps the lane healthy — a dead child WE started
/// gets relaunched within a capped budget (a crash-looping llama-server
/// must not burn a machine). When the loop isn't Running it reaps any child
/// outliving it (missed pause hook, mode switch, settings flag change).
async fn sidecar_monitor(st: &Arc<AppState>) {
    let auto = st.settings_snapshot().local_model.mode == LlmMode::Auto;
    match st.phase() {
        Phase::Idle | Phase::Paused | Phase::Error | Phase::Stopping => {
            if st.sidecar_child_alive() {
                st.stop_sidecar();
            }
        }
        Phase::Running => {
            if !auto {
                // A leftover child from an auto→custom/none settings flip
                // mid-run must go — Custom/None never own a sidecar.
                if st.sidecar_child_alive() {
                    st.stop_sidecar();
                }
                return;
            }
            match st.lane_status() {
                // begin_run / the download task own these phases.
                LaneStatus::Off
                | LaneStatus::ModelMissing
                | LaneStatus::ModelError
                | LaneStatus::BinaryMissing
                | LaneStatus::BinaryDownloading
                | LaneStatus::Downloading
                | LaneStatus::Starting => return,
                LaneStatus::Ready | LaneStatus::ModelReady | LaneStatus::Unhealthy => {}
            }
            if st.sidecar_child_alive() {
                st.respawn_attempts.store(0, Ordering::Relaxed);
                return;
            }
            // No live child. Ready + not ours = reusing the volunteer's own
            // server — nothing to babysit (its loss surfaces as classify
            // failures and an unhealthy Start/Register later).
            if st.lane_status() == LaneStatus::Ready && !guard(&st.lane).spawned_by_us {
                return;
            }
            let attempts = st.respawn_attempts.fetch_add(1, Ordering::Relaxed) + 1;
            if attempts > SIDECAR_MAX_RESPAWN {
                st.set_lane(
                    LaneStatus::Unhealthy,
                    Some(format!(
                        "the local model sidecar kept exiting ({} restart attempts failed — llama-server.log in the app data dir has the details); check it, then Start again",
                        attempts - 1
                    )),
                );
                return;
            }
            // ensure_llm_ready writes the lane + the human-readable reason
            // itself; the monitor deliberately doesn't duplicate them.
            let _ = st
                .ensure_llm_ready(Duration::from_secs(SIDECAR_MONITOR_HEALTH_SECS))
                .await;
        }
        Phase::Starting => {} // begin_run's own health budget owns this
    }
}

/// Kick off the bundled-model download (no-op when one is in flight).
/// Progress flows into the lane so the Settings model row live-refreshes;
/// success lands the verified file and flips the lane to `model_ready`.
fn start_model_download(st: &Arc<AppState>, spec: &'static sidecar::ModelSpec) {
    if st.download_active.swap(true, Ordering::Relaxed) {
        return;
    }
    {
        let mut lane = guard(&st.lane);
        lane.status = LaneStatus::Downloading;
        lane.detail = None;
        lane.progress = Some((0, spec.bytes_hint));
    }
    let st2 = st.clone();
    tauri::async_runtime::spawn(async move {
        let result = sidecar::download_model(
            &st2.data_dir,
            spec,
            |p| {
                let mut lane = guard(&st2.lane);
                lane.status = LaneStatus::Downloading;
                lane.progress = Some((p.done, p.total));
            },
        )
        .await;
        st2.download_active.store(false, Ordering::Relaxed);
        match result {
            Ok(path) => {
                let mut lane = guard(&st2.lane);
                lane.status = LaneStatus::ModelReady;
                lane.progress = None;
                lane.detail = Some(format!(
                    "bundled model verified ({} MB) → {}",
                    sidecar::on_disk_bytes(&st2.data_dir, spec) / 1_048_576,
                    path.display()
                ));
            }
            Err(e) => {
                let mut lane = guard(&st2.lane);
                lane.status = LaneStatus::ModelError;
                lane.progress = None;
                lane.detail = Some(e.clone());
                eprintln!("[local model] download failed: {e}");
            }
        }
    });
}

/// The Settings/dashboard Local Model view from current settings + lane +
/// on-disk facts (mirrored into the status poll so model/progress rows
/// live-refresh while a download or sidecar startup is in flight).
fn llm_view(st: &AppState) -> LlmView {
    let sidecar_running = st.sidecar_child_alive();
    let s = st.settings_snapshot();
    let lm = s.local_model;
    let active_spec = sidecar::model_for_id(&lm.model_id).unwrap_or(&DEFAULT_MODEL);
    let lane = guard(&st.lane);
    let progress = lane.progress.map(|(d, t)| LlmProgressOut {
        done_bytes: d,
        total_bytes: t,
        percent: if t > 0 {
            ((d as f64 / t as f64) * 100.0).round() as u32
        } else {
            0
        },
    });
    let endpoint = if lm.mode == LlmMode::None {
        None
    } else {
        Some(sidecar::resolved_endpoint(&lm))
    };
    LlmView {
        mode: lm.mode.as_str(),
        status: lane.status.as_str(),
        detail: lane.detail.clone(),
        progress,
        model_line: match lm.mode {
            LlmMode::None => None,
            LlmMode::Auto => Some(sidecar::model_caption(active_spec)),
            LlmMode::Custom => {
                let name = lm.model_name.trim();
                Some(if name.is_empty() {
                    sidecar::resolved_endpoint(&lm)
                } else {
                    name.to_string()
                })
            }
        },
        model_bytes_on_disk: if lm.mode == LlmMode::Auto {
            Some(sidecar::on_disk_bytes(&st.data_dir, active_spec))
        } else {
            None
        },
        cap: lm.cap.as_str(),
        ctx_tokens: lm.ctx_tokens,
        endpoint_is_cloud: lm.mode == LlmMode::Custom
            && sidecar::custom_endpoint_kind(&sidecar::resolved_endpoint(&lm))
                == LlmEndpointKind::Cloud,
        binary_found: lm.mode != LlmMode::Auto
            || sidecar::resolve_binary(&lm.llama_server_bin, &st.data_dir).is_some(),
        // Which llama-server the lane resolved (or will run): the version
        // line +, for an app-installed binary, its llama.cpp tag and
        // backend flavor. Across restarts the version survives in the bin
        // download state even before any spawn has happened.
        binary_version: lane.binary_version.clone().or_else(|| {
            if lm.mode == LlmMode::Auto {
                sidecar::load_bin_state(&st.data_dir)
                    .filter(|s| s.finished)
                    .and_then(|s| s.version)
            } else {
                None
            }
        }),
        binary_backend: lane.binary_backend.map(str::to_string).or_else(|| {
            if lm.mode == LlmMode::Auto {
                sidecar::load_bin_state(&st.data_dir)
                    .filter(|s| s.finished)
                    .map(|s| s.backend)
            } else {
                None
            }
        }),
        binary_tag: lane.binary_tag.clone().or_else(|| {
            if lm.mode == LlmMode::Auto {
                sidecar::load_bin_state(&st.data_dir)
                    .filter(|s| s.finished)
                    .map(|s| s.tag)
            } else {
                None
            }
        }),
        // Auto mode self-acquires the pinned llama.cpp release at Start —
        // but only for hosts on the acquisition matrix. On an off-matrix
        // box (e.g. linux aarch64) the UI must say "install it yourself",
        // not promise a fetch that will fail.
        binary_fetch_supported: lm.mode != LlmMode::Auto || sidecar::host_on_matrix(),
        api_key_present: settings::read_llm_key(&st.data_dir).is_some(),
        reused_external_server: lane.status == LaneStatus::Ready
            && !lane.spawned_by_us
            && lm.mode != LlmMode::None,
        sidecar_running,
        endpoint,
    }
}

pub async fn poll_health(st: &AppState) {
    let api = match st.api_client() {
        Ok(a) => a,
        Err(_) => {
            st.coordinator_ok.store(2, Ordering::Relaxed);
            return;
        }
    };
    match api.health().await {
        Ok(_) => {
            st.coordinator_ok.store(1, Ordering::Relaxed);
            st.last_health_ok_ms.store(now_ms(), Ordering::Relaxed);
        }
        Err(_) => st.coordinator_ok.store(2, Ordering::Relaxed),
    }
}

async fn enrich_leases(st: &AppState) {
    let snap = {
        let active = guard(&st.active);
        match active.as_ref() {
            Some(run) if !run.finished => run.stats.snapshot(),
            _ => return,
        }
    };

    let mut wanted: Vec<String> = Vec::new();
    for act in &snap.slots {
        let id = match act {
            SlotActivity::Verifying { chunk_id, .. }
            | SlotActivity::Executing { chunk_id, .. }
            | SlotActivity::Submitting { chunk_id } => chunk_id.clone(),
            _ => continue,
        };
        if guard(&st.lease_targets).contains_key(&id) {
            continue;
        }
        if guard(&st.lease_attempts).get(&id).copied().unwrap_or(0) >= LEASE_LOOKUP_MAX_ATTEMPTS {
            continue;
        }
        if !wanted.contains(&id) {
            wanted.push(id);
        }
    }
    if wanted.is_empty() {
        return;
    }

    let api = match st.api_client() {
        Ok(a) => a,
        Err(_) => return,
    };
    for id in wanted {
        match api.fetch_contract(&id).await {
            Ok(contract) => {
                guard(&st.lease_targets)
                    .insert(id.clone(), contract.core.target_key);
            }
            Err(_) => {
                *guard(&st.lease_attempts).entry(id).or_insert(0) += 1;
            }
        }
    }

    // Cap lease_attempts so tracker-supplied failing chunk_ids cannot grow
    // it without bound (cand_9737add).
    if guard(&st.lease_attempts).len() > LEASE_ATTEMPTS_MAX_ENTRIES {
        prune_lease_attempts(st);
    }

    // Keep the cache from growing without bound across a long run.
    if guard(&st.lease_targets).len() > 400 {
        let live: Vec<String> = snap
            .slots
            .iter()
            .filter_map(|a| match a {
                SlotActivity::Verifying { chunk_id, .. }
                | SlotActivity::Executing { chunk_id, .. }
                | SlotActivity::Submitting { chunk_id } => Some(chunk_id.clone()),
                _ => None,
            })
            .collect();
        let mut targets = guard(&st.lease_targets);
        targets.retain(|k, _| live.iter().any(|id| id == k));
    }
}

/// Evict `lease_attempts` entries until they are back under the cap, dropping
/// the longest-retained entries first. `lease_attempts` is a plain HashMap so
/// iteration order is the best available proxy for recency; the cap itself
/// (not the eviction order) is the security boundary.
fn prune_lease_attempts(st: &AppState) {
    let mut attempts = guard(&st.lease_attempts);
    while attempts.len() > LEASE_ATTEMPTS_MAX_ENTRIES {
        let key = attempts.keys().next().cloned();
        let Some(key) = key else { break };
        attempts.remove(&key);
    }
}

// ---------------------------------------------------------------------------
// Cross-session runtime total
// ---------------------------------------------------------------------------

/// Whole seconds to add to the cross-session total for one 1 s tick.
/// None when uptime went backwards (a new Runner) or jumped implausibly.
fn runtime_delta(prev_uptime_secs: u64, cur_uptime_secs: u64) -> Option<u64> {
    let d = cur_uptime_secs.saturating_sub(prev_uptime_secs);
    if d == 0 || d > RUNTIME_MAX_TICK_SECS {
        None
    } else {
        Some(d)
    }
}

fn flush_runtime(st: &AppState) {
    st.runtime_persist_ticks.store(0, Ordering::Relaxed);
    let state = settings::RuntimeState {
        total_runtime_secs: st.runtime_total.load(Ordering::Relaxed),
    };
    if let Err(e) = settings::write_runtime(&st.data_dir, &state) {
        eprintln!("runtime.json: {e}");
    }
}

async fn runtime_tick(st: &AppState) {
    let snap = {
        let active = guard(&st.active);
        match active.as_ref() {
            Some(run) if !run.finished => Some(run.stats.snapshot()),
            _ => None,
        }
    };
    let Some(snap) = snap else {
        // No live Runner: the next session re-anchors from 0.
        st.runtime_prev_uptime.store(0, Ordering::Relaxed);
        return;
    };
    let cur = snap.uptime.as_secs();
    let prev = st.runtime_prev_uptime.swap(cur, Ordering::Relaxed);
    if let Some(d) = runtime_delta(prev, cur) {
        st.runtime_total.fetch_add(d, Ordering::Relaxed);
    }
    let ticks = st.runtime_persist_ticks.fetch_add(1, Ordering::Relaxed);
    if ticks >= RUNTIME_PERSIST_TICKS {
        st.runtime_persist_ticks.store(0, Ordering::Relaxed);
        flush_runtime(st);
    }
}

// ---------------------------------------------------------------------------
// Serialized views for the frontend
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct StatsOut {
    pub chunks_completed: u64,
    pub chunks_failed: u64,
    pub chunks_verified: u64,
    pub bytes_staged: u64,
    pub uptime_secs: u64,
}

#[derive(Serialize)]
pub struct SlotOut {
    pub slot: u32,
    pub activity: &'static str,
    pub chunk_id: Option<String>,
    pub chunk_type: Option<String>,
    pub target_key: Option<String>,
}

#[derive(Serialize, Clone)]
pub struct LlmProgressOut {
    pub done_bytes: u64,
    pub total_bytes: u64,
    pub percent: u32,
}

/// Live view of the Local Model lane: rendered by the Settings dialog's
/// "Local Model" section and mirrored into the status poll (so the model
/// status row live-refreshes while a download or sidecar startup is in
/// flight). Carries NO secrets — a stored API key is only ever reported as
/// `api_key_present`.
#[derive(Serialize, Clone)]
pub struct LlmView {
    pub mode: &'static str,
    /// off | model_missing | downloading | model_error | model_ready |
    /// binary_missing | binary_downloading | starting | ready | unhealthy
    pub status: &'static str,
    pub detail: Option<String>,
    pub progress: Option<LlmProgressOut>,
    /// Auto: the bundled model caption (family · quant · size · license);
    /// Custom: the entered model name (or the endpoint); None: null.
    pub model_line: Option<String>,
    pub model_bytes_on_disk: Option<u64>,
    pub cap: &'static str,
    pub ctx_tokens: u32,
    pub endpoint: Option<String>,
    /// Custom mode pointing at a non-local host → the UI shows its
    /// disclosure line ("search snippets transit this provider").
    pub endpoint_is_cloud: bool,
    /// Auto mode: a llama-server binary is locatable (override, app
    /// download, or PATH — Auto mode self-acquires at Start when not).
    pub binary_found: bool,
    /// `llama-server --version` first line of the binary this app runs —
    /// captured at first spawn; for an app-installed binary it also
    /// survives restarts via the download state. null when unknown.
    pub binary_version: Option<String>,
    /// App-installed flavor: "cpu" | "vulkan" | "native". null when the
    /// binary came from override/PATH.
    pub binary_backend: Option<String>,
    /// llama.cpp release tag the app installed (bin/). null for
    /// override/PATH-sourced binaries.
    pub binary_tag: Option<String>,
    /// Auto mode can self-acquire the pinned llama.cpp release on this
    /// box (host on the linux-x64 / windows-x64 / macos-arm64 matrix).
    pub binary_fetch_supported: bool,
    /// A stored key exists (its value is NEVER sent to the webview).
    pub api_key_present: bool,
    /// The healthy endpoint is a server the volunteer already runs —
    /// reused, and never killed by the app.
    pub reused_external_server: bool,
    pub sidecar_running: bool,
}

#[derive(Serialize)]
pub struct UiStatus {
    pub phase: &'static str,
    pub error: Option<String>,
    pub coordinator_ok: Option<bool>,
    pub last_health_ok_ms: Option<i64>,
    pub dry_run: bool,
    pub concurrency: u32,
    /// Why the loop is paused: "user" (explicit Pause / quit grace) or
    /// "schedule" (stopped by the run window); null unless phase == paused.
    pub pause_reason: Option<&'static str>,
    pub schedule_enabled: bool,
    /// Whether local wall-clock time is currently inside the run window
    /// (always true when no schedule is set).
    pub schedule_in_window: bool,
    /// Wall time the contributor loop has run across every session
    /// (accumulated GUI-side, seeded from runtime.json; includes the live
    /// session's tail).
    pub total_runtime_secs: u64,
    pub staging: &'static str,
    pub credentials_present: bool,
    pub client_id: Option<String>,
    pub stats: Option<StatsOut>,
    pub slots: Vec<SlotOut>,
    pub client_version: &'static str,
    pub gui_version: &'static str,
    /// Local Model lane live view (mode "none" → status "off").
    pub llm: LlmView,
}

#[derive(Serialize)]
pub struct SettingsView {
    pub coordinator_url: String,
    pub concurrency: u32,
    pub dry_run: bool,
    pub capabilities: settings::Capabilities,
    pub schedule: settings::ScheduleCfg,
    /// Launch-at-login intent as persisted. save_settings moves the OS
    /// entry the moment this flips, and main.rs re-points it at every
    /// startup, so the checkbox reflects what the next login will do.
    pub autostart: bool,
    pub staging_mode: &'static str,
    pub public_key_configured: bool,
    /// Last 6 hex chars of the configured PUBLIC key (not a secret, but we
    /// still avoid echoing the whole thing into the webview).
    pub public_key_tail: String,
    pub data_dir: String,
    pub credentials_path: String,
    pub credentials_present: bool,
    pub client_id: Option<String>,
    /// The persisted Local Model config (form initial values). The API key
    /// is deliberately absent — SettingsView exposes only `llm` (LlmView).
    pub local_model: settings::LocalModelCfg,
    /// The Auto-configure bundled model menu straight from
    /// sidecar::bundled_models (index 0 = the default). The Rust list is the
    /// ground truth the Settings select renders; settings-util.js mirrors it
    /// only so the browser demo shows the same shape.
    pub bundled_models: Vec<BundledModelOut>,
    pub llm: LlmView,
}

#[derive(Serialize)]
pub struct BundledModelOut {
    pub id: &'static str,
    pub label: &'static str,
    pub quant: &'static str,
    /// Exact file bytes from the pinned ModelSpec.
    pub bytes: u64,
    pub license: &'static str,
}

#[derive(Deserialize)]
pub struct CapabilitiesIn {
    pub examination: Option<bool>,
    pub ingest: Option<bool>,
    pub alt_source: Option<bool>,
    pub state_walk: Option<bool>,
    pub search: Option<bool>,
}

#[derive(Deserialize)]
pub struct ScheduleIn {
    pub enabled: Option<bool>,
    pub active_from: Option<String>,
    pub active_until: Option<String>,
}

#[derive(Deserialize)]
pub struct LocalModelIn {
    pub mode: Option<String>,
    pub cap: Option<String>,
    /// Auto mode: which bundled model id to steer — a sidecar::bundled_models
    /// id. Empty/unknown canonicalizes to the pinned default
    /// (normalize_local_model); the default 2B-class file is what an absent
    /// field keeps.
    pub model_id: Option<String>,
    pub endpoint: Option<String>,
    pub llama_server_bin: Option<String>,
    pub model_name: Option<String>,
    pub ctx_tokens: Option<u32>,
}

#[derive(Deserialize)]
pub struct SettingsInput {
    pub coordinator_url: Option<String>,
    pub public_key_hex: Option<String>,
    pub concurrency: Option<u32>,
    pub dry_run: Option<bool>,
    pub capabilities: Option<CapabilitiesIn>,
    pub schedule: Option<ScheduleIn>,
    pub local_model: Option<LocalModelIn>,
    /// Launch-at-login: Some replaces the stored intent (save_settings then
    /// writes/removes the OS entry BEFORE persisting); None keeps it.
    pub autostart: Option<bool>,
    /// Some replaces/deletes the stored API key (empty string = delete);
    /// None leaves it untouched. The value goes straight to
    /// llm_api_key.json (0600, same protection class as credentials.json) —
    /// never settings.json, never echoed back in any view.
    pub llm_api_key: Option<String>,
}

/// Apply a partial update onto the current settings. Validation of the
/// merged result is a separate step (`GuiSettings::validate`) so a partial
/// save can never leave an invalid combination behind (e.g. zero caps).
fn merge_settings(current: &GuiSettings, input: &SettingsInput) -> GuiSettings {
    let mut s = current.clone();
    if let Some(u) = &input.coordinator_url {
        s.coordinator_url = u.trim().to_string();
    }
    if let Some(k) = &input.public_key_hex {
        s.public_key_hex = k.trim().to_string();
    }
    if let Some(c) = input.concurrency {
        s.concurrency = c;
    }
    if let Some(d) = input.dry_run {
        s.dry_run = d;
    }
    if let Some(caps) = &input.capabilities {
        if let Some(v) = caps.examination {
            s.capabilities.examination = v;
        }
        if let Some(v) = caps.ingest {
            s.capabilities.ingest = v;
        }
        if let Some(v) = caps.alt_source {
            s.capabilities.alt_source = v;
        }
        if let Some(v) = caps.state_walk {
            s.capabilities.state_walk = v;
        }
        if let Some(v) = caps.search {
            s.capabilities.search = v;
        }
    }
    if let Some(sched) = &input.schedule {
        if let Some(v) = sched.enabled {
            s.schedule.enabled = v;
        }
        if let Some(v) = &sched.active_from {
            s.schedule.active_from = v.trim().to_string();
        }
        if let Some(v) = &sched.active_until {
            s.schedule.active_until = v.trim().to_string();
        }
    }
    // Launch-at-login: absent keeps the stored intent (a dialog that never
    // renders the control — or a partial save — must not touch it);
    // present replaces it.
    if let Some(a) = input.autostart {
        s.autostart = a;
    }
    if let Some(lm) = &input.local_model {
        if let Some(m) = &lm.mode {
            if let Some(parsed) = settings::LlmMode::parse(m.trim()) {
                s.local_model.mode = parsed;
            }
        }
        if let Some(c) = &lm.cap {
            if let Some(parsed) = settings::ResourceCap::parse(c.trim()) {
                s.local_model.cap = parsed;
            }
        }
        if let Some(e) = &lm.endpoint {
            s.local_model.endpoint = e.trim().to_string();
        }
        if let Some(b) = &lm.llama_server_bin {
            s.local_model.llama_server_bin = b.trim().to_string();
        }
        if let Some(n) = &lm.model_name {
            s.local_model.model_name = n.trim().to_string();
        }
        if let Some(m) = &lm.model_id {
            s.local_model.model_id = m.trim().to_string();
        }
        if let Some(c) = lm.ctx_tokens {
            s.local_model.ctx_tokens = c;
        }
    }
    // classify follows the model mode, and endpoints normalize to /v1 —
    // before validate enforces the invariant.
    s.normalize_local_model();
    s
}

fn slot_out(i: usize, act: &SlotActivity, targets: &HashMap<String, String>) -> SlotOut {
    let (activity, chunk_id, chunk_type) = match act {
        SlotActivity::Polling => ("polling", None, None),
        SlotActivity::Verifying { chunk_id, chunk_type } => (
            "verifying",
            Some(chunk_id.clone()),
            Some(chunk_type.as_str().to_string()),
        ),
        SlotActivity::Executing { chunk_id, chunk_type } => (
            "executing",
            Some(chunk_id.clone()),
            Some(chunk_type.as_str().to_string()),
        ),
        SlotActivity::Submitting { chunk_id } => ("submitting", Some(chunk_id.clone()), None),
        SlotActivity::Stopped => ("stopped", None, None),
    };
    let target_key = chunk_id.as_ref().and_then(|id| targets.get(id)).cloned();
    SlotOut {
        slot: i as u32,
        activity,
        chunk_id,
        chunk_type,
        target_key,
    }
}

fn settings_view(st: &AppState) -> SettingsView {
    let s = st.settings_snapshot();
    // The dialog's key field is an "advanced" override on top of the pinned
    // binary key: `configured`/`tail` describe the STORED override (what
    // "leave blank to keep" actually keeps), not the effective resolved key
    // — with the compile-time pin in place the resolved key is never empty,
    // and using it here would mislabel a blank field as an existing entry.
    // The run loop's build_config keeps resolving the full chain.
    let stored_key = s.public_key_hex.trim().to_string();
    let creds_present = st.creds_path().exists();
    let staging_mode = AppState::staging_mode(&s);
    SettingsView {
        coordinator_url: s.coordinator_url,
        concurrency: s.concurrency,
        dry_run: s.dry_run,
        capabilities: s.capabilities.clone(),
        schedule: s.schedule.clone(),
        autostart: s.autostart,
        staging_mode,
        public_key_configured: !stored_key.is_empty(),
        public_key_tail: if stored_key.len() >= 6 {
            stored_key[stored_key.len() - 6..].to_string()
        } else {
            String::new()
        },
        data_dir: st.data_dir.display().to_string(),
        credentials_path: st.creds_path().display().to_string(),
        credentials_present: creds_present,
        client_id: st.credentials().map(|c| c.client_id),
        local_model: s.local_model.clone(),
        bundled_models: sidecar::bundled_models()
            .into_iter()
            .map(|m| BundledModelOut {
                id: m.id,
                label: m.label,
                quant: m.quant,
                bytes: m.bytes_hint,
                license: m.license,
            })
            .collect(),
        llm: llm_view(st),
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn get_status(app: AppHandle) -> UiStatus {
    let st = app.state::<Arc<AppState>>();
    let phase = st.phase();
    let settings = st.settings_snapshot();
    let error = guard(&st.last_error).clone();

    let (stats, slots, live_tail_secs) = {
        let active = guard(&st.active);
        match active.as_ref() {
            Some(run) => {
                let snap = run.stats.snapshot();
                let targets = guard(&st.lease_targets);
                let slots = snap
                    .slots
                    .iter()
                    .enumerate()
                    .map(|(i, a)| slot_out(i, a, &targets))
                    .collect();
                // Finished Runner → use the uptime frozen at completion;
                // snapshot().uptime is Instant-based and would tick forever.
                let uptime_secs = if run.finished {
                    run.final_uptime_secs
                } else {
                    snap.uptime.as_secs()
                };
                let live_tail = if run.finished {
                    0
                } else {
                    uptime_secs
                        .saturating_sub(st.runtime_prev_uptime.load(Ordering::Relaxed))
                        .min(RUNTIME_MAX_TICK_SECS)
                };
                (
                    Some(StatsOut {
                        chunks_completed: snap.chunks_completed,
                        chunks_failed: snap.chunks_failed,
                        chunks_verified: snap.chunks_verified,
                        bytes_staged: snap.bytes_staged,
                        uptime_secs,
                    }),
                    slots,
                    live_tail,
                )
            }
            None => (None, Vec::new(), 0),
        }
    };

    UiStatus {
        phase: phase.as_str(),
        error,
        coordinator_ok: match st.coordinator_ok.load(Ordering::Relaxed) {
            0 => None,
            1 => Some(true),
            _ => Some(false),
        },
        last_health_ok_ms: match st.last_health_ok_ms.load(Ordering::Relaxed) {
            0 => None,
            v => Some(v),
        },
        dry_run: settings.dry_run,
        concurrency: settings.concurrency,
        pause_reason: match st.pause_reason.load(Ordering::Relaxed) {
            PAUSE_USER if phase == Phase::Paused => Some("user"),
            PAUSE_SCHEDULE if phase == Phase::Paused => Some("schedule"),
            _ => None,
        },
        schedule_enabled: settings.schedule.enabled,
        schedule_in_window: st.in_window_now(),
        total_runtime_secs: st.runtime_total.load(Ordering::Relaxed) + live_tail_secs,
        staging: AppState::staging_mode(&settings),
        credentials_present: st.creds_path().exists(),
        client_id: st.credentials().map(|c| c.client_id),
        stats,
        slots,
        client_version: lh_contributor_client::client_version(),
        gui_version: env!("CARGO_PKG_VERSION"),
        llm: llm_view(&st),
    }
}

#[tauri::command]
pub async fn start_contributor(app: AppHandle) -> Result<(), String> {
    begin_run(app.state::<Arc<AppState>>().inner()).await
}

#[tauri::command]
pub fn pause_contributor(app: AppHandle) -> Result<(), String> {
    app.state::<Arc<AppState>>().pause_user()
}

#[tauri::command]
pub async fn resume_contributor(app: AppHandle) -> Result<(), String> {
    let st = app.state::<Arc<AppState>>();
    match st.phase() {
        Phase::Running | Phase::Starting | Phase::Stopping => {
            Err("contributor loop already active — use Pause".to_string())
        }
        _ => begin_run(st.inner()).await,
    }
}

#[tauri::command]
pub async fn register_client(app: AppHandle) -> Result<String, String> {
    let st = app.state::<Arc<AppState>>();
    if matches!(
        st.phase(),
        Phase::Running | Phase::Starting | Phase::Stopping
    ) {
        return Err("pause the contributor loop before re-registering".to_string());
    }
    // Local Model gate: the llm capability — and with it classify leases —
    // is declared ONLY when a mode is enabled AND the endpoint probe is
    // healthy. Mode None skips every sidecar path: byte-identical pre-change
    // behavior (plain all-capabilities register request, `llm: null`).
    let llm_cfg = match st
        .ensure_llm_ready(Duration::from_secs(SIDECAR_MONITOR_HEALTH_SECS))
        .await
    {
        Ok(v) => v,
        Err(e) => return Err(e),
    };
    let api = st.api_client()?;
    let creds = if st.settings_snapshot().local_model.mode == LlmMode::None {
        api.register(&register_request())
            .await
            .map_err(|e| format!("register failed: {e}"))?
    } else {
        // The declared capability must match what `Config.llm` will carry at
        // Start — capability advertisement and execution ability can never
        // diverge (DESIGN.md: the tracker never leases classify without it).
        let cap = match &llm_cfg {
            Some(l) => l.capability.clone(),
            // Only a concurrent settings save can clear the gate mid-register;
            // fall back to the persisted shape build_llm_config defines so the
            // declaration still matches what Start would build.
            None => st
                .build_llm_config()
                .map(|c| c.capability)
                .ok_or_else(|| {
                    "the Local Model mode changed mid-register — press Register again".to_string()
                })?,
        };
        api.register(&register_request_with_llm(Some(&cap)))
            .await
            .map_err(|e| format!("register failed: {e}"))?
    };
    let path = save_credentials(&st.data_dir, &creds)
        .map_err(|e| format!("could not persist credentials: {e}"))?;
    *guard(&st.last_error) = None;
    if st.phase() == Phase::Error {
        st.set_phase(Phase::Idle);
    }
    Ok(format!(
        "registered as {} (credentials saved to {})",
        creds.client_id,
        path.display()
    ))
}

/// Remove the warrior identity locally (see `AppState::unregister_identity`
/// for what "best-effort server-side" means against a coordinator with no
/// revoke endpoint).
#[tauri::command]
pub fn unregister_identity(app: AppHandle) -> Result<String, String> {
    app.state::<Arc<AppState>>().unregister_identity()
}

#[tauri::command]
pub async fn check_coordinator(app: AppHandle, url: Option<String>) -> Result<String, String> {
    let st = app.state::<Arc<AppState>>();
    // Draft override: a coordinator URL typed but not yet saved is invisible
    // to the saved snapshot — validate exactly like save and probe the draft.
    let api = match url.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(draft_url) => {
            if !draft_url.starts_with("http://") && !draft_url.starts_with("https://") {
                return Err("coordinator URL must start with http:// or https://".to_string());
            }
            url::Url::parse(draft_url)
                .map_err(|_| "coordinator URL is not a valid URL".to_string())?;
            ApiClient::new(draft_url, Duration::from_secs(10))
                .map_err(|e| format!("bad coordinator URL: {e}"))?
        }
        None => st.api_client()?,
    };
    match api.health().await {
        Ok(h) => {
            st.coordinator_ok.store(1, Ordering::Relaxed);
            st.last_health_ok_ms.store(now_ms(), Ordering::Relaxed);
            Ok(format!("coordinator healthy (server time {})", h.time))
        }
        Err(e) => {
            st.coordinator_ok.store(2, Ordering::Relaxed);
            Err(format!("coordinator unreachable: {e}"))
        }
    }
}

/// Settings ▸ Local Model ▸ "Download model" — Auto-configure starts this
/// automatically on first enable; this is the explicit/retry path. Progress
/// streams into the model status row via the status poll.
#[tauri::command]
pub fn download_local_model(app: AppHandle) -> Result<String, String> {
    let st = app.state::<Arc<AppState>>();
    if st.settings_snapshot().local_model.mode != LlmMode::Auto {
        return Err(
            "the bundled model belongs to Auto-configure — Custom endpoint mode uses a server you run"
                .to_string(),
        );
    }
    let spec = st.active_model();
    if sidecar::model_is_ready(&st.data_dir, spec) {
        st.set_lane(LaneStatus::ModelReady, None);
        return Ok(format!(
            "bundled model already downloaded and verified: {}",
            sidecar::model_caption(spec)
        ));
    }
    if st.download_active.load(Ordering::Relaxed) {
        return Ok("download already in progress — the model row shows progress".to_string());
    }
    start_model_download(&st, spec);
    Ok(format!(
        "download started: {} (resumable, sha256-verified)",
        sidecar::model_caption(spec)
    ))
}

/// Settings ▸ Local Model ▸ free-the-disk: removes the verified model, any
/// `.part`, and the download state — and reaps a sidecar that might still
/// hold the file open. The model row flips to model_missing; re-download
/// any time (first enable downloads again automatically).
#[tauri::command]
pub fn delete_local_model(app: AppHandle) -> Result<String, String> {
    let st = app.state::<Arc<AppState>>();
    if st.download_active.load(Ordering::Relaxed) {
        return Err(
            "a download is in flight — it resumes safely after an app restart; delete once it finishes or stalls"
                .to_string(),
        );
    }
    if st.sidecar_child_alive() {
        st.stop_sidecar();
    }
    let freed = sidecar::delete_model(&st.data_dir, st.active_model());
    let auto = st.settings_snapshot().local_model.mode == LlmMode::Auto;
    st.set_lane(
        if auto {
            LaneStatus::ModelMissing
        } else {
            LaneStatus::Off
        },
        if freed > 0 {
            Some(format!("{} MB freed", freed / 1_048_576))
        } else {
            None
        },
    );
    st.respawn_attempts.store(0, Ordering::Relaxed);
    Ok(if freed > 0 {
        format!("bundled model removed — {} MB freed", freed / 1_048_576)
    } else {
        "no bundled model was on disk (nothing to remove)".to_string()
    })
}

/// Settings ▸ Local Model ▸ "Check now": probe the effective endpoint and
/// refresh the lane — exactly what Start/Register will see next. Auto mode
/// may bring the sidecar up to answer the probe; while the run loop is not
/// Running the safety monitor reaps it within ~10s (a sidecar lives only
/// while the loop runs). Errors are human sentences.
#[tauri::command]
pub async fn check_local_model(
    app: AppHandle,
    endpoint: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
) -> Result<String, String> {
    let st = app.state::<Arc<AppState>>();
    let mode = st.settings_snapshot().local_model.mode;
    if mode == LlmMode::None {
        return Ok("the Local Model is off — nothing to check".to_string());
    }
    // Draft probe: a Custom-mode endpoint typed but not yet saved is invisible
    // to the saved-snapshot path below ("not probed since the last save").
    // Validate exactly like GuiSettings::validate() — a probe must never be a
    // looser path than a save — and probe WITHOUT touching lane state:
    // nothing here persists, stores, or logs the draft values.
    if let Some(ep) = endpoint.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        if mode != LlmMode::Custom {
            return Err(
                "an endpoint override only applies to Custom endpoint mode".to_string(),
            );
        }
        if !ep.starts_with("http://") && !ep.starts_with("https://") {
            return Err("endpoint must start with http:// or https://".to_string());
        }
        url::Url::parse(ep).map_err(|_| "endpoint is not a valid URL".to_string())?;
        let base = crate::sidecar::ensure_v1(ep);
        let shown = model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|m| format!(" (model {m})"))
            .unwrap_or_default();
        // Draft key takes the draft path; no typed key falls back to the
        // stored one so Check-now verifies the full credential state either way.
        let key = api_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .map(str::to_owned)
            .or_else(|| crate::settings::read_llm_key(&st.data_dir));
        return match crate::sidecar::probe_endpoint(&base, key.as_deref()).await {
            Ok(()) => Ok(format!(
                "{base} is answering — press Save to keep it; Start/Register will then advertise the search-grading capability{shown}"
            )),
            Err(e) => Err(e),
        };
    }
    let budget = if mode == LlmMode::Auto {
        Duration::from_secs(SIDECAR_MONITOR_HEALTH_SECS)
    } else {
        Duration::from_secs(8)
    };
    match st.ensure_llm_ready(budget).await {
        Ok(Some(cfg)) => Ok(format!(
            "{} is healthy — Start/Register will advertise the search-grading capability ({} · ctx {} tokens){}",
            cfg.base_url,
            cfg.capability.model_family,
            cfg.capability.ctx_tokens,
            if mode == LlmMode::Auto
                && cfg.capability.endpoint_kind == LlmEndpointKind::CustomLocal
            {
                " — reusing a server you already run; the app did not start it and will not kill it"
            } else {
                ""
            }
        )),
        Ok(None) => Err("the Local Model mode changed mid-check — press Check again".to_string()),
        Err(e) => Err(e),
    }
}

/// What "Check for binary updates" reports — the comparison between the
/// installed bin/ build (or the app's pin when nothing is installed) and
/// what upstream llama.cpp's release chain resolves to today. Carries no
/// secrets; installs NOTHING by itself.
#[derive(Serialize)]
pub struct BinaryUpdateOut {
    /// True when the app has a verified llama-server under bin/.
    pub installed: bool,
    /// Tag the app would run today (installed tag, else the pin).
    pub current_tag: String,
    pub latest_tag: String,
    pub latest_asset: String,
    pub latest_bytes: u64,
    /// Strictly newer conforming build than `current_tag`.
    pub update_available: bool,
    /// Matrix backend the resolution used ("cpu" | "vulkan" | "native").
    pub backend: String,
    /// A set Advanced override wins resolution even after an update —
    /// surfaced so the UI can say "an override is set; updates won't
    /// affect what runs until you clear it".
    pub override_set: bool,
}

/// Re-resolve the upstream llama.cpp release chain (marker release →
/// nightly pointer → assets, GitHub-verified digests) and compare against
/// what the app has installed. NEVER switches anything on its own — the
/// user must press install_binary_update. The pinned default stays default.
#[tauri::command]
pub async fn check_binary_update(app: AppHandle) -> Result<BinaryUpdateOut, String> {
    let st = app.state::<Arc<AppState>>();
    let lm = st.settings_snapshot().local_model;
    let backend = st.auto_backend();
    let spec = sidecar::select_pin_spec(backend)?;
    let latest = sidecar::resolve_latest_binary(sidecar::GGML_RELEASES_API, backend).await?;
    let installed = sidecar::load_bin_state(&st.data_dir).filter(|s| s.finished);
    let current_tag = installed
        .as_ref()
        .map(|s| s.tag.clone())
        .unwrap_or_else(|| spec.tag.to_string());
    let update_available = match (sidecar::build_number(&latest.tag), sidecar::build_number(&current_tag)) {
        (Some(a), Some(b)) => a > b,
        // Non-conforming tags are never auto-compared — stay honest.
        _ => false,
    };
    Ok(BinaryUpdateOut {
        installed: installed.is_some(),
        current_tag,
        latest_tag: latest.tag,
        latest_asset: latest.asset,
        latest_bytes: latest.bytes_hint,
        update_available,
        backend: spec.backend_label.to_string(),
        override_set: !lm.llama_server_bin.trim().is_empty(),
    })
}

/// EXPLICIT user action following check_binary_update: download the current
/// upstream release, verify it against GitHub's own digest, self-check it,
/// and only then promote it into bin/. The old verified install stays
/// intact-and-trusted if anything fails. The app never runs this by itself.
#[tauri::command]
pub async fn install_binary_update(app: AppHandle) -> Result<String, String> {
    let st = app.state::<Arc<AppState>>();
    let backend = st.auto_backend();
    sidecar::select_pin_spec(backend)?; // this box is on the matrix at all
    let latest = sidecar::resolve_latest_binary(sidecar::GGML_RELEASES_API, backend).await?;
    if let Some(s) = sidecar::load_bin_state(&st.data_dir).filter(|s| s.finished) {
        if s.tag == latest.tag {
            return Ok(format!("{} is already installed — nothing to do", s.tag));
        }
    }
    st.set_lane(
        LaneStatus::BinaryDownloading,
        Some(format!(
            "installing llama.cpp {} — verifying {} against GitHub's digest before use",
            latest.tag, latest.asset
        )),
    );
    let result = sidecar::download_binary(&st.data_dir, &latest, |p| {
        let mut lane = guard(&st.lane);
        lane.progress = Some((p.done, p.total));
    })
    .await;
    match result {
        Ok(version) => {
            let mut lane = guard(&st.lane);
            lane.progress = None;
            lane.binary_version = Some(version.clone());
            lane.binary_backend = Some(latest.backend);
            lane.binary_tag = Some(latest.tag.clone());
            lane.detail = None;
            Ok(format!("installed llama.cpp {} — {}", latest.tag, version))
        }
        Err(e) => {
            // download_binary already restored the prior trust record (if a
            // different build was installed and verified, it is intact).
            let mut lane = guard(&st.lane);
            lane.progress = None;
            if sidecar::trusted_binary(&st.data_dir).is_some() {
                lane.status = LaneStatus::BinaryMissing;
                lane.detail = Some(format!(
                    "the update to {} failed: {e} — the previously installed build is still in place and trusted",
                    latest.tag
                ));
                // status name says "missing" for lane plumbing; the detail
                // says plainly that a working build remains.
            } else {
                lane.status = LaneStatus::BinaryMissing;
                lane.detail = Some(e.clone());
            }
            Err(e)
        }
    }
}

/// Explicitly remove the app-installed llama-server: the whole bin/ tree
/// (binary, companion libs, download state). A model file is NEVER touched,
/// and an override/PATH-sourced binary the volunteer installed themselves
/// stays exactly where it is. The pinned build is re-fetched (verified, as
/// always) on the next Start if Auto mode needs it again.
#[tauri::command]
pub fn remove_installed_binary(app: AppHandle) -> Result<String, String> {
    let st = app.state::<Arc<AppState>>();
    if st.sidecar_child_alive() {
        return Err(
            "the local model sidecar is still running — Pause/stop it first, then remove the installed binary"
                .to_string(),
        );
    }
    let was_installed = sidecar::trusted_binary(&st.data_dir).is_some()
        || sidecar::load_bin_state(&st.data_dir).is_some();
    let freed = sidecar::delete_binary(&st.data_dir);
    {
        let mut lane = guard(&st.lane);
        lane.binary_version = None;
        lane.binary_backend = None;
        lane.binary_tag = None;
        lane.progress = None;
    }
    if was_installed {
        Ok(format!(
            "removed the app-installed llama-server ({} MB freed) — Auto mode will re-fetch the pinned build at the next Start",
            freed / 1_000_000
        ))
    } else {
        Ok("no app-installed llama-server to remove — nothing under bin/".to_string())
    }
}

/// Write/remove the OS launch-at-login entry through the autostart plugin
/// (tauri-plugin-autostart 2): an HKCU Run-key value on Windows, an XDG
/// autostart .desktop on Linux (AppImage path resolved by the plugin), a
/// `~/Library/LaunchAgents` LaunchAgent on macOS. `disable()` is a no-op
/// when the entry is already absent (the plugin maps the Windows
/// "value not found" case). Every call is Rust-side — no webview
/// permission needed, so capabilities/default.json stays untouched.
fn apply_autostart(app: &AppHandle, enable: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let manager = app.autolaunch();
    let r = if enable { manager.enable() } else { manager.disable() };
    r.map_err(|e| {
        format!(
            "could not {} launch-at-login for this user: {e}",
            if enable { "register" } else { "remove" }
        )
    })
}

#[tauri::command]
pub fn get_settings_view(app: AppHandle) -> SettingsView {
    settings_view(&app.state::<Arc<AppState>>())
}

#[tauri::command]
pub fn save_settings(
    app: AppHandle,
    input: SettingsInput,
) -> Result<SettingsView, FieldError> {
    let st = app.state::<Arc<AppState>>();
    let prev = st.settings_snapshot();
    let s = {
        let merged = merge_settings(&prev, &input);
        merged.validate()?;
        merged
    };
    // API key first: an IO failure must not half-apply a settings save.
    // The value lands ONLY in llm_api_key.json — never settings.json,
    // never a settings view, never the log.
    if let Some(key) = &input.llm_api_key {
        settings::write_llm_key(&st.data_dir, Some(key))
            .map_err(|e| FieldError::new("llm_api_key", e))?;
    }
    // Launch-at-login intent changed: write the OS entry FIRST. If the OS
    // write fails the save aborts before settings.json is touched, so the
    // file can never advertise an autostart state the OS does not actually
    // have (intent and Run key / autostart .desktop / LaunchAgent stay in
    // lockstep by construction, not by luck).
    if s.autostart != prev.autostart {
        apply_autostart(&app, s.autostart).map_err(|e| FieldError::new("autostart", e))?;
    }
    *guard(&st.settings) = s.clone();
    settings::write(&st.data_dir, &s).map_err(|e| FieldError::new("settings_file", e))?;

    // Local Model lifecycle reaction to the edit — every lane converges
    // from here; the safety monitor relaunches with fresh flags when needed.
    let prev_lm = &prev.local_model;
    let now_lm = &s.local_model;
    let now_auto = now_lm.mode == LlmMode::Auto;
    if prev_lm != now_lm {
        st.respawn_attempts.store(0, Ordering::Relaxed);
        if st.sidecar_child_alive() {
            // Cap/ctx/binary/endpoint/mode changed under a live sidecar:
            // reap it now; the monitor relaunches it with the new flags.
            st.stop_sidecar();
        }
        if now_auto && prev_lm.mode != LlmMode::Auto {
            // First enable: the card's "first enable downloads" step —
            // start the bundled model download immediately so the Settings
            // model row can progress without waiting for Start.
            start_model_download(&st, sidecar::model_for_id(&now_lm.model_id).unwrap_or(&DEFAULT_MODEL));
        } else if now_auto && prev_lm.model_id != now_lm.model_id {
            // Bundled-model SWITCH (t_10daf382 "Can I AI?" opt-in 9B choice):
            // the new model is an explicit human pick, so kicking its
            // sha256-pinned download here is honoring that choice — the old
            // model's file + record stay on disk (per-model state), and the
            // reap above guarantees the sidecar restarts against the new one.
            start_model_download(&st, sidecar::model_for_id(&now_lm.model_id).unwrap_or(&DEFAULT_MODEL));
        } else if now_auto {
            let now_spec = sidecar::model_for_id(&now_lm.model_id).unwrap_or(&DEFAULT_MODEL);
            st.set_lane(
                if sidecar::model_is_ready(&st.data_dir, now_spec) {
                    LaneStatus::ModelReady
                } else if st.download_active.load(Ordering::Relaxed) {
                    LaneStatus::Downloading
                } else {
                    LaneStatus::ModelMissing
                },
                None,
            );
        } else {
            st.set_lane(LaneStatus::Off, None);
        }
    }
    // A schedule edit takes effect immediately: politely stop a loop that
    // just fell out of window / start one that just entered it. begin_run
    // is async, so the tick runs on the async runtime.
    let st2: Arc<AppState> = st.inner().clone();
    tauri::async_runtime::spawn(async move {
        schedule_tick(&st2).await;
    });
    Ok(settings_view(&st))
}

/// Subset of the tracker's `/public/stats.json` rendered on the community
/// line. Every field defaults to 0 so endpoint drift can't break the GUI.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CommunityStats {
    #[serde(default)]
    pub chunks_verified: u64,
    #[serde(default)]
    pub chunks_pending: u64,
    #[serde(default)]
    pub chunks_leased: u64,
    #[serde(default)]
    pub warriors_online: u64,
    #[serde(default)]
    pub warriors_total: u64,
    #[serde(default)]
    pub updated_at: i64,
}

async fn fetch_community(url: &str) -> Result<CommunityStats, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(url).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let body = resp.text().await.map_err(|e| e.to_string())?;
    serde_json::from_str::<CommunityStats>(&body).map_err(|e| format!("bad /public/stats.json: {e}"))
}

/// Community totals for the dashboard line. The 60 s cache lives here (the
/// frontend polls this every 2 s; only one network hit per minute actually
/// leaves the box). Failures never surface: stale cache if any, else null,
/// which the UI hides.
#[tauri::command]
pub async fn community_stats(app: AppHandle) -> Option<CommunityStats> {
    let st = app.state::<Arc<AppState>>();
    if now_ms() < st.community_next_attempt_ms.load(Ordering::Relaxed) {
        return guard(&st.community).clone();
    }
    if st.community_fetching.swap(true, Ordering::Relaxed) {
        // Another poll is already fetching; serve whatever we have.
        return guard(&st.community).clone();
    }
    let url = format!(
        "{}/public/stats.json",
        st.settings_snapshot().coordinator_url.trim_end_matches('/')
    );
    let result = fetch_community(&url).await;
    st.community_fetching.store(false, Ordering::Relaxed);
    let fresh = now_ms();
    match result {
        Ok(stats) => {
            st.community_next_attempt_ms
                .store(fresh + COMMUNITY_TTL_MS, Ordering::Relaxed);
            *guard(&st.community) = Some(stats.clone());
            Some(stats)
        }
        Err(_) => {
            st.community_next_attempt_ms
                .store(fresh + COMMUNITY_RETRY_MS, Ordering::Relaxed);
            guard(&st.community).clone()
        }
    }
}

#[tauri::command]
pub fn quit_app(app: AppHandle) {
    graceful_shutdown_and_exit(&app);
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::settings::{Capabilities, ScheduleCfg};
	use lh_contributor_client::api::ChunkType;
	use std::fs;
	use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    #[test]
    fn runtime_delta_counts_single_second_ticks() {
        assert_eq!(runtime_delta(10, 11), Some(1));
        assert_eq!(runtime_delta(10, 12), Some(2));
        assert_eq!(runtime_delta(10, 10), None);
        // A new Runner re-anchors uptime smaller than the old session's prev.
        assert_eq!(runtime_delta(4300, 2), None);
        // Implausible jumps (different/newer Runner snapshot) never retro-add.
        assert_eq!(runtime_delta(10, 10_000), None);
    }

    #[test]
    fn lease_attempts_prune_caps_at_max_entries() {
        let dir = std::env::temp_dir().join("lhc-lease-attempts-prune");
        let _ = fs::remove_dir_all(&dir);
        let settings = crate::settings::GuiSettings::defaults_from_env();
        let st = Arc::new(AppState::new(dir.clone(), settings, None));

        // A tracker that keeps supplying failing chunk_ids must not grow the
        // map without bound (cand_9737add).
        for i in 0..(LEASE_ATTEMPTS_MAX_ENTRIES + 50) {
            st.lease_attempts
                .lock()
                .unwrap()
                .insert(format!("chunk-{i}"), 1);
        }
        assert_eq!(
            st.lease_attempts.lock().unwrap().len(),
            LEASE_ATTEMPTS_MAX_ENTRIES + 50
        );

        prune_lease_attempts(&st);

        let n = st.lease_attempts.lock().unwrap().len();
        assert!(n <= LEASE_ATTEMPTS_MAX_ENTRIES, "map must be capped, got {n}");
        assert_eq!(n, LEASE_ATTEMPTS_MAX_ENTRIES);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn slot_out_resolves_target_key_through_the_cache() {
        let mut targets = HashMap::new();
        targets.insert("c1".to_string(), "Maricopa AZ/parcel-00042".to_string());
        let s = slot_out(
            0,
            &SlotActivity::Executing {
                chunk_id: "c1".to_string(),
                chunk_type: ChunkType::StateWalk,
            },
            &targets,
        );
        assert_eq!(s.activity, "executing");
        assert_eq!(s.chunk_id.as_deref(), Some("c1"));
        assert_eq!(s.chunk_type.as_deref(), Some("state_walk"));
        assert_eq!(s.target_key.as_deref(), Some("Maricopa AZ/parcel-00042"));

        let p = slot_out(1, &SlotActivity::Polling, &targets);
        assert_eq!(p.activity, "polling");
        assert_eq!(p.chunk_id, None);
        assert_eq!(p.target_key, None);

        // Unknown lease degrades to the chunk id (target_key stays None).
        let u = slot_out(
            2,
            &SlotActivity::Submitting {
                chunk_id: "zz".to_string(),
            },
            &targets,
        );
        assert_eq!(u.activity, "submitting");
        assert_eq!(u.chunk_id.as_deref(), Some("zz"));
        assert_eq!(u.target_key, None);
    }

    #[test]
    fn settings_drive_the_run_options_the_lib_consumes() {
        // Capabilities → RunOptions.capabilities mapping, and the merged
        // partial-save shape that save_settings persists.
        let current = GuiSettings {
            coordinator_url: "https://coordinator.test".to_string(),
            public_key_hex: String::new(),
            concurrency: 4,
            dry_run: true,
            capabilities: Capabilities {
                examination: true,
                ingest: true,
                alt_source: false,
                state_walk: false,
                search: false,
                classify: false,
            },
            schedule: ScheduleCfg::default(),
            local_model: settings::LocalModelCfg::default(),
            autostart: false,
        };
        let input = SettingsInput {
            coordinator_url: None,
            public_key_hex: None,
            concurrency: Some(6),
            dry_run: None,
            capabilities: Some(CapabilitiesIn {
                examination: Some(false),
                ingest: None, // untouched → stays enabled
                alt_source: Some(true),
                state_walk: None,
                search: Some(true),
            }),
            schedule: Some(ScheduleIn {
                enabled: Some(true),
                active_from: Some("22:00".to_string()),
                active_until: Some("06:00".to_string()),
            }),
            local_model: None,
            autostart: None,
            llm_api_key: None,
        };
        let merged = merge_settings(&current, &input);
        assert!(merged.validate().is_ok());
        assert_eq!(merged.concurrency, 6);
        assert_eq!(
            merged.capabilities.enabled_list(),
            vec![ChunkType::Ingest, ChunkType::AltSource, ChunkType::Search]
        );
        assert_eq!(merged.schedule.active_from, "22:00");

        // Turning EVERY capability off fails validation (the lib bails on an
        // empty RunOptions.capabilities — the GUI must refuse it earlier).
        let off = SettingsInput {
            capabilities: Some(CapabilitiesIn {
                examination: Some(false),
                ingest: Some(false),
                alt_source: Some(false),
                state_walk: Some(false),
                search: Some(false),
            }),
            coordinator_url: None,
            public_key_hex: None,
            concurrency: None,
            dry_run: None,
            schedule: None,
            local_model: None,
            autostart: None,
            llm_api_key: None,
        };
        let merged_off = merge_settings(&current, &off);
        assert_eq!(merged_off.validate().unwrap_err().field, "capabilities");
    }

    #[test]
    fn merged_model_id_canonicalizes_to_the_pinned_menu() {
        let base = GuiSettings {
            coordinator_url: "https://coordinator.test".to_string(),
            public_key_hex: String::new(),
            concurrency: 2,
            dry_run: true,
            capabilities: Capabilities::default(),
            schedule: ScheduleCfg::default(),
            local_model: settings::LocalModelCfg::default(),
            autostart: false,
        };
        let input_for = |model_id: &str| SettingsInput {
            coordinator_url: None,
            public_key_hex: None,
            concurrency: None,
            dry_run: None,
            capabilities: None,
            schedule: None,
            local_model: Some(LocalModelIn {
                mode: Some("auto".to_string()),
                cap: None,
                model_id: Some(model_id.to_string()),
                endpoint: None,
                llama_server_bin: None,
                model_name: None,
                ctx_tokens: None,
            }),
            llm_api_key: None,
            autostart: None,
        };
        // The 9B opt-in id survives the merge exactly.
        let merged = merge_settings(&base, &input_for(sidecar::LARGE_MODEL.id));
        assert!(merged.validate().is_ok());
        assert_eq!(merged.local_model.model_id, sidecar::LARGE_MODEL.id);
        assert_eq!(merged.local_model.mode, LlmMode::Auto);
        assert!(merged.capabilities.classify, "auto ⇒ classify (unchanged invariant)");
        // Junk and empty ids can never survive into Auto mode's sidecar path.
        let junk = merge_settings(&base, &input_for("totally-not-a-model"));
        assert_eq!(junk.local_model.model_id, sidecar::DEFAULT_MODEL.id);
        let empty = merge_settings(&base, &input_for("   "));
        assert_eq!(empty.local_model.model_id, sidecar::DEFAULT_MODEL.id);
        // Absent field keeps whatever the current settings say.
        let untouched = merge_settings(
            &GuiSettings {
                local_model: settings::LocalModelCfg {
                    model_id: sidecar::LARGE_MODEL.id.to_string(),
                    ..settings::LocalModelCfg::default()
                },
                ..base.clone()
            },
            &SettingsInput {
                local_model: Some(LocalModelIn {
                    model_id: None,
                    mode: Some("auto".to_string()),
                    cap: None,
                    endpoint: None,
                    llama_server_bin: None,
                    model_name: None,
                    ctx_tokens: None,
                }),
                coordinator_url: None,
                public_key_hex: None,
                concurrency: None,
                dry_run: None,
                capabilities: None,
                schedule: None,
                llm_api_key: None,
                autostart: None,
            },
        );
        assert_eq!(untouched.local_model.model_id, sidecar::LARGE_MODEL.id);
    }

    #[test]
    fn merge_autostart_absent_keeps_intent_present_replaces_it() {
        // An unrelated partial save (no autostart key) must leave the
        // launch-at-login intent exactly as stored — the OS entry belongs
        // to the human's checkbox, not to every settings write.
        let base = GuiSettings {
            coordinator_url: "https://coordinator.test".to_string(),
            public_key_hex: String::new(),
            concurrency: 2,
            dry_run: true,
            capabilities: Capabilities::default(),
            schedule: ScheduleCfg::default(),
            local_model: settings::LocalModelCfg::default(),
            autostart: true,
        };
        let input = |autostart: Option<bool>| SettingsInput {
            coordinator_url: None,
            public_key_hex: None,
            concurrency: None,
            dry_run: None,
            capabilities: None,
            schedule: None,
            local_model: None,
            autostart,
            llm_api_key: None,
        };
        assert!(merge_settings(&base, &input(None)).autostart, "absent keeps true");
        assert!(!merge_settings(&base, &input(Some(false))).autostart);
        let off_base = GuiSettings { autostart: false, ..base.clone() };
        assert!(merge_settings(&off_base, &input(Some(true))).autostart);
    }

    /// Minimal HTTP coordinator stand-in (no external deps): /health,
    /// /v1/register, /v1/lease (always `backoff_sec:1`), counting every hit
    /// and remembering the last lease request body.
    struct FakeCoord {
        base_url: String,
        lease_hits: Arc<AtomicUsize>,
        register_hits: Arc<AtomicUsize>,
        last_lease_body: Arc<Mutex<String>>,
    }

    fn spawn_fake_coordinator() -> FakeCoord {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake coordinator");
        let port = listener.local_addr().expect("port").port();
        let lease_hits = Arc::new(AtomicUsize::new(0));
        let register_hits = Arc::new(AtomicUsize::new(0));
        let last_lease_body = Arc::new(Mutex::new(String::new()));
        let (lh, rh, lb) = (
            lease_hits.clone(),
            register_hits.clone(),
            last_lease_body.clone(),
        );
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                    continue;
                }
                let mut content_len = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    let t = line.trim();
                    if t.is_empty() {
                        break;
                    }
                    if let Some(v) = t
                        .strip_prefix("Content-Length:")
                        .or_else(|| t.strip_prefix("content-length:"))
                    {
                        content_len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; content_len];
                if content_len > 0 {
                    let _ = reader.read_exact(&mut body);
                }
                let body_text = String::from_utf8_lossy(&body).to_string();
                let (status, reason, resp_body) = if request_line.starts_with("GET /health") {
                    (200u16, "OK", r#"{"ok":true,"time":1731700000000}"#.to_string())
                } else if request_line.starts_with("POST /v1/register") {
                    rh.fetch_add(1, Ordering::Relaxed);
                    (
                        200,
                        "OK",
                        r#"{"client_id":"test-warrior-0001","auth_token":"test-token-0001"}"#.to_string(),
                    )
                } else if request_line.starts_with("POST /v1/lease") {
                    lh.fetch_add(1, Ordering::Relaxed);
                    *lb.lock().expect("lease body lock") = body_text;
                    (200, "OK", r#"{"backoff_sec":1}"#.to_string())
                } else {
                    (404, "Not Found", r#"{"error":"not found"}"#.to_string())
                };
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                    resp_body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });
        FakeCoord {
            base_url: format!("http://127.0.0.1:{port}"),
            lease_hits,
            register_hits,
            last_lease_body,
        }
    }

    /// End-to-end lifecycle against the fake coordinator: the exact
    /// begin_run / pause_user code paths the Start/Resume commands and the
    /// tray menu call. Proves (1) persisted settings drive the lib loop —
    /// lease polls carry only the enabled capabilities and the Runner has
    /// `settings.concurrency` slots; (2) pause stops lease polling politely;
    /// (3) resume restarts polling while reusing the persisted identity
    /// (register hit stays at 1); (4) unregister revokes the local identity.
    #[test]
    fn fake_coordinator_lifecycle_settings_drive_loop_pause_resume_identity() {
        let fake = spawn_fake_coordinator();
        let dir = std::env::temp_dir().join("lhc-gui-lifecycle-test");
        let _ = fs::remove_dir_all(&dir);

        let settings = GuiSettings {
            coordinator_url: fake.base_url.clone(),
            public_key_hex: "ab".repeat(32),
            concurrency: 3,
            dry_run: true,
            capabilities: Capabilities {
                examination: false,
                ingest: true,
                alt_source: false,
                state_walk: false,
                search: true,
                classify: false,
            },
            schedule: ScheduleCfg::default(),
            // Mode None → the lifecycle under test is the pre-change path:
            // no llm declaration, no sidecar, `Config.llm = None`.
            local_model: settings::LocalModelCfg::default(),
            autostart: false,
        };
        let st = Arc::new(AppState::new(dir.clone(), settings, None));

        tauri::async_runtime::block_on(async {
            let lease_hits = || fake.lease_hits.load(Ordering::Relaxed);
            let register_hits = || fake.register_hits.load(Ordering::Relaxed);

            // 1. Settings → lib run loop.
            begin_run(&st).await.expect("begin_run");
            let deadline = Instant::now() + Duration::from_secs(12);
            while lease_hits() < 3 && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            assert!(lease_hits() >= 3, "three slots should each lease-poll, got {}", lease_hits());
            assert_eq!(register_hits(), 1, "auto-register exactly once");
            assert!(dir.join("credentials.json").exists(), "register persists credentials");
            let body = fake.last_lease_body.lock().expect("body").clone();
            assert!(
                body.contains(r#""capabilities":["ingest","search"]"#),
                "lease polls must offer only the enabled capabilities; got: {body}"
            );
            assert_eq!(
                st.slot_count(),
                Some(3),
                "settings.concurrency must drive the Runner slot count"
            );

            // 2. Pause politely stops lease polling.
            st.pause_user().expect("pause");
            let deadline = Instant::now() + Duration::from_secs(12);
            loop {
                let p = st.phase();
                if p == Phase::Paused {
                    break;
                }
                if Instant::now() > deadline {
                    panic!("pause did not settle the run (phase {:?})", p);
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            let hits_at_pause = lease_hits();
            tokio::time::sleep(Duration::from_secs(3)).await;
            assert_eq!(
                lease_hits(),
                hits_at_pause,
                "a paused loop must not lease-poll"
            );
            assert_eq!(st.pause_reason.load(Ordering::Relaxed), PAUSE_USER);

            // 3. Resume restarts polling and reuses the saved identity.
            begin_run(&st).await.expect("resume");
            tokio::time::sleep(Duration::from_millis(2500)).await;
            assert!(lease_hits() > hits_at_pause, "resume must restart lease polling");
            assert_eq!(
                register_hits(),
                1,
                "resume must reuse the persisted identity, not re-register"
            );

            // Teardown: stop, then unregister the identity locally.
            let _ = st.pause_user();
            let deadline = Instant::now() + Duration::from_secs(12);
            loop {
                let p = st.phase();
                if matches!(p, Phase::Paused | Phase::Idle) || Instant::now() > deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            let msg = st.unregister_identity().expect("unregister");
            assert!(msg.contains("identity removed locally"));
            assert!(
                !dir.join("credentials.json").exists(),
                "unregister must delete the local credentials file"
            );
        });
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unregister_refuses_while_the_loop_is_active() {
        let dir = std::env::temp_dir().join("lhc-gui-unregister-test");
        let _ = fs::remove_dir_all(&dir);
        let st = Arc::new(AppState::new(
            dir.clone(),
            GuiSettings::defaults_from_env(),
            None,
        ));
        st.set_phase(Phase::Running);
        assert_eq!(
            st.unregister_identity().unwrap_err(),
            "pause the contributor loop before unregistering"
        );
        st.set_phase(Phase::Idle);
        assert!(st.unregister_identity().expect("nothing registered yet").contains("never registered"));
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(st.creds_path(), r#"{"client_id":"c","auth_token":"t"}"#).expect("seed creds");
        let msg = st.unregister_identity().expect("unregister");
        assert!(msg.contains("identity removed locally"));
        assert!(!st.creds_path().exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn schedule_gate_and_manual_pause_semantics() {
        let dir = std::env::temp_dir().join("lhc-gui-schedule-test");
        let _ = fs::remove_dir_all(&dir);
        let mut base = GuiSettings::defaults_from_env();
        base.coordinator_url = "http://127.0.0.1:1".to_string(); // unroutable on purpose; begin_run pre-flight only
        base.schedule = ScheduleCfg {
            enabled: true,
            active_from: "09:00".to_string(),
            active_until: "17:00".to_string(),
        };
        let st = Arc::new(AppState::new(dir.clone(), base.clone(), None));

        // in_window_now agrees with the stored schedule + local clock.
        let now_min = {
            let n = Local::now();
            n.hour() * 60 + n.minute()
        };
        assert_eq!(
            st.in_window_now(),
            settings::in_window_minutes(now_min, &base.schedule)
        );

        // pause_reason semantics: user pause records USER; begin_run resets
        // it. (The schedule tick's own stop/resume branch reads the lib's
        // phase — proven above against the fake coordinator.)
        st.set_phase(Phase::Running);
        st.pause_reason.store(PAUSE_SCHEDULE, Ordering::Relaxed);
        let _ = st.stop_loop(PAUSE_SCHEDULE);
        assert_eq!(st.pause_reason.load(Ordering::Relaxed), PAUSE_SCHEDULE);
        st.set_phase(Phase::Idle);
        st.pause_reason.store(0, Ordering::Relaxed);
        assert!(st.pause_user().is_err(), "pause only acts on a running loop");
        let _ = fs::remove_dir_all(&dir);
    }
}
