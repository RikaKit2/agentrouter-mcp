use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Supported upstream API routing modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApiMode {
    /// Automatically detects whether the model needs /v1/responses or /v1/chat/completions (with caching)
    #[default]
    Auto,
    /// Force using OpenAI Responses API (/v1/responses) directly without prior chat/completions probe
    Responses,
    /// Force using standard OpenAI Chat Completions API (/v1/chat/completions)
    Completions,
}

impl FromStr for ApiMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().trim() {
            "auto" => Ok(Self::Auto),
            "responses" | "response" | "v1/responses" => Ok(Self::Responses),
            "completions" | "chat" | "chat_completions" => Ok(Self::Completions),
            other => Err(format!(
                "Invalid api-mode '{other}'. Supported values: auto, responses, completions"
            )),
        }
    }
}

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

    /// Optional static API key (bypasses executing `key_cmd` if set)
    pub api_key: Option<String>,

    /// API endpoint routing mode (auto, responses, completions)
    pub api_mode: ApiMode,

    /// User-Agent header sent to AgentRouter to satisfy Cloudflare WAF validation
    pub user_agent: String,

    /// Directory where persistent conversation sessions are stored as JSON files
    pub sessions_dir: PathBuf,

    /// Maximum number of messages retained per session history (>= 1)
    pub max_history_messages: usize,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            default_model: Self::DEFAULT_FALLBACK_MODEL.to_string(),
            base_url: Self::DEFAULT_BASE_URL.to_string(),
            timeout_secs: Self::DEFAULT_TIMEOUT_SECS,
            key_cmd: Self::DEFAULT_KEY_CMD.to_string(),
            api_key: None,
            api_mode: ApiMode::default(),
            user_agent: Self::DEFAULT_USER_AGENT.to_string(),
            sessions_dir: Self::default_sessions_dir(),
            max_history_messages: Self::DEFAULT_MAX_HISTORY_MESSAGES,
        }
    }
}

impl AppConfig {
    pub const DEFAULT_BASE_URL: &'static str = "https://agentrouter.org/v1";
    pub const DEFAULT_FALLBACK_MODEL: &'static str = "gpt-6.1-sol";
    pub const DEFAULT_TIMEOUT_SECS: u64 = 300;
    pub const DEFAULT_KEY_CMD: &'static str = "secret-tool lookup service agentrouter";
    pub const DEFAULT_USER_AGENT: &'static str = "opencode/1.18.25";
    pub const DEFAULT_MAX_HISTORY_MESSAGES: usize = 30;

    /// Resolve the default directory for session persistence using XDG / HOME / temp_dir fallback.
    pub fn default_sessions_dir() -> PathBuf {
        let xdg_data_home = std::env::var("XDG_DATA_HOME").ok();
        let home_dir = std::env::var("HOME").ok();
        Self::resolve_default_sessions_dir(xdg_data_home.as_deref(), home_dir.as_deref())
    }

    fn resolve_default_sessions_dir(
        xdg_data_home: Option<&str>,
        home_dir: Option<&str>,
    ) -> PathBuf {
        match xdg_data_home {
            Some(xdg) if !xdg.trim().is_empty() => {
                PathBuf::from(xdg.trim()).join("agentrouter-mcp/sessions")
            }
            _ => match home_dir {
                Some(home) if !home.trim().is_empty() => {
                    PathBuf::from(home.trim()).join(".local/share/agentrouter-mcp/sessions")
                }
                _ => std::env::temp_dir().join("agentrouter-mcp/sessions"),
            },
        }
    }

    fn parse_max_history(raw: &str) -> Result<usize, String> {
        match raw.trim().parse::<usize>() {
            Ok(0) => Err("value must be >= 1".to_string()),
            Ok(val) => Ok(val),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Parse configuration from CLI arguments, environment variables, and optional .env files.
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
        match std::env::var("AGENTROUTER_API_KEY") {
            Ok(key) if !key.trim().is_empty() => {
                config.api_key = Some(key.trim().to_string());
            }
            _ => {}
        }
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
            Ok(t) if !t.trim().is_empty() => match t.parse::<u64>() {
                Ok(val) => config.timeout_secs = val,
                Err(e) => eprintln!("Warning: invalid AGENTROUTER_TIMEOUT '{t}': {e}"),
            },
            _ => {}
        }
        match std::env::var("AGENTROUTER_API_MODE") {
            Ok(mode_str) if !mode_str.trim().is_empty() => match ApiMode::from_str(&mode_str) {
                Ok(m) => config.api_mode = m,
                Err(e) => eprintln!("Warning: invalid AGENTROUTER_API_MODE '{mode_str}': {e}"),
            },
            _ => {}
        }
        match std::env::var("AGENTROUTER_USER_AGENT") {
            Ok(ua) if !ua.trim().is_empty() => {
                config.user_agent = ua.trim().to_string();
            }
            _ => {}
        }
        match std::env::var("AGENTROUTER_SESSIONS_DIR") {
            Ok(dir) if !dir.trim().is_empty() => {
                config.sessions_dir = PathBuf::from(dir.trim());
            }
            _ => {}
        }
        match std::env::var("AGENTROUTER_MAX_HISTORY") {
            Ok(raw) if !raw.trim().is_empty() => match Self::parse_max_history(&raw) {
                Ok(val) => config.max_history_messages = val,
                Err(e) => eprintln!("Warning: invalid AGENTROUTER_MAX_HISTORY '{raw}': {e}"),
            },
            _ => {}
        }

        // 3. Overlay explicit CLI arguments (highest priority)
        match Self::apply_cli_args(&mut config, &args) {
            Ok(true) => config,
            Ok(false) => {
                Self::print_help();
                std::process::exit(0);
            }
            Err(msg) => {
                eprintln!("Error: {msg}");
                std::process::exit(1);
            }
        }
    }

    fn apply_cli_args(config: &mut Self, args: &[String]) -> Result<bool, String> {
        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--api-key" if i + 1 < args.len() => {
                    config.api_key = Some(args[i + 1].trim().to_string());
                    i += 2;
                }
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
                    match args[i + 1].parse::<u64>() {
                        Ok(val) => config.timeout_secs = val,
                        Err(e) => {
                            return Err(format!(
                                "invalid value for --timeout '{}': {e}",
                                args[i + 1]
                            ));
                        }
                    }
                    i += 2;
                }
                "--api-mode" | "-a" if i + 1 < args.len() => {
                    config.api_mode = ApiMode::from_str(&args[i + 1])?;
                    i += 2;
                }
                "--user-agent" if i + 1 < args.len() => {
                    config.user_agent = args[i + 1].trim().to_string();
                    i += 2;
                }
                "--sessions-dir" if i + 1 < args.len() => {
                    let trimmed = args[i + 1].trim();
                    match trimmed.is_empty() {
                        true => {
                            return Err("invalid value for --sessions-dir: path cannot be empty"
                                .to_string());
                        }
                        false => config.sessions_dir = PathBuf::from(trimmed),
                    }
                    i += 2;
                }
                "--max-history" if i + 1 < args.len() => {
                    match Self::parse_max_history(&args[i + 1]) {
                        Ok(val) => config.max_history_messages = val,
                        Err(e) => {
                            return Err(format!(
                                "invalid value for --max-history '{}': {e}",
                                args[i + 1]
                            ));
                        }
                    }
                    i += 2;
                }
                "--env-file" if i + 1 < args.len() => {
                    i += 2; // already handled in step 1
                }
                "--help" | "-h" => {
                    return Ok(false);
                }
                _ => {
                    i += 1;
                }
            }
        }
        Ok(true)
    }

    fn print_help() {
        eprintln!("agentrouter-mcp: Model-agnostic MCP server for AgentRouter");
        eprintln!();
        eprintln!("Usage: agentrouter-mcp [OPTIONS]");
        eprintln!();
        eprintln!("Options:");
        eprintln!("      --api-key <KEY>         Static API key (overrides --key-cmd)");
        eprintln!("  -k, --key-cmd <COMMAND>      Shell command to retrieve API key on-demand");
        eprintln!(
            "                               (default: {})",
            Self::DEFAULT_KEY_CMD
        );
        eprintln!("  -m, --default-model <MODEL>  Default model when not specified in tool calls");
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
        eprintln!(
            "  -a, --api-mode <MODE>       API routing mode: auto (default), responses, completions"
        );
        eprintln!(
            "      --user-agent <UA>       User-Agent header for Cloudflare WAF (default: {})",
            Self::DEFAULT_USER_AGENT
        );
        eprintln!("      --sessions-dir <PATH>   Directory for persistent session JSON files");
        eprintln!(
            "                               (default: $XDG_DATA_HOME/agentrouter-mcp/sessions)"
        );
        eprintln!(
            "      --max-history <N>       Maximum retained messages per session, >= 1 (default: {})",
            Self::DEFAULT_MAX_HISTORY_MESSAGES
        );
        eprintln!("      --env-file <PATH>       Path to .env configuration file");
        eprintln!("  -h, --help                  Print this help message");
    }

    fn load_env_file_hierarchy(config: &mut Self, custom_path: Option<&Path>) {
        match custom_path {
            Some(path) => {
                Self::apply_env_file(path, config);
            }
            None => {
                let mut candidates = vec![PathBuf::from(".env")];
                match std::env::var("XDG_CONFIG_HOME") {
                    Ok(xdg) if !xdg.trim().is_empty() => {
                        candidates.push(PathBuf::from(xdg).join("agentrouter-mcp/.env"));
                    }
                    _ => match std::env::var("HOME") {
                        Ok(home) if !home.trim().is_empty() => {
                            candidates
                                .push(PathBuf::from(home).join(".config/agentrouter-mcp/.env"));
                        }
                        _ => {}
                    },
                }

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
                        "AGENTROUTER_API_KEY" | "API_KEY" if !val.is_empty() => {
                            config.api_key = Some(val.to_string());
                        }
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
                            Err(e) => eprintln!("Warning: invalid TIMEOUT in .env '{val}': {e}"),
                        },
                        "AGENTROUTER_API_MODE" | "API_MODE" => match ApiMode::from_str(val) {
                            Ok(m) => config.api_mode = m,
                            Err(e) => eprintln!("Warning: invalid API_MODE in .env '{val}': {e}"),
                        },
                        "AGENTROUTER_USER_AGENT" | "USER_AGENT" if !val.is_empty() => {
                            config.user_agent = val.to_string();
                        }
                        "AGENTROUTER_SESSIONS_DIR" | "SESSIONS_DIR" if !val.is_empty() => {
                            config.sessions_dir = PathBuf::from(val);
                        }
                        "AGENTROUTER_MAX_HISTORY" | "MAX_HISTORY" => {
                            match Self::parse_max_history(val) {
                                Ok(n) => config.max_history_messages = n,
                                Err(e) => {
                                    eprintln!("Warning: invalid MAX_HISTORY in .env '{val}': {e}")
                                }
                            }
                        }
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
        assert_eq!(config.api_key, None);
        assert_eq!(config.api_mode, ApiMode::Auto);
        assert_eq!(config.user_agent, AppConfig::DEFAULT_USER_AGENT);
        assert_eq!(
            config.max_history_messages,
            AppConfig::DEFAULT_MAX_HISTORY_MESSAGES
        );
        assert!(config.sessions_dir.ends_with("agentrouter-mcp/sessions"));
    }

    #[test]
    fn test_resolve_default_sessions_dir_fallbacks() {
        assert_eq!(
            AppConfig::resolve_default_sessions_dir(Some("/custom/xdg"), Some("/home/user")),
            PathBuf::from("/custom/xdg/agentrouter-mcp/sessions")
        );
        assert_eq!(
            AppConfig::resolve_default_sessions_dir(Some("  "), Some("/home/user")),
            PathBuf::from("/home/user/.local/share/agentrouter-mcp/sessions")
        );
        assert_eq!(
            AppConfig::resolve_default_sessions_dir(None, Some("/home/user")),
            PathBuf::from("/home/user/.local/share/agentrouter-mcp/sessions")
        );
        assert_eq!(
            AppConfig::resolve_default_sessions_dir(None, None),
            std::env::temp_dir().join("agentrouter-mcp/sessions")
        );
    }

    #[test]
    fn test_parse_max_history_validation() {
        assert_eq!(AppConfig::parse_max_history("1"), Ok(1));
        assert_eq!(AppConfig::parse_max_history(" 42 "), Ok(42));
        assert!(AppConfig::parse_max_history("0").is_err());
        assert!(AppConfig::parse_max_history("-5").is_err());
        assert!(AppConfig::parse_max_history("abc").is_err());
    }

    #[test]
    fn test_apply_env_file_sessions_dir_and_max_history() {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "agentrouter-config-test-{}-{}",
            std::process::id(),
            "env"
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let env_path = dir.join(".env");
        std::fs::write(&env_path, "SESSIONS_DIR=/tmp/my-sessions\nMAX_HISTORY=50\n")
            .expect("write env");

        let mut config = AppConfig::default();
        AppConfig::apply_env_file(&env_path, &mut config);
        assert_eq!(config.sessions_dir, PathBuf::from("/tmp/my-sessions"));
        assert_eq!(config.max_history_messages, 50);

        // Invalid MAX_HISTORY=0 keeps existing value
        std::fs::write(
            &env_path,
            "AGENTROUTER_SESSIONS_DIR=/tmp/other\nAGENTROUTER_MAX_HISTORY=0\n",
        )
        .expect("write env");
        AppConfig::apply_env_file(&env_path, &mut config);
        assert_eq!(config.sessions_dir, PathBuf::from("/tmp/other"));
        assert_eq!(config.max_history_messages, 50);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_apply_cli_args_sessions_dir_and_max_history() {
        let mut config = AppConfig::default();
        let args = vec![
            "agentrouter-mcp".to_string(),
            "--sessions-dir".to_string(),
            "/var/tmp/ar-sessions".to_string(),
            "--max-history".to_string(),
            "12".to_string(),
        ];
        assert_eq!(AppConfig::apply_cli_args(&mut config, &args), Ok(true));
        assert_eq!(config.sessions_dir, PathBuf::from("/var/tmp/ar-sessions"));
        assert_eq!(config.max_history_messages, 12);

        let bad_history = vec![
            "agentrouter-mcp".to_string(),
            "--max-history".to_string(),
            "0".to_string(),
        ];
        assert!(AppConfig::apply_cli_args(&mut config, &bad_history).is_err());

        let bad_dir = vec![
            "agentrouter-mcp".to_string(),
            "--sessions-dir".to_string(),
            "   ".to_string(),
        ];
        assert!(AppConfig::apply_cli_args(&mut config, &bad_dir).is_err());
    }

    #[test]
    fn test_api_mode_parsing() {
        assert_eq!(ApiMode::from_str("auto").unwrap(), ApiMode::Auto);
        assert_eq!(ApiMode::from_str("responses").unwrap(), ApiMode::Responses);
        assert_eq!(
            ApiMode::from_str("completions").unwrap(),
            ApiMode::Completions
        );
    }
}
