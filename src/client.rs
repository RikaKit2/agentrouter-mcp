use std::time::Duration;
use thiserror::Error;

use crate::{
    config::AppConfig,
    types::{
        ApiErrorEnvelope, ChatCompletionRequest, ChatCompletionResponse, ChatMessage,
        CodeReviewRequest, ModelsListResponse, ResponsesApiRequest, ResponsesApiResponse,
    },
};

/// Errors arising from HTTP communication with AgentRouter.
#[derive(Debug, Error)]
pub enum ClientError {
    #[error("network or transport error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("API error (HTTP {status}): {message}")]
    Api { status: u16, message: String },

    #[error("API returned zero completion choices")]
    EmptyChoices,

    #[error("failed to deserialize response: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// Model-agnostic HTTP client wrapper for AgentRouter API endpoints.
#[derive(Debug, Clone)]
pub struct AgentRouterClient {
    http: reqwest::Client,
    config: AppConfig,
}

impl AgentRouterClient {
    // WHY: AgentRouter WAF requires a known first-party coding tool User-Agent
    pub const DEFAULT_USER_AGENT: &'static str = "opencode/1.18.25";

    pub fn new(config: AppConfig) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .user_agent(Self::DEFAULT_USER_AGENT)
            .build()?;
        Ok(Self { http, config })
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    pub async fn request_review(
        &self,
        token: &str,
        req: &CodeReviewRequest,
    ) -> Result<String, ClientError> {
        let target_model = match &req.model {
            Some(m) if !m.trim().is_empty() => m.trim(),
            _ => self.config.default_model.as_str(),
        };

        let chat_url = format!("{}/chat/completions", self.config.base_url);
        let chat_payload = self.build_review_payload(req, target_model);

        let resp = self
            .http
            .post(&chat_url)
            .bearer_auth(token)
            .json(&chat_payload)
            .send()
            .await?;

        let status = resp.status();
        match status.is_success() {
            true => Self::parse_completion_response(resp).await,
            false => {
                let status_code = status.as_u16();
                let text = resp.text().await.unwrap_or_default();
                let message = Self::extract_error_message(status_code, &text);

                // Auto-fallback to OpenAI Responses API if upstream requires /v1/responses
                match status_code == 400 && message.contains("/v1/responses") {
                    true => {
                        self.request_review_via_responses(token, req, target_model)
                            .await
                    }
                    false => Err(ClientError::Api {
                        status: status_code,
                        message,
                    }),
                }
            }
        }
    }

    async fn request_review_via_responses(
        &self,
        token: &str,
        req: &CodeReviewRequest,
        model: &str,
    ) -> Result<String, ClientError> {
        let url = format!("{}/responses", self.config.base_url);
        let payload = self.build_responses_payload(req, model);

        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .json(&payload)
            .send()
            .await?;

        let status = resp.status();
        match status.is_success() {
            true => {
                let body: ResponsesApiResponse = resp.json().await?;
                let mut full_text = String::new();
                for item in body.output {
                    match item.item_type.as_deref() {
                        Some("message") => {
                            for block in item.content {
                                match block.text {
                                    Some(txt) => full_text.push_str(&txt),
                                    None => {}
                                }
                            }
                        }
                        _ => {}
                    }
                }

                match full_text.trim().is_empty() {
                    true => Err(ClientError::EmptyChoices),
                    false => Ok(full_text),
                }
            }
            false => {
                let status_code = status.as_u16();
                let text = resp.text().await.unwrap_or_default();
                let message = Self::extract_error_message(status_code, &text);
                Err(ClientError::Api {
                    status: status_code,
                    message,
                })
            }
        }
    }

    fn build_responses_payload(&self, req: &CodeReviewRequest, model: &str) -> ResponsesApiRequest {
        let system_msg = Self::system_prompt();
        let user_msg = Self::format_user_prompt(req);

        ResponsesApiRequest {
            model: model.to_string(),
            input: vec![
                ChatMessage {
                    role: "system".to_string(),
                    content: system_msg,
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: user_msg,
                },
            ],
        }
    }

    pub async fn list_models(&self, token: &str) -> Result<Vec<String>, ClientError> {
        let url = format!("{}/models", self.config.base_url);
        let resp = self.http.get(&url).bearer_auth(token).send().await?;

        let status = resp.status();
        match status.is_success() {
            true => {
                let body: ModelsListResponse = resp.json().await?;
                let mut ids: Vec<String> = body.data.into_iter().map(|m| m.id).collect();
                ids.sort();
                Ok(ids)
            }
            false => {
                let status_code = status.as_u16();
                let text = resp.text().await.unwrap_or_default();
                let message = Self::extract_error_message(status_code, &text);
                Err(ClientError::Api {
                    status: status_code,
                    message,
                })
            }
        }
    }

    pub async fn ping_api(&self, token: &str) -> Result<String, ClientError> {
        let models = self.list_models(token).await?;
        Ok(format!("Connection OK ({} models available)", models.len()))
    }

    fn build_review_payload(&self, req: &CodeReviewRequest, model: &str) -> ChatCompletionRequest {
        let system_msg = Self::system_prompt();
        let user_msg = Self::format_user_prompt(req);

        ChatCompletionRequest {
            model: model.to_string(),
            messages: vec![
                ChatMessage {
                    role: "system".to_string(),
                    content: system_msg,
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: user_msg,
                },
            ],
        }
    }

    fn system_prompt() -> String {
        [
            "You are an elite principal systems engineer and code reviewer.",
            "Perform a rigorous, prioritized code review focusing on:",
            "1. Correctness, edge cases, invariants, data races, and concurrency safety.",
            "2. Rust: strict borrow checker discipline (no gratuitous clones/Box::leak/unwrap_or_default), cancellation safety in async Tokio code, thiserror/anyhow conventions.",
            "3. NixOS/Nix: evaluation purity, hermeticity, module options, and flake hygiene.",
            "4. Structure your response into:",
            "   - [CRITICAL]: Broken invariants, safety hazards, leaks, or logic bugs.",
            "   - [WARNING]: Performance bottlenecks, edge case omissions, or questionable patterns.",
            "   - [SUGGESTION]: Idiomatic refactorings and style improvements.",
            "   - [VERDICT]: LGTM, LGTM with comments, or Request Changes (with brief summary).",
            "Be precise, reference exact line numbers/functions, and propose minimal drop-in replacement snippets.",
        ]
        .join("\n")
    }

    fn format_user_prompt(req: &CodeReviewRequest) -> String {
        let mut prompt = format!("# Title: {}\n\n", req.title);
        match &req.context {
            Some(ctx) if !ctx.trim().is_empty() => {
                prompt.push_str("## Additional Context / Invariants:\n");
                prompt.push_str(ctx.trim());
                prompt.push_str("\n\n");
            }
            _ => {}
        }
        prompt.push_str("## Code Diff / Changes:\n```\n");
        prompt.push_str(&req.diff);
        prompt.push_str("\n```\n");
        prompt
    }

    async fn parse_completion_response(resp: reqwest::Response) -> Result<String, ClientError> {
        let status = resp.status();
        match status.is_success() {
            true => {
                let body: ChatCompletionResponse = resp.json().await?;
                match body.choices.into_iter().next() {
                    Some(choice) => Ok(choice.message.content),
                    None => Err(ClientError::EmptyChoices),
                }
            }
            false => {
                let status_code = status.as_u16();
                let text = resp.text().await.unwrap_or_default();
                let message = Self::extract_error_message(status_code, &text);
                Err(ClientError::Api {
                    status: status_code,
                    message,
                })
            }
        }
    }

    fn extract_error_message(status: u16, body_text: &str) -> String {
        match serde_json::from_str::<ApiErrorEnvelope>(body_text) {
            Ok(env) => env.error.message,
            Err(_) => match status {
                401 => "Invalid AgentRouter API key. Check secret-tool storage.".to_string(),
                429 => "AgentRouter rate limit exceeded.".to_string(),
                _ => body_text.to_string(),
            },
        }
    }
}
