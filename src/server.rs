use rmcp::{handler::server::wrapper::Parameters, tool, tool_router};
use tracing::{error, info};

use crate::{
    auth::{mask_token, resolve_token},
    client::AgentRouterClient,
    config::AppConfig,
    session_store::{SessionStore, encode_session_id, prune_history},
    types::{
        ChatMessage, ChatRequest, GetSessionHistoryRequest, HealthRequest, ListSessionsRequest,
        ResetChatRequest,
    },
};

const MAX_SINGLE_FILE_BYTES: u64 = 1024 * 1024; // 1 MB per file
const MAX_TOTAL_FILES_BYTES: u64 = 3 * 1024 * 1024; // 3 MB total per request

/// MCP server instance hosting model-agnostic AgentRouter chat and utility tools.
#[derive(Debug, Clone)]
pub struct AgentRouterServer {
    client: AgentRouterClient,
    sessions: SessionStore,
}

impl AgentRouterServer {
    pub fn new(config: AppConfig) -> Result<Self, reqwest::Error> {
        let sessions = SessionStore::new(config.sessions_dir.clone(), config.max_history_messages);
        let client = AgentRouterClient::new(config)?;
        Ok(Self { client, sessions })
    }
}

/// Format a UNIX timestamp (seconds) as a readable UTC date-time string (`YYYY-MM-DD HH:MM:SS UTC`).
fn format_unix_utc(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let secs_of_day = unix_secs % 86_400;
    let hour = secs_of_day / 3_600;
    let minute = (secs_of_day % 3_600) / 60;
    let second = secs_of_day % 60;

    // Howard Hinnant civil-from-days algorithm
    let z = days + 719_468;
    let era = match z >= 0 {
        true => z / 146_097,
        false => (z - 146_096) / 146_097,
    };
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = match mp < 10 {
        true => mp + 3,
        false => mp - 9,
    };
    let year = match m <= 2 {
        true => y + 1,
        false => y,
    };

    format!("{year:04}-{m:02}-{d:02} {hour:02}:{minute:02}:{second:02} UTC")
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
        description = "Send a prompt or message to any LLM on AgentRouter (such as gpt-6.1-sol or another configured model). Supports attaching context_files directly from disk (zero token overhead for caller), persistent multi-turn conversation context via session_id, and custom system instructions."
    )]
    async fn chat(&self, Parameters(req): Parameters<ChatRequest>) -> Result<String, String> {
        // Validate session_id upfront before auth or disk file I/O
        match req.session_id.as_deref() {
            Some(sid) => {
                encode_session_id(sid).map_err(|e| e.to_string())?;
            }
            None => {}
        }

        let default_model = &self.client.config().default_model;
        let chosen_model = match req.model.as_deref() {
            Some(m) => m,
            None => default_model,
        };
        let files_count = match req.context_files.as_ref() {
            Some(files) => files.len(),
            None => 0,
        };
        info!(
            "Executing chat request using model: '{}', session_id: '{:?}', context_files count: {}",
            chosen_model, req.session_id, files_count
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
                let reset = req.reset_session == Some(true);
                let slot_arc = self
                    .sessions
                    .acquire_slot(&session_id)
                    .await
                    .map_err(|e| e.to_string())?;
                let mut slot = slot_arc.lock().await;
                let session = self
                    .sessions
                    .load_or_reset(&session_id, &mut slot, reset)
                    .await
                    .map_err(|e| e.to_string())?;

                // Inject custom system prompt if provided and not yet present at index 0
                let inserted_system = match req.system_prompt.as_deref() {
                    Some(sys)
                        if !sys.trim().is_empty()
                            && (session.messages.is_empty()
                                || session.messages[0].role != "system") =>
                    {
                        session.messages.insert(
                            0,
                            ChatMessage {
                                role: "system".to_string(),
                                content: sys.to_string(),
                            },
                        );
                        true
                    }
                    _ => false,
                };

                session.messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: effective_message,
                });

                // Prune history before sending so upstream payload stays bounded
                prune_history(&mut session.messages, self.sessions.max_history_messages());

                match self
                    .client
                    .chat(&token, chosen_model, &session.messages)
                    .await
                {
                    Ok(reply) => {
                        info!("Received chat reply ({} bytes)", reply.len());
                        session.messages.push(ChatMessage {
                            role: "assistant".to_string(),
                            content: reply.clone(),
                        });
                        match self.sessions.save_session(session, chosen_model).await {
                            Ok(()) => Ok(reply),
                            Err(e) => {
                                error!("Failed to persist session '{session_id}': {e}");
                                Err(format!("Failed to persist session '{session_id}': {e}"))
                            }
                        }
                    }
                    Err(e) => {
                        // Rollback unfulfilled user message (and newly added system prompt if empty)
                        session.messages.pop();
                        match inserted_system && session.messages.len() == 1 {
                            true => {
                                session.messages.pop();
                            }
                            false => {}
                        }
                        self.sessions.cleanup_if_empty(&mut slot);
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

    #[tool(
        description = "Reset and clear conversation history for a given session_id from memory and disk."
    )]
    async fn reset_chat(
        &self,
        Parameters(req): Parameters<ResetChatRequest>,
    ) -> Result<String, String> {
        match self.sessions.reset_session(&req.session_id).await {
            Ok(Some(count)) => Ok(format!(
                "Session '{}' successfully cleared (discarded {} messages).",
                req.session_id, count
            )),
            Ok(None) => Ok(format!("Session '{}' was not active.", req.session_id)),
            Err(e) => Err(format!("Failed to reset session '{}': {e}", req.session_id)),
        }
    }

    #[tool(
        description = "List all persisted conversation sessions with model, message count, and last update timestamp."
    )]
    async fn list_sessions(
        &self,
        Parameters(req): Parameters<ListSessionsRequest>,
    ) -> Result<String, String> {
        let sessions = self
            .sessions
            .list_sessions()
            .await
            .map_err(|e| format!("Failed to list sessions: {e}"))?;

        match sessions.is_empty() {
            true => Ok("No saved sessions found.".to_string()),
            false => {
                let total = sessions.len();
                let displayed: Vec<_> = match req.limit {
                    Some(limit) => sessions.into_iter().take(limit).collect(),
                    None => sessions,
                };
                let mut out = format!(
                    "Saved sessions (showing {} of {}):\n\n",
                    displayed.len(),
                    total
                );
                for item in displayed {
                    let model_name = item.model.as_deref().unwrap_or("unknown");
                    let formatted_ts = format_unix_utc(item.updated_at_unix);
                    out.push_str(&format!(
                        "- **{}** | model: {} | messages: {} | updated: {} (unix: {})\n",
                        item.session_id,
                        model_name,
                        item.message_count,
                        formatted_ts,
                        item.updated_at_unix
                    ));
                }
                Ok(out)
            }
        }
    }

    #[tool(
        description = "Retrieve saved conversation messages for a persisted session_id without calling the AgentRouter API."
    )]
    async fn get_session_history(
        &self,
        Parameters(req): Parameters<GetSessionHistoryRequest>,
    ) -> Result<String, String> {
        let session_opt = self
            .sessions
            .get_session(&req.session_id)
            .await
            .map_err(|e| format!("Failed to load session '{}': {e}", req.session_id))?;

        match session_opt {
            None => Err(format!("Session '{}' not found.", req.session_id)),
            Some(session) => {
                let total = session.messages.len();
                let start = match req.last_n {
                    Some(n) => total.saturating_sub(n),
                    None => 0,
                };
                let slice = &session.messages[start..];
                let model_name = session.model.as_deref().unwrap_or("unknown");
                let formatted_ts = format_unix_utc(session.updated_at_unix);
                let mut out = format!(
                    "Session '{}' | model: {} | showing {} of {} messages | updated: {} (unix: {})\n",
                    session.session_id,
                    model_name,
                    slice.len(),
                    total,
                    formatted_ts,
                    session.updated_at_unix
                );

                for (offset, message) in slice.iter().enumerate() {
                    let index = start + offset + 1;
                    out.push_str(&format!(
                        "\n[#{} {}]:\n{}\n",
                        index, message.role, message.content
                    ));
                }

                Ok(out)
            }
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
                        "AgentRouter Health: OK\n- Key: {masked}\n- Key Source: {key_source}\n- API Ping: {status}\n- Configured default model: '{default_m}'\n- Base URL: '{}'\n- API Mode: '{:?}'\n- User-Agent: '{}'\n- Sessions Dir: '{}'\n- Max History: {}",
                        self.client.config().base_url,
                        self.client.config().api_mode,
                        self.client.config().user_agent,
                        self.sessions.sessions_dir().display(),
                        self.sessions.max_history_messages(),
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
                    "AgentRouter Health: OK (Key retrieved via {key_source}: {masked}, API ping skipped, default model: '{default_m}', sessions dir: '{}', max history: {}).",
                    self.sessions.sessions_dir().display(),
                    self.sessions.max_history_messages(),
                );
                Ok(msg)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ApiMode;
    use crate::session_store::encode_session_id;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU64, Ordering},
    };

    static TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempDirGuard {
        path: PathBuf,
    }

    impl TempDirGuard {
        fn new(label: &str) -> Self {
            let seq = TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "agentrouter-server-test-{label}-{}-{seq}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            Self { path }
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn spawn_mock_completions_server(
        responses: Vec<(u16, String)>,
    ) -> (String, Arc<StdMutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let addr = listener.local_addr().expect("local addr");
        let captured_bodies = Arc::new(StdMutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured_bodies);

        std::thread::spawn(move || {
            for (status, body_text) in responses {
                let (mut stream, _) = match listener.accept() {
                    Ok(conn) => conn,
                    Err(_) => break,
                };
                let mut buf = [0u8; 16384];
                let read_len = stream.read(&mut buf).unwrap_or(0);
                let req_str = String::from_utf8_lossy(&buf[..read_len]).to_string();
                if let Some((_, json_part)) = req_str.split_once("\r\n\r\n") {
                    captured_clone
                        .lock()
                        .expect("lock captured")
                        .push(json_part.to_string());
                }

                let response_payload = match status {
                    200 => format!(
                        r#"{{"choices":[{{"message":{{"role":"assistant","content":"{body_text}"}},"finish_reason":"stop"}}]}}"#
                    ),
                    _ => format!(r#"{{"error":{{"message":"{body_text}"}}}}"#),
                };
                let http_resp = format!(
                    "HTTP/1.1 {status} MOCK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_payload.len(),
                    response_payload
                );
                let _ = stream.write_all(http_resp.as_bytes());
                let _ = stream.flush();
            }
        });

        (format!("http://{addr}/v1"), captured_bodies)
    }

    fn test_config(base_url: String, sessions_dir: PathBuf, max_history: usize) -> AppConfig {
        AppConfig {
            default_model: "mock-model".to_string(),
            base_url,
            timeout_secs: 10,
            key_cmd: "false".to_string(),
            api_key: Some("test-token-123456".to_string()),
            api_mode: ApiMode::Completions,
            user_agent: "test-agent/1.0".to_string(),
            sessions_dir,
            max_history_messages: max_history,
        }
    }

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
    async fn test_server_persists_across_restarts_and_tools_work() {
        let guard = TempDirGuard::new("persist-tools");
        let (base_url, captured) = spawn_mock_completions_server(vec![
            (200, "reply-1".to_string()),
            (200, "reply-2".to_string()),
            (200, "reply-3".to_string()),
        ]);

        // Server instance 1: two turns, repeated system_prompt must not duplicate history[0]
        let server1 = AgentRouterServer::new(test_config(base_url.clone(), guard.path.clone(), 30))
            .expect("server1");
        let r1 = server1
            .chat(Parameters(ChatRequest {
                message: "msg1".to_string(),
                context_files: None,
                session_id: Some("sess-alpha".to_string()),
                system_prompt: Some("sys-prompt".to_string()),
                model: None,
                reset_session: None,
            }))
            .await
            .expect("turn 1");
        assert_eq!(r1, "reply-1");

        let r2 = server1
            .chat(Parameters(ChatRequest {
                message: "msg2".to_string(),
                context_files: None,
                session_id: Some("sess-alpha".to_string()),
                system_prompt: Some("sys-prompt-ignored".to_string()),
                model: Some("custom-model".to_string()),
                reset_session: None,
            }))
            .await
            .expect("turn 2");
        assert_eq!(r2, "reply-2");

        // Server instance 2 (fresh in-memory map) reads from disk via tools and continues chat
        let server2 =
            AgentRouterServer::new(test_config(base_url, guard.path.clone(), 30)).expect("server2");

        let list_out = server2
            .list_sessions(Parameters(ListSessionsRequest { limit: Some(5) }))
            .await
            .expect("list_sessions");
        assert!(list_out.contains("sess-alpha"));
        assert!(list_out.contains("custom-model"));
        assert!(list_out.contains("messages: 5"));

        let hist_full = server2
            .get_session_history(Parameters(GetSessionHistoryRequest {
                session_id: "sess-alpha".to_string(),
                last_n: None,
            }))
            .await
            .expect("get_session_history full");
        assert!(hist_full.contains("[#1 system]:\nsys-prompt"));
        assert!(!hist_full.contains("sys-prompt-ignored"));
        assert!(hist_full.contains("[#5 assistant]:\nreply-2"));

        let hist_last_2 = server2
            .get_session_history(Parameters(GetSessionHistoryRequest {
                session_id: "sess-alpha".to_string(),
                last_n: Some(2),
            }))
            .await
            .expect("get_session_history last_n");
        assert!(hist_last_2.contains("showing 2 of 5 messages"));
        assert!(!hist_last_2.contains("[#1 system]"));
        assert!(hist_last_2.contains("[#4 user]:\nmsg2"));
        assert!(hist_last_2.contains("[#5 assistant]:\nreply-2"));

        // Turn 3 on server2 must send restored history to upstream
        let r3 = server2
            .chat(Parameters(ChatRequest {
                message: "msg3".to_string(),
                context_files: None,
                session_id: Some("sess-alpha".to_string()),
                system_prompt: None,
                model: None,
                reset_session: None,
            }))
            .await
            .expect("turn 3");
        assert_eq!(r3, "reply-3");

        let bodies = captured.lock().expect("captured lock");
        assert_eq!(bodies.len(), 3);
        let req3_json: serde_json::Value =
            serde_json::from_str(&bodies[2]).expect("parse req3 json");
        let req3_msgs = req3_json["messages"].as_array().expect("messages array");
        assert_eq!(req3_msgs.len(), 6);
        assert_eq!(req3_msgs[0]["role"], "system");
        assert_eq!(req3_msgs[5]["content"], "msg3");
    }

    #[tokio::test]
    async fn test_server_reset_chat_and_reset_session_delete_disk() {
        let guard = TempDirGuard::new("reset-tools");
        let (base_url, _) = spawn_mock_completions_server(vec![
            (200, "first".to_string()),
            (200, "second".to_string()),
        ]);

        let server =
            AgentRouterServer::new(test_config(base_url, guard.path.clone(), 30)).expect("server");

        server
            .chat(Parameters(ChatRequest {
                message: "hello".to_string(),
                context_files: None,
                session_id: Some("to-reset".to_string()),
                system_prompt: None,
                model: None,
                reset_session: None,
            }))
            .await
            .expect("turn 1");

        let disk_file = guard.path.join("to-reset.json");
        assert!(disk_file.exists());

        // reset_session: Some(true) deletes old session and starts fresh
        server
            .chat(Parameters(ChatRequest {
                message: "fresh".to_string(),
                context_files: None,
                session_id: Some("to-reset".to_string()),
                system_prompt: None,
                model: None,
                reset_session: Some(true),
            }))
            .await
            .expect("turn 2 with reset_session");

        let hist = server
            .get_session_history(Parameters(GetSessionHistoryRequest {
                session_id: "to-reset".to_string(),
                last_n: None,
            }))
            .await
            .expect("get_session_history");
        assert!(hist.contains("showing 2 of 2 messages"));
        assert!(!hist.contains("hello"));
        assert!(hist.contains("fresh"));

        // reset_chat deletes from memory and disk
        let reset_msg = server
            .reset_chat(Parameters(ResetChatRequest {
                session_id: "to-reset".to_string(),
            }))
            .await
            .expect("reset_chat");
        assert!(reset_msg.contains("discarded 2 messages"));
        assert!(!disk_file.exists());

        let missing = server
            .get_session_history(Parameters(GetSessionHistoryRequest {
                session_id: "to-reset".to_string(),
                last_n: None,
            }))
            .await;
        assert!(missing.is_err());
    }

    #[tokio::test]
    async fn test_server_api_error_does_not_persist_and_traversal_is_contained() {
        let guard = TempDirGuard::new("error-and-traversal");
        let (base_url, _) = spawn_mock_completions_server(vec![
            (500, "upstream failure".to_string()),
            (200, "safe".to_string()),
        ]);

        let server =
            AgentRouterServer::new(test_config(base_url, guard.path.clone(), 30)).expect("server");

        // API error must not create session file on disk
        let err_res = server
            .chat(Parameters(ChatRequest {
                message: "will fail".to_string(),
                context_files: None,
                session_id: Some("err-sess".to_string()),
                system_prompt: Some("sys".to_string()),
                model: None,
                reset_session: None,
            }))
            .await;
        assert!(err_res.is_err());
        assert!(!guard.path.join("err-sess.json").exists());
        assert!(
            server
                .get_session_history(Parameters(GetSessionHistoryRequest {
                    session_id: "err-sess".to_string(),
                    last_n: None,
                }))
                .await
                .is_err()
        );

        // Empty session_id is rejected
        let empty_res = server
            .chat(Parameters(ChatRequest {
                message: "hi".to_string(),
                context_files: None,
                session_id: Some("".to_string()),
                system_prompt: None,
                model: None,
                reset_session: None,
            }))
            .await;
        assert!(empty_res.is_err());

        // Path traversal session_id is safely encoded inside sessions_dir
        let traversal_id = "../../etc/passwd";
        let ok_res = server
            .chat(Parameters(ChatRequest {
                message: "check traversal".to_string(),
                context_files: None,
                session_id: Some(traversal_id.to_string()),
                system_prompt: None,
                model: None,
                reset_session: None,
            }))
            .await;
        assert_eq!(ok_res.expect("traversal safe chat"), "safe");

        let encoded = encode_session_id(traversal_id).expect("encoded");
        let expected_path = guard.path.join(format!("{encoded}.json"));
        assert!(expected_path.exists());
        assert_eq!(expected_path.parent(), Some(guard.path.as_path()));
    }

    #[tokio::test]
    #[ignore]
    async fn test_live_smoke_two_messages_session_with_responses_api() {
        let mut config = AppConfig::from_args();
        config.api_mode = ApiMode::Responses;
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
        let resp1 = server.chat(Parameters(req1)).await.expect("turn 1 failed");
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
        let resp2 = server.chat(Parameters(req2)).await.expect("turn 2 failed");
        println!("\n=== Turn 2 Response ===\n{resp2}\n======================");
        assert!(!resp2.trim().is_empty());
    }
}
