//! Integration tests: the client against a mock lh-tracker coordinator
//! (wiremock), covering registration, lease polling + backoff, signature
//! verification, heartbeat, result submission and fail reports.

use ed25519_dalek::{Signer, SigningKey};
use lh_contributor_client::api::{
    ApiClient, ChunkType, Credentials, FailRequest, FailureType, LeasePoll, RegisterRequest,
    ResultSubmit,
};
use lh_contributor_client::crypto::verify_lease_contract;
use lh_contributor_client::error::ApiError;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Deterministic test signing key (operator stand-in).
fn operator_key() -> SigningKey {
    use sha2::Sha256 as H;
    let digest = H::digest(b"lh-integration-test-operator-key");
    SigningKey::from_bytes(&digest.into())
}

fn pubkey_hex() -> String {
    hex::encode(operator_key().verifying_key().to_bytes())
}

/// Build a signed contract Value exactly as stored in chunks.contract_json
/// (flattened envelope fields + embedded signature).
fn signed_contract_value(chunk_id: &str, chunk_type: &str, lease_expires_ms: i64) -> Value {
    let now = lh_contributor_client::api::unix_ms();
    let contract_expires = now + 2 * 3600 * 1000;
    let mut contract = json!({
        "type": chunk_type,
        "target_key": "WI/55011",
        "schema_version": 1,
        "expires_at": contract_expires,
        "payload": {
            "fetch_plan": {
                "base_url": "https://gis.example.test/v2",
                "method": "GET",
                "pages": [
                    {"path": "/parcels", "params": {"page": "1"}, "expect_content_type": "text/csv"},
                ],
                "rate_limit": {"requests_per_min": 120, "bytes_per_min": 5_000_000},
            },
        },
    });
    // Sign the core (contract minus envelope fields) over the canonical form.
    let core: Value = {
        let mut v = contract.clone();
        v.as_object_mut().unwrap().remove("expires_at");
        v
    };
    let payload = format!(
        "{chunk_id}:{}:{contract_expires}",
        lh_contributor_client::crypto::canonical_json(&core)
    );
    let sig = operator_key().sign(payload.as_bytes());
    contract["operator_signature_hex"] = json!(hex::encode(sig.to_bytes()));
    let _ = lease_expires_ms; // freshness asserted by caller via verify_lease_contract
    contract
}

/// Canonical JSON for signing in tests: delegates to the crate's
/// implementation, which is pinned against the TS `canonicalJson` in
/// src/crypto.rs tests.
fn lease_response_json(chunk_type: &str, slot_note: &str) -> Value {
    let now = lh_contributor_client::api::unix_ms();
    let chunk_id = format!("11111111-2222-4333-8444-55556666777{slot_note}");
    let contract = signed_contract_value(&chunk_id, chunk_type, now + 3600 * 1000);
    json!({
        "lease_id": format!("lease-{}", slot_note),
        "chunk_id": chunk_id,
        "contract": contract,
        "expires_at": now + 3600 * 1000,
        "heartbeat_interval_sec": 60,
        "staging_upload_prefix": format!("stage/raw/{chunk_type}/WI/55011/lease-{}/", slot_note),
        "rate_budget": {"requests_per_min": 120, "bytes_per_min": 5_000_000},
    })
}

fn client(server: &MockServer) -> ApiClient {
    ApiClient::new(&server.uri(), std::time::Duration::from_secs(10)).unwrap()
}

// ---------------------------------------------------------------------------
// Registration flow
// ---------------------------------------------------------------------------

#[tokio::test]
async fn register_returns_credentials() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/register"))
        .and(body_partial_json(json!({
            "platform": "linux-x86_64-rust",
            "capabilities": ["ingest", "search"],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "client_id": "id-123",
            "auth_token": "tok-abc",
        })))
        .mount(&server)
        .await;

    let creds = client(&server)
        .register(&RegisterRequest {
            platform: "linux-x86_64-rust".to_string(),
            version: "0.1.0".to_string(),
            capabilities: vec![ChunkType::Ingest, ChunkType::Search],
        })
        .await
        .unwrap();
    assert_eq!(creds.client_id, "id-123");
    assert_eq!(creds.auth_token, "tok-abc");
}

#[tokio::test]
async fn register_rate_limit_maps_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/register"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "error": "rate_limited",
            "retry_after_sec": 720,
        })))
        .mount(&server)
        .await;

    let err = client(&server)
        .register(&RegisterRequest {
            platform: "p".to_string(),
            version: "0.1.0".to_string(),
            capabilities: vec![],
        })
        .await
        .unwrap_err();
    match err {
        ApiError::RateLimited { retry_after_sec: Some(720) } => {}
        other => panic!("expected RateLimited(720), got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Lease polling + backoff
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lease_backoff_then_full_lease_with_signature_verification() {
    let server = MockServer::start().await;
    let no_work = ResponseTemplate::new(200).set_body_json(json!({ "backoff_sec": 5 }));
    Mock::given(method("POST"))
        .and(path("/v1/lease"))
        .and(body_partial_json(json!({ "slot_id": 0 })))
        .respond_with(no_work)
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let work = ResponseTemplate::new(200).set_body_json(lease_response_json("ingest", "1"));
    Mock::given(method("POST"))
        .and(path("/v1/lease"))
        .respond_with(work)
        .mount(&server)
        .await;

    let api = client(&server);
    let creds = Credentials {
        client_id: "c1".to_string(),
        auth_token: "t1".to_string(),
    };

    let first = api.lease(&creds, 0, &[ChunkType::Ingest]).await.unwrap();
    let backoff = match first {
        LeasePoll::Backoff { backoff_sec } => backoff_sec,
        other => panic!("expected backoff, got {other:?}"),
    };
    assert_eq!(backoff, 5);

    let second = api.lease(&creds, 0, &[ChunkType::Ingest]).await.unwrap();
    let lease = match second {
        LeasePoll::Lease(lease) => lease,
        other => panic!("expected lease, got {other:?}"),
    };
    assert_eq!(lease.heartbeat_interval_sec, 60);
    assert!(lease.staging_upload_prefix.ends_with("/"));

    // The lease's embedded operator signature must verify + be fresh.
    verify_lease_contract(&pubkey_hex(), &lease, lh_contributor_client::api::unix_ms())
        .expect("lease signature + freshness must verify");
}

#[tokio::test]
async fn lease_auth_failures_map_to_typed_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/lease"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error": "unauthorized"})))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/lease"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"error": "client suspended"})))
        .mount(&server)
        .await;

    let api = client(&server);
    let creds = Credentials {
        client_id: "c1".to_string(),
        auth_token: "wrong".to_string(),
    };
    assert!(matches!(
        api.lease(&creds, 0, &[ChunkType::Ingest]).await,
        Err(ApiError::Unauthorized)
    ));
    assert!(matches!(
        api.lease(&creds, 0, &[ChunkType::Ingest]).await,
        Err(ApiError::Forbidden)
    ));
}

// ---------------------------------------------------------------------------
// Signature verification rejects tampering
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tampered_contract_fails_verification() {
    let server = MockServer::start().await;
    let mut lease_json = lease_response_json("ingest", "2");
    // Tamper with the signed payload: bump schema_version.
    lease_json["contract"]["schema_version"] = json!(2);
    Mock::given(method("POST"))
        .and(path("/v1/lease"))
        .respond_with(ResponseTemplate::new(200).set_body_json(lease_json))
        .mount(&server)
        .await;

    let api = client(&server);
    let creds = Credentials {
        client_id: "c1".to_string(),
        auth_token: "t1".to_string(),
    };
    let lease = match api.lease(&creds, 0, &[ChunkType::Ingest]).await.unwrap() {
        LeasePoll::Lease(l) => l,
        other => panic!("expected lease, got {other:?}"),
    };
    let err = verify_lease_contract(&pubkey_hex(), &lease, lh_contributor_client::api::unix_ms())
        .expect_err("tampered contract must fail verification");
    assert_eq!(err, lh_contributor_client::error::LeaseVerifyError::BadSignature);
}

#[tokio::test]
async fn lease_outliving_contract_fails_freshness() {
    let server = MockServer::start().await;
    let mut lease_json = lease_response_json("ingest", "3");
    // Lease expires AFTER the contract's signed validity window.
    lease_json["expires_at"] = json!(lease_json["contract"]["expires_at"].as_i64().unwrap() + 1000);
    Mock::given(method("POST"))
        .and(path("/v1/lease"))
        .respond_with(ResponseTemplate::new(200).set_body_json(lease_json))
        .mount(&server)
        .await;

    let api = client(&server);
    let creds = Credentials {
        client_id: "c1".to_string(),
        auth_token: "t1".to_string(),
    };
    let lease = match api.lease(&creds, 0, &[ChunkType::Ingest]).await.unwrap() {
        LeasePoll::Lease(l) => l,
        other => panic!("expected lease, got {other:?}"),
    };
    let err = verify_lease_contract(&pubkey_hex(), &lease, lh_contributor_client::api::unix_ms())
        .expect_err("lease outliving contract must fail");
    assert_eq!(
        err,
        lh_contributor_client::error::LeaseVerifyError::LeaseOutlivesContract
    );
}

// ---------------------------------------------------------------------------
// Heartbeat / result / fail / contract refresh
// ---------------------------------------------------------------------------

#[tokio::test]
async fn heartbeat_ok_and_expired() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/heartbeat"))
        .and(body_partial_json(json!({"lease_id": "live"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "lease_expires_at": 1234567890123i64,
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/heartbeat"))
        .and(body_partial_json(json!({"lease_id": "dead"})))
        .respond_with(ResponseTemplate::new(410).set_body_json(json!({"error": "lease_expired"})))
        .mount(&server)
        .await;

    let api = client(&server);
    let hb = api.heartbeat("c1", "live", Some("executing")).await.unwrap();
    assert!(hb.ok);
    assert_eq!(hb.lease_expires_at, 1234567890123);

    assert!(matches!(
        api.heartbeat("c1", "dead", None).await,
        Err(ApiError::LeaseExpired)
    ));
}

#[tokio::test]
async fn result_submission_201_and_conflict() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/result"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "result_id": "res-1", "status": "staged",
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/result"))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "error": "no active lease for this chunk and client",
        })))
        .mount(&server)
        .await;

    let api = client(&server);
    let submit = ResultSubmit {
        chunk_id: "11111111-2222-4333-8444-555566667771".to_string(),
        client_id: "c1".to_string(),
        started_at: 1000,
        completed_at: 2000,
        manifest: lh_contributor_client::api::WorkerManifest {
            chunk_type: ChunkType::Ingest,
            target: "WI/55011".to_string(),
            source_urls: vec!["https://gis.example.test/v2/parcels".to_string()],
            page_count: 1,
            page_sha256: vec![hex::encode(Sha256::digest(b"page"))],
            reconciliation: None,
            staged_features_blob: None,
            raw_pages_blob_prefix: Some("stage/raw/ingest/WI/55011/l1/".to_string()),
            extra: [
                ("blob_encoding".to_string(), json!("zstd")),
            ]
            .into_iter()
            .collect(),
        },
        evidence_blobs: vec!["stage/raw/ingest/WI/55011/l1/pages/0000.zst".to_string()],
    };

    let receipt = api.submit_result(&submit).await.unwrap();
    assert_eq!(receipt.status, "staged");
    assert!(matches!(
        api.submit_result(&submit).await,
        Err(ApiError::Conflict(_))
    ));
}

#[tokio::test]
async fn fail_report_ok_and_lease_gone() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/fail"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/fail"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"error": "lease_not_found"})))
        .mount(&server)
        .await;

    let api = client(&server);
    let req = FailRequest {
        lease_id: "lease-9".to_string(),
        client_id: "c1".to_string(),
        failure_type: FailureType::Http5xx,
        details: Some("503 from county GIS".to_string()),
    };
    api.report_fail(&req).await.unwrap();
    assert!(matches!(
        api.report_fail(&req).await,
        Err(ApiError::NotFound(_))
    ));
}

#[tokio::test]
async fn contract_refresh_serves_verifiable_contract() {
    let server = MockServer::start().await;
    let chunk_id = "11111111-2222-4333-8444-555566667774";
    let contract = signed_contract_value(chunk_id, "ingest", 0);
    Mock::given(method("GET"))
        .and(path(format!("/v1/chunk/{chunk_id}/contract")))
        .respond_with(ResponseTemplate::new(200).set_body_json(contract))
        .mount(&server)
        .await;

    let api = client(&server);
    let fetched = api.fetch_contract(chunk_id).await.unwrap();
    assert_eq!(fetched.core.chunk_type, ChunkType::Ingest);
    assert_eq!(fetched.core.target_key, "WI/55011");
    // Client-side verification on the refreshed copy.
    assert!(lh_contributor_client::crypto::verify_contract(
        &pubkey_hex(),
        chunk_id,
        &fetched
    ));
}

#[tokio::test]
async fn lease_poll_query_params_carry_capabilities() {
    let server = MockServer::start().await;
    let seen = std::sync::Arc::new(std::sync::Mutex::new(None::<Value>));
    let seen_for_mock = seen.clone();
    Mock::given(method("POST"))
        .and(path("/v1/lease"))
        .respond_with(move |req: &wiremock::Request| {
            // Capture the request body for inspection after the test.
            let body = serde_json::from_slice::<Value>(&req.body).unwrap_or(Value::Null);
            *seen_for_mock.lock().unwrap() = Some(body);
            ResponseTemplate::new(200).set_body_json(json!({"backoff_sec": 1}))
        })
        .mount(&server)
        .await;
    let api = client(&server);
    let creds = Credentials {
        client_id: "c9".to_string(),
        auth_token: "t9".to_string(),
    };
    let _ = api
        .lease(&creds, 7, &[ChunkType::StateWalk, ChunkType::Search])
        .await
        .unwrap();
    let body = seen.lock().unwrap().clone().expect("request captured");
    assert_eq!(body["client_id"], "c9");
    assert_eq!(body["auth_token"], "t9");
    assert_eq!(body["slot_id"], 7);
    assert_eq!(
        body["capabilities"],
        json!(["state_walk", "search"])
    );
}
