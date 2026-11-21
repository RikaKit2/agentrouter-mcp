use thiserror::Error;
use tokio::process::Command;

/// Domain errors encountered during credential retrieval.
#[derive(Debug, Error)]
pub enum AuthError {
    #[error("key retrieval command is not configured (empty)")]
    EmptyCommand,

    #[error("failed to execute key retrieval command via 'sh -c': {0}")]
    ProcessSpawn(#[from] std::io::Error),

    #[error("key retrieval command '{cmd}' exited with status {status}: {stderr}")]
    LookupFailed {
        cmd: String,
        status: i32,
        stderr: String,
    },

    #[error("key retrieval command '{cmd}' returned empty secret")]
    EmptySecret { cmd: String },

    #[error("key output contains invalid UTF-8 bytes: {0}")]
    InvalidUtf8(#[from] std::string::FromUtf8Error),
}

/// Retrieve the AgentRouter API key on-demand by executing the configured shell command asynchronously.
///
/// This delegates secret retrieval to an external command (e.g. secret-tool, pass, rbw),
/// avoiding any hardcoding in the binary and keeping plaintext credentials out of disk files.
pub async fn fetch_token_from_cmd(key_cmd: &str) -> Result<String, AuthError> {
    let trimmed_cmd = key_cmd.trim();
    match trimmed_cmd.is_empty() {
        true => Err(AuthError::EmptyCommand),
        false => {
            let output = Command::new("sh")
                .args(["-c", trimmed_cmd])
                .output()
                .await?;

            match output.status.success() {
                true => {
                    let secret = String::from_utf8(output.stdout)?.trim().to_string();
                    match secret.is_empty() {
                        true => Err(AuthError::EmptySecret {
                            cmd: trimmed_cmd.to_string(),
                        }),
                        false => Ok(secret),
                    }
                }
                false => {
                    let code = output.status.code().unwrap_or(-1);
                    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                    Err(AuthError::LookupFailed {
                        cmd: trimmed_cmd.to_string(),
                        status: code,
                        stderr,
                    })
                }
            }
        }
    }
}

/// Resolve the active API token, preferring an explicit API key if provided,
/// or falling back to on-demand execution of `key_cmd`.
pub async fn resolve_token(api_key: Option<&str>, key_cmd: &str) -> Result<String, AuthError> {
    if let Some(key) = api_key {
        let trimmed = key.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    fetch_token_from_cmd(key_cmd).await
}

/// Safely mask a token for status messages without leaking secrets.
pub fn mask_token(token: &str) -> String {
    let len = token.chars().count();
    match len {
        0..=8 => "********".to_string(),
        _ => {
            let prefix: String = token.chars().take(4).collect();
            let suffix: String = token.chars().skip(len.saturating_sub(4)).collect();
            format!("{prefix}...{suffix} ({len} chars)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_empty_command_error() {
        let res = fetch_token_from_cmd("").await;
        assert!(matches!(res, Err(AuthError::EmptyCommand)));
    }

    #[tokio::test]
    async fn test_whitespace_command_error() {
        let res = fetch_token_from_cmd("   ").await;
        assert!(matches!(res, Err(AuthError::EmptyCommand)));
    }

    #[tokio::test]
    async fn test_successful_echo_token() {
        let res = fetch_token_from_cmd("echo 'test-token-1234'").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "test-token-1234");
    }

    #[tokio::test]
    async fn test_resolve_token_precedence() {
        let res = resolve_token(Some("direct-token"), "invalid-cmd").await;
        assert_eq!(res.unwrap(), "direct-token");

        let res_cmd = resolve_token(None, "echo 'cmd-token'").await;
        assert_eq!(res_cmd.unwrap(), "cmd-token");
    }

    #[test]
    fn test_mask_token() {
        assert_eq!(mask_token("short"), "********");
        assert_eq!(mask_token("12345678901234"), "1234...1234 (14 chars)");
    }
}
