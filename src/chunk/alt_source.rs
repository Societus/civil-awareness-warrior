//! `alt_source` executor: availability-probe candidate URLs for a county.
//!
//! A candidate that answers (any status, even 404) is a FINDING, not a chunk
//! failure — the probe verdict is the product. Only total transport failure
//! of every candidate fails the chunk. Extended probe-plan capabilities
//! (enum_services, count_groupby, null_count, pagination_test,
//! return_centroid_test) are family-specific API behaviors not implemented
//! in v0: they fail the chunk with `contract_unknown_parser` BEFORE any
//! politeness budget is spent, so the operator can re-issue to a capable
//! client version.

use super::{fetch_page, manifest_scaffold, parse_payload, ExecContext, Outcome};
use crate::api::{AltSourcePayload, FailureType};
use crate::error::ChunkFailure;
use crate::staging::put_zstd;
use sha2::{Digest, Sha256};

pub(crate) async fn execute(
    ctx: &ExecContext,
    contract: &crate::api::SignedChunkContract,
) -> Result<Outcome, ChunkFailure> {
    let payload: AltSourcePayload = parse_payload(contract)?;

    // Fail fast on unimplemented probe capabilities (no budget spent).
    if let Some(plan) = &payload.probe_plan {
        let mut unsupported = Vec::new();
        if plan.enum_services {
            unsupported.push("enum_services");
        }
        if plan.count_groupby {
            unsupported.push("count_groupby");
        }
        if plan.null_count {
            unsupported.push("null_count");
        }
        if plan.pagination_test {
            unsupported.push("pagination_test");
        }
        if plan.return_centroid_test {
            unsupported.push("return_centroid_test");
        }
        if !unsupported.is_empty() {
            return Err(ChunkFailure::new(
                FailureType::ContractUnknownParser,
                format!(
                    "probe plan requires capabilities not implemented in client v{}: {}",
                    crate::client_version(),
                    unsupported.join(", ")
                ),
            ));
        }
    }

    if payload.candidate_urls.is_empty() {
        return Err(ChunkFailure::new(
            FailureType::SchemaDrift,
            "alt_source contract has no candidate_urls",
        ));
    }

    let prefix = &ctx.lease.staging_upload_prefix;
    let mut probes: Vec<serde_json::Value> = Vec::with_capacity(payload.candidate_urls.len());
    let mut source_urls = Vec::new();
    let mut hashes = Vec::new();
    let mut reachable = 0usize;

    for (i, url) in payload.candidate_urls.iter().enumerate() {
        match fetch_page(ctx, reqwest::Method::GET, url, &[], None, None).await {
            Ok(page) => {
                reachable += 1;
                let hash = hex::encode(Sha256::digest(&page.bytes));
                hashes.push(hash.clone());
                source_urls.push(page.final_url.clone());
                probes.push(serde_json::json!({
                    "candidate": i,
                    "url": url,
                    "final_url": page.final_url,
                    "reachable": true,
                    "status": 200,
                    "content_type": page.content_type,
                    "bytes": page.bytes.len(),
                    "sha256_raw": hash,
                }));
            }
            Err(failure) => {
                // A candidate that ANSWERS with any status (4xx/5xx/bot wall)
                // is transport-reachable and the status is a finding; only
                // TRANSPORT-level failures (unreachable/timeout) count as
                // unreachable.
                let transport_level = matches!(
                    failure.failure_type,
                    FailureType::SourceUnreachable | FailureType::ClientTimeout
                );
                if !transport_level {
                    reachable += 1;
                }
                probes.push(serde_json::json!({
                    "candidate": i,
                    "url": url,
                    "reachable": !transport_level,
                    "failure_type": failure.failure_type.to_string(),
                    "note": failure.details,
                }));
            }
        }
    }

    if reachable == 0 {
        return Err(ChunkFailure::new(
            FailureType::SourceUnreachable,
            format!(
                "all {} candidate URLs transport-unreachable (deterministic re-probe will agree)",
                payload.candidate_urls.len()
            ),
        ));
    }

    // Evidence blob: the full probe report.
    let report = serde_json::json!({
        "target": contract.core.target_key,
        "probe_plan_executed": "base_availability",
        "probes": probes,
    });
    let report_blob = put_zstd(
        &*ctx.stager,
        prefix,
        "probe_report",
        report.to_string().as_bytes(),
    )
    .await
    .map_err(|e| {
        ChunkFailure::new(
            FailureType::ClientError,
            format!("staging probe report failed: {e}"),
        )
    })?;

    let mut manifest = manifest_scaffold(contract, source_urls, hashes);
    manifest.staged_features_blob = Some(report_blob.key.clone());
    manifest
        .extra
        .insert("blob_encoding".into(), serde_json::json!("zstd"));
    manifest.extra.insert(
        "probe_summary".into(),
        serde_json::json!({
            "candidates": payload.candidate_urls.len(),
            "reachable": reachable,
            "probe_results": probes,
        }),
    );

    Ok(Outcome {
        manifest,
        evidence_blobs: vec![report_blob.key],
    })
}
