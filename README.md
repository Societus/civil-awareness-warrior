# lh-contributor-client

Rust client SDK + CLI for LandHunter **warriors** — volunteer nodes that lease
chunks from the [`lh-tracker`](https://github.com/Societus/lh-tracker)
coordinator (Cloudflare Worker + D1 + R2 + Durable Objects), execute them
locally, stage evidence blobs to R2, and submit results for verification.

```
register → poll /v1/lease → verify ed25519 contract → execute chunk
   → heartbeat every interval → stage blobs → POST /v1/result (or /v1/fail)
```

## Deliverables

- **Single static binary** (`x86_64-unknown-linux-musl`, static-pie, ~8 MB,
  no runtime deps — runs on any glibc/musl distro):

  ```sh
  cargo build --release --target x86_64-unknown-linux-musl
  # → target/x86_64-unknown-linux-musl/release/lh-contributor
  ```

- **Docker image** (FROM scratch, ~8 MB):

  ```sh
  podman build -t lh-contributor:latest .
  podman run --rm -e LH_COORDINATOR_URL=https://lh-tracker.example.workers.dev \
      -e LH_PUBLIC_KEY_HEX=<64 hex chars> \
      -v "$PWD/lh-work:/work" \
      lh-contributor:latest run
  ```

## Configuration (env)

| Variable | Meaning | Default |
|---|---|---|
| `LH_COORDINATOR_URL` | Coordinator base URL (**required**) | — |
| `LH_PUBLIC_KEY_HEX` | Operator ed25519 public key, 64 hex chars (**required**) | — |
| `LH_CLIENT_ID` / `LH_AUTH_TOKEN` | Existing identity (else loaded from work dir, else auto-registered) | — |
| `LH_CONCURRENCY` | Parallel slots, 1..=8 (server caps `slot_id` at 7) | 2 |
| `LH_WORK_DIR` | Credentials / manifests / dry-run staging | `./lh-work` |
| `LH_RATE_LIMIT` | Client-side politeness cap `requests/min,bytes/min` (min-ed with the lease budget) | — |
| `LH_UPLOAD_PRESIGN_TEMPLATE` | Presigned-URL template containing `{key}` | — |
| `LH_R2_BUCKET` + `LH_R2_ACCESS_KEY_ID` + `LH_R2_SECRET_ACCESS_KEY` + (`LH_R2_ENDPOINT` or `LH_R2_ACCOUNT_ID`) | Direct R2 upload via the S3 API (SigV4, region `auto`) | — |
| `LH_BRAVE_API_KEY` | Brave Search API key for `search` chunks (backend=brave) | — |
| `LH_HTTP_TIMEOUT_SEC` | Per-request timeout for coordinator + sources | 60 |

Staging precedence: presign template → direct R2 → local (dry-run only).
Real (non-dry) runs refuse local staging: submitted blob keys must exist in
R2 for server-side verification to fetch them.

## Usage

```sh
# 1. Register (mints client_id + auth_token, saves to $LH_WORK_DIR/credentials.json, 0600)
lh-contributor register

# 2. Run the loop
lh-contributor run                       # all chunk types, 2 slots
lh-contributor run --once                # exit after the first chunk settles
lh-contributor run --capabilities ingest,alt_source
lh-contributor run --concurrency 4
lh-contributor run --dry-run             # stage locally + write manifest, never submit
```

Backoff sleeps honor the server's `backoff_sec` with ±20% jitter (capped at
5 min) so a fleet doesn't thundering-herd. Ctrl-C finishes the current lease
and reports `aborted_by_user` so chunks requeue.

## Security model

- **No server-sent code.** Chunks are declarative contracts (fetch plans,
  probe plans, rosters, query plans); executors are built in.
- **Operator-signed contracts.** Every lease carries an ed25519 signature
  over `{chunk_id}:{canonical_core_json}:{expires_at}` (recursively
  key-sorted, no whitespace — byte-identical to lh-tracker's `canonicalJson`,
  pinned by cross-language test vectors). Clients verify **before executing
  anything**, plus freshness (lease must not outlive the contract). Three
  consecutive verification failures abort the run: a bad signature is
  tampering or a misconfigured `LH_PUBLIC_KEY_HEX`, and neither heals by
  hammering the coordinator.
- **Politeness.** Source fetches run through a rolling-window limiter using
  min(contract budget, `LH_RATE_LIMIT`) — 1000 volunteers must look like one
  polite client to a county GIS server. Bot-walled/js-only surfaces are
  *reported* (`bot_walled`, `js_only`), never circumvented.
- **Identity** lives in `credentials.json` (0600). A 401 on lease triggers
  exactly one emergency re-register (dev D1 wipe recovery); a second 401 is
  fatal.

## Chunk-type support matrix

| Type | Status | Behavior |
|---|---|---|
| `ingest` | **implemented** | Runs the declarative fetch plan page-by-page (GET/POST, params, expected content-type), hashes raw bytes, zstd-stages each page, evaluates `non_empty`/`exact_count` reconciliation rules, records the rest operator-side. Atomic: any page failure fails the chunk. |
| `alt_source` | **implemented (base probe)** | Availability-probes every candidate URL; a candidate that answers (even 404) is a finding, only total transport failure fails the chunk. Extended probe flags (enum_services, count_groupby, …) fail fast as `contract_unknown_parser` before spending politeness budget. |
| `state_walk` | **implemented (basic)** | Fetches the roster slice, validates slugs against `mint_relation`, extracts `<title>`/`<h1>`, checks `title_line_pattern`, stages a features blob. Slice is atomic. |
| `search` | **implemented (custom + brave)** | `custom` backend: GETs a contract endpoint per query (`{query}` placeholder or `q=` param), harvests `url`/`link` fields, stages raw responses. `brave`: Brave Search API with `LH_BRAVE_API_KEY`. `bing`/`duckduckgo`: typed failure. |
| `examination` | **stub** | Requires a built-in parser matching `parser_id`; the v0 registry is empty, so unknown parsers fail as `contract_unknown_parser` (retryable — the chunk waits for a capable client). Parsers plug in via the `BuiltinParser` trait. |

Blob convention: blobs are zstd-compressed (`.zst`), manifests record
**raw** sha256 in `page_sha256` (what byte-hash verification compares) plus
`blob_encoding: "zstd"` so verifiers know to decompress.

## Failure reporting

Every execution failure maps to the coordinator's `failure_type` taxonomy
(`source_unreachable`, `http_4xx` [terminal], `http_5xx`, `bot_walled`,
`schema_drift`, `contract_unknown_parser`, `client_error`, `client_timeout`,
`aborted_by_user`) and is POSTed to `/v1/fail` so the reaper can requeue or
quarantine appropriately.

## Development

```sh
cargo test            # 39 tests: unit + protocol (wiremock) + executors
cargo clippy --all-targets
```

Cross-language vectors (regenerate with the scripts in `tests/vectors/`):

- `ts_payload_vector.mjs` — canonical payload + ed25519 signature produced
  by the verbatim `canonicalJson`/`buildContractPayload` logic from
  lh-tracker's `src/lib/auth.ts` via node:crypto; the Rust suite asserts
  payload bytes AND verifies the node signature under `ed25519-dalek`.
- `sigv4_vector.py` — SigV4 PUT signature computed independently with
  hashlib/hmac; pins the Rust SigV4 implementation.

Layout:

```
src/api.rs       wire types (mirrors lh-tracker types.ts/zod schemas) + ApiClient
src/crypto.rs    canonical JSON + ed25519 verification + freshness gates
src/config.rs    LH_* env + credentials persistence
src/rate.rs      rolling-window politeness limiter
src/staging.rs   BlobStager: R2 direct (SigV4) / presign template / local
src/chunk/       per-type executors + dispatch
src/main.rs      CLI: register / run (slot loop, heartbeat, submit/fail)
Dockerfile       rust:alpine → scratch, single static binary
```

## Status

- Coordinator compatibility: lh-tracker `src/routes/{register,lease,heartbeat,result,fail,contract}.ts`
  as of 2026-11-16.
- Worker-relay blob upload ("direct via Worker") is not implemented — the
  coordinator exposes no upload endpoint yet; use direct R2 or presign
  templates. (Task spec: "presigned URL or direct via Worker".)
- Repository is local-only; GitHub publication (`Societus/lh-contributor-client`)
  awaits operator review per the public-content review convention.
