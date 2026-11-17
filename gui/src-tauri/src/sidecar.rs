//! Local Model sidecar manager (opt-in local LLM grading, resource-capped).
//!
//! Three modes collapse onto the SDK's single `LlmConfig` surface:
//!
//! - **None** — nothing here runs; zero sidecar code paths touched.
//! - **Auto-configure** — download the bundled GGUF (see `DEFAULT_MODEL`)
//!   into `{data_dir}/models/`, self-acquire the pinned `llama-server`
//!   release binary into `{data_dir}/bin/` when the volunteer has none
//!   (override > downloaded > PATH — see the binary-acquisition section),
//!   spawn `llama-server` as a child process with resource-cap flags
//!   (`--threads`, `--n-gpu-layers`), a single slot (`--parallel 1`), a
//!   small context, and health-probe `{origin}/health` (fallback
//!   `{base}/models`) before declaring ready. The child is killed
//!   (SIGTERM, then SIGKILL after a grace window) whenever the run loop
//!   stops, so an enabled app never leaves an orphaned server behind.
//! - **Custom endpoint** — no sidecar at all: probe a volunteer-provided
//!   OpenAI-compatible endpoint and gate the capability on its health.
//!
//! Model choice (documented deviation from the card's "e.g. a Qwen3 small
//! instruct"): the official `Qwen/Qwen3-1.7B-GGUF` repo ships ONLY Q8_0
//! (1.83 GB) — its Q4 mirrors live in gated third-party repos that return
//! 401 to anonymous downloaders. `Qwen/Qwen2.5-1.5B-Instruct-GGUF` (official,
//! Apache-2.0) ships `q4_k_m` at exactly 1,117,320,736 bytes with a pinned
//! sha256, in the same small-instruct class the card asked for, and its
//! plain instruct template (no thinking-mode token sink) suits the strict
//! `classify_verdict_v1` + 512-max-tokens verdict lane. Sizes/licenses in
//! the UI come from these pinned facts.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::settings::{LocalModelCfg, ResourceCap};

/// Default sidecar port (the SDK's DEFAULT_LLM_BASE_URL points here).
pub const DEFAULT_LLM_PORT: u16 = 8081;

/// One bundled Auto-configure model choice — the full menu lives in
/// `bundled_models()`. All fields are primary-source facts from
/// `GET https://huggingface.co/api/models/Qwen/Qwen2.5-1.5B-Instruct-GGUF?blobs=true`
/// (file name, exact byte size, lfs.sha256, license=apache-2.0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSpec {
    pub id: &'static str,
    pub label: &'static str,
    pub family: &'static str,
    pub quant: &'static str,
    pub license: &'static str,
    pub repo: &'static str,
    pub file: &'static str,
    pub url: &'static str,
    /// Exact file size in bytes (from the HF blob metadata).
    pub bytes_hint: u64,
    /// Pinned sha256 (lowercase hex) verified after download.
    pub sha256: &'static str,
}

pub const DEFAULT_MODEL: ModelSpec = ModelSpec {
    id: "qwen2.5-1.5b-instruct-q4_k_m",
    label: "Qwen2.5 1.5B Instruct",
    family: "Qwen2.5",
    quant: "Q4_K_M",
    license: "Apache-2.0",
    repo: "Qwen/Qwen2.5-1.5B-Instruct-GGUF",
    file: "qwen2.5-1.5b-instruct-q4_k_m.gguf",
    url: "https://huggingface.co/Qwen/Qwen2.5-1.5B-Instruct-GGUF/resolve/main/qwen2.5-1.5b-instruct-q4_k_m.gguf",
    bytes_hint: 1_117_320_736,
    sha256: "6a1a2eb6d15622bf3c96857206351ba97e1af16c30d7a74ee38970e434e9407e",
};

/// Second, OPT-IN Auto-configure choice: the "ornith-1.5-9b-class" model the
/// "Can I AI?" advisor (hardware.rs) names for machines whose probe shows the
/// operator-proven tier-1 config (≥10 GiB dedicated VRAM or ≥24 GiB unified
/// memory). All fields are primary-source facts from
/// `GET https://huggingface.co/api/models/ornith-ai/Ornith-1.5-9B-GGUF?blobs=true`
/// (file name, exact byte size, lfs sha256, license=mit), re-verified
/// 2026-11-17. The smaller default model above stays the default; this one
/// is a selectable choice, never a silent switch (Settings ▸ Local Model ▸
/// Bundled model).
pub const LARGE_MODEL: ModelSpec = ModelSpec {
    id: "ornith-1.5-9b-q4_k_m",
    label: "Ornith-1.5-9B",
    family: "Ornith",
    quant: "Q4_K_M",
    license: "MIT",
    repo: "ornith-ai/Ornith-1.5-9B-GGUF",
    file: "ornith-1.5-9b-q4_k_m.gguf",
    url: "https://huggingface.co/ornith-ai/Ornith-1.5-9B-GGUF/resolve/main/ornith-1.5-9b-q4_k_m.gguf",
    bytes_hint: 5_780_090_816,
    sha256: "7ae5846389f612735899b85f8586b70059f0260d21248883d39fe755f878dcf5",
};

/// The bundled model menu, selection order (index 0 = the default). The
/// Rust list is the ground truth; SettingsDialog renders it from
/// SettingsView.bundled_models (settings-util.js mirrors it for the demo).
pub fn bundled_models() -> Vec<&'static ModelSpec> {
    vec![&DEFAULT_MODEL, &LARGE_MODEL]
}

/// Resolve the configured bundled model id (LocalModelCfg.model_id).
/// Unknown ids resolve to None HERE — settings.rs normalizes them to the
/// pinned default at load/save time, so a hand-edited settings.json can
/// never steer the sidecar toward an unpinned model.
pub fn model_for_id(id: &str) -> Option<&'static ModelSpec> {
    let id = id.trim();
    bundled_models().into_iter().find(|m| m.id == id)
}

pub fn models_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("models")
}

pub fn model_path(data_dir: &Path, spec: &ModelSpec) -> PathBuf {
    models_dir(data_dir).join(spec.file)
}

fn part_path(data_dir: &Path, spec: &ModelSpec) -> PathBuf {
    models_dir(data_dir).join(format!("{}.part", spec.file))
}

/// Download bookkeeping lives in ONE FILE PER MODEL — switching the bundled
/// model must never invalidate an already-verified different model, and two
/// models' resume state can't clobber each other.
fn state_path(data_dir: &Path, spec: &ModelSpec) -> PathBuf {
    models_dir(data_dir).join(format!("download_state-{}.json", spec.id))
}

/// The single state file the original sidecar card shipped. A record in it
/// that matches the requested model is copied forward to the per-model file
/// once; a record for any OTHER model is never honored (load_state).
fn legacy_state_path(data_dir: &Path) -> PathBuf {
    models_dir(data_dir).join("download_state.json")
}

/// Persisted download bookkeeping. `finished` is the trust flag: the final
/// file only exists under it (rename happens after the sha256 check), so an
/// existing model file always passed verification when it was downloaded.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DownloadState {
    pub model_id: String,
    pub done_bytes: u64,
    pub total_bytes: u64,
    #[serde(default)]
    pub finished: bool,
    #[serde(default)]
    pub error: Option<String>,
}

pub fn load_state(data_dir: &Path, spec: &ModelSpec) -> Option<DownloadState> {
    if let Some(state) = fs::read_to_string(state_path(data_dir, spec))
        .ok()
        .and_then(|t| serde_json::from_str::<DownloadState>(&t).ok())
    {
        return Some(state);
    }
    // One-time migration from the legacy single-file state: only a record
    // that names THIS model counts, and it is copied forward (the legacy
    // file is left alone — harmless, never read again once a per-model file
    // exists).
    let migrated = fs::read_to_string(legacy_state_path(data_dir))
        .ok()
        .and_then(|t| serde_json::from_str::<DownloadState>(&t).ok())
        .filter(|s| s.model_id == spec.id);
    if let Some(state) = &migrated {
        save_state(data_dir, state);
    }
    migrated
}

fn save_state(data_dir: &Path, state: &DownloadState) {
    let path = models_dir(data_dir).join(format!("download_state-{}.json", state.model_id));
    let _ = fs::create_dir_all(models_dir(data_dir));
    if let Ok(json) = serde_json::to_string_pretty(state) {
        let _ = fs::write(&path, json);
    }
}

/// True when the model file exists AND a finished download state vouches for
/// it (a stray/corrupt file of the same name is re-downloaded, not trusted).
pub fn model_is_ready(data_dir: &Path, spec: &ModelSpec) -> bool {
    let path = model_path(data_dir, spec);
    path.is_file() && load_state(data_dir, spec).is_some_and(|s| s.finished && s.model_id == spec.id)
}

pub fn on_disk_bytes(data_dir: &Path, spec: &ModelSpec) -> u64 {
    fs::metadata(model_path(data_dir, spec))
        .map(|m| m.len())
        .unwrap_or(0)
}

/// Where the local-model lane currently sits, per the app's own bookkeeping
/// (rendered into the Settings "model status" row + the status poll).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LaneStatus {
    /// Mode None (or sidecar stopped with nothing on disk).
    #[default]
    Off,
    /// Auto mode, model file not present yet.
    ModelMissing,
    /// Auto mode, GGUF download in flight.
    Downloading,
    /// Last download attempt failed (sha mismatch / HTTP); retry button.
    ModelError,
    /// Model verified on disk; sidecar not running (starts with the loop).
    ModelReady,
    /// llama-server binary could not be located.
    BinaryMissing,
    /// llama-server release binary auto-acquisition in flight (t_5f4b0943):
    /// download + sha256 verify into `{data_dir}/bin/` (progress rides the
    /// same bar as the GGUF download).
    BinaryDownloading,
    /// Sidecar spawned, health probe pending.
    Starting,
    /// Endpoint answered the health probe — capability may be declared.
    Ready,
    /// Endpoint/sidecar unreachable while a mode is enabled.
    Unhealthy,
}

impl LaneStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            LaneStatus::Off => "off",
            LaneStatus::ModelMissing => "model_missing",
            LaneStatus::Downloading => "downloading",
            LaneStatus::ModelError => "model_error",
            LaneStatus::ModelReady => "model_ready",
            LaneStatus::BinaryMissing => "binary_missing",
            LaneStatus::BinaryDownloading => "binary_downloading",
            LaneStatus::Starting => "starting",
            LaneStatus::Ready => "ready",
            LaneStatus::Unhealthy => "unhealthy",
        }
    }
}

/// Live lane bookkeeping (Mutex'd inside AppState). `spawned_by_us`
/// distinguishes OUR sidecar child (which the app must reap) from a
/// volunteer's own llama-server satisfying the endpoint (which the app
/// must never kill).
#[derive(Debug, Default)]
pub struct LaneState {
    pub status: LaneStatus,
    pub detail: Option<String>,
    /// Download progress (done, total) — total 0 when the server lied.
    pub progress: Option<(u64, u64)>,
    pub pid: Option<u32>,
    pub spawned_by_us: bool,
    /// `llama-server --version` captured when a binary first answered the
    /// app (installed-acquisition or first spawn of an existing one).
    pub binary_version: Option<String>,
    /// Build flavor the app installed for this box: "cpu" | "vulkan" |
    /// "native" (the macOS single-asset build). None for override/PATH
    /// sourcing — the app didn't pick those builds.
    pub binary_backend: Option<&'static str>,
    /// llama.cpp tag the downloaded binary came from (None for override/
    /// PATH sourcing).
    pub binary_tag: Option<String>,
}

// ---------------------------------------------------------------------------
// Endpoint helpers
// ---------------------------------------------------------------------------

/// Append `/v1` when the stored URL carries no path (LLM endpoints are
/// always the OpenAI-compatible root, e.g. `http://127.0.0.1:8081/v1`).
pub fn ensure_v1(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    match Url::parse(trimmed) {
        Ok(u) if u.path().is_empty() || u.path() == "/" => format!("{trimmed}/v1"),
        _ => trimmed.to_string(),
    }
}

/// Effective endpoint for the saved settings (auto mode defaults to the
/// loopback sidecar port when no override was entered).
pub fn resolved_endpoint(cfg: &LocalModelCfg) -> String {
    if cfg.endpoint.trim().is_empty() {
        format!("http://127.0.0.1:{DEFAULT_LLM_PORT}/v1")
    } else {
        cfg.endpoint.trim().to_string()
    }
}

pub fn endpoint_port(cfg: &LocalModelCfg) -> u16 {
    if cfg.endpoint.trim().is_empty() {
        return DEFAULT_LLM_PORT;
    }
    Url::parse(cfg.endpoint.trim())
        .ok()
        .and_then(|u| u.port().or_else(|| u.port_or_known_default()))
        .unwrap_or(DEFAULT_LLM_PORT)
}

fn host_is_local(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(d)) => d == "localhost" || d.ends_with(".local"),
        Some(url::Host::Ipv4(v4)) => {
            let o = v4.octets();
            // 127/8 loopback + RFC1918 private ranges (a volunteer's LAN
            // llama-server/vLLM is still "local", not a third party).
            o[0] == 127 || o[0] == 10 || (o[0] == 172 && (16..=31).contains(&o[1])) || (o[0] == 192 && o[1] == 168)
        }
        Some(url::Host::Ipv6(v6)) => {
            let s = v6.to_string();
            s == "::1" || s.starts_with("fe80") || s.starts_with("fc") || s.starts_with("fd")
        }
        None => false,
    }
}

/// Public form used by settings validation (Auto mode endpoint rules).
pub fn is_loopback_url(url_str: &str) -> bool {
    match Url::parse(url_str.trim()) {
        Ok(u) => host_is_local(&u),
        Err(_) => false,
    }
}

/// Honest `endpoint_kind` for the register declaration: a loopback/LAN
/// server the volunteer already runs is `custom-local`; anything else is a
/// third-party `cloud` endpoint (search snippets transit it).
pub fn custom_endpoint_kind(url_str: &str) -> lh_contributor_client::api::LlmEndpointKind {
    use lh_contributor_client::api::LlmEndpointKind;
    match Url::parse(url_str) {
        Ok(u) if host_is_local(&u) => LlmEndpointKind::CustomLocal,
        _ => LlmEndpointKind::Cloud,
    }
}

// ---------------------------------------------------------------------------
// Health probe (capability is advertised only after a probe succeeds)
// ---------------------------------------------------------------------------

/// Probe an OpenAI-compatible endpoint: llama-server's `/health` at the
/// origin root first, then `{base}/models`. Any 2xx on either means healthy.
pub async fn probe_endpoint(base: &str) -> Result<(), String> {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(4))
        .build()
    {
        Ok(c) => c,
        Err(e) => return Err(format!("probe client: {e}")),
    };
    let base_norm = ensure_v1(base);
    let parsed = match Url::parse(&base_norm) {
        Ok(u) => u,
        Err(e) => return Err(format!("invalid endpoint {base:?}: {e}")),
    };
    let origin_port = parsed
        .port()
        .or_else(|| parsed.port_or_known_default())
        .map(|p| format!(":{p}"))
        .unwrap_or_default();
    let origin = format!(
        "{}://{}{}",
        parsed.scheme(),
        parsed.host_str().unwrap_or("127.0.0.1"),
        origin_port
    );
    let health_url = format!("{origin}/health");
    let models_url = format!("{base_norm}/models");

    // The health-probe failure message, carried into the models-probe
    // error text (why we fell back).
    let health_msg = match client.get(&health_url).send().await {
        Ok(r) if r.status().is_success() => return Ok(()),
        Ok(r) => format!("GET {health_url} → {}", r.status()),
        Err(e) => format!("GET {health_url} → {e}"),
    };
    match client.get(&models_url).send().await {
        Ok(r) if r.status().is_success() => Ok(()),
        Ok(r) => Err(format!("endpoint unreachable ({health_msg}; GET {models_url} → {})", r.status())),
        Err(e) => Err(format!("endpoint unreachable ({health_msg}; GET {models_url} → {e})")),
    }
}

pub async fn wait_healthy(base: &str, budget: Duration) -> Result<(), String> {
    let started = Instant::now();
    let mut last_err = String::new();
    while started.elapsed() < budget {
        match probe_endpoint(base).await {
            Ok(()) => return Ok(()),
            Err(e) => last_err = e,
        }
        tokio::time::sleep(Duration::from_millis(700)).await;
    }
    Err(last_err)
}

// ---------------------------------------------------------------------------
// Launch plan + process lifecycle
// ---------------------------------------------------------------------------

/// Resource caps → llama-server flags.
/// CPU caps map threads per the spec (`max(1, floor(cores/3))`, halves for
/// the ½ cap) and pin everything to CPU (`--n-gpu-layers 0`). The GPU cap
/// offloads every layer (`--n-gpu-layers 999`, llama.cpp's "as much as
/// possible") and leaves threading to llama-server's default.
pub fn cap_threads(cap: ResourceCap, cores: usize) -> Option<usize> {
    match cap {
        ResourceCap::Gpu => None,
        ResourceCap::CpuThird => Some((cores / 3).max(1)),
        ResourceCap::CpuHalf => Some((cores / 2).max(1)),
    }
}

pub fn gpu_offload(cap: ResourceCap) -> bool {
    matches!(cap, ResourceCap::Gpu)
}

pub fn launch_args(cfg: &LocalModelCfg, model: &Path, port: u16) -> Vec<String> {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let mut args = vec![
        "--model".to_string(),
        model.display().to_string(),
        "--host".to_string(),
        "127.0.0.1".to_string(),
        "--port".to_string(),
        port.to_string(),
        // Single slot + small ctx: a grading sidecar, not a chat server.
        "--parallel".to_string(),
        "1".to_string(),
        "--ctx-size".to_string(),
        cfg.ctx_tokens.to_string(),
    ];
    if let Some(t) = cap_threads(cfg.cap, cores) {
        args.extend(["--threads".to_string(), t.to_string()]);
        args.extend(["--n-gpu-layers".to_string(), "0".to_string()]);
    }
    if gpu_offload(cfg.cap) {
        args.extend(["--n-gpu-layers".to_string(), "999".to_string()]);
    }
    args
}

/// Locate the llama-server binary: explicit override first, then the
/// app-installed acquisition (only when its download state vouches for it
/// — same verify-before-use shape as the model file), then PATH and the
/// usual install homes. None means "nothing resolvable" — Auto mode then
/// self-acquires the pinned release asset (see the binary-acquisition
/// section), so the old "install llama.cpp yourself" dead-end is a last
/// resort, not the first failure.
pub fn resolve_binary(override_bin: &str, data_dir: &Path) -> Option<PathBuf> {
    let o = override_bin.trim();
    if !o.is_empty() {
        let p = PathBuf::from(o);
        return if p.is_file() { Some(p) } else { None };
    }
    if let Some(p) = trusted_binary(data_dir) {
        return Some(p);
    }
    path_binary()
}

/// The llama-server executable name for the current OS.
pub fn main_exe_name() -> &'static str {
    if cfg!(windows) {
        "llama-server.exe"
    } else {
        "llama-server"
    }
}

/// PATH + conventional install-home search (no override, no app-installed
/// binary). Split out so llm_view can name the source honestly.
pub fn path_binary() -> Option<PathBuf> {
    let name = main_exe_name();
    let mut candidates: Vec<PathBuf> = std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .collect();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut extra = vec![
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/opt/homebrew/bin"),
    ];
    if let Some(h) = home {
        extra.push(h.join(".local/bin"));
        extra.push(h.join("bin"));
    }
    candidates.extend(extra.iter().map(|d| d.join(name)));
    candidates.into_iter().find(|p| p.is_file())
}

/// Spawn the sidecar child. stdout+stderr are appended to
/// `{data_dir}/llama-server.log` so startup failures are diagnosable from
/// the Settings UI without a console.
pub fn spawn_sidecar(
    cfg: &LocalModelCfg,
    data_dir: &Path,
    spec: &ModelSpec,
) -> Result<Child, String> {
    let bin = resolve_binary(&cfg.llama_server_bin, data_dir).ok_or_else(|| {
        if !cfg.llama_server_bin.trim().is_empty() {
            format!(
                "llama-server override {:#?} is not a file — fix the path or clear it to search PATH",
                cfg.llama_server_bin.trim()
            )
        } else {
            // Auto mode self-acquires, so this only fires when the
            // acquisition is disabled (custom endpoint mode never spawns)
            // or the fetch never completed — never a silent fallback.
            "no llama-server binary on PATH and none installed under bin/ — Start will fetch the pinned llama.cpp release, or point Settings ▸ Local Model ▸ Advanced at your own binary".to_string()
        }
    })?;
    let model = model_path(data_dir, spec);
    if !model.is_file() {
        return Err("model file disappeared — re-download it under Local Model".to_string());
    }
    let log_path = data_dir.join("llama-server.log");
    let log = OpenOptions::new()
        .append(true)
        .create(true)
        .open(&log_path)
        .map_err(|e| format!("could not open {}: {e}", log_path.display()))?;
    let log2 = log
        .try_clone()
        .map_err(|e| format!("log handle: {e}"))?;
    Command::new(&bin)
        .args(launch_args(cfg, &model, endpoint_port(cfg)))
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2))
        .spawn()
        .map_err(|e| format!("could not start {} {:#?}: {e}", bin.display(), launch_args(cfg, &model, endpoint_port(cfg))))
}

fn last_log_lines(log_path: &Path, max: usize) -> String {
    let Ok(text) = fs::read_to_string(log_path) else { return String::new() };
    text.lines().rev().take(max).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")
}

/// Last lines of `{data_dir}/llama-server.log` for surfacing sidecar
/// failures in the Settings UI (a startup failure is diagnosable without a
/// console — that's why the sidecar's stdout/stderr append there).
pub fn log_tail(data_dir: &Path, lines: usize) -> String {
    last_log_lines(&data_dir.join("llama-server.log"), lines)
}

/// Delete the bundled model, its `.part`, and the download state. Returns
/// the byte count that was on disk (0 if nothing was there).
pub fn delete_model(data_dir: &Path, spec: &ModelSpec) -> u64 {
    let freed = on_disk_bytes(data_dir, spec);
    let _ = fs::remove_file(model_path(data_dir, spec));
    let _ = fs::remove_file(part_path(data_dir, spec));
    let _ = fs::remove_file(state_path(data_dir, spec));
    freed
}

/// Graceful stop: SIGTERM llama-server (it flushes and exits clean), wait
/// up to 4 s, then SIGKILL. The child is always reaped — an orphaned
/// llama-server is the failure mode this card exists to prevent.
pub fn stop_child(mut child: Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if started.elapsed() < Duration::from_secs(4) => {
                std::thread::sleep(Duration::from_millis(100));
            }
            _ => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

// ---------------------------------------------------------------------------
// Model download (resumable, sha256-verified, HF resolve endpoint)
// ---------------------------------------------------------------------------

pub struct Progress {
    pub done: u64,
    pub total: u64,
}

/// Download (or resume) the bundled model into the data dir. `on_progress`
/// is called on a throttle; the returned path is only produced after the
/// pinned sha256 matched and the `.part` file was renamed into place.
pub async fn download_model(
    data_dir: &Path,
    spec: &ModelSpec,
    mut on_progress: impl FnMut(&Progress),
) -> Result<PathBuf, String> {
    fs::create_dir_all(models_dir(data_dir))
        .map_err(|e| format!("could not create {}: {e}", models_dir(data_dir).display()))?;
    let final_path = model_path(data_dir, spec);
    let part = part_path(data_dir, spec);

    let mut state = load_state(data_dir, spec).unwrap_or(DownloadState {
        model_id: spec.id.to_string(),
        done_bytes: 0,
        total_bytes: spec.bytes_hint,
        finished: false,
        error: None,
    });
    if state.finished && final_path.is_file() {
        return Ok(final_path);
    }
    // A stale `.part` (size disagrees with the recorded progress) restarts.
    let disk_len = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let mut done = if disk_len == state.done_bytes && disk_len > 0 { state.done_bytes } else { 0 };
    if done == 0 {
        let _ = fs::remove_file(&part);
    }

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| format!("download client: {e}"))?;
    let mut req = client.get(spec.url);
    if done > 0 {
        req = req.header("Range", format!("bytes={done}-"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("could not reach {}/resolve: {e}", spec.repo))?;
    let status = resp.status();
    let resumable = status.as_u16() == 206 && done > 0;
    if !(status.is_success() || resumable) {
        state.error = Some(format!("HTTP {status} from {}", spec.url));
        save_state(data_dir, &state);
        return Err(format!("download failed: HTTP {status} from {}", spec.url));
    }
    if !resumable {
        // Server ignored/punted the Range (or refused resume): start over.
        done = 0;
        let _ = fs::remove_file(&part);
    }
    let total = if resumable {
        done.saturating_add(resp.content_length().unwrap_or(spec.bytes_hint.saturating_sub(done)))
    } else {
        resp.content_length().unwrap_or(spec.bytes_hint)
    };
    state.done_bytes = done;
    state.total_bytes = total;
    state.error = None;
    save_state(data_dir, &state);

    // Hash from the start — on a resumed download, fold the existing prefix
    // in first so the final digest still covers every byte.
    let mut hasher = Sha256::new();
    if done > 0 {
        let mut prefix = fs::File::open(&part).map_err(|e| format!("resume read: {e}"))?;
        let mut buf = vec![0u8; 1024 * 1024];
        let mut left = done;
        while left > 0 {
            let n = buf.len().min(left as usize);
            let read = std::io::Read::read(&mut prefix, &mut buf[..n])
                .map_err(|e| format!("resume read: {e}"))?;
            if read == 0 {
                break;
            }
            hasher.update(&buf[..read]);
            left -= read as u64;
        }
    }

    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .open(&part)
        .map_err(|e| format!("could not open {}: {e}", part.display()))?;
    let mut stream = resp.bytes_stream();
    let mut since_persist = 0u64;
    let mut since_report = 0u64;
    while let Some(chunk) = stream
        .try_next()
        .await
        .map_err(|e| format!("download stream error: {e}"))?
    {
        file.write_all(&chunk).map_err(|e| format!("write error: {e}"))?;
        hasher.update(&chunk);
        done += chunk.len() as u64;
        since_persist += chunk.len() as u64;
        since_report += chunk.len() as u64;
        if since_persist >= 4 * 1024 * 1024 {
            let _ = file.flush();
            state.done_bytes = done;
            state.total_bytes = total;
            save_state(data_dir, &state);
            since_persist = 0;
        }
        // Throttle progress reports to ~1 MB granularity — the callback
        // writes shared UI state and must not fire per 16 KB chunk.
        if since_report >= 1024 * 1024 {
            on_progress(&Progress { done, total });
            since_report = 0;
        }
    }
    on_progress(&Progress { done, total });
    drop(file);

    let got = format!("{:x}", hasher.finalize());
    if !got.eq_ignore_ascii_case(spec.sha256) {
        let _ = fs::remove_file(&part);
        state.done_bytes = 0;
        state.finished = false;
        state.error = Some(format!("sha256 mismatch (got {got})"));
        save_state(data_dir, &state);
        return Err(format!(
            "sha256 mismatch for {}: expected {}, got {got} — file discarded",
            spec.file, spec.sha256
        ));
    }
    fs::rename(&part, &final_path)
        .map_err(|e| format!("could not move model into place: {e}"))?;
    let size = fs::metadata(&final_path).map(|m| m.len()).unwrap_or(done);
    state.done_bytes = size;
    state.total_bytes = size;
    state.finished = true;
    state.error = None;
    save_state(data_dir, &state);
    Ok(final_path)
}

/// Human-readable model line for the UI, e.g.
/// "Qwen2.5 1.5B Instruct · Q4_K_M · 1.1 GB · Apache-2.0".
pub fn model_caption(spec: &ModelSpec) -> String {
    format!(
        "{} · {} · {:.1} GB · {}",
        spec.label,
        spec.quant,
        spec.bytes_hint as f64 / 1e9,
        spec.license
    )
}

// ---------------------------------------------------------------------------
// llama-server binary auto-acquisition (card t_5f4b0943, Option B)
// ---------------------------------------------------------------------------
//
// Auto-configure no longer dead-ends on "install llama.cpp yourself": it
// fetches the llama.cpp release asset for this box from GitHub, verifies it
// against a sha256 over HTTPS BEFORE anything is executed, extracts only
// what the sidecar needs into `{data_dir}/bin/`, and runs `--version` from
// the staging directory before a single byte touches the final location
// (verify-before-use, rename-after-check — the same trust shape as the
// GGUF pin above).
//
// Pin discipline: BIN_PIN below is the default; "Check binary updates"
// re-resolves upstream and offers a switch, but NOTHING auto-switches.
// Run commands stay deterministic.
//
// Pin resolution (2026-11-17): llama.cpp v1 releases publish stable tags
// as marker releases whose only asset is `nightly-tag.txt`, a pointer to
// the nightly build carrying the real binaries. `/releases/latest` today
// is v1.0.0; its pointer names **b12359** (commit b1b770d90, built
// 2026-11-13). All four pinned digests below were re-downloaded and
// sha256-matched against the release API today, and the linux asset was
// extracted + `llama-server --version`-run on this box (flat bin/ layout,
// $ORIGIN rpath — companion libs are REQUIRED next to the binary).

use std::io::BufReader;

use flate2::read::GzDecoder;

/// The release asset name the upstream pointer file carries (v1 scheme).
pub const NIGHTLY_POINTER_FILE: &str = "nightly-tag.txt";

/// GitHub repo API root for the llama.cpp releases.
pub const GGML_RELEASES_API: &str = "https://api.github.com/repos/ggml-org/llama.cpp";

/// UA for every acquisition request (GitHub requires a UA on the API).
const ACQUISITION_UA: &str = "lh-contributor-gui/0.1.0 llama-server-acquisition-t5f4b0943";

/// Build flavor the selection matrix offers. Vulkan only when the
/// hardware probe measured a dedicated GPU; CPU (and the macOS single
/// asset) is the safe default. Unknown/absent probes pick CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinBackend {
    Cpu,
    Vulkan,
}

/// One pinned release asset. Every field was verified against the live
/// llama.cpp releases API on 2026-11-17 (asset names, exact sizes, and the
/// per-asset `digest` field — downloaded + hashed to confirm each one).
#[derive(Debug, Clone)]
pub struct BinAssetSpec {
    pub tag: &'static str,
    pub asset: &'static str,
    /// Release download URL (always HTTPS).
    pub url: &'static str,
    pub sha256: &'static str,
    pub bytes_hint: u64,
    pub backend_label: &'static str,
}

/// The pinned llama.cpp tag (Option B default). Never rolling.
pub const BIN_PIN_TAG: &str = "b12359";

const PIN_LINUX_CPU: BinAssetSpec = BinAssetSpec {
    tag: BIN_PIN_TAG,
    asset: "llama-b12359-bin-ubuntu-x64.tar.gz",
    url: "https://github.com/ggml-org/llama.cpp/releases/download/b12359/llama-b12359-bin-ubuntu-x64.tar.gz",
    sha256: "0bc1f091e8916102b6d8f2cf944233a620e90a0f856c72d0ce41150c65c2e72e",
    bytes_hint: 23_311_084,
    backend_label: "cpu",
};

const PIN_LINUX_VULKAN: BinAssetSpec = BinAssetSpec {
    tag: BIN_PIN_TAG,
    asset: "llama-b12359-bin-ubuntu-vulkan-x64.tar.gz",
    url: "https://github.com/ggml-org/llama.cpp/releases/download/b12359/llama-b12359-bin-ubuntu-vulkan-x64.tar.gz",
    sha256: "19b60f00395a3f4954e46a5037695bc1d843404fc9d3124f82a612ecd8fbe9f0",
    bytes_hint: 36_153_377,
    backend_label: "vulkan",
};

const PIN_WIN_CPU: BinAssetSpec = BinAssetSpec {
    tag: BIN_PIN_TAG,
    asset: "llama-b12359-bin-win-cpu-x64.zip",
    url: "https://github.com/ggml-org/llama.cpp/releases/download/b12359/llama-b12359-bin-win-cpu-x64.zip",
    sha256: "837c1655e87df3b66989f4be33a792a1399c785bf259038dc75c900096683e69",
    bytes_hint: 25_097_509,
    backend_label: "cpu",
};

const PIN_WIN_VULKAN: BinAssetSpec = BinAssetSpec {
    tag: BIN_PIN_TAG,
    asset: "llama-b12359-bin-win-vulkan-x64.zip",
    url: "https://github.com/ggml-org/llama.cpp/releases/download/b12359/llama-b12359-bin-win-vulkan-x64.zip",
    sha256: "e8d18c6e4210cca87e7307c2ea78363ac8d158e4e1397358ec96ab70b53a0d5e",
    bytes_hint: 38_090_643,
    backend_label: "vulkan",
};

/// The macOS arm64 asset is ONE build carrying Metal + BLAS — the matrix
/// has no separate Vulkan variant for macOS.
const PIN_MACOS_ARM64: BinAssetSpec = BinAssetSpec {
    tag: BIN_PIN_TAG,
    asset: "llama-b12359-bin-macos-arm64.tar.gz",
    url: "https://github.com/ggml-org/llama.cpp/releases/download/b12359/llama-b12359-bin-macos-arm64.tar.gz",
    sha256: "82b00abc740160e1b9980e6cc79c117345fa34af058c80e6c0a69aa8ddccde75",
    bytes_hint: 16_907_811,
    backend_label: "native",
};

/// True when this compile target is on the acquisition matrix
/// (linux-x64 / windows-x64 / macos-arm64). Everything else must be told
/// plainly, not guessed at.
pub fn host_on_matrix() -> bool {
    (cfg!(any(target_os = "linux", target_os = "windows")) && cfg!(target_arch = "x86_64"))
        || (cfg!(target_os = "macos") && cfg!(target_arch = "aarch64"))
}

fn host_desc() -> String {
    let os = if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "this OS"
    };
    format!("{os}/{}", std::env::consts::ARCH)
}

fn unsupported_matrix_error() -> String {
    format!(
        "Auto-configure cannot auto-install a llama-server build for {} — the pinned matrix covers linux-x64, windows-x64 and macos-arm64 only. Install llama.cpp yourself, point Settings ▸ Local Model ▸ Advanced at your binary, or switch the Local Model mode to Custom endpoint",
        host_desc()
    )
}

/// Deterministic pick from the pin table: OS/arch from the compile target,
/// backend from the caller (the hardware advisor's GPU knowledge flows in
/// via contributor). macOS arm64 has one asset; backend never changes it.
pub fn select_pin_spec(backend: BinBackend) -> Result<&'static BinAssetSpec, String> {
    let vulkan = backend == BinBackend::Vulkan;
    if cfg!(target_os = "linux") && cfg!(target_arch = "x86_64") {
        return Ok(if vulkan { &PIN_LINUX_VULKAN } else { &PIN_LINUX_CPU });
    }
    if cfg!(windows) && cfg!(target_arch = "x86_64") {
        return Ok(if vulkan { &PIN_WIN_VULKAN } else { &PIN_WIN_CPU });
    }
    if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
        return Ok(&PIN_MACOS_ARM64);
    }
    Err(unsupported_matrix_error())
}

/// The exact asset name llama.cpp uses for this box + backend at a given
/// tag. The release-JSON resolver requires an EXACT name match against
/// this string — renamed assets fail loudly, never fuzzy-guessed.
pub fn host_asset_name(tag: &str, backend: BinBackend) -> Result<String, String> {
    let vulkan = backend == BinBackend::Vulkan;
    let suffix: &str = if cfg!(target_os = "linux") && cfg!(target_arch = "x86_64") {
        if vulkan { "bin-ubuntu-vulkan-x64.tar.gz" } else { "bin-ubuntu-x64.tar.gz" }
    } else if cfg!(windows) && cfg!(target_arch = "x86_64") {
        if vulkan { "bin-win-vulkan-x64.zip" } else { "bin-win-cpu-x64.zip" }
    } else if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
        "bin-macos-arm64.tar.gz"
    } else {
        return Err(unsupported_matrix_error());
    };
    Ok(format!("llama-{tag}-{suffix}"))
}

/// The exe name inside the archive — archives are platform-shaped
/// (llama.cpp ships .zip for Windows, .tar.gz for unix).
fn main_name_for_archive(asset: &str) -> &'static str {
    if asset.ends_with(".zip") {
        "llama-server.exe"
    } else {
        "llama-server"
    }
}

/// A concrete, digest-pinned binary to install — either straight from the
/// pin table or freshly resolved from the releases API (update path).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedBinary {
    pub tag: String,
    pub asset: String,
    pub url: String,
    pub sha256: String,
    pub bytes_hint: u64,
    pub backend: &'static str,
}

impl ResolvedBinary {
    pub fn from_pin(spec: &BinAssetSpec) -> ResolvedBinary {
        ResolvedBinary {
            tag: spec.tag.to_string(),
            asset: spec.asset.to_string(),
            url: spec.url.to_string(),
            sha256: spec.sha256.to_string(),
            bytes_hint: spec.bytes_hint,
            backend: spec.backend_label,
        }
    }
}

/// "b12359" → 12359. Non-conforming tags return None — update decisions
/// only ever compare conforming llama.cpp tags.
pub fn build_number(tag: &str) -> Option<u64> {
    let rest = tag.strip_prefix('b')?;
    if rest.is_empty() || rest.len() > 10 || !rest.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    rest.parse::<u64>().ok()
}

// ---- releases API shapes -------------------------------------------------

#[derive(Debug, Deserialize)]
struct ReleaseInfo {
    tag_name: String,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
    #[serde(default)]
    size: u64,
    browser_download_url: String,
    /// GitHub computes this for release assets (sha256). A missing field
    /// means the asset was never hashed server-side → refuse, never install
    /// an unverified artifact.
    #[serde(default)]
    digest: Option<String>,
}

fn api_digest_hex(digest: Option<&str>, asset: &str) -> Result<String, String> {
    let raw = digest.ok_or_else(|| {
        format!("GitHub API carries no sha256 digest for {asset} — refusing to install it unverified")
    })?;
    let hex = raw.strip_prefix("sha256:").ok_or_else(|| {
        format!("unexpected digest form {raw:?} for {asset} — refusing")
    })?;
    if hex.len() != 64 || !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("malformed sha256 digest for {asset} — refusing"));
    }
    Ok(hex.to_lowercase())
}

/// Asset download URLs must be llama.cpp's own HTTPS host. Loopback HTTP
/// is accepted ONLY for the test fixture server (127.0.0.1 / localhost).
fn ok_download_url(url: &str) -> Result<String, String> {
    match Url::parse(url) {
        Ok(u) if u.scheme() == "https" => Ok(url.to_string()),
        Ok(u)
            if u.scheme() == "http"
                && matches!(u.host_str(), Some("127.0.0.1") | Some("localhost")) =>
        {
            Ok(url.to_string())
        }
        _ => Err(format!("refusing non-HTTPS release asset URL {url:?}")),
    }
}

async fn api_get_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
) -> Result<T, String> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("could not reach {url}: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("GitHub API said {status} for {url} — try again later (rate limits are anonymous here)"));
    }
    let body = resp.text().await.map_err(|e| format!("reading {url}: {e}"))?;
    serde_json::from_str::<T>(&body).map_err(|e| {
        format!("GitHub API returned JSON this app cannot parse ({e}) — the llama.cpp release layout may have changed")
    })
}

/// Fetch a small asset (the nightly pointer) with its API digest verified
/// BEFORE the content is trusted. Capped at 4 KiB — it is a build tag file.
async fn fetch_pointer_text(
    client: &reqwest::Client,
    asset: &ReleaseAsset,
) -> Result<String, String> {
    let want = api_digest_hex(asset.digest.as_deref(), &asset.name)?;
    let url = ok_download_url(&asset.browser_download_url)?;
    if asset.size > 4096 {
        return Err(format!("pointer asset {url} claims {} bytes — far beyond the 7-byte build-tag file expected", asset.size));
    }
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("could not fetch {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {} fetching {url}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| format!("reading {url}: {e}"))?;
    if bytes.len() > 4096 {
        return Err(format!("{url} streamed {}/4 KiB allowed — refusing", bytes.len()));
    }
    let got = format!("{:x}", Sha256::digest(&bytes));
    if !got.eq_ignore_ascii_case(&want) {
        return Err(format!(
            "sha256 mismatch for {}: expected {}, got {got} — pointer discarded",
            asset.name, want
        ));
    }
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

/// Parse the pointer file content: a strict llama.cpp build tag (`b` + digits).
fn parse_pointer_tag(text: &str) -> Result<String, String> {
    let t = text.trim();
    build_number(t)
        .map(|_| t.to_string())
        .ok_or_else(|| format!("nightly-tag.txt holds {t:?} — not a llama.cpp build tag; refusing to guess"))
}

/// Resolve the newest llama.cpp binary release this app would install:
/// `/releases/latest` first (its own assets if they match the matrix —
/// old-style releases), otherwise follow the v1 marker's `nightly-tag.txt`
/// pointer to the release that actually carries binaries. The returned
/// digests are GitHub's own (HTTPS-trusted) — the update path only.
pub async fn resolve_latest_binary(
    api_base: &str,
    backend: BinBackend,
) -> Result<ResolvedBinary, String> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(20))
        .user_agent(ACQUISITION_UA)
        .build()
        .map_err(|e| format!("API client: {e}"))?;
    let base = api_base.trim_end_matches('/');

    let latest: ReleaseInfo = api_get_json(&client, &format!("{base}/releases/latest")).await?;
    let expected = host_asset_name(&latest.tag_name, backend)?;

    if let Some(asset) = latest.assets.iter().find(|a| a.name == expected) {
        return resolved_from_asset(asset, &latest.tag_name, backend);
    }

    // v1 scheme: the stable release is a marker; binaries live in the
    // nightly it points at. Verify the pointer, then fetch that release.
    let pointer = latest.assets.iter().find(|a| a.name == NIGHTLY_POINTER_FILE).ok_or_else(|| {
        format!(
            "release {} carries neither the matrix asset {expected:?} nor {NIGHTLY_POINTER_FILE} — the llama.cpp release layout changed; refusing to guess an asset name",
            latest.tag_name
        )
    })?;
    let nightly_tag = parse_pointer_tag(&fetch_pointer_text(&client, pointer).await?)?;
    let nightly: ReleaseInfo = api_get_json(&client, &format!("{base}/releases/tags/{nightly_tag}")).await?;
    let expected = host_asset_name(&nightly.tag_name, backend)?;
    let asset = nightly.assets.iter().find(|a| a.name == expected).ok_or_else(|| {
        format!(
            "nightly release {} has no {expected:?} asset — refusing to fuzzy-match asset names",
            nightly.tag_name
        )
    })?;
    resolved_from_asset(asset, &nightly.tag_name, backend)
}

fn resolved_from_asset(
    asset: &ReleaseAsset,
    tag: &str,
    backend: BinBackend,
) -> Result<ResolvedBinary, String> {
    let sha256 = api_digest_hex(asset.digest.as_deref(), &asset.name)?;
    let url = ok_download_url(&asset.browser_download_url)?;
    let backend_label = select_pin_spec(backend)?.backend_label;
    Ok(ResolvedBinary {
        tag: tag.to_string(),
        asset: asset.name.clone(),
        url,
        sha256,
        bytes_hint: asset.size,
        backend: backend_label,
    })
}

// ---- on-disk bookkeeping --------------------------------------------------

fn bin_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("bin")
}

/// Final location of the app-installed llama-server executable.
pub fn main_exe_path(data_dir: &Path) -> PathBuf {
    bin_dir(data_dir).join(main_exe_name())
}

fn bin_part_path(data_dir: &Path, asset: &str) -> PathBuf {
    bin_dir(data_dir).join(format!("{asset}.part"))
}

fn bin_state_path(data_dir: &Path) -> PathBuf {
    bin_dir(data_dir).join("download_state.json")
}

/// Download/install bookkeeping, mirroring `DownloadState`: `finished` is
/// the trust flag — the final bin/ layout only exists under it (every
/// promotion happens after the archive hash AND the `--version` self-check
/// both passed), so a file under bin/ always passed verification.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BinDownloadState {
    pub tag: String,
    pub asset: String,
    /// "cpu" | "vulkan" | "native" as installed for this box.
    pub backend: String,
    pub sha256: String,
    pub total_bytes: u64,
    pub done_bytes: u64,
    #[serde(default)]
    pub finished: bool,
    #[serde(default)]
    pub error: Option<String>,
    /// `llama-server --version` first line, captured at install.
    #[serde(default)]
    pub version: Option<String>,
}

pub fn load_bin_state(data_dir: &Path) -> Option<BinDownloadState> {
    fs::read_to_string(bin_state_path(data_dir))
        .ok()
        .and_then(|t| serde_json::from_str::<BinDownloadState>(&t).ok())
}

fn save_bin_state(data_dir: &Path, state: &BinDownloadState) {
    let _ = fs::create_dir_all(bin_dir(data_dir));
    if let Ok(json) = serde_json::to_string_pretty(state) {
        let _ = fs::write(bin_state_path(data_dir), json);
    }
}

/// Persist a FAILED attempt's bookkeeping — unless a different tag/asset
/// was already verified and its files are still on disk untouched, in which
/// case the old trust record is restored instead (a failed update never
/// untrusts a working binary; the failure lives in the returned error and
/// the lane instead).
fn save_bin_failure(
    data_dir: &Path,
    state: &BinDownloadState,
    prior: Option<&BinDownloadState>,
    restore_prior: bool,
) {
    if restore_prior {
        if let Some(p) = prior {
            save_bin_state(data_dir, p);
            return;
        }
    }
    save_bin_state(data_dir, state);
}

/// The app-installed binary is only trusted when a finished bin download
/// state vouches for it — identical verify-before-use shape to
/// `model_is_ready`. Returns None (so PATH/acquisition take over) unless
/// the exe is present AND verified.
pub fn trusted_binary(data_dir: &Path) -> Option<PathBuf> {
    let exe = main_exe_path(data_dir);
    if !exe.is_file() {
        return None;
    }
    let state = load_bin_state(data_dir)?;
    (state.finished && !state.asset.is_empty()).then_some(exe)
}

/// Extract-only filter: the sidecar needs `llama-server` and its companion
/// shared libs (linux `.so` families incl. CPU-ISA variants, macOS
/// `.dylib` families, Windows DLLs — llama-server dlopens these from its
/// own directory). Every other tool in the archive stays out.
fn should_extract_leaf(name: &str, main_name: &str) -> bool {
    name == main_name || name.contains(".so") || name.ends_with(".dylib") || name.ends_with(".dll")
}

fn extract_archive(part: &Path, staging: &Path, asset: &str) -> Result<(), String> {
    if asset.ends_with(".tar.gz") {
        let file = fs::File::open(part).map_err(|e| format!("opening {}: {e}", part.display()))?;
        tar::Archive::new(GzDecoder::new(file))
            // unpack() joins entry paths under staging and rejects
            // traversal/absolute paths internally.
            .unpack(staging)
            .map_err(|e| format!("extracting tar.gz {}: {e}", part.display()))
    } else if asset.ends_with(".zip") {
        let file = fs::File::open(part).map_err(|e| format!("opening {}: {e}", part.display()))?;
        let mut archive = zip::ZipArchive::new(BufReader::new(file))
            .map_err(|e| format!("not a readable zip {}: {e}", part.display()))?;
        let main_name = main_name_for_archive(asset);
        let mut written = 0usize;
        for i in 0..archive.len() {
            let mut entry = archive
                .by_index(i)
                .map_err(|e| format!("zip index {i}: {e}"))?;
            // Windows zips are flat; flatten anyway (and enclosed_name()
            // refuses traversal entries).
            let Some(rel) = entry.enclosed_name() else { continue };
            let Some(leaf) = rel.file_name().map(|s| s.to_string_lossy().to_string()) else { continue };
            if entry.is_dir() || !should_extract_leaf(&leaf, main_name) {
                continue;
            }
            if entry.is_symlink() {
                // llama.cpp release zips carry none; anything else is not
                // something this app should create.
                continue;
            }
            let dest = staging.join(&leaf);
            let mut out = fs::File::create(&dest)
                .map_err(|e| format!("creating {}: {e}", dest.display()))?;
            std::io::copy(&mut entry, &mut out)
                .map_err(|e| format!("extracting {leaf}: {e}"))?;
            written += 1;
        }
        if written == 0 {
            return Err(format!("zip {} holds nothing the matrix asked for", part.display()));
        }
        Ok(())
    } else {
        Err(format!("unknown archive type for asset {asset} — refusing"))
    }
}

/// Walk the staging tree collecting exactly the files the matrix wants.
fn collect_promotables(staging: &Path, main_name: &str) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    let mut stack = vec![staging.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir).map_err(|e| format!("scanning {}: {e}", dir.display()))?;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let path = entry.path();
            let ftype = entry
                .file_type()
                .map_err(|e| format!("stat {}: {e}", path.display()))?;
            if ftype.is_dir() {
                stack.push(path);
            } else if should_extract_leaf(&name, main_name) {
                out.push(path);
            }
        }
    }
    Ok(out)
}

fn remove_if_present(path: &Path) {
    // Plain remove_file clears regular files AND symlinks (never follows).
    let _ = fs::remove_file(path);
}

fn promote_all(_staging: &Path, candidates: &[PathBuf], bin: &Path, main_name: &str) -> Result<PathBuf, String> {
    for src in candidates {
        let leaf = src
            .file_name()
            .ok_or_else(|| format!("no file name under {}", src.display()))?
            .to_string_lossy()
            .to_string();
        let dest = bin.join(&leaf);
        remove_if_present(&dest);
        if fs::symlink_metadata(src)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            // Re-link by BASENAME into bin/: the real archives link
            // libX.so → libX.so.1 within their own directory, so a
            // basename-only link stays valid after flattening. A link
            // pointing outside the archive is skipped, never followed.
            let target = fs::read_link(src).map_err(|e| format!("read_link {}: {e}", src.display()))?;
            let Some(target_leaf) = target.file_name().map(|s| s.to_string_lossy().to_string()) else {
                continue;
            };
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target_leaf, &dest)
                .map_err(|e| format!("symlink {leaf} → {target_leaf}: {e}"))?;
            #[cfg(not(unix))]
            {
                let _ = &target_leaf; // no unprivileged symlink creation on Windows; llama.cpp zips need none
            }
        } else {
            fs::rename(src, &dest).map_err(|e| format!("moving {leaf} into place: {e}"))?;
        }
    }
    let main = bin.join(main_name);
    if !main.is_file() {
        return Err(format!("archive layout changed: no {main_name} was promoted — refusing to run anything"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&main, fs::Permissions::from_mode(0o755));
    }
    Ok(main)
}

/// `llama-server --version` → first meaningful line, e.g.
/// "version: 1.0.0-dev (build 12359, commit b1b770d90)". None on ANY
/// failure: spawn error, empty output, or non-zero exit — a binary that
/// fails its own `--version` has no version to report, and acquisition
/// refuses to install it on that signal.
pub fn capture_version(path: &Path) -> Option<String> {
    let output = Command::new(path).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let mut first_any: Option<String> = None;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let s = t.chars().take(160).collect::<String>();
        if t.contains("version") {
            return Some(s);
        }
        if first_any.is_none() {
            first_any = Some(s);
        }
    }
    first_any
}

/// Download (or resume) a resolved llama-server archive, verify it against
/// its sha256, extract the matrix-relevant files, `--version`-check them
/// in staging, and only then promote them into `{data_dir}/bin/`.
/// Returns the captured version line. Nothing in bin/ changes until the
/// archive hash and the self-check have BOTH passed — a failed run leaves
/// any previously installed binary untouched.
pub async fn download_binary(
    data_dir: &Path,
    resolved: &ResolvedBinary,
    mut on_progress: impl FnMut(&Progress),
) -> Result<String, String> {
    let bin = bin_dir(data_dir);
    fs::create_dir_all(&bin).map_err(|e| format!("could not create {}: {e}", bin.display()))?;
    let part = bin_part_path(data_dir, &resolved.asset);
    let main_name = main_name_for_archive(&resolved.asset);

    let mut state = load_bin_state(data_dir).unwrap_or_default();
    // Snapshot the prior verified install: if this NEW attempt fails, the
    // old finished record (and the untouched old files it vouches for)
    // must be restored — a failed *update* never untrusts a working binary.
    let prior = load_bin_state(data_dir);
    let prior_finished_for_different_asset = prior.as_ref().is_some_and(|p| {
        p.finished && (p.tag != resolved.tag || p.asset != resolved.asset)
    });
    let resume_valid = state.tag == resolved.tag
        && state.asset == resolved.asset
        && !state.sha256.is_empty()
        && state.sha256.eq_ignore_ascii_case(&resolved.sha256);
    if !resume_valid {
        state = BinDownloadState {
            tag: resolved.tag.clone(),
            asset: resolved.asset.clone(),
            backend: resolved.backend.to_string(),
            sha256: resolved.sha256.clone(),
            ..Default::default()
        };
    }
    let disk_len = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let mut done = if disk_len == state.done_bytes && disk_len > 0 {
        state.done_bytes
    } else {
        0
    };
    if done == 0 {
        let _ = fs::remove_file(&part);
        // Tidy: stale .part files from other tags/assets and half-used
        // staging dirs belong to dead attempts, not to this one.
        if let Ok(entries) = fs::read_dir(&bin) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".part") && name != format!("{}.part", resolved.asset) {
                    let _ = fs::remove_file(entry.path());
                } else if name.starts_with(".staging") {
                    let _ = fs::remove_dir_all(entry.path());
                }
            }
        }
    }

    // A fully downloaded, fully hashed-against archive part needs no network
    // at all — re-verify from disk and go straight to extraction. (This is
    // also what a digest-mismatch retry after a partial fetch avoids:
    // `done` was reset then, so this branch only hits complete parts.)
    let part_complete = done > 0 && state.total_bytes > 0 && done == state.total_bytes && disk_len == done;

    if !part_complete {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .user_agent(ACQUISITION_UA)
            .build()
            .map_err(|e| format!("download client: {e}"))?;
        let mut req = client.get(&resolved.url);
        if done > 0 {
            req = req.header("Range", format!("bytes={done}-"));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("could not reach {}: {e}", resolved.url))?;
        let status = resp.status();
        let resumable = status.as_u16() == 206 && done > 0;
        if !(status.is_success() || resumable) {
            state.error = Some(format!("HTTP {status} from {}", resolved.url));
            save_bin_failure(data_dir, &state, prior.as_ref(), prior_finished_for_different_asset);
            return Err(format!("download failed: HTTP {status} from {}", resolved.url));
        }
        if !resumable {
            done = 0;
            let _ = fs::remove_file(&part);
        }
        let total = if resumable {
            done.saturating_add(
                resp.content_length()
                    .unwrap_or(resolved.bytes_hint.saturating_sub(done)),
            )
        } else {
            resp.content_length().unwrap_or(resolved.bytes_hint)
        };
        state.done_bytes = done;
        state.total_bytes = total;
        state.error = None;
        save_bin_state(data_dir, &state);

        // Hash from the start — fold a resumed prefix in first (same as
        // the model downloader) so the final digest covers every byte.
        let mut hasher = Sha256::new();
        if done > 0 {
            let mut prefix = fs::File::open(&part).map_err(|e| format!("resume read: {e}"))?;
            let mut buf = vec![0u8; 1024 * 1024];
            let mut left = done;
            while left > 0 {
                let n = buf.len().min(left as usize);
                let read = std::io::Read::read(&mut prefix, &mut buf[..n])
                    .map_err(|e| format!("resume read: {e}"))?;
                if read == 0 {
                    break;
                }
                hasher.update(&buf[..read]);
                left -= read as u64;
            }
        }

        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&part)
            .map_err(|e| format!("could not open {}: {e}", part.display()))?;
        let mut stream = resp.bytes_stream();
        let mut since_persist = 0u64;
        let mut since_report = 0u64;
        while let Some(chunk) = stream
            .try_next()
            .await
            .map_err(|e| format!("download stream error: {e}"))?
        {
            file.write_all(&chunk).map_err(|e| format!("write error: {e}"))?;
            hasher.update(&chunk);
            done += chunk.len() as u64;
            since_persist += chunk.len() as u64;
            since_report += chunk.len() as u64;
            if since_persist >= 1024 * 1024 {
                let _ = file.flush();
                state.done_bytes = done;
                state.total_bytes = total;
                save_bin_state(data_dir, &state);
                since_persist = 0;
            }
            if since_report >= 256 * 1024 {
                on_progress(&Progress { done, total });
                since_report = 0;
            }
        }
        on_progress(&Progress { done, total });
        drop(file);

        let got = format!("{:x}", hasher.finalize());
        if !got.eq_ignore_ascii_case(&resolved.sha256) {
            let _ = fs::remove_file(&part);
            state.done_bytes = 0;
            state.finished = false;
            state.error = Some(format!("sha256 mismatch (got {got})"));
            save_bin_failure(data_dir, &state, prior.as_ref(), prior_finished_for_different_asset);
            return Err(format!(
                "sha256 mismatch for llama.cpp asset {}: expected {}, got {got} — archive discarded",
                resolved.asset, resolved.sha256
            ));
        }
        state.done_bytes = done;
        state.total_bytes = total;
    } else {
        let mut hasher = Sha256::new();
        let data = fs::read(&part).map_err(|e| format!("re-reading {}: {e}", part.display()))?;
        hasher.update(&data);
        let got = format!("{:x}", hasher.finalize());
        if !got.eq_ignore_ascii_case(&resolved.sha256) {
            let _ = fs::remove_file(&part);
            state.done_bytes = 0;
            state.finished = false;
            state.error = Some(format!("sha256 mismatch (got {got})"));
            save_bin_failure(data_dir, &state, prior.as_ref(), prior_finished_for_different_asset);
            return Err(format!(
                "sha256 mismatch for llama.cpp asset {}: expected {}, got {got} — archive discarded",
                resolved.asset, resolved.sha256
            ));
        }
        on_progress(&Progress { done, total: done });
    }

    // Verify-before-use, phase 2: extract into staging, --version the
    // staged binary (its $ORIGIN rpath resolves the staged companions),
    // and only then promote everything into bin/.
    let staging = bin.join(format!(".staging-{}", resolved.tag));
    fs::create_dir_all(&staging).map_err(|e| format!("staging dir: {e}"))?;
    let outcome = (|| -> Result<String, String> {
        extract_archive(&part, &staging, &resolved.asset)?;
        let candidates = collect_promotables(&staging, main_name)?;
        let staged_main = candidates
            .iter()
            .find(|p| p.file_name().map(|s| s.to_string_lossy().to_string()) == Some(main_name.to_string()))
            .ok_or_else(|| format!("archive layout changed: {main_name} is missing from {}", resolved.asset))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(staged_main, fs::Permissions::from_mode(0o755));
        }
        let version = capture_version(staged_main).ok_or_else(|| {
            format!(
                "the downloaded {} failed its self-check (`--version`) on this machine — refusing to install it; Settings ▸ Local Model ▸ Advanced can point at a llama-server you install yourself",
                main_name
            )
        })?;
        promote_all(&staging, &candidates, &bin, main_name)?;
        Ok(version)
    })();
    let _ = fs::remove_dir_all(&staging);
    match outcome {
        Ok(version) => {
            let _ = fs::remove_file(&part);
            state.finished = true;
            state.done_bytes = state.total_bytes;
            state.error = None;
            state.version = Some(version.clone());
            save_bin_state(data_dir, &state);
            Ok(version)
        }
        Err(e) => {
            // Extraction/self-check failure: staging was discarded and
            // bin/ was never touched — if a different tag/asset was
            // already verified, its trust record is restored intact.
            // The .part is dropped too: the restored (or error) state
            // won't resume from it, and refusing to run this artifact
            // shouldn't leave dead weight beside a working install.
            let _ = fs::remove_file(&part);
            state.error = Some(e.clone());
            save_bin_failure(data_dir, &state, prior.as_ref(), prior_finished_for_different_asset);
            Err(e)
        }
    }
}

/// Remove the app-installed binary set (bin/ + its download state),
/// keeping the volunteer's own PATH/override binary untouched.
pub fn delete_binary(data_dir: &Path) -> u64 {
    let dir = bin_dir(data_dir);
    let mut freed = 0u64;
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    freed += meta.len();
                }
            }
            let _ = fs::remove_file(entry.path());
        }
    }
    let _ = fs::remove_dir(&dir);
    freed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{LlmMode, LocalModelCfg};
    use std::io::{Read, Write as IoWrite};
    use std::net::TcpListener;

    fn cfg_base() -> LocalModelCfg {
        LocalModelCfg::default()
    }

    #[test]
    fn endpoint_helpers() {
        assert_eq!(ensure_v1("http://127.0.0.1:8081/"), "http://127.0.0.1:8081/v1");
        assert_eq!(ensure_v1("http://127.0.0.1:8081"), "http://127.0.0.1:8081/v1");
        assert_eq!(ensure_v1("http://127.0.0.1:8081/v1"), "http://127.0.0.1:8081/v1");
        assert_eq!(ensure_v1("http://192.168.1.5:1234/foo"), "http://192.168.1.5:1234/foo");
        let mut c = cfg_base();
        assert_eq!(resolved_endpoint(&c), "http://127.0.0.1:8081/v1");
        assert_eq!(endpoint_port(&c), 8081);
        c.endpoint = "http://127.0.0.1:9090/v1".to_string();
        assert_eq!(endpoint_port(&c), 9090);
    }

    #[test]
    fn endpoint_kind_honesty_loopback_lan_cloud() {
        use lh_contributor_client::api::LlmEndpointKind;
        assert_eq!(
            custom_endpoint_kind("http://127.0.0.1:8081/v1"),
            LlmEndpointKind::CustomLocal
        );
        assert_eq!(
            custom_endpoint_kind("http://localhost:1234/v1"),
            LlmEndpointKind::CustomLocal
        );
        assert_eq!(
            custom_endpoint_kind("http://192.168.4.20:8080/v1"),
            LlmEndpointKind::CustomLocal,
            "a volunteer's LAN server is still 'local'"
        );
        assert_eq!(
            custom_endpoint_kind("http://10.0.0.5:8080/v1"),
            LlmEndpointKind::CustomLocal
        );
        assert_eq!(
            custom_endpoint_kind("https://api.example-provider.com/v1"),
            LlmEndpointKind::Cloud
        );
    }

    #[test]
    fn resource_caps_map_to_llama_server_flags() {
        let cores = std::thread::available_parallelism().unwrap().get();
        let mut c = cfg_base();

        c.cap = ResourceCap::CpuThird;
        assert_eq!(cap_threads(c.cap, cores), Some((cores / 3).max(1)));
        let args = launch_args(&c, Path::new("/models/x.gguf"), 8081);
        assert!(args.windows(2).any(|w| w == ["--threads", &((cores / 3).max(1)).to_string()]));
        assert!(args.windows(2).any(|w| w == ["--n-gpu-layers", "0"]));
        assert!(args.windows(2).any(|w| w == ["--parallel", "1"]));
        assert!(args.windows(2).any(|w| w == ["--host", "127.0.0.1"]));
        assert!(args.windows(2).any(|w| w == ["--port", "8081"]));
        assert!(!gpu_offload(c.cap));

        c.cap = ResourceCap::CpuHalf;
        assert_eq!(cap_threads(c.cap, 5), Some(2));
        assert_eq!(cap_threads(c.cap, 1), Some(1), "max(1, floor(cores/2))");
        let args2 = launch_args(&c, Path::new("/m"), 8081);
        // cap_threads(CpuHalf, 5) = max(1, floor(5/2)) = 2 — the max() arm
        // is a no-op for this fixture, so the expected flag is the literal.
        assert!(args2.windows(2).any(|w| w == ["--threads", "2"]));

        c.cap = ResourceCap::Gpu;
        assert_eq!(cap_threads(c.cap, cores), None);
        let args3 = launch_args(&c, Path::new("/m"), 8081);
        assert!(args3.windows(2).any(|w| w == ["--n-gpu-layers", "999"]), "GPU mode offloads all layers");
        assert!(!args3.windows(2).any(|w| w[0] == "--threads"), "GPU mode leaves threading to llama-server");
    }

    /// Tiny single-request HTTP stand-in (like the contributor FakeCoord):
    /// answers the exact paths the probe tries.
    fn spawn_probe_server(health_status: u16, models_status: u16) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind probe server");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 1024];
                let n = stream.read(&mut buf).unwrap_or(0);
                let first_line = String::from_utf8_lossy(&buf[..n]).lines().next().unwrap_or("").to_string();
                let (status, body) = if first_line.starts_with("GET /health") {
                    (health_status, r#"{"ok":true}"#.to_string())
                } else if first_line.starts_with("GET /v1/models") {
                    (models_status, r#"{"data":[]}"#.to_string())
                } else {
                    (404, r#"{"error":"no"}"#.to_string())
                };
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://127.0.0.1:{port}/v1")
    }

    #[tokio::test]
    async fn probe_prefers_health_then_falls_back_to_models() {
        let base = spawn_probe_server(200, 404);
        probe_endpoint(&base).await.expect("health 200 is healthy");
        let base2 = spawn_probe_server(404, 200);
        probe_endpoint(&base2).await.expect("models 200 fallback is healthy");
        let base3 = spawn_probe_server(503, 503);
        let err = probe_endpoint(&base3).await.unwrap_err();
        assert!(err.contains("unreachable"), "{err}");
    }

    #[test]
    fn stop_child_kills_and_reaps() {
        // `sleep 30` stands in for a sidecar process: SIGTERM path (sleep
        // dies on TERM) must reap it without reaching SIGKILL escalation.
        let child = Command::new("sleep").arg("30").spawn().expect("spawn stand-in");
        let pid = child.id();
        stop_child(child);
        // The pid must be gone (reaped or reused by the OS — kill -0 on the
        // same pid with the child already reaped can't hit OUR sleep).
        let alive = unsafe { libc::kill(pid as i32, 0) == 0 };
        assert!(!alive, "sidecar pid {pid} survived stop_child — orphan leak");
    }

    #[test]
    fn download_state_and_model_ready_flag_the_verified_file() {
        let dir = std::env::temp_dir().join("lhc-sidecar-state-test");
        let _ = fs::remove_dir_all(&dir);
        assert!(!model_is_ready(&dir, &DEFAULT_MODEL));
        // A file named like the model but not backed by a finished download
        // state is NOT trusted (corrupt stray file → re-download).
        fs::create_dir_all(models_dir(&dir)).unwrap();
        fs::write(model_path(&dir, &DEFAULT_MODEL), b"junk").unwrap();
        assert!(!model_is_ready(&dir, &DEFAULT_MODEL));
        save_state(
            &dir,
            &DownloadState {
                model_id: DEFAULT_MODEL.id.to_string(),
                done_bytes: 123,
                total_bytes: 123,
                finished: true,
                error: None,
            },
        );
        assert!(model_is_ready(&dir, &DEFAULT_MODEL));
        assert_eq!(on_disk_bytes(&dir, &DEFAULT_MODEL), 4); // "junk"
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_records_are_per_model_across_a_model_switch() {
        let dir = std::env::temp_dir().join("lhc-sidecar-state-per-model");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(models_dir(&dir)).unwrap();
        fs::write(model_path(&dir, &DEFAULT_MODEL), b"default-gguf").unwrap();
        save_state(
            &dir,
            &DownloadState {
                model_id: DEFAULT_MODEL.id.to_string(),
                done_bytes: 9,
                total_bytes: 9,
                finished: true,
                error: None,
            },
        );
        assert!(model_is_ready(&dir, &DEFAULT_MODEL));
        // A finished download record for the OTHER bundled model neither
        // vouches for the default model nor overwrites its record.
        save_state(
            &dir,
            &DownloadState {
                model_id: LARGE_MODEL.id.to_string(),
                done_bytes: 1,
                total_bytes: 1,
                finished: true,
                error: None,
            },
        );
        assert!(
            model_is_ready(&dir, &DEFAULT_MODEL),
            "the default model's record survives a 9B download untouched"
        );
        assert!(!model_is_ready(&dir, &LARGE_MODEL), "no 9B file on disk yet");
        fs::write(model_path(&dir, &LARGE_MODEL), b"nine-b-gguf").unwrap();
        assert!(model_is_ready(&dir, &LARGE_MODEL));
        // Deleting one model's file+record leaves the other model intact.
        assert!(delete_model(&dir, &LARGE_MODEL) > 0);
        assert!(model_is_ready(&dir, &DEFAULT_MODEL));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_single_state_file_migrates_per_model_without_cross_vouching() {
        let dir = std::env::temp_dir().join("lhc-sidecar-state-migrate");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(models_dir(&dir)).unwrap();
        fs::write(model_path(&dir, &DEFAULT_MODEL), b"legacy-gguf").unwrap();
        fs::write(
            legacy_state_path(&dir),
            serde_json::to_string(&DownloadState {
                model_id: DEFAULT_MODEL.id.to_string(),
                done_bytes: 3,
                total_bytes: 3,
                finished: true,
                error: None,
            })
            .unwrap(),
        )
        .unwrap();
        assert!(
            model_is_ready(&dir, &DEFAULT_MODEL),
            "the legacy record is read once and copied forward"
        );
        assert!(
            state_path(&dir, &DEFAULT_MODEL).is_file(),
            "the per-model record now exists"
        );
        assert!(
            load_state(&dir, &LARGE_MODEL).is_none(),
            "a legacy record never vouches for a different model"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bundled_menu_resolves_ids_and_holds_the_verified_nine_b_pin() {
        let ids: Vec<&str> = bundled_models().iter().map(|m| m.id).collect();
        assert_eq!(
            ids,
            vec![DEFAULT_MODEL.id, LARGE_MODEL.id],
            "index 0 stays the default 2B-class file; the 9B is opt-in"
        );
        assert_eq!(model_for_id(LARGE_MODEL.id).map(|m| m.file), Some(LARGE_MODEL.file));
        assert_eq!(
            model_for_id("  ornith-1.5-9b-q4_k_m  ").map(|m| m.id),
            Some(LARGE_MODEL.id),
            "paste noise tolerated at the seam (settings.rs canonicalizes separately)"
        );
        assert_eq!(model_for_id(""), None);
        assert_eq!(model_for_id("not-a-model"), None, "unknown ids never resolve — settings.rs normalizes them to the pinned default");

        let c = model_caption(&LARGE_MODEL);
        assert!(c.contains("Ornith-1.5-9B"));
        assert!(c.contains("Q4_K_M"));
        assert!(c.contains("5.8 GB"), "the ~6 GB Q4 size the card promises to show");
        assert!(c.contains("MIT"));
        // Pin facts as pulled from the HF blob API 2026-11-17 (exact byte
        // size + lfs sha256 + file name) — a silently changed upstream file
        // must not slip in unverified.
        assert_eq!(LARGE_MODEL.bytes_hint, 5_780_090_816);
        assert_eq!(LARGE_MODEL.sha256, "7ae5846389f612735899b85f8586b70059f0260d21248883d39fe755f878dcf5");
        assert_eq!(LARGE_MODEL.file, "ornith-1.5-9b-q4_k_m.gguf");
    }

    #[test]
    fn model_caption_carries_size_and_license() {
        let c = model_caption(&DEFAULT_MODEL);
        assert!(c.contains("Qwen2.5"));
        assert!(c.contains("Q4_K_M"));
        assert!(c.contains("1.1 GB"));
        assert!(c.contains("Apache-2.0"));
    }

    // ------------------------------------------------------------------
    // Real downloader against a local HTTP server (fixture serving), incl.
    // Range resume. No Hugging Face traffic in CI: the same code path runs
    // against an in-process Rust fixture server (Range-aware; python
    // http.server can't serve 206 and fixed fixture ports raced across
    // parallel tests) serving a fixture whose sha256 we control.
    // ------------------------------------------------------------------

    fn fixture_bytes(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 + 13) as u8).collect()
    }

    fn fixture_sha(data: &[u8]) -> String {
        format!("{:x}", Sha256::digest(data))
    }

    /// A fixture file: (URL-path, body bytes).
    type FixtureFiles = std::sync::Arc<std::sync::Mutex<Vec<(String, Vec<u8>)>>>;

    struct FixtureServer {
        port: u16,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
        /// Interior-mutable so tests can register JSON that embeds the
        /// server's own port (release metadata pointing back at fixtures).
        files: FixtureFiles,
    }

    impl FixtureServer {
        fn base_url(&self) -> String {
            format!("http://127.0.0.1:{}", self.port)
        }
        fn add_file(&self, name: &str, data: Vec<u8>) {
            self.files
                .lock()
                .expect("fixture files lock")
                .push((name.to_string(), data));
        }
    }

    impl Drop for FixtureServer {
        fn drop(&mut self) {
            self.stop
                .store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    /// In-process stand-in for a GGUF host: serves `files` over HTTP/1.1,
    /// honoring `Range: bytes=N-` with 206 + Content-Range so the resume
    /// path is genuinely exercised. Ephemeral port — parallel tests never
    /// clash, and Drop shuts the server down cleanly.
    fn start_fixture_server(files: Vec<(&str, Vec<u8>)>) -> FixtureServer {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let listener = TcpListener::bind("127.0.0.1:0").expect("fixture bind");
        let port = listener.local_addr().unwrap().port();
        listener
            .set_nonblocking(true)
            .expect("fixture nonblocking accept");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let files = Arc::new(std::sync::Mutex::new(
            files
                .into_iter()
                .map(|(n, d)| (n.to_string(), d))
                .collect::<Vec<(String, Vec<u8>)>>(),
        ));
        let files_for_thread = files.clone();
        let handle = std::thread::spawn(move || {
            loop {
                if stop_thread.load(Ordering::SeqCst) {
                    break;
                }
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(_) => break,
                };
                // Force blocking writes: a 1 MB body must not half-write on
                // a socket that inherited the listener's nonblocking mode.
                let _ = stream.set_nonblocking(false);
                let mut head = String::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = stream.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    head.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if head.contains("\r\n\r\n") {
                        break;
                    }
                }
                let mut parts = head.lines().next().unwrap_or("").split_whitespace();
                let method = parts.next().unwrap_or("GET").to_string();
                let path = parts.next().unwrap_or("/").trim_start_matches('/').to_string();
                let guard = match files_for_thread.lock() {
                    Ok(g) => g,
                    Err(_) => break,
                };
                let Some((_, data)) = guard.iter().find(|(name, _)| *name == path) else {
                    let _ = stream
                        .write_all(b"HTTP/1.1 404 Missing\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    continue;
                };
                let mut start = 0usize;
                for line in head.lines().skip(1) {
                    let l = line.trim();
                    if let Some(v) = l.strip_prefix("Range:").or_else(|| l.strip_prefix("range:")) {
                        if let Some(n) = v
                            .trim()
                            .trim_start_matches("bytes=")
                            .split('-')
                            .next()
                            .and_then(|s| s.parse::<usize>().ok())
                        {
                            start = n.min(data.len());
                        }
                    }
                }
                let slice = &data[start..];
                let header = if start > 0 {
                    format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                        slice.len(),
                        start,
                        data.len() - 1,
                        data.len()
                    )
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        slice.len()
                    )
                };
                let _ = stream.write_all(header.as_bytes());
                if method != "HEAD" {
                    let _ = stream.write_all(slice);
                }
                let _ = stream.flush();
            }
        });
        FixtureServer {
            port,
            stop,
            handle: Some(handle),
            files,
        }
    }

    fn test_spec(url: &str, sha: &str) -> ModelSpec {
        // ModelSpec pins &'static strs; leaked fixtures live for the test
        // process's lifetime and die with it (test-only allocation).
        ModelSpec {
            id: "fixture-model",
            label: "Fixture",
            family: "FixtureNet",
            quant: "F32",
            license: "MIT",
            repo: "local/fixture",
            file: "fixture.gguf",
            url: Box::leak(url.to_string().into_boxed_str()),
            bytes_hint: 1_048_576,
            sha256: Box::leak(sha.to_string().into_boxed_str()),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn downloader_verifies_sha256_and_lands_the_file() {
        let data_dir = std::env::temp_dir().join("lhc-sidecar-fixture-data");
        let _ = fs::remove_dir_all(&data_dir);
        let fixture = fixture_bytes(1_048_576);
        let server = start_fixture_server(vec![("fixture.gguf", fixture.clone())]);
        let spec = test_spec(
            &format!("http://127.0.0.1:{}/fixture.gguf", server.port),
            &fixture_sha(&fixture),
        );
        let mut last_done = 0u64;
        let result = download_model(&data_dir, &spec, |p| last_done = p.done).await;
        drop(server);

        let path = result.expect("download ok");
        assert_eq!(path, model_path(&data_dir, &spec));
        assert_eq!(fs::read(&path).unwrap(), fixture);
        assert!(last_done >= fixture.len() as u64 - 1, "progress callbacks advanced");
        assert!(model_is_ready(&data_dir, &spec));
        assert!(!part_path(&data_dir, &spec).exists(), ".part is renamed away");
        let state = load_state(&data_dir, &spec).expect("state");
        assert!(state.finished);
        assert_eq!(state.total_bytes, fixture.len() as u64);
        let _ = fs::remove_dir_all(&data_dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn downloader_resumes_a_part_file_and_still_verifies() {
        let data_dir = std::env::temp_dir().join("lhc-sidecar-resume-data");
        let _ = fs::remove_dir_all(&data_dir);
        let fixture = fixture_bytes(1_048_576);

        // Seed a half-written .part + matching state (simulated crash).
        fs::create_dir_all(models_dir(&data_dir)).unwrap();
        let half = &fixture[..524_288];
        fs::write(models_dir(&data_dir).join("fixture.gguf.part"), half).unwrap();
        save_state(
            &data_dir,
            &DownloadState {
                model_id: "fixture-model".to_string(),
                done_bytes: 524_288,
                total_bytes: 1_048_576,
                finished: false,
                error: None,
            },
        );

        let server = start_fixture_server(vec![("fixture.gguf", fixture.clone())]);
        let spec = test_spec(
            &format!("http://127.0.0.1:{}/fixture.gguf", server.port),
            &fixture_sha(&fixture),
        );
        let result = download_model(&data_dir, &spec, |_| {}).await;
        drop(server);

        // The fixture honors Range (206 → a true half-resume); the download
        // code must also survive servers that ignore Range and send the full
        // file (restart-from-zero). Either way the final file is byte-exact
        // and passes the pinned hash. That tolerance IS the test.
        let path = result.expect("resumed download completes");
        assert_eq!(fs::read(&path).unwrap(), fixture);
        assert!(model_is_ready(&data_dir, &spec));
        let _ = fs::remove_dir_all(&data_dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn downloader_rejects_hash_mismatch_and_leaves_no_part() {
        let data_dir = std::env::temp_dir().join("lhc-sidecar-badhash-data");
        let _ = fs::remove_dir_all(&data_dir);
        let fixture = fixture_bytes(262_144);
        let server = start_fixture_server(vec![("fixture.gguf", fixture.clone())]);
        let spec = test_spec(&format!("http://127.0.0.1:{}/fixture.gguf", server.port), &"0".repeat(64));
        let err = download_model(&data_dir, &spec, |_| {}).await.unwrap_err();
        drop(server);

        assert!(err.contains("sha256 mismatch"), "{err}");
        assert!(!part_path(&data_dir, &spec).exists(), "bad file must be discarded");
        assert!(!model_is_ready(&data_dir, &spec));
        let state = load_state(&data_dir, &spec).expect("state");
        assert!(state.error.is_some());
        let _ = fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn launch_plan_uses_loopback_and_small_ctx_defaults() {
        let c = cfg_base();
        assert_eq!(c.mode, LlmMode::None, "opt-in by construction");
        let args = launch_args(&c, Path::new("/m.gguf"), 8081);
        let ctx = args.windows(2).find(|w| w[0] == "--ctx-size").expect("ctx-size")[1].clone();
        assert_eq!(ctx, "4096", "default ctx is deliberately small");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stub_llama_server_full_lifecycle_spawn_probe_args_and_kill() {
        // Integration stand-in (the documented choice on this card): this
        // box has no llama-server binary, so a python3 script *posing as*
        // llama-server goes through the real spawn_sidecar / probe_endpoint /
        // log_tail / stop_child production code. The stub echoes its received
        // argv, so the test asserts the production resource-cap flags really
        // reach the child, and it handles SIGTERM cleanly like llama-server.
        use std::os::unix::fs::PermissionsExt;

        let data_dir = std::env::temp_dir().join("lhc-sidecar-e2e");
        let _ = fs::remove_dir_all(&data_dir);
        fs::create_dir_all(models_dir(&data_dir)).unwrap();
        // A tiny stand-in "model file": production code only requires it to
        // exist; the stub never parses it.
        fs::write(model_path(&data_dir, &DEFAULT_MODEL), b"stub-gguf").unwrap();

        let stub = data_dir.join("stub-llama-server.py");
        fs::write(
            &stub,
            r#"#!/usr/bin/env python3
import socketserver, signal, sys, time

class H(socketserver.BaseRequestHandler):
    def handle(self):
        try:
            data = self.request.recv(4096).decode("latin-1", "replace")
            line = data.split("\r\n", 1)[0]
            path = line.split(" ")[1] if " " in line else "/"
            if path == "/health":
                body, status = b'{"status":"ok"}', "200 OK"
            elif path == "/v1/models":
                body, status = b'{"data":[]}', "200 OK"
            else:
                body, status = b'{"error":"no"}', "404 Not Found"
            head = "HTTP/1.1 %s\r\nContent-Length: %d\r\nConnection: close\r\n\r\n" % (status, len(body))
            self.request.sendall(head.encode() + body)
        except Exception:
            pass

class Srv(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True

def main():
    argv = sys.argv[1:]
    print("stub-llama-server argv:", flush=True)
    for a in argv:
        print(a, flush=True)
    port = 8081
    if "--port" in argv:
        port = int(argv[argv.index("--port") + 1])
    def bye(*_a):
        print("stub-llama-server: SIGTERM received, exiting clean", flush=True)
        sys.exit(0)
    signal.signal(signal.SIGTERM, bye)
    for _ in range(40):
        try:
            with Srv(("127.0.0.1", port), H) as srv:
                print("stub-llama-server: listening on %d" % port, flush=True)
                srv.serve_forever()
            return
        except OSError:
            time.sleep(0.25)
    sys.exit("stub could not bind port %d" % port)

main()
"#,
        )
        .unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();

        // Ask the OS for a free port, then hand the number to the stub.
        let listener = TcpListener::bind("127.0.0.1:0").expect("port probe bind");
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let mut c = cfg_base();
        c.mode = LlmMode::Auto;
        c.cap = ResourceCap::CpuThird;
        c.endpoint = format!("http://127.0.0.1:{port}/v1");
        c.llama_server_bin = stub.display().to_string();

        let child = spawn_sidecar(&c, &data_dir, &DEFAULT_MODEL).expect("stub sidecar spawns");
        let pid = child.id();

        // The stub is a python script: give the interpreter time to boot.
        let mut healthy = false;
        for _ in 0..50 {
            if probe_endpoint(&c.endpoint).await.is_ok() {
                healthy = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        assert!(healthy, "stub sidecar never became healthy on port {port}");

        stop_child(child);
        let orphan = unsafe { libc::kill(pid as i32, 0) == 0 };
        assert!(!orphan, "stub sidecar pid {pid} survived stop_child — orphan leak");

        let log = log_tail(&data_dir, 80);
        let args: Vec<String> = log.lines().map(|l| l.trim().to_string()).collect();
        let flag_value = |flag: &str| -> Option<String> {
            args.iter()
                .position(|a| a == flag)
                .and_then(|i| args.get(i + 1))
                .cloned()
        };
        let cores = std::thread::available_parallelism().unwrap().get();
        let port_s = port.to_string();
        let threads_s = ((cores / 3).max(1)).to_string();
        assert_eq!(flag_value("--port").as_deref(), Some(port_s.as_str()));
        assert_eq!(flag_value("--parallel").as_deref(), Some("1"), "single slot");
        assert_eq!(flag_value("--ctx-size").as_deref(), Some("4096"));
        assert_eq!(
            flag_value("--n-gpu-layers").as_deref(),
            Some("0"),
            "cpu cap forbids GPU offload"
        );
        assert_eq!(
            flag_value("--threads").as_deref(),
            Some(threads_s.as_str()),
            "CPU ≤⅓ cores maps to llama-server --threads"
        );
        assert!(flag_value("--model").is_some(), "model path flag reached the child");
        assert!(log.contains("listening"), "stub reached its serve loop");
        assert!(log.contains("SIGTERM"), "graceful SIGTERM handshake logged");

        delete_model(&data_dir, &DEFAULT_MODEL);
        let _ = fs::remove_dir_all(&data_dir);
    }

    // ---- binary auto-acquisition tests (card t_5f4b0943) -----------------

    fn fresh_bin_dir(label: &str) -> PathBuf {
        let d = std::env::temp_dir().join(label);
        let _ = fs::remove_dir_all(&d);
        d
    }

    /// Build a real .tar.gz fixture shaped like llama.cpp's release:
    /// `llama-<tag>/` top dir, regular files + real symlinks.
    fn tar_gz_fixture(top: &str, files: &[(&str, &[u8], u32)], links: &[(&str, &str)]) -> Vec<u8> {
        use flate2::write::GzEncoder;
        use tar::{Builder as TarBuilder, EntryType as TarEntryType, Header as TarHeader};
        let enc = GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut b = TarBuilder::new(enc);
        for (name, data, mode) in files {
            let mut h = TarHeader::new_gnu();
            h.set_mode(*mode);
            h.set_entry_type(TarEntryType::Regular);
            h.set_size(data.len() as u64);
            b.append_data(&mut h, format!("{top}/{name}"), *data)
                .expect("tar fixture append");
        }
        for (name, target) in links {
            let mut h = TarHeader::new_gnu();
            h.set_entry_type(TarEntryType::Symlink);
            h.set_mode(0o777);
            h.set_size(0);
            h.set_link_name(*target).expect("tar fixture link name");
            b.append_data(&mut h, format!("{top}/{name}"), std::io::empty())
                .expect("tar fixture symlink append");
        }
        b.into_inner().expect("tar fixture into_inner").finish().expect("tar fixture gzip finish")
    }

    fn zip_fixture(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        use zip::write::{SimpleFileOptions, ZipWriter};
        let mut w = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, data, mode) in entries {
            w.start_file(name, SimpleFileOptions::default().unix_permissions(*mode))
                .expect("zip fixture start");
            w.write_all(data).expect("zip fixture write");
        }
        w.finish().expect("zip fixture finish").into_inner()
    }

    fn version_stub(build: &str) -> Vec<u8> {
        format!("#!/bin/sh\necho 'version: 0.0.0-fixture (build {build})'\n").into_bytes()
    }

    fn selfcheck_fail_stub() -> Vec<u8> {
        b"#!/bin/sh\necho 'this stub exits 7' >&2\nexit 7\n".to_vec()
    }

    fn host_fixture_asset(tag: &str) -> Option<String> {
        host_asset_name(tag, BinBackend::Cpu).ok()
    }

    fn fixture_resolved(port: u16, tag: &str, asset: &str, data: &[u8], backend: &'static str) -> ResolvedBinary {
        ResolvedBinary {
            tag: tag.to_string(),
            asset: asset.to_string(),
            url: format!("http://127.0.0.1:{port}/{asset}"),
            sha256: fixture_sha(data),
            bytes_hint: data.len() as u64,
            backend,
        }
    }

    #[test]
    fn binary_pin_table_and_asset_matrix_are_deterministic() {
        assert_eq!(BIN_PIN_TAG, "b12359");
        for spec in [&PIN_LINUX_CPU, &PIN_LINUX_VULKAN, &PIN_WIN_CPU, &PIN_WIN_VULKAN, &PIN_MACOS_ARM64] {
            assert!(spec.asset.starts_with(&format!("llama-{}-bin-", spec.tag)));
            assert!(spec.url.starts_with("https://github.com/ggml-org/llama.cpp/releases/download/"));
            assert_eq!(spec.sha256.len(), 64);
            assert!(
                spec.sha256.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "{} digest must be lowercase hex", spec.asset
            );
            assert!(spec.bytes_hint > 1_000_000);
        }
        assert_eq!(build_number("b12359"), Some(12359));
        assert_eq!(build_number("v1.0.0"), None);
        assert_eq!(build_number("b12359rc"), None);
        assert_eq!(build_number("b"), None);
        assert_eq!(main_name_for_archive("llama-b1-bin-win-cpu-x64.zip"), "llama-server.exe");
        assert_eq!(main_name_for_archive("llama-b1-bin-ubuntu-x64.tar.gz"), "llama-server");
        // Extract filter: the server + its companion libs, nothing else.
        assert!(should_extract_leaf("libggml-cpu-zen4.so", "llama-server"));
        assert!(should_extract_leaf("libbase.so.1", "llama-server"));
        assert!(should_extract_leaf("libbase.1.0.0.dylib", "llama-server"));
        assert!(should_extract_leaf("ggml.dll", "llama-server.exe"));
        assert!(should_extract_leaf("llama-server-impl.dll", "llama-server.exe"));
        assert!(!should_extract_leaf("llama-cli.exe", "llama-server.exe"));
        assert!(!should_extract_leaf("ggml-base.lib", "llama-server.exe"));
        assert!(!should_extract_leaf("LICENSE", "llama-server"));
        // Host-side matrix selection.
        if host_on_matrix() {
            let cpu = host_asset_name("b99999", BinBackend::Cpu).expect("matrix host has a cpu asset");
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            {
                assert_eq!(cpu, "llama-b99999-bin-ubuntu-x64.tar.gz");
                assert_eq!(host_asset_name("b99999", BinBackend::Vulkan).unwrap(), "llama-b99999-bin-ubuntu-vulkan-x64.tar.gz");
                assert_eq!(select_pin_spec(BinBackend::Cpu).unwrap().asset, PIN_LINUX_CPU.asset);
                assert_eq!(select_pin_spec(BinBackend::Vulkan).unwrap().asset, PIN_LINUX_VULKAN.asset);
            }
            #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
            {
                assert_eq!(cpu, "llama-b99999-bin-win-cpu-x64.zip");
                assert_eq!(host_asset_name("b99999", BinBackend::Vulkan).unwrap(), "llama-b99999-bin-win-vulkan-x64.zip");
                assert_eq!(select_pin_spec(BinBackend::Cpu).unwrap().asset, PIN_WIN_CPU.asset);
                assert_eq!(select_pin_spec(BinBackend::Vulkan).unwrap().asset, PIN_WIN_VULKAN.asset);
            }
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                assert_eq!(cpu, "llama-b99999-bin-macos-arm64.tar.gz");
                // macOS has ONE asset; the Vulkan request maps onto it too.
                assert_eq!(host_asset_name("b99999", BinBackend::Vulkan).unwrap(), "llama-b99999-bin-macos-arm64.tar.gz");
                assert_eq!(select_pin_spec(BinBackend::Vulkan).unwrap().asset, PIN_MACOS_ARM64.asset);
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn binary_acquisition_installs_only_matrix_files_after_digest_and_selfcheck() {
        let Some(asset) = host_fixture_asset("b99999") else {
            println!("host not on the binary matrix — skipping");
            return;
        };
        let data_dir = fresh_bin_dir("lhc-bin-acquire");
        let tar = tar_gz_fixture(
            "llama-b99999",
            &[
                ("llama-server", &version_stub("99999"), 0o755),
                ("libbase.so", b"BASE", 0o644),
                ("libbase.so.1.0.0", b"BASE-100", 0o644),
                ("llama-cli", b"a tool the matrix must not extract", 0o755),
                ("LICENSE", b"MIT", 0o644),
            ],
            &[("libbase.so.1", "libbase.so")],
        );
        let server = start_fixture_server(vec![(asset.as_str(), tar.clone())]);
        let resolved = fixture_resolved(server.port, "b99999", &asset, &tar, "cpu");
        let mut saw_progress = false;
        let version = download_binary(&data_dir, &resolved, |p| {
            if p.done > 0 {
                saw_progress = true;
            }
        })
        .await
        .expect("fixture install");
        drop(server);

        assert!(version.contains("build 99999"), "{version}");
        let bin = data_dir.join("bin");
        let exe = main_exe_path(&data_dir);
        assert_eq!(fs::read(&exe).unwrap(), version_stub("99999"));
        use std::os::unix::fs::PermissionsExt;
        assert!(fs::metadata(&exe).unwrap().permissions().mode() & 0o111 != 0, "installed binary is executable");
        assert_eq!(fs::read(bin.join("libbase.so.1.0.0")).unwrap(), b"BASE-100");
        // Archive symlinks survive promotion, re-based onto bin/ names.
        let link = fs::symlink_metadata(bin.join("libbase.so.1")).expect("symlink promoted");
        assert!(link.file_type().is_symlink());
        assert_eq!(fs::read_link(bin.join("libbase.so.1")).unwrap().to_string_lossy(), "libbase.so");
        assert_eq!(fs::read_to_string(bin.join("libbase.so.1")).unwrap(), "BASE");
        // Non-matrix members of the archive stayed out entirely.
        assert!(!bin.join("llama-cli").exists());
        assert!(!bin.join("LICENSE").exists());
        // No .part, no staging debris after a successful run.
        let debris: Vec<String> = fs::read_dir(&bin)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".part") || n.starts_with(".staging"))
            .collect();
        assert!(debris.is_empty(), "leftover debris: {debris:?}");
        // Bookkeeping vouches for the install.
        let state = load_bin_state(&data_dir).expect("bin download state");
        assert!(state.finished);
        assert_eq!(state.tag, "b99999");
        assert_eq!(state.asset, asset);
        assert_eq!(state.sha256, fixture_sha(&tar));
        assert_eq!(state.version.as_deref(), Some(version.as_str()));
        assert_eq!(state.total_bytes, tar.len() as u64);
        assert_eq!(state.done_bytes, tar.len() as u64);
        assert!(saw_progress, "progress callbacks advanced");
        // Resolution order: downloaded binary answers an empty override;
        // a broken override is a hard None, never a silent fallthrough.
        assert_eq!(trusted_binary(&data_dir), Some(exe.clone()));
        assert_eq!(resolve_binary("", &data_dir), Some(exe.clone()));
        assert_eq!(resolve_binary("   ", &data_dir), Some(exe.clone()));
        assert_eq!(resolve_binary("/definitely/not/a/file", &data_dir), None);
        // The promoted flat layout really runs.
        assert_eq!(capture_version(&exe).as_deref(), Some(version.as_str()));
        // delete_binary clears bin/ + state (nothing outside bin/ is touched).
        let freed = delete_binary(&data_dir);
        assert!(freed > 0);
        assert!(!bin.exists());
        assert_eq!(trusted_binary(&data_dir), None);
        let _ = fs::remove_dir_all(&data_dir);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn binary_acquisition_discards_digest_mismatch_and_installs_nothing() {
        let Some(asset) = host_fixture_asset("b99998") else {
            println!("host not on the binary matrix — skipping");
            return;
        };
        let data_dir = fresh_bin_dir("lhc-bin-badhash");
        let tar = tar_gz_fixture("llama-b99998", &[("llama-server", &version_stub("99998"), 0o755)], &[]);
        let server = start_fixture_server(vec![(asset.as_str(), tar.clone())]);
        let mut resolved = fixture_resolved(server.port, "b99998", &asset, &tar, "cpu");
        // The pin lies → the verification must be what catches it.
        resolved.sha256 = "0".repeat(64);
        let err = download_binary(&data_dir, &resolved, |_| {}).await.unwrap_err();
        drop(server);
        assert!(err.contains("sha256 mismatch"), "{err}");
        let bin = data_dir.join("bin");
        assert!(!bin.join("llama-server").exists(), "nothing unchecked may reach bin/");
        assert_eq!(trusted_binary(&data_dir), None);
        assert!(!bin.join(format!("{asset}.part")).exists(), "the bad archive is discarded");
        let state = load_bin_state(&data_dir).expect("failure state persisted");
        assert!(!state.finished);
        assert!(state.error.is_some());
        let _ = fs::remove_dir_all(&data_dir);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_binary_update_keeps_the_previous_verified_install_trusted() {
        let Some(asset) = host_fixture_asset("b99997") else {
            println!("host not on the binary matrix — skipping");
            return;
        };
        let data_dir = fresh_bin_dir("lhc-bin-updatefail");
        let bin = data_dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let old_exe = main_exe_path(&data_dir);
        fs::write(&old_exe, b"ORIGINAL-BINARY").unwrap();
        save_bin_state(
            &data_dir,
            &BinDownloadState {
                tag: "b55555".to_string(),
                asset: "llama-b55555-bin-ubuntu-x64.tar.gz".to_string(),
                backend: "cpu".to_string(),
                sha256: "a".repeat(64),
                total_bytes: 10,
                done_bytes: 10,
                finished: true,
                error: None,
                version: Some("version: old (build 55555)".to_string()),
            },
        );
        assert_eq!(trusted_binary(&data_dir), Some(old_exe.clone()));

        // The NEW asset extracts but fails its --version self-check.
        let tar = tar_gz_fixture("llama-b99997", &[("llama-server", &selfcheck_fail_stub(), 0o755)], &[]);
        let server = start_fixture_server(vec![(asset.as_str(), tar.clone())]);
        let resolved = fixture_resolved(server.port, "b99997", &asset, &tar, "cpu");
        let err = download_binary(&data_dir, &resolved, |_| {}).await.unwrap_err();
        drop(server);
        assert!(err.contains("self-check"), "{err}");
        assert_eq!(fs::read(&old_exe).unwrap(), b"ORIGINAL-BINARY", "old binary bytes untouched");
        let state = load_bin_state(&data_dir).unwrap();
        assert!(state.finished && state.tag == "b55555", "failed update must restore the old trust record, got {state:?}");
        assert_eq!(trusted_binary(&data_dir), Some(old_exe.clone()), "old install stays trusted");
        assert!(!bin.join(format!("{asset}.part")).exists());
        let _ = fs::remove_dir_all(&data_dir);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn windows_zip_asset_installs_server_plus_every_dll_and_nothing_else() {
        // Shape proof on a unix runner: main_name_for_archive() keys off the
        // ARCHIVE (zip → llama-server.exe), not the host, so the Windows
        // asset layout is genuinely exercised end-to-end here.
        let data_dir = fresh_bin_dir("lhc-bin-zipshape");
        let asset = "llama-b99996-bin-win-cpu-x64.zip";
        let exe_stub = version_stub("99996");
        let zip = zip_fixture(&[
            ("llama-server.exe", &exe_stub, 0o755),
            ("ggml-base.dll", b"DLL1", 0o644),
            ("llama-server-impl.dll", b"DLL2", 0o644),
            ("libomp.dll", b"DLL3", 0o644),
            ("llama-cli.exe", b"helper the matrix excludes", 0o755),
            ("ggml-base.lib", b"import lib excluded", 0o644),
            ("LICENSE-LLVM", b"notice text excluded", 0o644),
        ]);
        let server = start_fixture_server(vec![(asset, zip.clone())]);
        let resolved = fixture_resolved(server.port, "b99996", asset, &zip, "cpu");
        let version = download_binary(&data_dir, &resolved, |_| {}).await.expect("zip shape install");
        drop(server);
        assert!(version.contains("build 99996"), "{version}");
        let bin = data_dir.join("bin");
        assert_eq!(fs::read(bin.join("llama-server.exe")).unwrap(), exe_stub);
        assert_eq!(fs::read(bin.join("ggml-base.dll")).unwrap(), b"DLL1");
        assert!(bin.join("llama-server-impl.dll").exists());
        assert!(bin.join("libomp.dll").exists(), "the whole zip's DLLs ship together");
        assert!(!bin.join("llama-cli.exe").exists());
        assert!(!bin.join("ggml-base.lib").exists());
        assert!(!bin.join("LICENSE-LLVM").exists());
        assert!(!bin.join(format!("{asset}.part")).exists());
        let state = load_bin_state(&data_dir).unwrap();
        assert!(state.finished && state.tag == "b99996" && state.asset == asset);
        let _ = fs::remove_dir_all(&data_dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolver_follows_verified_nightly_pointer_to_release_with_binaries() {
        if !host_on_matrix() {
            println!("host not on the binary matrix — skipping");
            return;
        }
        let nightly_bytes = b"b99999\n".to_vec();
        let tar = tar_gz_fixture("llama-b99999", &[("llama-server", &version_stub("99999"), 0o755)], &[]);
        let data_dir = fresh_bin_dir("lhc-bin-resolve");
        let server = start_fixture_server(vec![]);
        let base = server.base_url();
        let latest = serde_json::json!({
            "tag_name": "v9.9.9",
            "assets": [{
                "name": NIGHTLY_POINTER_FILE,
                "size": nightly_bytes.len(),
                "digest": format!("sha256:{}", fixture_sha(&nightly_bytes)),
                "browser_download_url": format!("{base}/{NIGHTLY_POINTER_FILE}"),
            }]
        });
        server.add_file("api/releases/latest", latest.to_string().into_bytes());
        server.add_file(NIGHTLY_POINTER_FILE, nightly_bytes.clone());
        // The nightly release the pointer names — with its real digest.
        let expected_asset = host_fixture_asset("b99999").expect("matrix host");
        server.add_file(
            "api/releases/tags/b99999",
            serde_json::json!({
                "tag_name": "b99999",
                "assets": [{
                    "name": expected_asset,
                    "size": tar.len(),
                    "digest": format!("sha256:{}", fixture_sha(&tar)),
                    "browser_download_url": format!("{base}/{expected_asset}"),
                }]
            })
            .to_string()
            .into_bytes(),
        );
        server.add_file(&expected_asset, tar.clone());
        let resolved = resolve_latest_binary(&format!("{base}/api"), BinBackend::Cpu)
            .await
            .expect("v1-style latest resolves through the pointer");
        assert_eq!(resolved.tag, "b99999");
        assert_eq!(resolved.asset, host_fixture_asset("b99999").expect("matrix host"));
        assert_eq!(resolved.sha256, fixture_sha(&tar));
        assert_eq!(resolved.bytes_hint, tar.len() as u64);
        // An API-resolved resolution installs with the same gate (digest +
        // self-check) as a pinned one — this is the update path's trust shape.
        let asset = resolved.asset.clone();
        server.add_file(&asset, tar.clone());
        let version = download_binary(&data_dir, &resolved, |_| {}).await.expect("API-resolved install");
        assert!(version.contains("build 99999"), "{version}");
        assert_eq!(trusted_binary(&data_dir), Some(main_exe_path(&data_dir)));
        drop(server);
        let _ = fs::remove_dir_all(&data_dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolver_refuses_layout_drift_missing_digests_and_bad_pointers() {
        if !host_on_matrix() {
            println!("host not on the binary matrix — skipping");
            return;
        }
        let expected = host_asset_name("b99993", BinBackend::Cpu).expect("matrix host");

        // (a) latest has neither the matrix asset nor a pointer — no guessing.
        let s = start_fixture_server(vec![(
            "api/releases/latest",
            serde_json::json!({"tag_name": "v9", "assets": [{
                "name": "totally-renamed.tar.gz",
                "size": 5,
                "digest": format!("sha256:{}", "e".repeat(64)),
                "browser_download_url": "https://github.com/ggml-org/llama.cpp/releases/download/v9/totally-renamed.tar.gz",
            }]}).to_string().into_bytes(),
        )]);
        let e = resolve_latest_binary(&format!("{}/api", s.base_url()), BinBackend::Cpu).await.unwrap_err();
        assert!(e.contains("refusing to guess"), "{e}");
        drop(s);

        // (b) pointer content is not a llama.cpp build tag (loopback fetch
        // succeeds; the CONTENT is what the resolver must reject).
        let bad_pointer = b"stable\n".to_vec();
        let s = start_fixture_server(vec![]);
        let base = s.base_url();
        s.add_file(
            "api/releases/latest",
            serde_json::json!({"tag_name": "v9", "assets": [{
                "name": NIGHTLY_POINTER_FILE,
                "size": bad_pointer.len(),
                "digest": format!("sha256:{}", fixture_sha(&bad_pointer)),
                "browser_download_url": format!("{base}/{NIGHTLY_POINTER_FILE}"),
            }]}).to_string().into_bytes(),
        );
        s.add_file(NIGHTLY_POINTER_FILE, bad_pointer.clone());
        let e = resolve_latest_binary(&format!("{base}/api"), BinBackend::Cpu).await.unwrap_err();
        assert!(e.contains("not a llama.cpp build tag"), "{e}");
        drop(s);

        // (c) nightly release asset renamed — exact-name matrix, no fuzzy match.
        let pointer = b"b99993\n".to_vec();
        let s = start_fixture_server(vec![]);
        let base = s.base_url();
        s.add_file(
            "api/releases/latest",
            serde_json::json!({"tag_name": "v9", "assets": [{
                "name": NIGHTLY_POINTER_FILE,
                "size": pointer.len(),
                "digest": format!("sha256:{}", fixture_sha(&pointer)),
                "browser_download_url": format!("{base}/{NIGHTLY_POINTER_FILE}"),
            }]}).to_string().into_bytes(),
        );
        s.add_file(NIGHTLY_POINTER_FILE, pointer.clone());
        s.add_file(
            "api/releases/tags/b99993",
            serde_json::json!({"tag_name": "b99993", "assets": [{
                "name": "renamed-thing.tar.gz",
                "size": 9,
                "digest": format!("sha256:{}", "d".repeat(64)),
                "browser_download_url": format!("{base}/renamed-thing.tar.gz"),
            }]}).to_string().into_bytes(),
        );
        let e = resolve_latest_binary(&format!("{base}/api"), BinBackend::Cpu).await.unwrap_err();
        assert!(e.contains("refusing to fuzzy-match"), "{e}");
        drop(s);

        // (d) matrix asset present but the API carries no digest → refuse.
        let s = start_fixture_server(vec![(
            "api/releases/latest",
            serde_json::json!({"tag_name": "b99993", "assets": [{
                "name": expected,
                "size": 4242,
                "browser_download_url": format!("https://github.com/ggml-org/llama.cpp/releases/download/b99993/{expected}"),
            }]}).to_string().into_bytes(),
        )]);
        let e = resolve_latest_binary(&format!("{}/api", s.base_url()), BinBackend::Cpu).await.unwrap_err();
        assert!(e.contains("no sha256 digest"), "{e}");
        drop(s);

        // (e) non-HTTPS asset URL → refuse.
        let s = start_fixture_server(vec![(
            "api/releases/latest",
            serde_json::json!({"tag_name": "b99993", "assets": [{
                "name": expected,
                "size": 4242,
                "digest": format!("sha256:{}", "1".repeat(64)),
                "browser_download_url": "http://evil.example.invalid/x.tar.gz",
            }]}).to_string().into_bytes(),
        )]);
        let e = resolve_latest_binary(&format!("{}/api", s.base_url()), BinBackend::Cpu).await.unwrap_err();
        assert!(e.contains("refusing non-HTTPS"), "{e}");
        drop(s);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn binary_acquisition_reverifies_complete_part_from_disk_without_network() {
        let Some(asset) = host_fixture_asset("b99992") else {
            println!("host not on the binary matrix — skipping");
            return;
        };
        let data_dir = fresh_bin_dir("lhc-bin-offline");
        let tar = tar_gz_fixture("llama-b99992", &[("llama-server", &version_stub("99992"), 0o755)], &[]);
        let bin = data_dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join(format!("{asset}.part")), &tar).unwrap();
        save_bin_state(
            &data_dir,
            &BinDownloadState {
                tag: "b99992".to_string(),
                asset: asset.clone(),
                backend: "cpu".to_string(),
                sha256: fixture_sha(&tar),
                total_bytes: tar.len() as u64,
                done_bytes: tar.len() as u64,
                finished: false,
                error: Some("previous run was cut off before extraction".to_string()),
                version: None,
            },
        );
        // URL deliberately points at a CLOSED port: reaching it would fail
        // the test, proving the disk re-verification path needs no network.
        let resolved = fixture_resolved(1, "b99992", &asset, &tar, "cpu");
        let version = download_binary(&data_dir, &resolved, |_| {}).await.expect("disk-only re-verification");
        assert!(version.contains("build 99992"), "{version}");
        assert_eq!(trusted_binary(&data_dir), Some(main_exe_path(&data_dir)));
        assert!(!bin.join(format!("{asset}.part")).exists(), "part consumed on success");
        let _ = fs::remove_dir_all(&data_dir);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn binary_acquisition_resumes_partial_part_and_still_verifies() {
        let Some(asset) = host_fixture_asset("b99991") else {
            println!("host not on the binary matrix — skipping");
            return;
        };
        let data_dir = fresh_bin_dir("lhc-bin-resume");
        // A big filler entry makes the compressed archive worth resuming.
        let stub = version_stub("99991");
        let filler = vec![0xABu8; 1_500_000];
        let entries: [(&str, &[u8], u32); 2] = [("llama-server", &stub, 0o755), ("libfill.so", &filler, 0o644)];
        let tar = tar_gz_fixture("llama-b99991", &entries, &[]);
        let half = tar.len() / 2;
        let bin = data_dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join(format!("{asset}.part")), &tar[..half]).unwrap();
        save_bin_state(
            &data_dir,
            &BinDownloadState {
                tag: "b99991".to_string(),
                asset: asset.clone(),
                backend: "cpu".to_string(),
                sha256: fixture_sha(&tar),
                total_bytes: tar.len() as u64,
                done_bytes: half as u64,
                finished: false,
                error: None,
                version: None,
            },
        );
        let server = start_fixture_server(vec![(asset.as_str(), tar.clone())]);
        let resolved = fixture_resolved(server.port, "b99991", &asset, &tar, "cpu");
        let version = download_binary(&data_dir, &resolved, |_| {}).await.expect("resumed install verifies");
        drop(server);
        assert!(version.contains("build 99991"), "{version}");
        assert_eq!(trusted_binary(&data_dir), Some(main_exe_path(&data_dir)));
        assert_eq!(fs::read(data_dir.join("bin/libfill.so")).unwrap().len(), 1_500_000);
        assert!(!bin.join(format!("{asset}.part")).exists());
        let state = load_bin_state(&data_dir).unwrap();
        assert!(state.finished && state.done_bytes == state.total_bytes);
        let _ = fs::remove_dir_all(&data_dir);
    }

    /// Live end-to-end (card verify step 2) against the REAL GitHub API +
    /// asset store: resolve the pin chain, download the real linux-x64 CPU
    /// asset (~23 MB), install it, run `llama-server --version` for real,
    /// then delete everything. Ignored by default:
    ///   cargo test live_binary -- --ignored --nocapture
    #[cfg(all(unix, target_arch = "x86_64"))]
    #[ignore]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_binary_fetch_install_and_version_check_against_github() {
        let data_dir = fresh_bin_dir("lhc-bin-live-e2e");
        let spec = select_pin_spec(BinBackend::Cpu).expect("linux-x64 is on the matrix");
        // The live resolution chain must STILL point at the pinned tag with
        // the pinned digest — if upstream moved, this test fails on purpose
        // and the pin table must be re-verified before trusting the chain.
        let live = resolve_latest_binary(GGML_RELEASES_API, BinBackend::Cpu).await.expect("resolve latest");
        assert_eq!(live.tag, BIN_PIN_TAG, "upstream latest no longer matches the pin — re-verify BIN_PIN");
        assert_eq!(live.sha256, spec.sha256, "pin digest vs live API digest drift");
        assert_eq!(live.asset, spec.asset);
        assert_eq!(live.bytes_hint, spec.bytes_hint);

        let version = download_binary(&data_dir, &ResolvedBinary::from_pin(spec), |p| println!("live fetch {}/{}", p.done, p.total))
            .await
            .expect("live fetch + install");
        assert!(version.contains(&format!("build {}", build_number(BIN_PIN_TAG).unwrap())), "{version}");
        let exe = main_exe_path(&data_dir);
        assert_eq!(capture_version(&exe).as_deref(), Some(version.as_str()));

        // The flat bin/ layout must genuinely load its companions: a run
        // against a nonexistent model must REACH model loading before
        // failing — a broken library layout dies at dlopen instead.
        let out = Command::new(&exe).args(["-m", "/nonexistent.gguf"]).output().expect("spawn stubbed model path");
        let combined = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(
            combined.contains("llama_model_load") || combined.contains("load_model") || combined.contains("model_loader"),
            "binary never reached model loading — extracted layout is broken:\n{combined}"
        );

        let freed = delete_binary(&data_dir);
        assert!(freed > 1_000_000, "live install should have put MBs on disk");
        assert!(!data_dir.join("bin").exists());
        let _ = fs::remove_dir_all(&data_dir);
    }

    /// Pin-table hygiene: every pinned asset name + digest + size must
    /// still match the live releases API. Ignored by default:
    ///   cargo test live_binary_pins_match_upstream -- --ignored
    #[ignore]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_binary_pins_match_upstream_release_digests() {
        let client = reqwest::Client::builder().user_agent(ACQUISITION_UA).build().expect("client");
        let release: ReleaseInfo = api_get_json(
            &client,
            &format!("{GGML_RELEASES_API}/releases/tags/{BIN_PIN_TAG}"),
        )
        .await
        .expect("pinned release still on GitHub");
        for pinned in [&PIN_LINUX_CPU, &PIN_LINUX_VULKAN, &PIN_WIN_CPU, &PIN_WIN_VULKAN, &PIN_MACOS_ARM64] {
            let asset = release
                .assets
                .iter()
                .find(|a| a.name == pinned.asset)
                .unwrap_or_else(|| panic!("{} is no longer an asset of release {BIN_PIN_TAG} — re-pin required", pinned.asset));
            let hex = api_digest_hex(asset.digest.as_deref(), pinned.asset).expect("digest present");
            assert_eq!(hex, pinned.sha256, "{} digest drift — re-pin required", pinned.asset);
            assert_eq!(asset.size, pinned.bytes_hint, "{} size drift — re-pin required", pinned.asset);
        }
    }
}
