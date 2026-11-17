//! GUI settings persistence (app data dir, 0600 on unix).
//!
//! First launch seeds defaults from the LH_* environment, writes them to
//! `settings.json`, and from then on the file wins (the env public key is
//! still honored as a fallback when the stored key is empty).
//!
//! New fields (`capabilities`, `schedule`) carry `serde(default)` so a
//! settings.json written by an older build still loads — the old file simply
//! gets today's defaults for the fields it never stored.

use lh_contributor_client::api::ChunkType;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

pub const DEFAULT_COORDINATOR: &str = "https://cap.landhunter.app";

/// Validation failure tied to a settings field. Serialized into the webview
/// rejection so the dialog can render the message under the exact control
/// instead of a generic banner.
#[derive(Debug, Clone, Serialize)]
pub struct FieldError {
    pub field: &'static str,
    pub message: String,
}

impl FieldError {
    pub fn new(field: &'static str, message: impl Into<String>) -> FieldError {
        FieldError {
            field,
            message: message.into(),
        }
    }
}

impl From<FieldError> for String {
    fn from(e: FieldError) -> String {
        e.message
    }
}

/// Per-chunk-type capability toggles. A disabled type is never offered to
/// the coordinator on lease polls (`RunOptions::capabilities` carries only
/// the enabled ones). Registration still advertises all five types (the
/// client lib's `register_request()` is fixed) — the effective gate is the
/// lease poll capability list.
///
/// `classify` (search-result grading) is not a free toggle: it mirrors the
/// Local Model mode (`GuiSettings::normalize_local_model` keeps
/// `classify == (mode != None)`), because offering classify without a
/// healthy LLM lane would be a capability the client cannot execute.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    pub examination: bool,
    pub ingest: bool,
    pub alt_source: bool,
    pub state_walk: bool,
    pub search: bool,
    #[serde(default)]
    pub classify: bool,
}

impl Default for Capabilities {
    fn default() -> Self {
        Capabilities {
            examination: true,
            ingest: true,
            alt_source: true,
            state_walk: true,
            search: true,
            // Legacy settings.json files (and fresh installs) predate the
            // Local Model card: classify stays off until a model mode is
            // picked. This is what keeps the disabled path byte-identical
            // to the pre-change app.
            classify: false,
        }
    }
}

impl Capabilities {
    /// Enabled types in canonical order (matches `ALL_CHUNK_TYPES`).
    pub fn enabled_list(&self) -> Vec<ChunkType> {
        let flags = [
            (ChunkType::Examination, self.examination),
            (ChunkType::Ingest, self.ingest),
            (ChunkType::AltSource, self.alt_source),
            (ChunkType::StateWalk, self.state_walk),
            (ChunkType::Search, self.search),
            (ChunkType::Classify, self.classify),
        ];
        flags
            .into_iter()
            .filter_map(|(t, on)| if on { Some(t) } else { None })
            .collect()
    }

    pub fn any_enabled(&self) -> bool {
        self.examination
            || self.ingest
            || self.alt_source
            || self.state_walk
            || self.search
            || self.classify
    }

    /// Seed from `LH_CAPABILITIES` — comma list of type names. Unknown
    /// tokens are rejected so a typo doesn't silently shrink the set.
    /// `classify` is accepted here (the Local Model card landed); the
    /// mode/classify invariant is re-applied when settings load.
    fn from_csv(csv: &str) -> Option<Capabilities> {
        let mut c = Capabilities {
            examination: false,
            ingest: false,
            alt_source: false,
            state_walk: false,
            search: false,
            classify: false,
        };
        let mut seen = false;
        for token in csv.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            match ChunkType::parse(token) {
                Some(ChunkType::Examination) => c.examination = true,
                Some(ChunkType::Ingest) => c.ingest = true,
                Some(ChunkType::AltSource) => c.alt_source = true,
                Some(ChunkType::StateWalk) => c.state_walk = true,
                Some(ChunkType::Search) => c.search = true,
                Some(ChunkType::Classify) => c.classify = true,
                None => return None,
            }
            seen = true;
        }
        if seen {
            Some(c)
        } else {
            None
        }
    }
}

/// Optional run-window schedule. Disabled = "always active" (the default).
/// The window is [active_from, active_until) in local wall-clock time;
/// `from > to` wraps past midnight (e.g. 22:00→06:00).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleCfg {
    pub enabled: bool,
    pub active_from: String,
    pub active_until: String,
}

impl Default for ScheduleCfg {
    fn default() -> Self {
        ScheduleCfg {
            enabled: false,
            active_from: "09:00".to_string(),
            active_until: "17:00".to_string(),
        }
    }
}

/// Local-model grading mode (Settings ▸ Local Model). Three modes collapse
/// onto one SDK config surface (`Config.llm`) + capability declaration:
/// - `None` (default): no LLM anywhere; zero sidecar code paths touched.
/// - `Auto`: the bundled GGUF + llama-server sidecar this card builds.
/// - `Custom`: volunteer-provided OpenAI-compatible endpoint (their own
///   local server, LAN, or a cloud provider with an API key).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LlmMode {
    #[default]
    None,
    Auto,
    Custom,
}

impl LlmMode {
    pub fn as_str(self) -> &'static str {
        match self {
            LlmMode::None => "none",
            LlmMode::Auto => "auto",
            LlmMode::Custom => "custom",
        }
    }

    pub fn parse(s: &str) -> Option<LlmMode> {
        match s {
            "none" => Some(LlmMode::None),
            "auto" => Some(LlmMode::Auto),
            "custom" => Some(LlmMode::Custom),
            _ => None,
        }
    }
}

/// Sidecar resource cap → llama-server launch flags (`--n-gpu-layers`,
/// `--threads`; CPU caps use `max(1, floor(cores/3))` and `max(1,
/// floor(cores/2))`). Only meaningful in Auto mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResourceCap {
    Gpu,
    #[default]
    CpuThird,
    CpuHalf,
}

impl ResourceCap {
    pub fn as_str(self) -> &'static str {
        match self {
            ResourceCap::Gpu => "gpu",
            ResourceCap::CpuThird => "cpu_third",
            ResourceCap::CpuHalf => "cpu_half",
        }
    }

    pub fn parse(s: &str) -> Option<ResourceCap> {
        match s {
            "gpu" => Some(ResourceCap::Gpu),
            "cpu_third" => Some(ResourceCap::CpuThird),
            "cpu_half" => Some(ResourceCap::CpuHalf),
            _ => None,
        }
    }
}

/// The persisted Local Model settings. The API key is deliberately NOT a
/// field: it lives in its own 0600 `llm_api_key.json` (same protection
/// class as credentials.json) and is only ever sent to `endpoint`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalModelCfg {
    #[serde(default)]
    pub mode: LlmMode,
    #[serde(default)]
    pub cap: ResourceCap,
    /// Auto mode: which bundled model file the sidecar steers — a
    /// sidecar::bundled_models id. "" normalizes to the pinned default
    /// (normalize_local_model); the default stays the small 2B-class file
    /// and the 9B choice (t_10daf382 "Can I AI?" tier-1 suggestion) is
    /// strictly opt-in. Unknown ids never survive normalization — the
    /// sidecar can only ever be pointed at a pinned ModelSpec.
    #[serde(default)]
    pub model_id: String,
    /// "" = default sidecar endpoint (http://127.0.0.1:8081/v1). In Auto
    /// mode this may only retarget the sidecar to another loopback port.
    #[serde(default)]
    pub endpoint: String,
    /// Optional explicit llama-server binary path (Auto mode). Empty = PATH
    /// search.
    #[serde(default)]
    pub llama_server_bin: String,
    /// Custom mode: model slug for chat/completions (llama-server ignores
    /// it; cloud providers need the exact name) and the model_family echo
    /// in the capability declaration.
    #[serde(default)]
    pub model_name: String,
    /// Context window handed to llama-server (`--ctx-size`) / declared in
    /// the capability. Kept small on purpose — this is a grading sidecar.
    #[serde(default = "default_ctx_tokens")]
    pub ctx_tokens: u32,
}

pub fn default_ctx_tokens() -> u32 {
    4096
}

/// Sanity bounds on the declared/served context: the point of the sidecar
/// is a SMALL ctx (grading a result set, not hosting a chat).
pub const MIN_CTX_TOKENS: u32 = 1024;
pub const MAX_CTX_TOKENS: u32 = 32_768;

impl Default for LocalModelCfg {
    fn default() -> Self {
        LocalModelCfg {
            mode: LlmMode::None,
            cap: ResourceCap::CpuThird,
            model_id: String::new(),
            endpoint: String::new(),
            llama_server_bin: String::new(),
            model_name: String::new(),
            ctx_tokens: default_ctx_tokens(),
        }
    }
}

/// Strict `HH:MM` (zero-padded 24-hour) → minutes since local midnight.
pub fn parse_hhmm(s: &str) -> Option<u32> {
    let b = s.as_bytes();
    if b.len() != 5 || b[2] != b':' {
        return None;
    }
    let dig = |i: usize| b[i].is_ascii_digit();
    if !dig(0) || !dig(1) || !dig(3) || !dig(4) {
        return None;
    }
    let h: u32 = ((b[0] - b'0') * 10 + (b[1] - b'0')).into();
    let m: u32 = ((b[3] - b'0') * 10 + (b[4] - b'0')).into();
    if h > 23 || m > 59 {
        return None;
    }
    Some(h * 60 + m)
}

/// Is `minute_of_day` inside the schedule window? Disabled schedules are
/// always inside; a corrupt stored schedule degrades to always-inside
/// rather than trapping the loop outside its own window (save-time
/// validation rejects it instead, and `from == to` is defensive-inside).
pub fn in_window_minutes(minute_of_day: u32, schedule: &ScheduleCfg) -> bool {
    if !schedule.enabled {
        return true;
    }
    let parsed = (
        parse_hhmm(&schedule.active_from),
        parse_hhmm(&schedule.active_until),
    );
    let (from, to) = match parsed {
        (Some(f), Some(t)) => (f, t),
        _ => return true,
    };
    if from == to {
        return true;
    }
    if from < to {
        minute_of_day >= from && minute_of_day < to
    } else {
        minute_of_day >= from || minute_of_day < to
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuiSettings {
    pub coordinator_url: String,
    /// Stored ed25519 contract key (64 hex). Empty = fall back to
    /// LH_PUBLIC_KEY_HEX at run time.
    pub public_key_hex: String,
    pub concurrency: u32,
    pub dry_run: bool,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub schedule: ScheduleCfg,
    #[serde(default)]
    pub local_model: LocalModelCfg,
    /// Launch-at-login INTENT (Settings checkbox → apply_autostart in
    /// contributor.rs). The OS entry the autostart plugin writes: an HKCU
    /// `Software\Microsoft\Windows\CurrentVersion\Run` value on Windows, a
    /// `~/.config/autostart/<app>.desktop` on Linux (the plugin resolves
    /// the AppImage path), a LaunchAgent plist under
    /// `~/Library/LaunchAgents` on macOS. settings.json stores the human's
    /// intent; save_settings applies OS changes immediately and main.rs
    /// re-points the entry at the running binary on every startup, so the
    /// entry and the installed app can never drift apart.
    /// `serde(default)`: settings.json files written before this field
    /// shipped keep autostart off.
    #[serde(default)]
    pub autostart: bool,
}

impl GuiSettings {
    pub fn defaults_from_env() -> GuiSettings {
        let dry_var = std::env::var("LH_GUI_DRY_RUN").ok();
        let dry_run = match dry_var.as_deref().map(str::trim) {
            Some(v) if !v.is_empty() => !(v == "0" || v.eq_ignore_ascii_case("false")),
            _ => true,
        };
        let capabilities = std::env::var("LH_CAPABILITIES")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .and_then(|v| {
                Capabilities::from_csv(&v).or_else(|| {
                    eprintln!("LH_CAPABILITIES: unrecognized chunk type in '{v}'; using all types");
                    None
                })
            })
            .unwrap_or_default();
        // Local Model seed from the SDK's LH_LLM_* vocabulary so a
        // volunteer who already runs their own server can pre-wire the GUI
        // with env only (mode custom). The bundled-model auto mode is a GUI
        // concept and is not env-selectable on purpose — enabling it should
        // be a deliberate UI action.
        let llm_base = std::env::var("LH_LLM_BASE_URL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        let local_model = match llm_base {
            Some(base) => LocalModelCfg {
                mode: LlmMode::Custom,
                cap: ResourceCap::CpuThird,
                model_id: String::new(),
                endpoint: crate::sidecar::ensure_v1(&base),
                llama_server_bin: String::new(),
                model_name: std::env::var("LH_LLM_MODEL")
                    .ok()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .unwrap_or_default(),
                ctx_tokens: std::env::var("LH_LLM_CTX_TOKENS")
                    .ok()
                    .and_then(|v| v.trim().parse().ok())
                    .filter(|v: &u32| (MIN_CTX_TOKENS..=MAX_CTX_TOKENS).contains(v))
                    .unwrap_or_else(default_ctx_tokens),
            },
            None => LocalModelCfg::default(),
        };
        let mut s = GuiSettings {
            coordinator_url: std::env::var("LH_COORDINATOR_URL")
                .ok()
                .map(|v: String| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| DEFAULT_COORDINATOR.to_string()),
            public_key_hex: std::env::var("LH_PUBLIC_KEY_HEX")
                .map(|v| v.trim().to_string())
                .unwrap_or_default(),
            concurrency: std::env::var("LH_CONCURRENCY")
                .ok()
                .and_then(|v| v.trim().parse::<u32>().ok())
                .unwrap_or(2)
                .clamp(1, 8),
            dry_run,
            capabilities,
            schedule: ScheduleCfg::default(),
            local_model,
            // Launch-at-login is deliberately NOT env-seedable: registering
            // a per-user OS login entry is a human action (the Settings
            // checkbox), same doctrine as Auto model mode being UI-only.
            autostart: false,
        };
        s.normalize_local_model();
        s
    }

    /// Keep the classify capability in lockstep with the Local Model mode:
    /// classify is offered iff a model mode is picked. (A stale hand-edited
    /// settings.json is normalized on load rather than rejected.)
    pub fn normalize_local_model(&mut self) {
        self.capabilities.classify = !matches!(self.local_model.mode, LlmMode::None);
        // The bundled model id canonicalizes against the sidecar's pinned
        // menu: empty or unknown lands on the pinned default, so a
        // hand-edited settings.json can never steer the sidecar toward an
        // unpinned model (sidecar::model_for_id is the only resolver).
        self.local_model.model_id = crate::sidecar::model_for_id(&self.local_model.model_id)
            .map(|spec| spec.id.to_string())
            .unwrap_or_else(|| crate::sidecar::DEFAULT_MODEL.id.to_string());
        let trimmed = self.local_model.endpoint.trim().to_string();
        self.local_model.endpoint = if trimmed.is_empty() {
            String::new()
        } else {
            crate::sidecar::ensure_v1(&trimmed)
        };
    }

    /// Effective contract key: stored value first, env as fallback.
    pub fn resolved_public_key(&self) -> String {
        let stored = self.public_key_hex.trim();
        if !stored.is_empty() {
            return stored.to_string();
        }
        std::env::var("LH_PUBLIC_KEY_HEX")
            .unwrap_or_default()
            .trim()
            .to_string()
    }

    /// Whole-settings validation (field-keyed). `save_settings` applies a
    /// partial input to the current state, then validates the merged result
    /// so a partial update can never leave an invalid combination behind
    /// (e.g. zero capabilities, or a schedule window with a corrupt time).
    pub fn validate(&self) -> Result<(), FieldError> {
        let url = self.coordinator_url.trim();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(FieldError::new(
                "coordinator_url",
                "coordinator URL must start with http:// or https://",
            ));
        }
        let key = self.public_key_hex.trim();
        if !key.is_empty() && (key.len() != 64 || !key.chars().all(|c| c.is_ascii_hexdigit())) {
            return Err(FieldError::new(
                "public_key_hex",
                "public key must be 64 hex characters (32-byte ed25519)",
            ));
        }
        if !(1..=8).contains(&self.concurrency) {
            return Err(FieldError::new(
                "concurrency",
                "concurrency must be between 1 and 8",
            ));
        }
        if !self.capabilities.any_enabled() {
            return Err(FieldError::new(
                "capabilities",
                "enable at least one chunk type — the run loop refuses to start with an empty capability set",
            ));
        }
        if self.schedule.enabled {
            let from = parse_hhmm(self.schedule.active_from.trim());
            let to = parse_hhmm(self.schedule.active_until.trim());
            let bad_time = |which: &str, bad: &str| {
                FieldError::new(
                    "schedule",
                    format!(
                        "active-{which} '{bad}' is not a valid time — use HH:MM 24-hour (use Always active for no window)"
                    ),
                )
            };
            match (from, to) {
                (Some(f), Some(t)) if f == t => {
                    return Err(FieldError::new(
                        "schedule",
                        "active-from and active-until must differ — or switch back to Always active",
                    ));
                }
                (Some(_), Some(_)) => {}
                (Some(_), None) => {
                    return Err(bad_time("until", &self.schedule.active_until));
                }
                _ => {
                    return Err(bad_time("from", &self.schedule.active_from));
                }
            }
        }
        if !(MIN_CTX_TOKENS..=MAX_CTX_TOKENS).contains(&self.local_model.ctx_tokens) {
            return Err(FieldError::new(
                "local_model",
                format!(
                    "context size must be between {MIN_CTX_TOKENS} and {MAX_CTX_TOKENS} tokens (default {})",
                    default_ctx_tokens()
                ),
            ));
        }
        match self.local_model.mode {
            LlmMode::None => {}
            LlmMode::Auto => {
                // Auto mode runs the sidecar on this machine: an endpoint
                // override may only retarget it to another loopback port.
                let e = self.local_model.endpoint.trim();
                if !e.is_empty() && !crate::sidecar::is_loopback_url(e) {
                    return Err(FieldError::new(
                        "local_model",
                        "auto-configure runs a sidecar on THIS machine — the endpoint override must be a 127.0.0.1/localhost http URL. Pick Custom endpoint to use another server.",
                    ));
                }
            }
            LlmMode::Custom => {
                let e = self.local_model.endpoint.trim();
                if e.is_empty() {
                    return Err(FieldError::new(
                        "local_model",
                        "custom endpoint mode needs a base URL (e.g. http://127.0.0.1:1234/v1 for LM Studio, or your provider's https URL)",
                    ));
                }
                if !e.starts_with("http://") && !e.starts_with("https://") {
                    return Err(FieldError::new(
                        "local_model",
                        "endpoint must start with http:// or https://",
                    ));
                }
                if url::Url::parse(e).is_err() {
                    return Err(FieldError::new("local_model", "endpoint is not a valid URL"));
                }
            }
        }
        // Invariant: classify follows the model mode. merge_settings keeps
        // them aligned; this catches hand-edited settings.json drift.
        // (Drift = classify is ON exactly when the mode is None.)
        if self.capabilities.classify == matches!(self.local_model.mode, LlmMode::None) {
            return Err(FieldError::new(
                "capabilities",
                "the search-grading (classify) capability follows the Local Model setting — pick a model mode or switch the model off",
            ));
        }
        Ok(())
    }
}

pub fn settings_path(dir: &Path) -> std::path::PathBuf {
    dir.join("settings.json")
}

pub fn load_or_create(dir: &Path) -> Result<GuiSettings, String> {
    let path = settings_path(dir);
    if let Ok(text) = fs::read_to_string(&path) {
        match serde_json::from_str::<GuiSettings>(&text) {
            Ok(mut s) => {
                // Normalize before anything reads the settings: an older
                // file (no local_model / classify) or a hand-edited one
                // (classify without a model mode) lands consistent.
                s.normalize_local_model();
                return Ok(s);
            }
            Err(e) => eprintln!("settings.json corrupt ({e}); writing defaults"),
        }
    }
    let s = GuiSettings::defaults_from_env();
    write(dir, &s)?;
    Ok(s)
}

pub fn write(dir: &Path, s: &GuiSettings) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("could not create app data dir: {e}"))?;
    let path = settings_path(dir);
    let json = serde_json::to_string_pretty(s).expect("settings serialize");
    fs::write(&path, json).map_err(|e| format!("could not write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Local Model API key (Custom cloud mode) — separate 0600 credential file
// ---------------------------------------------------------------------------

/// The custom-endpoint API key lives OUT of settings.json, in its own file
/// with the same protection class as credentials.json: never echoed into a
/// settings view, never logged, and only ever sent to `local_model.endpoint`
/// (as a bearer token on chat/completions).
pub fn llm_key_path(dir: &Path) -> std::path::PathBuf {
    dir.join("llm_api_key.json")
}

pub fn read_llm_key(dir: &Path) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct KeyFile {
        api_key: String,
    }
    fs::read_to_string(llm_key_path(dir))
        .ok()
        .and_then(|t| serde_json::from_str::<KeyFile>(&t).ok())
        .map(|k| k.api_key)
        .filter(|k| !k.is_empty())
}

/// Some(non-empty) stores; Some("") removes; None is a no-op.
pub fn write_llm_key(dir: &Path, key: Option<&str>) -> Result<(), String> {
    let path = llm_key_path(dir);
    match key.map(str::trim) {
        None => Ok(()),
        Some("") => {
            if path.exists() {
                fs::remove_file(&path).map_err(|e| format!("could not remove key file: {e}"))?;
            }
            Ok(())
        }
        Some(k) => {
            fs::create_dir_all(dir).map_err(|e| format!("could not create app data dir: {e}"))?;
            let json = serde_json::json!({ "api_key": k }).to_string();
            fs::write(&path, json).map_err(|e| format!("could not write {}: {e}", path.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Cross-session runtime total (runtime.json)
// ---------------------------------------------------------------------------

/// Total contributor-loop runtime summed across every session (app restarts
/// and pause/resume cycles included). The GUI accumulates wall time spent
/// with an unfinished `Runner` and re-seeds from this file on next launch,
/// since the lib's `StatsSnapshot::uptime` is per-Runner.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RuntimeState {
    pub total_runtime_secs: u64,
}

pub fn runtime_path(dir: &Path) -> std::path::PathBuf {
    dir.join("runtime.json")
}

pub fn load_runtime(dir: &Path) -> RuntimeState {
    fs::read_to_string(runtime_path(dir))
        .ok()
        .and_then(|t| serde_json::from_str::<RuntimeState>(&t).ok())
        .unwrap_or_default()
}

pub fn write_runtime(dir: &Path, s: &RuntimeState) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("could not create app data dir: {e}"))?;
    let path = runtime_path(dir);
    let json = serde_json::to_string_pretty(s).expect("runtime state serialize");
    fs::write(&path, json).map_err(|e| format!("could not write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_roundtrip_and_missing_file_defaults() {
        let dir = std::env::temp_dir().join("lhc-gui-runtime-test");
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(load_runtime(&dir).total_runtime_secs, 0);
        write_runtime(&dir, &RuntimeState { total_runtime_secs: 123 }).expect("write");
        assert_eq!(load_runtime(&dir).total_runtime_secs, 123);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_roundtrip_keeps_capabilities_and_schedule() {
        let dir = std::env::temp_dir().join("lhc-gui-settings-test");
        let _ = fs::remove_dir_all(&dir);
        let s = GuiSettings {
            coordinator_url: "http://127.0.0.1:8787".to_string(),
            public_key_hex: "ab".repeat(32),
            concurrency: 5,
            dry_run: true,
            capabilities: Capabilities {
                examination: false,
                ingest: true,
                alt_source: false,
                state_walk: true,
                search: true,
                classify: true,
            },
            schedule: ScheduleCfg {
                enabled: true,
                active_from: "06:30".to_string(),
                active_until: "22:05".to_string(),
            },
            local_model: LocalModelCfg {
                mode: LlmMode::Custom,
                cap: ResourceCap::CpuHalf,
                model_id: crate::sidecar::LARGE_MODEL.id.to_string(),
                endpoint: "http://127.0.0.1:1234/v1".to_string(),
                llama_server_bin: String::new(),
                model_name: "qwen3:8b".to_string(),
                ctx_tokens: 8192,
            },
            // true on purpose: the roundtrip must prove the intent survives
            // write → load, not just the off default.
            autostart: true,
        };
        write(&dir, &s).expect("write settings");
        let loaded = load_or_create(&dir).expect("load");
        assert_eq!(loaded, s);
        // The run-loop-facing mapping survives the roundtrip too.
        assert_eq!(
            loaded.capabilities.enabled_list(),
            vec![ChunkType::Ingest, ChunkType::StateWalk, ChunkType::Search, ChunkType::Classify]
        );
        assert_eq!(loaded.concurrency, 5);
        assert_eq!(loaded.local_model.mode, LlmMode::Custom);
        assert_eq!(loaded.local_model.cap, ResourceCap::CpuHalf);
        assert_eq!(
            loaded.local_model.model_id,
            crate::sidecar::LARGE_MODEL.id,
            "a legitimate bundled id round-trips untouched"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn autostart_defaults_off_and_survives_legacy_files() {
        // Fresh install: launch-at-login is OFF until a human ticks the
        // checkbox — defaults_from_env carries no autostart env seed.
        let dir = std::env::temp_dir().join("lhc-gui-autostart-test");
        let _ = fs::remove_dir_all(&dir);
        assert!(!GuiSettings::defaults_from_env().autostart);

        // The persisted intent round-trips true.
        let mut s = GuiSettings::defaults_from_env();
        s.autostart = true;
        write(&dir, &s).expect("write");
        assert!(load_or_create(&dir).expect("load").autostart);

        // settings.json from a pre-autostart build (field never stored)
        // loads with the entry OFF via serde(default) — no load failure,
        // no surprise enable.
        fs::write(
            settings_path(&dir),
            r#"{"coordinator_url":"https://example.test","public_key_hex":"","concurrency":3,"dry_run":false}"#,
        )
        .expect("seed legacy");
        let legacy = load_or_create(&dir).expect("load legacy");
        assert!(!legacy.autostart);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_settings_json_gets_new_field_defaults() {
        // A settings.json written before capabilities/schedule existed.
        let dir = std::env::temp_dir().join("lhc-gui-settings-legacy-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("dir");
        fs::write(
            settings_path(&dir),
            r#"{"coordinator_url":"https://example.test","public_key_hex":"","concurrency":3,"dry_run":false}"#,
        )
        .expect("seed");
        let s = load_or_create(&dir).expect("load legacy");
        assert_eq!(s.concurrency, 3);
        assert!(!s.dry_run);
        assert!(s.capabilities.any_enabled());
        assert_eq!(
            s.capabilities.enabled_list().len(),
            5,
            "legacy files keep offering all chunk types (no classify)"
        );
        assert!(!s.capabilities.classify, "legacy files never opt into classify");
        assert_eq!(s.local_model.mode, LlmMode::None, "legacy files never enable a model");
        assert_eq!(s.local_model.ctx_tokens, default_ctx_tokens());
        assert!(!s.schedule.enabled, "legacy files stay 'always active'");
        assert!(s.validate().is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hand_edited_classify_drift_is_normalized_on_load() {
        // A file that offers classify without a Local Model mode (hand edit
        // or a downgrade that forgot the flag) is normalized, not rejected.
        let dir = std::env::temp_dir().join("lhc-gui-settings-drift-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("dir");
        fs::write(
            settings_path(&dir),
            r#"{"coordinator_url":"https://example.test","public_key_hex":"","concurrency":2,"dry_run":true,"capabilities":{"examination":true,"ingest":true,"alt_source":true,"state_walk":true,"search":true,"classify":true},"local_model":{"mode":"none","ctx_tokens":4096}}"#,
        )
        .expect("seed");
        let s = load_or_create(&dir).expect("load");
        assert!(!s.capabilities.classify, "normalize_local_model forces classify off");
        assert_eq!(s.capabilities.enabled_list().len(), 5);
        assert!(s.validate().is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn endpoint_is_v1_normalized_on_load() {
        let dir = std::env::temp_dir().join("lhc-gui-settings-v1-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("dir");
        fs::write(
            settings_path(&dir),
            r#"{"coordinator_url":"https://example.test","public_key_hex":"","concurrency":2,"dry_run":true,"local_model":{"mode":"custom","endpoint":"http://127.0.0.1:1234","ctx_tokens":4096}}"#,
        )
        .expect("seed");
        let s = load_or_create(&dir).expect("load");
        assert_eq!(s.local_model.endpoint, "http://127.0.0.1:1234/v1");
        assert!(s.capabilities.classify, "custom mode implies classify");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn llm_api_key_roundtrip_is_0600_and_outside_settings_json() {
        let dir = std::env::temp_dir().join("lhc-gui-llmkey-test");
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(read_llm_key(&dir), None, "absent file → None");
        write_llm_key(&dir, Some("  sk-super-secret  ")).expect("write key");
        assert_eq!(read_llm_key(&dir).as_deref(), Some("sk-super-secret"), "trimmed on store");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(llm_key_path(&dir)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "llm key file carries credentials-class permissions");
        }
        // The key must never sneak into settings.json.
        let s = GuiSettings::defaults_from_env();
        write(&dir, &s).expect("write settings");
        let text = fs::read_to_string(settings_path(&dir)).expect("read");
        assert!(!text.contains("sk-super-secret"));
        // Some("") removes; None is a no-op.
        write_llm_key(&dir, None).expect("noop");
        assert!(read_llm_key(&dir).is_some());
        write_llm_key(&dir, Some("")).expect("clear");
        assert_eq!(read_llm_key(&dir), None);
        assert!(!llm_key_path(&dir).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn validation_pinpoints_the_bad_field() {
        let base = GuiSettings {
            coordinator_url: "https://ok.test".to_string(),
            public_key_hex: String::new(),
            concurrency: 2,
            dry_run: true,
            capabilities: Capabilities::default(),
            schedule: ScheduleCfg::default(),
            local_model: LocalModelCfg::default(),
            autostart: false,
        };
        assert!(base.clone().validate().is_ok());

        let mut no_caps = base.clone();
        no_caps.capabilities = Capabilities {
            examination: false,
            ingest: false,
            alt_source: false,
            state_walk: false,
            search: false,
            classify: false,
        };
        let err = no_caps.validate().unwrap_err();
        assert_eq!(err.field, "capabilities");

        let mut bad_time = base.clone();
        bad_time.schedule = ScheduleCfg {
            enabled: true,
            active_from: "9:00".to_string(), // not zero-padded HH:MM
            active_until: "17:00".to_string(),
        };
        let err = bad_time.validate().unwrap_err();
        assert_eq!(err.field, "schedule");

        let mut same_window = base.clone();
        same_window.schedule = ScheduleCfg {
            enabled: true,
            active_from: "09:00".to_string(),
            active_until: "09:00".to_string(),
        };
        assert_eq!(same_window.validate().unwrap_err().field, "schedule");

        let mut bad_url = base.clone();
        bad_url.coordinator_url = "cap.landhunter.app".to_string();
        assert_eq!(bad_url.validate().unwrap_err().field, "coordinator_url");

        let mut bad_key = base.clone();
        bad_key.public_key_hex = "deadbeef".to_string();
        assert_eq!(bad_key.validate().unwrap_err().field, "public_key_hex");

        let mut bad_conc = base.clone();
        bad_conc.concurrency = 9;
        assert_eq!(bad_conc.validate().unwrap_err().field, "concurrency");
    }

    #[test]
    fn window_membership_boundaries_and_wrap() {
        let day = ScheduleCfg {
            enabled: true,
            active_from: "09:00".to_string(),
            active_until: "17:00".to_string(),
        };
        assert!(!in_window_minutes(539, &day), "08:59 outside");
        assert!(in_window_minutes(540, &day), "09:00 is the first active minute");
        assert!(in_window_minutes(1019, &day), "16:59 inside");
        assert!(!in_window_minutes(1020, &day), "17:00 is the first inactive minute");

        let wrap = ScheduleCfg {
            enabled: true,
            active_from: "22:00".to_string(),
            active_until: "06:00".to_string(),
        };
        assert!(in_window_minutes(1439, &wrap), "23:59 inside a wrap-around window");
        assert!(in_window_minutes(180, &wrap), "03:00 inside a wrap-around window");
        assert!(!in_window_minutes(720, &wrap), "12:00 outside a wrap-around window");

        let off = ScheduleCfg::default();
        assert!(in_window_minutes(3, &off), "disabled schedule = always active");
        // Corrupt stored values degrade to always-active rather than trapping
        // the loop outside its own window.
        let corrupt = ScheduleCfg {
            enabled: true,
            active_from: "25:00".to_string(),
            active_until: "07:61".to_string(),
        };
        assert!(in_window_minutes(120, &corrupt));
    }

    #[test]
    fn capability_csv_seed() {
        let c = Capabilities::from_csv("ingest, alt_source ,search").expect("parse");
        assert_eq!(
            c.enabled_list(),
            vec![ChunkType::Ingest, ChunkType::AltSource, ChunkType::Search]
        );
        assert!(
            Capabilities::from_csv("ingest,banana").is_none(),
            "typos must not silently shrink the set"
        );
        // classify is now a real token (the Local Model card landed).
        let c2 = Capabilities::from_csv("classify,search").expect("classify parses");
        assert!(c2.classify);
        assert_eq!(
            c2.enabled_list(),
            vec![ChunkType::Search, ChunkType::Classify]
        );
    }

    #[test]
    fn local_model_validation_pinpoints() {
        let base = GuiSettings {
            coordinator_url: "https://ok.test".to_string(),
            public_key_hex: String::new(),
            concurrency: 2,
            dry_run: true,
            capabilities: Capabilities::default(),
            schedule: ScheduleCfg::default(),
            local_model: LocalModelCfg::default(),
            autostart: false,
        };

        // Custom mode without an endpoint is refused.
        let mut no_endpoint = base.clone();
        no_endpoint.local_model.mode = LlmMode::Custom;
        no_endpoint.capabilities.classify = true;
        let err = no_endpoint.validate().unwrap_err();
        assert_eq!(err.field, "local_model");

        // Custom mode with a junk URL is refused.
        let mut junk = base.clone();
        junk.local_model = LocalModelCfg {
            mode: LlmMode::Custom,
            cap: ResourceCap::CpuThird,
            model_id: String::new(),
            endpoint: "127.0.0.1:1234/v1".to_string(),
            llama_server_bin: String::new(),
            model_name: String::new(),
            ctx_tokens: 4096,
        };
        junk.capabilities.classify = true;
        assert_eq!(junk.validate().unwrap_err().field, "local_model");

        // Auto mode may not point the sidecar at a non-loopback host.
        let mut auto_external = base.clone();
        auto_external.local_model = LocalModelCfg {
            mode: LlmMode::Auto,
            cap: ResourceCap::CpuThird,
            model_id: String::new(),
            endpoint: "https://api.some-provider.example/v1".to_string(),
            llama_server_bin: String::new(),
            model_name: String::new(),
            ctx_tokens: 4096,
        };
        auto_external.capabilities.classify = true;
        assert_eq!(auto_external.validate().unwrap_err().field, "local_model");

        // Auto mode pointing at another loopback port is fine (the
        // "volunteers who already run their own" endpoint override).
        let mut auto_loopback = base.clone();
        auto_loopback.local_model = LocalModelCfg {
            mode: LlmMode::Auto,
            cap: ResourceCap::Gpu,
            model_id: String::new(),
            endpoint: "http://127.0.0.1:8090/v1".to_string(),
            llama_server_bin: String::new(),
            model_name: String::new(),
            ctx_tokens: 8192,
        };
        auto_loopback.capabilities.classify = true;
        assert!(auto_loopback.validate().is_ok(), "loopback endpoint override allowed");

        // Context size out of bounds is refused.
        let mut bad_ctx = base.clone();
        bad_ctx.local_model.ctx_tokens = 100;
        assert_eq!(bad_ctx.validate().unwrap_err().field, "local_model");

        // classify on without a model mode (hand-edited file saved back
        // through save_settings) is refused at the capabilities field.
        let mut drift = base.clone();
        drift.capabilities.classify = true;
        let err = drift.validate().unwrap_err();
        assert_eq!(err.field, "capabilities");
    }

    #[test]
    fn bundled_model_id_normalizes_to_the_pinned_menu_at_load_and_save() {
        let dir = std::env::temp_dir().join("lhc-settings-model-id-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // Hand-edited junk id must not survive into the sidecar path.
        fs::write(
            dir.join("settings.json"),
            r#"{
  "coordinator_url": "http://coordinator.test",
  "public_key_hex": "",
  "concurrency": 2,
  "dry_run": true,
  "capabilities": { "examination": true, "ingest": true, "alt_source": false, "state_walk": false, "search": false, "classify": true },
  "schedule": { "enabled": false, "active_from": "06:00", "active_until": "22:00" },
  "local_model": { "mode": "auto", "model_id": "totally-not-a-model" }
}"#,
        )
        .unwrap();
        let loaded = load_or_create(&dir).expect("load");
        assert_eq!(loaded.local_model.model_id, crate::sidecar::DEFAULT_MODEL.id);
        assert!(loaded.capabilities.classify);

        // The opt-in 9B choice survives normalize + write + reload.
        let mut chosen = loaded.clone();
        chosen.local_model.model_id = crate::sidecar::LARGE_MODEL.id.to_string();
        chosen.normalize_local_model();
        assert_eq!(chosen.local_model.model_id, crate::sidecar::LARGE_MODEL.id);
        write(&dir, &chosen).expect("write");
        let reloaded = load_or_create(&dir).expect("reload");
        assert_eq!(reloaded.local_model.model_id, crate::sidecar::LARGE_MODEL.id);

        // A legacy settings.json without the field at all gets the pinned
        // default (serde default → canonicalize), never an empty steer.
        let legacy_dir = std::env::temp_dir().join("lhc-settings-model-id-legacy");
        let _ = fs::remove_dir_all(&legacy_dir);
        fs::create_dir_all(&legacy_dir).unwrap();
        fs::write(
            legacy_dir.join("settings.json"),
            r#"{
  "coordinator_url": "http://coordinator.test",
  "public_key_hex": "",
  "concurrency": 2,
  "dry_run": true,
  "capabilities": { "examination": true, "ingest": true, "alt_source": false, "state_walk": false, "search": false, "classify": false },
  "schedule": { "enabled": false, "active_from": "06:00", "active_until": "22:00" },
  "local_model": { "mode": "custom", "endpoint": "http://127.0.0.1:1234/v1" }
}"#,
        )
        .unwrap();
        let legacy = load_or_create(&legacy_dir).expect("legacy load");
        assert_eq!(
            legacy.local_model.model_id,
            crate::sidecar::DEFAULT_MODEL.id,
            "missing field is serde-defaulted then canonicalized"
        );
        assert_eq!(legacy.local_model.mode, LlmMode::Custom);

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&legacy_dir);
    }
}
