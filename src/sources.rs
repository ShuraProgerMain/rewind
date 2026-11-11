//! What can be recorded: displays and windows.

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::System::Threading::GetCurrentProcessId;
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
