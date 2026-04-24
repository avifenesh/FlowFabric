//! Shared types + OpenRouter helpers for the llm-race UC-38 example.
//!
//! The example races three free OpenRouter providers against the same
//! prompt as sibling flow-member executions with an
//! `AnyOf { CancelRemaining }` edge group on the downstream aggregator
//! (RFC-016 Stage B/C). The aggregator streams the winning response
//! into a `DurableSummary` stream with `JsonMergePatch` deltas
//! (RFC-015).

use serde::{Deserialize, Serialize};

pub const DEFAULT_SERVER_URL: &str = "http://localhost:9090";
pub const DEFAULT_NAMESPACE: &str = "llm-race";

pub const LANE_PROVIDER: &str = "provider";
pub const LANE_AGGREGATOR: &str = "aggregator";
pub const LANE_REVIEW: &str = "review";

pub const EXECUTION_KIND_PROVIDER: &str = "llm_race_provider";
pub const EXECUTION_KIND_AGGREGATOR: &str = "llm_race_aggregator";

pub const CAP_ROLE_PROVIDER: &str = "role=provider";
pub const CAP_ROLE_AGGREGATOR: &str = "role=aggregate";

/// Payload submitted with each provider execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderPayload {
    pub prompt: String,
    pub model: String,
    pub max_tokens: u32,
}

/// Payload submitted with the aggregator execution.
///
/// The aggregator claims after one provider upstream satisfies the
/// `AnyOf { CancelRemaining }` edge group. It reads the winning
/// provider's result (via `data_passing_ref` on the staged edge — or
/// simply by tailing upstream streams; the example takes the simpler
/// path and lets the provider's complete-payload flow through the
/// edge's input_payload projection).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregatorPayload {
    pub prompt: String,
    /// Optional: waitpoint_key that HITL reviewers signal to approve
    /// the winner before it's materialised into the final summary.
    /// `None` when `--review` was not set on the submitter.
    pub review_waitpoint_key: Option<String>,
}

/// Final aggregated output surfaced to the submit CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinnerRecord {
    pub model: String,
    pub output: String,
    pub tokens_used: u64,
}

/// Review signal payload (same shape as coding-agent uses).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewPayload {
    pub approved: bool,
    pub reviewer: String,
}

/// One OpenRouter model discovered from `GET /api/v1/models`.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenRouterModel {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub pricing: OpenRouterPricing,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenRouterPricing {
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub completion: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ModelsEnvelope {
    data: Vec<OpenRouterModel>,
}

/// Hit the OpenRouter models endpoint and return entries where BOTH
/// `pricing.prompt` and `pricing.completion` are the string `"0"`.
/// Caller filters down to the top-N they want (the example takes up to
/// 3). No API key is required for this endpoint.
pub async fn discover_free_models(
    client: &reqwest::Client,
) -> Result<Vec<OpenRouterModel>, Box<dyn std::error::Error>> {
    let resp = client
        .get("https://openrouter.ai/api/v1/models")
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(format!("openrouter /models returned {}", resp.status()).into());
    }
    let env: ModelsEnvelope = resp.json().await?;
    Ok(env
        .data
        .into_iter()
        .filter(|m| {
            matches!(&m.pricing.prompt, Some(p) if p == "0")
                && matches!(&m.pricing.completion, Some(c) if c == "0")
        })
        .collect())
}

/// Chat completion request body for OpenRouter.
#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct ChatUsage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<ChatUsage>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
}

#[derive(Debug, Deserialize)]
struct ChatChoiceMessage {
    #[serde(default)]
    content: Option<String>,
}

/// Call OpenRouter chat completions. Non-streaming — the provider
/// worker returns the full response in one shot; the aggregator
/// worker then fans that out as merge-patch deltas on its own summary
/// stream.
pub async fn openrouter_chat(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    prompt: &str,
    max_tokens: u32,
) -> Result<(String, ChatUsage), Box<dyn std::error::Error>> {
    let body = serde_json::json!({
        "model": model,
        "messages": [ChatMessage { role: "user".into(), content: prompt.into() }],
        "max_tokens": max_tokens,
    });
    let resp = client
        .post("https://openrouter.ai/api/v1/chat/completions")
        .header("Authorization", format!("Bearer {}", api_key))
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("openrouter chat returned {status}: {text}").into());
    }
    let parsed: ChatResponse = resp.json().await?;
    let content = parsed
        .choices
        .first()
        .and_then(|c| c.message.content.clone())
        .filter(|s| !s.is_empty())
        .ok_or("openrouter returned empty content")?;
    let usage = parsed.usage.unwrap_or(ChatUsage {
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
    });
    Ok((content, usage))
}
