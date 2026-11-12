//! "В Resolve": puts a video with its markers into the project open in DaVinci Resolve.
//! It imports the same `<name>.otio` a person would pick in File → Import → Timeline, into
//! a "Rewind" bin, and opens the new timeline. Resolve's scripting is reached through
//! Python and the module Resolve installs; Resolve must be running with external scripting
//! allowed (Preferences → System → General → External scripting using: Local).

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const SCRIPT: &str = r#"
import sys
try:
    import DaVinciResolveScript as dvr
except Exception as e:
    print("NOMODULE", e); sys.exit(5)
r = dvr.scriptapp("Resolve")
if not r:
    print("NORESOLVE"); sys.exit(2)
project = r.GetProjectManager().GetCurrentProject()
if not project:
    print("NOPROJECT"); sys.exit(3)
pool = project.GetMediaPool()
root = pool.GetRootFolder()
folder = next((f for f in (root.GetSubFolderList() or []) if f.GetName() == "Rewind"), None) or pool.AddSubFolder(root, "Rewind")
if folder:
    pool.SetCurrentFolder(folder)
names = set(project.GetTimelineByIndex(i + 1).GetName() for i in range(project.GetTimelineCount()))
name, n = sys.argv[2], 2
while name in names:
    name = "%s (%d)" % (sys.argv[2], n); n += 1
timeline = pool.ImportTimelineFromFile(sys.argv[1], {"timelineName": name, "importSourceClips": True})
if not timeline:
    print("IMPORTFAIL"); sys.exit(4)
project.SetCurrentTimeline(timeline)
r.OpenPage("edit")
print("OK", timeline.GetName())
"#;

fn resolve_paths() -> (PathBuf, PathBuf) {
    let program_data = std::env::var_os("PROGRAMDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    let program_files = std::env::var_os("PROGRAMFILES").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
    let api = program_data.join(r"Blackmagic Design\DaVinci Resolve\Support\Developer\Scripting");
    let lib = program_files.join(r"Blackmagic Design\DaVinci Resolve\fusionscript.dll");
    (api, lib)
}

/// Imports the video (and its markers) into the open Resolve project. Blocks for a few
/// seconds; call it on a worker thread. Ok = the timeline's name in Resolve.
pub fn send(video: &Path) -> Result<String> {
    let (api, lib) = resolve_paths();
    if !lib.exists() {
        bail!("DaVinci Resolve не установлен");
    }
    let timeline = crate::marks::write_timeline(video, &crate::marks::load(video)).context("не удалось подготовить таймлайн")?;
    let name = video.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "Rewind".into());

    let mut last_error = String::from("не найден Python 3");
    for (exe, pre) in [("python", &[][..]), ("py", &["-3"][..])] {
        let out = Command::new(exe)
            .args(pre)
            .arg("-c")
            .arg(SCRIPT)
            .arg(&timeline)
            .arg(&name)
            .env("RESOLVE_SCRIPT_API", &api)
            .env("RESOLVE_SCRIPT_LIB", &lib)
            .env("PYTHONPATH", api.join("Modules"))
            .env("PYTHONIOENCODING", "utf-8")
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        let Ok(out) = out else { continue }; // this launcher is not installed
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        crate::log::line(&format!("resolve ({exe}): {} {}", text, String::from_utf8_lossy(&out.stderr).trim()));
        if let Some(done) = text.lines().find_map(|l| l.strip_prefix("OK ")) {
            return Ok(done.to_string());
        }
        last_error = match out.status.code() {
            Some(2) => "Resolve не запущен или в его настройках выключены внешние скрипты (Preferences → System → General → External scripting using: Local)".into(),
            Some(3) => "в Resolve не открыт проект".into(),
            Some(4) => "Resolve не принял таймлайн".into(),
            Some(5) => "Python не смог загрузить модуль Resolve".into(),
            // 9009: the Store stub of python.exe without Python installed.
            Some(9009) => continue,
            _ => {
                let err = String::from_utf8_lossy(&out.stderr);
                format!("ошибка скрипта: {}", err.trim().lines().last().unwrap_or(&text))
            }
        };
        break;
    }
    bail!(last_error)
}
