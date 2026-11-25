use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};

/// Request parameters for the `chat` MCP tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChatRequest {
    /// The message or prompt to send to the model (e.g. question, instructions, or review prompt).
    pub message: String,

    /// Optional list of file paths to attach as context (e.g. ["crates/foo/src/main.rs", "output_for_sol.md"]).
    /// Read directly from disk by the server, eliminating token overhead and truncation for the calling agent.
    pub context_files: Option<Vec<String>>,

    /// Optional session ID for multi-turn conversations. If provided, chat history is preserved.
    pub session_id: Option<String>,

    /// Optional system prompt / instructions for the model.
    pub system_prompt: Option<String>,

    /// Optional model identifier on AgentRouter (e.g. "gpt-6.1-sol"). If omitted, uses configured default model.
    pub model: Option<String>,

    /// If true, clears any existing history for `session_id` before sending this message.
    pub reset_session: Option<bool>,
}

/// Request parameters for the `reset_chat` MCP tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResetChatRequest {
    /// Session identifier to clear from memory and disk.
    pub session_id: String,
}

/// Request parameters for the `list_sessions` MCP tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListSessionsRequest {
    /// Optional maximum number of sessions to return (sorted by most recently updated first).
    pub limit: Option<usize>,
}

/// Request parameters for the `get_session_history` MCP tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetSessionHistoryRequest {
    /// Session identifier whose persisted message history should be retrieved.
    pub session_id: String,

    /// Optional limit to return only the last N messages from the session.
    pub last_n: Option<usize>,
}

/// Request parameters for the `agentrouter_health` MCP tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct HealthRequest {
    /// When true, also queries the AgentRouter /models endpoint to verify API token and network connectivity.
    pub ping_api: Option<bool>,
}

/// Message within an OpenAI-compatible chat payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
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
