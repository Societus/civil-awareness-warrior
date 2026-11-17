//! `ingest` executor: run a declarative county fetch plan.
//!
//! The atomic unit is ONE county fetch plan (all pages). Every page is
//! fetched politely (contract rate budget ∩ LH_RATE_LIMIT), hashed,
//! zstd-compressed and staged under the lease prefix; the manifest carries
//! raw-page sha256s for coordinator-side byte-hash verification. Any page
//! failure fails the whole chunk — a partial county fetch is not a result.

use super::{fetch_page, manifest_scaffold, parse_payload, ExecContext, Outcome};
use crate::api::{FailureType, IngestPayload};
use crate::crypto::canonical_json;
use crate::error::ChunkFailure;
use crate::staging::put_zstd;
use sha2::{Digest, Sha256};
use url::Url;

pub(crate) async fn execute(
    ctx: &ExecContext,
    contract: &crate::api::SignedChunkContract,
) -> Result<Outcome, ChunkFailure> {
    let payload: IngestPayload = parse_payload(contract)?;
    let fp = &payload.fetch_plan;

    let method = match fp.method.to_ascii_uppercase().as_str() {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        other => {
            return Err(ChunkFailure::new(
                FailureType::SchemaDrift,
                format!("fetch_plan.method {other:?} not supported (GET/POST only)"),
            ))
        }
    };

    // Contract headers -> request headers (Value::String only).
    let headers: Vec<(String, String)> = fp
        .headers
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
        .collect();

    let prefix = &ctx.lease.staging_upload_prefix;
    let mut page_hashes: Vec<String> = Vec::with_capacity(fp.pages.len());
    let mut source_urls: Vec<String> = Vec::with_capacity(fp.pages.len());
    let mut page_notes: Vec<serde_json::Value> = Vec::new();
    let mut evidence: Vec<String> = Vec::new();

    for (i, page) in fp.pages.iter().enumerate() {
        let mut url = build_url(&fp.base_url, &page.path)?;
        if !page.params.is_empty() {
            let pairs: Vec<(String, String)> = page
                .params
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect();
            url.query_pairs_mut().extend_pairs(pairs);
        }

        let fetched = fetch_page(
            ctx,
            method.clone(),
            url.as_str(),
            &headers,
            page.body.as_deref(),
            page.expect_content_type.as_deref(),
        )
        .await?;

        let raw_hash = hex::encode(Sha256::digest(&fetched.bytes));
        let blob = put_zstd(
            &*ctx.stager,
            prefix,
            &format!("pages/{i:04}"),
            &fetched.bytes,
        )
        .await
        .map_err(|e| {
            ChunkFailure::new(
                FailureType::ClientError,
                format!("staging page {i} failed: {e}"),
            )
        })?;
        evidence.push(blob.key.clone());

        page_hashes.push(raw_hash);
        source_urls.push(fetched.final_url.clone());
        page_notes.push(serde_json::json!({
            "page": i,
            "requested_url": fetched.requested_url,
            "final_url": fetched.final_url,
            "status": 200,
            "content_type": fetched.content_type,
            "raw_bytes": fetched.bytes.len(),
            "blob_key": blob.key,
        }));
    }

    // Reconciliation rules: evaluate what a generic declarative client can,
    // record the rest for operator/verifier-side evaluation.
    let mut rules_report: Vec<serde_json::Value> = Vec::new();
    let mut all_evaluated_passed = true;
    for rule in &payload.reconciliation_rules {
        let entry = match rule.expected.as_str() {
            "non_empty" => {
                // Every page body must be non-empty.
                let non_empty = fp.pages.len() == page_notes.len()
                    && page_notes
                        .iter()
                        .all(|p| p["raw_bytes"].as_u64().unwrap_or(0) > 0);
                all_evaluated_passed &= non_empty;
                serde_json::json!({
                    "field": rule.field,
                    "expected": rule.expected,
                    "evaluated": true,
                    "passed": non_empty,
                })
            }
            "exact_count" => {
                let expected_count = rule
                    .params
                    .as_ref()
                    .and_then(|p| p.get("count"))
                    .and_then(|c| c.as_i64());
                match expected_count {
                    Some(n) => {
                        let passed = n == page_hashes.len() as i64;
                        all_evaluated_passed &= passed;
                        serde_json::json!({
                            "field": rule.field,
                            "expected": rule.expected,
                            "evaluated": true,
                            "passed": passed,
                            "expected_count": n,
                            "actual_count": page_hashes.len(),
                        })
                    }
                    None => serde_json::json!({
                        "field": rule.field,
                        "expected": rule.expected,
                        "evaluated": false,
                        "note": "missing params.count; operator/verifier-side",
                    }),
                }
            }
            // staged_plus_guard_plus_nulls_equals_live needs parsed county
            // records plus a live re-fetch — verifier-side by design.
            _ => serde_json::json!({
                "field": rule.field,
                "expected": rule.expected,
                "evaluated": false,
                "note": "operator/verifier-side rule",
            }),
        };
        rules_report.push(entry);
    }

    if !all_evaluated_passed {
        return Err(ChunkFailure::new(
            FailureType::SchemaDrift,
            format!(
                "reconciliation rules failed: {}",
                canonical_json(&serde_json::Value::Array(
                    rules_report
                        .iter()
                        .filter(|r| r["evaluated"] == serde_json::json!(true)
                            && r["passed"] == serde_json::json!(false))
                        .cloned()
                        .collect()
                ))
            ),
        ));
    }

    let mut manifest = manifest_scaffold(contract, source_urls, page_hashes);
    manifest.reconciliation = Some(serde_json::json!({
        "rules": rules_report,
    }));
    manifest.raw_pages_blob_prefix = Some(prefix.to_string());
    manifest
        .extra
        .insert("blob_encoding".into(), serde_json::json!("zstd"));
    manifest
        .extra
        .insert("pages".into(), serde_json::Value::Array(page_notes));

    Ok(Outcome {
        manifest,
        evidence_blobs: evidence,
    })
}

/// base_url + path string concatenation (the operator's contract semantics:
/// paths are relative to the plan's base, appended verbatim).
fn build_url(base_url: &str, path: &str) -> Result<Url, ChunkFailure> {
    let joined = if path.starts_with('/') {
        format!("{base_url}{path}")
    } else {
        format!("{base_url}/{path}")
    };
    Url::parse(&joined).map_err(|e| {
        ChunkFailure::new(
            FailureType::SchemaDrift,
            format!("bad fetch plan url {joined:?}: {e}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_url_appends_paths_verbatim() {
        assert_eq!(
            build_url("https://gis.example.com/v2", "/parcels?page=1")
                .unwrap()
                .as_str(),
            "https://gis.example.com/v2/parcels?page=1"
        );
        assert!(build_url("https://gis.example.com/v2", "rel").is_ok());
        assert!(build_url("not a url", "/x").is_err());
    }
}
