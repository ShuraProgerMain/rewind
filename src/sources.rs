//! What can be recorded: displays and windows.

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::core::PWSTR;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindowLongW, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    GWL_EXSTYLE, WS_EX_TOOLWINDOW,
};

const MONITORINFOF_PRIMARY: u32 = 1;

pub struct Monitor {
    pub handle: isize,
    /// Device name such as `\\.\DISPLAY1`; stable enough to remember in settings.
    pub name: String,
    pub rect: RECT,
    pub primary: bool,
}

impl Monitor {
    pub fn width(&self) -> i32 {
        self.rect.right - self.rect.left
    }
    pub fn height(&self) -> i32 {
        self.rect.bottom - self.rect.top
    }
}

unsafe extern "system" fn monitor_cb(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
    let list = &mut *(data.0 as *mut Vec<isize>);
    list.push(h.0 as isize);
    true.into()
}

/// Displays, primary first.
pub fn monitors() -> Vec<Monitor> {
    let mut handles: Vec<isize> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(monitor_cb), LPARAM(&mut handles as *mut _ as isize));
    }
    let mut out: Vec<Monitor> = handles
        .into_iter()
        .filter_map(|h| unsafe {
            let mut mi = MONITORINFOEXW::default();
            mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            if !GetMonitorInfoW(HMONITOR(h as _), &mut mi as *mut _ as *mut MONITORINFO).as_bool() {
                return None;
            }
            let len = mi.szDevice.iter().position(|&c| c == 0).unwrap_or(mi.szDevice.len());
            Some(Monitor {
                handle: h,
                name: String::from_utf16_lossy(&mi.szDevice[..len]),
                rect: mi.monitorInfo.rcMonitor,
                primary: mi.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            })
        })
        .collect();
    out.sort_by_key(|m| !m.primary);
    out
}

unsafe extern "system" fn window_cb(hwnd: HWND, data: LPARAM) -> BOOL {
    let list = &mut *(data.0 as *mut Vec<(isize, String)>);
    if IsWindowVisible(hwnd).as_bool() && is_app_window(hwnd) {
        let mut buf = [0u16; 512];
        let n = GetWindowTextW(hwnd, &mut buf);
        let mut r = RECT::default();
        if n > 0 && GetWindowRect(hwnd, &mut r).is_ok() && r.right - r.left > 64 && r.bottom - r.top > 64 {
            list.push((hwnd.0 as isize, String::from_utf16_lossy(&buf[..n as usize])));
        }
    }
    true.into()
}

/// Filters out what the user would not call a window: hidden ("cloaked") store apps, tool
/// windows, the desktop itself and our own windows.
unsafe fn is_app_window(hwnd: HWND) -> bool {
    let mut cloaked = 0u32;
    let _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut _, 4);
    if cloaked != 0 || GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0 {
        return false;
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == GetCurrentProcessId() {
        return false;
    }
    let mut class = [0u16; 64];
    let n = GetClassNameW(hwnd, &mut class) as usize;
    !matches!(String::from_utf16_lossy(&class[..n]).as_str(), "Progman" | "WorkerW")
}

/// Visible top-level windows with a title: (handle, title).
pub fn windows_list() -> Vec<(isize, String)> {
    let mut list: Vec<(isize, String)> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(window_cb), LPARAM(&mut list as *mut _ as isize));
    }
    list
}

/// First visible window whose title contains `text` (case-insensitive).
pub fn find_window(text: &str) -> Option<(isize, String)> {
    let needle = text.to_lowercase();
    windows_list().into_iter().find(|(_, t)| t.to_lowercase().contains(&needle))
}

/// A program with a visible window: what "record only this program's sound" offers.
#[derive(Clone, Debug)]
pub struct App {
    pub pid: u32,
    /// File name of the exe, lower case ("eldenring.exe"): stable between launches.
    pub exe: String,
    /// What people call it: the exe's description ("ELDEN RING"), else its file name.
    pub name: String,
}

pub fn window_pid(hwnd: isize) -> Option<u32> {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(HWND(hwnd as _), Some(&mut pid)) };
    (pid != 0).then_some(pid)
}

/// Full path of a process's exe.
pub fn process_path(pid: u32) -> Option<String> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(process);
        ok.then(|| String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// The "File description" from an exe's version information, if it has a useful one.
pub fn exe_description(path: &str) -> Option<String> {
    unsafe {
        let wide = windows::core::HSTRING::from(path);
        let size = GetFileVersionInfoSizeW(&wide, None);
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(&wide, None, size, data.as_mut_ptr() as *mut _).ok()?;
        // The first language/code page pair names the block that holds the strings.
        let mut ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut len = 0u32;
        if !VerQueryValueW(data.as_ptr() as *const _, windows::core::w!("\\VarFileInfo\\Translation"), &mut ptr, &mut len).as_bool()
            || len < 4
        {
            return None;
        }
        let pair = std::ptr::read_unaligned(ptr as *const [u16; 2]);
        let key = format!("\\StringFileInfo\\{:04x}{:04x}\\FileDescription", pair[0], pair[1]);
        let key = windows::core::HSTRING::from(key);
        if !VerQueryValueW(data.as_ptr() as *const _, &key, &mut ptr, &mut len).as_bool() || len == 0 {
            return None;
        }
        let text = std::slice::from_raw_parts(ptr as *const u16, len as usize);
        let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
        let name = String::from_utf16_lossy(&text[..end]).trim().to_string();
        (!name.is_empty()).then_some(name)
    }
}

fn app_for(pid: u32) -> Option<App> {
    let path = process_path(pid)?;
    let file = std::path::Path::new(&path).file_name()?.to_string_lossy().to_string();
    let stem = std::path::Path::new(&file).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let name = exe_description(&path).unwrap_or(stem);
    Some(App { pid, exe: file.to_lowercase(), name })
}

/// Programs that have a visible window, one entry per exe, in window order.
pub fn apps() -> Vec<App> {
    let mut out: Vec<App> = Vec::new();
    for (hwnd, _) in windows_list() {
        let Some(pid) = window_pid(hwnd) else { continue };
        if out.iter().any(|a| a.pid == pid) {
            continue;
        }
        if let Some(app) = app_for(pid) {
            if !out.iter().any(|a| a.exe == app.exe) {
                out.push(app);
            }
        }
    }
    out
}

/// The program behind a window.
pub fn app_of_window(hwnd: isize) -> Option<App> {
    app_for(window_pid(hwnd)?)
}

/// The first running process with this exe file name (case-insensitive).
pub fn find_process(exe: &str) -> Option<u32> {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut entry = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut found = None;
        let mut more = Process32FirstW(snap, &mut entry).is_ok();
        while more {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            if String::from_utf16_lossy(&entry.szExeFile[..len]).eq_ignore_ascii_case(exe) {
                found = Some(entry.th32ProcessID);
                break;
            }
            more = Process32NextW(snap, &mut entry).is_ok();
        }
        let _ = CloseHandle(snap);
        found
    }
}
