//! The title box: a small field over the game for naming a marker that was just placed.
//! The marker itself is already stored when the box appears (its moment is the key press),
//! so the box only adds a title. Enter keeps the text, Esc leaves the marker untitled,
//! clicking elsewhere keeps what was typed. Focus goes back to the game afterwards.
//! Invisible to screen capture, like the notifications. Lives on the tray (UI) thread.

use std::cell::RefCell;
use std::ffi::c_void;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect, GetMonitorInfoW,
    MonitorFromWindow, SelectObject, SetBkColor, SetBkMode, SetTextColor, CLEARTYPE_QUALITY, DT_END_ELLIPSIS,
    DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HBRUSH, HDC, HFONT, HGDIOBJ, MONITORINFO,
    MONITOR_DEFAULTTOPRIMARY, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CallWindowProcW, CreateWindowExW, DefWindowProcW, GetForegroundWindow, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsWindow, PostMessageW, RegisterClassExW, SendMessageW,
    SetForegroundWindow, SetWindowDisplayAffinity, SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow,
    ES_AUTOHSCROLL, GWLP_WNDPROC, HMENU, HWND_TOPMOST, SWP_SHOWWINDOW, SW_HIDE, WA_INACTIVE,
    WDA_EXCLUDEFROMCAPTURE, WM_ACTIVATE, WM_APP, WM_CHAR, WM_CTLCOLOREDIT, WM_KEYDOWN, WM_PAINT, WM_SETFONT,
    WNDCLASSEXW, WNDPROC, WS_CHILD, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};

/// Posted to the owner when the box closes; `take()` then gives the marker id and title
/// (None = closed without a title).
pub const WM_NOTE_DONE: u32 = WM_APP + 14;

const WIDTH: i32 = 440;
const HEIGHT: i32 = 116;
/// Where the field starts, below the heading.
const FIELD_TOP: i32 = 44;
const PAD: i32 = 16;
const FIELD_HEIGHT: i32 = 34;
const BACKGROUND: u32 = 0x001F_1F1F; // COLORREF is 0x00BBGGRR
const FIELD: u32 = 0x002C_2C2C;
const BORDER: u32 = 0x003A_3A3A;
const TITLE: u32 = 0x00FF_FFFF;
const DETAIL: u32 = 0x00B4_B0AE;

struct State {
    hwnd: HWND,
    edit: HWND,
    owner: HWND,
    /// The window that had focus before (the game), to give it back.
    previous: HWND,
    id: Option<u64>,
    heading: Vec<u16>,
    dpi: u32,
    /// Heading, field text, hint.
    fonts: Option<(HFONT, HFONT, HFONT)>,
    field_brush: HBRUSH,
    old_edit_proc: WNDPROC,
    /// Boxes closed and not yet collected by the owner (one may close as the next opens).
    results: Vec<(u64, Option<String>)>,
    /// When the box opened (ms tick): focus shuffles right after opening are not "clicked away".
    opened_at: u64,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn scale(v: i32, dpi: u32) -> i32 {
    (v as i64 * dpi as i64 / 96) as i32
}

unsafe fn font(px: i32, weight: i32) -> HFONT {
    CreateFontW(-px, 0, 0, 0, weight, 0, 0, 0, Default::default(), Default::default(), Default::default(), CLEARTYPE_QUALITY, 0, w!("Segoe UI"))
}

unsafe fn create(owner: HWND) -> Option<State> {
    let instance = GetModuleHandleW(None).ok()?;
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        lpszClassName: w!("RewindNote"),
        ..Default::default()
    };
    RegisterClassExW(&class);
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        w!("RewindNote"),
        w!("Заголовок метки"),
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
    let edit = CreateWindowExW(
        Default::default(),
        w!("EDIT"),
        w!(""),
        WS_CHILD | WS_VISIBLE | windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(ES_AUTOHSCROLL as u32),
        0,
        0,
        0,
        0,
        Some(hwnd),
        Some(HMENU(1 as _)),
        Some(instance.into()),
        None,
    )
    .ok()?;
    SendMessageW(edit, 0x00C5 /* EM_LIMITTEXT */, Some(WPARAM(200)), None);
    // Enter and Esc reach a single-line field as key presses only: watch them there.
    let old: isize = SetWindowLongPtrW(edit, GWLP_WNDPROC, edit_proc as *const () as usize as isize);
    let old_edit_proc: WNDPROC = std::mem::transmute::<isize, WNDPROC>(old);

    if std::env::var_os("REWIND_TOAST_CAPTURABLE").is_none() {
        let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
    }
    let round: u32 = 2; // DWMWCP_ROUND
    let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &round as *const u32 as *const c_void, 4);
    let border: u32 = BORDER;
    let _ = DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &border as *const u32 as *const c_void, 4);
    Some(State {
        hwnd,
        edit,
        owner,
        previous: HWND::default(),
        id: None,
        heading: Vec::new(),
        dpi: 96,
        fonts: None,
        field_brush: CreateSolidBrush(COLORREF(FIELD)),
        old_edit_proc,
        results: Vec::new(),
        opened_at: 0,
    })
}

/// Shows the box for marker `id`; `heading` says which marker ("Метка 3 · 12:41").
/// A box that is already open is closed first, keeping what was typed there.
pub fn open(owner: HWND, id: u64, heading: &str) {
    if is_open() {
        finish(true);
    }
    // Window calls below can send messages straight back to this module's window procedures,
    // so the state is only borrowed to read and write fields, never across such a call.
    let ready = STATE.with(|cell| cell.borrow().is_some());
    if !ready {
        let made = unsafe { create(owner) };
        STATE.with(|cell| *cell.borrow_mut() = made);
    }
    let plan = STATE.with(|cell| unsafe {
        let mut slot = cell.borrow_mut();
        let s = slot.as_mut()?;
        s.owner = owner;
        s.id = Some(id);
        s.opened_at = windows::Win32::System::SystemInformation::GetTickCount64();
        s.heading = heading.encode_utf16().collect();
        s.previous = GetForegroundWindow();

        // Top middle of the screen the game is on.
        let monitor = MonitorFromWindow(s.previous, MONITOR_DEFAULTTOPRIMARY);
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            s.id = None;
            return None;
        }
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        if s.dpi != dx.max(48) || s.fonts.is_none() {
            s.dpi = dx.max(48);
            if let Some((a, b, c)) = s.fonts.take() {
                let _ = DeleteObject(HGDIOBJ(a.0));
                let _ = DeleteObject(HGDIOBJ(b.0));
                let _ = DeleteObject(HGDIOBJ(c.0));
            }
            s.fonts = Some((font(scale(14, s.dpi), 600), font(scale(15, s.dpi), 400), font(scale(12, s.dpi), 400)));
        }
        let (w, h) = (scale(WIDTH, s.dpi), scale(HEIGHT, s.dpi));
        let x = (info.rcMonitor.left + info.rcMonitor.right - w) / 2;
        let y = info.rcMonitor.top + scale(48, s.dpi);
        let (pad, field) = (scale(PAD, s.dpi), scale(FIELD_HEIGHT, s.dpi));
        let inner = scale(8, s.dpi);
        let line = scale(20, s.dpi);
        let text_font = s.fonts.map(|(_, f, _)| f).unwrap_or_default();
        // The field: the edit control sits inside a drawn rounded box, padded.
        let edit_rc = (pad + inner, scale(FIELD_TOP, s.dpi) + (field - line) / 2, w - 2 * (pad + inner), line);
        Some((s.hwnd, s.edit, text_font, edit_rc, (x, y, w, h)))
    });
    let Some((hwnd, edit, text_font, (ex, ey, ew, eh), (x, y, w, h))) = plan else { return };
    unsafe {
        SendMessageW(edit, WM_SETFONT, Some(WPARAM(text_font.0 as usize)), Some(LPARAM(1)));
        let _ = SetWindowPos(edit, None, ex, ey, ew, eh, Default::default());
        let _ = SetWindowTextW(edit, w!(""));
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_SHOWWINDOW);
        take_focus(hwnd);
        let _ = SetFocus(Some(edit));
    }
}

/// Windows lets a background program take focus only in some cases; joining the input of
/// the window in front for a moment is the accepted way when a hotkey asked for it.
unsafe fn take_focus(hwnd: HWND) {
    if SetForegroundWindow(hwnd).as_bool() && GetForegroundWindow() == hwnd {
        return;
    }
    let front = GetForegroundWindow();
    let theirs = GetWindowThreadProcessId(front, None);
    let ours = GetCurrentThreadId();
    let joined = theirs != 0 && theirs != ours && AttachThreadInput(ours, theirs, true).as_bool();
    let _ = BringWindowToTop(hwnd);
    let _ = SetForegroundWindow(hwnd);
    if joined {
        let _ = AttachThreadInput(ours, theirs, false);
    }
}

pub fn is_open() -> bool {
    STATE.with(|cell| cell.borrow().as_ref().is_some_and(|s| s.id.is_some()))
}

/// Closes the box. `keep`: store what was typed (Enter, focus lost); else no title (Esc).
fn finish(keep: bool) {
    // Taking the id first makes the box count as closed for the messages the calls below cause.
    let open = STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let s = slot.as_mut()?;
        let id = s.id.take()?;
        Some((id, s.hwnd, s.edit, s.previous, s.owner))
    });
    let Some((id, hwnd, edit, previous, owner)) = open else { return };
    unsafe {
        let mut text = None;
        if keep {
            let len = GetWindowTextLengthW(edit);
            let mut buf = vec![0u16; len as usize + 1];
            let got = GetWindowTextW(edit, &mut buf);
            let typed = String::from_utf16_lossy(&buf[..got.max(0) as usize]).trim().to_string();
            if !typed.is_empty() {
                text = Some(typed);
            }
        }
        STATE.with(|cell| {
            if let Some(s) = cell.borrow_mut().as_mut() {
                s.results.push((id, text));
            }
        });
        let _ = ShowWindow(hwnd, SW_HIDE);
        // Back to the game.
        if !previous.is_invalid() && IsWindow(Some(previous)).as_bool() {
            let _ = SetForegroundWindow(previous);
        }
        let _ = PostMessageW(Some(owner), WM_NOTE_DONE, WPARAM(0), LPARAM(0));
    }
}

/// The outcome of the boxes that closed: marker id and title (None = no title).
pub fn take() -> Vec<(u64, Option<String>)> {
    STATE.with(|cell| cell.borrow_mut().as_mut().map(|s| std::mem::take(&mut s.results)).unwrap_or_default())
}

/// Test hook: types `text` into the open box and presses Enter.
pub fn submit(text: &str) {
    let edit = STATE.with(|cell| cell.borrow().as_ref().filter(|s| s.id.is_some()).map(|s| s.edit));
    if let Some(edit) = edit {
        let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        unsafe {
            let _ = SetWindowTextW(edit, PCWSTR(wide.as_ptr()));
        }
        finish(true);
    }
}

unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let dc: HDC = BeginPaint(hwnd, &mut ps);
    STATE.with(|cell| {
        let slot = cell.borrow();
        let Some(s) = slot.as_ref() else { return };
        let mut rc = RECT::default();
        let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rc);
        let bg = CreateSolidBrush(COLORREF(BACKGROUND));
        FillRect(dc, &rc, bg);
        let _ = DeleteObject(HGDIOBJ(bg.0));
        let (pad, field) = (scale(PAD, s.dpi), scale(FIELD_HEIGHT, s.dpi));
        let Some((head_font, _, hint_font)) = s.fonts else { return };
        SetBkMode(dc, TRANSPARENT);

        // Heading above the field, the keys below it.
        let top = RECT { left: pad, top: pad, right: rc.right - pad, bottom: pad + scale(20, s.dpi) };
        let old = SelectObject(dc, HGDIOBJ(head_font.0));
        SetTextColor(dc, COLORREF(TITLE));
        let mut heading = s.heading.clone();
        DrawTextW(dc, &mut heading, &mut top.clone(), DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS);
        let field_top = scale(FIELD_TOP, s.dpi);
        let field_rc = RECT { left: pad, top: field_top, right: rc.right - pad, bottom: field_top + field };
        SelectObject(dc, HGDIOBJ(hint_font.0));
        SetTextColor(dc, COLORREF(DETAIL));
        let mut hint: Vec<u16> = "Enter — сохранить · Esc — без заголовка".encode_utf16().collect();
        let mut hint_rc = RECT { left: pad + scale(2, s.dpi), top: field_rc.bottom + scale(6, s.dpi), right: rc.right - pad, bottom: rc.bottom - scale(8, s.dpi) };
        DrawTextW(dc, &mut hint, &mut hint_rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
        // The field's box.
        let pen_brush = s.field_brush;
        let old_brush = SelectObject(dc, HGDIOBJ(pen_brush.0));
        let old_pen = SelectObject(dc, windows::Win32::Graphics::Gdi::GetStockObject(windows::Win32::Graphics::Gdi::NULL_PEN));
        let r = scale(8, s.dpi);
        let _ = windows::Win32::Graphics::Gdi::RoundRect(dc, field_rc.left, field_rc.top, field_rc.right + 1, field_rc.bottom + 1, r, r);
        SelectObject(dc, old_pen);
        SelectObject(dc, old_brush);
        SelectObject(dc, old);
    });
    let _ = EndPaint(hwnd, &ps);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_CTLCOLOREDIT => {
            let dc = HDC(wparam.0 as _);
            SetTextColor(dc, COLORREF(TITLE));
            SetBkColor(dc, COLORREF(FIELD));
            let brush = STATE.with(|cell| cell.borrow().as_ref().map(|s| s.field_brush)).unwrap_or_default();
            LRESULT(brush.0 as isize)
        }
        WM_ACTIVATE => {
            // Clicked away: keep what was typed.
            if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE && is_open() {
                let opened = STATE.with(|cell| cell.borrow().as_ref().map_or(0, |s| s.opened_at));
                let age = windows::Win32::System::SystemInformation::GetTickCount64().saturating_sub(opened);
                // Test scripts start a process per command, which takes the focus: with
                // REWIND_DEBUG the box waits for Enter/Esc (or the test hook) instead.
                if std::env::var_os("REWIND_DEBUG").is_some() {
                    return LRESULT(0);
                }
                if age < 600 {
                    // Windows is still shuffling focus after the box appeared: take it back.
                    let edit = STATE.with(|cell| cell.borrow().as_ref().map(|s| s.edit));
                    take_focus(hwnd);
                    if let Some(edit) = edit {
                        let _ = SetFocus(Some(edit));
                    }
                } else {
                    finish(true);
                }
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe extern "system" fn edit_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_KEYDOWN && (wparam.0 == VK_RETURN.0 as usize || wparam.0 == VK_ESCAPE.0 as usize) {
        finish(wparam.0 == VK_RETURN.0 as usize);
        return LRESULT(0);
    }
    if msg == WM_CHAR && (wparam.0 == 13 || wparam.0 == 27) {
        return LRESULT(0); // no beep
    }
    let old = STATE.with(|cell| cell.borrow().as_ref().and_then(|s| s.old_edit_proc));
    CallWindowProcW(old, hwnd, msg, wparam, lparam)
}
