use std::process::Command;
use thiserror::Error;

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

/// Retrieve the AgentRouter API key on-demand by executing the configured shell command.
///
/// This delegates secret retrieval to an external command (e.g. secret-tool, pass, rbw),
/// avoiding any hardcoding in the binary and keeping plaintext credentials out of disk files.
pub fn fetch_token_from_cmd(key_cmd: &str) -> Result<String, AuthError> {
    let trimmed_cmd = key_cmd.trim();
    match trimmed_cmd.is_empty() {
        true => Err(AuthError::EmptyCommand),
        false => {
            let output = Command::new("sh").args(["-c", trimmed_cmd]).output()?;

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
