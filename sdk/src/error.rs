//! Error types shared across the SDK.

use crate::api::FailureType;
use thiserror::Error;

/// Coordinator API call failure. Status-code semantics mirror lh-tracker:
/// 401 unauthorized, 403 suspended/forbidden, 404 not found, 409 conflict,
/// 410 lease expired, 429 rate limited, 400 bad request, 5xx server error.
#[derive(Debug, Error)]
pub enum ApiError {
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),

    #[error("unauthorized (unknown client or bad auth token)")]
    Unauthorized,

    #[error("forbidden (client suspended or wrong owner)")]
    Forbidden,

    #[error("not found: {0}")]
    NotFound(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("lease expired")]
    LeaseExpired,

    #[error("rate limited (retry after {retry_after_sec:?}s)")]
    RateLimited { retry_after_sec: Option<u64> },

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("server error {status}: {body}")]
    Server { status: u16, body: String },

    #[error("response decode failed: {source} (body: {body})")]
    Decode {
        source: serde_json::Error,
        body: String,
    },
}

impl ApiError {
    /// True when the error means the lease is gone for good and execution
    /// must stop (expired, not found, or settled by someone else).
    pub fn lease_is_lost(&self) -> bool {
        matches!(
            self,
            ApiError::LeaseExpired
                | ApiError::NotFound(_)
                | ApiError::Forbidden
                | ApiError::Conflict(_)
        )
    }
}

/// ed25519 contract-signature verification failure.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum LeaseVerifyError {
    #[error("contract is unsigned or malformed (missing expires_at/operator_signature_hex)")]
    Unsigned,

    #[error("lease is already stale (expires_at in the past)")]
    LeaseStale,

    #[error("lease outlives the contract's signed validity window")]
    LeaseOutlivesContract,

    #[error("contract expired (expires_at in the past)")]
    ContractExpired,

    #[error("operator signature does not verify against the configured public key")]
    BadSignature,

    #[error("lease staging_upload_prefix is invalid: {0}")]
    InvalidPrefix(String),

    #[error("configured public key is malformed: {0}")]
    BadPublicKey(String),
}

/// Blob staging (R2 / presign template / local) failure.
#[derive(Debug, Error)]
pub enum StageError {
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),

    #[error("staging request failed with status {status}: {body}")]
    Http { status: u16, body: String },

    #[error("invalid blob key: {0}")]
    BadKey(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Local-LLM lane failure (classify executor). Every variant — including
/// timeout and model garbage — maps to an honest no_verdict result, never a
/// guessed verdict and never an abort of the lease.
#[derive(Debug, Error)]
pub enum LlmError {
    #[error("llm endpoint misconfigured: {0}")]
    BadEndpoint(String),

    #[error("llm endpoint transport error: {0}")]
    Transport(reqwest::Error),

    #[error("llm endpoint timed out")]
    Timeout,

    #[error("llm endpoint returned status {status}: {body}")]
    Http { status: u16, body: String },

    #[error("model output is not a schema-valid verdict: {0}")]
    MalformedVerdict(String),
}

impl From<reqwest::Error> for LlmError {
    fn from(err: reqwest::Error) -> Self {
        if err.is_timeout() {
            LlmError::Timeout
        } else {
            LlmError::Transport(err)
        }
    }
}

/// A chunk execution failure, carrying the coordinator `failure_type` the
/// client will report via POST /v1/fail. `http_4xx` marks the chunk terminal
/// server-side; every other type requeues it for another client.
#[derive(Debug, Error)]
#[error("chunk failed ({failure_type}): {details}")]
pub struct ChunkFailure {
    pub failure_type: FailureType,
    pub details: String,
}

impl ChunkFailure {
    pub fn new(failure_type: FailureType, details: impl Into<String>) -> Self {
        Self {
            failure_type,
            details: details.into(),
        }
    }
}
