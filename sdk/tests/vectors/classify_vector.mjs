#!/usr/bin/env node
// Cross-language vector for the CLASSIFY chunk contract: runs the ACTUAL
// canonicalization + payload-building logic copied verbatim from
// lh-tracker/src/lib/auth.ts (canonicalJson, contractCore,
// buildContractPayload) over a classify fixture, prints the payload string,
// signs it with node:crypto ed25519, and emits the values the Rust test
// `classify_contract_payload_matches_typescript` pins (payload sha256,
// pubkey, signature). Proves the ed25519 envelope covers the new classify
// payload fields byte-for-byte on both sides without any envelope change.

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

// Fixture: a classify contract exactly as operator issuance would store it
// (minus the signature envelope, which the payload strips anyway).
const contract = {
  type: 'classify',
  target_key: 'WI/55011',
  schema_version: 1,
  staging_upload_prefix: 'stage/raw/classify/wi/55011/lease-1/',
  payload: {
    prompt_template_id: 'search_result_relevance_v1',
    rubric: {
      objective: 'Find official county GIS or election-data portals',
      relevant_criteria: ['official .gov or .us domain', 'serves parcel/zoning/election data'],
      irrelevant_criteria: ['third-party aggregators', 'paywalled news articles'],
    },
    input: {
      kind: 'search_result_set',
      query_plan: {
        queries: [{ query: 'wi 55011 gis parcels', max_results: 5 }],
        backend: 'brave',
      },
    },
    verdict_schema: 'classify_verdict_v1',
    expected_verdicts: 3,
  },
  expires_at: 1799999999999,
};
const chunk_id = 'c1a551f7-0000-4000-8000-00000000000f';

const payload = buildContractPayload(chunk_id, contract);
const payload_bytes = Buffer.from(payload, 'utf8');
const sha256 = createHash('sha256').update(payload_bytes).digest('hex');

// Deterministic ed25519 key from a seed (reproducible across runs).
import { createPrivateKey, createPublicKey } from 'node:crypto';
const seed = createHash('sha256').update('lh-classify-vector-key').digest();
const pkcs8 = Buffer.concat([
  Buffer.from('302e020100300506032b657004220420', 'hex'), // RFC 8410 prefix
  seed,
]);
const privateKey = createPrivateKey({ key: pkcs8, format: 'der', type: 'pkcs8' });
const signature = nodeSign(null, payload_bytes, privateKey);
const publicKeyHex = Buffer.from(
  createPublicKey(privateKey).export({ format: 'jwk' }).x,
  'base64url'
).toString('hex');

console.log('payload:');
console.log(payload);
console.log('sha256:', sha256);
console.log('pubkey:', publicKeyHex);
console.log('signature:', signature.toString('hex'));
