//! Local-LLM grading lane for `classify` chunks: prompt templates, the
//! OpenAI-compatible endpoint client, and STRICT verdict validation.
//!
//! Trust model (mirrors lh-tracker docs/DESIGN.md): model output is
//! PROPOSAL-GRADE. The executor never guesses — only an explicit,
//! schema-valid verdict is submitted as a verdict; every other outcome
//! (transport error, timeout, unparseable or invalid output) becomes an
//! honest `no_verdict` result. This is the NOTIFY/IGNORE lesson: a small
//! model's silence or garbage must fail toward "no answer", never toward a
//! default classification.

use crate::api::{ClassifyRubric, LlmCapability};
use crate::error::LlmError;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use url::Url;

/// The only verdict schema classify defines today. Named (not inline) in
/// contracts; unknown names are a typed client failure.
pub const CLASSIFY_VERDICT_SCHEMA_V1: &str = "classify_verdict_v1";

// ---------------------------------------------------------------------------
// Prompt template registry
// ---------------------------------------------------------------------------

/// A built-in prompt template. Templates version by ID: changing the text is
/// a NEW id (`..._v2`), so a chunk's meaning can never drift under a lease.
/// Contracts carry only the ID; the text lives here in the SDK.
pub struct PromptTemplate {
    pub id: &'static str,
    pub verdict_schema: &'static str,
    /// Serialized-input budget (chars) before rendering; truncation is
    /// recorded in the result manifest so the operator can tell.
    pub max_input_chars: usize,
}

/// Prompt template for grading a search-result set against a rubric.
/// Rubric slots are fixed; instructions are fixed; only the rubric content
/// and the input vary.
pub const SEARCH_RESULT_RELEVANCE_V1: PromptTemplate = PromptTemplate {
    id: "search_result_relevance_v1",
    verdict_schema: CLASSIFY_VERDICT_SCHEMA_V1,
    max_input_chars: 60_000,
};

pub fn template_for(id: &str) -> Option<&'static PromptTemplate> {
    match id {
        "search_result_relevance_v1" => Some(&SEARCH_RESULT_RELEVANCE_V1),
        _ => None,
    }
}

/// Render the system + user messages for a template. Slot filling is plain
/// replacement — no format machinery, no injection surface beyond the rubric
/// text the operator signed.
pub fn render_search_result_relevance(
    rubric: &ClassifyRubric,
    input_json: &str,
) -> (String, String) {
    let system = "You are a precise classification assistant for a civil-data research pipeline. \
You grade search result sets strictly against a rubric. \
You output ONLY a single JSON object — no prose, no markdown fences, no explanations.";
    let user = format!(
        "Objective: {objective}\n\
A result set is relevant only if it satisfies the relevant criteria; it is irrelevant if it matches any irrelevant criterion or fails the relevant ones.\n\
\nRelevant criteria:\n{relevant}\n\
\nIrrelevant criteria:\n{irrelevant}\n\
\nResult set (JSON):\n{input}\n\
\nGrade the result set. Respond with EXACTLY one JSON object of this shape and nothing else:\n\
{{\"verdict\":\"relevant\"|\"irrelevant\",\"confidence\":<number 0.0-1.0>,\"candidate_urls\":[<subset of input URLs you judge usable, max 20>]}}\n\
Omit candidate_urls when the verdict is irrelevant. No other fields are allowed.",
        objective = rubric.objective,
        relevant = bullet_list(&rubric.relevant_criteria),
        irrelevant = bullet_list(&rubric.irrelevant_criteria),
        input = input_json,
    );
    (system.to_string(), user)
}

fn bullet_list(items: &[String]) -> String {
    if items.is_empty() {
        return "- (none specified)".to_string();
    }
    items
        .iter()
        .map(|i| format!("- {i}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Verdict (classify_verdict_v1, strict)
// ---------------------------------------------------------------------------

/// Schema-valid verdict. Deserialization is STRICT (`deny_unknown_fields`):
/// hallucinated fields make the whole output a no_verdict, mirroring the
/// tracker-side zod `.strict()`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verdict {
    pub verdict: VerdictLabel,
    pub confidence: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidate_urls: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictLabel {
    Relevant,
    Irrelevant,
}

/// Parse and semantically validate the model's output against
/// classify_verdict_v1. Tolerates the two shapes small models actually emit
/// (bare JSON, or JSON inside a ```json fence); anything else — prose,
/// truncated JSON, wrong enums, confidence out of range, non-URL candidates,
/// unknown fields — is an error, and the executor turns errors into
/// no_verdict rather than guessing.
pub fn parse_verdict(content: &str) -> Result<Verdict, LlmError> {
    let json_str = extract_json_object(content)?;
    let mut verdict: Verdict = serde_json::from_str(json_str).map_err(|e| {
        LlmError::MalformedVerdict(format!(
            "verdict does not match classify_verdict_v1 (strict): {e}"
        ))
    })?;
    if !verdict.confidence.is_finite() || !(0.0..=1.0).contains(&verdict.confidence) {
        return Err(LlmError::MalformedVerdict(format!(
            "confidence {} outside 0.0..=1.0",
            verdict.confidence
        )));
    }
    // Candidate URLs must be real http(s) URLs; drop nothing silently —
    // an invalid entry invalidates the verdict (never guess which ones count).
    for u in &verdict.candidate_urls {
        let parsed = Url::parse(u).map_err(|e| {
            LlmError::MalformedVerdict(format!("candidate_url {u:?} is not a URL: {e}"))
        })?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(LlmError::MalformedVerdict(format!(
                "candidate_url {u:?} is not http(s)"
            )));
        }
    }
    verdict.candidate_urls.truncate(20);
    Ok(verdict)
}

/// Extract the outermost JSON object from model output: accepts a bare
/// object and a fenced ```json block; finds the first balanced `{...}`
/// otherwise. Returns the sliced substring (serde does the real validation).
fn extract_json_object(content: &str) -> Result<&str, LlmError> {
    let trimmed = content.trim();
    // Strip a markdown fence if present.
    let unfenced = if trimmed.starts_with("```") {
        let inner = trimmed
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```");
        inner.trim()
    } else {
        trimmed
    };
    let start = unfenced
        .find('{')
        .ok_or_else(|| LlmError::MalformedVerdict("no JSON object in model output".to_string()))?;
    let bytes = unfenced.as_bytes();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(&unfenced[start..=i]);
                }
            }
            _ => {}
        }
    }
    Err(LlmError::MalformedVerdict(
        "model output contains an unbalanced JSON object".to_string(),
    ))
}

// ---------------------------------------------------------------------------
// OpenAI-compatible endpoint
// ---------------------------------------------------------------------------

/// A local llama-server-style OpenAI-compatible chat endpoint. The default
/// base URL targets a local sidecar (llama-server on 127.0.0.1:8081); a
/// custom base URL + optional key turns the same client into a
/// custom-local or cloud lane. The endpoint sees only the rendered prompt —
/// it never sees credentials, contracts, or the coordinator.
#[derive(Debug, Clone)]
pub struct LlmEndpoint {
    http: reqwest::Client,
    base: Url,
    api_key: Option<String>,
    model: String,
    timeout: Duration,
}

/// Everything the executor needs: endpoint plus the capability echo that
/// rides into result manifests so the operator can weight by model class.
#[derive(Debug, Clone)]
pub struct LlmLane {
    pub endpoint: LlmEndpoint,
    pub capability: LlmCapability,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    #[serde(default)]
    content: Option<String>,
}

impl LlmEndpoint {
    pub fn new(
        base_url: &str,
        api_key: Option<String>,
        model: &str,
        timeout: Duration,
    ) -> Result<LlmEndpoint, LlmError> {
        let base = Url::parse(base_url).map_err(|e| {
            LlmError::BadEndpoint(format!("invalid LLM base URL {base_url:?}: {e}"))
        })?;
        Ok(LlmEndpoint {
            http: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .map_err(|e| LlmError::BadEndpoint(format!("http client build failed: {e}")))?,
            base,
            api_key,
            model: model.to_string(),
            timeout,
        })
    }

    /// One non-streaming chat completion. Returns the message content; the
    /// caller (classify executor) validates it. Errors carry no retry
    /// semantics — the executor converts them to no_verdict reasons.
    pub async fn complete(&self, system: &str, user: &str) -> Result<String, LlmError> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|_| LlmError::BadEndpoint("base URL is not extendable".to_string()))?
            .extend(["chat", "completions"]);
        let body = serde_json::json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
            "temperature": 0.0,
            "max_tokens": 512,
            "stream": false,
        });
        let mut req = self.http.post(url).json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(LlmError::Http {
                status,
                body: text.chars().take(512).collect(),
            });
        }
        let parsed: ChatCompletionResponse = serde_json::from_str(&text).map_err(|e| {
            LlmError::BadEndpoint(format!(
                "endpoint response is not chat.completion JSON: {e}"
            ))
        })?;
        let content = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .ok_or_else(|| {
                LlmError::BadEndpoint("empty choices/content in response".to_string())
            })?;
        Ok(content)
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rubric() -> ClassifyRubric {
        ClassifyRubric {
            objective: "Find official county GIS portals".to_string(),
            relevant_criteria: vec!["official .gov domain".to_string()],
            irrelevant_criteria: vec!["third-party aggregators".to_string()],
        }
    }

    #[test]
    fn parses_bare_json_verdict() {
        let v = parse_verdict(r#"{"verdict":"relevant","confidence":0.8,"candidate_urls":["https://gis.example.gov/"]}"#)
            .expect("valid");
        assert_eq!(v.verdict, VerdictLabel::Relevant);
        assert_eq!(v.confidence, 0.8);
        assert_eq!(
            v.candidate_urls,
            vec!["https://gis.example.gov/".to_string()]
        );
    }

    #[test]
    fn parses_fenced_json_verdict() {
        let v = parse_verdict("```json\n{\"verdict\":\"irrelevant\",\"confidence\":0.55}\n```")
            .expect("valid");
        assert_eq!(v.verdict, VerdictLabel::Irrelevant);
        assert!(v.candidate_urls.is_empty());
    }

    #[test]
    fn parses_json_embedded_in_prose() {
        let v = parse_verdict(
            "Here is my assessment.\n{\"verdict\":\"relevant\",\"confidence\":1.0}\nDone.",
        )
        .expect("valid");
        assert_eq!(v.verdict, VerdictLabel::Relevant);
    }

    #[test]
    fn prose_without_json_is_malformed() {
        assert!(parse_verdict("The results look relevant to me!").is_err());
    }

    #[test]
    fn unbalanced_json_is_malformed() {
        assert!(parse_verdict(r#"{"verdict":"relevant","confidence":0.5"#).is_err());
    }

    #[test]
    fn unknown_field_is_malformed() {
        // deny_unknown_fields: hallucinated keys invalidate the verdict.
        assert!(parse_verdict(
            r#"{"verdict":"relevant","confidence":0.5,"reasoning":"looks good"}"#
        )
        .is_err());
    }

    #[test]
    fn confidence_out_of_range_is_malformed() {
        assert!(parse_verdict(r#"{"verdict":"relevant","confidence":1.7}"#).is_err());
        assert!(parse_verdict(r#"{"verdict":"relevant","confidence":-0.1}"#).is_err());
    }

    #[test]
    fn wrong_verdict_enum_is_malformed() {
        assert!(parse_verdict(r#"{"verdict":"maybe","confidence":0.5}"#).is_err());
    }

    #[test]
    fn non_http_candidate_url_is_malformed() {
        assert!(parse_verdict(
            r#"{"verdict":"relevant","confidence":0.5,"candidate_urls":["ftp://x.example/e"]}"#
        )
        .is_err());
        assert!(parse_verdict(
            r#"{"verdict":"relevant","confidence":0.5,"candidate_urls":["not a url"]}"#
        )
        .is_err());
    }

    #[test]
    fn truncates_candidate_urls_at_20() {
        let urls: Vec<String> = (0..30).map(|i| format!("https://x.example/{i}")).collect();
        let body = serde_json::json!({
            "verdict": "relevant",
            "confidence": 0.5,
            "candidate_urls": urls,
        });
        let v = parse_verdict(&body.to_string()).expect("valid");
        assert_eq!(v.candidate_urls.len(), 20);
    }

    #[test]
    fn template_render_contains_rubric_and_schema_instruction() {
        let (system, user) = render_search_result_relevance(&rubric(), r#"{"queries":[]}"#);
        assert!(system.contains("ONLY a single JSON object"));
        assert!(user.contains("Find official county GIS portals"));
        assert!(user.contains("- official .gov domain"));
        assert!(user.contains("- third-party aggregators"));
        assert!(user.contains("\"verdict\":\"relevant\"|\"irrelevant\""));
        assert!(user.contains("{\"queries\":[]}"));
    }

    #[test]
    fn template_registry_knows_the_builtin_id() {
        let t = template_for("search_result_relevance_v1").expect("registered");
        assert_eq!(t.verdict_schema, CLASSIFY_VERDICT_SCHEMA_V1);
        assert!(template_for("something_else_v9").is_none());
    }

    #[tokio::test]
    async fn endpoint_builder_rejects_bad_urls() {
        assert!(LlmEndpoint::new("not a url", None, "m", Duration::from_secs(1)).is_err());
    }
}
