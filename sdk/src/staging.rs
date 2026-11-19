//! Blob staging: upload result/evidence blobs to the lease's
//! `staging_upload_prefix` in R2.
//!
//! Three transports:
//! - [`R2Stager`]: direct PUT to Cloudflare R2's S3-compatible API,
//!   SigV4-signed with operator-supplied access keys.
//! - [`PresignStager`]: PUT to a presigned URL template (`{key}` placeholder)
//!   — the path a future coordinator-side presign endpoint will feed.
//! - [`LocalStager`]: write under a local root (dry runs, offline tests).
//!
//! Blob convention: pages and evidence are zstd-compressed (`.zst` suffix);
//! manifests record sha256 of the RAW bytes in `page_sha256` (what
//! coordinator-side byte-hash verification compares) plus the stored blob
//! keys, with `blob_encoding: "zstd"` set so verifiers know to decompress.

use crate::error::StageError;
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use url::Url;

#[async_trait]
pub trait BlobStager: Send + Sync {
    /// Upload one blob. `key` is the full staging key, already prefixed with
    /// the lease's `staging_upload_prefix`.
    async fn put_object(
        &self,
        key: &str,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> Result<(), StageError>;

    /// Fetch a blob back (examination chunks read staged blobs).
    async fn get_object(&self, key: &str) -> Result<Vec<u8>, StageError>;
}

// ---------------------------------------------------------------------------
// zstd blob helper
// ---------------------------------------------------------------------------

/// Metadata for one stored blob.
#[derive(Debug, Clone)]
pub struct StoredBlob {
    pub key: String,
    /// sha256 of the raw (pre-compression) bytes, hex.
    pub raw_sha256: String,
    /// sha256 of the stored (compressed) bytes, hex.
    pub stored_sha256: String,
    pub raw_len: u64,
    pub stored_len: u64,
}

/// Compress + upload one blob under `{prefix}{name}.zst`.
///
/// The lease `staging_upload_prefix` is validated against the strict
/// grammar ([`validate_blob_key`]) before it is folded into the blob key,
/// so no stager (R2, presign, or local) ever stages under an invalid
/// prefix.
pub async fn put_zstd(
    stager: &dyn BlobStager,
    prefix: &str,
    name: &str,
    raw: &[u8],
) -> Result<StoredBlob, StageError> {
    validate_blob_key(prefix)?;
    let compressed = zstd::bulk::compress(raw, 3)
        .map_err(|e| StageError::BadKey(format!("zstd compression failed: {e}")))?;
    let key = format!("{prefix}{name}.zst");
    stager
        .put_object(&key, "application/zstd", compressed.clone())
        .await?;
    Ok(StoredBlob {
        key,
        raw_sha256: hex::encode(Sha256::digest(raw)),
        stored_sha256: hex::encode(Sha256::digest(&compressed)),
        raw_len: raw.len() as u64,
        stored_len: compressed.len() as u64,
    })
}

// ---------------------------------------------------------------------------
// S3 / SigV4 helpers (shared by the direct R2 stager and the test vectors)
// ---------------------------------------------------------------------------

/// AWS SigV4 URI encoding: encode everything except `A-Za-z0-9-._~`; within
/// a key path, `/` is kept literal (S3 path-style canonical URI).
pub fn s3_uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~');
        if unreserved || (keep_slash && b == b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Civil date from days-since-epoch (Howard Hinnant's civil_from_days).
/// Returns (year, month, day).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `YYYYMMDD'T'HHMMSS'Z'` (SigV4 amz_date) and `YYYYMMDD` (date_stamp) for a
/// unix-seconds timestamp.
pub fn amz_dates(unix_secs: u64) -> (String, String) {
    let days = (unix_secs / 86_400) as i64;
    let secs_of_day = unix_secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    (
        format!(
            "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
            secs_of_day / 3600,
            (secs_of_day % 3600) / 60,
            secs_of_day % 60
        ),
        format!("{y:04}{m:02}{d:02}"),
    )
}

/// SigV4 signing key derivation (HMAC chain).
pub fn sigv4_signing_key(secret: &str, date_stamp: &str, region: &str, service: &str) -> Vec<u8> {
    type HmacSha256 = Hmac<Sha256>;
    let mut k = HmacSha256::new_from_slice(format!("AWS4{secret}").as_bytes())
        .expect("hmac accepts any key length");
    k.update(date_stamp.as_bytes());
    let k_date = k.finalize().into_bytes();

    let mut k = HmacSha256::new_from_slice(&k_date).expect("hmac");
    k.update(region.as_bytes());
    let k_region = k.finalize().into_bytes();

    let mut k = HmacSha256::new_from_slice(&k_region).expect("hmac");
    k.update(service.as_bytes());
    let k_service = k.finalize().into_bytes();

    let mut k = HmacSha256::new_from_slice(&k_service).expect("hmac");
    k.update(b"aws4_request");
    k.finalize().into_bytes().to_vec()
}

/// Inputs to a SigV4 PUT, exposed for cross-language test vectors.
pub struct SigV4Input<'a> {
    pub access_key_id: &'a str,
    pub secret_access_key: &'a str,
    pub host: &'a str,
    /// Path-style canonical URI, e.g. `/bucket/stage/raw/....zst`.
    pub canonical_uri: String,
    pub payload_sha256_hex: &'a str,
    pub amz_date: &'a str,
    pub date_stamp: &'a str,
    pub region: &'a str,
    pub service: &'a str,
}

/// Full SigV4 signature computation for a PUT (no query string).
pub fn sigv4_signature(input: &SigV4Input) -> String {
    type HmacSha256 = Hmac<Sha256>;
    let canonical_headers = format!(
        "host:{}\nx-amz-content-sha256:{}\nx-amz-date:{}\n",
        input.host, input.payload_sha256_hex, input.amz_date
    );
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical_request = format!(
        "PUT\n{}\n{}\n{}\n{}\n{}",
        input.canonical_uri, "", canonical_headers, signed_headers, input.payload_sha256_hex
    );

    let scope = format!(
        "{}/{}/{}/aws4_request",
        input.date_stamp, input.region, input.service
    );
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        input.amz_date,
        scope,
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    );

    let key = sigv4_signing_key(
        input.secret_access_key,
        input.date_stamp,
        input.region,
        input.service,
    );
    let mut mac = HmacSha256::new_from_slice(&key).expect("hmac");
    mac.update(string_to_sign.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

// ---------------------------------------------------------------------------
// Direct R2 stager
// ---------------------------------------------------------------------------

pub struct R2Stager {
    http: reqwest::Client,
    endpoint: Url,
    bucket: String,
    access_key_id: String,
    secret_access_key: String,
    region: &'static str, // R2 uses "auto"
    service: &'static str,
}

impl R2Stager {
    pub fn new(
        http: reqwest::Client,
        endpoint: &str,
        bucket: &str,
        access_key_id: &str,
        secret_access_key: &str,
    ) -> Result<R2Stager, StageError> {
        let endpoint = Url::parse(endpoint)
            .map_err(|e| StageError::BadKey(format!("invalid R2 endpoint: {e}")))?;
        Ok(R2Stager {
            http,
            endpoint,
            bucket: bucket.to_string(),
            access_key_id: access_key_id.to_string(),
            secret_access_key: secret_access_key.to_string(),
            region: "auto",
            service: "s3",
        })
    }

    fn host(&self) -> String {
        let mut host = self.endpoint.host_str().unwrap_or_default().to_string();
        if let Some(port) = self.endpoint.port() {
            host.push_str(&format!(":{port}"));
        }
        host
    }

    /// Path-style canonical URI for an object key.
    fn canonical_uri(&self, key: &str) -> String {
        format!("/{}/{}", self.bucket, s3_uri_encode(key, true))
    }

    /// Build the signed PUT request (URL + headers).
    fn signed_put(&self, key: &str, payload_sha256_hex: &str) -> (Url, reqwest::header::HeaderMap) {
        use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let (amz_date, date_stamp) = amz_dates(now_secs);
        let input = SigV4Input {
            access_key_id: &self.access_key_id,
            secret_access_key: &self.secret_access_key,
            host: &self.host(),
            canonical_uri: self.canonical_uri(key),
            payload_sha256_hex,
            amz_date: &amz_date,
            date_stamp: &date_stamp,
            region: self.region,
            service: self.service,
        };
        let signature = sigv4_signature(&input);
        let url = format!(
            "{}{}",
            self.endpoint.as_str().trim_end_matches('/'),
            input.canonical_uri
        );

        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-amz-content-sha256"),
            HeaderValue::from_str(payload_sha256_hex).expect("hex is header-safe"),
        );
        headers.insert(
            HeaderName::from_static("x-amz-date"),
            HeaderValue::from_str(&amz_date).expect("amz date is header-safe"),
        );
        let auth = format!(
            "AWS4-HMAC-SHA256 Credential={}/{}/{}/{}/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={signature}",
            input.access_key_id, input.date_stamp, input.region, input.service
        );
        headers.insert(
            reqwest::header::AUTHORIZATION,
            HeaderValue::from_str(&auth).expect("auth header is header-safe"),
        );
        (
            Url::parse(&url).expect("canonical uri is a valid url"),
            headers,
        )
    }
}

#[async_trait]
impl BlobStager for R2Stager {
    async fn put_object(
        &self,
        key: &str,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> Result<(), StageError> {
        let payload_hash = hex::encode(Sha256::digest(&bytes));
        let (url, headers) = self.signed_put(key, &payload_hash);
        let resp = self
            .http
            .put(url)
            .headers(headers)
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .body(bytes)
            .send()
            .await?;
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(StageError::Http {
            status,
            body: body.chars().take(512).collect(),
        })
    }

    async fn get_object(&self, key: &str) -> Result<Vec<u8>, StageError> {
        let empty_hash = hex::encode(Sha256::digest([]));
        let (url, headers) = self.signed_get(key, &empty_hash);
        let resp = self.http.get(url).headers(headers).send().await?;
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(resp.bytes().await?.to_vec());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(StageError::Http {
            status,
            body: body.chars().take(512).collect(),
        })
    }
}

impl R2Stager {
    fn signed_get(&self, key: &str, payload_sha256_hex: &str) -> (Url, reqwest::header::HeaderMap) {
        // GET signs identically except the method line and empty payload.
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let (amz_date, date_stamp) = amz_dates(now_secs);
        let canonical_headers = format!(
            "host:{}\nx-amz-content-sha256:{}\nx-amz-date:{}\n",
            self.host(),
            payload_sha256_hex,
            amz_date
        );
        let signed_headers = "host;x-amz-content-sha256;x-amz-date";
        let canonical_uri = self.canonical_uri(key);
        let canonical_request = format!(
            "GET\n{}\n{}\n{}\n{}\n{}",
            canonical_uri, "", canonical_headers, signed_headers, payload_sha256_hex
        );
        let scope = format!(
            "{}/{}/{}/aws4_request",
            date_stamp, self.region, self.service
        );
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            amz_date,
            scope,
            hex::encode(Sha256::digest(canonical_request.as_bytes()))
        );
        let key = sigv4_signing_key(
            &self.secret_access_key,
            &date_stamp,
            self.region,
            self.service,
        );
        type HmacSha256 = Hmac<Sha256>;
        let mut mac = HmacSha256::new_from_slice(&key).expect("hmac");
        mac.update(string_to_sign.as_bytes());
        let signature = hex::encode(mac.finalize().into_bytes());

        let url = format!(
            "{}{}",
            self.endpoint.as_str().trim_end_matches('/'),
            canonical_uri
        );
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::HeaderName::from_static("x-amz-content-sha256"),
            reqwest::header::HeaderValue::from_str(payload_sha256_hex).expect("hex"),
        );
        headers.insert(
            reqwest::header::HeaderName::from_static("x-amz-date"),
            reqwest::header::HeaderValue::from_str(&amz_date).expect("date"),
        );
        let auth = format!(
            "AWS4-HMAC-SHA256 Credential={}/{}/{}/{}/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={signature}",
            self.access_key_id, date_stamp, self.region, self.service
        );
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&auth).expect("auth"),
        );
        (Url::parse(&url).expect("uri"), headers)
    }
}

// ---------------------------------------------------------------------------
// Presign-template stager
// ---------------------------------------------------------------------------

pub struct PresignStager {
    http: reqwest::Client,
    url_template: String,
}

impl PresignStager {
    pub fn new(http: reqwest::Client, url_template: String) -> PresignStager {
        PresignStager { http, url_template }
    }

    fn url_for(&self, key: &str) -> String {
        self.url_template
            .replace("{key}", &s3_uri_encode(key, true))
    }
}

#[async_trait]
impl BlobStager for PresignStager {
    async fn put_object(
        &self,
        key: &str,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> Result<(), StageError> {
        let resp = self
            .http
            .put(self.url_for(key))
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .body(bytes)
            .send()
            .await?;
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(());
        }
        Err(StageError::Http {
            status,
            body: resp
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(512)
                .collect(),
        })
    }

    async fn get_object(&self, key: &str) -> Result<Vec<u8>, StageError> {
        let resp = self.http.get(self.url_for(key)).send().await?;
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(resp.bytes().await?.to_vec());
        }
        Err(StageError::Http {
            status,
            body: resp
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(512)
                .collect(),
        })
    }
}

// ---------------------------------------------------------------------------
// Local stager (dry runs + offline tests)
// ---------------------------------------------------------------------------

pub struct LocalStager {
    root: PathBuf,
}

impl LocalStager {
    pub fn new(root: impl Into<PathBuf>) -> LocalStager {
        LocalStager { root: root.into() }
    }

    fn path_for(&self, key: &str) -> Result<PathBuf, StageError> {
        if key.split('/').any(|seg| seg == ".." || seg.is_empty()) {
            return Err(StageError::BadKey(format!(
                "key contains empty or '..' segment: {key}"
            )));
        }
        Ok(self.root.join(key))
    }

    /// Absolute path a blob was stored at (used by dry-run reporting).
    pub fn stored_path(&self, key: &str) -> Result<PathBuf, StageError> {
        self.path_for(key)
    }
}

#[async_trait]
impl BlobStager for LocalStager {
    async fn put_object(
        &self,
        key: &str,
        _content_type: &str,
        bytes: Vec<u8>,
    ) -> Result<(), StageError> {
        let path = self.path_for(key)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(path, bytes).await?;
        Ok(())
    }

    async fn get_object(&self, key: &str) -> Result<Vec<u8>, StageError> {
        let path = self.path_for(key)?;
        tokio::fs::read(path).await.map_err(StageError::from)
    }
}

// ---------------------------------------------------------------------------
// Construction from config
// ---------------------------------------------------------------------------

/// Build the stager matching the resolved [`crate::config::StagingConfig`].
pub fn build_stager(
    cfg: &crate::config::StagingConfig,
    http: reqwest::Client,
) -> Result<std::sync::Arc<dyn BlobStager>, StageError> {
    match cfg {
        crate::config::StagingConfig::PresignTemplate { url_template } => Ok(std::sync::Arc::new(
            PresignStager::new(http, url_template.clone()),
        )),
        crate::config::StagingConfig::R2 {
            endpoint,
            bucket,
            access_key_id,
            secret_access_key,
        } => Ok(std::sync::Arc::new(R2Stager::new(
            http,
            endpoint,
            bucket,
            access_key_id,
            secret_access_key,
        )?)),
        crate::config::StagingConfig::Local { root } => {
            Ok(std::sync::Arc::new(LocalStager::new(root.clone())))
        }
    }
}

/// Maximum length of a lease `staging_upload_prefix`.
pub const MAX_PREFIX_LEN: usize = 128;

/// Strict grammar for a lease `staging_upload_prefix`, validated before the
/// prefix is ever used to build a blob key (R2, presign, or local stager).
///
/// Accepted: lowercase alphanumerics, hyphens, and forward slashes only.
/// Rejected: backslash, drive letters/colons (excluded by the charset),
/// empty segments, dot segments (`.` / `..`), and any prefix over
/// [`MAX_PREFIX_LEN`] bytes. A single trailing slash (directory convention)
/// is tolerated.
///
/// Returns [`StageError::BadKey`] on any violation. Callers treat rejection
/// as fail-closed: never stage under an invalid prefix.
pub fn validate_blob_key(prefix: &str) -> Result<(), StageError> {
    if prefix.is_empty() {
        tracing::warn!("staging prefix is empty; rejecting lease");
        return Err(StageError::BadKey("staging prefix is empty".to_string()));
    }
    if prefix.len() > MAX_PREFIX_LEN {
        tracing::warn!(
            "staging prefix too long ({} > {MAX_PREFIX_LEN}): {prefix}",
            prefix.len()
        );
        return Err(StageError::BadKey(format!(
            "staging prefix too long ({} > {MAX_PREFIX_LEN}): {prefix}",
            prefix.len()
        )));
    }
    for b in prefix.bytes() {
        if !(b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'/') {
            tracing::warn!(
                "staging prefix has invalid byte {b:?}: {prefix}; rejecting lease"
            );
            return Err(StageError::BadKey(format!(
                "staging prefix has invalid byte {b:?}: {prefix}"
            )));
        }
    }
    // A trailing slash is the directory convention; strip it so segment
    // checks below don't see a spurious empty trailing segment.
    let body = prefix.strip_suffix('/').unwrap_or(prefix);
    if body.is_empty() {
        tracing::warn!("staging prefix is empty; rejecting lease");
        return Err(StageError::BadKey("staging prefix is empty".to_string()));
    }
    for seg in body.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            tracing::warn!(
                "staging prefix has empty or dot segment: {prefix}; rejecting lease"
            );
            return Err(StageError::BadKey(format!(
                "staging prefix has empty or dot segment: {prefix}"
            )));
        }
    }
    Ok(())
}

/// Unused path helper kept deliberately for callers that probe the local
/// filesystem directly (not part of the prefix-validation path).
#[allow(dead_code)]
fn unused_path_helper(p: &Path) -> bool {
    p.exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_date_known_values() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2026-11-16 = 20673 days after epoch (leap-year-aware check below
        // via amz_dates round trip).
        let (amz, stamp) = amz_dates(1_794_804_000); // 2026-11-16T12:00:00Z-ish
        assert!(stamp.len() == 8 && amz.ends_with('Z'));
        assert_eq!(amz_dates(0).1, "19700101");
        assert_eq!(amz_dates(946_684_800).1, "20000101"); // 2000-01-01
        assert_eq!(amz_dates(951_782_400).1, "20000229"); // 2000-02-29 leap day
        assert_eq!(amz_dates(1_709_164_800).1, "20240229"); // 2024-02-29 leap day
    }

    #[test]
    fn uri_encode_keeps_unreserved_and_slashes() {
        assert_eq!(
            s3_uri_encode("stage/raw/ingest/WI/55011/a b.zst", true),
            "stage/raw/ingest/WI/55011/a%20b.zst"
        );
        // keep_slash=false encodes EVERYTHING outside the unreserved set
        // (used for query-string values, not canonical paths).
        assert_eq!(
            s3_uri_encode("stage/raw/a+b=c", false),
            "stage%2Fraw%2Fa%2Bb%3Dc"
        );
    }

    /// Cross-language vector: canonical request, string-to-sign and final
    /// signature computed independently with Python hashlib/hmac
    /// (see tests/vectors/sigv4_vector.py output, pasted here).
    #[test]
    fn sigv4_signature_matches_independent_python_implementation() {
        let payload = b"landhunter-test-payload";
        let payload_hash = hex::encode(Sha256::digest(payload));
        let input = SigV4Input {
            access_key_id: "AKIDEXAMPLE",
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            host: "abc123.r2.cloudflarestorage.com",
            canonical_uri: "/lh-stage/stage/raw/ingest/WI/55011/l1/pages/0000.zst".to_string(),
            payload_sha256_hex: &payload_hash,
            amz_date: "20261116T120000Z",
            date_stamp: "20261116",
            region: "auto",
            service: "s3",
        };
        // Computed by python3 tests/vectors/sigv4_vector.py (hashlib/hmac,
        // independent implementation of the same algorithm).
        let expected_sig = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/vectors/sigv4_signature.txt"
        ))
        .trim()
        .to_string();
        let got = sigv4_signature(&input);
        assert_eq!(got, expected_sig);
    }

    #[tokio::test]
    async fn local_stager_roundtrips_and_rejects_traversal() {
        let dir = std::env::temp_dir().join(format!("lh-stager-test-{}", std::process::id()));
        let stager = LocalStager::new(&dir);
        stager
            .put_object(
                "stage/raw/a/b.bin",
                "application/octet-stream",
                b"hello".to_vec(),
            )
            .await
            .unwrap();
        let got = stager.get_object("stage/raw/a/b.bin").await.unwrap();
        assert_eq!(got, b"hello");
        assert!(stager.get_object("../escape").await.is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn put_zstd_roundtrip() {
        let dir = std::env::temp_dir().join(format!("lh-zstd-test-{}", std::process::id()));
        let stager = LocalStager::new(&dir);
        let raw = b"setback interior: 25 ft; abutting street: 30 ft".repeat(50);
        let blob = put_zstd(&stager, "stage/raw/ingest/x/1/l1/", "pages/0000", &raw)
            .await
            .unwrap();
        assert_eq!(blob.key, "stage/raw/ingest/x/1/l1/pages/0000.zst");
        assert_eq!(blob.raw_sha256, hex::encode(Sha256::digest(&raw)));
        let stored = stager.get_object(&blob.key).await.unwrap();
        assert_eq!(hex::encode(Sha256::digest(&stored)), blob.stored_sha256);
        let decompressed = zstd::bulk::decompress(&stored, raw.len() * 2).unwrap();
        assert_eq!(decompressed, raw);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn validate_blob_key_accepts_valid_prefixes() {
        for prefix in [
            "stage/raw/ingest/wi/55011/lease-1/",
            "stage/raw/ingest/wi/55011/lease-1",
            "a",
            "a-b",
            "a/b/c",
            "0/9/zz",
        ] {
            assert!(
                validate_blob_key(prefix).is_ok(),
                "prefix {prefix:?} should be valid"
            );
        }
        // Exactly 128 bytes is at the cap and accepted.
        assert!(validate_blob_key(&"a".repeat(128)).is_ok());
    }

    #[test]
    fn validate_blob_key_rejects_invalid_prefixes() {
        let invalid = [
            "stage\\\\raw\\\\ingest",   // backslash (Windows traversal)
            "C:\\\\staging",           // drive-absolute Windows path
            "c:/mix",                  // colon
            "stage/../escape",         // .. segment
            "stage/./raw",             // . segment
            "stage/raw/..",            // trailing ..
            "stage//raw",              // empty segment
            "/stage/raw",              // leading empty segment
            "Stage/raw",               // uppercase
            "stage/RAW",               // uppercase
        ];
        for prefix in invalid {
            assert!(
                validate_blob_key(prefix).is_err(),
                "prefix {prefix:?} should be invalid"
            );
        }
        // Oversized (> 128 bytes).
        assert!(validate_blob_key(&"a".repeat(129)).is_err());
    }

    #[tokio::test]
    async fn put_zstd_rejects_invalid_prefix() {
        let dir = std::env::temp_dir().join(format!(
            "lh-zstd-bad-{}",
            std::process::id()
        ));
        let stager = LocalStager::new(&dir);
        let err = put_zstd(&stager, "stage\\\\bad\\\\prefix", "x.zst", b"hi")
            .await
            .unwrap_err();
        assert!(matches!(err, StageError::BadKey(_)));
        // Nothing was staged under the bad prefix.
        assert!(!dir.exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
