use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};

/// Request parameters for the `code_review` MCP tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CodeReviewRequest {
    /// Brief title or summary of the review target (e.g. "PR #12: Add auth module")
    pub title: String,

    /// Git diff or source code changes to review
    pub diff: String,

    /// Optional context: architecture rules, compiler errors, types, or specific review questions
    pub context: Option<String>,

    /// Optional model identifier on AgentRouter. If omitted, uses the configured default model.
    pub model: Option<String>,
}

/// Request parameters for the `agentrouter_health` MCP tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct HealthRequest {
    /// When true, also queries the AgentRouter /models endpoint to verify API token and network connectivity
    pub ping_api: Option<bool>,
}

/// Message within an OpenAI-compatible chat payload.
#[derive(Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

/// Request payload sent to `/chat/completions`.
#[derive(Debug, Serialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
}

/// Choice entry in the OpenAI-compatible response.
#[derive(Debug, Deserialize)]
pub struct ChatChoice {
    pub message: ChatMessage,
    #[allow(dead_code)]
    pub finish_reason: Option<String>,
}

/// Response payload from OpenAI-compatible chat completion.
#[derive(Debug, Deserialize)]
pub struct ChatCompletionResponse {
    pub choices: Vec<ChatChoice>,
}

/// Model descriptor returned by `/models`.
#[derive(Debug, Deserialize)]
pub struct ModelItem {
    pub id: String,
    #[allow(dead_code)]
    pub object: Option<String>,
}

/// Response payload from `/models`.
#[derive(Debug, Deserialize)]
pub struct ModelsListResponse {
    pub data: Vec<ModelItem>,
}

/// Detailed error from OpenAI-compatible API error body.
#[derive(Debug, Deserialize)]
pub struct ApiErrorDetail {
    pub message: String,
    #[allow(dead_code)]
    pub code: Option<String>,
}

/// Error body wrapper from OpenAI-compatible API.
#[derive(Debug, Deserialize)]
pub struct ApiErrorEnvelope {
    pub error: ApiErrorDetail,
}

/// Request payload sent to OpenAI Responses API (`/v1/responses`).
#[derive(Debug, Serialize)]
pub struct ResponsesApiRequest {
    pub model: String,
    pub input: Vec<ChatMessage>,
}

/// Content block in Responses API message output.
#[derive(Debug, Deserialize)]
pub struct ResponseContentBlock {
    #[allow(dead_code)]
    #[serde(rename = "type")]
    pub block_type: Option<String>,
    pub text: Option<String>,
}

/// Output item in Responses API response.
#[derive(Debug, Deserialize)]
pub struct ResponseOutputItem {
    #[serde(rename = "type")]
    pub item_type: Option<String>,
    #[serde(default)]
    pub content: Vec<ResponseContentBlock>,
}

/// Response payload from OpenAI Responses API (`/v1/responses`).
#[derive(Debug, Deserialize)]
pub struct ResponsesApiResponse {
    #[serde(default)]
    pub output: Vec<ResponseOutputItem>,
}
