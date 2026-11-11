//! Notification: a small dark card in the top-right corner of the screen the user is
//! looking at. It never takes focus, lets clicks through, and is invisible to screen
//! capture, so it does not end up in the recording. Lives on the tray (UI) thread.

use std::cell::RefCell;
use std::ffi::c_void;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect, GetMonitorInfoW,
    InvalidateRect, MonitorFromWindow, SelectObject, SetBkMode, SetTextColor, CLEARTYPE_QUALITY, DT_CENTER,
    DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HFONT, MONITORINFO, MONITOR_DEFAULTTOPRIMARY,
    PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetClientRect, GetForegroundWindow, KillTimer, RegisterClassExW,
    SetLayeredWindowAttributes, SetTimer, SetWindowDisplayAffinity, SetWindowPos, ShowWindow, HTTRANSPARENT,
    HWND_TOPMOST, LWA_ALPHA, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, WDA_EXCLUDEFROMCAPTURE, WM_NCHITTEST, WM_PAINT,
    WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
    WS_POPUP,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Info,
    Good,
    Record,
    Error,
}

// Design sizes in 96-dpi pixels; scaled by the monitor's DPI.
const WIDTH: i32 = 340;
const HEIGHT: i32 = 64;
const MARGIN: i32 = 16;
const FADE_IN_MS: u64 = 160;
const HOLD_MS: u32 = 3200;
const FADE_OUT_MS: u64 = 260;
const MAX_ALPHA: f32 = 247.0;
const TICK_MS: u32 = 15;

const BACKGROUND: u32 = 0x001F_1F1F; // COLORREF is 0x00BBGGRR
const BORDER: u32 = 0x003A_3A3A;
const TITLE: u32 = 0x00FF_FFFF;
const DETAIL: u32 = 0x00B4_B0AE;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Hidden,
    FadeIn,
    Hold,
    FadeOut,
}

struct Fonts {
    dpi: u32,
    title: HFONT,
    detail: HFONT,
    icon: HFONT,
}

struct State {
    hwnd: HWND,
    title: Vec<u16>,
    detail: Vec<u16>,
    glyph: [u16; 1],
    color: u32,
    phase: Phase,
    since: u64,
    dpi: u32,
    fonts: Option<Fonts>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn scale(v: i32, dpi: u32) -> i32 {
    (v as i64 * dpi as i64 / 96) as i32
}

unsafe fn font(px: i32, weight: i32, face: PCWSTR) -> HFONT {
    CreateFontW(-px, 0, 0, 0, weight, 0, 0, 0, Default::default(), Default::default(), Default::default(), CLEARTYPE_QUALITY, 0, face)
}

unsafe fn create() -> Option<State> {
    let instance = GetModuleHandleW(None).ok()?;
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        lpszClassName: w!("RewindToast"),
        ..Default::default()
    };
    RegisterClassExW(&class);
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT,
        w!("RewindToast"),
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

    // Keep the card out of recordings and screenshots (tests can switch this off).
    if std::env::var_os("REWIND_TOAST_CAPTURABLE").is_none() {
        let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
    }
    // Windows 11 rounds the corners and draws the hairline border for us.
    let round: u32 = 2; // DWMWCP_ROUND
    let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &round as *const u32 as *const c_void, 4);
    let border: u32 = BORDER;
    let _ = DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &border as *const u32 as *const c_void, 4);

    Some(State {
        hwnd,
        title: Vec::new(),
        detail: Vec::new(),
        glyph: [0],
        color: TITLE,
        phase: Phase::Hidden,
        since: 0,
        dpi: 96,
        fonts: None,
    })
}

/// Shows (or replaces) the notification.
pub fn show(tone: Tone, title: &str, detail: &str) {
    STATE.with(|cell| unsafe {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = create();
        }
        let Some(s) = slot.as_mut() else { return };

        // Segoe Fluent Icons glyphs: info, check mark, record, warning.
        let (glyph, color) = match tone {
            Tone::Info => (0xE946, 0x00C8_C4C2),
            Tone::Good => (0xE73E, 0x0058_D130),
            Tone::Record => (0xE7C8, 0x003A_45FF),
            Tone::Error => (0xE7BA, 0x000A_9FFF),
        };
        s.glyph = [glyph];
        s.color = color;
        s.title = title.encode_utf16().collect();
        s.detail = detail.encode_utf16().collect();

        // The screen the user is working on, without the taskbar.
        let monitor = MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTOPRIMARY);
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return;
        }
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        s.dpi = dx.max(48);
        let (w, h, m) = (scale(WIDTH, s.dpi), scale(HEIGHT, s.dpi), scale(MARGIN, s.dpi));
        let x = info.rcWork.right - w - m;
        let y = info.rcWork.top + m;

        // Already on screen: swap the text in place instead of blinking.
        let visible = matches!(s.phase, Phase::FadeIn | Phase::Hold);
        s.phase = if visible { Phase::Hold } else { Phase::FadeIn };
        s.since = GetTickCount64();
        let alpha = if visible { MAX_ALPHA as u8 } else { 0 };
        let _ = SetLayeredWindowAttributes(s.hwnd, COLORREF(0), alpha, LWA_ALPHA);
        let _ = SetWindowPos(s.hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        let _ = InvalidateRect(Some(s.hwnd), None, true);
        SetTimer(Some(s.hwnd), 1, if visible { HOLD_MS } else { TICK_MS }, None);
    });
}

unsafe fn step(s: &mut State) {
    let elapsed = GetTickCount64().saturating_sub(s.since);
    match s.phase {
        Phase::FadeIn => {
            let t = (elapsed as f32 / FADE_IN_MS as f32).min(1.0);
            let eased = 1.0 - (1.0 - t) * (1.0 - t); // ease-out
            let _ = SetLayeredWindowAttributes(s.hwnd, COLORREF(0), (eased * MAX_ALPHA) as u8, LWA_ALPHA);
            if t >= 1.0 {
                s.phase = Phase::Hold;
                SetTimer(Some(s.hwnd), 1, HOLD_MS, None); // one long sleep, no ticking while it just sits there
            }
        }
        Phase::Hold => {
            s.phase = Phase::FadeOut;
            s.since = GetTickCount64();
            SetTimer(Some(s.hwnd), 1, TICK_MS, None);
        }
        Phase::FadeOut => {
            let t = (elapsed as f32 / FADE_OUT_MS as f32).min(1.0);
            let _ = SetLayeredWindowAttributes(s.hwnd, COLORREF(0), ((1.0 - t * t) * MAX_ALPHA) as u8, LWA_ALPHA);
            if t >= 1.0 {
                s.phase = Phase::Hidden;
                let _ = KillTimer(Some(s.hwnd), 1);
                let _ = ShowWindow(s.hwnd, SW_HIDE);
            }
        }
        Phase::Hidden => {
            let _ = KillTimer(Some(s.hwnd), 1);
        }
    }
}

unsafe fn paint(s: &mut State) {
    if s.fonts.as_ref().map(|f| f.dpi) != Some(s.dpi) {
        if let Some(old) = s.fonts.take() {
            let _ = DeleteObject(old.title.into());
            let _ = DeleteObject(old.detail.into());
            let _ = DeleteObject(old.icon.into());
        }
        s.fonts = Some(Fonts {
            dpi: s.dpi,
            title: font(scale(15, s.dpi), 600, w!("Segoe UI Variable Text")),
            detail: font(scale(13, s.dpi), 400, w!("Segoe UI Variable Text")),
            icon: font(scale(20, s.dpi), 400, w!("Segoe Fluent Icons")),
        });
    }
    let Some(fonts) = s.fonts.as_ref() else { return };

    let mut ps = PAINTSTRUCT::default();
    let dc = BeginPaint(s.hwnd, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(s.hwnd, &mut rc);
    let brush = CreateSolidBrush(COLORREF(BACKGROUND));
    FillRect(dc, &rc, brush);
    let _ = DeleteObject(brush.into());
    SetBkMode(dc, TRANSPARENT);

    let d = |v: i32| scale(v, s.dpi);
    let old = SelectObject(dc, fonts.icon.into());
    SetTextColor(dc, COLORREF(s.color));
    let mut icon_rc = RECT { left: d(14), top: 0, right: d(14 + 28), bottom: rc.bottom };
    DrawTextW(dc, &mut s.glyph, &mut icon_rc, DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX);

    let text_left = d(54);
    let text_right = rc.right - d(16);
    let single = DT_SINGLELINE | DT_LEFT | DT_NOPREFIX | DT_END_ELLIPSIS;
    SelectObject(dc, fonts.title.into());
    SetTextColor(dc, COLORREF(TITLE));
    if s.detail.is_empty() {
        let mut r = RECT { left: text_left, top: 0, right: text_right, bottom: rc.bottom };
        DrawTextW(dc, &mut s.title, &mut r, single | DT_VCENTER);
    } else {
        let mid = rc.bottom / 2;
        let mut r = RECT { left: text_left, top: mid - d(21), right: text_right, bottom: mid };
        DrawTextW(dc, &mut s.title, &mut r, single | DT_VCENTER);
        SelectObject(dc, fonts.detail.into());
        SetTextColor(dc, COLORREF(DETAIL));
        let mut r = RECT { left: text_left, top: mid + d(1), right: text_right, bottom: mid + d(20) };
        DrawTextW(dc, &mut s.detail, &mut r, single | DT_VCENTER);
    }
    SelectObject(dc, old);
    let _ = EndPaint(s.hwnd, &ps);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_TIMER | WM_PAINT => {
            let handled = STATE.with(|cell| match cell.try_borrow_mut() {
                Ok(mut slot) => match slot.as_mut() {
                    Some(s) if s.hwnd == hwnd => {
                        if msg == WM_TIMER {
                            step(s)
                        } else {
                            paint(s)
                        }
                        true
                    }
                    _ => false,
                },
                Err(_) => false,
            });
            if handled {
                LRESULT(0)
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
