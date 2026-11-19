//! Chunk type-specific executors + shared execution plumbing.
//!
//! Chunks are DECLARATIVE contracts (fetch plans, probe plans, rosters) —
//! no server-sent code is ever executed. Each executor consumes its payload
//! shape, fetches politely (shared [`RateLimiter`]), stages blobs, and
//! returns a [`WorkerManifest`] for `/v1/result` — or a [`ChunkFailure`]
//! carrying the coordinator failure_type for `/v1/fail`.

pub mod alt_source;
pub mod classify;
pub mod examination;
pub mod ingest;
pub mod search;
pub mod state_walk;

use crate::api::{
    ChunkType, FailureType, LeaseResponse, SearchProxy, SignedChunkContract, WorkerManifest,
};
use crate::error::ChunkFailure;
use crate::llm::LlmLane;
use crate::rate::RateLimiter;
use crate::staging::BlobStager;
use futures_util::{stream::Stream, StreamExt as _};
use std::sync::Arc;
use tokio::sync::watch;

/// One in-flight lease handed to executors.
#[derive(Debug, Clone)]
pub struct LeaseContext {
    pub lease_id: String,
    pub chunk_id: String,
    pub target_key: String,
    pub staging_upload_prefix: String,
}

impl LeaseContext {
    pub fn from_lease(lease: &LeaseResponse) -> LeaseContext {
        LeaseContext {
            lease_id: lease.lease_id.clone(),
            chunk_id: lease.chunk_id.clone(),
            target_key: lease.contract.core.target_key.clone(),
            staging_upload_prefix: lease.staging_upload_prefix.clone(),
        }
    }
}

/// Everything an executor needs besides the contract payload.
pub struct ExecContext {
    pub http: reqwest::Client,
    /// Politeness limiter for SOURCE hosts (contract budget ∩ client cap).
    pub limiter: Arc<RateLimiter>,
    pub stager: Arc<dyn BlobStager>,
    pub lease: LeaseContext,
    pub user_agent: String,
    /// Brave Search API key for search chunks (backend=brave).
    pub brave_api_key: Option<String>,
    /// Local-LLM grading lane; required for classify chunks. `None` on a
    /// classify lease is a client wiring bug → typed client_error failure
    /// (the tracker only leases classify to llm-declared clients anyway).
    pub llm: Option<LlmLane>,
    /// Coordinator search proxy for classify input fetches (operator-key
    /// search without the client ever holding a search key).
    pub search_proxy: Option<SearchProxy>,
    /// Set true when the heartbeat loop reports the lease as lost/expired —
    /// executors check between units of work and bail out.
    pub lease_lost: watch::Receiver<bool>,
    /// Dry runs stage locally and never submit.
    pub dry_run: bool,
}

impl ExecContext {
    /// Non-blocking lease-liveness check for use between work units.
    pub fn lease_is_lost(&self) -> bool {
        *self.lease_lost.borrow()
    }
}

/// Successful execution: the manifest to submit + blob keys as evidence.
#[derive(Debug)]
pub struct Outcome {
    pub manifest: WorkerManifest,
    pub evidence_blobs: Vec<String>,
}

/// Dispatch to the type-specific executor.
pub async fn execute(
    ctx: &ExecContext,
    contract: &SignedChunkContract,
) -> Result<Outcome, ChunkFailure> {
    match contract.core.chunk_type {
        ChunkType::Ingest => ingest::execute(ctx, contract).await,
        ChunkType::AltSource => alt_source::execute(ctx, contract).await,
        ChunkType::StateWalk => state_walk::execute(ctx, contract).await,
        ChunkType::Search => search::execute(ctx, contract).await,
        ChunkType::Examination => examination::execute(ctx, contract).await,
        ChunkType::Classify => classify::execute(ctx, contract).await,
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Parse a type-specific payload out of the contract, mapping shape errors
/// to schema_drift (the operator's contract doesn't match the client's
/// understanding of the schema).
pub(crate) fn parse_payload<T: serde::de::DeserializeOwned>(
    contract: &SignedChunkContract,
) -> Result<T, ChunkFailure> {
    serde_json::from_value(contract.core.payload.clone()).map_err(|e| {
        ChunkFailure::new(
            FailureType::SchemaDrift,
            format!(
                "contract payload does not match {} schema: {e}",
                contract.core.chunk_type
            ),
        )
    })
}

/// Map a source-side HTTP status to a coordinator failure type.
/// 403/429 read as bot walls (retryable); other 4xx are terminal (the
/// planned source path/schema is wrong); 5xx are server-side trouble.
pub(crate) fn failure_for_status(status: u16, url: &str) -> ChunkFailure {
    match status {
        403 | 429 => ChunkFailure::new(
            FailureType::BotWalled,
            format!("status {status} (bot wall suspected) at {url}"),
        ),
        s if (500..600).contains(&s) => {
            ChunkFailure::new(FailureType::Http5xx, format!("status {s} at {url}"))
        }
        s => ChunkFailure::new(FailureType::Http4xx, format!("status {s} at {url}")),
    }
}

/// Map a transport-level reqwest error to a coordinator failure type.
pub(crate) fn failure_for_transport(err: &reqwest::Error, url: &str) -> ChunkFailure {
    if err.is_timeout() {
        ChunkFailure::new(
            FailureType::ClientTimeout,
            format!("timeout fetching {url}: {err}"),
        )
    } else if err.is_connect() || err.is_request() {
        ChunkFailure::new(
            FailureType::SourceUnreachable,
            format!("unreachable {url}: {err}"),
        )
    } else {
        ChunkFailure::new(
            FailureType::SourceUnreachable,
            format!("transport error fetching {url}: {err}"),
        )
    }
}

/// One fetched page, post status/content-type validation.
#[derive(Debug)]
pub(crate) struct FetchedPage {
    pub requested_url: String,
    pub final_url: String,
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
}

/// Hard cap on a single fetched page body: never buffer unbounded.
/// 16 MiB is ample for any source page (GIS parcel HTML, CSVs, rosters);
/// anything larger is a misconfigured contract or an abuse attempt, so we
/// reject it with a clear error instead of allocating without limit.
pub(crate) const MAX_BODY_BYTES: u64 = 16 * 1024 * 1024;

/// Read a response body stream, enforcing [`MAX_BODY_BYTES`] so a malicious
/// or broken source can never make us buffer an unbounded body in memory.
/// Returns a clear [`FailureType::ClientError`] when the source exceeds the
/// cap. Transport errors mid-stream map to the usual transport failure.
async fn read_capped_body<B, S>(
    mut stream: S,
    url: &str,
) -> Result<Vec<u8>, ChunkFailure>
where
    S: Stream<Item = Result<B, reqwest::Error>> + Unpin,
    B: AsRef<[u8]>,
{
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| failure_for_transport(&e, url))?;
        let chunk_len = chunk.as_ref().len();
        let remaining = MAX_BODY_BYTES as usize - buf.len();
        if chunk_len > remaining {
            return Err(ChunkFailure::new(
                FailureType::ClientError,
                format!(
                    "response body exceeds {MAX_BODY_BYTES} byte cap at {url}; \
                     refusing to buffer unbounded"
                ),
            ));
        }
        buf.extend_from_slice(chunk.as_ref());
    }
    Ok(buf)
}

/// Polite fetch with rate limiting, status mapping and content-type checks.
pub(crate) async fn fetch_page(
    ctx: &ExecContext,
    method: reqwest::Method,
    url: &str,
    extra_headers: &[(String, String)],
    body: Option<&str>,
    expect_content_type: Option<&str>,
) -> Result<FetchedPage, ChunkFailure> {
    ctx.limiter.acquire().await;
    if ctx.lease_is_lost() {
        return Err(ChunkFailure::new(
            FailureType::ClientError,
            "lease lost (heartbeat failed) mid-execution; aborting without result",
        ));
    }

    let parsed_url = url::Url::parse(url).map_err(|e| {
        ChunkFailure::new(FailureType::SchemaDrift, format!("bad url {url:?}: {e}"))
    })?;

    let mut req = ctx.http.request(method, parsed_url);
    for (name, value) in extra_headers {
        let lower = name.to_ascii_lowercase();
        // Never let a contract override transport-critical headers.
        if matches!(lower.as_str(), "host" | "content-length") {
            continue;
        }
        req = req.header(name, value);
    }
    req = req.header(reqwest::header::USER_AGENT, &ctx.user_agent);
    if let Some(b) = body {
        req = req.body(b.to_string());
    }

    let resp = req
        // Redirect policy (Policy::none) is pinned on the http client at
        // construction (runner.rs `source_http`): any 3xx is rejected rather
        // than followed, so a compromised source can never redirect the
        // client off-host (SSRF / host-drift). The contracted URL is the
        // final destination; operators must put final URLs in contracts.
        .send()
        .await
        .map_err(|e| failure_for_transport(&e, url))?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(failure_for_status(status, url));
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    if let (Some(expect), Some(actual)) = (expect_content_type, content_type.as_deref()) {
        if !actual.starts_with(expect) {
            return Err(ChunkFailure::new(
                FailureType::SchemaDrift,
                format!("content-type mismatch at {url}: expected {expect:?}, got {actual:?}"),
            ));
        }
    }
    let final_url = resp.url().to_string();
    let bytes = read_capped_body(resp.bytes_stream(), url).await?;
    ctx.limiter.record_bytes(bytes.len() as u64).await;
    Ok(FetchedPage {
        requested_url: url.to_string(),
        final_url,
        content_type,
        bytes,
    })
}

/// Common manifest scaffold.
pub(crate) fn manifest_scaffold(
    contract: &SignedChunkContract,
    source_urls: Vec<String>,
    page_sha256: Vec<String>,
) -> WorkerManifest {
    WorkerManifest {
        chunk_type: contract.core.chunk_type,
        target: contract.core.target_key.clone(),
        source_urls,
        page_count: page_sha256.len() as i64,
        page_sha256,
        reconciliation: None,
        staged_features_blob: None,
        raw_pages_blob_prefix: None,
        extra: serde_json::Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_mapping_matches_fail_endpoint_policy() {
        assert_eq!(
            failure_for_status(403, "u").failure_type,
            FailureType::BotWalled
        );
        assert_eq!(
            failure_for_status(429, "u").failure_type,
            FailureType::BotWalled
        );
        assert_eq!(
            failure_for_status(503, "u").failure_type,
            FailureType::Http5xx
        );
        assert_eq!(
            failure_for_status(404, "u").failure_type,
            FailureType::Http4xx
        );
        // http_4xx is the only terminal type server-side; check it stays so.
        assert!(!failure_for_status(404, "u").failure_type.is_retryable());
        assert!(failure_for_status(503, "u").failure_type.is_retryable());
        assert!(failure_for_status(403, "u").failure_type.is_retryable());
    }

    use crate::staging::LocalStager;
    use reqwest::redirect::Policy;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_ctx() -> ExecContext {
        let (lost_tx, lost_rx) = watch::channel(false);
        std::mem::forget(lost_tx); // never lost in these tests
        ExecContext {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .redirect(Policy::none()) // contracted URL is the final destination
                .build()
                .unwrap(),
            limiter: Arc::new(RateLimiter::new(10_000, 1_000_000_000)),
            stager: Arc::new(LocalStager::new(
                std::env::temp_dir().join("lhc-chunk-fetch-test"),
            )) as Arc<dyn BlobStager>,
            lease: LeaseContext {
                lease_id: "lease-1".to_string(),
                chunk_id: "chunk-1".to_string(),
                target_key: "WI/55011".to_string(),
                staging_upload_prefix: "stage/raw/test/wi/55011/lease-1/".to_string(),
            },
            user_agent: "lh-contributor/test".to_string(),
            brave_api_key: None,
            llm: None,
            search_proxy: None,
            lease_lost: lost_rx,
            dry_run: true,
        }
    }

    /// 16 MiB cap: an oversized body is refused with a clear error and never
    /// buffered without bound.
    #[tokio::test]
    async fn fetch_page_rejects_body_over_cap() {
        let server = MockServer::start().await;
        let over = vec![0u8; MAX_BODY_BYTES as usize + 1];
        Mock::given(method("GET"))
            .and(path("/oversized"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                over.clone(),
                "application/octet-stream",
            ))
            .mount(&server)
            .await;
        let ctx = test_ctx();
        let err = fetch_page(
            &ctx,
            reqwest::Method::GET,
            &format!("{}/oversized", server.uri()),
            &[],
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            err.details.contains(&MAX_BODY_BYTES.to_string()),
            "cap message must name the limit, got: {err}"
        );
        assert!(
            err.details.contains("unbounded"),
            "cap message must refuse unbounded buffering, got: {err}"
        );
    }

    /// A body under the cap still fetches fine.
    #[tokio::test]
    async fn fetch_page_accepts_body_under_cap() {
        let server = MockServer::start().await;
        let body = b"small page";
        Mock::given(method("GET"))
            .and(path("/small"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                body.to_vec(),
                "text/plain",
            ))
            .mount(&server)
            .await;
        let ctx = test_ctx();
        let page = fetch_page(
            &ctx,
            reqwest::Method::GET,
            &format!("{}/small", server.uri()),
            &[],
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(page.bytes, body);
    }

    /// Redirect policy: Policy::none() means a 3xx is rejected rather than
    /// followed, so the client is never steered off the contracted host.
    #[tokio::test]
    async fn fetch_page_refuses_redirect() {
        let source = MockServer::start().await;
        let target = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/redirect"))
            .respond_with(
                ResponseTemplate::new(302).insert_header(
                    "Location",
                    format!("{}/final", target.uri()),
                ),
            )
            .mount(&source)
            .await;
        // The target must never be observed — if the redirect were followed
        // it would return 200 with this body.
        Mock::given(method("GET"))
            .and(path("/final"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                b"target".to_vec(),
                "text/plain",
            ))
            .mount(&target)
            .await;
        let ctx = test_ctx();
        let err = fetch_page(
            &ctx,
            reqwest::Method::GET,
            &format!("{}/redirect", source.uri()),
            &[],
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(!err.details.is_empty(), "redirect must be refused, not followed");
    }
}
