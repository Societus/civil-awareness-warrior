//! `classify` executor: proposal-grade LLM grading (see lh-tracker
//! docs/DESIGN.md for the trust model).
//!
//! Flow: parse the signed payload → resolve the built-in prompt template →
//! fetch the input (operator-key search proxy, or a staged blob) → render
//! the fixed prompt with the rubric → one local OpenAI-compatible
//! completion → STRICT verdict validation → manifest.
//!
//! FAIL-SAFE SEMANTICS (never guess): a schema-valid verdict is submitted as
//! a verdict; every other model outcome — transport error, timeout,
//! unparseable or schema-invalid output — is submitted as an honest
//! `no_verdict` result with a machine-readable reason. Nothing aborts the
//! lease, nothing poisons the chunk for other graders, and NO blobs are
//! staged: proposals live in the manifest, never in the R2 tree.

use super::{manifest_scaffold, parse_payload, ExecContext, Outcome};
use crate::api::{ClassifyInput, ClassifyPayload, FailureType};
use crate::error::{ChunkFailure, LlmError};
use crate::llm::{parse_verdict, render_search_result_relevance, template_for, LlmLane, Verdict};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// Machine-readable reasons for a no_verdict. Mirrors the tracker's enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoVerdictReason {
    LlmTransport,
    LlmTimeout,
    MalformedVerdict,
    InputUnavailable,
}

impl NoVerdictReason {
    fn as_str(&self) -> &'static str {
        match self {
            NoVerdictReason::LlmTransport => "llm_transport",
            NoVerdictReason::LlmTimeout => "llm_timeout",
            NoVerdictReason::MalformedVerdict => "malformed_verdict",
            NoVerdictReason::InputUnavailable => "input_unavailable",
        }
    }
}

/// What the grader saw and what the model concluded — everything the
/// manifest reports for one classify chunk.
struct Grading {
    input_sha256: String,
    input_truncated: bool,
    source_urls: Vec<String>,
    outcome: Result<Verdict, NoVerdictReason>,
}

pub(crate) async fn execute(
    ctx: &ExecContext,
    contract: &crate::api::SignedChunkContract,
) -> Result<Outcome, ChunkFailure> {
    let payload: ClassifyPayload = parse_payload(contract)?;

    // --- Template + verdict-schema resolution (typed failures on skew) ---
    let template = template_for(&payload.prompt_template_id).ok_or_else(|| {
        ChunkFailure::new(
            FailureType::ContractUnknownParser,
            format!(
                "unknown prompt_template_id {:?} in client v{} (built-in: search_result_relevance_v1)",
                payload.prompt_template_id,
                crate::client_version()
            ),
        )
    })?;
    if let Some(named) = &payload.verdict_schema {
        if named != template.verdict_schema {
            return Err(ChunkFailure::new(
                FailureType::ContractUnknownParser,
                format!(
                    "contract verdict_schema {named:?} is not implemented in client v{} (supports {:?})",
                    crate::client_version(),
                    template.verdict_schema
                ),
            ));
        }
    }

    // --- Input fetch ---
    let (input_json, source_urls, input_sha256, input_truncated) = match &payload.input {
        ClassifyInput::SearchResultSet { .. } => {
            let proxy = ctx.search_proxy.as_ref().ok_or_else(|| {
                ChunkFailure::new(
                    FailureType::ClientError,
                    "classify chunk leased but no search proxy configured on this client",
                )
            })?;
            if ctx.lease_is_lost() {
                return Err(ChunkFailure::new(
                    FailureType::ClientError,
                    "lease lost before input fetch",
                ));
            }
            let resp = proxy
                .fetch_classify_input(&ctx.lease.chunk_id)
                .await
                .map_err(|e| {
                    ChunkFailure::new(
                        FailureType::ClientError,
                        format!(
                            "search proxy fetch failed for chunk {}: {e}",
                            ctx.lease.chunk_id
                        ),
                    )
                })?;
            let bytes = serde_json::to_vec(&resp).unwrap_or_default();
            let urls = resp
                .queries
                .iter()
                .map(|q| q.url.clone())
                .collect::<Vec<_>>();
            (
                serde_json::to_value(&resp).unwrap_or(Value::Null),
                urls,
                hex::encode(Sha256::digest(&bytes)),
                false,
            )
        }
        ClassifyInput::StagedBlob { staged_blob_key } => {
            if ctx.lease_is_lost() {
                return Err(ChunkFailure::new(
                    FailureType::ClientError,
                    "lease lost before input fetch",
                ));
            }
            let bytes = ctx.stager.get_object(staged_blob_key).await.map_err(|e| {
                ChunkFailure::new(
                    FailureType::ClientError,
                    format!("staged input {staged_blob_key} unavailable: {e}"),
                )
            })?;
            // Staging writes zstd blobs (.zst keys, put_zstd); decompress
            // before parsing so graders see the same bytes the operator
            // staged. Generous capacity: inputs are bounded by the prompt
            // budget anyway.
            let json_bytes = if staged_blob_key.ends_with(".zst") {
                zstd::bulk::decompress(&bytes, 16 * 1024 * 1024).map_err(|e| {
                    ChunkFailure::new(
                        FailureType::SchemaDrift,
                        format!("staged input {staged_blob_key} is not valid zstd: {e}"),
                    )
                })?
            } else {
                bytes
            };
            let parsed: Value = serde_json::from_slice(&json_bytes).map_err(|e| {
                ChunkFailure::new(
                    FailureType::SchemaDrift,
                    format!("staged input {staged_blob_key} is not JSON: {e}"),
                )
            })?;
            let sha = hex::encode(Sha256::digest(&json_bytes));
            (parsed, Vec::new(), sha, false)
        }
    };

    // Serialize + truncate the input to the template budget (recorded).
    let mut input_str = serde_json::to_string(&input_json).unwrap_or_else(|_| "{}".to_string());
    let mut truncated = input_truncated;
    if input_str.len() > template.max_input_chars {
        // Cut on a char boundary; the model sees a truncated input and the
        // manifest says so. Never silently.
        let mut cut = template.max_input_chars;
        while cut > 0 && !input_str.is_char_boundary(cut) {
            cut -= 1;
        }
        input_str.truncate(cut);
        truncated = true;
    }

    // --- Local grading ---
    let lane: &LlmLane = ctx.llm.as_ref().ok_or_else(|| {
        ChunkFailure::new(
            FailureType::ClientError,
            "classify chunk leased but the LLM lane is not configured",
        )
    })?;
    let (system, user) = render_search_result_relevance(&payload.rubric, &input_str);

    let outcome = match lane.endpoint.complete(&system, &user).await {
        Ok(content) => match parse_verdict(&content) {
            Ok(v) => Ok(v),
            Err(_) => Err(NoVerdictReason::MalformedVerdict),
        },
        Err(LlmError::Timeout) => Err(NoVerdictReason::LlmTimeout),
        Err(LlmError::Transport(_) | LlmError::Http { .. } | LlmError::BadEndpoint(_)) => {
            Err(NoVerdictReason::LlmTransport)
        }
        Err(LlmError::MalformedVerdict(_)) => Err(NoVerdictReason::MalformedVerdict),
    };

    let grading = Grading {
        input_sha256,
        input_truncated: truncated,
        source_urls,
        outcome,
    };
    build_outcome(contract, &payload, lane, grading)
}

/// Assemble the manifest. `page_sha256[0]` is the sha256 of the canonical
/// classification JSON actually reported — the hash is over OUR submission,
/// not the model's raw text (which never leaves the machine).
fn build_outcome(
    contract: &crate::api::SignedChunkContract,
    payload: &ClassifyPayload,
    lane: &LlmLane,
    grading: Grading,
) -> Result<Outcome, ChunkFailure> {
    let mut manifest = manifest_scaffold(contract, grading.source_urls, Vec::new());

    match &grading.outcome {
        Ok(verdict) => {
            let canonical = serde_json::to_string(verdict).unwrap_or_default();
            manifest.page_sha256 = vec![hex::encode(Sha256::digest(canonical.as_bytes()))];
            manifest.page_count = 1;
            manifest
                .extra
                .insert("classification".into(), json!("verdict"));
            manifest.extra.insert(
                "verdict".into(),
                serde_json::to_value(verdict).unwrap_or(Value::Null),
            );
        }
        Err(reason) => {
            let marker = json!({ "classification": "no_verdict", "reason": reason.as_str() });
            let canonical = serde_json::to_string(&marker).unwrap_or_default();
            manifest.page_sha256 = vec![hex::encode(Sha256::digest(canonical.as_bytes()))];
            manifest.page_count = 1;
            manifest
                .extra
                .insert("classification".into(), json!("no_verdict"));
            manifest
                .extra
                .insert("no_verdict_reason".into(), json!(reason.as_str()));
        }
    }

    manifest.extra.insert(
        "prompt_template_id".into(),
        json!(payload.prompt_template_id),
    );
    manifest
        .extra
        .insert("input_sha256".into(), json!(grading.input_sha256));
    if grading.input_truncated {
        manifest.extra.insert("input_truncated".into(), json!(true));
    }
    // Capability echo: lets the operator weight proposals by model class
    // without any additional client fingerprinting.
    manifest.extra.insert(
        "llm".into(),
        serde_json::to_value(&lane.capability).unwrap_or(Value::Null),
    );

    // Deliberately NO evidence blobs: proposals never enter (even raw)
    // staging. The tracker rejects classify results that claim blobs.
    Ok(Outcome {
        manifest,
        evidence_blobs: Vec::new(),
    })
}
