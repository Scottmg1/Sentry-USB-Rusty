//! AWS SNS — uses subprocess (SigV4 signing is complex).

use anyhow::{bail, Result};

pub async fn send(topic_arn: &str, title: &str, message: &str) -> Result<()> {
    match sentryusb_shell::run_with_timeout(
        std::time::Duration::from_secs(30),
        "python3",
        &["/root/bin/send_sns.py", "-t", topic_arn, "-s", title, "-m", message],
    ).await {
        Ok(_) => Ok(()),
        Err(e) => bail!("SNS python script failed: {}", e),
    }
}
