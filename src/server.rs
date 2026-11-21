use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use rmcp::{handler::server::wrapper::Parameters, tool, tool_router};
use tracing::{error, info};

use crate::{
    auth::{mask_token, resolve_token},
    client::AgentRouterClient,
    config::AppConfig,
    types::{ChatMessage, ChatRequest, HealthRequest, ResetChatRequest},
};

const MAX_SINGLE_FILE_BYTES: u64 = 1024 * 1024; // 1 MB per file
const MAX_TOTAL_FILES_BYTES: u64 = 3 * 1024 * 1024; // 3 MB total per request
const MAX_HISTORY_MESSAGES: usize = 30; // Max messages retained per session_id to avoid memory leaks

type SessionHistory = Arc<Mutex<Vec<ChatMessage>>>;
type SessionStore = Arc<Mutex<HashMap<String, SessionHistory>>>;

/// MCP server instance hosting model-agnostic AgentRouter chat and utility tools.
#[derive(Debug, Clone)]
pub struct AgentRouterServer {
    client: AgentRouterClient,
    sessions: SessionStore,
}

impl AgentRouterServer {
    pub fn new(config: AppConfig) -> Result<Self, reqwest::Error> {
        let client = AgentRouterClient::new(config)?;
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        Ok(Self { client, sessions })
    }
}

/// Helper function to prune history while guaranteeing valid user/assistant alternation
/// and preserving any leading system prompt.
fn prune_history(history: &mut Vec<ChatMessage>, max_messages: usize) {
    let non_system_start = if !history.is_empty() && history[0].role == "system" {
        1
    } else {
        0
    };

    while history.len() > max_messages && history.len() > non_system_start {
        history.remove(non_system_start);
    }

    // Guarantee that after the optional system prompt, the history starts with a user message
    while history.len() > non_system_start && history[non_system_start].role != "user" {
        history.remove(non_system_start);
    }
}

/// Helper function to safely read files asynchronously from disk and format them using XML tags.
async fn read_context_files(file_paths: &[String]) -> Result<String, String> {
    let mut total_bytes = 0u64;
    let mut out = String::new();

    for raw_path in file_paths {
        let path = std::path::Path::new(raw_path);

        if !path.exists() {
            return Err(format!("Context file not found: '{raw_path}'"));
        }

        // Canonicalize first to resolve symlinks and normalize path before security inspection
        let canonical = tokio::fs::canonicalize(path)
            .await
            .map_err(|e| format!("Failed to canonicalize path '{raw_path}': {e}"))?;

        // Security check: reject paths accessing sensitive files (checked against canonical and raw paths)
        let canonical_str = canonical.to_string_lossy().to_lowercase();
        let raw_str = raw_path.to_lowercase();
        let is_sensitive = |s: &str| {
            s.contains(".env")
                || s.contains(".ssh")
                || s.contains(".gnupg")
                || s.contains(".aws")
                || s.contains(".netrc")
                || s.contains(".kube")
                || s.contains(".config/gh")
                || s.contains("id_rsa")
                || s.contains("id_ed25519")
                || s.contains("id_ecdsa")
                || s.contains("id_dsa")
                || s.contains("/etc/shadow")
                || s.contains("/etc/sudoers")
                || s.ends_with(".pem")
                || s.ends_with(".key")
        };

        if is_sensitive(&canonical_str) || is_sensitive(&raw_str) {
            return Err(format!(
                "Security rejection: access to sensitive path '{}' (resolved: '{}') is forbidden.",
                raw_path,
                canonical.display()
            ));
        }

        if !canonical.is_file() {
            return Err(format!(
                "Context file path is not a regular file: '{raw_path}'"
            ));
        }

        let metadata = tokio::fs::metadata(&canonical)
            .await
            .map_err(|e| format!("Failed to read metadata for '{raw_path}': {e}"))?;
        let len = metadata.len();
        if len > MAX_SINGLE_FILE_BYTES {
            return Err(format!(
                "File '{raw_path}' exceeds maximum allowed size of 1 MB (actual: {len} bytes)."
            ));
        }
        total_bytes += len;
        if total_bytes > MAX_TOTAL_FILES_BYTES {
            return Err(format!(
                "Total context files size exceeds limit of 3 MB (accumulated: {total_bytes} bytes)."
            ));
        }

        let content = tokio::fs::read_to_string(&canonical)
            .await
            .map_err(|e| format!("Failed to read file '{raw_path}' as UTF-8 text: {e}"))?;

        // Wrap in XML tags to safely avoid markdown code fence collisions (e.g. nested ``` in .md)
        out.push_str(&format!(
            "\n\n<file path=\"{raw_path}\">\n{content}\n</file>\n"
        ));
    }

    Ok(out)
}

#[tool_router(server_handler)]
impl AgentRouterServer {
    #[tool(
        description = "Send a prompt or message to any LLM on AgentRouter (such as gpt-6.1-sol or another configured model). Supports attaching context_files directly from disk (zero token overhead for caller), multi-turn conversation context via session_id, and custom system instructions."
    )]
    async fn chat(&self, Parameters(req): Parameters<ChatRequest>) -> Result<String, String> {
        let default_model = &self.client.config().default_model;
        let chosen_model = req.model.as_deref().unwrap_or(default_model);
        info!(
            "Executing chat request using model: '{}', session_id: '{:?}', context_files count: {}",
            chosen_model,
            req.session_id,
            req.context_files.as_ref().map(|f| f.len()).unwrap_or(0)
        );

        let key_cmd = &self.client.config().key_cmd;
        let token = match resolve_token(self.client.config().api_key.as_deref(), key_cmd).await {
            Ok(token) => token,
            Err(e) => {
                error!("Authentication failure using command '{key_cmd}': {e}");
                return Err(format!(
                    "Authentication error: {e}.\nPlease ensure the key command works or update AGENTROUTER_KEY_CMD / AGENTROUTER_API_KEY in your .env file."
                ));
            }
        };

        // Combine user message with any attached context files read asynchronously from disk
        let effective_message = match req.context_files {
            Some(ref files) if !files.is_empty() => {
                let files_content = read_context_files(files).await?;
                format!(
                    "{}\n\n## Attached Context Files:{files_content}",
                    req.message
                )
            }
            _ => req.message,
        };

        match req.session_id {
            Some(session_id) => {
                // Acquire global map lock briefly to retrieve or initialize the per-session Arc<Mutex>
                let session_lock = {
                    let mut sessions = self.sessions.lock().await;
                    if req.reset_session == Some(true) {
                        sessions.remove(&session_id);
                    }
                    Arc::clone(sessions.entry(session_id.clone()).or_default())
                };
                // Global sessions lock is dropped here immediately

                let mut history = session_lock.lock().await;

                // Inject custom system prompt if provided and not yet present
                match req.system_prompt.as_deref() {
                    Some(sys)
                        if !sys.trim().is_empty()
                            && (history.is_empty() || history[0].role != "system") =>
                    {
                        history.insert(
                            0,
                            ChatMessage {
                                role: "system".to_string(),
                                content: sys.to_string(),
                            },
                        );
                    }
                    _ => {}
                }

                history.push(ChatMessage {
                    role: "user".to_string(),
                    content: effective_message,
                });

                // Prune history if it exceeds threshold (keep system prompt at index 0 and ensure starts with user)
                prune_history(&mut history, MAX_HISTORY_MESSAGES);

                match self.client.chat(&token, chosen_model, &history).await {
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
                match req.system_prompt.as_deref() {
                    Some(sys) if !sys.trim().is_empty() => {
                        messages.push(ChatMessage {
                            role: "system".to_string(),
                            content: sys.to_string(),
                        });
                    }
                    _ => {}
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

    #[tool(description = "Reset and clear conversation history for a given session_id.")]
    async fn reset_chat(
        &self,
        Parameters(req): Parameters<ResetChatRequest>,
    ) -> Result<String, String> {
        let session_opt = {
            let mut sessions = self.sessions.lock().await;
            sessions.remove(&req.session_id)
        };

        match session_opt {
            Some(session_arc) => {
                let count = session_arc.lock().await.len();
                Ok(format!(
                    "Session '{}' successfully cleared (discarded {} messages).",
                    req.session_id, count
                ))
            }
            None => Ok(format!("Session '{}' was not active.", req.session_id)),
        }
    }

    #[tool(description = "List all models currently available and active on AgentRouter.")]
    async fn list_models(&self) -> Result<String, String> {
        info!("Executing list_models request");

        let key_cmd = &self.client.config().key_cmd;
        let token = match resolve_token(self.client.config().api_key.as_deref(), key_cmd).await {
            Ok(token) => token,
            Err(e) => {
                return Err(format!(
                    "Authentication error: {e}.\nFix: check AGENTROUTER_KEY_CMD command: '{key_cmd}' or configure AGENTROUTER_API_KEY"
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
        let token = match resolve_token(self.client.config().api_key.as_deref(), key_cmd).await {
            Ok(t) => t,
            Err(e) => {
                return Err(format!(
                    "Health Check FAILED: Key retrieval failed: {e}.\nFix: verify your command or .env file (AGENTROUTER_KEY_CMD / AGENTROUTER_API_KEY)."
                ));
            }
        };

        let masked = mask_token(&token);
        let ping_api = req.ping_api.unwrap_or(true);
        let default_m = &self.client.config().default_model;
        let key_source = match &self.client.config().api_key {
            Some(_) => "Direct AGENTROUTER_API_KEY".to_string(),
            None => format!("Key Command: '{key_cmd}'"),
        };

        match ping_api {
            true => match self.client.ping_api(&token).await {
                Ok(status) => {
                    let msg = format!(
                        "AgentRouter Health: OK\n- Key: {masked}\n- Key Source: {key_source}\n- API Ping: {status}\n- Configured default model: '{default_m}'\n- Base URL: '{}'\n- API Mode: '{:?}'\n- User-Agent: '{}'",
                        self.client.config().base_url,
                        self.client.config().api_mode,
                        self.client.config().user_agent,
                    );
                    Ok(msg)
                }
                Err(e) => {
                    let msg = format!(
                        "AgentRouter Health: Partial (Key retrieved via {key_source}, but API ping failed)\n- Key: {masked}\n- Error: {e}"
                    );
                    Err(msg)
                }
            },
            false => {
                let msg = format!(
                    "AgentRouter Health: OK (Key retrieved via {key_source}: {masked}, API ping skipped, default model: '{default_m}')."
                );
                Ok(msg)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prune_history_preserves_system_and_ensures_user_first() {
        let mut history = vec![
            ChatMessage {
                role: "system".to_string(),
                content: "sys".to_string(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: "u1".to_string(),
            },
            ChatMessage {
                role: "assistant".to_string(),
                content: "a1".to_string(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: "u2".to_string(),
            },
        ];

        // Pruning to 3 messages: removing 1 message must remove both u1 and a1
        // so that index 1 does not end up being an assistant!
        prune_history(&mut history, 3);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].role, "system");
        assert_eq!(history[1].role, "user");
        assert_eq!(history[1].content, "u2");
    }

    #[test]
    fn test_prune_history_without_system() {
        let mut history = vec![
            ChatMessage {
                role: "user".to_string(),
                content: "u1".to_string(),
            },
            ChatMessage {
                role: "assistant".to_string(),
                content: "a1".to_string(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: "u2".to_string(),
            },
        ];

        prune_history(&mut history, 2);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, "user");
        assert_eq!(history[0].content, "u2");
    }

    #[tokio::test]
    #[ignore]
    async fn test_live_smoke_two_messages_session_with_responses_api() {
        let mut config = AppConfig::from_args();
        config.api_mode = crate::config::ApiMode::Responses;
        let server = AgentRouterServer::new(config).expect("server init");

        // Message 1 with system_prompt and context_files
        let req1 = ChatRequest {
            message: "What is the package name in Cargo.toml? Answer with just the name."
                .to_string(),
            context_files: Some(vec!["Cargo.toml".to_string()]),
            session_id: Some("smoke-test-session-v2".to_string()),
            system_prompt: Some(
                "You are a concise testing bot. Output only the short factual answer.".to_string(),
            ),
            model: None,
            reset_session: Some(true),
        };
        let resp1 = server
            .chat(rmcp::handler::server::wrapper::Parameters(req1))
            .await
            .expect("turn 1 failed");
        println!("\n=== Turn 1 Response ===\n{resp1}");
        assert!(!resp1.trim().is_empty());

        // Message 2 with same session_id (no context_files) to verify session history retention
        let req2 = ChatRequest {
            message: "What did I ask you in the previous turn? Answer in one short sentence."
                .to_string(),
            context_files: None,
            session_id: Some("smoke-test-session-v2".to_string()),
            system_prompt: None,
            model: None,
            reset_session: None,
        };
        let resp2 = server
            .chat(rmcp::handler::server::wrapper::Parameters(req2))
            .await
            .expect("turn 2 failed");
        println!("\n=== Turn 2 Response ===\n{resp2}\n======================");
        assert!(!resp2.trim().is_empty());
    }
}
