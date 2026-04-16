use std::time::Duration;

use anyhow::{bail, Result};
use tokio::process::Command;
use tracing::debug;

/// Default command timeout.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Executes a command and returns its stdout output with a 30-second timeout.
pub async fn run(name: &str, args: &[&str]) -> Result<String> {
    run_with_timeout(DEFAULT_TIMEOUT, name, args).await
}

/// Executes a command with a custom timeout and returns its stdout output.
pub async fn run_with_timeout(timeout: Duration, name: &str, args: &[&str]) -> Result<String> {
    debug!(cmd = name, ?args, "executing command");

    let result = tokio::time::timeout(timeout, async {
        Command::new(name)
            .args(args)
            .output()
            .await
    })
    .await;

    match result {
        Err(_) => bail!("command timed out after {:?}", timeout),
        Ok(Err(e)) => bail!("failed to execute command: {}", e),
        Ok(Ok(output)) => {
            if output.status.success() {
                Ok(String::from_utf8_lossy(&output.stdout).into_owned())
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!(
                    "command failed (exit {}): {}",
                    output.status.code().unwrap_or(-1),
                    clean_stderr(&stderr)
                )
            }
        }
    }
}

/// Executes a command and returns both stdout and stderr (for cases where
/// stderr contains useful info even on success).
pub async fn run_with_stderr(name: &str, args: &[&str]) -> Result<(String, String)> {
    let result = tokio::time::timeout(DEFAULT_TIMEOUT, async {
        Command::new(name).args(args).output().await
    })
    .await;

    match result {
        Err(_) => bail!("command timed out after {:?}", DEFAULT_TIMEOUT),
        Ok(Err(e)) => bail!("failed to execute command: {}", e),
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            if output.status.success() {
                Ok((stdout, stderr))
            } else {
                bail!(
                    "command failed (exit {}): {}",
                    output.status.code().unwrap_or(-1),
                    clean_stderr(&stderr)
                )
            }
        }
    }
}

/// Strips noisy tool output (e.g. curl progress) from an error message.
pub fn clean_stderr(msg: &str) -> String {
    let mut result = String::with_capacity(msg.len());
    for line in msg.lines() {
        let trimmed = line.trim();
        // Skip curl progress meter lines
        if trimmed.starts_with("% Total") || is_curl_progress_line(trimmed) {
            continue;
        }
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(line);
    }
    // Collapse multiple blank lines
    while result.contains("\n\n\n") {
        result = result.replace("\n\n\n", "\n\n");
    }
    result.trim().to_string()
}

/// Checks if a line looks like a curl progress meter data line.
fn is_curl_progress_line(line: &str) -> bool {
    // Curl progress lines are like: "  0  1234    0  0    0     0      0      0 --:--:-- --:--:-- --:--:--     0"
    let parts: Vec<&str> = line.split_whitespace().collect();
    parts.len() >= 6 && parts.iter().take(6).all(|p| p.parse::<u64>().is_ok() || *p == "0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_stderr_removes_curl_progress() {
        let input = "% Total    % Received\n  0  1234    0  0    0     0      0      0\nActual error message";
        let cleaned = clean_stderr(input);
        assert_eq!(cleaned, "Actual error message");
    }

    #[test]
    fn test_clean_stderr_preserves_real_errors() {
        let input = "Error: something went wrong";
        assert_eq!(clean_stderr(input), "Error: something went wrong");
    }
}
