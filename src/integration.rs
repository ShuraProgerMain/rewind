//! How the program makes itself at home without an installer: a Start menu shortcut and
//! (if the user wants) a start-with-Windows entry. Both follow the exe when the folder is
//! moved, and both can be removed from the settings window.
//!
//! Only a real copy does this. A portable or test copy (its own `data` folder, `--data`,
//! or a build inside `target\`) never touches the system.

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use windows::core::{Interface, HSTRING, PCWSTR};
use windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, IPersistFile, CLSCTX_INPROC_SERVER, STGM_READ};
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};
use windows::Win32::UI::Shell::{FOLDERID_Programs, IShellLinkW, SHGetKnownFolderPath, ShellLink, KF_FLAG_DEFAULT};

const NAME: &str = "Rewind";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

pub struct System {
    exe: PathBuf,
    /// Folder of the Start menu shortcut.
    programs: PathBuf,
    /// Registry key (under HKCU) with the start-with-Windows entry.
    run_key: String,
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

impl System {
    /// None when this copy must leave the system alone. `standard_data` tells whether the
    /// settings live in the normal per-user folder.
    pub fn detect(standard_data: bool) -> Option<System> {
        let exe = std::env::current_exe().ok()?;
        // Tests: REWIND_SYSTEM_TEST=<folder> sends the shortcut there and the registry
        // entry to a scratch key, so the real Start menu and autostart stay untouched.
        if let Some(dir) = std::env::var_os("REWIND_SYSTEM_TEST") {
            return Some(System { exe, programs: PathBuf::from(dir), run_key: r"Software\Rewind\TestRun".into() });
        }
        let lower = exe.to_string_lossy().to_lowercase();
        if !standard_data || lower.contains(r"\target\release\") || lower.contains(r"\target\debug\") {
            return None;
        }
        let programs = unsafe {
            let raw = SHGetKnownFolderPath(&FOLDERID_Programs, KF_FLAG_DEFAULT, None).ok()?;
            let path = raw.to_string().ok().map(PathBuf::from);
            CoTaskMemFree(Some(raw.0 as *const c_void));
            path?
        };
        Some(System { exe, programs, run_key: RUN_KEY.into() })
    }

    fn shortcut(&self) -> PathBuf {
        self.programs.join(format!("{NAME}.lnk"))
    }

    fn run_command(&self) -> String {
        format!("\"{}\" --background", self.exe.display())
    }

    /// Brings the system in line with the two switches. Cheap when nothing has to change,
    /// so it runs on every start (which is what re-points everything after the folder moved).
    pub fn apply(&self, start_menu: bool, autostart: bool) -> Result<()> {
        let link = self.shortcut();
        if start_menu {
            if unsafe { shortcut_target(&link) }.as_deref() != Some(self.exe.as_path()) {
                unsafe { write_shortcut(&link, &self.exe)? };
            }
        } else if link.exists() {
            std::fs::remove_file(&link)?;
        }

        let key = HSTRING::from(self.run_key.as_str());
        let name = HSTRING::from(NAME);
        unsafe {
            if autostart {
                let command = self.run_command();
                if read_string(&key, &name).as_deref() != Some(command.as_str()) {
                    let data = wide(&command);
                    RegSetKeyValueW(
                        HKEY_CURRENT_USER,
                        &key,
                        &name,
                        REG_SZ.0,
                        Some(data.as_ptr() as *const c_void),
                        (data.len() * 2) as u32,
                    )
                    .ok()
                    .map_err(|e| anyhow!("cannot write the autostart entry: {e}"))?;
                }
            } else {
                let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, &key, &name);
            }
        }
        Ok(())
    }

    /// Takes the program out of the Start menu and out of autostart.
    pub fn remove(&self) {
        let _ = self.apply(false, false);
    }
}

unsafe fn read_string(key: &HSTRING, name: &HSTRING) -> Option<String> {
    let mut buf = [0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    RegGetValueW(
        HKEY_CURRENT_USER,
        key,
        name,
        RRF_RT_REG_SZ,
        None,
        Some(buf.as_mut_ptr() as *mut c_void),
        Some(&mut size),
    )
    .ok()
    .ok()?;
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// Where an existing shortcut points, if it can be read.
unsafe fn shortcut_target(link: &Path) -> Option<PathBuf> {
    if !link.exists() {
        return None;
    }
    let shell: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
    let file: IPersistFile = shell.cast().ok()?;
    file.Load(&HSTRING::from(link.as_os_str()), STGM_READ).ok()?;
    let mut buf = [0u16; 1024];
    shell.GetPath(&mut buf, std::ptr::null_mut::<WIN32_FIND_DATAW>(), 0).ok()?;
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    (len > 0).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..len])))
}

unsafe fn write_shortcut(link: &Path, exe: &Path) -> Result<()> {
    if let Some(dir) = link.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let shell: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
    let target = HSTRING::from(exe.as_os_str());
    shell.SetPath(&target)?;
    if let Some(dir) = exe.parent() {
        shell.SetWorkingDirectory(&HSTRING::from(dir.as_os_str()))?;
    }
    shell.SetDescription(&HSTRING::from("Запись экрана и повтор последних минут"))?;
    shell.SetIconLocation(&target, 0)?;
    let file: IPersistFile = shell.cast()?;
    file.Save(PCWSTR(HSTRING::from(link.as_os_str()).as_ptr()), true)?;
    Ok(())
}
