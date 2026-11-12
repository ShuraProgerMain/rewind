//! On-screen indicator: a small pill in the top-left corner of the recorded display.
//! "REC 1:23" with a red dot while recording, a quiet dot while only the replay buffer runs.
//! Like the notification card it never takes focus, lets clicks through and is invisible to
//! screen capture, so it is never in the video. Lives on the tray (UI) thread.

use std::cell::RefCell;
use std::ffi::c_void;
use std::time::Instant;

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, Ellipse, EndPaint, FillRect, GetMonitorInfoW,
    GetStockObject, InvalidateRect, MonitorFromWindow, SelectObject, SetBkMode, SetTextColor, CLEARTYPE_QUALITY,
    DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HFONT, HMONITOR, MONITORINFO, MONITOR_DEFAULTTOPRIMARY, NULL_PEN,
    PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetClientRect, KillTimer, RegisterClassExW, SetLayeredWindowAttributes,
    SetTimer, SetWindowDisplayAffinity, SetWindowPos, ShowWindow, HTTRANSPARENT, HWND_TOPMOST, LWA_ALPHA,
    SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, WDA_EXCLUDEFROMCAPTURE, WM_NCHITTEST, WM_PAINT, WM_TIMER, WNDCLASSEXW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Show {
    Hidden,
    /// The replay buffer runs (nothing is being written to a file).
    Buffer,
    /// A recording is being written.
    Recording,
}

const MARGIN: i32 = 12;
const HEIGHT: i32 = 26;
const BACKGROUND: u32 = 0x001C_1C1E;
const RED: u32 = 0x003A_45FF;
const QUIET: u32 = 0x00A1_9C9C;
const TEXT: u32 = 0x00FF_FFFF;

struct State {
    hwnd: HWND,
    show: Show,
    since: Instant,
    monitor: isize,
    dpi: u32,
    font: Option<(u32, HFONT)>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn scale(v: i32, dpi: u32) -> i32 {
    (v as i64 * dpi as i64 / 96) as i32
}

unsafe fn create() -> Option<State> {
    let instance = GetModuleHandleW(None).ok()?;
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        lpszClassName: w!("RewindIndicator"),
        ..Default::default()
    };
    RegisterClassExW(&class);
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT,
        w!("RewindIndicator"),
        w!(""),
        WS_POPUP,
        0,
        0,
        0,
        0,
        None,
        None,
        Some(instance.into()),
        None,
    )
    .ok()?;
    if std::env::var_os("REWIND_TOAST_CAPTURABLE").is_none() {
        let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
    }
    let round: u32 = 2; // DWMWCP_ROUND
    let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &round as *const u32 as *const c_void, 4);
    let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 225, LWA_ALPHA);
    Some(State { hwnd, show: Show::Hidden, since: Instant::now(), monitor: 0, dpi: 96, font: None })
}

fn label(s: &State) -> String {
    match s.show {
        Show::Recording => {
            let secs = s.since.elapsed().as_secs();
            if secs >= 3600 {
                format!("REC {}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
            } else {
                format!("REC {}:{:02}", secs / 60, secs % 60)
            }
        }
        Show::Buffer => "Буфер".to_string(),
        Show::Hidden => String::new(),
    }
}

/// Shows the indicator in the given state on the given display (raw HMONITOR, 0 = primary).
pub fn set(show: Show, monitor: isize) {
    STATE.with(|cell| unsafe {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            if show == Show::Hidden {
                return;
            }
            *slot = create();
        }
        let Some(s) = slot.as_mut() else { return };
        if show == Show::Hidden {
            s.show = show;
            let _ = KillTimer(Some(s.hwnd), 1);
            let _ = ShowWindow(s.hwnd, SW_HIDE);
            return;
        }
        if show != s.show {
            s.since = Instant::now(); // the timer counts from the start of the recording
        }
        s.show = show;
        s.monitor = monitor;
        place(s);
        // Only a running timer needs repainting, once a second.
        if show == Show::Recording {
            SetTimer(Some(s.hwnd), 1, 1000, None);
        } else {
            let _ = KillTimer(Some(s.hwnd), 1);
        }
    });
}

unsafe fn place(s: &mut State) {
    let monitor = if s.monitor != 0 {
        HMONITOR(s.monitor as _)
    } else {
        MonitorFromWindow(HWND::default(), MONITOR_DEFAULTTOPRIMARY)
    };
    let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    if !GetMonitorInfoW(monitor, &mut info).as_bool() {
        return;
    }
    let (mut dx, mut dy) = (96u32, 96u32);
    let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
    s.dpi = dx.max(48);
    let width = match s.show {
        Show::Recording => 96,
        _ => 72,
    };
    let (w, h, m) = (scale(width, s.dpi), scale(HEIGHT, s.dpi), scale(MARGIN, s.dpi));
    let _ = SetWindowPos(
        s.hwnd,
        Some(HWND_TOPMOST),
        info.rcMonitor.left + m,
        info.rcMonitor.top + m,
        w,
        h,
        SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );
    let _ = InvalidateRect(Some(s.hwnd), None, true);
}

unsafe fn paint(s: &mut State) {
    if s.font.map(|(d, _)| d) != Some(s.dpi) {
        if let Some((_, old)) = s.font.take() {
            let _ = DeleteObject(old.into());
        }
        let font = CreateFontW(
            -scale(12, s.dpi), 0, 0, 0, 600, 0, 0, 0,
            Default::default(), Default::default(), Default::default(), CLEARTYPE_QUALITY, 0,
            w!("Segoe UI Variable Text"),
        );
        s.font = Some((s.dpi, font));
    }
    let mut ps = PAINTSTRUCT::default();
    let dc = BeginPaint(s.hwnd, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(s.hwnd, &mut rc);
    let bg = CreateSolidBrush(COLORREF(BACKGROUND));
    FillRect(dc, &rc, bg);
    let _ = DeleteObject(bg.into());

    let d = |v: i32| scale(v, s.dpi);
    let dot = CreateSolidBrush(COLORREF(if s.show == Show::Recording { RED } else { QUIET }));
    let old_brush = SelectObject(dc, dot.into());
    let old_pen = SelectObject(dc, GetStockObject(NULL_PEN));
    let cy = rc.bottom / 2;
    let _ = Ellipse(dc, d(10), cy - d(4), d(10) + d(9), cy - d(4) + d(9));
    SelectObject(dc, old_pen);
    SelectObject(dc, old_brush);
    let _ = DeleteObject(dot.into());

    if let Some((_, font)) = s.font {
        let old = SelectObject(dc, font.into());
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(if s.show == Show::Recording { TEXT } else { QUIET }));
        let mut text: Vec<u16> = label(s).encode_utf16().collect();
        let mut r = RECT { left: d(26), top: 0, right: rc.right - d(8), bottom: rc.bottom };
        DrawTextW(dc, &mut text, &mut r, DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_NOPREFIX);
        SelectObject(dc, old);
    }
    let _ = EndPaint(s.hwnd, &ps);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_TIMER | WM_PAINT => {
            let handled = STATE
                .try_with(|cell| match cell.try_borrow_mut() {
                    Ok(mut slot) => match slot.as_mut() {
                        Some(s) if s.hwnd == hwnd => {
                            if msg == WM_TIMER {
                                let _ = InvalidateRect(Some(hwnd), None, false);
                            } else {
                                paint(s);
                            }
                            true
                        }
                        _ => false,
                    },
                    Err(_) => false,
                })
                .unwrap_or(false);
            if handled {
                LRESULT(0)
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
