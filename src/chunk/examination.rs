//! `examination` executor: N-of-M agreement over a staged blob's parsed
//! measurements.
//!
//! Examination work needs a BUILT-IN PARSER matching the contract's
//! `parser_id` (no server-sent code, ever). The v0 registry is empty — this
//! executor is the plumbing around it: unknown parsers fail with
//! `contract_unknown_parser` (retryable, visible to the operator) and the
//! chunk is left for a client that implements the parser. Future parsers
//! implement [`BuiltinParser`] and register in [`parser_registry`].

use super::{parse_payload, ExecContext, Outcome};
use crate::api::{ExaminationPayload, FailureType};
use crate::error::ChunkFailure;
use std::collections::HashMap;

/// A built-in parser: consumes the staged blob bytes + the contract's
/// expected measurements, produces the parsed measurement values as JSON.
pub trait BuiltinParser: Send + Sync {
    fn id(&self) -> &'static str;
    /// Parse raw staged-blob bytes into measurement values. Returns
    /// (measurements, evidence-note).
    fn parse(
        &self,
        blob: &[u8],
        expected: &[crate::api::Measurement],
    ) -> Result<serde_json::Value, ChunkFailure>;
}

/// v0 registry: intentionally empty. Operators must issue examination
/// chunks only to clients advertising the matching parser capability
/// (capability advertisement rides the register/lease capabilities list in
/// a future schema_version bump).
pub fn parser_registry() -> HashMap<&'static str, Box<dyn BuiltinParser>> {
    HashMap::new()
}

pub(crate) async fn execute(
    ctx: &ExecContext,
    contract: &crate::api::SignedChunkContract,
) -> Result<Outcome, ChunkFailure> {
    let payload: ExaminationPayload = parse_payload(contract)?;
    let registry = parser_registry();
    let Some(parser) = registry.get(payload.parser_id.as_str()) else {
        return Err(ChunkFailure::new(
            FailureType::ContractUnknownParser,
            format!(
                "parser '{}' is not built into client v{}; built-in parsers: {}",
                payload.parser_id,
                crate::client_version(),
                if registry.is_empty() {
                    "none".to_string()
                } else {
                    registry.keys().map(|s| s.to_string()).collect::<Vec<_>>().join(",")
                }
            ),
        ));
    };

    // Downstream plumbing (kept for the first real parser): fetch the staged
    // blob, run the parser, upload parsed evidence, build the manifest.
    let blob = ctx
        .stager
        .get_object(&payload.staged_blob_key)
        .await
        .map_err(|e| {
            ChunkFailure::new(
                FailureType::ClientError,
                format!("staged blob {} unavailable: {e}", payload.staged_blob_key),
            )
        })?;
    let measurements = parser.parse(&blob, &payload.expected_measurements)?;

    let mut manifest = super::manifest_scaffold(contract, vec![], vec![]);
    manifest
        .extra
        .insert("parser_id".into(), serde_json::json!(payload.parser_id));
    manifest
        .extra
        .insert("measurements".into(), measurements);
    manifest.staged_features_blob = Some(payload.staged_blob_key.clone());

    Ok(Outcome {
        manifest,
        evidence_blobs: vec![payload.staged_blob_key.clone()],
    })
}
