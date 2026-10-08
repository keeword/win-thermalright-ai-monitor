use crate::{APP_NAME, agents::AgentKind};
use anyhow::{Context, Result};
use chrono::{Local, Timelike};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub left: AgentKind,
    pub right: AgentKind,
    pub brightness: u8,
    pub rotate: bool,
    pub follow_system_display: bool,
    pub night_enabled: bool,
    pub night_start: u16,
    pub night_end: u16,
    pub font: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            left: AgentKind::Claude,
            right: AgentKind::Codex,
            brightness: 1,
            rotate: true,
            follow_system_display: true,
            night_enabled: false,
            night_start: 1110,
            night_end: 540,
            font: None,
        }
    }
}

impl Settings {
    pub fn path() -> PathBuf {
        directories::ProjectDirs::from("", "", APP_NAME)
            .map(|p| p.config_dir().join("settings.json"))
            .unwrap_or_else(|| PathBuf::from("settings.json"))
    }
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let path = path.map(Path::to_path_buf).unwrap_or_else(Self::path);
        if !path.exists() {
            return Ok(Self::default());
        }
        let settings: Self = serde_json::from_slice(&std::fs::read(&path)?)
            .with_context(|| format!("Invalid settings: {}", path.display()))?;
        anyhow::ensure!(
            (1..=10).contains(&settings.brightness),
            "brightness must be 1..10"
        );
        anyhow::ensure!(
            settings.night_start < 1440 && settings.night_end < 1440,
            "night times must be minutes in 0..1440"
        );
        Ok(settings)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
    pub fn is_night(&self) -> bool {
        let now = Local::now();
        self.night_enabled
            && in_window(
                (now.hour() * 60 + now.minute()) as u16,
                self.night_start,
                self.night_end,
            )
    }
}

fn in_window(minute: u16, start: u16, end: u16) -> bool {
    if start < end {
        minute >= start && minute < end
    } else if start > end {
        minute >= start || minute < end
    } else {
        false
    }
}

#[test]
fn night_window_crosses_midnight() {
    assert!(in_window(23 * 60, 1110, 540));
    assert!(in_window(8 * 60, 1110, 540));
    assert!(!in_window(540, 1110, 540));
    assert!(!in_window(720, 1110, 540));
    assert!(!in_window(720, 720, 720));
}

#[test]
fn system_display_sync_defaults_on_and_persists_opt_out() {
    let old_config: Settings = serde_json::from_str(r#"{"brightness": 2}"#).unwrap();
    assert!(old_config.follow_system_display);
    let settings = Settings {
        follow_system_display: false,
        ..Default::default()
    };
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("settings.json");
    settings.save(&path).unwrap();
    assert!(!Settings::load(Some(&path)).unwrap().follow_system_display);
}
