use std::path::{Path, PathBuf};

/// Runtime configuration for the AgentRouter MCP server.
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// Default model to use when not specified in the tool call
    pub default_model: String,

    /// Base URL for the OpenAI-compatible AgentRouter API
    pub base_url: String,

    /// HTTP request timeout in seconds
    pub timeout_secs: u64,

    /// Shell command executed on-demand to retrieve the API key from a secure vault/tool
    pub key_cmd: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            default_model: Self::DEFAULT_FALLBACK_MODEL.to_string(),
            base_url: Self::DEFAULT_BASE_URL.to_string(),
            timeout_secs: Self::DEFAULT_TIMEOUT_SECS,
            key_cmd: Self::DEFAULT_KEY_CMD.to_string(),
        }
    }
}

impl AppConfig {
    pub const DEFAULT_BASE_URL: &'static str = "https://agentrouter.org/v1";
    pub const DEFAULT_FALLBACK_MODEL: &'static str = "gpt-6.1-sol";
    pub const DEFAULT_TIMEOUT_SECS: u64 = 300;
    pub const DEFAULT_KEY_CMD: &'static str = "secret-tool lookup service agentrouter";

    /// Parse configuration from CLI arguments, environment variables, and optional .env files.
    ///
    /// Priority (highest to lowest):
    /// 1. Explicit CLI arguments (`--key-cmd`, `--default-model`, `--base-url`, `--timeout`)
    /// 2. Process environment variables (`AGENTROUTER_KEY_CMD`, `AGENTROUTER_DEFAULT_MODEL`, etc.)
    /// 3. `.env` file (specified via `--env-file` or found in project/current directory)
    /// 4. Built-in defaults
    pub fn from_args() -> Self {
        let mut config = Self::default();

        // 1. Scan for custom --env-file in args or look in standard locations
        let args: Vec<String> = std::env::args().collect();
        let mut custom_env_file: Option<PathBuf> = None;
        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--env-file" if i + 1 < args.len() => {
                    custom_env_file = Some(PathBuf::from(&args[i + 1]));
                    i += 2;
                }
                _ => {
                    i += 1;
                }
            }
        }

        Self::load_env_file_hierarchy(&mut config, custom_env_file.as_deref());

        // 2. Overlay process environment variables
        match std::env::var("AGENTROUTER_KEY_CMD") {
            Ok(cmd) if !cmd.trim().is_empty() => {
                config.key_cmd = cmd.trim().to_string();
            }
            _ => {}
        }
        match std::env::var("AGENTROUTER_DEFAULT_MODEL") {
            Ok(model) if !model.trim().is_empty() => {
                config.default_model = model.trim().to_string();
            }
            _ => {}
        }
        match std::env::var("AGENTROUTER_BASE_URL") {
            Ok(url) if !url.trim().is_empty() => {
                config.base_url = url.trim().trim_end_matches('/').to_string();
            }
            _ => {}
        }
        match std::env::var("AGENTROUTER_TIMEOUT") {
            Ok(t) => match t.parse::<u64>() {
                Ok(val) => config.timeout_secs = val,
                Err(_) => {}
            },
            _ => {}
        }

        // 3. Overlay explicit CLI arguments (highest priority)
        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--key-cmd" | "-k" if i + 1 < args.len() => {
                    config.key_cmd = args[i + 1].trim().to_string();
                    i += 2;
                }
                "--default-model" | "-m" if i + 1 < args.len() => {
                    config.default_model = args[i + 1].trim().to_string();
                    i += 2;
                }
                "--base-url" | "-u" if i + 1 < args.len() => {
                    config.base_url = args[i + 1].trim().trim_end_matches('/').to_string();
                    i += 2;
                }
                "--timeout" | "-t" if i + 1 < args.len() => {
                    if let Ok(val) = args[i + 1].parse::<u64>() {
                        config.timeout_secs = val;
                    }
                    i += 2;
                }
                "--env-file" if i + 1 < args.len() => {
                    i += 2; // already handled in step 1
                }
                "--help" | "-h" => {
                    eprintln!("agentrouter-mcp: Model-agnostic MCP server for AgentRouter");
                    eprintln!();
                    eprintln!("Usage: agentrouter-mcp [OPTIONS]");
                    eprintln!();
                    eprintln!("Options:");
                    eprintln!(
                        "  -k, --key-cmd <COMMAND>      Shell command to retrieve API key on-demand"
                    );
                    eprintln!(
                        "                               (default: {})",
                        Self::DEFAULT_KEY_CMD
                    );
                    eprintln!(
                        "  -m, --default-model <MODEL>  Default model when not specified in tool calls"
                    );
                    eprintln!(
                        "                               (default: {})",
                        Self::DEFAULT_FALLBACK_MODEL
                    );
                    eprintln!("  -u, --base-url <URL>        Base URL for AgentRouter API");
                    eprintln!(
                        "                               (default: {})",
                        Self::DEFAULT_BASE_URL
                    );
                    eprintln!(
                        "  -t, --timeout <SECS>        HTTP timeout in seconds (default: {})",
                        Self::DEFAULT_TIMEOUT_SECS
                    );
                    eprintln!("      --env-file <PATH>       Path to .env configuration file");
                    eprintln!("  -h, --help                  Print this help message");
                    std::process::exit(0);
                }
                _ => {
                    i += 1;
                }
            }
        }

        config
    }

    fn load_env_file_hierarchy(config: &mut Self, custom_path: Option<&Path>) {
        match custom_path {
            Some(path) => {
                Self::apply_env_file(path, config);
            }
            None => {
                let candidates = [
                    PathBuf::from(".env"),
                    PathBuf::from("/home/user/projects/agentrouter-mcp/.env"),
                ];

                for path in &candidates {
                    match path.is_file() {
                        true => {
                            Self::apply_env_file(path, config);
                            break;
                        }
                        false => {}
                    }
                }
            }
        }
    }

    fn apply_env_file(path: &Path, config: &mut Self) {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return,
        };

        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let clean_line = trimmed.strip_prefix("export ").unwrap_or(trimmed).trim();
            match clean_line.split_once('=') {
                Some((k, v)) => {
                    let key = k.trim();
                    let val = v.trim().trim_matches('"').trim_matches('\'').trim();
                    match key {
                        "AGENTROUTER_KEY_CMD" | "KEY_CMD" if !val.is_empty() => {
                            config.key_cmd = val.to_string();
                        }
                        "AGENTROUTER_DEFAULT_MODEL" | "DEFAULT_MODEL" if !val.is_empty() => {
                            config.default_model = val.to_string();
                        }
                        "AGENTROUTER_BASE_URL" | "BASE_URL" if !val.is_empty() => {
                            config.base_url = val.trim_end_matches('/').to_string();
                        }
                        "AGENTROUTER_TIMEOUT" | "TIMEOUT" => match val.parse::<u64>() {
                            Ok(t) => config.timeout_secs = t,
                            Err(_) => {}
                        },
                        _ => {}
                    }
                }
                None => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_values() {
        let config = AppConfig::default();
        assert_eq!(config.default_model, AppConfig::DEFAULT_FALLBACK_MODEL);
        assert_eq!(config.base_url, AppConfig::DEFAULT_BASE_URL);
        assert_eq!(config.timeout_secs, AppConfig::DEFAULT_TIMEOUT_SECS);
        assert_eq!(config.key_cmd, AppConfig::DEFAULT_KEY_CMD);
    }
}
