#!/usr/bin/env node
// Cross-language vector: runs the ACTUAL canonicalization + payload-building
// logic copied verbatim from lh-tracker/src/lib/auth.ts (canonicalJson,
// contractCore, buildContractPayload) over the same fixture the Rust test
// `payload_matches_typescript_implementation` uses, and prints the payload
// string. The Rust test asserts equality with this output.
//
// Signature check too: signs with node:crypto ed25519 and prints the
// signature so the Rust side can verify the full chain against WebCrypto's
// ed25519.

import { generateKeyPairSync, sign as nodeSign, createHash } from 'node:crypto';

// ---- verbatim from lh-tracker/src/lib/auth.ts ----
function canonicalJson(value) {
  return JSON.stringify(canon(value)) ?? 'null';
}
function canon(value) {
  if (Array.isArray(value)) {
    return value.map(canon);
  }
  if (value !== null && typeof value === 'object') {
    const source = value;
    const out = {};
    for (const key of Object.keys(source).sort()) {
      const v = source[key];
      if (v !== undefined) out[key] = canon(v);
    }
    return out;
  }
  return value;
}
function buildContractPayload(chunkId, contract) {
  const { expires_at, operator_signature_hex, ...core } = contract;
  return `${chunkId}:${canonicalJson(core)}:${contract.expires_at}`;
}
// ---- end verbatim ----

// Fixture mirroring the Rust test's fixture_contract().
const contract = {
  type: 'examination',
  target_key: 'WI/55011',
  payload: {
    parser_id: 'municode_extract_v1',
    staged_blob_key: 'stage/ops/examination/WI/55011/blob.zst',
    expected_measurements: [
      { name: 'page_count', expected_type: 'number', tolerance: 0.5 },
      { name: 'has_setbacks', expected_type: 'boolean' },
    ],
    unicode: 'Königsberger Straße \u0007',
    ratio: 2.5,
    count: 1200,
    flag: true,
    nothing: null,
  },
  schema_version: 1,
  staging_upload_prefix: 'stage/raw/examination/wi/55011/lease-1/',
  expires_at: 1799999999999,
};

const chunkId = '0f0e6d3a-1111-4222-8333-444455556666';
const payload = buildContractPayload(chunkId, contract);

// Sign with node WebCrypto-independent ed25519.
const { publicKey, privateKey } = generateKeyPairSync('ed25519');
const pubJwk = publicKey.export({ format: 'jwk' }); // x = base64url raw 32 bytes
const sig = nodeSign(null, Buffer.from(payload, 'utf8'), privateKey);

console.log('--- payload ---');
console.log(payload);
console.log('--- payload utf8 sha256 ---');
console.log(createHash('sha256').update(payload, 'utf8').digest('hex'));
console.log('--- pubkey raw hex ---');
console.log(Buffer.from(pubJwk.x, 'base64url').toString('hex'));
console.log('--- signature hex ---');
console.log(sig.toString('hex'));
console.log('--- canonical core json ---');
console.log(canonicalJson({ ...contract, expires_at: undefined, operator_signature_hex: undefined }));
