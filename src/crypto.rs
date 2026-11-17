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
//!   and no whitespace.
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
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
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
            r#"{"payload":{"count":1200,"expected_measurements":[{"expected_type":"number","name":"page_count","tolerance":0.5},{"expected_type":"boolean","name":"has_setbacks"}],"flag":true,"nothing":null,"parser_id":"municode_extract_v1","ratio":2.5,"staged_blob_key":"stage/ops/examination/WI/55011/blob.zst","unicode":"Königsberger Straße \u0007"},"schema_version":1,"target_key":"WI/55011","type":"examination"}:"#,
            "1799999999999",
        );
        assert_eq!(payload_str, expected);
        // Byte-level pin: sha256 of the exact payload bytes as computed by
        // the TS vector script (node:crypto createHash).
        use sha2::{Digest, Sha256};
        assert_eq!(
            hex::encode(Sha256::digest(&payload)),
            "93db718df48ec3ee59df87a3b2e9278c33f4b3635c181eab450d9f071df0abfd"
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
        let pubkey_hex = "8976efbb9c452a4b160d0a318dd2250219bd970b688c8e579154b3e0fb5f0b36";
        let sig_hex = "11028a08b552c0f4d092ff08ea3e08e154f9bbaea1bb23c27ca66f551d30256812d6c5d65fbff2f5fdd87babb1f11d1c7ec41933a2c69aeccb8b446311463906";
        let contract = fixture_contract(1_799_999_999_999);
        let chunk_id = "0f0e6d3a-1111-4222-8333-444455556666";
        let payload = build_contract_payload(chunk_id, &contract);
        // Guard: the payload this signature covers must be OUR payload.
        assert_eq!(
            hex::encode(Sha256::digest(&payload)),
            "93db718df48ec3ee59df87a3b2e9278c33f4b3635c181eab450d9f071df0abfd"
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
