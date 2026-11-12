//! The everyday program: a tray icon with a menu, global hotkeys and notifications, driving
//! the recording engine. No window and no web view exist while it just sits in the tray.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::SystemTime;

use anyhow::{anyhow, Result};
use windows::core::{w, HSTRING, PCWSTR};
use serde_json::json;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey};
use windows::Win32::UI::Shell::{
    ILCreateFromPathW, ILFree, SHOpenFolderAndSelectItems, ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE,
    NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu,
    DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, PostMessageW, PostQuitMessage, RegisterClassExW,
    RegisterWindowMessageW, SetForegroundWindow, SetTimer, WM_TIMER, TrackPopupMenu, TranslateMessage, HICON, HMENU, MF_CHECKED,
    MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MSG, SM_CXSMICON, SW_SHOWNORMAL, TPM_NONOTIFY, TPM_RETURNCMD,
    ASFW_ANY, TPM_RIGHTBUTTON, WM_APP, WM_CONTEXTMENU, WM_DESTROY, WM_ENDSESSION, WM_HOTKEY, WM_NULL, WM_SETTINGCHANGE,
    WNDCLASSEXW, WS_OVERLAPPED,
};

use crate::engine::{self, Cmd, Config, Event};
use crate::gamepad;
use crate::hotkey;
use crate::icons;
use crate::integration::System;
use crate::ipc;
use crate::log;
use crate::picker;
use crate::settings::{self, Settings, SourceSetting, APP_NAME};
use crate::settings_window::{self, SettingsWindow, WM_UI_CLOSE, WM_UI_FOCUS};
use crate::sources;
use crate::toast::{self, Tone};
use crate::updater::{self, Outcome};

const WM_TRAY: u32 = WM_APP + 1;
const WM_ENGINE: u32 = WM_APP + 2;
const WM_IPC: u32 = WM_APP + 3;
/// The settings page sent something.
const WM_UI: u32 = WM_APP + 4;
/// An update check has finished.
const WM_UPDATE: u32 = WM_APP + 7;
/// A trim has finished.
const WM_TRIMMED: u32 = WM_APP + 9;
/// A screenshot has been saved (or failed).
const WM_SHOT: u32 = WM_APP + 13;
// Buttons of the "saved" notification (ids for WM_TOAST_ACTION).
const ACT_OPEN: usize = 1;
const ACT_REVEAL: usize = 2;
const ACT_TRIM: usize = 3;
const TIMER_UPDATE: usize = 1;
const FIRST_CHECK_MS: u32 = 30_000;
const DAY_MS: u32 = 24 * 60 * 60 * 1000;
const NIN_SELECT: u32 = 0x400;
const NIN_KEYSELECT: u32 = 0x401;

// Menu commands. Hotkey ids reuse the first three.
const CMD_SAVE: usize = 1;
const CMD_RECORD: usize = 2;
const CMD_BUFFER: usize = 3;
const CMD_SHOT: usize = 4;
const CMD_OPEN_FOLDER: usize = 10;
const CMD_SHOW_LAST: usize = 11;
const CMD_SETTINGS: usize = 12;
const CMD_OPEN_LOG: usize = 13;
const CMD_SOUND: usize = 20;
const CMD_MIC: usize = 21;
const CMD_NOTIFY: usize = 22;
const CMD_QUIT: usize = 99;
const CMD_MONITOR: usize = 100; // + display index
const CMD_LENGTH: usize = 200; // + index into BUFFER_LENGTHS
const CMD_WINDOW: usize = 400; // + index into the window list shown in the menu

const BUFFER_LENGTHS: [u32; 8] = [30, 60, 120, 300, 600, 900, 1800, 3600];

struct App {
    hwnd: HWND,
    settings_path: PathBuf,
    log_path: PathBuf,
    settings: Settings,
    settings_stamp: Option<SystemTime>,
    /// What the engine currently runs with.
    engine_cfg: Option<Config>,
    tx: Sender<Cmd>,
    events: Receiver<Event>,
    ipc: Receiver<String>,
    engine: Option<JoinHandle<()>>,
    buffer_on: bool,
    recording: bool,
    last_file: Option<PathBuf>,
    icons: Vec<HICON>,
    shown_icon: usize,
    taskbar_created: u32,
    /// Window titles offered in the menu that is currently open.
    menu_windows: Vec<String>,
    data_dir: PathBuf,
    /// The settings window; exists only while it is open.
    ui: Option<SettingsWindow>,
    /// The page has loaded and asked for its state.
    ui_ready: bool,
    ui_tx: Sender<String>,
    ui_rx: Receiver<String>,
    /// Hotkeys that another program already owns.
    taken: Vec<String>,
    exe: PathBuf,
    /// Arguments to start this copy again with (after an update).
    launch_args: Vec<String>,
    /// Present for a real copy; a portable or test copy leaves the system alone.
    system: Option<System>,
    update: UpdateState,
    update_tx: Sender<(bool, Result<Outcome, String>)>,
    update_rx: Receiver<(bool, Result<Outcome, String>)>,
    /// A file to open in the trim view as soon as the settings page is ready.
    pending_trim: Option<PathBuf>,
    trim_tx: Sender<Result<crate::library::Trimmed, String>>,
    trim_rx: Receiver<Result<crate::library::Trimmed, String>>,
    shot_tx: Sender<Result<PathBuf, String>>,
    shot_rx: Receiver<Result<PathBuf, String>>,
    /// A screenshot is being taken (a second press waits for it).
    shot_busy: bool,
    /// The action whose gamepad combination is being learned.
    pad_learning: Option<String>,
    /// Gamepads connected, as last seen while learning.
    pads: Option<usize>,
    /// Shown under the combination being learned ("needs two buttons").
    pad_hint: String,
}

/// What the settings page shows about updates.
#[derive(Clone, Debug, PartialEq, Eq)]
enum UpdateState {
    Idle,
    Checking,
    Current,
    /// Downloaded; goes live at the next start.
    Ready(String),
    /// A newer release needs the full archive: version and release page.
    Manual(String, String),
    Failed,
}

/// Things a page message asks for that run their own message loop (dialogs, dragging), so
/// they have to happen after the app state is released.
enum Deferred {
    Drag(HWND),
    PickFolder(HWND, PathBuf),
    PickArea(RECT),
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Runs `f` on the app state. Returns None if the state is busy (a nested message while
/// another handler is still running), in which case the caller simply skips or re-posts.
/// Also None while the process is exiting: window messages can still arrive then (the
/// browser shutting down), after the state is gone.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.try_with(|cell| cell.try_borrow_mut().ok().and_then(|mut slot| slot.as_mut().map(f))).ok().flatten()
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// "30 с", "10 мин", "1 ч 5 мин".
fn duration_text(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs} с"),
        60..=3599 if secs % 60 == 0 => format!("{} мин", secs / 60),
        60..=3599 => format!("{} мин {} с", secs / 60, secs % 60),
        _ if secs % 3600 < 60 => format!("{} ч", secs / 3600),
        _ => format!("{} ч {} мин", secs / 3600, secs % 3600 / 60),
    }
}

fn size_text(bytes: u64) -> String {
    let mb = bytes as f64 / 1048576.0;
    if mb >= 1024.0 {
        format!("{:.1} ГБ", mb / 1024.0)
    } else if mb >= 10.0 {
        format!("{mb:.0} МБ")
    } else {
        format!("{mb:.1} МБ")
    }
}

impl App {
    fn notify(&self, tone: Tone, title: &str, detail: &str) {
        if self.settings.notifications || tone == Tone::Error {
            toast::show(tone, title, detail);
        }
    }

    /// "Saved" notification with buttons that act on the file just made.
    fn notify_saved(&self, title: &str, detail: &str) {
        if self.settings.notifications {
            let actions = [("Открыть", ACT_OPEN), ("В папке", ACT_REVEAL), ("Обрезать", ACT_TRIM)];
            toast::show_with(Tone::Good, title, detail, &actions);
        }
    }

    /// Opens the trim view for a file (opening the settings window first if needed).
    fn want_trim(&mut self, path: PathBuf) {
        self.pending_trim = Some(path);
    }

    /// Hands a pending trim to the page once it can take it.
    fn deliver_trim(&mut self) {
        if !self.ui_ready {
            return;
        }
        if let (Some(ui), Some(path)) = (&self.ui, self.pending_trim.take()) {
            let arg = serde_json::to_string(&path.to_string_lossy()).unwrap_or_default();
            ui.eval(&format!("window.rewind&&window.rewind.trim({arg})"));
            ui.show();
        }
    }

    /// Cuts a file on a worker thread; the result comes back as WM_TRIMMED.
    fn start_trim(&self, path: PathBuf, start: f64, end: f64) {
        if !crate::library::inside(&path) {
            log::line(&format!("refused to trim a file outside the recordings folder: {}", path.display()));
            return;
        }
        let (tx, target) = (self.trim_tx.clone(), self.hwnd.0 as isize);
        thread::spawn(move || {
            let result = crate::library::trim(&path, start, end).map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
            unsafe {
                let _ = PostMessageW(Some(HWND(target as _)), WM_TRIMMED, WPARAM(0), LPARAM(0));
            }
        });
    }

    fn on_trimmed(&mut self, result: Result<crate::library::Trimmed, String>) {
        match &result {
            Ok(done) => {
                let secs = (done.end - done.start).max(0.0);
                log::line(&format!("trimmed: {} ({:.2}..{:.2} s)", done.path, done.start, done.end));
                self.last_file = Some(PathBuf::from(&done.path));
                if self.settings.notifications {
                    let actions = [("Открыть", ACT_OPEN), ("В папке", ACT_REVEAL)];
                    toast::show_with(Tone::Good, "Обрезанный клип сохранён", &format!("{} · рядом с исходным", duration_text(secs.round() as u64)), &actions);
                }
            }
            Err(e) => {
                log::line(&format!("trim failed: {e}"));
                self.notify(Tone::Error, "Не удалось обрезать", e);
            }
        }
        if let Some(ui) = &self.ui {
            let answer = match &result {
                Ok(done) => serde_json::json!({ "ok": true, "path": done.path, "start": done.start, "end": done.end }),
                Err(e) => serde_json::json!({ "ok": false, "error": e }),
            };
            ui.eval(&format!("window.rewind&&window.rewind.trimmed({answer})"));
        }
        self.push_files();
    }

    /// A button on the "saved" notification was pressed.
    fn on_toast_action(&mut self, id: usize) {
        let Some(file) = self.last_file.clone() else { return };
        match id {
            ACT_OPEN => open_path(file, None),
            ACT_REVEAL => reveal_file(file),
            ACT_TRIM => self.want_trim(file),
            _ => {}
        }
    }

    fn icon_state(&self) -> icons::State {
        if self.recording {
            icons::State::Recording
        } else if self.buffer_on {
            icons::State::Buffer
        } else {
            icons::State::Off
        }
    }

    fn tooltip(&self) -> String {
        let buffer = format!("буфер: последние {}", duration_text(self.settings.buffer_seconds as u64));
        match (self.recording, self.buffer_on) {
            (true, true) => format!("{APP_NAME} — идёт запись, {buffer}"),
            (true, false) => format!("{APP_NAME} — идёт запись"),
            (false, true) => format!("{APP_NAME} — {buffer}"),
            (false, false) => format!("{APP_NAME} — выключен"),
        }
    }

    fn tray_data(&self) -> NOTIFYICONDATAW {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
            uCallbackMessage: WM_TRAY,
            hIcon: self.icons.get(self.icon_state() as usize).copied().unwrap_or_default(),
            ..Default::default()
        };
        let tip = wide(&self.tooltip());
        let n = tip.len().min(nid.szTip.len() - 1);
        nid.szTip[..n].copy_from_slice(&tip[..n]);
        nid.szTip[nid.szTip.len() - 1] = 0;
        nid
    }

    fn add_tray(&mut self) {
        let mut nid = self.tray_data();
        unsafe {
            let _ = Shell_NotifyIconW(NIM_ADD, &nid);
            nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
        }
        self.shown_icon = self.icon_state() as usize;
    }

    fn update_tray(&mut self) {
        let nid = self.tray_data();
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
        self.shown_icon = self.icon_state() as usize;
        self.update_indicator();
    }

    /// The on-screen pill: while recording, or also while the buffer runs, as the user chose.
    fn update_indicator(&self) {
        use crate::indicator::{set, Show};
        let mode = self.settings.indicator.as_str();
        let show = if mode == "off" {
            Show::Hidden
        } else if self.recording {
            Show::Recording
        } else if self.buffer_on && mode == "always" {
            Show::Buffer
        } else {
            Show::Hidden
        };
        // On the display being recorded (for a window: the display it is on).
        let monitor = match self.engine_cfg.as_ref().map(|c| c.source) {
            Some(crate::capture::Source::Monitor(h)) => h,
            Some(crate::capture::Source::Window(h)) => unsafe {
                windows::Win32::Graphics::Gdi::MonitorFromWindow(HWND(h as _), windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST).0 as isize
            },
            None => 0,
        };
        set(show, monitor);
    }

    fn make_icons(&mut self) {
        let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16) as u32;
        let light = icons::taskbar_is_light();
        let fresh: Vec<HICON> = icons::ALL.iter().filter_map(|s| icons::make(*s, size, light).ok()).collect();
        if fresh.len() == icons::ALL.len() {
            for old in std::mem::replace(&mut self.icons, fresh) {
                unsafe {
                    let _ = DestroyIcon(old);
                }
            }
        }
    }

    /// (Re)registers the global hotkeys; returns the ones another program already owns.
    fn register_hotkeys(&self) -> Vec<String> {
        let mut taken = Vec::new();
        let keys = &self.settings.hotkeys;
        for (id, text) in [
            (CMD_SAVE, &keys.save_clip),
            (CMD_RECORD, &keys.record),
            (CMD_BUFFER, &keys.buffer),
            (CMD_SHOT, &keys.screenshot),
        ] {
            unsafe {
                let _ = UnregisterHotKey(Some(self.hwnd), id as i32);
                if text.trim().is_empty() {
                    continue; // switched off by the user
                }
                match hotkey::parse(text) {
                    Some(hk) => {
                        if RegisterHotKey(Some(self.hwnd), id as i32, hk.mods, hk.vk).is_err() {
                            taken.push(text.clone());
                        }
                    }
                    None => log::line(&format!("hotkey \"{text}\" is not understood and was ignored")),
                }
            }
        }
        taken
    }

    /// Hands the gamepad combinations to the pad reader (which runs only when some are set).
    fn configure_pad(&self) {
        let p = &self.settings.pad;
        let combos = [(CMD_SAVE, &p.save_clip), (CMD_RECORD, &p.record), (CMD_BUFFER, &p.buffer), (CMD_SHOT, &p.screenshot)]
            .into_iter()
            .filter_map(|(id, text)| gamepad::parse(text).map(|mask| (id, mask)))
            .collect();
        gamepad::configure(self.hwnd, combos);
    }

    /// Starts or stops learning a gamepad combination for one action.
    fn pad_learn(&mut self, id: Option<String>) {
        self.pad_hint.clear();
        gamepad::learn(self.hwnd, id.is_some());
        if id.is_none() {
            self.pads = None;
        }
        self.pad_learning = id;
        self.push_state();
    }

    /// Buttons were held together and let go while learning.
    fn on_pad_learned(&mut self, mask: u32) {
        let Some(id) = self.pad_learning.clone() else { return };
        if mask.count_ones() < gamepad::MIN_BUTTONS {
            self.pad_hint = "Нужны хотя бы две кнопки одновременно".into();
            self.push_state();
            return;
        }
        let text = gamepad::format(mask);
        // One combination, one action: it is taken away from any other.
        for other in ["save_clip", "record", "buffer", "screenshot"] {
            if let Some(value) = self.settings.pad.get_mut(other) {
                if *value == text {
                    value.clear();
                }
            }
        }
        if let Some(value) = self.settings.pad.get_mut(&id) {
            *value = text;
        }
        self.pad_learn(None);
        self.settings_changed(true);
    }

    /// Takes a screenshot on a worker thread; the result comes back as WM_SHOT.
    fn start_shot(&mut self) {
        if self.shot_busy {
            return;
        }
        let cfg = match self.settings.engine_config() {
            Ok(cfg) => cfg,
            Err(e) => {
                self.notify(Tone::Error, "Скриншот не сделан", &format!("{e:#}"));
                return;
            }
        };
        self.shot_busy = true;
        let (tx, target) = (self.shot_tx.clone(), self.hwnd.0 as isize);
        thread::spawn(move || {
            let result = crate::shot::take(&cfg).map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
            unsafe {
                let _ = PostMessageW(Some(HWND(target as _)), WM_SHOT, WPARAM(0), LPARAM(0));
            }
        });
    }

    fn on_shot(&mut self, result: Result<PathBuf, String>) {
        self.shot_busy = false;
        match result {
            Ok(path) => {
                log::line(&format!("screenshot: {}", path.display()));
                let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                self.last_file = Some(path);
                if self.settings.notifications {
                    let actions = [("Открыть", ACT_OPEN), ("В папке", ACT_REVEAL)];
                    toast::show_with(Tone::Good, "Скриншот сохранён", &format!("PNG · {}", size_text(bytes)), &actions);
                }
                self.push_files();
            }
            Err(e) => {
                log::line(&format!("screenshot failed: {e}"));
                self.notify(Tone::Error, "Скриншот не сделан", &e);
            }
        }
    }

    fn save_settings(&mut self) {
        if let Err(e) = self.settings.save(&self.settings_path) {
            log::line(&format!("cannot save settings: {e:#}"));
        }
        self.settings_stamp = std::fs::metadata(&self.settings_path).and_then(|m| m.modified()).ok();
    }

    /// Makes the engine follow the current settings. The buffer on/off state is the
    /// engine's own (it is toggled by commands), so it does not count as a change.
    fn apply_to_engine(&mut self) {
        match self.settings.engine_config() {
            Ok(mut cfg) => {
                cfg.buffer_on = false;
                if self.engine_cfg.as_ref() != Some(&cfg) {
                    self.engine_cfg = Some(cfg.clone());
                    let _ = self.tx.send(Cmd::SetConfig(Box::new(cfg)));
                }
                if self.settings.buffer_seconds == 0 {
                    self.buffer_on = false;
                }
            }
            Err(e) => {
                log::line(&format!("settings not applied: {e:#}"));
                self.notify(Tone::Error, "Источник недоступен", &format!("{e:#}"));
            }
        }
    }

    /// Settings were changed (menu or file): store them and bring everything in line.
    fn settings_changed(&mut self, save: bool) {
        if save {
            self.save_settings();
        }
        let taken = self.register_hotkeys();
        self.configure_pad();
        if !taken.is_empty() && taken != self.taken {
            self.notify(Tone::Error, "Клавиша занята другой программой", &taken.join(", "));
        }
        self.taken = taken;
        self.apply_to_engine();
        self.apply_system();
        self.update_tray();
        self.push_state();
    }

    /// Everything the settings page shows, as JSON.
    fn state_json(&self) -> String {
        let monitors: Vec<_> = sources::monitors()
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let main = if m.primary { ", основной" } else { "" };
                json!({
                    "device": m.name,
                    "label": format!("Экран {} — {}×{}{main}", i + 1, m.width(), m.height()),
                    "primary": m.primary,
                })
            })
            .collect();
        let own = self.ui.as_ref().map(|u| u.hwnd().0 as isize);
        let mut windows: Vec<String> = Vec::new();
        for (handle, title) in sources::windows_list() {
            if Some(handle) != own && !windows.contains(&title) && windows.len() < 40 {
                windows.push(title);
            }
        }
        json!({
            "settings": self.settings,
            "monitors": monitors,
            "windows": windows,
            // Programs with a window, for "sound only from this program".
            "apps": sources::apps()
                .into_iter()
                .filter(|a| a.pid != std::process::id())
                .map(|a| json!({ "exe": a.exe, "name": a.name }))
                .collect::<Vec<_>>(),
            "status": { "buffer_on": self.buffer_on, "recording": self.recording, "has_last": self.last_file.is_some() },
            "taken": self.taken,
            "pad": { "learning": self.pad_learning, "pads": self.pads, "hint": self.pad_hint },
            "folder": self.settings.output_dir().display().to_string(),
            "version": updater::VERSION,
            "system": { "available": self.system.is_some() },
            "update": match &self.update {
                UpdateState::Idle => json!({ "state": "idle" }),
                UpdateState::Checking => json!({ "state": "checking" }),
                UpdateState::Current => json!({ "state": "current" }),
                UpdateState::Ready(version) => json!({ "state": "ready", "version": version }),
                UpdateState::Manual(version, _) => json!({ "state": "manual", "version": version }),
                UpdateState::Failed => json!({ "state": "error" }),
            },
        })
        .to_string()
    }

    /// Start menu shortcut and autostart follow the two switches (real copies only).
    fn apply_system(&self) {
        self.configure_library();
        if let Some(system) = &self.system {
            if let Err(e) = system.apply(self.settings.start_menu, self.settings.autostart) {
                log::line(&format!("system integration: {e:#}"));
            }
        }
    }

    /// Looks for a new version in the background. `manual` = the user pressed the button.
    fn check_update(&mut self, manual: bool) {
        if self.system.is_none() || self.update == UpdateState::Checking {
            return;
        }
        self.update = UpdateState::Checking;
        self.push_state();
        let (exe, tx, target) = (self.exe.clone(), self.update_tx.clone(), self.hwnd.0 as isize);
        thread::spawn(move || {
            let result = updater::check(&exe).map_err(|e| format!("{e:#}"));
            let _ = tx.send((manual, result));
            unsafe {
                let _ = PostMessageW(Some(HWND(target as _)), WM_UPDATE, WPARAM(0), LPARAM(0));
            }
        });
    }

    fn on_update(&mut self, manual: bool, result: Result<Outcome, String>) {
        let before = self.update.clone();
        self.update = match result {
            Ok(Outcome::UpToDate) => UpdateState::Current,
            Ok(Outcome::Staged { version }) => UpdateState::Ready(version),
            Ok(Outcome::Manual { version, page }) => UpdateState::Manual(version, page),
            Err(e) => {
                log::line(&format!("update check failed: {e}"));
                UpdateState::Failed
            }
        };
        // Say it once per version, not on every daily check.
        match &self.update {
            UpdateState::Ready(version) if before != self.update || manual => {
                log::line(&format!("update {version} downloaded"));
                self.notify(Tone::Good, &format!("Обновление {version} загружено"), "Установится при следующем запуске");
            }
            UpdateState::Manual(version, _) if before != self.update => {
                log::line(&format!("version {version} is out but needs the full archive"));
                self.notify(Tone::Info, &format!("Вышла версия {version}"), "Откройте настройки, вкладка «Система»");
            }
            _ => {}
        }
        self.push_state();
    }

    /// Restarts into the downloaded version right now (the user asked for it).
    fn restart_for_update(&mut self) {
        match updater::relaunch(&self.exe, &self.launch_args) {
            Ok(()) => self.quit(), // recordings are finalized first
            Err(e) => {
                log::line(&format!("{e:#}"));
                self.notify(Tone::Error, "Не удалось перезапуститься", "Закройте Rewind и откройте снова");
            }
        }
    }

    /// Refreshes the settings page, if it is open.
    fn push_state(&self) {
        if let Some(ui) = &self.ui {
            ui.update(&self.state_json());
        }
    }

    /// The recordings folder, as the file list and the page server see it.
    fn configure_library(&self) {
        crate::library::configure(self.settings.output_dir(), self.data_dir.join("thumbs"));
    }

    /// Sends the list of clips and recordings to the settings page, if it is open.
    fn push_files(&self) {
        if let Some(ui) = &self.ui {
            let list = crate::library::list(60);
            ui.eval(&format!("window.rewind&&window.rewind.files({})", serde_json::to_string(&list).unwrap_or("[]".into())));
        }
    }

    /// Open / show / rename / delete one file from the list on the page.
    fn on_file(&mut self, op: &str, path: &str, name: &str) {
        let path = PathBuf::from(path);
        if !crate::library::inside(&path) {
            log::line(&format!("refused a file action outside the recordings folder: {}", path.display()));
            return;
        }
        match op {
            "open" => open_path(path, None),
            "reveal" => reveal_file(path),
            "delete" => match crate::library::recycle(&path) {
                Ok(()) => {
                    if self.last_file.as_ref() == Some(&path) {
                        self.last_file = None;
                    }
                    self.notify(Tone::Info, "Перемещено в корзину", &path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
                }
                Err(e) => self.notify(Tone::Error, "Не удалось удалить", &format!("{e:#}")),
            },
            "rename" => match crate::library::rename(&path, name) {
                Ok(new) => {
                    if self.last_file.as_ref() == Some(&path) {
                        self.last_file = Some(new);
                    }
                }
                Err(e) => self.notify(Tone::Error, "Не удалось переименовать", &format!("{e:#}")),
            },
            _ => {}
        }
        self.push_files();
        self.push_state();
    }

    fn close_settings(&mut self) {
        if self.ui.take().is_some() {
            self.ui_ready = false;
            self.taken = self.register_hotkeys(); // in case a hotkey was being entered
        }
        if self.pad_learning.is_some() {
            self.pad_learn(None);
        }
    }

    /// Applies a partial settings object sent by the page.
    fn merge_settings(&mut self, patch: &serde_json::Value) {
        let Ok(mut current) = serde_json::to_value(&self.settings) else { return };
        if let (Some(target), Some(changes)) = (current.as_object_mut(), patch.as_object()) {
            for (key, value) in changes {
                target.insert(key.clone(), value.clone());
            }
        }
        match serde_json::from_value::<Settings>(current) {
            Ok(mut fresh) => {
                fresh.buffer_on = self.settings.buffer_on;
                if fresh != self.settings {
                    self.settings = fresh;
                    self.settings_changed(true);
                }
            }
            Err(e) => log::line(&format!("settings from the page were rejected: {e}")),
        }
    }

    /// Handles one message from the settings page.
    fn on_ui(&mut self, raw: &str) -> Option<Deferred> {
        let msg: serde_json::Value = serde_json::from_str(raw).ok()?;
        match msg["t"].as_str()? {
            "ready" => {
                self.ui_ready = true;
                self.push_state();
                self.deliver_trim();
                if let Some(ui) = &self.ui {
                    ui.show();
                }
            }
            "set" => self.merge_settings(&msg["settings"]),
            "pad_learn" => self.pad_learn(msg["id"].as_str().map(str::to_string)),
            "trim" => {
                let path = PathBuf::from(msg["path"].as_str().unwrap_or_default());
                let (start, end) = (msg["start"].as_f64().unwrap_or(0.0), msg["end"].as_f64().unwrap_or(0.0));
                self.start_trim(path, start, end);
            }
            "file" => {
                let field = |k: &str| msg[k].as_str().unwrap_or_default().to_string();
                self.on_file(&field("op"), &field("path"), &field("name"));
            }
            // Diagnostics from the page (test scripts), shortened.
            "log" => log::line(&format!("page: {}", msg["text"].as_str().unwrap_or("").chars().take(300).collect::<String>())),
            "cmd" => match msg["name"].as_str()? {
                "save" => self.on_command(CMD_SAVE),
                "record" => self.on_command(CMD_RECORD),
                "buffer" => self.on_command(CMD_BUFFER),
                "screenshot" => self.on_command(CMD_SHOT),
                "open_folder" => self.on_command(CMD_OPEN_FOLDER),
                "show_last" => self.on_command(CMD_SHOW_LAST),
                "files" => self.push_files(),
                "open_log" => self.on_command(CMD_OPEN_LOG),
                "clear_area" => {
                    self.settings.crop = None;
                    self.settings_changed(true);
                }
                // While a new hotkey is being typed, the old ones must not fire.
                "hotkeys_pause" => {
                    for id in [CMD_SAVE, CMD_RECORD, CMD_BUFFER, CMD_SHOT] {
                        unsafe {
                            let _ = UnregisterHotKey(Some(self.hwnd), id as i32);
                        }
                    }
                }
                "hotkeys_resume" => {
                    self.taken = self.register_hotkeys();
                    self.push_state();
                }
                "close" => self.close_settings(),
                "check_update" => self.check_update(true),
                "restart_update" => self.restart_for_update(),
                "open_release" => {
                    if let UpdateState::Manual(_, page) = &self.update {
                        open_path(PathBuf::from(page), None);
                    }
                }
                "remove_system" => {
                    if let Some(system) = &self.system {
                        system.remove();
                    }
                    self.settings.autostart = false;
                    self.settings.start_menu = false;
                    self.settings_changed(true);
                    self.notify(Tone::Info, "Rewind убран из системы", "Ярлык и автозапуск удалены");
                }
                "drag" => return self.ui.as_ref().map(|ui| Deferred::Drag(ui.hwnd())),
                "pick_folder" => {
                    return self.ui.as_ref().map(|ui| Deferred::PickFolder(ui.hwnd(), self.settings.output_dir()))
                }
                "pick_area" => {
                    let monitors = sources::monitors();
                    let chosen = match &self.settings.source {
                        SourceSetting::Monitor { device } => monitors.iter().find(|m| &m.name == device),
                        _ => None,
                    };
                    let rect = chosen.or(monitors.first())?.rect;
                    if let Some(ui) = &self.ui {
                        ui.hide(); // out of the way while the area is being drawn
                    }
                    return Some(Deferred::PickArea(rect));
                }
                _ => {}
            },
            _ => {}
        }
        None
    }

    /// Picks up edits made to the settings file by hand.
    fn reload_if_edited(&mut self) {
        let stamp = std::fs::metadata(&self.settings_path).and_then(|m| m.modified()).ok();
        if stamp.is_some() && stamp != self.settings_stamp {
            self.settings_stamp = stamp;
            let mut fresh = Settings::load(&self.settings_path);
            fresh.buffer_on = self.buffer_on;
            if fresh != self.settings {
                self.settings = fresh;
                self.settings_changed(false);
            }
        }
    }

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::PipelineOn { width, height, fps, encoder, gpu } => {
                log::line(&format!("capture on: {width}x{height} @ {fps} fps, {encoder} on {gpu}"));
            }
            Event::PipelineOff => {
                log::line("capture off");
                self.recording = false;
            }
            Event::Buffer(on) => {
                self.buffer_on = on;
                self.settings.buffer_on = on;
                self.save_settings();
                if on {
                    let keep = duration_text(self.settings.buffer_seconds as u64);
                    self.notify(Tone::Info, "Буфер повтора включён", &format!("Хранит последние {keep}"));
                } else {
                    self.notify(Tone::Info, "Буфер повтора выключен", "");
                }
            }
            Event::BufferStopped => {
                self.buffer_on = false; // not saved: it comes back on the next launch
                self.notify(Tone::Error, "Буфер повтора остановлен", "Запись не запускается, подробности в журнале");
            }
            Event::RecordingStarted(path) => {
                self.recording = true;
                log::line(&format!("recording to {}", path.display()));
                self.notify(Tone::Record, "Запись началась", "");
            }
            Event::RecordingSaved { path, secs, bytes } => {
                self.recording = false;
                log::line(&format!("recording saved: {} ({secs:.1} s, {bytes} bytes)", path.display()));
                self.last_file = Some(path.clone());
                self.notify_saved("Запись сохранена", &format!("{} · {}", duration_text(secs.round() as u64), size_text(bytes)));
                self.last_file = Some(path);
            }
            Event::ClipSaved { path, secs, bytes, ms } => {
                log::line(&format!("clip saved: {} ({secs:.1} s, {bytes} bytes, {ms} ms)", path.display()));
                self.last_file = Some(path.clone());
                self.notify_saved("Клип сохранён", &format!("Последние {} · {}", duration_text(secs.round() as u64), size_text(bytes)));
                self.last_file = Some(path);
            }
            Event::NothingToSave => {
                self.notify(Tone::Info, "Нечего сохранять", "Буфер повтора выключен или ещё пуст");
            }
            Event::Stat(_) => {}
            Event::Error(text) => {
                log::line(&format!("error: {text}"));
                self.notify(Tone::Error, "Ошибка записи", &text);
            }
        }
        self.update_tray();
        self.push_state();
        self.push_files(); // a new clip or recording appears in the list at once
    }

    fn build_menu(&mut self) -> HMENU {
        unsafe {
            let add = |menu: HMENU, flags, id: usize, text: &str| {
                let _ = AppendMenuW(menu, flags, id, PCWSTR(wide(text).as_ptr()));
            };
            let with_key = |text: &str, key: &str| {
                if key.trim().is_empty() {
                    text.to_string()
                } else {
                    format!("{text}\t{key}")
                }
            };
            let check = |on: bool| if on { MF_STRING | MF_CHECKED } else { MF_STRING };
            let s = &self.settings;
            let keep = duration_text(s.buffer_seconds as u64);
            let menu = CreatePopupMenu().unwrap_or_default();

            add(menu, MF_STRING, CMD_SETTINGS, "Настройки…");
            add(menu, MF_SEPARATOR, 0, "");
            let save_flags = if self.buffer_on { MF_STRING } else { MF_STRING | MF_GRAYED };
            add(menu, save_flags, CMD_SAVE, &with_key(&format!("Сохранить последние {keep}"), &s.hotkeys.save_clip));
            let rec = if self.recording { "Остановить запись" } else { "Начать запись" };
            add(menu, MF_STRING, CMD_RECORD, &with_key(rec, &s.hotkeys.record));
            add(menu, MF_STRING, CMD_SHOT, &with_key("Скриншот", &s.hotkeys.screenshot));
            add(menu, check(self.buffer_on), CMD_BUFFER, &with_key("Буфер повтора", &s.hotkeys.buffer));
            add(menu, MF_SEPARATOR, 0, "");

            // What to record.
            let source = CreatePopupMenu().unwrap_or_default();
            for (i, m) in sources::monitors().iter().enumerate() {
                let current = match &s.source {
                    SourceSetting::Primary => m.primary,
                    SourceSetting::Monitor { device } => device == &m.name,
                    SourceSetting::Window { .. } => false,
                };
                let label =
                    format!("Экран {} — {}×{}{}", i + 1, m.width(), m.height(), if m.primary { ", основной" } else { "" });
                add(source, check(current), CMD_MONITOR + i, &label);
            }
            add(source, MF_SEPARATOR, 0, "");
            let current_window = match &s.source {
                SourceSetting::Window { title } => Some(title.clone()),
                _ => None,
            };
            self.menu_windows = sources::windows_list().into_iter().map(|(_, t)| t).take(40).collect();
            for (i, title) in self.menu_windows.iter().enumerate() {
                let short: String = if title.chars().count() > 60 {
                    title.chars().take(59).chain(std::iter::once('…')).collect()
                } else {
                    title.clone()
                };
                // "&" would become an underlined shortcut letter.
                add(source, check(current_window.as_ref() == Some(title)), CMD_WINDOW + i, &short.replace('&', "&&"));
            }
            add(menu, MF_POPUP, source.0 as usize, "Что записывать");

            let length = CreatePopupMenu().unwrap_or_default();
            for (i, secs) in BUFFER_LENGTHS.iter().enumerate() {
                add(length, check(*secs == s.buffer_seconds), CMD_LENGTH + i, &duration_text(*secs as u64));
            }
            add(menu, MF_POPUP, length.0 as usize, "Длина буфера");

            let sound = CreatePopupMenu().unwrap_or_default();
            add(sound, check(s.system_sound), CMD_SOUND, "Звук программ");
            add(sound, check(s.microphone), CMD_MIC, "Микрофон");
            add(menu, MF_POPUP, sound.0 as usize, "Звук");
            add(menu, check(s.notifications), CMD_NOTIFY, "Уведомления");
            add(menu, MF_SEPARATOR, 0, "");

            add(menu, MF_STRING, CMD_OPEN_FOLDER, "Открыть папку с записями");
            let last = if self.last_file.is_some() { MF_STRING } else { MF_STRING | MF_GRAYED };
            add(menu, last, CMD_SHOW_LAST, "Показать последнюю запись");
            add(menu, MF_STRING, CMD_OPEN_LOG, "Журнал");
            add(menu, MF_SEPARATOR, 0, "");
            add(menu, MF_STRING, CMD_QUIT, "Выход");
            menu
        }
    }

    fn on_command(&mut self, id: usize) {
        match id {
            CMD_SAVE => drop(self.tx.send(Cmd::SaveClip)),
            CMD_RECORD => drop(self.tx.send(Cmd::ToggleRecord)),
            CMD_SHOT => self.start_shot(),
            CMD_BUFFER => {
                if self.settings.buffer_seconds == 0 {
                    self.settings.buffer_seconds = 600;
                    self.settings_changed(true);
                }
                drop(self.tx.send(Cmd::ToggleBuffer));
            }
            CMD_OPEN_FOLDER => {
                let dir = self.settings.output_dir();
                let _ = std::fs::create_dir_all(&dir);
                open_path(dir, None);
            }
            CMD_SHOW_LAST => {
                if let Some(file) = self.last_file.clone() {
                    reveal_file(file);
                }
            }
            CMD_OPEN_LOG => open_path(PathBuf::from("notepad.exe"), Some(self.log_path.clone())),
            CMD_SOUND => {
                self.settings.system_sound = !self.settings.system_sound;
                self.settings_changed(true);
            }
            CMD_MIC => {
                self.settings.microphone = !self.settings.microphone;
                self.settings_changed(true);
            }
            CMD_NOTIFY => {
                self.settings.notifications = !self.settings.notifications;
                self.settings_changed(true);
            }
            CMD_QUIT => self.quit(),
            _ if (CMD_WINDOW..CMD_WINDOW + 100).contains(&id) => {
                if let Some(title) = self.menu_windows.get(id - CMD_WINDOW) {
                    self.settings.source = SourceSetting::Window { title: title.clone() };
                    self.settings_changed(true);
                }
            }
            _ if (CMD_LENGTH..CMD_LENGTH + BUFFER_LENGTHS.len()).contains(&id) => {
                self.settings.buffer_seconds = BUFFER_LENGTHS[id - CMD_LENGTH];
                self.settings_changed(true);
            }
            _ if (CMD_MONITOR..CMD_MONITOR + 32).contains(&id) => {
                if let Some(m) = sources::monitors().get(id - CMD_MONITOR) {
                    self.settings.source = if m.primary {
                        SourceSetting::Primary
                    } else {
                        SourceSetting::Monitor { device: m.name.clone() }
                    };
                    self.settings_changed(true);
                }
            }
            _ => {}
        }
    }

    fn on_ipc(&mut self, command: &str) {
        match command {
            "save" => self.on_command(CMD_SAVE),
            "record" => self.on_command(CMD_RECORD),
            "buffer" => self.on_command(CMD_BUFFER),
            "screenshot" => self.on_command(CMD_SHOT),
            "quit" => self.on_command(CMD_QUIT),
            // Lets tests press menu items: "menu 201".
            other => {
                // Test hooks for the gamepad, which a script cannot press: "pad 4" acts as
                // if the combination for command 4 went down, "pad_learned 960" as if those
                // buttons were held together while learning.
                if let Some(id) = other.strip_prefix("pad ").and_then(|n| n.trim().parse::<usize>().ok()) {
                    unsafe {
                        let _ = PostMessageW(Some(self.hwnd), gamepad::WM_PAD, WPARAM(id), LPARAM(0));
                    }
                    return;
                }
                if let Some(mask) = other.strip_prefix("pad_learned ").and_then(|n| n.trim().parse::<usize>().ok()) {
                    unsafe {
                        let _ = PostMessageW(Some(self.hwnd), gamepad::WM_PAD_LEARNED, WPARAM(mask), LPARAM(0));
                    }
                    return;
                }
                if let Some(id) = other.strip_prefix("menu ").and_then(|n| n.trim().parse::<usize>().ok()) {
                    self.reload_if_edited();
                    self.menu_windows = sources::windows_list().into_iter().map(|(_, t)| t).take(40).collect();
                    self.on_command(id);
                }
            }
        }
    }

    /// Leaves the program. Everything that matters happens right here (open files are
    /// finalized, the tray icon is removed) and then the process ends at once. Waiting for
    /// the message loop to unwind is not safe: a modal loop (a menu, a window being dragged,
    /// a dialog) may be holding it, and the program would hang around invisibly.
    fn quit(&mut self) -> ! {
        self.ui = None; // close the browser window while everything is still alive
        self.stop_engine();
        let nid = self.tray_data();
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        }
        log::line("stopped");
        std::process::exit(0)
    }

    /// Stops the engine and waits until open files are finalized.
    fn stop_engine(&mut self) {
        let _ = self.tx.send(Cmd::Quit);
        if let Some(handle) = self.engine.take() {
            let _ = handle.join();
        }
    }
}

/// Opens a folder, or a file with a given program, without blocking the tray thread.
fn open_path(target: PathBuf, argument: Option<PathBuf>) {
    thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let file = HSTRING::from(target.as_os_str());
        let args = argument.map(|a| HSTRING::from(format!("\"{}\"", a.display())));
        let args_ptr = args.as_ref().map_or(PCWSTR::null(), |a| PCWSTR(a.as_ptr()));
        ShellExecuteW(None, w!("open"), &file, args_ptr, PCWSTR::null(), SW_SHOWNORMAL);
    });
}

/// Opens the folder with the file selected; reuses an Explorer window that is already open.
fn reveal_file(file: PathBuf) {
    thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let item = ILCreateFromPathW(&HSTRING::from(file.as_os_str()));
        if !item.is_null() {
            let _ = SHOpenFolderAndSelectItems(item, None, 0);
            ILFree(Some(item));
        } else if let Some(dir) = file.parent() {
            open_path(dir.to_path_buf(), None);
        }
    });
}

fn show_menu(hwnd: HWND) {
    // Build while holding the state, show without it: the menu runs its own message loop,
    // and engine events must keep flowing meanwhile.
    let Some(menu) = with_app(|app| {
        app.reload_if_edited();
        app.build_menu()
    }) else {
        return;
    };
    let chosen = unsafe {
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd); // otherwise the menu would not close on a click elsewhere
        let id = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY, pt.x, pt.y, None, hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        id.0 as usize
    };
    if chosen == CMD_SETTINGS {
        open_settings();
    } else if chosen != 0 {
        with_app(|app| app.on_command(chosen));
    }
}

/// Opens the settings window, or brings it forward if it is already open. Creating the
/// browser runs a nested message loop, so the app state is not held meanwhile.
fn open_settings() {
    let create = with_app(|app| match &app.ui {
        Some(ui) => {
            ui.show();
            app.push_state();
            None
        }
        None => Some((app.hwnd, app.data_dir.clone(), app.ui_tx.clone())),
    })
    .flatten();
    let Some((hwnd, data_dir, tx)) = create else { return };
    let target = hwnd.0 as isize;
    let opened = SettingsWindow::open(hwnd, &data_dir, move |message| {
        let _ = tx.send(message);
        unsafe {
            let _ = PostMessageW(Some(HWND(target as _)), WM_UI, WPARAM(0), LPARAM(0));
        }
    });
    with_app(|app| match opened {
        Ok(ui) => {
            if app.ui_ready {
                ui.show(); // the page finished loading while the window was still being created
            }
            app.ui = Some(ui);
            app.push_state();
        }
        Err(e) => {
            log::line(&format!("{e:#}"));
            app.notify(Tone::Error, "Окно настроек не открылось", "Подробности в журнале");
        }
    });
}

/// Handles everything the settings page has sent so far.
fn drain_ui(hwnd: HWND) {
    loop {
        let message = match with_app(|app| app.ui_rx.try_recv().ok()) {
            Some(Some(m)) => m,
            Some(None) => return,
            None => {
                // The state is busy right now; come back to the queue in a moment.
                unsafe {
                    let _ = PostMessageW(Some(hwnd), WM_UI, WPARAM(0), LPARAM(0));
                }
                return;
            }
        };
        match with_app(|app| app.on_ui(&message)).flatten() {
            Some(Deferred::Drag(window)) => settings_window::start_drag(window),
            Some(Deferred::PickFolder(owner, start)) => {
                if let Some(dir) = settings_window::pick_folder(owner, &start) {
                    with_app(|app| {
                        app.settings.output_dir = dir.display().to_string();
                        app.settings_changed(true);
                    });
                }
            }
            Some(Deferred::PickArea(rect)) => {
                let area = picker::pick_area(rect);
                with_app(|app| {
                    if let Some(area) = area {
                        app.settings.crop = Some(area);
                        app.settings_changed(true);
                    }
                    if let Some(ui) = &app.ui {
                        ui.show();
                    }
                });
            }
            None => {}
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            let what = (lparam.0 as u32) & 0xFFFF;
            if what == WM_CONTEXTMENU {
                show_menu(hwnd);
            } else if what == NIN_SELECT || what == NIN_KEYSELECT {
                open_settings();
            }
            LRESULT(0)
        }
        WM_UI => {
            drain_ui(hwnd);
            LRESULT(0)
        }
        toast::WM_TOAST_ACTION => {
            let trim = with_app(|app| {
                app.on_toast_action(wparam.0);
                app.pending_trim.is_some()
            })
            .unwrap_or(false);
            if trim {
                open_settings(); // the trim view lives in the settings window
                with_app(|app| app.deliver_trim());
            }
            LRESULT(0)
        }
        gamepad::WM_PAD => {
            with_app(|app| app.on_command(wparam.0));
            LRESULT(0)
        }
        gamepad::WM_PAD_LEARNED => {
            with_app(|app| app.on_pad_learned(wparam.0 as u32));
            LRESULT(0)
        }
        gamepad::WM_PAD_COUNT => {
            with_app(|app| {
                app.pads = Some(wparam.0);
                app.push_state();
            });
            LRESULT(0)
        }
        WM_SHOT => {
            with_app(|app| {
                while let Ok(result) = app.shot_rx.try_recv() {
                    app.on_shot(result);
                }
            });
            LRESULT(0)
        }
        WM_TRIMMED => {
            with_app(|app| {
                while let Ok(result) = app.trim_rx.try_recv() {
                    app.on_trimmed(result);
                }
            });
            LRESULT(0)
        }
        WM_UI_CLOSE => {
            with_app(|app| app.close_settings());
            LRESULT(0)
        }
        WM_UI_FOCUS => {
            // Back in front: put the keyboard focus into the page and refresh the lists.
            with_app(|app| {
                if let Some(ui) = &app.ui {
                    ui.focus();
                }
                app.push_state();
            });
            LRESULT(0)
        }
        WM_ENGINE => {
            let done = with_app(|app| {
                while let Ok(ev) = app.events.try_recv() {
                    app.on_event(ev);
                }
            });
            if done.is_none() {
                let _ = PostMessageW(Some(hwnd), WM_ENGINE, WPARAM(0), LPARAM(0)); // busy: try again shortly
            }
            LRESULT(0)
        }
        WM_IPC => {
            let commands: Vec<String> = with_app(|app| app.ipc.try_iter().collect()).unwrap_or_default();
            for command in commands {
                if command == "show" {
                    open_settings(); // a second launch: this is what the user is looking for
                } else if let Some(script) = command.strip_prefix("ui ") {
                    // Test aid, only with REWIND_DEBUG set: run a script in the settings page.
                    if std::env::var_os("REWIND_DEBUG").is_some() {
                        with_app(|app| {
                            if let Some(ui) = &app.ui {
                                ui.eval(script);
                            }
                        });
                    }
                } else {
                    with_app(|app| app.on_ipc(&command));
                }
            }
            LRESULT(0)
        }
        WM_HOTKEY => {
            with_app(|app| app.on_command(wparam.0));
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            // Light/dark taskbar switched: redraw the icons to stay visible.
            with_app(|app| {
                app.make_icons();
                app.update_tray();
            });
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_UPDATE => {
            with_app(|app| {
                while let Ok((manual, result)) = app.update_rx.try_recv() {
                    app.on_update(manual, result);
                }
            });
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_UPDATE => {
            // First check shortly after start, then once a day.
            SetTimer(Some(hwnd), TIMER_UPDATE, DAY_MS, None);
            with_app(|app| {
                if app.settings.auto_update {
                    app.check_update(false);
                }
            });
            LRESULT(0)
        }
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                with_app(|app| app.stop_engine()); // shutdown or sign-out: finalize recordings first
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            with_app(|app| {
                let nid = app.tray_data();
                let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            });
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => {
            // Explorer restarted: the tray is empty again, put the icon back.
            let recreated = with_app(|app| app.taskbar_created != 0 && msg == app.taskbar_created).unwrap_or(false);
            if recreated {
                with_app(|app| app.add_tray());
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}

/// `data` overrides the data folder; `background` starts silently (autostart).
pub fn run(data: Option<PathBuf>, background: bool, wait_pid: Option<u32>) -> Result<()> {
    if let Some(pid) = wait_pid {
        updater::wait_for(pid); // we were started by a copy that is on its way out
    }
    let mut launch_args: Vec<String> = Vec::new();
    if let Some(dir) = &data {
        launch_args.extend(["--data".to_string(), dir.display().to_string()]);
    }
    if background {
        launch_args.push("--background".to_string());
    }
    let (data_dir, standard_data) = settings::data_dir(data);
    let pipe = ipc::pipe_name(&data_dir);
    // Already running? Hand over and leave within milliseconds. Windows only lets the
    // running copy come to the front if the process the user just started allows it.
    unsafe {
        let _ = AllowSetForegroundWindow(ASFW_ANY);
    }
    if ipc::send(&pipe, if background { "ping" } else { "show" }) {
        return Ok(());
    }
    std::fs::create_dir_all(&data_dir)?;
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED); // dialogs and the browser need it
    }

    // A real copy keeps itself up to date: an update downloaded earlier goes live now.
    let exe = std::env::current_exe()?;
    let system = System::detect(standard_data);
    if system.is_some() {
        updater::cleanup(&exe);
        match updater::swap_in(&exe) {
            Ok(true) => return updater::relaunch(&exe, &launch_args),
            Ok(false) => {}
            Err(e) => eprintln!("update not applied: {e:#}"), // keep running the current version
        }
    }
    let log_path = data_dir.join("rewind.log");
    log::init(&log_path);
    log::line(&format!("started Rewind {}, data folder {}", updater::VERSION, data_dir.display()));

    let hwnd = unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: w!("RewindTray"),
            ..Default::default()
        };
        RegisterClassExW(&class);
        // Never shown; it only receives messages (tray, hotkeys, engine events).
        CreateWindowExW(
            Default::default(),
            w!("RewindTray"),
            w!("Rewind"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )?
    };
    let target = hwnd.0 as isize;

    let (ipc_tx, ipc_rx) = channel::<String>();
    ipc::serve(&pipe, move |command| {
        let _ = ipc_tx.send(command);
        unsafe {
            let _ = PostMessageW(Some(HWND(target as _)), WM_IPC, WPARAM(0), LPARAM(0));
        }
    })
    .map_err(|e| anyhow!("{e}"))?;

    let settings_path = data_dir.join("settings.json");
    let first_run = !settings_path.exists();
    let settings = Settings::load(&settings_path);

    let (tx, rx) = channel::<Cmd>();
    let (ev_tx, ev_rx) = channel::<Event>();
    let emit: engine::Emit = Arc::new(move |ev| {
        let _ = ev_tx.send(ev);
        unsafe {
            let _ = PostMessageW(Some(HWND(target as _)), WM_ENGINE, WPARAM(0), LPARAM(0));
        }
    });

    let (ui_tx, ui_rx) = channel::<String>();
    let (update_tx, update_rx) = channel();
    let (trim_tx, trim_rx) = channel();
    let (shot_tx, shot_rx) = channel();
    let waiting = updater::staged(&exe);
    let mut app = App {
        hwnd,
        settings_path,
        log_path,
        settings,
        settings_stamp: None,
        engine_cfg: None,
        tx,
        events: ev_rx,
        ipc: ipc_rx,
        engine: None,
        buffer_on: false,
        recording: false,
        last_file: None,
        icons: Vec::new(),
        shown_icon: 0,
        taskbar_created: unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
        menu_windows: Vec::new(),
        data_dir: data_dir.clone(),
        ui: None,
        ui_ready: false,
        ui_tx,
        ui_rx,
        taken: Vec::new(),
        exe,
        launch_args,
        system,
        update: match waiting {
            Some(version) => UpdateState::Ready(version),
            None => UpdateState::Idle,
        },
        update_tx,
        update_rx,
        pending_trim: None,
        trim_tx,
        trim_rx,
        shot_tx,
        shot_rx,
        shot_busy: false,
        pad_learning: None,
        pads: None,
        pad_hint: String::new(),
    };
    if first_run {
        app.save_settings(); // so the file exists for editing
    } else {
        app.settings_stamp = std::fs::metadata(&app.settings_path).and_then(|m| m.modified()).ok();
    }

    // Start the engine. If the remembered source is gone (a closed window), fall back to
    // the primary display rather than starting dead.
    let (start_cfg, source_error) = match app.settings.engine_config() {
        Ok(cfg) => (cfg, None),
        Err(e) => {
            let mut fallback = app.settings.clone();
            fallback.source = SourceSetting::Primary;
            (fallback.engine_config()?, Some(format!("{e:#}")))
        }
    };
    app.buffer_on = start_cfg.buffer_on;
    let mut remembered = start_cfg.clone();
    remembered.buffer_on = false;
    app.engine_cfg = Some(remembered);
    app.engine = Some(thread::spawn(move || engine::run(start_cfg, rx, emit)));

    app.make_icons();
    app.add_tray();
    app.apply_system();
    if app.system.is_some() {
        unsafe {
            SetTimer(Some(hwnd), TIMER_UPDATE, FIRST_CHECK_MS, None);
        }
    }
    let taken = app.register_hotkeys();
    app.taken = taken.clone();
    app.configure_pad();
    if let Some(e) = source_error {
        log::line(&format!("source unavailable, recording the primary display instead: {e}"));
        app.notify(Tone::Error, "Источник недоступен", "Записывается основной экран");
    } else if !taken.is_empty() {
        log::line(&format!("hotkeys taken by another program: {}", taken.join(", ")));
        app.notify(Tone::Error, "Клавиша занята другой программой", &taken.join(", "));
    }
    toast::set_owner(hwnd);
    APP.with(|cell| *cell.borrow_mut() = Some(app));
    if first_run && !background {
        open_settings(); // the very first launch: show that the program is here and what it does
    }

    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // Leaving: let the engine finish its files.
    if let Some(mut app) = APP.with(|cell| cell.borrow_mut().take()) {
        app.ui = None;
        app.stop_engine();
        for icon in app.icons.drain(..) {
            unsafe {
                let _ = DestroyIcon(icon);
            }
        }
    }
    log::line("stopped");
    Ok(())
}
