//! Classify-lane integration tests: the executor against a mock coordinator
//! (search proxy) and a mock OpenAI-compatible llama-server endpoint, plus
//! the pinned cross-language signature vector for the classify contract.
//!
//! Fail-safe semantics under test: only an explicit, schema-valid verdict is
//! submitted as a verdict — model garbage, fences-then-garbage, out-of-range
//! confidence, HTTP errors and timeouts all become honest no_verdict
//! results (never a guess, never an abort).

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier};
use lh_contributor_client::api::{
    ApiClient, ChunkType, Credentials, FailureType, LlmCapability, LlmEndpointKind, RegisterRequest,
};
use lh_contributor_client::chunk::{execute, ExecContext, LeaseContext};
use lh_contributor_client::llm::{LlmEndpoint, LlmLane};
use lh_contributor_client::rate::RateLimiter;
use lh_contributor_client::staging::{BlobStager, LocalStager};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CLIENT_ID: &str = "cl-classify-test";
const AUTH_TOKEN: &str = "tok-classify-test";
const CHUNK_ID: &str = "chunk-classify-1";

fn llm_capability() -> LlmCapability {
    LlmCapability {
        model_family: "qwen3".to_string(),
        quant: Some("Q4_K_M".to_string()),
        ctx_tokens: 8192,
        endpoint_kind: LlmEndpointKind::LocalSidecar,
    }
}

fn operator_key() -> ed25519_dalek::SigningKey {
    let digest = Sha256::digest(b"lh-classify-executor-test-operator");
    SigningKey::from_bytes(&digest.into())
}

fn classify_contract_value(input: Value) -> Value {
    json!({
        "type": "classify",
        "target_key": "WI/55011",
        "schema_version": 1,
        "staging_upload_prefix": "stage/raw/classify/wi/55011/lease-1/",
        "expires_at": lh_contributor_client::api::unix_ms() + 3600 * 1000,
        "payload": {
            "prompt_template_id": "search_result_relevance_v1",
            "rubric": {
                "objective": "Find official county GIS portals",
                "relevant_criteria": ["official .gov domain", "serves parcel data"],
                "irrelevant_criteria": ["third-party aggregators"]
            },
            "input": input,
            "verdict_schema": "classify_verdict_v1",
            "expected_verdicts": 3,
        },
        "operator_signature_hex": &hex::encode(operator_key().sign(b"unused").to_bytes()),
    })
}

fn search_result_set_input() -> Value {
    json!({
        "kind": "search_result_set",
        "query_plan": {
            "queries": [{"query": "wi 55011 gis parcels", "max_results": 5}],
            "backend": "brave",
        }
    })
}

fn parse_contract(v: Value) -> lh_contributor_client::api::SignedChunkContract {
    serde_json::from_value(v).expect("test contract deserializes")
}

fn creds() -> Credentials {
    Credentials {
        client_id: CLIENT_ID.to_string(),
        auth_token: AUTH_TOKEN.to_string(),
    }
}

struct Harness {
    coordinator: MockServer,
    llm: MockServer,
    staging_root: std::path::PathBuf,
}

impl Harness {
    async fn new() -> Harness {
        let coordinator = MockServer::start().await;
        let llm = MockServer::start().await;
        let staging_root = std::env::temp_dir().join(format!(
            "lh-classify-{}-{}",
            std::process::id(),
            rand_suffix()
        ));
        Harness {
            coordinator,
            llm,
            staging_root,
        }
    }

    fn ctx(&self) -> ExecContext {
        let (lost_tx, lost_rx) = watch::channel(false);
        std::mem::forget(lost_tx); // never lost in these tests
        let api = ApiClient::new(&self.coordinator.uri(), Duration::from_secs(10)).unwrap();
        let endpoint = LlmEndpoint::new(
            &format!("{}/v1", self.llm.uri()),
            None,
            "test-classifier",
            Duration::from_secs(10),
        )
        .unwrap();
        ExecContext {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            limiter: Arc::new(RateLimiter::new(10_000, 1_000_000_000)),
            stager: Arc::new(LocalStager::new(&self.staging_root)),
            lease: LeaseContext {
                lease_id: "lease-1".to_string(),
                chunk_id: CHUNK_ID.to_string(),
                target_key: "WI/55011".to_string(),
                staging_upload_prefix: "stage/raw/classify/wi/55011/lease-1/".to_string(),
            },
            user_agent: "lh-contributor/test".to_string(),
            brave_api_key: None,
            llm: Some(LlmLane {
                endpoint,
                capability: llm_capability(),
            }),
            search_proxy: Some(api.search_proxy(&creds())),
            lease_lost: lost_rx,
            dry_run: true,
        }
    }
}

fn rand_suffix() -> String {
    let d = Sha256::digest(format!("{:?}", std::time::Instant::now()));
    hex::encode(&d[..4]).to_string()
}

/// Mount the search-proxy mock and assert the wire shape: body-carried auth,
/// chunk id only — the client never supplies queries.
async fn mount_search_proxy(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/search-proxy"))
        .and(body_partial_json(json!({
            "client_id": CLIENT_ID,
            "auth_token": AUTH_TOKEN,
            "chunk_id": CHUNK_ID,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chunk_id": CHUNK_ID,
            "backend": "brave",
            "fetched_at": 1700000000000i64,
            "queries": [{
                "query": "wi 55011 gis parcels",
                "url": "https://api.search.brave.com/res/v1/web/search?q=wi%2055011%20gis%20parcels&count=5",
                "status": 200,
                "sha256": "ab".repeat(32),
                "body": {"web": {"results": [{"url": "https://gis.example.gov/parcels", "title": "Example County GIS"}]}}
            }]
        })))
        .mount(server)
        .await;
}

// ---------------------------------------------------------------------------
// Happy path: proxy -> prompt -> verdict
// ---------------------------------------------------------------------------

#[tokio::test]
async fn classify_full_path_submits_schema_valid_verdict() {
    let h = Harness::new().await;
    mount_search_proxy(&h.coordinator).await;

    // Capture the LLM request body to assert the prompt shape, and return a
    // fenced verdict (proves fence-stripping on the wire path).
    let seen: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let seen_for_mock = seen.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |req: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            *seen_for_mock.lock().unwrap() = Some(body);
            ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{
                    "message": {"role": "assistant",
                        "content": "```json\n{\"verdict\":\"relevant\",\"confidence\":0.82,\"candidate_urls\":[\"https://gis.example.gov/parcels\"]}\n```"}
                }]
            }))
        })
        .mount(&h.llm)
        .await;

    let ctx = h.ctx();
    let contract = parse_contract(classify_contract_value(search_result_set_input()));
    let outcome = execute(&ctx, &contract).await.expect("classify succeeds");

    let m = &outcome.manifest;
    assert_eq!(m.chunk_type, ChunkType::Classify);
    assert_eq!(m.extra["classification"], json!("verdict"));
    assert_eq!(m.extra["verdict"]["verdict"], json!("relevant"));
    assert_eq!(m.extra["verdict"]["confidence"], json!(0.82));
    assert_eq!(
        m.extra["verdict"]["candidate_urls"],
        json!(["https://gis.example.gov/parcels"])
    );
    assert_eq!(m.page_count, 1);
    assert_eq!(m.page_sha256.len(), 1);
    assert!(
        m.page_sha256[0].len() == 64,
        "sha256 hex of the submitted verdict json"
    );
    // The proxy request URL rides in source_urls.
    assert_eq!(
        m.source_urls,
        vec![
            "https://api.search.brave.com/res/v1/web/search?q=wi%2055011%20gis%20parcels&count=5"
                .to_string()
        ]
    );
    // Proposal never stages anything.
    assert!(outcome.evidence_blobs.is_empty());
    // Input provenance + capability echo for operator weighting.
    assert!(m.extra["input_sha256"].as_str().unwrap().len() == 64);
    assert_eq!(m.extra["llm"]["model_family"], json!("qwen3"));
    assert_eq!(m.extra["llm"]["endpoint_kind"], json!("local-sidecar"));

    // LLM wire shape: system+user messages, fixed temperature, no stream,
    // prompt carries the rubric and the input URLs.
    let body = seen.lock().unwrap().clone().expect("llm called");
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["role"], json!("system"));
    assert_eq!(msgs[1]["role"], json!("user"));
    let user = msgs[1]["content"].as_str().unwrap();
    assert!(user.contains("Find official county GIS portals"));
    assert!(user.contains("- official .gov domain"));
    assert!(user.contains("https://gis.example.gov/parcels"));
    assert!(user.contains("\"verdict\":\"relevant\"|\"irrelevant\""));
    assert_eq!(body["temperature"], json!(0.0));
    assert_eq!(body["stream"], json!(false));
    assert_eq!(body["model"], json!("test-classifier"));
    // The endpoint saw no Authorization header (local lane, no key).
    // (wiremock asserts via mocks above; nothing more to check here.)

    let _ = std::fs::remove_dir_all(&h.staging_root);
}

#[tokio::test]
async fn classify_staged_blob_input_grades_without_proxy() {
    let h = Harness::new().await;
    // Seed the staged input blob through the LocalStager.
    let stager = LocalStager::new(&h.staging_root);
    let input = json!({"candidates": [{"url": "https://records.example.gov/elections"}]});
    stager
        .put_object(
            "stage/ops/classify-input/WI/55011/set.json.zst",
            "application/zstd",
            zstd::bulk::compress(input.to_string().as_bytes(), 3).unwrap(),
        )
        .await
        .unwrap();

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "{\"verdict\":\"irrelevant\",\"confidence\":0.33}"
                }
            }]
        })))
        .mount(&h.llm)
        .await;

    let mut ctx = h.ctx();
    // Staged-blob path must not need the proxy.
    ctx.search_proxy = None;
    let contract = parse_contract(classify_contract_value(json!({
        "kind": "staged_blob",
        "staged_blob_key": "stage/ops/classify-input/WI/55011/set.json.zst",
    })));
    let outcome = execute(&ctx, &contract).await.expect("classify succeeds");
    assert_eq!(outcome.manifest.extra["classification"], json!("verdict"));
    assert_eq!(
        outcome.manifest.extra["verdict"]["verdict"],
        json!("irrelevant")
    );
    assert_eq!(outcome.manifest.source_urls, Vec::<String>::new());
    let _ = std::fs::remove_dir_all(&h.staging_root);
}

// ---------------------------------------------------------------------------
// Fail-safe: everything else is no_verdict
// ---------------------------------------------------------------------------

#[tokio::test]
async fn classify_model_garbage_becomes_no_verdict() {
    let h = Harness::new().await;
    mount_search_proxy(&h.coordinator).await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"role": "assistant", "content": "The results look quite relevant to me!"}}]
        })))
        .mount(&h.llm)
        .await;

    let ctx = h.ctx();
    let contract = parse_contract(classify_contract_value(search_result_set_input()));
    let outcome = execute(&ctx, &contract)
        .await
        .expect("still a successful result");
    assert_eq!(
        outcome.manifest.extra["classification"],
        json!("no_verdict")
    );
    assert_eq!(
        outcome.manifest.extra["no_verdict_reason"],
        json!("malformed_verdict")
    );
    assert_eq!(outcome.manifest.page_count, 1);
    assert!(outcome.evidence_blobs.is_empty());
    let _ = std::fs::remove_dir_all(&h.staging_root);
}

#[tokio::test]
async fn classify_out_of_range_confidence_becomes_no_verdict() {
    let h = Harness::new().await;
    mount_search_proxy(&h.coordinator).await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "{\"verdict\":\"relevant\",\"confidence\":1.7}"
                }
            }]
        })))
        .mount(&h.llm)
        .await;

    let ctx = h.ctx();
    let contract = parse_contract(classify_contract_value(search_result_set_input()));
    let outcome = execute(&ctx, &contract)
        .await
        .expect("honest no_verdict, not an error");
    assert_eq!(
        outcome.manifest.extra["classification"],
        json!("no_verdict")
    );
    assert_eq!(
        outcome.manifest.extra["no_verdict_reason"],
        json!("malformed_verdict")
    );
    let _ = std::fs::remove_dir_all(&h.staging_root);
}

#[tokio::test]
async fn classify_llm_http_error_becomes_no_verdict_transport() {
    let h = Harness::new().await;
    mount_search_proxy(&h.coordinator).await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&h.llm)
        .await;

    let ctx = h.ctx();
    let contract = parse_contract(classify_contract_value(search_result_set_input()));
    let outcome = execute(&ctx, &contract)
        .await
        .expect("honest no_verdict, not an error");
    assert_eq!(
        outcome.manifest.extra["classification"],
        json!("no_verdict")
    );
    assert_eq!(
        outcome.manifest.extra["no_verdict_reason"],
        json!("llm_transport")
    );
    let _ = std::fs::remove_dir_all(&h.staging_root);
}

#[tokio::test]
async fn classify_llm_timeout_becomes_no_verdict_timeout() {
    let h = Harness::new().await;
    mount_search_proxy(&h.coordinator).await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "choices": [{"message": {"role": "assistant", "content": "{\"verdict\":\"relevant\",\"confidence\":0.5}"} }]
                }))
                .set_delay(Duration::from_millis(1200)),
        )
        .mount(&h.llm)
        .await;

    // Short-timeout endpoint so the delayed answer triggers a timeout.
    let mut ctx = h.ctx();
    if let Some(lane) = ctx.llm.as_mut() {
        lane.endpoint = LlmEndpoint::new(
            &format!("{}/v1", h.llm.uri()),
            None,
            "test-classifier",
            Duration::from_millis(200),
        )
        .unwrap();
    }

    let contract = parse_contract(classify_contract_value(search_result_set_input()));
    let outcome = execute(&ctx, &contract)
        .await
        .expect("honest no_verdict, not an error");
    assert_eq!(
        outcome.manifest.extra["classification"],
        json!("no_verdict")
    );
    assert_eq!(
        outcome.manifest.extra["no_verdict_reason"],
        json!("llm_timeout")
    );
    let _ = std::fs::remove_dir_all(&h.staging_root);
}

// ---------------------------------------------------------------------------
// Typed failures (operator/client skew, wiring bugs)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn classify_unknown_template_is_typed_failure() {
    let h = Harness::new().await;
    let ctx = h.ctx();
    let mut contract = classify_contract_value(search_result_set_input());
    contract["payload"]["prompt_template_id"] = json!("relevance_superv2");
    let err = execute(&ctx, &parse_contract(contract))
        .await
        .expect_err("must fail typed");
    assert_eq!(err.failure_type, FailureType::ContractUnknownParser);
    assert!(err.details.contains("relevance_superv2"));
}

#[tokio::test]
async fn classify_without_llm_lane_is_typed_client_error() {
    let h = Harness::new().await;
    mount_search_proxy(&h.coordinator).await;
    let mut ctx = h.ctx();
    ctx.llm = None;
    let contract = parse_contract(classify_contract_value(search_result_set_input()));
    let err = execute(&ctx, &contract)
        .await
        .expect_err("no lane configured");
    assert_eq!(err.failure_type, FailureType::ClientError);
    assert!(err.details.contains("LLM lane"));
}

// ---------------------------------------------------------------------------
// Register wire: llm capability declaration
// ---------------------------------------------------------------------------

#[tokio::test]
async fn register_wire_carries_llm_capability_when_configured() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/register"))
        .and(body_partial_json(json!({
            "llm": {
                "model_family": "qwen3",
                "quant": "Q4_K_M",
                "ctx_tokens": 8192,
                "endpoint_kind": "local-sidecar",
            }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "client_id": "id-llm", "auth_token": "tok-llm",
        })))
        .mount(&server)
        .await;

    let api = ApiClient::new(&server.uri(), Duration::from_secs(10)).unwrap();
    let req = lh_contributor_client::runner::register_request_with_llm(Some(&llm_capability()));
    let creds = api.register(&req).await.unwrap();
    assert_eq!(creds.client_id, "id-llm");

    // And the no-llm variant must NOT carry the key at all.
    let server2 = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/register"))
        .respond_with(move |req: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            assert!(
                body.get("llm").is_none(),
                "llm key must be absent when unconfigured"
            );
            ResponseTemplate::new(200).set_body_json(json!({"client_id": "i", "auth_token": "t"}))
        })
        .mount(&server2)
        .await;
    let api2 = ApiClient::new(&server2.uri(), Duration::from_secs(10)).unwrap();
    let _ = api2
        .register(&RegisterRequest {
            platform: "p".to_string(),
            version: "0.1.0".to_string(),
            capabilities: vec![],
            llm: None,
        })
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// Cross-language signature vector: the classify contract signs/verifies
// byte-for-byte under the TS canonicalization (tests/vectors/classify_vector.mjs)
// ---------------------------------------------------------------------------

#[test]
fn classify_contract_payload_matches_typescript() {
    let payload_json = json!({
        "prompt_template_id": "search_result_relevance_v1",
        "rubric": {
            "objective": "Find official county GIS or election-data portals",
            "relevant_criteria": ["official .gov or .us domain", "serves parcel/zoning/election data"],
            "irrelevant_criteria": ["third-party aggregators", "paywalled news articles"],
        },
        "input": {
            "kind": "search_result_set",
            "query_plan": {
                "queries": [{"query": "wi 55011 gis parcels", "max_results": 5}],
                "backend": "brave",
            },
        },
        "verdict_schema": "classify_verdict_v1",
        "expected_verdicts": 3,
    });
    let contract = lh_contributor_client::api::SignedChunkContract {
        core: lh_contributor_client::api::ChunkContract {
            chunk_type: ChunkType::Classify,
            target_key: "WI/55011".to_string(),
            payload: payload_json,
            schema_version: 1,
        },
        expires_at: 1_799_999_999_999,
        operator_signature_hex: String::new(),
        staging_upload_prefix: "stage/raw/classify/wi/55011/lease-1/".to_string(),
    };
    let chunk_id = "c1a551f7-0000-4000-8000-00000000000f";
    let payload = lh_contributor_client::crypto::build_contract_payload(chunk_id, &contract);
    let payload_str = String::from_utf8(payload.clone()).unwrap();

    // Byte-level pin against the TS vector output.
    assert_eq!(
        payload_str,
        concat!(
            "c1a551f7-0000-4000-8000-00000000000f:",
            "{\"payload\":{\"expected_verdicts\":3,\"input\":{\"kind\":\"search_result_set\",\"query_plan\":{\"backend\":\"brave\",\"queries\":[{\"max_results\":5,\"query\":\"wi 55011 gis parcels\"}]}},\"prompt_template_id\":\"search_result_relevance_v1\",\"rubric\":{\"irrelevant_criteria\":[\"third-party aggregators\",\"paywalled news articles\"],\"objective\":\"Find official county GIS or election-data portals\",\"relevant_criteria\":[\"official .gov or .us domain\",\"serves parcel/zoning/election data\"]},\"verdict_schema\":\"classify_verdict_v1\"},\"schema_version\":1,\"staging_upload_prefix\":\"stage/raw/classify/wi/55011/lease-1/\",\"target_key\":\"WI/55011\",\"type\":\"classify\"}:",
            "1799999999999",
        )
    );
    assert_eq!(
        hex::encode(Sha256::digest(&payload)),
        "8d570642bfccf718a638deefc9cc150a56f9fc4f72de705bd6c0c0f0832d2b48"
    );

    // Full chain: node ed25519 signature over those exact bytes verifies
    // under dalek (values from tests/vectors/classify_vector.mjs).
    let pubkey_hex = "3e7127b77afa2f7241ad41278af4d6f8df235710b379c2e03798bc522b150835";
    let sig_hex = "0269089cf00531eb0c32b13cc68352f8c3a74471ca37f8cb6cf67be0f0fbd8b88c5dd12983ba0a536d7e638bd53e5abe1a4f758201e4de401597128b8f85c70a";
    let vk = lh_contributor_client::crypto::parse_public_key_hex(pubkey_hex).unwrap();
    let sig = Signature::from_bytes(&hex::decode(sig_hex).unwrap().try_into().unwrap());
    assert!(
        vk.verify(&payload, &sig).is_ok(),
        "node ed25519 signature must verify under dalek"
    );
}
