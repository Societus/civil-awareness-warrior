//! GUI settings persistence (app data dir, 0600 on unix).
//!
//! First launch seeds defaults from the LH_* environment, writes them to
//! `settings.json`, and from then on the file wins (the env public key is
//! still honored as a fallback when the stored key is empty).

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

pub const DEFAULT_COORDINATOR: &str = "https://cap.landhunter.app";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuiSettings {
    pub coordinator_url: String,
    /// Stored ed25519 contract key (64 hex). Empty = fall back to
    /// LH_PUBLIC_KEY_HEX at run time.
    pub public_key_hex: String,
    pub concurrency: u32,
    pub dry_run: bool,
}

impl GuiSettings {
    pub fn defaults_from_env() -> GuiSettings {
        let dry_var = std::env::var("LH_GUI_DRY_RUN").ok();
        let dry_run = match dry_var.as_deref().map(str::trim) {
            Some(v) if !v.is_empty() => !(v == "0" || v.eq_ignore_ascii_case("false")),
            _ => true,
        };
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
