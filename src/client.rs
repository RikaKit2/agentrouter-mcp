use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::RwLock;

use crate::{
    config::{ApiMode, AppConfig},
    types::{
        ApiErrorEnvelope, ChatCompletionRequest, ChatCompletionResponse, ChatMessage,
        ModelsListResponse, ResponsesApiRequest, ResponsesApiResponse,
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
    known_responses_models: Arc<RwLock<HashSet<String>>>,
}

impl AgentRouterClient {
    pub fn new(config: AppConfig) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .user_agent(&config.user_agent)
            .build()?;
        let known_responses_models = Arc::new(RwLock::new(HashSet::new()));
        Ok(Self {
            http,
            config,
            known_responses_models,
        })
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    pub async fn chat(
        &self,
        token: &str,
        model: &str,
        messages: &[ChatMessage],
    ) -> Result<String, ClientError> {
        let target_model = match model.trim().is_empty() {
            true => self.config.default_model.as_str(),
            false => model.trim(),
        };

        let should_use_responses = match self.config.api_mode {
            ApiMode::Responses => true,
            ApiMode::Completions => false,
            ApiMode::Auto => {
                // Heuristic: models containing "sol" require /v1/responses on AgentRouter,
                // or any model previously discovered to require it during this session.
                target_model.contains("sol")
                    || self
                        .known_responses_models
                        .read()
                        .await
                        .contains(target_model)
            }
        };

        if should_use_responses {
            return self.chat_via_responses(token, target_model, messages).await;
        }

        // Attempt chat/completions with automatic fallback to responses if upstream rejects it
        match self
            .chat_via_completions(token, target_model, messages)
            .await
        {
            Ok(res) => Ok(res),
            Err(ClientError::Api { status: 400, message })
                if message.contains("/v1/responses") =>
            {
                tracing::info!(
                    "Model '{}' requires Responses API. Caching in known_responses_models.",
                    target_model
                );
                self.known_responses_models
                    .write()
                    .await
                    .insert(target_model.to_string());
                self.chat_via_responses(token, target_model, messages).await
            }
            Err(e) => Err(e),
        }
    }

    async fn chat_via_completions(
        &self,
        token: &str,
        model: &str,
        messages: &[ChatMessage],
    ) -> Result<String, ClientError> {
        let chat_url = format!("{}/chat/completions", self.config.base_url);
        let chat_payload = ChatCompletionRequest {
            model: model.to_string(),
            messages: messages.to_vec(),
        };

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
                Err(ClientError::Api {
                    status: status_code,
                    message,
                })
            }
        }
    }

    async fn chat_via_responses(
        &self,
        token: &str,
        model: &str,
        messages: &[ChatMessage],
    ) -> Result<String, ClientError> {
        let url = format!("{}/responses", self.config.base_url);

        // Extract system prompt into top-level instructions if present, per OpenAI Responses API
        let mut instructions: Option<String> = None;
        let mut input_messages: Vec<ChatMessage> = Vec::new();

        for msg in messages {
            match msg.role.as_str() {
                "system" => {
                    instructions = Some(msg.content.clone());
                }
                _ => {
                    input_messages.push(msg.clone());
                }
            }
        }

        let payload = ResponsesApiRequest {
            model: model.to_string(),
            input: input_messages,
            instructions,
        };

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
                                if let Some(txt) = block.text {
                                    full_text.push_str(&txt);
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
                401 => "Invalid AgentRouter API key or unauthorized client.".to_string(),
                429 => "AgentRouter rate limit exceeded.".to_string(),
                _ => body_text.to_string(),
            },
        }
    }
}
