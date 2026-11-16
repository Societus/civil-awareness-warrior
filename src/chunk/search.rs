//! `search` executor: run a query plan against a search backend, bank the
//! result URLs + raw responses.
//!
//! v0 backends:
//! - `custom`: GET a contract-supplied endpoint per query. If the endpoint
//!   contains a `{query}` placeholder it is percent-encoded-substituted;
//!   otherwise the query rides as the `q` parameter. Result URLs are
//!   harvested from any `url`/`link` string fields in the JSON response.
//! - `brave`: Brave Search API (`LH_BRAVE_API_KEY` env required).
//!
//! `bing`/`duckduckgo` are recognized but not implemented — typed
//! `contract_unknown_parser` failure so the operator sees the gap.

use super::{manifest_scaffold, parse_payload, ExecContext, Outcome};
use crate::api::{FailureType, SearchPayload};
use crate::error::ChunkFailure;
use crate::staging::put_zstd;
use sha2::{Digest, Sha256};

fn harvest_urls(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                harvest_urls(item, out);
            }
        }
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                if (k == "url" || k == "link") && v.is_string() {
                    if let Some(u) = v.as_str() {
                        out.push(u.to_string());
                    }
                } else {
                    harvest_urls(v, out);
                }
            }
        }
        _ => {}
    }
}

pub(crate) async fn execute(
    ctx: &ExecContext,
    contract: &crate::api::SignedChunkContract,
) -> Result<Outcome, ChunkFailure> {
    let payload: SearchPayload = parse_payload(contract)?;
    let qp = &payload.query_plan;

    if qp.queries.is_empty() {
        return Err(ChunkFailure::new(
            FailureType::SchemaDrift,
            "search contract has no queries",
        ));
    }

    let prefix = &ctx.lease.staging_upload_prefix;
    let mut source_urls: Vec<String> = Vec::new();
    let mut hashes: Vec<String> = Vec::new();
    let mut per_query: Vec<serde_json::Value> = Vec::new();
    let mut evidence_keys = Vec::new();

    for (i, spec) in qp.queries.iter().enumerate() {
        let (url, headers) = match qp.backend.as_str() {
            "custom" => {
                let endpoint = qp.custom_endpoint.as_deref().ok_or_else(|| {
                    ChunkFailure::new(
                        FailureType::SchemaDrift,
                        "backend=custom requires query_plan.custom_endpoint",
                    )
                })?;
                let mut req_url = if endpoint.contains("{query}") {
                    endpoint.replace(
                        "{query}",
                        &url::form_urlencoded::Serializer::new(String::new())
                            .append_pair("", &spec.query)
                            .finish()[1..],
                    )
                } else {
                    let sep = if endpoint.contains('?') { '&' } else { '?' };
                    format!("{endpoint}{sep}q={}", encode_query(&spec.query))
                };
                if !spec.filters.is_empty() {
                    let pairs: Vec<(String, String)> = spec
                        .filters
                        .iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect();
                    let sep = if req_url.contains('?') { '&' } else { '?' };
                    let encoded = url::form_urlencoded::Serializer::new(String::new())
                        .extend_pairs(pairs)
                        .finish();
                    req_url.push_str(&format!("{sep}{encoded}"));
                }
                (req_url, Vec::new())
            }
            "brave" => {
                let key = ctx.brave_api_key.as_deref().ok_or_else(|| {
                    ChunkFailure::new(
                        FailureType::ContractUnknownParser,
                        "backend=brave requires LH_BRAVE_API_KEY in the environment",
                    )
                })?;
                (
                    format!(
                        "https://api.search.brave.com/res/v1/web/search?q={}&count={}",
                        encode_query(&spec.query),
                        spec.max_results.clamp(1, 20)
                    ),
                    vec![
                        ("Accept".to_string(), "application/json".to_string()),
                        ("Accept-Encoding".to_string(), "gzip".to_string()),
                        ("X-Subscription-Token".to_string(), key.to_string()),
                    ],
                )
            }
            other => {
                return Err(ChunkFailure::new(
                    FailureType::ContractUnknownParser,
                    format!(
                        "search backend {other:?} not implemented in client v{} (custom, brave available)",
                        crate::client_version()
                    ),
                ));
            }
        };

        ctx.limiter.acquire().await;
        if ctx.lease_is_lost() {
            return Err(ChunkFailure::new(
                FailureType::ClientError,
                "lease lost mid-execution",
            ));
        }
        let parsed = url::Url::parse(&url).map_err(|e| {
            ChunkFailure::new(FailureType::SchemaDrift, format!("bad query url {url:?}: {e}"))
        })?;
        let mut req = ctx.http.get(parsed);
        for (k, v) in &headers {
            req = req.header(k, v);
        }
        req = req.header(reqwest::header::USER_AGENT, &ctx.user_agent);
        let resp = req.send().await.map_err(|e| super::failure_for_transport(&e, &url))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(super::failure_for_status(status, &url));
        }
        let body = resp.bytes().await.map_err(|e| super::failure_for_transport(&e, &url))?.to_vec();
        ctx.limiter.record_bytes(body.len() as u64).await;

        hashes.push(hex::encode(Sha256::digest(&body)));
        let blob = put_zstd(&*ctx.stager, prefix, &format!("responses/{i:04}"), &body)
            .await
            .map_err(|e| {
                ChunkFailure::new(FailureType::ClientError, format!("staging response failed: {e}"))
            })?;
        evidence_keys.push(blob.key.clone());

        let parsed_body: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
        let mut urls = Vec::new();
        harvest_urls(&parsed_body, &mut urls);
        urls.truncate(spec.max_results as usize);
        source_urls.extend(urls.iter().cloned());
        per_query.push(serde_json::json!({
            "query": spec.query,
            "backend": qp.backend,
            "url": url,
            "result_urls": urls,
            "bytes": body.len(),
            "sha256_raw": hashes.last(),
            "blob_key": blob.key,
        }));
    }

    source_urls.dedup();

    let mut manifest = manifest_scaffold(contract, source_urls, hashes);
    manifest.staged_features_blob = Some(evidence_keys.first().cloned().unwrap_or_default());
    manifest
        .extra
        .insert("blob_encoding".into(), serde_json::json!("zstd"));
    manifest
        .extra
        .insert("source_class_priority".into(), serde_json::json!(payload.source_class_priority));
    manifest
        .extra
        .insert("queries".into(), serde_json::Value::Array(per_query));

    Ok(Outcome {
        manifest,
        evidence_blobs: evidence_keys,
    })
}

fn encode_query(q: &str) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .append_pair("", q)
        .finish()[1..]
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harvests_urls_from_nested_json() {
        let v = serde_json::json!({
            "web": {"results": [
                {"url": "https://a.example.com/x", "title": "a"},
                {"link": "https://b.example.com/y"}
            ]},
            "url": "https://meta.example.com",
            "other": 1
        });
        let mut got = Vec::new();
        harvest_urls(&v, &mut got);
        assert_eq!(
            got,
            vec![
                "https://meta.example.com".to_string(),
                "https://a.example.com/x".to_string(),
                "https://b.example.com/y".to_string(),
            ]
        );
    }

    #[test]
    fn encodes_queries() {
        assert_eq!(encode_query("municode WI § zoning"), "municode+WI+%C2%A7+zoning");
    }
}
