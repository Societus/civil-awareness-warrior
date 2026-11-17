//! Local Model sidecar manager (opt-in local LLM grading, resource-capped).
//!
//! Three modes collapse onto the SDK's single `LlmConfig` surface:
//!
//! - **None** — nothing here runs; zero sidecar code paths touched.
//! - **Auto-configure** — download the bundled GGUF (see `DEFAULT_MODEL`)
//!   into `{data_dir}/models/`, spawn `llama-server` as a child process with
//!   resource-cap flags (`--threads`, `--n-gpu-layers`), a single slot
//!   (`--parallel 1`), a small context, and health-probe `{origin}/health`
//!   (fallback `{base}/models`) before declaring ready. The child is killed
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

/// Locate the llama-server binary: explicit override first, then PATH,
/// then the usual install homes. None means "not installed" — the UI tells
/// the volunteer to install llama.cpp or switch to a Custom endpoint.
pub fn resolve_binary(override_bin: &str) -> Option<PathBuf> {
    let o = override_bin.trim();
    if !o.is_empty() {
        let p = PathBuf::from(o);
        return if p.is_file() { Some(p) } else { None };
    }
    let name = if cfg!(windows) { "llama-server.exe" } else { "llama-server" };
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
    let bin = resolve_binary(&cfg.llama_server_bin).ok_or_else(|| {
        if !cfg.llama_server_bin.trim().is_empty() {
            format!(
                "llama-server override {:#?} is not a file — fix the path or clear it to search PATH",
                cfg.llama_server_bin.trim()
            )
        } else {
            "llama-server binary not found on PATH — install llama.cpp, point Settings at your binary, or switch the Local Model mode to Custom endpoint".to_string()
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

    struct FixtureServer {
        port: u16,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
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
        let files: Vec<(String, Vec<u8>)> = files
            .into_iter()
            .map(|(n, d)| (n.to_string(), d))
            .collect();
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
                let Some((_, data)) = files.iter().find(|(name, _)| *name == path) else {
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
}
