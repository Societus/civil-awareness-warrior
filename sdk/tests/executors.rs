//! Executor tests: chunk handlers running against a mock SOURCE host with
//! local (dry-run) staging — the real fetch/hash/compress/stage path, minus
//! the coordinator.

use ed25519_dalek::{Signer, SigningKey};
use lh_contributor_client::api::{ChunkType, FailureType};
use lh_contributor_client::chunk::{execute, ExecContext, LeaseContext};
use lh_contributor_client::rate::RateLimiter;
use lh_contributor_client::staging::LocalStager;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::sync::watch;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn operator_key() -> SigningKey {
    use sha2::Sha256 as H;
    let digest = H::digest(b"lh-executor-test-operator-key");
    SigningKey::from_bytes(&digest.into())
}

fn contract_value(chunk_type: &str, payload: Value) -> Value {
    json!({
        "type": chunk_type,
        "target_key": "WI/55011",
        "schema_version": 1,
        "expires_at": lh_contributor_client::api::unix_ms() + 3600 * 1000,
        "payload": payload,
        "operator_signature_hex": &hex::encode(operator_key().sign(b"unused").to_bytes()),
    })
}

fn exec_ctx(_server: &MockServer, staging_root: &std::path::Path) -> ExecContext {
    let (lost_tx, lost_rx) = watch::channel(false);
    std::mem::forget(lost_tx); // never lost in these tests
    ExecContext {
        http: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap(),
        limiter: Arc::new(RateLimiter::new(10_000, 1_000_000_000)),
        stager: Arc::new(LocalStager::new(staging_root)),
        lease: LeaseContext {
            lease_id: "lease-1".to_string(),
            chunk_id: "chunk-1".to_string(),
            target_key: "WI/55011".to_string(),
            staging_upload_prefix: "stage/raw/test/WI/55011/lease-1/".to_string(),
        },
        user_agent: "lh-contributor/test".to_string(),
        brave_api_key: None,
        llm: None,
        search_proxy: None,
        lease_lost: lost_rx,
        dry_run: true,
    }
}

fn parse_contract(v: Value) -> lh_contributor_client::api::SignedChunkContract {
    serde_json::from_value(v).expect("test contract deserializes")
}

// ---------------------------------------------------------------------------
// ingest: real fetch-plan execution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ingest_fetches_pages_hashes_and_stages() {
    let source = MockServer::start().await;
    let page1_body = "parcel_id,setback_interior\n1,25\n2,30\n";
    let page2_body = "parcel_id,setback_interior\n3,30\n4,35\n";
    // NOTE: set_body_string FIRST, insert_header AFTER — set_body_string
    // stamps its own text/plain content-type that would otherwise override.
    Mock::given(method("GET"))
        .and(path("/v2/parcels"))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(page1_body))
        .mount(&source)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/parcels"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_string(page2_body))
        .mount(&source)
        .await;

    let staging_root = std::env::temp_dir().join(format!("lh-exec-ingest-{}", std::process::id()));
    let source_uri = source.uri();
    let contract = contract_value(
        "ingest",
        json!({
            "fetch_plan": {
                "base_url": format!("{source_uri}/v2"),
                "method": "GET",
                "pages": [
                    {"path": "/parcels", "params": {"page": "1"}, "expect_content_type": "text/plain"},
                    {"path": "/parcels", "params": {"page": "2"}, "expect_content_type": "text/plain"},
                ],
                "rate_limit": {"requests_per_min": 1000, "bytes_per_min": 100_000_000},
            },
            "reconciliation_rules": [
                {"field": "pages", "expected": "non_empty"},
                {"field": "pages", "expected": "exact_count", "params": {"count": 2}},
            ],
        }),
    );

    let ctx = exec_ctx(&source, &staging_root);
    let outcome = execute(&ctx, &parse_contract(contract))
        .await
        .expect("ingest succeeds");

    let m = &outcome.manifest;
    assert_eq!(m.chunk_type, ChunkType::Ingest);
    assert_eq!(m.target, "WI/55011");
    assert_eq!(m.page_count, 2);
    assert_eq!(
        m.page_sha256[0],
        hex::encode(Sha256::digest(page1_body.as_bytes()))
    );
    assert_eq!(
        m.page_sha256[1],
        hex::encode(Sha256::digest(page2_body.as_bytes()))
    );
    assert_eq!(
        m.source_urls,
        vec![
            format!("{source_uri}/v2/parcels?page=1"),
            format!("{source_uri}/v2/parcels?page=2"),
        ]
    );
    assert_eq!(
        m.raw_pages_blob_prefix.as_deref(),
        Some("stage/raw/test/WI/55011/lease-1/")
    );
    assert_eq!(m.extra["blob_encoding"], json!("zstd"));
    // Reconciliation evaluated + passed.
    assert_eq!(
        m.reconciliation.as_ref().unwrap()["rules"][0]["passed"],
        json!(true)
    );
    assert_eq!(
        m.reconciliation.as_ref().unwrap()["rules"][1]["passed"],
        json!(true)
    );

    // Blobs actually staged as zstd under the lease prefix.
    for (i, key) in outcome.evidence_blobs.iter().enumerate() {
        let expected = format!("stage/raw/test/WI/55011/lease-1/pages/{i:04}.zst");
        assert_eq!(key, &expected);
        let raw = match i {
            0 => page1_body.as_bytes(),
            _ => page2_body.as_bytes(),
        };
        let stored = std::fs::read(staging_root.join(&expected)).expect("blob staged");
        let decompressed = zstd::bulk::decompress(&stored, raw.len() * 4).unwrap();
        assert_eq!(decompressed, raw);
    }

    let _ = std::fs::remove_dir_all(staging_root);
}

#[tokio::test]
async fn ingest_content_type_mismatch_is_schema_drift() {
    let source = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/parcels"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("{}")
                .insert_header("Content-Type", "application/json"),
        )
        .mount(&source)
        .await;

    let staging_root = std::env::temp_dir().join(format!("lh-exec-ct-{}", std::process::id()));
    let contract = contract_value(
        "ingest",
        json!({
            "fetch_plan": {
                "base_url": format!("{}/v2", source.uri()),
                "method": "GET",
                "pages": [{"path": "/parcels", "expect_content_type": "text/csv"}],
            },
        }),
    );
    let ctx = exec_ctx(&source, &staging_root);
    let err = execute(&ctx, &parse_contract(contract))
        .await
        .expect_err("must fail");
    assert_eq!(err.failure_type, FailureType::SchemaDrift);
    let _ = std::fs::remove_dir_all(staging_root);
}

#[tokio::test]
async fn ingest_http_500_maps_to_http_5xx() {
    let source = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/parcels"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&source)
        .await;

    let staging_root = std::env::temp_dir().join(format!("lh-exec-5xx-{}", std::process::id()));
    let contract = contract_value(
        "ingest",
        json!({
            "fetch_plan": {
                "base_url": format!("{}/v2", source.uri()),
                "method": "GET",
                "pages": [{"path": "/parcels"}],
            },
        }),
    );
    let ctx = exec_ctx(&source, &staging_root);
    let err = execute(&ctx, &parse_contract(contract))
        .await
        .expect_err("must fail");
    assert_eq!(err.failure_type, FailureType::Http5xx);
    let _ = std::fs::remove_dir_all(staging_root);
}

// ---------------------------------------------------------------------------
// alt_source: availability probing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn alt_source_probes_candidates_and_records_findings() {
    let source = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/gis/parcels"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "application/json")
                .set_body_string(r#"{"features": []}"#),
        )
        .mount(&source)
        .await;
    // A 404 candidate is a FINDING, not a chunk failure.
    Mock::given(method("GET"))
        .and(path("/legacy.zip"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&source)
        .await;

    let staging_root = std::env::temp_dir().join(format!("lh-exec-alt-{}", std::process::id()));
    let uri = source.uri();
    let contract = contract_value(
        "alt_source",
        json!({
            "candidate_urls": [
                format!("{uri}/gis/parcels"),
                format!("{uri}/legacy.zip"),
            ],
            "probe_plan": {"enum_services": false, "count_groupby": false, "null_count": false, "pagination_test": false, "return_centroid_test": false},
        }),
    );

    let ctx = exec_ctx(&source, &staging_root);
    let outcome = execute(&ctx, &parse_contract(contract))
        .await
        .expect("probe succeeds");
    let summary = &outcome.manifest.extra["probe_summary"];
    assert_eq!(summary["candidates"], json!(2));
    // Both candidates transport-answered (200 and 404) → both reachable.
    assert_eq!(summary["reachable"], json!(2));
    assert_eq!(summary["probe_results"][0]["reachable"], json!(true));
    assert_eq!(summary["probe_results"][0]["status"], json!(200));
    // The 404 candidate: reachable but recorded as an http_4xx finding.
    assert_eq!(summary["probe_results"][1]["reachable"], json!(true));
    assert_eq!(
        summary["probe_results"][1]["failure_type"],
        json!("http_4xx")
    );
    let _ = std::fs::remove_dir_all(staging_root);
}

#[tokio::test]
async fn alt_source_extended_probe_flags_fail_fast_without_requests() {
    let source = MockServer::start().await;
    // NO mocks mounted: any request would panic the mock server in debug.
    let staging_root = std::env::temp_dir().join(format!("lh-exec-alt2-{}", std::process::id()));
    let contract = contract_value(
        "alt_source",
        json!({
            "candidate_urls": ["https://unreachable.invalid/x"],
            "probe_plan": {"enum_services": true, "count_groupby": false, "null_count": false, "pagination_test": false, "return_centroid_test": false},
        }),
    );
    let ctx = exec_ctx(&source, &staging_root);
    let err = execute(&ctx, &parse_contract(contract))
        .await
        .expect_err("must fail fast");
    assert_eq!(err.failure_type, FailureType::ContractUnknownParser);
    assert!(err.details.contains("enum_services"));
    let _ = std::fs::remove_dir_all(staging_root);
}

// ---------------------------------------------------------------------------
// examination / search: typed-failure stubs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn examination_unknown_parser_fails_with_contract_unknown_parser() {
    let source = MockServer::start().await;
    let staging_root = std::env::temp_dir().join(format!("lh-exec-exam-{}", std::process::id()));
    let contract = contract_value(
        "examination",
        json!({
            "parser_id": "municode_extract_v1",
            "staged_blob_key": "stage/ops/examination/WI/55011/blob.zst",
            "expected_measurements": [{"name": "page_count", "expected_type": "number"}],
        }),
    );
    let ctx = exec_ctx(&source, &staging_root);
    let err = execute(&ctx, &parse_contract(contract))
        .await
        .expect_err("registry is empty");
    assert_eq!(err.failure_type, FailureType::ContractUnknownParser);
    assert!(err.details.contains("municode_extract_v1"));
    let _ = std::fs::remove_dir_all(staging_root);
}

#[tokio::test]
async fn search_unimplemented_backend_fails_typed() {
    let source = MockServer::start().await;
    let staging_root = std::env::temp_dir().join(format!("lh-exec-search-{}", std::process::id()));
    let contract = contract_value(
        "search",
        json!({
            "query_plan": {
                "queries": [{"query": "municode WI 55011 zoning", "max_results": 5}],
                "backend": "duckduckgo",
            },
            "source_class_priority": ["official_enr", "certified_pdf"],
        }),
    );
    let ctx = exec_ctx(&source, &staging_root);
    let err = execute(&ctx, &parse_contract(contract))
        .await
        .expect_err("must fail typed");
    assert_eq!(err.failure_type, FailureType::ContractUnknownParser);
    assert!(err.details.contains("duckduckgo"));
    let _ = std::fs::remove_dir_all(staging_root);
}

// ---------------------------------------------------------------------------
// state_walk: roster fetch + mint validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn state_walk_fetches_roster_and_stages_features() {
    let source = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/junctures/WI/55011/city-of-example"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(
                    "<html><head><title>Municode — City of Example WI</title></head>\
                     <body><h1>Zoning Code</h1><p>setbacks</p></body></html>",
                )
                .insert_header("Content-Type", "text/html"),
        )
        .mount(&source)
        .await;

    let staging_root = std::env::temp_dir().join(format!("lh-exec-walk-{}", std::process::id()));
    let uri = source.uri();
    let contract = contract_value(
        "state_walk",
        json!({
            "roster_slice": [
                {"slug": "wi/55011/city-of-example", "display_name": "City of Example", "url": format!("{uri}/junctures/WI/55011/city-of-example")},
            ],
            "walk_recipe": {
                "title_line_pattern": "^Municode",
                "mint_relation": "^wi/\\d{5}/[a-z0-9-]+$",
                "heading_capture": true,
            },
        }),
    );

    let ctx = exec_ctx(&source, &staging_root);
    let outcome = execute(&ctx, &parse_contract(contract))
        .await
        .expect("walk succeeds");
    assert_eq!(outcome.manifest.page_count, 1);
    assert_eq!(
        outcome.manifest.staged_features_blob.as_deref(),
        Some("stage/raw/test/WI/55011/lease-1/features.zst")
    );
    let blob_path = staging_root.join("stage/raw/test/WI/55011/lease-1/features.zst");
    assert!(blob_path.exists(), "features blob must be staged");
    let _ = std::fs::remove_dir_all(staging_root);
}

#[tokio::test]
async fn state_walk_mint_violation_is_schema_drift() {
    let source = MockServer::start().await;
    let staging_root = std::env::temp_dir().join(format!("lh-exec-walk2-{}", std::process::id()));
    let contract = contract_value(
        "state_walk",
        json!({
            "roster_slice": [
                {"slug": "NOT A MINT", "display_name": "Bad", "url": "https://unreachable.invalid/x"},
            ],
            "walk_recipe": {
                "title_line_pattern": "",
                "mint_relation": "^wi/\\d{5}/[a-z0-9-]+$",
                "heading_capture": false,
            },
        }),
    );
    let ctx = exec_ctx(&source, &staging_root);
    let err = execute(&ctx, &parse_contract(contract))
        .await
        .expect_err("mint must fail");
    assert_eq!(err.failure_type, FailureType::SchemaDrift);
    assert!(err.details.contains("mint_relation"));
    let _ = std::fs::remove_dir_all(staging_root);
}
