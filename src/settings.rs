//! User settings: a JSON file in the data folder. Reading tolerates a missing or broken
//! file (defaults are used), unknown keys are ignored, new keys get defaults, and writing
//! is atomic, so an update or a crash never costs the user their settings.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use windows::Win32::UI::Shell::{FOLDERID_RoamingAppData, FOLDERID_Videos, SHGetKnownFolderPath, KF_FLAG_DEFAULT};
use windows::Win32::System::Com::CoTaskMemFree;

use crate::capture::Source;
use crate::encoder::{Codec, VideoCfg};
use crate::engine::Config;
use crate::sources;

pub const APP_NAME: &str = "Rewind";
const DATA_FOLDER: &str = "Rewind";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SourceSetting {
    /// The primary display.
    Primary,
    /// A display by its device name (`\\.\DISPLAY2`).
    Monitor { device: String },
    /// The first window whose title contains this text.
    Window { title: String },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Hotkeys {
    pub save_clip: String,
    pub record: String,
    pub buffer: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self { save_clip: "Ctrl+Alt+F10".into(), record: "Ctrl+Alt+F9".into(), buffer: "Ctrl+Alt+F8".into() }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub source: SourceSetting,
    /// Record only this area of the source: [x, y, width, height].
    pub crop: Option<[u32; 4]>,
    pub fps: u32,
    /// "hevc", "h264" or "av1".
    pub codec: String,
    /// Constant quality, lower is better.
    pub quality: u32,
    pub max_mbit: u32,
    /// Length of the replay buffer.
    pub buffer_seconds: u32,
    /// Whether the replay buffer runs. Remembered between launches.
    pub buffer_on: bool,
    pub buffer_memory_mb: usize,
    /// Empty = the user's Videos folder \ Rewind.
    pub output_dir: String,
    pub cursor: bool,
    pub system_sound: bool,
    pub microphone: bool,
    pub audio_kbit: u32,
    pub hotkeys: Hotkeys,
    pub notifications: bool,
    /// Start with Windows, silently in the tray.
    pub autostart: bool,
    /// Keep a shortcut in the Start menu.
    pub start_menu: bool,
    /// Look for a new version once a day and download it.
    pub auto_update: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            source: SourceSetting::Primary,
            crop: None,
            fps: 60,
            codec: "hevc".into(),
            quality: 20,
            max_mbit: 100,
            buffer_seconds: 600,
            buffer_on: true,
            buffer_memory_mb: 4096,
            output_dir: String::new(),
            cursor: true,
            system_sound: true,
            microphone: true,
            audio_kbit: 192,
            hotkeys: Hotkeys::default(),
            notifications: true,
            autostart: false,
            start_menu: true,
            auto_update: true,
        }
    }
}

fn known_folder(id: &windows::core::GUID) -> Option<PathBuf> {
    unsafe {
        let raw = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let path = raw.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(raw.0 as *const _));
        path
    }
}

/// Where settings and the journal live: `--data DIR`, else a `data` folder next to the
/// exe (portable / test builds keep their own data), else `%APPDATA%\Rewind`.
/// The flag is true for that last, standard place: only such a copy settles into the system.
pub fn data_dir(explicit: Option<PathBuf>) -> (PathBuf, bool) {
    if let Some(dir) = explicit {
        return (dir, false);
    }
    if let Some(portable) = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.join("data"))) {
        if portable.is_dir() {
            return (portable, false);
        }
    }
    (known_folder(&FOLDERID_RoamingAppData).unwrap_or_else(|| PathBuf::from(".")).join(DATA_FOLDER), true)
}

impl Settings {
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(text.trim_start_matches('\u{feff}')).unwrap_or_else(|e| {
                crate::log::line(&format!("settings file is damaged ({e}); using defaults"));
                Settings::default()
            }),
            Err(_) => Settings::default(),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, path)?; // replaces the old file in one step
        Ok(())
    }

    pub fn output_dir(&self) -> PathBuf {
        if !self.output_dir.trim().is_empty() {
            return PathBuf::from(self.output_dir.trim());
        }
        known_folder(&FOLDERID_Videos)
            .or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join("Videos")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join(APP_NAME)
    }

    /// Resolves the settings into what the engine needs right now. A display that is no
    /// longer connected falls back to the primary one; a missing window is an error.
    pub fn engine_config(&self) -> Result<Config> {
        let monitors = sources::monitors();
        let primary = monitors.first().ok_or_else(|| anyhow!("no display found"))?;
        let source = match &self.source {
            SourceSetting::Primary => Source::Monitor(primary.handle),
            SourceSetting::Monitor { device } => {
                Source::Monitor(monitors.iter().find(|m| &m.name == device).unwrap_or(primary).handle)
            }
            SourceSetting::Window { title } => {
                let (handle, _) =
                    sources::find_window(title).ok_or_else(|| anyhow!("no open window with \"{title}\" in its title"))?;
                Source::Window(handle)
            }
        };
        let codec = match self.codec.to_ascii_lowercase().as_str() {
            "h264" | "avc" => Codec::H264,
            "av1" => Codec::Av1,
            _ => Codec::Hevc,
        };
        Ok(Config {
            source,
            fps: self.fps.clamp(1, 240),
            video: VideoCfg {
                codec,
                quality: self.quality.clamp(1, 51),
                max_bitrate: self.max_mbit.clamp(1, 500) as i64 * 1_000_000,
                gop_secs: 2,
            },
            buffer_secs: self.buffer_seconds,
            buffer_on: self.buffer_on && self.buffer_seconds > 0,
            ring_mb: self.buffer_memory_mb.max(64),
            out_dir: self.output_dir(),
            crop: self.crop.map(|c| (c[0], c[1], c[2], c[3])),
            cursor: self.cursor,
            audio_system: self.system_sound,
            audio_mic: self.microphone,
            audio_bitrate: self.audio_kbit.clamp(32, 512) as i64 * 1000,
            force_vendor: None,
            verbose: false,
        })
    }
}
