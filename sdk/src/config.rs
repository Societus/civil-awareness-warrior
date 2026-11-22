//! Environment-driven configuration + credential persistence.

use crate::api::{Credentials, LlmCapability, LlmEndpointKind};
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

/// Local-LLM grading lane configuration (classify chunks). Active only when
/// the capability fields are present in the environment; absent config means
/// the client never declares the llm capability and simply never receives
/// classify work (all other chunk types unaffected).
///
/// Collapses the GUI's three modes onto one surface: auto-configured
/// sidecar (base_url = local llama-server), custom local endpoint, or a
/// cloud OpenAI-compatible provider (base_url + api_key).
///
/// `api_key` is a credential: `Debug` is deliberately NOT derived so it can
/// never be formatted by a careless `{:?}`/log path.
#[derive(Clone)]
pub struct LlmConfig {
    /// OpenAI-compatible base URL (…/v1). Default http://127.0.0.1:8081/v1
    /// (the llama-server sidecar port).
    pub base_url: String,
    /// Optional bearer key for the endpoint (cloud providers). Stored like
    /// every credential: never logged, sent only to this endpoint.
    pub api_key: Option<String>,
    /// Model name for chat/completions. Free-form; llama-server ignores it,
    /// cloud providers need their exact slug.
    pub model: String,
    /// Per-completion timeout. Local CPU inference on long prompts is slow;
    /// the default is deliberately generous.
    pub timeout_secs: u64,
    /// What register declares. Presence of the capability is what activates
    /// the lane end-to-end.
    pub capability: LlmCapability,
}

pub const DEFAULT_LLM_BASE_URL: &str = "http://127.0.0.1:8081/v1";
const DEFAULT_LLM_MODEL: &str = "landhunter-classifier";
const DEFAULT_LLM_TIMEOUT_SECS: u64 = 240;

/// Load the LLM lane from LH_LLM_* variables. Ok(None) — lane disabled —
/// unless BOTH LH_LLM_MODEL_FAMILY and LH_LLM_CTX_TOKENS are set (the two
/// fields a capability declaration cannot default). Everything else has
/// defaults; invalid explicit values are errors, not silent fallbacks.
fn llm_from_env() -> Result<Option<LlmConfig>, ConfigError> {
    let model_family = env_nonempty("LH_LLM_MODEL_FAMILY");
    let ctx_tokens = match std::env::var("LH_LLM_CTX_TOKENS") {
        Ok(v) if !v.trim().is_empty() => {
            Some(v.trim().parse::<u32>().map_err(|_| ConfigError::BadEnv {
                var: "LH_LLM_CTX_TOKENS",
                problem: format!("{v:?} is not a number"),
            })?)
        }
        _ => None,
    };
    let (model_family, ctx_tokens) = match (model_family, ctx_tokens) {
        (None, None) => return Ok(None),
        (Some(f), Some(c)) => (f, c),
        (Some(_), None) | (None, Some(_)) => {
            return Err(ConfigError::BadEnv {
                var: "LH_LLM_MODEL_FAMILY + LH_LLM_CTX_TOKENS",
                problem: "both are required to enable the LLM lane (set both or neither)"
                    .to_string(),
            });
        }
    };

    let endpoint_kind = match std::env::var("LH_LLM_ENDPOINT_KIND") {
        Ok(v) if !v.trim().is_empty() => {
            LlmEndpointKind::parse(v.trim()).ok_or(ConfigError::BadEnv {
                var: "LH_LLM_ENDPOINT_KIND",
                problem: format!("{v:?} is not one of local-sidecar | custom-local | cloud"),
            })?
        }
        _ => LlmEndpointKind::LocalSidecar,
    };
    let timeout_secs = std::env::var("LH_LLM_TIMEOUT_SEC")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_LLM_TIMEOUT_SECS);

    Ok(Some(LlmConfig {
        base_url: std::env::var("LH_LLM_BASE_URL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_LLM_BASE_URL.to_string()),
        api_key: std::env::var("LH_LLM_API_KEY")
            .ok()
            .filter(|s| !s.is_empty()),
        model: std::env::var("LH_LLM_MODEL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_LLM_MODEL.to_string()),
        timeout_secs,
        capability: LlmCapability {
            model_family,
            quant: std::env::var("LH_LLM_QUANT").ok().filter(|s| !s.is_empty()),
            ctx_tokens,
            endpoint_kind,
        },
    }))
}

fn env_nonempty(var: &'static str) -> Option<String> {
    std::env::var(var)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Where uploaded blobs go.
///
/// `R2::secret_access_key` is a credential: `Debug` is deliberately NOT
/// derived so it can never be formatted by a careless `{:?}`/log path.
#[derive(Clone)]
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

/// The persisted client configuration.
///
/// `credentials` and `brave_api_key` are credentials: `Debug` is deliberately
/// NOT derived so they can never be formatted by a careless `{:?}`/log path.
#[derive(Clone)]
pub struct Config {
    /// Coordinator base URL, e.g. `https://lh-tracker.example.workers.dev`.
    pub coordinator_url: String,
    /// Operator ed25519 public key (64 hex chars = raw 32 bytes).
    pub public_key_hex: String,
    /// The compile-time contract key the EMBEDDING app pins (the GUI's
    /// PUBLIC_KEY_HEX_DEFAULT), set only when the effective
    /// `public_key_hex` is a stored override. Some(pin) where
    /// public_key_hex != pin is the RT2 F2 stale-override shape: the
    /// runner's contract-verification abort then points the volunteer at
    /// the stored key ("clear it under Settings") instead of the
    /// LH_PUBLIC_KEY_HEX env var, which was never the culprit. CLI /
    /// from_env configs carry None and keep the generic hint.
    pub pinned_public_key_hex: Option<String>,
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
    /// Local-LLM grading lane; None = no llm capability, no classify work.
    pub llm: Option<LlmConfig>,
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

        let work_dir =
            PathBuf::from(std::env::var("LH_WORK_DIR").unwrap_or_else(|_| "./lh-work".to_string()));

        // Explicit env credentials win over any file (unchanged precedence);
        // otherwise load the credentials bound to THIS coordinator host
        // (RT2 F3a — a legacy single-file fallback is honored only for the
        // historical default URL, never replayed to a different host).
        let credentials = match env_nonempty("LH_CLIENT_ID").zip(env_nonempty("LH_AUTH_TOKEN")) {
            Some((id, token)) => Some(Credentials {
                client_id: id,
                auth_token: token,
            }),
            None => load_credentials_for(&work_dir, &coordinator_url)?,
        };

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
        let brave_api_key = std::env::var("LH_BRAVE_API_KEY")
            .ok()
            .filter(|s| !s.is_empty());
        let http_timeout_secs = std::env::var("LH_HTTP_TIMEOUT_SEC")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(60);
        let llm = llm_from_env()?;

        Ok(Config {
            coordinator_url,
            public_key_hex,
            pinned_public_key_hex: None,
            credentials,
            concurrency,
            work_dir,
            rate_limit,
            staging,
            brave_api_key,
            http_timeout_secs,
            llm,
        })
    }

    /// Path of the credentials file bound to this config's coordinator
    /// (per-host, RT2 F3a).
    pub fn credentials_path(&self) -> PathBuf {
        credentials_file_for(&self.work_dir, &self.coordinator_url)
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
        return Err(format!(
            "expected 64 hex chars (32 bytes), got {}",
            bytes.len() * 2
        ));
    }
    Ok(())
}

/// Per-coordinator-host credentials (RT2 F3a): the auth token is only ever
/// valid for the coordinator that minted it, so it must only ever be read
/// back for that same host. Files are `credentials-<host-tag>.json` in the
/// work/app-data dir; a coordinator URL change therefore lands on a fresh
/// file and re-registers instead of replaying the old tracker's token.
///
/// The v0.1.x single `credentials.json` predates host binding and carries
/// no host of its own, so the only provenance we can assert is "this is the
/// URL the whole v0.1.x fleet shipped with": a legacy file is adopted ONLY
/// when the configured coordinator is that default, and otherwise ignored
/// outright (never sent anywhere, never deleted here — Unregister is still
/// the only deleter). Must track the GUI's `DEFAULT_COORDINATOR`
/// (gui/src-tauri/src/settings.rs; a test on each side locks them together).
pub const LEGACY_DEFAULT_COORDINATOR: &str = "https://caw.landhunter.app";

/// Filesystem-safe identity tag for a coordinator URL: the URL host plus
/// the explicit port when it differs from the scheme's default
/// (`https://h` and `https://h:443` are the same origin; explicit
/// non-default ports stay part of the identity). Characters outside
/// `[A-Za-z0-9._-]` (IPv6 colons/brackets, anything exotic) collapse to
/// `_`, and a URL without a parsable host degrades to `unknown` — the tag
/// is a filename, not a security boundary (it only needs to be stable and
/// collision-free for the URLs a user actually configures).
fn credentials_host_tag(coordinator_url: &str) -> String {
    let sanitize = |raw: String| -> String {
        let mut out: String = raw
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        out.truncate(120);
        if out.is_empty() {
            "unknown".to_string()
        } else {
            out
        }
    };
    let parsed = url::Url::parse(coordinator_url.trim());
    let Some(u) = parsed.ok() else {
        return sanitize(String::new());
    };
    let host = match u.host() {
        Some(url::Host::Domain(d)) => d.to_string(),
        Some(url::Host::Ipv4(ip)) => ip.to_string(),
        Some(url::Host::Ipv6(ip)) => ip.to_string(),
        None => return sanitize(String::new()),
    };
    match u.port() {
        Some(p) if Some(p) != scheme_default_port(u.scheme()) => sanitize(format!("{host}:{p}")),
        _ => sanitize(host),
    }
}

/// Well-known port for a scheme (coordinator URLs are http/https in
/// practice; unknown schemes keep an explicit port in the tag).
fn scheme_default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "https" | "wss" => Some(443),
        "http" | "ws" => Some(80),
        _ => None,
    }
}

/// Credentials file for a specific coordinator URL (per-host, RT2 F3a).
pub fn credentials_file_for(work_dir: &Path, coordinator_url: &str) -> PathBuf {
    work_dir.join(format!(
        "credentials-{}.json",
        credentials_host_tag(coordinator_url)
    ))
}

fn read_credentials_file(path: &Path) -> Result<Option<Credentials>, ConfigError> {
    let Ok(text) = fs::read_to_string(path) else {
        return Ok(None);
    };
    let creds: Credentials =
        serde_json::from_str(&text).map_err(|e| ConfigError::BadCredentials {
            path: path.to_path_buf(),
            problem: e.to_string(),
        })?;
    Ok(Some(creds))
}

/// Load the credentials bound to `coordinator_url`: the per-host file
/// first, then — only when the URL IS the historical default — the legacy
/// single `credentials.json` (adopted read-only; it stays on disk until
/// Unregister removes it). A legacy file under any other configured host
/// is ignored: no read, no send, no delete. Explicit LH_CLIENT_ID /
/// LH_AUTH_TOKEN env vars (checked by `Config::from_env` before this)
/// still win over both files.
pub fn load_credentials_for(
    work_dir: &Path,
    coordinator_url: &str,
) -> Result<Option<Credentials>, ConfigError> {
    let per_host = credentials_file_for(work_dir, coordinator_url);
    if per_host.exists() {
        return read_credentials_file(&per_host);
    }
    if credentials_host_tag(coordinator_url)
        == credentials_host_tag(LEGACY_DEFAULT_COORDINATOR)
    {
        let legacy = work_dir.join("credentials.json");
        if legacy.exists() {
            return read_credentials_file(&legacy);
        }
    }
    Ok(None)
}

/// Save freshly-minted credentials to the per-coordinator-host file
/// `credentials-<host-tag>.json` under `work_dir` (RT2 F3a), with 0600
/// permissions (unix; best effort elsewhere).
///
/// The file is written to a temp file in the same directory, chmodded 0600
/// and then atomically renamed over the target: the file is never visible
/// with lax permissions (the write-then-chmod window is closed), and a chmod
/// failure is fatal (`Err`) rather than silently ignored (`let _ =`).
pub fn save_credentials(
    work_dir: &Path,
    coordinator_url: &str,
    creds: &Credentials,
) -> Result<PathBuf, ConfigError> {
    fs::create_dir_all(work_dir)?;
    let path = credentials_file_for(work_dir, coordinator_url);
    let json = serde_json::to_string_pretty(creds).expect("credentials serialize");
    let temp_path = work_dir.join(format!(".credentials.json.tmp.{}", std::process::id()));
    fs::write(&temp_path, &json)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp_path, fs::Permissions::from_mode(0o600))
            .map_err(ConfigError::Io)?;
    }
    fs::rename(&temp_path, &path).map_err(ConfigError::Io)?;
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
    let ak = std::env::var("LH_R2_ACCESS_KEY_ID")
        .ok()
        .filter(|s| !s.is_empty());
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
                format!("https://{}.r2.cloudflarestorage.com", account.trim())
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

    #[test]
    fn save_credentials_writes_0600_and_leaves_no_temp_file() {
        let dir = std::env::temp_dir().join("lhc-save-creds-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let creds = Credentials {
            client_id: "c1".to_string(),
            auth_token: "***".to_string(),
        };
        let url = "https://tracker.test";
        let path = save_credentials(&dir, url, &creds).expect("save");
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            "credentials-tracker.test.json",
            "credentials land in the per-host file (RT2 F3a)"
        );
        assert!(path.exists());
        let text = fs::read_to_string(&path).expect("read");
        assert_eq!(
            text,
            "{\n  \"client_id\": \"c1\",\n  \"auth_token\": \"***\"\n}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "credentials file must be 0600");
        }
        // The temp file must be renamed away, not left behind.
        let leftover: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".credentials.json.tmp.")
            })
            .collect();
        assert!(
            leftover.is_empty(),
            "temp file must be renamed away, got {leftover:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn save_credentials_propagates_io_errors_instead_of_silently_ignoring() {
        let dir = std::env::temp_dir().join("lhc-save-creds-fail-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        // Read-only dir: the temp write / chmod / rename cannot succeed. The
        // error must surface as Err (the chmod failure is no longer dropped
        // with `let _ =`).
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
        let creds = Credentials {
            client_id: "c1".to_string(),
            auth_token: "***".to_string(),
        };
        let err = save_credentials(&dir, "https://tracker.test", &creds).unwrap_err();
        assert!(
            matches!(err, ConfigError::Io(_)),
            "expected Io error, got {err:?}"
        );
        // Restore permissions so the temp dir can be removed.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn host_tag_is_stable_sanitized_and_port_aware() {
        let tag = |url: &str| {
            credentials_file_for(Path::new("/w"), url)
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(
            tag("https://caw.landhunter.app"),
            "credentials-caw.landhunter.app.json"
        );
        // An explicit default port is the same origin as no port.
        assert_eq!(
            tag("https://caw.landhunter.app:443/"),
            "credentials-caw.landhunter.app.json"
        );
        assert_eq!(
            tag("http://127.0.0.1:8787"),
            "credentials-127.0.0.1_8787.json",
            "non-default port stays part of the identity"
        );
        assert_eq!(
            tag("http://[::1]:8080"),
            "credentials-__1_8080.json",
            "IPv6 punctuation sanitizes to underscores"
        );
        assert_eq!(tag("not a url"), "credentials-unknown.json");
    }

    #[test]
    fn credentials_are_scoped_to_the_coordinator_host() {
        let dir = std::env::temp_dir().join("lhc-creds-per-host-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let a = Credentials {
            client_id: "a".to_string(),
            auth_token: "***".to_string(),
        };
        let host_a = "https://tracker-a.test";
        let host_b = "https://tracker-b.test";
        save_credentials(&dir, host_a, &a).expect("save");

        // Same host: adopted. A different coordinator: NOTHING loads —
        // switching the URL can never replay host A's token to host B.
        assert_eq!(load_credentials_for(&dir, host_a).unwrap().unwrap().client_id, "a");
        assert!(load_credentials_for(&dir, host_b).unwrap().is_none());
        // Same host, explicit default port: still the same identity.
        assert_eq!(
            load_credentials_for(&dir, "https://tracker-a.test:443")
                .unwrap()
                .unwrap()
                .client_id,
            "a"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_credentials_adopt_only_for_the_default_url_and_are_never_deleted() {
        let dir = std::env::temp_dir().join("lhc-creds-legacy-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("credentials.json"),
            r#"{"client_id":"legacy-c","auth_token":"legacy-t"}"#,
        )
        .unwrap();

        // The historical default URL: adopted (read-only — the file stays).
        let adopted = load_credentials_for(&dir, LEGACY_DEFAULT_COORDINATOR)
            .unwrap()
            .expect("legacy adopted");
        assert_eq!(adopted.client_id, "legacy-c");
        assert!(dir.join("credentials.json").exists(), "adoption never deletes legacy data");

        // Any other coordinator: ignored outright — no read, no send, and
        // the file is left untouched (Unregister is the only deleter).
        assert!(load_credentials_for(&dir, "https://mirror.example.org")
            .unwrap()
            .is_none());
        assert!(dir.join("credentials.json").exists());

        // A per-host file outranks the legacy fallback for the same host.
        let fresh = Credentials {
            client_id: "fresh-c".to_string(),
            auth_token: "***".to_string(),
        };
        save_credentials(&dir, LEGACY_DEFAULT_COORDINATOR, &fresh).expect("save");
        assert_eq!(
            load_credentials_for(&dir, LEGACY_DEFAULT_COORDINATOR)
                .unwrap()
                .unwrap()
                .client_id,
            "fresh-c",
            "per-host file wins once it exists"
        );
        assert!(dir.join("credentials.json").exists(), "legacy file still untouched");
        let _ = fs::remove_dir_all(&dir);
    }
}
