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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    pub examination: bool,
    pub ingest: bool,
    pub alt_source: bool,
    pub state_walk: bool,
    pub search: bool,
}

impl Default for Capabilities {
    fn default() -> Self {
        Capabilities {
            examination: true,
            ingest: true,
            alt_source: true,
            state_walk: true,
            search: true,
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
        ];
        flags
            .into_iter()
            .filter_map(|(t, on)| if on { Some(t) } else { None })
            .collect()
    }

    pub fn any_enabled(&self) -> bool {
        self.examination || self.ingest || self.alt_source || self.state_walk || self.search
    }

    /// Seed from `LH_CAPABILITIES` — comma list of type names. Unknown
    /// tokens are rejected so a typo doesn't silently shrink the set.
    fn from_csv(csv: &str) -> Option<Capabilities> {
        let mut c = Capabilities {
            examination: false,
            ingest: false,
            alt_source: false,
            state_walk: false,
            search: false,
        };
        let mut seen = false;
        for token in csv.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            match ChunkType::parse(token) {
                Some(ChunkType::Examination) => c.examination = true,
                Some(ChunkType::Ingest) => c.ingest = true,
                Some(ChunkType::AltSource) => c.alt_source = true,
                Some(ChunkType::StateWalk) => c.state_walk = true,
                Some(ChunkType::Search) => c.search = true,
                // classify exists in the SDK but has no toggle in this build
                // yet (the Local Model card adds it). Treat it as unknown so
                // a stale/future capabilities string fails loudly instead of
                // silently enabling work this GUI cannot execute.
                Some(ChunkType::Classify) | None => return None,
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
        GuiSettings {
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
        }
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
            Ok(s) => return Ok(s),
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
            },
            schedule: ScheduleCfg {
                enabled: true,
                active_from: "06:30".to_string(),
                active_until: "22:05".to_string(),
            },
        };
        write(&dir, &s).expect("write settings");
        let loaded = load_or_create(&dir).expect("load");
        assert_eq!(loaded, s);
        // The run-loop-facing mapping survives the roundtrip too.
        assert_eq!(
            loaded.capabilities.enabled_list(),
            vec![ChunkType::Ingest, ChunkType::StateWalk, ChunkType::Search]
        );
        assert_eq!(loaded.concurrency, 5);
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
            "legacy files keep offering all chunk types"
        );
        assert!(!s.schedule.enabled, "legacy files stay 'always active'");
        assert!(s.validate().is_ok());
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
        };
        assert!(base.clone().validate().is_ok());

        let mut no_caps = base.clone();
        no_caps.capabilities = Capabilities {
            examination: false,
            ingest: false,
            alt_source: false,
            state_walk: false,
            search: false,
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
    }
}
