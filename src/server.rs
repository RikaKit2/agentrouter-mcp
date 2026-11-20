use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use rmcp::{handler::server::wrapper::Parameters, tool, tool_router};
use tracing::{error, info};

use crate::{
    auth::{fetch_token_from_cmd, mask_token},
    client::AgentRouterClient,
    config::AppConfig,
    types::{ChatMessage, ChatRequest, HealthRequest, ResetChatRequest},
};

const MAX_SINGLE_FILE_BYTES: u64 = 1024 * 1024; // 1 MB per file
const MAX_TOTAL_FILES_BYTES: u64 = 3 * 1024 * 1024; // 3 MB total per request
const MAX_HISTORY_MESSAGES: usize = 30; // Max messages retained per session_id to avoid memory leaks

/// MCP server instance hosting model-agnostic AgentRouter chat and utility tools.
#[derive(Debug, Clone)]
pub struct AgentRouterServer {
    client: AgentRouterClient,
    sessions: Arc<Mutex<HashMap<String, Vec<ChatMessage>>>>,
}

impl AgentRouterServer {
    pub fn new(config: AppConfig) -> Result<Self, reqwest::Error> {
        let client = AgentRouterClient::new(config)?;
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        Ok(Self { client, sessions })
    }
}

/// Helper function to safely read files from disk and format them as markdown code blocks.
fn read_context_files(file_paths: &[String]) -> Result<String, String> {
    let mut total_bytes = 0u64;
    let mut out = String::new();

    for raw_path in file_paths {
        let path = std::path::Path::new(raw_path);

        // Security check: reject paths accessing sensitive files
        let path_str = raw_path.to_lowercase();
        if path_str.contains(".env")
            || path_str.contains(".ssh")
            || path_str.contains(".gnupg")
            || path_str.contains("id_rsa")
            || path_str.contains("id_ed25519")
            || path_str.contains("/etc/shadow")
            || path_str.contains("/etc/sudoers")
        {
            return Err(format!(
                "Security rejection: access to sensitive path '{}' is forbidden.",
                raw_path
            ));
        }

        if !path.exists() {
            return Err(format!("Context file not found: '{}'", raw_path));
        }
        if !path.is_file() {
            return Err(format!(
                "Context file path is not a regular file: '{}'",
                raw_path
            ));
        }

        let metadata = path
            .metadata()
            .map_err(|e| format!("Failed to read metadata for '{}': {e}", raw_path))?;
        let len = metadata.len();
        if len > MAX_SINGLE_FILE_BYTES {
            return Err(format!(
                "File '{}' exceeds maximum allowed size of 1 MB (actual: {} bytes).",
                raw_path, len
            ));
        }
        total_bytes += len;
        if total_bytes > MAX_TOTAL_FILES_BYTES {
            return Err(format!(
                "Total context files size exceeds limit of 3 MB (accumulated: {} bytes).",
                total_bytes
            ));
        }

        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read file '{}' as UTF-8 text: {e}", raw_path))?;

        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        out.push_str(&format!(
            "\n\n### File: `{}`\n```{}\n{}\n```\n",
            raw_path, ext, content
        ));
    }

    Ok(out)
}

#[tool_router(server_handler)]
impl AgentRouterServer {
    #[tool(
        description = "Send a prompt or message to any LLM on AgentRouter (such as gpt-6.1-sol or another configured model). Supports attaching context_files directly from disk (zero token overhead for caller), multi-turn conversation context via session_id, and custom system instructions."
    )]
    async fn chat(
        &self,
        Parameters(req): Parameters<ChatRequest>,
    ) -> Result<String, String> {
        let default_model = &self.client.config().default_model;
        let chosen_model = req.model.as_deref().unwrap_or(default_model);
        info!(
            "Executing chat request using model: '{}', session_id: '{:?}', context_files count: {}",
            chosen_model,
            req.session_id,
            req.context_files.as_ref().map(|f| f.len()).unwrap_or(0)
        );

        let key_cmd = &self.client.config().key_cmd;
        let token = match fetch_token_from_cmd(key_cmd) {
            Ok(token) => token,
            Err(e) => {
                error!("Authentication failure using command '{key_cmd}': {e}");
                return Err(format!(
                    "Authentication error: {e}.\nPlease ensure the key command works or update AGENTROUTER_KEY_CMD in your .env file."
                ));
            }
        };

        // Combine user message with any attached context files read directly from disk
        let effective_message = match req.context_files {
            Some(ref files) if !files.is_empty() => {
                let files_content = read_context_files(files)?;
                format!("{}\n\n## Attached Context Files:{files_content}", req.message)
            }
            _ => req.message,
        };

        match req.session_id {
            Some(session_id) => {
                let mut sessions = self.sessions.lock().await;
                if req.reset_session == Some(true) {
                    sessions.remove(&session_id);
                }

                let history = sessions.entry(session_id.clone()).or_default();

                // Inject custom system prompt if provided and not yet present
                if let Some(ref sys) = req.system_prompt {
                    if !sys.trim().is_empty() && (history.is_empty() || history[0].role != "system") {
                        history.insert(
                            0,
                            ChatMessage {
                                role: "system".to_string(),
                                content: sys.clone(),
                            },
                        );
                    }
                }

                history.push(ChatMessage {
                    role: "user".to_string(),
                    content: effective_message,
                });

                // Prune history if it exceeds threshold (keep system prompt at index 0)
                if history.len() > MAX_HISTORY_MESSAGES {
                    let has_system = !history.is_empty() && history[0].role == "system";
                    let drain_start = if has_system { 1 } else { 0 };
                    let to_remove = history.len() - MAX_HISTORY_MESSAGES;
                    history.drain(drain_start..drain_start + to_remove);
                }

                match self.client.chat(&token, chosen_model, history).await {
                    Ok(reply) => {
                        info!("Received chat reply ({} bytes)", reply.len());
                        history.push(ChatMessage {
                            role: "assistant".to_string(),
                            content: reply.clone(),
                        });
                        Ok(reply)
                    }
                    Err(e) => {
                        // Rollback unfulfilled user message on error
                        history.pop();
                        error!("AgentRouter chat error: {e}");
                        Err(format!("AgentRouter error: {e}"))
                    }
                }
            }
            None => {
                let mut messages = Vec::new();
                if let Some(sys) = req.system_prompt {
                    if !sys.trim().is_empty() {
                        messages.push(ChatMessage {
                            role: "system".to_string(),
                            content: sys,
                        });
                    }
                }
                messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: effective_message,
                });

                match self.client.chat(&token, chosen_model, &messages).await {
                    Ok(reply) => {
                        info!("Received chat reply ({} bytes)", reply.len());
                        Ok(reply)
                    }
                    Err(e) => {
                        error!("AgentRouter chat error: {e}");
                        Err(format!("AgentRouter error: {e}"))
                    }
                }
            }
        }
    }

    #[tool(
        description = "Reset and clear conversation history for a given session_id."
    )]
    async fn reset_chat(
        &self,
        Parameters(req): Parameters<ResetChatRequest>,
    ) -> Result<String, String> {
        let mut sessions = self.sessions.lock().await;
        match sessions.remove(&req.session_id) {
            Some(msgs) => Ok(format!(
                "Session '{}' successfully cleared (discarded {} messages).",
                req.session_id,
                msgs.len()
            )),
            None => Ok(format!("Session '{}' was not active.", req.session_id)),
        }
    }

    #[tool(description = "List all models currently available and active on AgentRouter.")]
    async fn list_models(&self) -> Result<String, String> {
        info!("Executing list_models request");

        let key_cmd = &self.client.config().key_cmd;
        let token = match fetch_token_from_cmd(key_cmd) {
            Ok(token) => token,
            Err(e) => {
                return Err(format!(
                    "Authentication error: {e}.\nFix: check AGENTROUTER_KEY_CMD command: '{key_cmd}'"
                ));
            }
        };

        match self.client.list_models(&token).await {
            Ok(models) => {
                let default_m = &self.client.config().default_model;
                let mut out = format!(
                    "AgentRouter Models (total: {} | default: '{}'):\n\n",
                    models.len(),
                    default_m
                );
                for m in models {
                    match m == *default_m {
                        true => out.push_str(&format!("- **{}** (default)\n", m)),
                        false => out.push_str(&format!("- {}\n", m)),
                    }
                }
                Ok(out)
            }
            Err(e) => Err(format!("Failed to list models from AgentRouter: {e}")),
        }
    }

    #[tool(description = "Check key retrieval command and connectivity to AgentRouter.")]
    async fn agentrouter_health(
        &self,
        Parameters(req): Parameters<HealthRequest>,
    ) -> Result<String, String> {
        info!("Executing agentrouter_health check");

        let key_cmd = &self.client.config().key_cmd;
        let token = match fetch_token_from_cmd(key_cmd) {
            Ok(t) => t,
            Err(e) => {
                return Err(format!(
                    "Health Check FAILED: Key retrieval command '{key_cmd}' failed: {e}.\nFix: verify your command or .env file (AGENTROUTER_KEY_CMD)."
                ));
            }
        };

        let masked = mask_token(&token);
        let ping_api = req.ping_api.unwrap_or(true);
        let default_m = &self.client.config().default_model;

        match ping_api {
            true => match self.client.ping_api(&token).await {
                Ok(status) => {
                    let msg = format!(
                        "AgentRouter Health: OK\n- Key: {masked}\n- Key Command: '{key_cmd}'\n- API Ping: {status}\n- Configured default model: '{default_m}'\n- Base URL: '{}'\n- API Mode: '{:?}'\n- User-Agent: '{}'",
                        self.client.config().base_url,
                        self.client.config().api_mode,
                        self.client.config().user_agent,
                    );
                    Ok(msg)
                }
                Err(e) => {
                    let msg = format!(
                        "AgentRouter Health: Partial (Key retrieved via '{key_cmd}', but API ping failed)\n- Key: {masked}\n- Error: {e}"
                    );
                    Err(msg)
                }
            },
            false => {
                let msg = format!(
                    "AgentRouter Health: OK (Key retrieved via '{key_cmd}': {masked}, API ping skipped, default model: '{default_m}')."
                );
                Ok(msg)
            }
        }
    }
}
