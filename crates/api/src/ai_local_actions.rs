//! Local, confirmation-gated actions offered by Sentry AI.
//!
//! The cloud response carries only a server-owned action ID. This Pi-side
//! handler independently allowlists that ID and owns every config key/value.

use axum::extract::{Path, State};
use axum::{http::StatusCode, Json};

use crate::router::AppState;

const REBOOT_REQUIRED_MARKER: &str = "/run/sentryusb-ai-setting-reboot-required";

fn archive_action_updates(id: &str) -> Option<Vec<(&'static str, &'static str)>> {
    match id {
        "set-archive-sentry-without-recent" => Some(vec![
            ("ARCHIVE_SENTRYCLIPS", "true"),
            ("ARCHIVE_RECENTCLIPS", "false"),
        ]),
        "enable-archive-saved" => Some(vec![("ARCHIVE_SAVEDCLIPS", "true")]),
        "disable-archive-saved" => Some(vec![("ARCHIVE_SAVEDCLIPS", "false")]),
        "enable-archive-sentry" => Some(vec![("ARCHIVE_SENTRYCLIPS", "true")]),
        "disable-archive-sentry" => Some(vec![("ARCHIVE_SENTRYCLIPS", "false")]),
        "enable-archive-recent" => Some(vec![("ARCHIVE_RECENTCLIPS", "true")]),
        "disable-archive-recent" => Some(vec![("ARCHIVE_RECENTCLIPS", "false")]),
        "enable-archive-track-mode" => Some(vec![("ARCHIVE_TRACKMODECLIPS", "true")]),
        "disable-archive-track-mode" => Some(vec![("ARCHIVE_TRACKMODECLIPS", "false")]),
        _ => None,
    }
}

fn apply_archive_action(id: &str, active: &mut sentryusb_config::SetupConfig) -> Option<bool> {
    let updates = archive_action_updates(id)?;
    let mut changed = false;
    for (key, value) in updates {
        changed |= active.get(key).map(String::as_str) != Some(value);
        active.insert(key.to_string(), value.to_string());
    }
    Some(changed)
}

fn action_summary(id: &str, changed: bool, requires_reboot: bool) -> String {
    if !changed {
        return if requires_reboot {
            "That archive setting was already saved. Restart the Pi to load the pending archive settings."
                .to_string()
        } else {
            "That archive setting is already selected; no change was needed.".to_string()
        };
    }
    let change = match id {
        "set-archive-sentry-without-recent" => {
            "Sentry Clips archiving is enabled and Recent Clips archiving is disabled. Saved Clips and Track Mode were left unchanged."
        }
        "enable-archive-saved" => "Saved Clips archiving is enabled.",
        "disable-archive-saved" => "Saved Clips archiving is disabled.",
        "enable-archive-sentry" => "Sentry Clips archiving is enabled.",
        "disable-archive-sentry" => "Sentry Clips archiving is disabled.",
        "enable-archive-recent" => "Recent Clips archiving is enabled.",
        "disable-archive-recent" => "Recent Clips archiving is disabled.",
        "enable-archive-track-mode" => "Track Mode archiving is enabled.",
        "disable-archive-track-mode" => "Track Mode archiving is disabled.",
        _ => "Archive setting saved.",
    };
    format!("{change} Restart the Pi to load the new archive settings.")
}

fn remount_root(read_only: bool) -> anyhow::Result<()> {
    let status = if read_only {
        std::process::Command::new("mount")
            .args(["-o", "remount,ro", "/"])
            .status()?
    } else {
        std::process::Command::new("/root/bin/remountfs_rw").status()?
    };
    if !status.success() {
        anyhow::bail!(
            "could not remount the root filesystem {}",
            if read_only {
                "read-only"
            } else {
                "read-write"
            }
        );
    }
    Ok(())
}

/// POST /api/support/local-actions/{id}
pub async fn apply_local_action(
    State(_s): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    if archive_action_updates(&id).is_none() {
        return crate::json_error(StatusCode::NOT_FOUND, "unsupported local support action");
    }

    let action_id = id.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<(bool, bool)> {
        let config_path = sentryusb_config::find_config_path();
        let (mut active, _) = sentryusb_config::parse_file(config_path)?;
        let changed = apply_archive_action(&action_id, &mut active)
            .ok_or_else(|| anyhow::anyhow!("unsupported local support action"))?;
        if changed {
            remount_root(false)?;
            let write_result = sentryusb_config::write_file(config_path, &active);
            // Restore read-only-root protection immediately. The separate reboot
            // button reloads the archive process from the saved config later.
            let remount_result = remount_root(true);
            match (write_result, remount_result) {
                (Err(write_error), Err(remount_error)) => anyhow::bail!(
                    "config write failed ({write_error}); read-only protection also could not be restored ({remount_error})"
                ),
                (Err(write_error), Ok(())) => return Err(write_error),
                (Ok(()), Err(remount_error)) => return Err(remount_error),
                (Ok(()), Ok(())) => {}
            }
            if let Err(error) = std::fs::write(REBOOT_REQUIRED_MARKER, &action_id) {
                tracing::warn!("could not write AI setting reboot marker: {error}");
            }
        }
        Ok((
            changed,
            changed || std::path::Path::new(REBOOT_REQUIRED_MARKER).exists(),
        ))
    })
    .await;

    let (changed, requires_reboot) = match result {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(error)) => {
            return crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("could not update archive settings: {error}"),
            );
        }
        Err(error) => {
            return crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("archive setting task failed: {error}"),
            );
        }
    };

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "success": true,
            "actionId": id,
            "changed": changed,
            "requiresReboot": requires_reboot,
            "summary": action_summary(&id, changed, requires_reboot),
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_action_changes_only_sentry_and_recent() {
        let mut config = sentryusb_config::SetupConfig::from([
            ("ARCHIVE_SAVEDCLIPS".to_string(), "true".to_string()),
            ("ARCHIVE_SENTRYCLIPS".to_string(), "false".to_string()),
            ("ARCHIVE_RECENTCLIPS".to_string(), "true".to_string()),
            ("ARCHIVE_TRACKMODECLIPS".to_string(), "false".to_string()),
        ]);
        assert_eq!(
            apply_archive_action("set-archive-sentry-without-recent", &mut config),
            Some(true)
        );
        assert_eq!(
            config.get("ARCHIVE_SAVEDCLIPS").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            config.get("ARCHIVE_SENTRYCLIPS").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            config.get("ARCHIVE_RECENTCLIPS").map(String::as_str),
            Some("false")
        );
        assert_eq!(
            config.get("ARCHIVE_TRACKMODECLIPS").map(String::as_str),
            Some("false")
        );
        assert_eq!(
            apply_archive_action("set-archive-sentry-without-recent", &mut config),
            Some(false)
        );
        assert_eq!(
            apply_archive_action("enable-archive-recent", &mut config),
            Some(true)
        );
        assert_eq!(
            config.get("ARCHIVE_RECENTCLIPS").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            apply_archive_action("disable-archive-track-mode", &mut config),
            Some(false)
        );
        assert_eq!(apply_archive_action("unsupported", &mut config), None);
        assert!(action_summary("enable-archive-recent", false, false).contains("no change"));
        assert!(action_summary("enable-archive-recent", false, true).contains("Restart the Pi"));
    }
}
