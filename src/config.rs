//! Environment-driven configuration + credential persistence.

use crate::api::Credentials;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("{var} is required (set it in the environment)")]
    MissingEnv { var: &'static str },

    #[error("{var}: {problem}")]
    BadEnv { var: &'static str, problem: String },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("credentials file at {path} is corrupt: {problem}")]
    BadCredentials { path: PathBuf, problem: String },
}

/// Where uploaded blobs go.
#[derive(Debug, Clone)]
pub enum StagingConfig {
    /// PUT to a presigned URL template; `{key}` is replaced with the
    /// URI-encoded blob key. Use when a coordinator/operator hands out
    /// presigned R2 URLs.
    PresignTemplate { url_template: String },
    /// Direct upload to Cloudflare R2's S3-compatible API (SigV4-signed).
    R2 {
        endpoint: String,
        bucket: String,
        access_key_id: String,
        secret_access_key: String,
    },
    /// Write blobs under a local directory (offline inspection / dry runs).
    Local { root: PathBuf },
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Coordinator base URL, e.g. `https://lh-tracker.example.workers.dev`.
    pub coordinator_url: String,
    /// Operator ed25519 public key (64 hex chars = raw 32 bytes).
    pub public_key_hex: String,
    /// Saved identity; absent until first register.
    pub credentials: Option<Credentials>,
    /// Parallel slots, 1..=8 (server schema caps slot_id at 7).
    pub concurrency: u32,
    pub work_dir: PathBuf,
    /// Optional client-side cap on source politeness (requests/min,
    /// bytes/min); the effective budget is the min of this and the lease's
    /// contract budget.
    pub rate_limit: Option<(u64, u64)>,
    pub staging: StagingConfig,
    /// Brave Search API key for `search` chunks with backend=brave.
    pub brave_api_key: Option<String>,
    /// Per-request timeout for coordinator + source HTTP calls.
    pub http_timeout_secs: u64,
}

impl Config {
    /// Load from LH_* environment variables, merging saved credentials from
    /// `{work_dir}/credentials.json` when LH_CLIENT_ID/LH_AUTH_TOKEN are
    /// absent.
    pub fn from_env() -> Result<Config, ConfigError> {
        let coordinator_url = env_required("LH_COORDINATOR_URL")?;
        let public_key_hex = env_required("LH_PUBLIC_KEY_HEX")?;
        validate_public_key(&public_key_hex).map_err(|problem| ConfigError::BadEnv {
            var: "LH_PUBLIC_KEY_HEX",
            problem,
        })?;

        let concurrency = match std::env::var("LH_CONCURRENCY") {
            Ok(v) => v
                .trim()
                .parse::<u32>()
                .map_err(|_| ConfigError::BadEnv {
                    var: "LH_CONCURRENCY",
                    problem: format!("{v:?} is not a number"),
                })?
                .clamp(1, 8),
            Err(_) => 2,
        };

        let work_dir = PathBuf::from(
            std::env::var("LH_WORK_DIR").unwrap_or_else(|_| "./lh-work".to_string()),
        );

        let credentials = load_credentials(&work_dir)?;

        let rate_limit = match std::env::var("LH_RATE_LIMIT") {
            Ok(v) => {
                let parts: Vec<&str> = v.split(',').map(str::trim).collect();
                if parts.len() != 2 {
                    return Err(ConfigError::BadEnv {
                        var: "LH_RATE_LIMIT",
                        problem: format!("expected \"requests/min,bytes/min\", got {v:?}"),
                    });
                }
                let rpm = parts[0].parse::<u64>().map_err(|_| ConfigError::BadEnv {
                    var: "LH_RATE_LIMIT",
                    problem: format!("{:?} is not a number", parts[0]),
                })?;
                let bpm = parts[1].parse::<u64>().map_err(|_| ConfigError::BadEnv {
                    var: "LH_RATE_LIMIT",
                    problem: format!("{:?} is not a number", parts[1]),
                })?;
                Some((rpm, bpm))
            }
            Err(_) => None,
        };

        let staging = staging_from_env(&work_dir)?;
        let brave_api_key = std::env::var("LH_BRAVE_API_KEY").ok().filter(|s| !s.is_empty());
        let http_timeout_secs = std::env::var("LH_HTTP_TIMEOUT_SEC")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(60);

        Ok(Config {
            coordinator_url,
            public_key_hex,
            credentials,
            concurrency,
            work_dir,
            rate_limit,
            staging,
            brave_api_key,
            http_timeout_secs,
        })
    }

    /// Path of the persisted credentials file.
    pub fn credentials_path(&self) -> PathBuf {
        self.work_dir.join("credentials.json")
    }
}

fn env_required(var: &'static str) -> Result<String, ConfigError> {
    match std::env::var(var) {
        Ok(v) if !v.trim().is_empty() => Ok(v.trim().to_string()),
        _ => Err(ConfigError::MissingEnv { var }),
    }
}

fn validate_public_key(hex_str: &str) -> Result<(), String> {
    let bytes = hex::decode(hex_str).map_err(|e| format!("not hex: {e}"))?;
    if bytes.len() != 32 {
        return Err(format!("expected 64 hex chars (32 bytes), got {}", bytes.len() * 2));
    }
    Ok(())
}

fn load_credentials(work_dir: &Path) -> Result<Option<Credentials>, ConfigError> {
    if let (Ok(id), Ok(token)) = (
        std::env::var("LH_CLIENT_ID"),
        std::env::var("LH_AUTH_TOKEN"),
    ) {
        if !id.trim().is_empty() && !token.trim().is_empty() {
            return Ok(Some(Credentials {
                client_id: id.trim().to_string(),
                auth_token: token.trim().to_string(),
            }));
        }
    }
    let path = work_dir.join("credentials.json");
    let Ok(text) = fs::read_to_string(&path) else {
        return Ok(None);
    };
    let creds: Credentials = serde_json::from_str(&text).map_err(|e| ConfigError::BadCredentials {
        path,
        problem: e.to_string(),
    })?;
    Ok(Some(creds))
}

/// Save freshly-minted credentials to `{work_dir}/credentials.json` with
/// 0600 permissions (unix; best effort elsewhere).
pub fn save_credentials(work_dir: &Path, creds: &Credentials) -> Result<PathBuf, ConfigError> {
    fs::create_dir_all(work_dir)?;
    let path = work_dir.join("credentials.json");
    let json = serde_json::to_string_pretty(creds).expect("credentials serialize");
    fs::write(&path, json)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Ok(path)
}

fn staging_from_env(work_dir: &Path) -> Result<StagingConfig, ConfigError> {
    // 1. Presign template wins if present.
    if let Ok(t) = std::env::var("LH_UPLOAD_PRESIGN_TEMPLATE") {
        if !t.trim().is_empty() {
            if !t.contains("{key}") {
                return Err(ConfigError::BadEnv {
                    var: "LH_UPLOAD_PRESIGN_TEMPLATE",
                    problem: "template must contain a {key} placeholder".to_string(),
                });
            }
            return Ok(StagingConfig::PresignTemplate {
                url_template: t.trim().to_string(),
            });
        }
    }
    // 2. Direct R2 (S3 API) when bucket + keys are configured.
    let bucket = std::env::var("LH_R2_BUCKET").ok().filter(|s| !s.is_empty());
    let ak = std::env::var("LH_R2_ACCESS_KEY_ID").ok().filter(|s| !s.is_empty());
    let sk = std::env::var("LH_R2_SECRET_ACCESS_KEY")
        .ok()
        .filter(|s| !s.is_empty());
    if let (Some(bucket), Some(ak), Some(sk)) = (bucket, ak, sk) {
        let endpoint = match std::env::var("LH_R2_ENDPOINT") {
            Ok(e) if !e.trim().is_empty() => e.trim().to_string(),
            _ => {
                let account =
                    std::env::var("LH_R2_ACCOUNT_ID").map_err(|_| ConfigError::MissingEnv {
                        var: "LH_R2_ACCOUNT_ID (or LH_R2_ENDPOINT)",
                    })?;
                if account.trim().is_empty() {
                    return Err(ConfigError::MissingEnv {
                        var: "LH_R2_ACCOUNT_ID (or LH_R2_ENDPOINT)",
                    });
                }
                format!(
                    "https://{}.r2.cloudflarestorage.com",
                    account.trim()
                )
            }
        };
        return Ok(StagingConfig::R2 {
            endpoint,
            bucket,
            access_key_id: ak,
            secret_access_key: sk,
        });
    }
    // 3. Local staging root (the default; used by dry runs and offline
    // tests). Lives under the work dir so blobs land next to credentials
    // and manifests, regardless of the process cwd. Real runs require the
    // PresignTemplate or R2 config above and refuse Local staging.
    Ok(StagingConfig::Local {
        root: work_dir.join("staging"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_key_validation_rejects_bad_hex() {
        assert!(validate_public_key(&"ab".repeat(32)).is_ok());
        assert!(validate_public_key("zz").is_err());
        assert!(validate_public_key(&"ab".repeat(31)).is_err());
    }
}
