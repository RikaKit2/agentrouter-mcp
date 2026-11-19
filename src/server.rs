use rmcp::{handler::server::wrapper::Parameters, tool, tool_router};
use tracing::{error, info};

use crate::{
    auth::{fetch_token_from_cmd, mask_token},
    client::AgentRouterClient,
    config::AppConfig,
    types::{CodeReviewRequest, HealthRequest},
};

/// MCP server instance hosting model-agnostic AgentRouter review and utility tools.
#[derive(Debug, Clone)]
pub struct AgentRouterServer {
    client: AgentRouterClient,
}

impl AgentRouterServer {
    pub fn new(config: AppConfig) -> Result<Self, reqwest::Error> {
        let client = AgentRouterClient::new(config)?;
        Ok(Self { client })
    }
}

#[tool_router(server_handler)]
impl AgentRouterServer {
    #[tool(
        description = "Perform deep, thorough code review via AgentRouter. Analyzes correctness, concurrency, error invariants, memory safety, and provides prioritized findings. Supports any model on the platform."
    )]
    async fn code_review(
        &self,
        Parameters(req): Parameters<CodeReviewRequest>,
    ) -> Result<String, String> {
        let default_model = &self.client.config().default_model;
        let chosen_model = req.model.as_deref().unwrap_or(default_model);
        info!(
            "Executing code_review for title: '{}' using model: '{}'",
            req.title, chosen_model
        );

        let key_cmd = &self.client.config().key_cmd;
        let token = match fetch_token_from_cmd(key_cmd) {
            Ok(token) => token,
            Err(e) => {
                error!("Authentication failure using command '{key_cmd}': {e}");
                return Err(format!(
                    "Authentication error: {e}.\nPlease ensure the key command works or update AGENTROUTER_KEY_CMD."
                ));
            }
        };

        match self.client.request_review(&token, &req).await {
            Ok(review) => {
                info!("Successfully received code review ({} bytes)", review.len());
                Ok(review)
            }
            Err(e) => {
                error!("AgentRouter review error: {e}");
                Err(format!("Failed to generate review via AgentRouter: {e}"))
            }
        }
    }

    #[tool(
        description = "Alias for code_review. Retained for backward-compatible invocations."
    )]
    async fn sol_code_review(
        &self,
        params: Parameters<CodeReviewRequest>,
    ) -> Result<String, String> {
        self.code_review(params).await
    }

    #[tool(
        description = "List all models currently available and active on AgentRouter."
    )]
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

    #[tool(
        description = "Check key retrieval command and connectivity to AgentRouter."
    )]
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
                        "AgentRouter Health: OK\n- Key: {masked}\n- Key Command: '{key_cmd}'\n- API Ping: {status}\n- Configured default model: '{default_m}'\n- Base URL: '{}'",
                        self.client.config().base_url
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
