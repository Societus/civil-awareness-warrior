//! GUI ↔ protocol wiring: settings, run-loop lifecycle, status snapshots.
//!
//! Lifecycle model (the lib has no "pause" primitive — RunOptions only has
//! the shutdown watch channel, so pause == graceful stop):
//!
//! - `start_contributor` builds a `Config` from settings + env and spawns
//!   `Runner::run` on Tauri's tokio runtime, holding the `watch::Sender`
//!   in `ActiveRun`.
//! - `pause_contributor` fires that watch channel. The runner bridges it to
//!   its internal shutdown signal: slots stop leasing, in-flight chunks
//!   settle politely, heartbeats stop, and `run()` returns.
//! - `resume` (or the primary button after pause/error) constructs a fresh
//!   `Runner`; the client identity persists in `credentials.json`, so a
//!   resume reuses the registered warrior instead of re-registering.
//!
//! Lease targets: `StatsHandle` exposes `{chunk_type, chunk_id}` per slot
//! but not the contract's `target_key` (the lib cannot be modified from
//! here), so a 2 s enrichment task resolves chunk_id → target_key through
//! the public `GET /v1/chunk/:id/contract` endpoint and caches it. When a
//! lookup fails the UI degrades to the chunk id.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lh_contributor_client::api::{ALL_CHUNK_TYPES, ApiClient, Credentials};
use lh_contributor_client::config::{save_credentials, Config, StagingConfig};
use lh_contributor_client::stats::SlotActivity;
use lh_contributor_client::{register_request, RunOptions, Runner, StatsHandle};
use serde::{Deserialize, Serialize};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tokio::sync::watch;
use tauri::{AppHandle, Manager};

use crate::settings::{self, GuiSettings};

pub const TRAY_ID: &str = "lh-contributor-tray";

const HEALTH_POLL_SECS: u64 = 20;
const ENRICH_POLL_SECS: u64 = 2;
const RUNTIME_TICK_SECS: u64 = 1;
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
}

impl AppState {
    pub fn new(data_dir: PathBuf, settings: GuiSettings) -> AppState {
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

    fn build_config(&self) -> Result<(Config, bool), String> {
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
        };
        Ok((cfg, s.dry_run))
    }
}

// ---------------------------------------------------------------------------
// Run-loop lifecycle
// ---------------------------------------------------------------------------

pub fn start(app: &AppHandle) -> Result<(), String> {
    let st = app.state::<AppState>();
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

    let fail = |msg: String| {
        *guard(&st.last_error) = Some(msg.clone());
        st.set_phase(Phase::Error);
        set_pause_label(app, false);
        msg
    };

    let (cfg, dry_run) = match st.build_config() {
        Ok(v) => v,
        Err(e) => return Err(fail(e)),
    };
    let (stop_tx, stop_rx) = watch::channel(false);
    let runner = match Runner::new(cfg, RunOptions {
        once: false,
        concurrency: None, // settings.concurrency is already inside Config
        capabilities: ALL_CHUNK_TYPES.to_vec(),
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
    set_pause_label(app, true);

    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        let result = runner.run().await;
        let st = app2.state::<AppState>();
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
        set_pause_label(&app2, false);
    });
    Ok(())
}

pub fn pause(app: &AppHandle) -> Result<(), String> {
    let st = app.state::<AppState>();
    match st.phase() {
        Phase::Running => {
            if let Some(run) = guard(&st.active).as_mut() {
                run.stop_requested = true;
                let _ = run.stop_tx.send(true);
            }
            st.set_phase(Phase::Stopping);
            set_pause_label(app, false);
            flush_runtime(&st);
            Ok(())
        }
        Phase::Stopping => Err("already stopping — waiting for in-flight leases to settle".to_string()),
        _ => Err("contributor loop is not running".to_string()),
    }
}

pub fn toggle_pause(app: &AppHandle) {
    match app.state::<AppState>().phase() {
        Phase::Running => {
            let _ = pause(app);
        }
        Phase::Starting | Phase::Stopping => {}
        _ => {
            let _ = start(app);
        }
    }
}

pub fn graceful_shutdown_and_exit(app: &AppHandle) {
    let st = app.state::<AppState>();
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

pub fn set_pause_label(app: &AppHandle, running: bool) {
    // TrayIcon has no menu getter in this tauri version (only set_menu), so
    // the label flip rebuilds the three-item menu. Event handlers live on
    // the TrayIcon itself and keep dispatching for the replaced menu.
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let pause_label = if running { "Pause" } else { "Resume" };
    let built = (|| -> tauri::Result<()> {
        let show = MenuItem::with_id(app, "show", "Show", true, None::<&str>)?;
        let pause = MenuItem::with_id(app, "pause", pause_label, true, None::<&str>)?;
        let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
        let sep = PredefinedMenuItem::separator(app)?;
        let menu = Menu::with_items(app, &[&show, &pause, &sep, &quit])?;
        tray.set_menu(Some(menu))
    })();
    let _ = built;
}

// ---------------------------------------------------------------------------
// Background tasks (coordinator health + lease target enrichment)
// ---------------------------------------------------------------------------

pub fn start_background_tasks(app: &AppHandle) {
    let h = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(HEALTH_POLL_SECS)).await;
            poll_health(&h).await;
        }
    });
    let h = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(ENRICH_POLL_SECS)).await;
            enrich_leases(&h).await;
        }
    });
    let h = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(RUNTIME_TICK_SECS)).await;
            runtime_tick(&h).await;
        }
    });
}

pub async fn poll_health(app: &AppHandle) {
    let st = app.state::<AppState>();
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

async fn enrich_leases(app: &AppHandle) {
    let st = app.state::<AppState>();
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

async fn runtime_tick(app: &AppHandle) {
    let st = app.state::<AppState>();
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
        flush_runtime(&st);
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

#[derive(Serialize)]
pub struct UiStatus {
    pub phase: &'static str,
    pub error: Option<String>,
    pub coordinator_ok: Option<bool>,
    pub last_health_ok_ms: Option<i64>,
    pub dry_run: bool,
    pub concurrency: u32,
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
}

#[derive(Serialize)]
pub struct SettingsView {
    pub coordinator_url: String,
    pub concurrency: u32,
    pub dry_run: bool,
    pub staging_mode: &'static str,
    pub public_key_configured: bool,
    /// Last 6 hex chars of the configured PUBLIC key (not a secret, but we
    /// still avoid echoing the whole thing into the webview).
    pub public_key_tail: String,
    pub data_dir: String,
    pub credentials_path: String,
    pub credentials_present: bool,
    pub client_id: Option<String>,
}

#[derive(Deserialize)]
pub struct SettingsInput {
    pub coordinator_url: Option<String>,
    pub public_key_hex: Option<String>,
    pub concurrency: Option<u32>,
    pub dry_run: Option<bool>,
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
    let key = s.resolved_public_key();
    let creds_present = st.creds_path().exists();
    let staging_mode = AppState::staging_mode(&s);
    SettingsView {
        coordinator_url: s.coordinator_url,
        concurrency: s.concurrency,
        dry_run: s.dry_run,
        staging_mode,
        public_key_configured: !key.is_empty(),
        public_key_tail: if key.len() >= 6 {
            key[key.len() - 6..].to_string()
        } else {
            String::new()
        },
        data_dir: st.data_dir.display().to_string(),
        credentials_path: st.creds_path().display().to_string(),
        credentials_present: creds_present,
        client_id: st.credentials().map(|c| c.client_id),
    }
}

fn validate_hex_key(k: &str) -> Result<(), String> {
    if k.len() != 64 || !k.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("public key must be 64 hex characters (32-byte ed25519)".to_string());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn get_status(app: AppHandle) -> UiStatus {
    let st = app.state::<AppState>();
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
        total_runtime_secs: st.runtime_total.load(Ordering::Relaxed) + live_tail_secs,
        staging: AppState::staging_mode(&settings),
        credentials_present: st.creds_path().exists(),
        client_id: st.credentials().map(|c| c.client_id),
        stats,
        slots,
        client_version: lh_contributor_client::client_version(),
        gui_version: env!("CARGO_PKG_VERSION"),
    }
}

#[tauri::command]
pub fn start_contributor(app: AppHandle) -> Result<(), String> {
    start(&app)
}

#[tauri::command]
pub fn pause_contributor(app: AppHandle) -> Result<(), String> {
    pause(&app)
}

#[tauri::command]
pub fn resume_contributor(app: AppHandle) -> Result<(), String> {
    match app.state::<AppState>().phase() {
        Phase::Running | Phase::Starting | Phase::Stopping => {
            Err("contributor loop already active — use Pause".to_string())
        }
        _ => start(&app),
    }
}

#[tauri::command]
pub async fn register_client(app: AppHandle) -> Result<String, String> {
    let st = app.state::<AppState>();
    if matches!(
        st.phase(),
        Phase::Running | Phase::Starting | Phase::Stopping
    ) {
        return Err("pause the contributor loop before re-registering".to_string());
    }
    let api = st.api_client()?;
    let creds = api
        .register(&register_request())
        .await
        .map_err(|e| format!("register failed: {e}"))?;
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

#[tauri::command]
pub async fn check_coordinator(app: AppHandle) -> Result<String, String> {
    let st = app.state::<AppState>();
    let api = st.api_client()?;
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

#[tauri::command]
pub fn get_settings_view(app: AppHandle) -> SettingsView {
    settings_view(&app.state::<AppState>())
}

#[tauri::command]
pub fn save_settings(app: AppHandle, input: SettingsInput) -> Result<SettingsView, String> {
    let st = app.state::<AppState>();
    let mut s = st.settings_snapshot();
    if let Some(u) = input.coordinator_url {
        let u = u.trim().to_string();
        if !(u.starts_with("http://") || u.starts_with("https://")) {
            return Err("coordinator URL must start with http:// or https://".to_string());
        }
        s.coordinator_url = u;
    }
    if let Some(k) = input.public_key_hex {
        let k = k.trim().to_string();
        if !k.is_empty() {
            validate_hex_key(&k)?;
            s.public_key_hex = k;
        }
    }
    if let Some(c) = input.concurrency {
        if !(1..=8).contains(&c) {
            return Err("concurrency must be between 1 and 8".to_string());
        }
        s.concurrency = c;
    }
    if let Some(d) = input.dry_run {
        s.dry_run = d;
    }
    *guard(&st.settings) = s.clone();
    settings::write(&st.data_dir, &s)?;
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
    let st = app.state::<AppState>();
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

#[cfg(test)]
mod tests {
    use super::*;
    use lh_contributor_client::api::ChunkType;

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
}

#[tauri::command]
pub fn quit_app(app: AppHandle) {
    graceful_shutdown_and_exit(&app);
}
