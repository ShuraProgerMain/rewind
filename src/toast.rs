//! Notification: a small dark card in the top-right corner of the screen the user is
//! looking at. It never takes focus and is invisible to screen capture, so it does not end
//! up in the recording. A plain card lets clicks through; a card with buttons ("Открыть",
//! "В папке", "Обрезать") takes clicks and stays while the pointer is on it. Lives on the
//! tray (UI) thread; a pressed button is posted to the owner window as WM_TOAST_ACTION.

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect, GetMonitorInfoW,
    GetStockObject, GetTextExtentPoint32W, InvalidateRect, MonitorFromWindow, RoundRect, SelectObject, SetBkMode,
    SetTextColor, CLEARTYPE_QUALITY, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER,
    HDC, HFONT, MONITORINFO, MONITOR_DEFAULTTOPRIMARY, NULL_PEN, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
/// Sent after TrackMouseEvent(TME_LEAVE) when the pointer leaves (winuser.h).
const WM_MOUSELEAVE: u32 = 0x02A3;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetClientRect, GetForegroundWindow, GetWindowLongW, KillTimer, PostMessageW,
    RegisterClassExW, SetLayeredWindowAttributes, SetTimer, SetWindowDisplayAffinity, SetWindowLongW, SetWindowPos,
    ShowWindow, GWL_EXSTYLE, HTCLIENT, HTTRANSPARENT, HWND_TOPMOST, LWA_ALPHA, MA_NOACTIVATE, SWP_NOACTIVATE,
    SWP_SHOWWINDOW, SW_HIDE, WDA_EXCLUDEFROMCAPTURE, WM_APP, WM_LBUTTONUP, WM_MOUSEACTIVATE,
    WM_MOUSEMOVE, WM_NCHITTEST, WM_PAINT, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

/// Posted to the owner window when a button is pressed; WPARAM is the button's id.
pub const WM_TOAST_ACTION: u32 = WM_APP + 8;

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
const BUTTONS_HEIGHT: i32 = 36;
const MARGIN: i32 = 16;
const FADE_IN_MS: u64 = 160;
const HOLD_MS: u32 = 3200;
/// A card with buttons stays a little longer: there is something to do with it.
const HOLD_ACTIONS_MS: u32 = 6000;
const FADE_OUT_MS: u64 = 260;
const MAX_ALPHA: f32 = 247.0;
const TICK_MS: u32 = 15;

const BACKGROUND: u32 = 0x001F_1F1F; // COLORREF is 0x00BBGGRR
const BORDER: u32 = 0x003A_3A3A;
const TITLE: u32 = 0x00FF_FFFF;
const DETAIL: u32 = 0x00B4_B0AE;
const BUTTON: u32 = 0x0033_3333;
const BUTTON_HOVER: u32 = 0x0047_4747;

static OWNER: AtomicIsize = AtomicIsize::new(0);

/// The window that receives WM_TOAST_ACTION.
pub fn set_owner(hwnd: HWND) {
    OWNER.store(hwnd.0 as isize, Ordering::Relaxed);
}

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
    button: HFONT,
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
    /// Button labels and ids; empty for a plain card.
    actions: Vec<(Vec<u16>, usize)>,
    /// Where each button is (client coordinates), filled in when painting.
    rects: Vec<RECT>,
    hover: Option<usize>,
    /// The pointer is on the card: it does not fade away meanwhile.
    pointer: bool,
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
        hCursor: windows::Win32::UI::WindowsAndMessaging::LoadCursorW(None, windows::Win32::UI::WindowsAndMessaging::IDC_ARROW)
            .unwrap_or_default(),
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
        actions: Vec::new(),
        rects: Vec::new(),
        hover: None,
        pointer: false,
    })
}

/// Shows (or replaces) a plain notification.
pub fn show(tone: Tone, title: &str, detail: &str) {
    show_with(tone, title, detail, &[]);
}

/// Shows a notification with buttons: (label, id) pairs; a press posts WM_TOAST_ACTION(id).
pub fn show_with(tone: Tone, title: &str, detail: &str, actions: &[(&str, usize)]) {
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
        s.actions = actions.iter().map(|(label, id)| (label.encode_utf16().collect(), *id)).collect();
        s.rects.clear();
        s.hover = None;

        // Clicks go through a plain card, a card with buttons takes them.
        let style = GetWindowLongW(s.hwnd, GWL_EXSTYLE) as u32;
        let wanted = if s.actions.is_empty() { style | WS_EX_TRANSPARENT.0 } else { style & !WS_EX_TRANSPARENT.0 };
        if wanted != style {
            SetWindowLongW(s.hwnd, GWL_EXSTYLE, wanted as i32);
        }

        // The screen the user is working on, without the taskbar.
        let monitor = MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTOPRIMARY);
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return;
        }
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        s.dpi = dx.max(48);
        let height = if s.actions.is_empty() { HEIGHT } else { HEIGHT + BUTTONS_HEIGHT };
        let (w, h, m) = (scale(WIDTH, s.dpi), scale(height, s.dpi), scale(MARGIN, s.dpi));
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
        SetTimer(Some(s.hwnd), 1, if visible { hold_ms(s) } else { TICK_MS }, None);
    });
}

fn hold_ms(s: &State) -> u32 {
    if s.actions.is_empty() {
        HOLD_MS
    } else {
        HOLD_ACTIONS_MS
    }
}

unsafe fn hide(s: &mut State) {
    s.phase = Phase::Hidden;
    s.pointer = false;
    s.hover = None;
    let _ = KillTimer(Some(s.hwnd), 1);
    let _ = ShowWindow(s.hwnd, SW_HIDE);
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
                SetTimer(Some(s.hwnd), 1, hold_ms(s), None); // one long sleep, no ticking meanwhile
            }
        }
        Phase::Hold => {
            if s.pointer {
                SetTimer(Some(s.hwnd), 1, hold_ms(s), None); // the user is about to press something
                return;
            }
            s.phase = Phase::FadeOut;
            s.since = GetTickCount64();
            SetTimer(Some(s.hwnd), 1, TICK_MS, None);
        }
        Phase::FadeOut => {
            if s.pointer {
                // Came back while it was fading: show it fully again.
                s.phase = Phase::Hold;
                let _ = SetLayeredWindowAttributes(s.hwnd, COLORREF(0), MAX_ALPHA as u8, LWA_ALPHA);
                SetTimer(Some(s.hwnd), 1, hold_ms(s), None);
                return;
            }
            let t = (elapsed as f32 / FADE_OUT_MS as f32).min(1.0);
            let _ = SetLayeredWindowAttributes(s.hwnd, COLORREF(0), ((1.0 - t * t) * MAX_ALPHA) as u8, LWA_ALPHA);
            if t >= 1.0 {
                hide(s);
            }
        }
        Phase::Hidden => {
            let _ = KillTimer(Some(s.hwnd), 1);
        }
    }
}

unsafe fn text_width(dc: HDC, text: &[u16]) -> i32 {
    let mut size = SIZE::default();
    let _ = GetTextExtentPoint32W(dc, text, &mut size);
    size.cx
}

unsafe fn paint(s: &mut State) {
    if s.fonts.as_ref().map(|f| f.dpi) != Some(s.dpi) {
        if let Some(old) = s.fonts.take() {
            for f in [old.title, old.detail, old.icon, old.button] {
                let _ = DeleteObject(f.into());
            }
        }
        s.fonts = Some(Fonts {
            dpi: s.dpi,
            title: font(scale(15, s.dpi), 600, w!("Segoe UI Variable Text")),
            detail: font(scale(13, s.dpi), 400, w!("Segoe UI Variable Text")),
            icon: font(scale(20, s.dpi), 400, w!("Segoe Fluent Icons")),
            button: font(scale(13, s.dpi), 500, w!("Segoe UI Variable Text")),
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
    let top = RECT { left: 0, top: 0, right: rc.right, bottom: d(HEIGHT) }; // text part
    let old = SelectObject(dc, fonts.icon.into());
    SetTextColor(dc, COLORREF(s.color));
    let mut icon_rc = RECT { left: d(14), top: 0, right: d(14 + 28), bottom: top.bottom };
    DrawTextW(dc, &mut s.glyph, &mut icon_rc, DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX);

    let text_left = d(54);
    let text_right = rc.right - d(16);
    let single = DT_SINGLELINE | DT_LEFT | DT_NOPREFIX | DT_END_ELLIPSIS;
    SelectObject(dc, fonts.title.into());
    SetTextColor(dc, COLORREF(TITLE));
    if s.detail.is_empty() {
        let mut r = RECT { left: text_left, top: 0, right: text_right, bottom: top.bottom };
        DrawTextW(dc, &mut s.title, &mut r, single | DT_VCENTER);
    } else {
        let mid = top.bottom / 2;
        let mut r = RECT { left: text_left, top: mid - d(21), right: text_right, bottom: mid };
        DrawTextW(dc, &mut s.title, &mut r, single | DT_VCENTER);
        SelectObject(dc, fonts.detail.into());
        SetTextColor(dc, COLORREF(DETAIL));
        let mut r = RECT { left: text_left, top: mid + d(1), right: text_right, bottom: mid + d(20) };
        DrawTextW(dc, &mut s.detail, &mut r, single | DT_VCENTER);
    }

    // Buttons in a row under the text, lined up with it.
    s.rects.clear();
    if !s.actions.is_empty() {
        SelectObject(dc, fonts.button.into());
        let old_pen = SelectObject(dc, GetStockObject(NULL_PEN));
        let mut x = text_left;
        let (y0, y1) = (top.bottom - d(4), top.bottom - d(4) + d(26));
        for (i, (label, _)) in s.actions.iter().enumerate() {
            let w = text_width(dc, label) + d(22);
            let r = RECT { left: x, top: y0, right: x + w, bottom: y1 };
            let fill = CreateSolidBrush(COLORREF(if s.hover == Some(i) { BUTTON_HOVER } else { BUTTON }));
            let old_brush = SelectObject(dc, fill.into());
            let _ = RoundRect(dc, r.left, r.top, r.right + 1, r.bottom + 1, d(12), d(12));
            SelectObject(dc, old_brush);
            let _ = DeleteObject(fill.into());
            SetTextColor(dc, COLORREF(TITLE));
            let mut label_rc = r;
            let mut text = label.clone();
            DrawTextW(dc, &mut text, &mut label_rc, DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX);
            s.rects.push(r);
            x += w + d(6);
        }
        SelectObject(dc, old_pen);
    }
    SelectObject(dc, old);
    let _ = EndPaint(s.hwnd, &ps);
}

fn button_at(s: &State, lparam: LPARAM) -> Option<usize> {
    let p = POINT { x: (lparam.0 & 0xFFFF) as i16 as i32, y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32 };
    s.rects.iter().position(|r| p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let with_state = |f: &mut dyn FnMut(&mut State) -> Option<LRESULT>| -> Option<LRESULT> {
        STATE
            .try_with(|cell| match cell.try_borrow_mut() {
                Ok(mut slot) => match slot.as_mut() {
                    Some(s) if s.hwnd == hwnd => f(s),
                    _ => None,
                },
                Err(_) => None,
            })
            .ok()
            .flatten() // None also while the process is exiting
    };
    let answer = match msg {
        WM_NCHITTEST => with_state(&mut |s| {
            Some(LRESULT(if s.actions.is_empty() { HTTRANSPARENT as isize } else { HTCLIENT as isize }))
        })
        .or(Some(LRESULT(HTTRANSPARENT as isize))),
        // Pressing a button must not take the focus away from the game or program in front.
        WM_MOUSEACTIVATE => Some(LRESULT(MA_NOACTIVATE as isize)),
        WM_MOUSEMOVE => with_state(&mut |s| {
            if !s.pointer {
                s.pointer = true;
                let mut track = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut track);
                if s.phase == Phase::FadeOut {
                    step(s); // back to fully visible at once
                }
            }
            let hover = button_at(s, lparam);
            if hover != s.hover {
                s.hover = hover;
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            Some(LRESULT(0))
        }),
        WM_MOUSELEAVE => with_state(&mut |s| {
            s.pointer = false;
            if s.hover.take().is_some() {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            Some(LRESULT(0))
        }),
        WM_LBUTTONUP => with_state(&mut |s| {
            if let Some(i) = button_at(s, lparam) {
                let id = s.actions[i].1;
                hide(s);
                let owner = HWND(OWNER.load(Ordering::Relaxed) as _);
                let _ = PostMessageW(Some(owner), WM_TOAST_ACTION, WPARAM(id), LPARAM(0));
            }
            Some(LRESULT(0))
        }),
        WM_TIMER => with_state(&mut |s| {
            step(s);
            Some(LRESULT(0))
        }),
        WM_PAINT => with_state(&mut |s| {
            paint(s);
            Some(LRESULT(0))
        }),
        _ => None,
    };
    answer.unwrap_or_else(|| DefWindowProcW(hwnd, msg, wparam, lparam))
}
