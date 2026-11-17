//! `state_walk` executor: fetch a jurisdiction roster slice, extract titles,
//! validate slugs against the mint-relation, stage a features blob.
//!
//! The walk recipe is declarative:
//! - `mint_relation`: regex a slug must match (mismatch = schema_drift —
//!   the roster has drifted from the mint rules).
//! - `title_line_pattern`: regex the page <title> must match.
//! - `heading_capture`: require a non-empty <h1>.
//!
//! The slice is atomic: any unreachable entry or validation failure fails
//! the whole chunk (verification is mint-relation + citation check
//! server-side).

use super::{fetch_page, manifest_scaffold, parse_payload, ExecContext, Outcome};
use crate::api::{FailureType, StateWalkPayload};
use crate::error::ChunkFailure;
use crate::staging::put_zstd;
use sha2::{Digest, Sha256};

fn extract_tag(html: &str, tag: &str) -> Option<String> {
    let re = regex::Regex::new(&format!(r"(?is)<{tag}[^>]*>(.*?)</{tag}>")).ok()?;
    re.captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) async fn execute(
    ctx: &ExecContext,
    contract: &crate::api::SignedChunkContract,
) -> Result<Outcome, ChunkFailure> {
    let payload: StateWalkPayload = parse_payload(contract)?;
    let recipe = &payload.walk_recipe;

    let mint_re = regex::Regex::new(&recipe.mint_relation).map_err(|e| {
        ChunkFailure::new(
            FailureType::ClientError,
            format!("walk_recipe.mint_relation is not a valid regex: {e}"),
        )
    })?;
    let title_re = if recipe.title_line_pattern.is_empty() {
        None
    } else {
        Some(regex::Regex::new(&recipe.title_line_pattern).map_err(|e| {
            ChunkFailure::new(
                FailureType::ClientError,
                format!("walk_recipe.title_line_pattern is not a valid regex: {e}"),
            )
        })?)
    };

    let prefix = &ctx.lease.staging_upload_prefix;
    let mut entries: Vec<serde_json::Value> = Vec::with_capacity(payload.roster_slice.len());
    let mut source_urls = Vec::new();
    let mut hashes = Vec::new();

    for entry in &payload.roster_slice {
        if !mint_re.is_match(&entry.slug) {
            return Err(ChunkFailure::new(
                FailureType::SchemaDrift,
                format!(
                    "slug {:?} fails mint_relation {:?}",
                    entry.slug, recipe.mint_relation
                ),
            ));
        }

        let page = fetch_page(ctx, reqwest::Method::GET, &entry.url, &[], None, None).await?;
        let html = String::from_utf8_lossy(&page.bytes).to_string();
        let title = extract_tag(&html, "title").map(|t| strip_tags(&t));
        let h1 = extract_tag(&html, "h1").map(|t| strip_tags(&t));

        if let (Some(re), Some(title)) = (&title_re, &title) {
            if !re.is_match(title) {
                return Err(ChunkFailure::new(
                    FailureType::SchemaDrift,
                    format!(
                        "title {:?} does not match title_line_pattern {:?} for slug {:?}",
                        title, recipe.title_line_pattern, entry.slug
                    ),
                ));
            }
        }
        if recipe.heading_capture && h1.as_deref().unwrap_or("").is_empty() {
            return Err(ChunkFailure::new(
                FailureType::SchemaDrift,
                format!(
                    "heading_capture requested but no <h1> found for slug {:?}",
                    entry.slug
                ),
            ));
        }

        hashes.push(hex::encode(Sha256::digest(&page.bytes)));
        source_urls.push(page.final_url.clone());
        entries.push(serde_json::json!({
            "slug": entry.slug,
            "display_name": entry.display_name,
            "url": page.final_url,
            "title": title,
            "h1": h1,
            "sha256_raw": hashes.last(),
            "bytes": page.bytes.len(),
        }));
    }

    // Features blob: the extracted walk results.
    let features = serde_json::json!({
        "target": contract.core.target_key,
        "recipe": {
            "mint_relation": recipe.mint_relation,
            "title_line_pattern": recipe.title_line_pattern,
            "heading_capture": recipe.heading_capture,
        },
        "entries": entries,
    });
    let blob = put_zstd(
        &*ctx.stager,
        prefix,
        "features",
        features.to_string().as_bytes(),
    )
    .await
    .map_err(|e| {
        ChunkFailure::new(
            FailureType::ClientError,
            format!("staging features blob failed: {e}"),
        )
    })?;

    let mut manifest = manifest_scaffold(contract, source_urls, hashes);
    manifest.staged_features_blob = Some(blob.key.clone());
    manifest
        .extra
        .insert("blob_encoding".into(), serde_json::json!("zstd"));
    manifest
        .extra
        .insert("walked".into(), serde_json::json!(entries.len()));

    Ok(Outcome {
        manifest,
        evidence_blobs: vec![blob.key],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_and_cleans_titles() {
        let html = "<html><head><title>  Zoning — City of  Example </title></head><body><h1>Zoning <b>Board</b></h1></body></html>";
        assert_eq!(
            extract_tag(html, "title").map(|t| strip_tags(&t)),
            Some("Zoning — City of Example".to_string())
        );
        assert_eq!(
            extract_tag(html, "h1").map(|t| strip_tags(&t)),
            Some("Zoning Board".to_string())
        );
        assert_eq!(extract_tag("<html><body></body>", "title"), None);
    }
}
