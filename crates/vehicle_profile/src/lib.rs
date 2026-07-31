//! Vehicle profiles: how a car brand records dashcam footage onto the USB
//! drive (recording path, filename format, camera set, viewer grid). SentryUSB
//! is always Tesla; the profile exists so the `/api/profile` endpoint can tell
//! a client (SC) which ecosystem it's talking to, using the same schema the
//! Dash-USB sibling serves.
//!
//! Profiles are TOML under `profiles/`, embedded at compile time. Add a brand
//! by adding a file and listing it in [`EMBEDDED`]. `VEHICLE_PROFILE` in the
//! conf selects one by id; `SENTRYUSB_PROFILE_PATH` overrides with an on-disk
//! TOML for dev/bench. Anything invalid falls back to the default with a log.

use std::sync::OnceLock;

use anyhow::{Context, Result};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

pub const DEFAULT_PROFILE_ID: &str = "tesla";

/// Compiled-in profiles, keyed by `profile.id`.
const EMBEDDED: &[(&str, &str)] = &[("tesla", include_str!("../../../profiles/tesla.toml"))];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub profile: Meta,
    pub recording: Recording,
    pub cameras: Vec<Camera>,
    pub viewer: Viewer,
    pub virtual_drive: VirtualDrive,
    pub snapshots: Snapshots,
    pub features: Features,
    #[serde(skip)]
    compiled_regex: OnceLock<regex::Regex>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    pub display_name: String,
    pub brand: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recording {
    /// Recording root inside the virtual drive, relative, no leading slash.
    pub root: String,
    /// Must expose named captures `camera`, `y`, `mo`, `d`, `h`, `mi`, `s`.
    pub filename_regex: String,
    pub segment_seconds: u32,
    pub rolling_window_minutes: u32,
    pub approx_bytes_per_camera_segment: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Camera {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub optional: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Viewer {
    /// Row-major camera grid; empty string = empty cell.
    pub grid: Vec<Vec<String>>,
}

/// Not consumed by SentryUSB (it manages its virtual drive via setup config);
/// present only for schema parity with the Dash-USB profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualDrive {
    pub default_size: String,
    pub min_size: String,
    pub min_free_bytes: u64,
    pub filesystem: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshots {
    pub default_interval_secs: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Features {
    pub event_folders: bool,
    pub archive_everything_default: bool,
    pub nofua: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipInfo {
    /// Camera id exactly as captured (e.g. "front").
    pub camera: String,
    pub timestamp: chrono::NaiveDateTime,
}

impl ClipInfo {
    /// Date bucket for the recordings tree ("2026-07-17").
    pub fn date_str(&self) -> String {
        self.timestamp.format("%Y-%m-%d").to_string()
    }
}

impl Profile {
    pub fn from_toml(s: &str) -> Result<Self> {
        let p: Profile = toml::from_str(s).context("parsing vehicle profile TOML")?;
        p.validate()?;
        Ok(p)
    }

    fn validate(&self) -> Result<()> {
        let re = regex::Regex::new(&self.recording.filename_regex)
            .context("filename_regex does not compile")?;
        for cap in ["camera", "y", "mo", "d", "h", "mi", "s"] {
            anyhow::ensure!(
                re.capture_names().flatten().any(|n| n == cap),
                "filename_regex missing named capture `{cap}`"
            );
        }
        for row in &self.viewer.grid {
            for cell in row.iter().filter(|c| !c.is_empty()) {
                anyhow::ensure!(
                    self.cameras.iter().any(|c| &c.id == cell),
                    "viewer.grid references unknown camera `{cell}`"
                );
            }
        }
        anyhow::ensure!(
            self.recording.root.starts_with(|c: char| c.is_ascii_alphanumeric()),
            "recording.root must be a relative path"
        );
        Ok(())
    }

    pub fn embedded(id: &str) -> Option<Result<Self>> {
        EMBEDDED
            .iter()
            .find(|(pid, _)| *pid == id)
            .map(|(_, s)| Self::from_toml(s))
    }

    /// The process-wide active profile.
    ///
    /// Resolution: `SENTRYUSB_PROFILE_PATH` env (dev/bench override), then the
    /// `VEHICLE_PROFILE` conf key, then the default. Every failure path logs
    /// and falls back to the embedded default, which unit tests guarantee
    /// parses.
    pub fn active() -> &'static Profile {
        static ACTIVE: OnceLock<Profile> = OnceLock::new();
        ACTIVE.get_or_init(|| {
            if let Ok(path) = std::env::var("SENTRYUSB_PROFILE_PATH") {
                match std::fs::read_to_string(&path)
                    .map_err(anyhow::Error::from)
                    .and_then(|s| Self::from_toml(&s))
                {
                    Ok(p) => {
                        tracing::info!("vehicle profile: {} (from {})", p.profile.id, path);
                        return p;
                    }
                    Err(e) => {
                        tracing::warn!("SENTRYUSB_PROFILE_PATH={path} unusable ({e:#}); falling back")
                    }
                }
            }
            let (active, _) = sentryusb_config::parse_file(sentryusb_config::find_config_path())
                .unwrap_or_default();
            let id = active
                .get("VEHICLE_PROFILE")
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .unwrap_or(DEFAULT_PROFILE_ID)
                .to_string();
            match Self::embedded(&id) {
                Some(Ok(p)) => {
                    tracing::info!("vehicle profile: {}", p.profile.id);
                    return p;
                }
                Some(Err(e)) => tracing::warn!("embedded profile `{id}` invalid ({e:#})"),
                None => tracing::warn!("VEHICLE_PROFILE=`{id}` unknown; using default"),
            }
            Self::embedded(DEFAULT_PROFILE_ID)
                .expect("default profile is embedded")
                .expect("default profile parses (covered by unit test)")
        })
    }

    pub fn clip_regex(&self) -> &regex::Regex {
        self.compiled_regex.get_or_init(|| {
            // validate() already proved this compiles.
            regex::Regex::new(&self.recording.filename_regex).expect("validated regex")
        })
    }

    /// Parse a bare clip filename (no path components) into camera + timestamp.
    pub fn parse_clip_filename(&self, name: &str) -> Option<ClipInfo> {
        let caps = self.clip_regex().captures(name)?;
        let num = |k: &str| caps.name(k).and_then(|m| m.as_str().parse::<u32>().ok());
        let date = NaiveDate::from_ymd_opt(num("y")? as i32, num("mo")?, num("d")?)?;
        let ts = date.and_hms_opt(num("h")?, num("mi")?, num("s")?)?;
        Some(ClipInfo {
            camera: caps.name("camera")?.as_str().to_string(),
            timestamp: ts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tesla() -> Profile {
        Profile::embedded(DEFAULT_PROFILE_ID).unwrap().unwrap()
    }

    #[test]
    fn default_profile_parses_and_validates() {
        let p = tesla();
        assert_eq!(p.profile.id, "tesla");
        assert_eq!(p.profile.brand, "Tesla");
        assert_eq!(p.recording.segment_seconds, 60);
        // front, back, left/right repeater always present; pillars optional.
        assert!(p.cameras.iter().any(|c| c.id == "front" && !c.optional));
        assert!(p.cameras.iter().find(|c| c.id == "left_pillar").unwrap().optional);
    }

    #[test]
    fn parses_real_tesla_filenames() {
        let p = tesla();
        let info = p.parse_clip_filename("2026-07-17_19-34-53-front.mp4").unwrap();
        assert_eq!(info.camera, "front");
        assert_eq!(info.date_str(), "2026-07-17");
        for name in [
            "2026-07-17_19-04-53-back.mp4",
            "2026-07-17_19-39-53-left_repeater.mp4",
            "2026-07-17_19-04-19-right_repeater.mp4",
            "2026-07-17_19-04-19-left_pillar.mp4",
            "2026-07-17_19-04-19-right_pillar.mp4",
        ] {
            assert!(p.parse_clip_filename(name).is_some(), "{name} must parse");
        }
    }

    #[test]
    fn rejects_foreign_and_malformed_names() {
        let p = tesla();
        for name in [
            "FRONT_2026_07_17_T_19_34_53.mp4",   // GM format
            "2026-07-17_19-34-53-FRONT.mp4",     // uppercase camera
            "2026-07-17_19-34-53-selfie.mp4",    // unknown camera
            "2026-13-40_25-61-61-front.mp4",     // impossible date/time
            "2026-07-17_19-34-53-front.mp4.tmp",
            "thumbnail.jpg",
        ] {
            assert!(p.parse_clip_filename(name).is_none(), "{name} must be rejected");
        }
    }
}
