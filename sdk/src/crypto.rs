//! ed25519 chunk-contract verification (client side of the operator trust
//! chain defined in `lh-tracker/src/lib/auth.ts`).
//!
//! Signed payload (must match the TypeScript byte-for-byte):
//!
//! ```text
//! payload = `{chunk_id}:{canonical_core_json}:{expires_at}`
//! ```
//!
//! - `canonical_core_json`: the contract minus `expires_at` and
//!   `operator_signature_hex`, serialized with recursively sorted object keys
//!   and no whitespace. This includes `staging_upload_prefix`, so the
//!   operator signature covers the lease's staging prefix.
//! - `expires_at`: unix **milliseconds**, decimal.
//! - The UTF-8 bytes of that string are what the operator signed; we verify.
//!
//! Canonical-JSON compatibility notes vs the TS `canonicalJson`:
//! - Key sort: TS sorts by UTF-16 code unit, we sort by UTF-8 bytes. These
//!   orders differ only for supplementary-plane characters (U+10000+) versus
//!   characters in U+E000..U+FFFF — contract keys are ASCII field names, so
//!   the orders agree in practice.
//! - Numbers: JS `JSON.stringify` has no integer/double distinction (1.0 and
//!   1 both print `1`). serde_json keeps that distinction, so integer-valued
//!   floats are normalized down to integers here; contracts carry ints and
//!   strings, this only matters for exotic payloads.
//! - Strings: both escape `"` `\` `\b` `\t` `\n` `\f` `\r` and emit other
//!   control characters as `\u00XX`; both leave non-ASCII as raw UTF-8.

use crate::api::{LeaseResponse, SignedChunkContract};
use crate::error::LeaseVerifyError;
use crate::staging::validate_blob_key;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde_json::Value;

/// Parse the operator public key from its 64-char lowercase/uppercase hex
/// form (raw 32-byte ed25519 verifying key).
pub fn parse_public_key_hex(public_key_hex: &str) -> Result<VerifyingKey, LeaseVerifyError> {
    let bytes = hex::decode(public_key_hex.trim())
        .map_err(|_| LeaseVerifyError::BadPublicKey(format!("not hex: {public_key_hex}")))?;
    if bytes.len() != 32 {
        return Err(LeaseVerifyError::BadPublicKey(format!(
            "expected 32 raw bytes, got {}",
            bytes.len()
        )));
    }
    let arr: [u8; 32] = bytes.try_into().expect("length checked above");
    VerifyingKey::from_bytes(&arr)
        .map_err(|_| LeaseVerifyError::BadPublicKey("not a valid ed25519 point".to_string()))
}

/// Deterministic JSON: recursively sorted object keys, no whitespace,
/// integer-valued floats folded to integers. Arrays keep their order.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if let Some(i) = norm_int(n) {
                out.push_str(&i.to_string());
            } else {
                out.push_str(&n.to_string());
            }
        }
        Value::String(s) => write_json_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_string(key, out);
                out.push(':');
                write_canonical(&map[*key], out);
            }
            out.push('}');
        }
    }
}

/// Fold integer-valued f64 numbers to i64 so serialization matches
/// `JSON.stringify` (which prints `1.0` as `1`). Outside safe-integer range
/// the value cannot round-trip through JS exactly anyway.
fn norm_int(n: &serde_json::Number) -> Option<i64> {
    let f = n.as_f64()?;
    if f.fract() != 0.0 || f.abs() > 9.007_199_254_740_992e15 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    Some(f as i64)
}

/// JSON string escaping identical to `JSON.stringify` for well-formed input.
fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{09}' => out.push_str("\\t"),
            '\u{0A}' => out.push_str("\\n"),
            '\u{0C}' => out.push_str("\\f"),
            '\u{0D}' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Core of the contract = everything except the two envelope fields.
/// `staging_upload_prefix` is part of the core and is therefore covered by
/// the operator signature.
fn contract_core(contract: &SignedChunkContract) -> Value {
    let mut value = serde_json::to_value(contract).expect("contract serializes");
    if let Value::Object(map) = &mut value {
        map.remove("expires_at");
        map.remove("operator_signature_hex");
    }
    value
}

/// Exact bytes covered by the operator signature.
pub fn build_contract_payload(chunk_id: &str, contract: &SignedChunkContract) -> Vec<u8> {
    let core = canonical_json(&contract_core(contract));
    format!("{chunk_id}:{core}:{}", contract.expires_at).into_bytes()
}

/// Verify a contract's embedded operator signature. Returns false (never
/// panics) for malformed keys, malformed signatures, unsigned contracts, or
/// any payload mismatch.
pub fn verify_contract(
    public_key_hex: &str,
    chunk_id: &str,
    contract: &SignedChunkContract,
) -> bool {
    let key = match parse_public_key_hex(public_key_hex) {
        Ok(k) => k,
        Err(_) => return false,
    };
    let sig_bytes = match hex::decode(contract.operator_signature_hex.trim()) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let Ok(sig_arr) = <[u8; 64]>::try_from(sig_bytes.as_slice()) else {
        return false;
    };
    let sig = Signature::from_bytes(&sig_arr);
    let payload = build_contract_payload(chunk_id, contract);
    key.verify(&payload, &sig).is_ok()
}

// ---------------------------------------------------------------------------
// Operator-side signing (issuance): the mirror image of the verify path
// above. The private key lives only in the operator's settings as a PATH;
// key bytes are read fresh per operation and never logged or persisted.
// ---------------------------------------------------------------------------

/// Load an openssl-generated ed25519 private key from PKCS#8 PEM text
/// (`-----BEGIN PRIVATE KEY-----`). Accepts surrounding whitespace; rejects
/// anything that is not exactly one parseable ed25519 private key.
pub fn load_signing_key_pem(pem_text: &str) -> Result<SigningKey, String> {
    use ed25519_dalek::pkcs8::DecodePrivateKey;

    let trimmed = pem_text.trim();
    if trimmed.is_empty() {
        return Err("key file is empty".to_string());
    }
    let block = pem::parse(trimmed).map_err(|e| format!("not a PEM file: {e}"))?;
    if block.tag() != "PRIVATE KEY" {
        return Err(format!(
            "not a PKCS#8 private key (found -----BEGIN {}-----; openssl-generated ed25519 \
             keys carry the PRIVATE KEY tag, OpenSSH-format keys do not)",
            block.tag()
        ));
    }
    SigningKey::from_pkcs8_der(block.contents())
        .map_err(|e| format!("unreadable ed25519 private key: {e}"))
}

/// 64-char lowercase hex public half of a signing key — what the operator
/// compares against the Worker's PUBLIC_KEY_HEX secret and the app's pin.
#[must_use]
pub fn signing_key_public_hex(key: &SigningKey) -> String {
    hex::encode(key.verifying_key().to_bytes())
}

/// Sign a chunk contract the way the tracker's auth.ts verifies it: ed25519
/// over `{chunk_id}:{canonical_core_json}:{expires_at}`. Returns a copy of
/// `contract` with `operator_signature_hex` filled in.
#[must_use]
pub fn sign_chunk_contract(
    key: &SigningKey,
    chunk_id: &str,
    contract: &SignedChunkContract,
) -> SignedChunkContract {
    let payload = build_contract_payload(chunk_id, contract);
    let mut signed = contract.clone();
    signed.operator_signature_hex = hex::encode(key.sign(&payload).to_bytes());
    signed
}

/// Full client-side gate before executing anything:
/// signed envelope present, signature verifies, freshness windows sane.
pub fn verify_lease_contract(
    public_key_hex: &str,
    lease: &LeaseResponse,
    now_ms: i64,
) -> Result<(), LeaseVerifyError> {
    let contract = &lease.contract;
    if contract.expires_at <= 0 || contract.operator_signature_hex.is_empty() {
        return Err(LeaseVerifyError::Unsigned);
    }
    // Fail-closed: the lease envelope prefix (used to build blob keys) must
    // pass the strict staging-key grammar before anything else touches it.
    validate_blob_key(&lease.staging_upload_prefix)
        .map_err(|e| LeaseVerifyError::InvalidPrefix(e.to_string()))?;
    if now_ms >= lease.expires_at {
        return Err(LeaseVerifyError::LeaseStale);
    }
    if lease.expires_at > contract.expires_at {
        return Err(LeaseVerifyError::LeaseOutlivesContract);
    }
    if now_ms >= contract.expires_at {
        return Err(LeaseVerifyError::ContractExpired);
    }
    if !verify_contract(public_key_hex, &lease.chunk_id, contract) {
        return Err(LeaseVerifyError::BadSignature);
    }
    // Fail-closed: the prefix the client stages under (lease envelope) must
    // equal the prefix covered by the operator signature (contract). A lease
    // whose signature does not cover the prefix it actually uses is rejected.
    if lease.staging_upload_prefix != contract.staging_upload_prefix {
        tracing::warn!(
            "lease staging_upload_prefix {:?} does not match signed contract prefix {:?}; rejecting lease",
            lease.staging_upload_prefix,
            contract.staging_upload_prefix,
        );
        return Err(LeaseVerifyError::BadSignature);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn signing_key_from_seed(seed: [u8; 32]) -> SigningKey {
        use sha2::{Digest, Sha256};
        let hash = Sha256::digest(seed);
        let key_bytes: [u8; 32] = hash.into();
        SigningKey::from_bytes(&key_bytes)
    }

    fn fixture_contract(expires_at: i64) -> SignedChunkContract {
        let payload = serde_json::json!({
            "parser_id": "municode_extract_v1",
            "staged_blob_key": "stage/ops/examination/WI/55011/blob.zst",
            "expected_measurements": [
                {"name": "page_count", "expected_type": "number", "tolerance": 0.5},
                {"name": "has_setbacks", "expected_type": "boolean"}
            ],
            "unicode": "Königsberger Straße \u{7}",
            "ratio": 2.5,
            "count": 1200,
            "flag": true,
            "nothing": null
        });
        SignedChunkContract {
            core: crate::api::ChunkContract {
                chunk_type: crate::api::ChunkType::Examination,
                target_key: "WI/55011".to_string(),
                payload,
                schema_version: 1,
            },
            expires_at,
            operator_signature_hex: String::new(),
            staging_upload_prefix: "stage/raw/examination/wi/55011/lease-1/".to_string(),
        }
    }

    #[test]
    fn canonical_json_sorts_keys_recursively_and_folds_floats() {
        let v = serde_json::json!({
            "b": 1,
            "a": {"z": [3, 1, {"y": 2.0, "x": "s"}], "a": true},
            "c": [null, "x"],
            "f": 2.5,
            "e": "Königsberger Straße"
        });
        assert_eq!(
            canonical_json(&v),
            r#"{"a":{"a":true,"z":[3,1,{"x":"s","y":2}]},"b":1,"c":[null,"x"],"e":"Königsberger Straße","f":2.5}"#
        );
    }

    #[test]
    fn canonical_json_escapes_control_chars_like_json_stringify() {
        let v = serde_json::json!({"s": "a\u{1}b\"c\\d\ne"});
        // \u0001 must appear as the six-character escape, exactly like
        // JSON.stringify; \n stays the two-character \n escape.
        assert_eq!(canonical_json(&v), r#"{"s":"a\u0001b\"c\\d\ne"}"#);
    }

    #[test]
    fn roundtrip_sign_verify_rust_side() {
        let key = signing_key_from_seed(*b"lh-contributor-client-test-seedx");
        let contract = fixture_contract(1_700_000_000_000);
        let chunk_id = "0f0e6d3a-1111-4222-8333-444455556666";

        // Sign in Rust (operator side happens in TS, but the primitive is the
        // same ed25519 over the same payload bytes).
        let payload = build_contract_payload(chunk_id, &contract);
        let sig = key.sign(&payload);
        let mut signed = contract.clone();
        signed.operator_signature_hex = hex::encode(sig.to_bytes());

        let pub_hex = hex::encode(key.verifying_key().to_bytes());
        assert!(verify_contract(&pub_hex, chunk_id, &signed));

        // Tamper with the payload -> must fail.
        let mut tampered = signed.clone();
        tampered.core.payload["count"] = serde_json::json!(1201);
        assert!(!verify_contract(&pub_hex, chunk_id, &tampered));

        // Tamper with the signed prefix -> must fail.
        let mut bad_prefix = signed.clone();
        bad_prefix.staging_upload_prefix = "stage/tampered/WI/55011/lease-1/".to_string();
        assert!(!verify_contract(&pub_hex, chunk_id, &bad_prefix));

        // Wrong chunk_id -> must fail.
        assert!(!verify_contract(&pub_hex, "other-chunk", &signed));

        // Tampered signature -> must fail.
        let mut bad_sig = signed.clone();
        bad_sig.operator_signature_hex = "00".repeat(64);
        assert!(!verify_contract(&pub_hex, chunk_id, &bad_sig));
    }

    /// Cross-language vector: canonical payload bytes produced by the exact
    /// `canonicalJson`/`buildContractPayload` implementation in
    /// `lh-tracker/src/lib/auth.ts` (tests/vectors/ts_payload_vector.mjs,
    /// verbatim copy of the TS logic — output pinned here).
    #[test]
    fn payload_matches_typescript_implementation() {
        let contract = fixture_contract(1_799_999_999_999);
        let chunk_id = "0f0e6d3a-1111-4222-8333-444455556666";
        let payload = build_contract_payload(chunk_id, &contract);
        let payload_str = String::from_utf8(payload.clone()).unwrap();
        let expected = concat!(
            "0f0e6d3a-1111-4222-8333-444455556666:",
            r#"{"payload":{"count":1200,"expected_measurements":[{"expected_type":"number","name":"page_count","tolerance":0.5},{"expected_type":"boolean","name":"has_setbacks"}],"flag":true,"nothing":null,"parser_id":"municode_extract_v1","ratio":2.5,"staged_blob_key":"stage/ops/examination/WI/55011/blob.zst","unicode":"Königsberger Straße \u0007"},"schema_version":1,"staging_upload_prefix":"stage/raw/examination/wi/55011/lease-1/","target_key":"WI/55011","type":"examination"}:"#,
            "1799999999999",
        );
        assert_eq!(payload_str, expected);
        // Byte-level pin: sha256 of the exact payload bytes as computed by
        // the TS vector script (node:crypto createHash).
        use sha2::{Digest, Sha256};
        assert_eq!(
            hex::encode(Sha256::digest(&payload)),
            "1a89be82759024c001adbbd56d0d74570436c35ef29b6b2c6d78e1134eab252a"
        );
    }

    /// Operator issuance path: a PKCS#8 PEM (openssl-shaped) loads, its
    /// public half matches, and its signature passes verify_contract.
    #[test]
    fn pkcs8_pem_load_and_sign_verify_roundtrip() {
        use ed25519_dalek::pkcs8::EncodePrivateKey;

        let key = signing_key_from_seed(*b"operator-issuance-test-seed-0032");
        let der = key.to_pkcs8_der().expect("encode pkcs8 der");
        let pem_text = pem::Pem::new("PRIVATE KEY", der.as_bytes()).to_string();
        let loaded = load_signing_key_pem(&pem_text).expect("load pem");

        // The loaded key is the SAME key: same public half.
        assert_eq!(
            signing_key_public_hex(&loaded),
            signing_key_public_hex(&key)
        );

        // Its signature over the real payload verifies via the client path.
        let contract = fixture_contract(1_799_999_999_999);
        let chunk_id = "0f0e6d3a-1111-4222-8333-444455556666";
        let signed = sign_chunk_contract(&loaded, chunk_id, &contract);
        assert!(!signed.operator_signature_hex.is_empty());
        let pub_hex = signing_key_public_hex(&loaded);
        assert!(verify_contract(&pub_hex, chunk_id, &signed));
        assert!(!verify_contract(&pub_hex, "other-chunk", &signed));
    }

    /// Garbage and wrong-format keys are rejected with a helpful error,
    /// never a panic.
    #[test]
    fn pem_loader_rejects_garbage_and_wrong_formats() {
        assert!(load_signing_key_pem("").is_err());
        assert!(load_signing_key_pem("   \n  ").is_err());
        assert!(load_signing_key_pem("not a key at all").is_err());
        // Well-formed PEM armoring under the wrong tag names the expectation.
        let openssh = "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\n-----END OPENSSH PRIVATE KEY-----";
        let err = load_signing_key_pem(openssh).unwrap_err();
        assert!(err.contains("PKCS#8"), "error should name the expected format: {err}");
    }

    /// Canonical-JSON byte-match against the operator seeding tool
    /// (`caw-issue-chunks.py --compute-fingerprint`): the consensus manifest
    /// canonicalization must produce the identical sha256, or honest clients
    /// would fail canary verification and get shadowed.
    #[test]
    fn canonical_json_matches_consensus_fingerprint_vector() {
        use sha2::{Digest, Sha256};
        let manifest = serde_json::json!({
            "type": "ingest",
            "target": "WI/55011",
            "source_urls": ["https://example.gov/a", "https://example.gov/b"],
            "page_count": 2,
            "page_sha256": [
                hex::encode(Sha256::digest(b"page-one-bytes")),
                hex::encode(Sha256::digest(b"page-two-bytes")),
            ],
            "reconciliation": null,
        });
        assert_eq!(
            hex::encode(Sha256::digest(canonical_json(&manifest).as_bytes())),
            "9cdc3bc49204f2e1227781dfab32d846be7f85b86df47a33181b5df3e6973d7e",
            "canonicalization drift vs the Python seeding tool — honest clients WILL be shadowed"
        );
    }

    /// Full cross-language chain: verify a signature produced by node's
    /// ed25519 (the operator-signing primitive family) against our dalek
    /// verifier, using the pubkey/signature from ts_payload_vector.mjs.
    #[test]
    fn verifies_node_ed25519_signature() {
        use sha2::{Digest, Sha256};
        // Deterministic key would be nicer, but this pins the real
        // interop: pubkey + signature generated by node:crypto in the
        // vector script run, payload rebuilt on the Rust side.
        let pubkey_hex = "a0b333abf70fc8c0acc7cb5d79a089b6eebe861cc8fca48d1def781f21e707de";
        let sig_hex = "da7ae141ada61dc66a3c7eef894dbe8ac0fd06ce269a013df98c6c626ad80c15360c9866567f00fd58781774941a7ac77bb9c669d9132e29b4c077bfe393b60e";
        let contract = fixture_contract(1_799_999_999_999);
        let chunk_id = "0f0e6d3a-1111-4222-8333-444455556666";
        let payload = build_contract_payload(chunk_id, &contract);
        // Guard: the payload this signature covers must be OUR payload.
        assert_eq!(
            hex::encode(Sha256::digest(&payload)),
            "1a89be82759024c001adbbd56d0d74570436c35ef29b6b2c6d78e1134eab252a"
        );
        let vk = parse_public_key_hex(pubkey_hex).expect("pubkey");
        let sig_bytes = hex::decode(sig_hex).expect("sig hex");
        let sig = Signature::from_bytes(&sig_bytes.try_into().expect("64 bytes"));
        assert!(
            vk.verify(&payload, &sig).is_ok(),
            "node ed25519 signature must verify under dalek"
        );
    }
}
