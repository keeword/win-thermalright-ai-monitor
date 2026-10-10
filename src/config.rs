use crate::{APP_NAME, session::ViewPreferences};
use anyhow::{Context, Result};
use chrono::{Local, Timelike};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static SAVE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub agents: AgentSettings,
    pub agent_view: ViewPreferences,
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
            agents: AgentSettings::default(),
            agent_view: ViewPreferences::default(),
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
        anyhow::ensure!(
            (1..=86400).contains(&settings.agent_view.rotate_interval_seconds),
            "rotation interval must be 1..86400 seconds"
        );
        Ok(settings)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self)?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let filename = path.file_name().context("Settings path has no filename")?;
        // The same directory guarantees replacement stays on the same volume.
        // create_new also protects against stale files from an interrupted save.
        let (temporary, mut file) = loop {
            let sequence = SAVE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let mut name = filename.to_os_string();
            name.push(format!(".{}.{}.tmp", std::process::id(), sequence));
            let temporary = parent.join(name);
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
            {
                Ok(file) => break (temporary, file),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        };
        let result = (|| -> Result<()> {
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            replace_file(&temporary, path)?;
            #[cfg(unix)]
            fs::File::open(parent)?.sync_all()?;
            Ok(())
        })();
        // Failed writes/replacements leave the previous config intact. A crash
        // before replacement can leave only the uniquely named temporary file.
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.with_context(|| format!("Saving settings: {}", path.display()))
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

#[cfg(windows)]
fn replace_file(temporary: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let source: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // Unlike remove-then-rename, replacement never exposes a missing or partial
    // settings file. WRITE_THROUGH waits for the replacement to reach disk.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(temporary: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(temporary, destination)
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

#[test]
fn save_replaces_an_existing_config_and_removes_temporary_files() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("nested/settings.json");
    Settings::default().save(&path).unwrap();
    let updated = Settings {
        brightness: 9,
        rotate: false,
        ..Default::default()
    };
    updated.save(&path).unwrap();
    let loaded = Settings::load(Some(&path)).unwrap();
    assert_eq!(loaded.brightness, 9);
    assert!(!loaded.rotate);
    assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
}

#[cfg(windows)]
#[test]
fn replacement_failure_preserves_valid_config_and_cleans_temporary_file() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("settings.json");
    let original = Settings {
        brightness: 3,
        ..Default::default()
    };
    original.save(&path).unwrap();
    let original_bytes = fs::read(&path).unwrap();
    // Simulate another program reading the file without allowing replacement.
    // Reading remains possible, but Windows denies deletion/rename while held.
    let reader = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(&path)
        .unwrap();
    let updated = Settings {
        brightness: 8,
        ..Default::default()
    };
    assert!(updated.save(&path).is_err());
    assert_eq!(fs::read(&path).unwrap(), original_bytes);
    assert_eq!(Settings::load(Some(&path)).unwrap().brightness, 3);
    assert_eq!(fs::read_dir(folder.path()).unwrap().count(), 1);
    drop(reader);
    updated.save(&path).unwrap();
    assert_eq!(Settings::load(Some(&path)).unwrap().brightness, 8);
}

#[test]
fn simultaneous_saves_always_leave_a_complete_config() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("settings.json");
    Settings::default().save(&path).unwrap();
    let saved = std::thread::scope(|scope| {
        let mut writers = Vec::new();
        for brightness in 1..=10 {
            let path = &path;
            writers.push(scope.spawn(move || {
                Settings {
                    brightness,
                    ..Default::default()
                }
                .save(path)
            }));
        }
        writers
            .into_iter()
            .map(|writer| writer.join().unwrap())
            .filter(Result::is_ok)
            .count()
    });
    // Windows can deny racing replacements. Successful and failed writers must
    // both leave a readable complete config and clean up their own temp files.
    assert!(saved > 0);
    let settings = Settings::load(Some(&path)).unwrap();
    assert!((1..=10).contains(&settings.brightness));
    assert_eq!(fs::read_dir(folder.path()).unwrap().count(), 1);
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSettings {
    pub windows_enabled: bool,
    pub wsl_running_enabled: bool,
    pub wsl_default_user: bool,
    pub extra_wsl_users: Vec<WslUser>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct WslUser {
    pub distro: String,
    pub user: String,
}
impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            windows_enabled: true,
            wsl_running_enabled: true,
            wsl_default_user: true,
            extra_wsl_users: vec![],
        }
    }
}

#[test]
fn legacy_settings_migrate_without_session_binding_fields() {
    let settings: Settings = serde_json::from_str(r#"{"left":"Codex","right":"Codex","brightness":7,"rotate":false,"night_enabled":true,"night_start":1200,"night_end":400,"font":"custom.ttf","follow_system_display":false}"#).unwrap();
    let value = serde_json::to_value(&settings).unwrap();
    assert!(value.get("left").is_none() && value.get("right").is_none());
    assert_eq!(settings.brightness, 7);
    assert!(!settings.rotate);
    assert!(settings.night_enabled);
    assert!(!settings.follow_system_display);
    assert_eq!(settings.agent_view.mode, crate::session::ViewMode::Auto);
    assert!(settings.agents.windows_enabled && settings.agents.wsl_running_enabled);
}
