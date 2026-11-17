//! HTTP client + wire types for the lh-tracker coordinator API.
//!
//! Wire shapes mirror `lh-tracker/src/types.ts` and `src/lib/schema.ts`
//! (snake_case JSON, zod constraints). Auth is body-carried, not headers:
//! `/v1/lease` takes `client_id` + `auth_token`; `/v1/heartbeat`, `/v1/result`
//! and `/v1/fail` take `client_id` and/or `lease_id`.

use crate::error::ApiError;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::time::Duration;
use url::Url;

// ---------------------------------------------------------------------------
// Chunk contracts
// ---------------------------------------------------------------------------

pub const ALL_CHUNK_TYPES: [ChunkType; 6] = [
    ChunkType::Examination,
    ChunkType::Ingest,
    ChunkType::AltSource,
    ChunkType::StateWalk,
    ChunkType::Search,
    ChunkType::Classify,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkType {
    Examination,
    Ingest,
    AltSource,
    StateWalk,
    Search,
    Classify,
}

impl ChunkType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChunkType::Examination => "examination",
            ChunkType::Ingest => "ingest",
            ChunkType::AltSource => "alt_source",
            ChunkType::StateWalk => "state_walk",
            ChunkType::Search => "search",
            ChunkType::Classify => "classify",
        }
    }

    pub fn parse(s: &str) -> Option<ChunkType> {
        ALL_CHUNK_TYPES.iter().copied().find(|t| t.as_str() == s)
    }
}

impl std::fmt::Display for ChunkType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The declarative contract core (operator-issued, immutable).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChunkContract {
    #[serde(rename = "type")]
    pub chunk_type: ChunkType,
    pub target_key: String,
    #[serde(default)]
    pub payload: Value,
    pub schema_version: u32,
}

/// Contract carrying the operator's ed25519 signature envelope.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SignedChunkContract {
    #[serde(flatten)]
    pub core: ChunkContract,
    /// Chunk validity deadline (unix ms). Leases must not outlive it.
    pub expires_at: i64,
    /// ed25519 signature (hex) over `{chunk_id}:{canonical core json}:{expires_at}`.
    pub operator_signature_hex: String,
}

// ---------------------------------------------------------------------------
// Type-specific contract payloads (client parses these lazily per chunk type)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct Measurement {
    pub name: String,
    pub expected_type: String,
    #[serde(default)]
    pub tolerance: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExaminationPayload {
    pub parser_id: String,
    pub staged_blob_key: String,
    #[serde(default)]
    pub expected_measurements: Vec<Measurement>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RateSpec {
    pub requests_per_min: u64,
    pub bytes_per_min: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PageSpec {
    pub path: String,
    #[serde(default)]
    pub params: Map<String, Value>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub expect_content_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FetchPlan {
    pub base_url: String,
    pub method: String,
    #[serde(default)]
    pub headers: Map<String, Value>,
    pub pages: Vec<PageSpec>,
    #[serde(default)]
    pub rate_limit: Option<RateSpec>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReconciliationRule {
    pub field: String,
    pub expected: String,
    #[serde(default)]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IngestPayload {
    pub fetch_plan: FetchPlan,
    #[serde(default)]
    pub reconciliation_rules: Vec<ReconciliationRule>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProbePlan {
    #[serde(default)]
    pub enum_services: bool,
    #[serde(default)]
    pub count_groupby: bool,
    #[serde(default)]
    pub null_count: bool,
    #[serde(default)]
    pub pagination_test: bool,
    #[serde(default)]
    pub return_centroid_test: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AltSourcePayload {
    pub candidate_urls: Vec<String>,
    #[serde(default)]
    pub probe_plan: Option<ProbePlan>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RosterEntry {
    pub slug: String,
    pub display_name: String,
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WalkRecipe {
    pub title_line_pattern: String,
    pub mint_relation: String,
    pub heading_capture: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StateWalkPayload {
    pub roster_slice: Vec<RosterEntry>,
    pub walk_recipe: WalkRecipe,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QuerySpec {
    pub query: String,
    #[serde(default)]
    pub filters: Map<String, Value>,
    pub max_results: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QueryPlan {
    pub queries: Vec<QuerySpec>,
    pub backend: String,
    #[serde(default)]
    pub custom_endpoint: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchPayload {
    pub query_plan: QueryPlan,
    #[serde(default)]
    pub source_class_priority: Vec<String>,
}

/// Grading rubric carried by classify contracts; fills the fixed slots of
/// the built-in prompt template. Instructions live in the template, not here.
#[derive(Debug, Clone, Deserialize)]
pub struct ClassifyRubric {
    pub objective: String,
    #[serde(default)]
    pub relevant_criteria: Vec<String>,
    #[serde(default)]
    pub irrelevant_criteria: Vec<String>,
}

/// Input reference for a classify chunk: a search-result set fetched through
/// the tracker's operator-key proxy, or a staged blob fetched via the stager.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClassifyInput {
    SearchResultSet { query_plan: QueryPlan },
    StagedBlob { staged_blob_key: String },
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClassifyPayload {
    /// Built-in prompt template ID (see `crate::llm`). Unknown IDs are a
    /// typed `contract_unknown_parser` failure, like parser IDs.
    pub prompt_template_id: String,
    pub rubric: ClassifyRubric,
    pub input: ClassifyInput,
    /// Named verdict schema; only 'classify_verdict_v1' exists. Unknown
    /// names are a typed failure (operator/client version skew).
    #[serde(default)]
    pub verdict_schema: Option<String>,
    /// Distinct-client verdicts wanted before the chunk retires.
    /// Informational client-side; the tracker owns retirement.
    #[serde(default)]
    pub expected_verdicts: Option<u32>,
}

// ---------------------------------------------------------------------------
// LLM capability (register extension)
// ---------------------------------------------------------------------------

/// Where the grading model runs. Sent to the tracker at register; the tracker
/// records it (no policy enforced today).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LlmEndpointKind {
    LocalSidecar,
    CustomLocal,
    Cloud,
}

impl LlmEndpointKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            LlmEndpointKind::LocalSidecar => "local-sidecar",
            LlmEndpointKind::CustomLocal => "custom-local",
            LlmEndpointKind::Cloud => "cloud",
        }
    }

    pub fn parse(s: &str) -> Option<LlmEndpointKind> {
        match s {
            "local-sidecar" => Some(LlmEndpointKind::LocalSidecar),
            "custom-local" => Some(LlmEndpointKind::CustomLocal),
            "cloud" => Some(LlmEndpointKind::Cloud),
            _ => None,
        }
    }
}

/// Local-LLM capability declared at register when configured. Presence is
/// what gates classify leases; the fields give the operator grading-quality
/// context (and future lease policy hooks).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmCapability {
    pub model_family: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quant: Option<String>,
    pub ctx_tokens: u32,
    pub endpoint_kind: LlmEndpointKind,
}

// ---------------------------------------------------------------------------
// Lease
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct RateBudget {
    pub requests_per_min: u64,
    pub bytes_per_min: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LeaseResponse {
    pub lease_id: String,
    pub chunk_id: String,
    pub contract: SignedChunkContract,
    /// Lease expiry (unix ms).
    pub expires_at: i64,
    pub heartbeat_interval_sec: u64,
    /// R2 staging key prefix for this lease's blobs (ends with `/`).
    pub staging_upload_prefix: String,
    pub rate_budget: RateBudget,
}

/// What `POST /v1/lease` returns with HTTP 200: either a claimed lease or a
/// no-work/rate-gated backoff hint. Distinguished by `lease_id` presence,
/// exactly like the server's two response shapes.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum LeasePoll {
    Lease(Box<LeaseResponse>),
    Backoff { backoff_sec: u64 },
}

// ---------------------------------------------------------------------------
// Register / heartbeat / fail / result
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct RegisterRequest {
    pub platform: String,
    pub version: String,
    pub capabilities: Vec<ChunkType>,
    /// Local-LLM capability; declares classify-chunk eligibility. Omitted
    /// entirely (not null) when not configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm: Option<LlmCapability>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RegisterResponse {
    pub client_id: String,
    pub auth_token: String,
}

/// Saved identity minted by `POST /v1/register`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub client_id: String,
    pub auth_token: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct HeartbeatRequest {
    pub lease_id: String,
    pub client_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_note: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HeartbeatResponse {
    pub ok: bool,
    pub lease_expires_at: i64,
}

/// Mirrors lh-tracker's `FailureType` union. `http_4xx` is the one terminal
/// type server-side (chunk marked failed); every other type requeues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureType {
    SourceUnreachable,
    Http4xx,
    Http5xx,
    BotWalled,
    JsOnly,
    SchemaDrift,
    ContractUnknownParser,
    ClientError,
    ClientTimeout,
    AbortedByUser,
}

impl FailureType {
    /// Server-side retry policy: `http_4xx` marks the chunk failed; the rest
    /// put it back to pending. Keep in sync with lh-tracker's
    /// `RETRYABLE_FAILURE_TYPES` (src/routes/fail.ts).
    pub fn is_retryable(&self) -> bool {
        !matches!(self, FailureType::Http4xx)
    }
}

impl std::fmt::Display for FailureType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            FailureType::SourceUnreachable => "source_unreachable",
            FailureType::Http4xx => "http_4xx",
            FailureType::Http5xx => "http_5xx",
            FailureType::BotWalled => "bot_walled",
            FailureType::JsOnly => "js_only",
            FailureType::SchemaDrift => "schema_drift",
            FailureType::ContractUnknownParser => "contract_unknown_parser",
            FailureType::ClientError => "client_error",
            FailureType::ClientTimeout => "client_timeout",
            FailureType::AbortedByUser => "aborted_by_user",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FailRequest {
    pub lease_id: String,
    pub client_id: String,
    pub failure_type: FailureType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

/// Worker manifest (submitted inside `/v1/result`). Mirrors
/// `WorkerManifestSchema` — zod `.passthrough()` means extra keys are legal
/// and preserved, so client-specific evidence rides in `extra`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerManifest {
    #[serde(rename = "type")]
    pub chunk_type: ChunkType,
    pub target: String,
    pub source_urls: Vec<String>,
    pub page_count: i64,
    /// sha256 of each fetched page's RAW bytes (hex, 64 chars) — this is what
    /// coordinator-side byte-hash verification compares against.
    pub page_sha256: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reconciliation: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staged_features_blob: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_pages_blob_prefix: Option<String>,
    /// Passthrough for extra evidence keys (blob_encoding, probe_results...).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResultSubmit {
    pub chunk_id: String,
    pub client_id: String,
    /// Unix ms.
    pub started_at: i64,
    /// Unix ms.
    pub completed_at: i64,
    pub manifest: WorkerManifest,
    pub evidence_blobs: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResultReceipt {
    pub result_id: String,
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HealthInfo {
    pub ok: bool,
    pub time: i64,
}

// ---------------------------------------------------------------------------
// Search proxy (classify input fetch)
// ---------------------------------------------------------------------------

/// One query's raw result set from the operator-key search proxy.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProxyQueryResult {
    pub query: String,
    pub url: String,
    #[serde(default)]
    pub status: Option<u16>,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub body: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport_error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SearchProxyResponse {
    pub chunk_id: String,
    pub backend: String,
    pub fetched_at: i64,
    pub queries: Vec<ProxyQueryResult>,
}

/// Handle for `POST /v1/search-proxy` (classify input fetch). Body-carried
/// auth like every other coordinator call; the query plan is derived
/// server-side from the signed chunk contract, so the client only ever
/// sends ids. Build via [`ApiClient::search_proxy`].
#[derive(Debug, Clone)]
pub struct SearchProxy {
    http: reqwest::Client,
    base: Url,
    client_id: String,
    auth_token: String,
}

impl SearchProxy {
    /// Fetch the raw result sets for a leased classify chunk's fixed query
    /// plan. 409 = no active lease; 429 = per-client quota; 503 = operator
    /// key not configured. None of these are chunk failures — callers decide
    /// how to map them.
    pub async fn fetch_classify_input(
        &self,
        chunk_id: &str,
    ) -> Result<SearchProxyResponse, ApiError> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .expect("base url")
            .extend(["v1", "search-proxy"]);
        let body = serde_json::json!({
            "client_id": self.client_id,
            "auth_token": self.auth_token,
            "chunk_id": chunk_id,
        });
        let resp = self.http.post(url).json(&body).send().await?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        if status == 200 {
            return serde_json::from_str(&text).map_err(|source| ApiError::Decode {
                source,
                body: truncate_body(&text),
            });
        }
        Err(ApiClient::status_error(status, &text))
    }
}

// ---------------------------------------------------------------------------
// ApiClient
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ApiClient {
    http: reqwest::Client,
    base: Url,
}

impl ApiClient {
    pub fn new(base_url: &str, timeout: Duration) -> Result<ApiClient, ApiError> {
        let base = Url::parse(base_url)
            .map_err(|e| ApiError::BadRequest(format!("invalid LH_COORDINATOR_URL: {e}")))?;
        let http = reqwest::Client::builder()
            .user_agent(format!("lh-contributor-client/{}", crate::client_version()))
            .timeout(timeout)
            .build()?;
        Ok(ApiClient { http, base })
    }

    fn url(&self, path: &str) -> Url {
        // Base URLs are stripped of their trailing slash at construction; path
        // always starts with '/'.
        let mut base = self.base.clone();
        let p = path.trim_start_matches('/');
        let segs: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
        for seg in segs {
            base.path_segments_mut().expect("base url").push(seg);
        }
        base
    }

    /// POST a JSON body, map status codes to `ApiError`, decode JSON response.
    async fn post<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> Result<T, ApiError> {
        let resp = self.http.post(self.url(path)).json(body).send().await?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        if status == 200 || status == 201 {
            return serde_json::from_str(&text).map_err(|source| ApiError::Decode {
                source,
                body: truncate_body(&text),
            });
        }
        Err(Self::status_error(status, &text))
    }

    fn status_error(status: u16, body: &str) -> ApiError {
        let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        let err_msg = parsed
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or(body)
            .to_string();
        match status {
            401 => ApiError::Unauthorized,
            403 => ApiError::Forbidden,
            404 => ApiError::NotFound(err_msg),
            409 => ApiError::Conflict(err_msg),
            410 => ApiError::LeaseExpired,
            429 => {
                let retry = parsed.get("retry_after_sec").and_then(Value::as_u64);
                ApiError::RateLimited {
                    retry_after_sec: retry,
                }
            }
            400 => ApiError::BadRequest(err_msg),
            s => ApiError::Server {
                status: s,
                body: truncate_body(body),
            },
        }
    }

    pub async fn health(&self) -> Result<HealthInfo, ApiError> {
        let resp = self.http.get(self.url("/health")).send().await?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        if status == 200 {
            return serde_json::from_str(&text).map_err(|source| ApiError::Decode {
                source,
                body: truncate_body(&text),
            });
        }
        Err(Self::status_error(status, &text))
    }

    pub async fn register(&self, req: &RegisterRequest) -> Result<Credentials, ApiError> {
        let resp: RegisterResponse = self.post("/v1/register", req).await?;
        Ok(Credentials {
            client_id: resp.client_id,
            auth_token: resp.auth_token,
        })
    }

    /// Build a search-proxy handle bound to this coordinator and identity.
    /// Used by the classify executor to fetch a leased chunk's fixed query
    /// plan results without ever seeing the operator's search key.
    pub fn search_proxy(&self, creds: &Credentials) -> SearchProxy {
        SearchProxy {
            http: self.http.clone(),
            base: self.base.clone(),
            client_id: creds.client_id.clone(),
            auth_token: creds.auth_token.clone(),
        }
    }

    pub async fn lease(
        &self,
        creds: &Credentials,
        slot_id: u8,
        capabilities: &[ChunkType],
    ) -> Result<LeasePoll, ApiError> {
        let body = serde_json::json!({
            "client_id": creds.client_id,
            "auth_token": creds.auth_token,
            "slot_id": slot_id,
            "capabilities": capabilities,
        });
        self.post("/v1/lease", &body).await
    }

    pub async fn heartbeat(
        &self,
        client_id: &str,
        lease_id: &str,
        progress_note: Option<&str>,
    ) -> Result<HeartbeatResponse, ApiError> {
        let body = serde_json::json!({
            "lease_id": lease_id,
            "client_id": client_id,
            "progress_note": progress_note,
        });
        self.post("/v1/heartbeat", &body).await
    }

    pub async fn submit_result(&self, submit: &ResultSubmit) -> Result<ResultReceipt, ApiError> {
        self.post("/v1/result", submit).await
    }

    pub async fn report_fail(&self, req: &FailRequest) -> Result<(), ApiError> {
        let resp = self
            .http
            .post(self.url("/v1/fail"))
            .json(req)
            .send()
            .await?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        if status == 200 {
            return Ok(());
        }
        Err(Self::status_error(status, &text))
    }

    /// Public contract refresh endpoint (`GET /v1/chunk/:id/contract`). The
    /// server verifies before serving; clients verify independently anyway.
    pub async fn fetch_contract(&self, chunk_id: &str) -> Result<SignedChunkContract, ApiError> {
        let path = format!("/v1/chunk/{chunk_id}/contract");
        let resp = self.http.get(self.url(&path)).send().await?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        if status == 200 {
            return serde_json::from_str(&text).map_err(|source| ApiError::Decode {
                source,
                body: truncate_body(&text),
            });
        }
        Err(Self::status_error(status, &text))
    }
}

fn truncate_body(body: &str) -> String {
    const MAX: usize = 512;
    if body.chars().count() <= MAX {
        body.to_string()
    } else {
        let cut: String = body.chars().take(MAX).collect();
        format!("{cut}…")
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Unix milliseconds for `SystemTime::now()`.
pub fn unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
