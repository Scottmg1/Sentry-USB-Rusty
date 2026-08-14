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

    // kill_on_drop: when the timeout below fires it drops this future;
    // without the flag the child would keep running detached (a hung
    // `cp --reflink` or fsck could linger forever holding its loop device).
    let result = tokio::time::timeout(timeout, async {
        Command::new(name)
            .args(args)
            .kill_on_drop(true)
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

/// An SSH port rendered into the argument forms `ssh` and `rsync` expect.
///
/// A `None` port means the SSH default, and every accessor then yields no
/// arguments at all, so command lines for the usual port-22 case stay
/// exactly as they were before the port became configurable.
///
/// The rendered strings are owned by the struct so call sites can splice
/// the flags straight into the `&[&str]` that [`run_with_timeout`] takes:
///
/// ```ignore
/// let port = SshPort::new(Some(2222));
/// let mut args = vec!["-avh"];
/// args.extend(port.rsync_args());
/// args.extend_from_slice(&[src, dest]);
/// ```
#[derive(Debug, Clone)]
pub struct SshPort {
    number: Option<String>,
    remote_shell: Option<String>,
}

impl SshPort {
    /// `None` selects the SSH default port.
    pub fn new(port: Option<u16>) -> Self {
        Self {
            number: port.map(|p| p.to_string()),
            remote_shell: port.map(|p| format!("ssh -p {p}")),
        }
    }

    /// Flags for a direct `ssh` or `ssh-keyscan` invocation.
    pub fn ssh_args(&self) -> Vec<&str> {
        match &self.number {
            Some(n) => vec!["-p", n],
            None => Vec::new(),
        }
    }

    /// Flags for `rsync`, which can only reach a custom port through its
    /// remote-shell override. Passing the port to rsync directly would be
    /// read as `-p` (`--perms`), silently changing file permissions instead
    /// of connecting anywhere else.
    pub fn rsync_args(&self) -> Vec<&str> {
        match &self.remote_shell {
            Some(shell) => vec!["-e", shell],
            None => Vec::new(),
        }
    }
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

    #[test]
    fn default_ssh_port_adds_no_arguments() {
        // Every existing port-22 command line must stay byte for byte
        // what it was, so the default renders to nothing at all.
        let port = SshPort::new(None);
        assert!(port.ssh_args().is_empty());
        assert!(port.rsync_args().is_empty());
    }

    #[test]
    fn custom_ssh_port_uses_the_form_each_tool_expects() {
        let port = SshPort::new(Some(23232));
        assert_eq!(port.ssh_args(), ["-p", "23232"]);
        // Never ["-p", "23232"] for rsync: that is --perms.
        assert_eq!(port.rsync_args(), ["-e", "ssh -p 23232"]);
    }
}
