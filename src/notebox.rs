//! The title card: a small card over the game for naming a marker that was just placed.
//! The marker itself is already stored when the card appears (its moment is the key press),
//! so the card only adds a title. Invisible to screen capture, like the notifications.
//! Lives on the tray (UI) thread.
//!
//! One card, whatever opened it: a text field, and under it the preset titles and "no
//! title". Picking a row (the same gamepad combination again, or ↑/↓) puts its text into the
//! field; what is in the field when the card closes becomes the title.
//! * Opened from the gamepad, the card does not take the focus (the game keeps running and
//!   keeps its input): the first preset is already in the field, A takes it at once, and
//!   after a few seconds without presses it is taken anyway.
//! * Opened from the keyboard, or clicked, or asked for with the keyboard shortcut while it
//!   is up, the field takes the focus and the countdown stops: type, Enter keeps it, Esc
//!   leaves the marker untitled, clicking elsewhere keeps what was typed. Focus then goes
//!   back to the game.

use std::cell::RefCell;
use std::ffi::c_void;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect, GetMonitorInfoW,
    GetStockObject, InvalidateRect, MonitorFromWindow, RoundRect, SelectObject, SetBkColor, SetBkMode, SetTextColor,
    CLEARTYPE_QUALITY, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HBRUSH, HDC, HFONT, HGDIOBJ,
    MONITORINFO, MONITOR_DEFAULTTOPRIMARY, NULL_PEN, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_DOWN, VK_ESCAPE, VK_RETURN, VK_UP};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CallWindowProcW, CreateWindowExW, DefWindowProcW, GetClientRect, GetForegroundWindow,
    GetWindowLongW, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindow, KillTimer, PostMessageW,
    RegisterClassExW, SendMessageW, SetForegroundWindow, SetTimer, SetWindowDisplayAffinity, SetWindowLongPtrW,
    SetWindowLongW, SetWindowPos, SetWindowTextW, ShowWindow, ES_AUTOHSCROLL, GWLP_WNDPROC, GWL_EXSTYLE, HMENU,
    HWND_TOPMOST, MA_ACTIVATE, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, WA_INACTIVE, WDA_EXCLUDEFROMCAPTURE,
    WM_ACTIVATE, WM_APP, WM_CHAR, WM_CTLCOLOREDIT, WM_KEYDOWN, WM_MOUSEACTIVATE, WM_PAINT, WM_SETFONT, WM_TIMER,
    WNDCLASSEXW, WNDPROC, WS_CHILD, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};

/// Posted to the owner when the card closes; `take()` then gives the marker id and title
/// (None = closed without a title).
pub const WM_NOTE_DONE: u32 = WM_APP + 14;

const WIDTH: i32 = 440;
const PAD: i32 = 16;
const HEADING: i32 = 20;
const FIELD_HEIGHT: i32 = 34;
const ROW: i32 = 28;
const HINT: i32 = 18;
const GAP: i32 = 8;
/// Opened from the gamepad: the field's title is taken after this long without a press.
const PICK_MS: u32 = 3000;
const TIMER_PICK: usize = 1;
const TIMER_BAR: usize = 2;

const BACKGROUND: u32 = 0x001F_1F1F; // COLORREF is 0x00BBGGRR
const FIELD: u32 = 0x002C_2C2C;
const BORDER: u32 = 0x003A_3A3A;
const TITLE: u32 = 0x00FF_FFFF;
const DETAIL: u32 = 0x00B4_B0AE;
const ACCENT: u32 = 0x00FF_840A; // #0a84ff

struct State {
    hwnd: HWND,
    edit: HWND,
    owner: HWND,
    /// The window that had focus before (the game), to give it back.
    previous: HWND,
    id: Option<u64>,
    heading: Vec<u16>,
    dpi: u32,
    /// Heading, field and list text, hint.
    fonts: Option<(HFONT, HFONT, HFONT)>,
    field_brush: HBRUSH,
    old_edit_proc: WNDPROC,
    /// Cards closed and not yet collected by the owner (one may close as the next opens).
    results: Vec<(u64, Option<String>)>,
    presets: Vec<String>,
    /// Highlighted row: a preset, or `presets.len()` for "no title".
    selected: Option<usize>,
    /// The field has the focus (we took it from the game and give it back at the end).
    focused: bool,
    /// When the field took the focus (ms tick): focus shuffles right after are expected.
    focused_at: u64,
    /// Opened from the gamepad and not touched by the keyboard yet: the countdown runs.
    countdown: bool,
    /// When the countdown last started (ms tick), for its bar.
    stepped_at: u64,
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

/// Rows under the field: the presets and "no title" (none at all without presets).
fn rows(s: &State) -> usize {
    if s.presets.is_empty() {
        0
    } else {
        s.presets.len() + 1
    }
}

/// The title a row stands for ("no title" is empty).
fn row_text(s: &State, i: usize) -> String {
    s.presets.get(i).cloned().unwrap_or_default()
}

/// Where things go, in design pixels: (field top, list top, hint top, height).
fn layout(rows: usize) -> (i32, i32, i32, i32) {
    let field_top = PAD + HEADING + GAP;
    let list_top = field_top + FIELD_HEIGHT + GAP;
    let hint_top = list_top + rows as i32 * ROW + if rows > 0 { GAP / 2 } else { 0 };
    (field_top, list_top, hint_top, hint_top + HINT + PAD - 4)
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
    // Enter, Esc and the arrows reach a single-line field as key presses only: watch them there.
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
        presets: Vec::new(),
        selected: None,
        focused: false,
        focused_at: 0,
        countdown: false,
        stepped_at: 0,
    })
}

/// Shows the card for marker `id`; `heading` says which marker ("Метка 3 · 12:41").
/// `from_pad`: the gamepad asked for it (no focus taken, countdown; see the module notes).
/// A card that is already open is closed first, keeping what was in its field.
pub fn open(owner: HWND, id: u64, heading: &str, presets: &[String], from_pad: bool) {
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
    let presets: Vec<String> = presets.iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).take(8).collect();
    let plan = STATE.with(|cell| unsafe {
        let mut slot = cell.borrow_mut();
        let s = slot.as_mut()?;
        s.owner = owner;
        s.id = Some(id);
        s.heading = heading.encode_utf16().collect();
        s.previous = GetForegroundWindow();
        s.presets = presets;
        s.focused = false;
        s.countdown = from_pad && !s.presets.is_empty();
        s.selected = if s.countdown { Some(0) } else { None };
        s.stepped_at = GetTickCount64();

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
        let (field_top, _, _, height) = layout(rows(s));
        let (w, h) = (scale(WIDTH, s.dpi), scale(height, s.dpi));
        let x = (info.rcMonitor.left + info.rcMonitor.right - w) / 2;
        let y = info.rcMonitor.top + scale(48, s.dpi);
        let (pad, field) = (scale(PAD, s.dpi), scale(FIELD_HEIGHT, s.dpi));
        let inner = scale(8, s.dpi);
        let line = scale(20, s.dpi);
        let text_font = s.fonts.map(|(_, f, _)| f).unwrap_or_default();
        // The field: the edit control sits inside a drawn rounded box, padded.
        let edit_rc = (pad + inner, scale(field_top, s.dpi) + (field - line) / 2, w - 2 * (pad + inner), line);
        let first = s.selected.map(|i| row_text(s, i)).unwrap_or_default();
        Some((s.hwnd, s.edit, text_font, edit_rc, (x, y, w, h), s.countdown, first))
    });
    let Some((hwnd, edit, text_font, (ex, ey, ew, eh), (x, y, w, h), countdown, first)) = plan else { return };
    unsafe {
        SendMessageW(edit, WM_SETFONT, Some(WPARAM(text_font.0 as usize)), Some(LPARAM(1)));
        let _ = SetWindowPos(edit, None, ex, ey, ew, eh, Default::default());
        set_field(edit, &first);
        let _ = InvalidateRect(Some(hwnd), None, true);
        if countdown {
            // Shown without taking the focus from the game, not even when clicked (a click
            // asks for the field instead, see WM_MOUSEACTIVATE).
            let style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
            SetWindowLongW(hwnd, GWL_EXSTYLE, (style | WS_EX_NOACTIVATE.0) as i32);
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_SHOWWINDOW | SWP_NOACTIVATE);
            SetTimer(Some(hwnd), TIMER_PICK, PICK_MS, None);
            SetTimer(Some(hwnd), TIMER_BAR, 50, None);
        } else {
            let _ = KillTimer(Some(hwnd), TIMER_PICK);
            let _ = KillTimer(Some(hwnd), TIMER_BAR);
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_SHOWWINDOW | SWP_NOACTIVATE);
        }
    }
    if !countdown {
        focus_field();
    }
}

unsafe fn set_field(edit: HWND, text: &str) {
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    let _ = SetWindowTextW(edit, PCWSTR(wide.as_ptr()));
    let end = wide.len() - 1;
    SendMessageW(edit, 0x00B1 /* EM_SETSEL */, Some(WPARAM(end)), Some(LPARAM(end as isize)));
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

/// The field takes the focus to be typed in; the countdown stops. Used when the card is
/// opened from the keyboard, clicked, or asked for with the keyboard shortcut while it is up.
pub fn focus_field() {
    let plan = STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let s = slot.as_mut().filter(|s| s.id.is_some())?;
        s.countdown = false;
        s.focused = true;
        s.focused_at = unsafe { GetTickCount64() };
        Some((s.hwnd, s.edit))
    });
    let Some((hwnd, edit)) = plan else { return };
    unsafe {
        let _ = KillTimer(Some(hwnd), TIMER_PICK);
        let _ = KillTimer(Some(hwnd), TIMER_BAR);
        let style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        if style & WS_EX_NOACTIVATE.0 != 0 {
            SetWindowLongW(hwnd, GWL_EXSTYLE, (style & !WS_EX_NOACTIVATE.0) as i32);
        }
        take_focus(hwnd);
        let _ = SetFocus(Some(edit));
        let _ = InvalidateRect(Some(hwnd), None, true);
    }
}

/// Highlights the row `delta` away (wrapping) and puts its title into the field; on a card
/// opened from the gamepad the countdown starts again.
pub fn step(delta: i32) {
    let plan = STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let s = slot.as_mut().filter(|s| s.id.is_some())?;
        let count = rows(s) as i32;
        if count == 0 {
            return None;
        }
        let i = match s.selected {
            Some(i) => (i as i32 + delta).rem_euclid(count),
            None if delta > 0 => 0,
            None => count - 1,
        } as usize;
        s.selected = Some(i);
        s.stepped_at = unsafe { GetTickCount64() };
        Some((s.hwnd, s.edit, row_text(s, i), s.countdown))
    });
    if let Some((hwnd, edit, text, countdown)) = plan {
        unsafe {
            set_field(edit, &text);
            if countdown {
                SetTimer(Some(hwnd), TIMER_PICK, PICK_MS, None);
            }
            let _ = InvalidateRect(Some(hwnd), None, true);
        }
    }
}

/// Keeps what is in the field now (gamepad A).
pub fn choose() {
    if is_open() {
        finish(true);
    }
}

/// Closes the card. `keep`: the field's text becomes the title (Enter, A, countdown, focus
/// lost); else no title (Esc).
fn finish(keep: bool) {
    // Taking the id first makes the card count as closed for the messages the calls below cause.
    let open = STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let s = slot.as_mut()?;
        let id = s.id.take()?;
        let focused = std::mem::take(&mut s.focused);
        s.countdown = false;
        Some((id, s.hwnd, s.edit, s.previous, s.owner, focused))
    });
    let Some((id, hwnd, edit, previous, owner, focused)) = open else { return };
    unsafe {
        let _ = KillTimer(Some(hwnd), TIMER_PICK);
        let _ = KillTimer(Some(hwnd), TIMER_BAR);
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
        // Back to the game, if the card took the focus from it.
        if focused && !previous.is_invalid() && IsWindow(Some(previous)).as_bool() {
            let _ = SetForegroundWindow(previous);
        }
        let _ = PostMessageW(Some(owner), WM_NOTE_DONE, WPARAM(0), LPARAM(0));
    }
}

/// The outcome of the cards that closed: marker id and title (None = no title).
pub fn take() -> Vec<(u64, Option<String>)> {
    STATE.with(|cell| cell.borrow_mut().as_mut().map(|s| std::mem::take(&mut s.results)).unwrap_or_default())
}

/// Test hook: types `text` into the open card and presses Enter.
pub fn submit(text: &str) {
    let edit = STATE.with(|cell| cell.borrow().as_ref().filter(|s| s.id.is_some()).map(|s| s.edit));
    if let Some(edit) = edit {
        unsafe { set_field(edit, text) };
        finish(true);
    }
}

unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let dc: HDC = BeginPaint(hwnd, &mut ps);
    STATE.with(|cell| {
        let slot = cell.borrow();
        let Some(s) = slot.as_ref() else { return };
        let d = |v: i32| scale(v, s.dpi);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let bg = CreateSolidBrush(COLORREF(BACKGROUND));
        FillRect(dc, &rc, bg);
        let _ = DeleteObject(HGDIOBJ(bg.0));
        let Some((head_font, text_font, hint_font)) = s.fonts else { return };
        SetBkMode(dc, TRANSPARENT);
        let count = rows(s);
        let (field_top, list_top, hint_top, _) = layout(count);
        let flags = DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS;

        let old = SelectObject(dc, HGDIOBJ(head_font.0));
        SetTextColor(dc, COLORREF(TITLE));
        let mut heading = s.heading.clone();
        let mut top = RECT { left: d(PAD), top: d(PAD), right: rc.right - d(PAD), bottom: d(PAD + HEADING) };
        DrawTextW(dc, &mut heading, &mut top, flags);

        let old_pen = SelectObject(dc, GetStockObject(NULL_PEN));
        let old_brush = SelectObject(dc, HGDIOBJ(s.field_brush.0));
        let r = d(8);
        let _ = RoundRect(dc, d(PAD), d(field_top), rc.right - d(PAD) + 1, d(field_top + FIELD_HEIGHT) + 1, r, r);
        // The presets and "no title", the highlighted one marked.
        SelectObject(dc, HGDIOBJ(text_font.0));
        let accent = CreateSolidBrush(COLORREF(ACCENT));
        for i in 0..count {
            let y = d(list_top + i as i32 * ROW);
            let row = RECT { left: d(PAD), top: y, right: rc.right - d(PAD), bottom: y + d(ROW - 2) };
            let chosen = s.selected == Some(i);
            if chosen {
                SelectObject(dc, HGDIOBJ(accent.0));
                let r = d(6);
                let _ = RoundRect(dc, row.left, row.top, row.right + 1, row.bottom + 1, r, r);
            }
            let preset = i < s.presets.len();
            SetTextColor(dc, COLORREF(if chosen || preset { TITLE } else { DETAIL }));
            let label = if preset { s.presets[i].clone() } else { "Без заголовка".to_string() };
            let mut text: Vec<u16> = label.encode_utf16().collect();
            let mut text_rc = RECT { left: row.left + d(10), right: row.right - d(10), ..row };
            DrawTextW(dc, &mut text, &mut text_rc, flags);
        }
        // Opened from the gamepad: a bar along the bottom that runs out when the title is taken.
        if s.countdown {
            let left = (PICK_MS as u64).saturating_sub(GetTickCount64().saturating_sub(s.stepped_at)) as f64 / PICK_MS as f64;
            let full = rc.right - 2 * d(PAD);
            let bar = RECT { left: d(PAD), top: rc.bottom - d(3), right: d(PAD) + (full as f64 * left) as i32, bottom: rc.bottom - d(1) };
            FillRect(dc, &bar, accent);
        }
        let _ = DeleteObject(HGDIOBJ(accent.0));
        SelectObject(dc, old_brush);
        SelectObject(dc, old_pen);

        SelectObject(dc, HGDIOBJ(hint_font.0));
        SetTextColor(dc, COLORREF(DETAIL));
        let hint = if s.countdown {
            "Сочетание — дальше · A — сохранить · щелчок — свой текст"
        } else if count == 0 {
            "Enter — сохранить · Esc — без заголовка"
        } else {
            "↑↓ — заготовки · Enter — сохранить · Esc — без заголовка"
        };
        let mut hint: Vec<u16> = hint.encode_utf16().collect();
        let mut hint_rc = RECT { left: d(PAD + 2), top: d(hint_top), right: rc.right - d(PAD), bottom: d(hint_top + HINT) };
        DrawTextW(dc, &mut hint, &mut hint_rc, flags);
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
        WM_TIMER if wparam.0 == TIMER_PICK => {
            choose(); // the gamepad countdown ran out: keep what is in the field
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_BAR => {
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        // A click on a card that has not got the focus: the user wants to type.
        WM_MOUSEACTIVATE => {
            let focused = STATE.with(|cell| cell.borrow().as_ref().is_some_and(|s| s.focused));
            if is_open() && !focused {
                focus_field();
            }
            LRESULT(MA_ACTIVATE as isize)
        }
        WM_CTLCOLOREDIT => {
            let dc = HDC(wparam.0 as _);
            SetTextColor(dc, COLORREF(TITLE));
            SetBkColor(dc, COLORREF(FIELD));
            let brush = STATE.with(|cell| cell.borrow().as_ref().map(|s| s.field_brush)).unwrap_or_default();
            LRESULT(brush.0 as isize)
        }
        WM_ACTIVATE => {
            // Clicked away from the field: keep what was typed.
            let state = STATE.with(|cell| cell.borrow().as_ref().map(|s| (s.focused, s.focused_at)));
            if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE && is_open() {
                if let Some((true, since)) = state {
                    // Test scripts start a process per command, which takes the focus: with
                    // REWIND_DEBUG the card waits for Enter/Esc (or the test hook) instead.
                    if std::env::var_os("REWIND_DEBUG").is_some() {
                        return LRESULT(0);
                    }
                    if GetTickCount64().saturating_sub(since) < 600 {
                        // Windows is still shuffling focus after the field took it: take it back.
                        let edit = STATE.with(|cell| cell.borrow().as_ref().map(|s| s.edit));
                        take_focus(hwnd);
                        if let Some(edit) = edit {
                            let _ = SetFocus(Some(edit));
                        }
                    } else {
                        finish(true);
                    }
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
    if msg == WM_KEYDOWN && (wparam.0 == VK_UP.0 as usize || wparam.0 == VK_DOWN.0 as usize) {
        step(if wparam.0 == VK_UP.0 as usize { -1 } else { 1 });
        return LRESULT(0);
    }
    if msg == WM_CHAR && (wparam.0 == 13 || wparam.0 == 27) {
        return LRESULT(0); // no beep
    }
    let old = STATE.with(|cell| cell.borrow().as_ref().and_then(|s| s.old_edit_proc));
    CallWindowProcW(old, hwnd, msg, wparam, lparam)
}
