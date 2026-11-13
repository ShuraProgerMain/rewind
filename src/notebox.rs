//! The title box: a small card over the game for naming a marker that was just placed.
//! The marker itself is already stored when the box appears (its moment is the key press),
//! so the box only adds a title. Invisible to screen capture, like the notifications.
//! Lives on the tray (UI) thread.
//!
//! Two ways to use it:
//! * keyboard: a text field takes the focus; ↑/↓ pick one of the preset titles, or type your
//!   own; Enter keeps it, Esc leaves the marker untitled, clicking elsewhere keeps what was
//!   typed. Focus goes back to the game afterwards.
//! * gamepad: the card never takes the focus (the game keeps running and keeps its input).
//!   It lists the presets; pressing the same button combination again moves to the next one,
//!   and after a few seconds without presses the highlighted one is taken. The combination
//!   is one the game ignores, so nothing else has to be pressed.

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
    HWND_TOPMOST, MA_NOACTIVATE, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, SW_SHOW, WA_INACTIVE,
    WDA_EXCLUDEFROMCAPTURE, WM_ACTIVATE, WM_APP, WM_CHAR, WM_CTLCOLOREDIT, WM_KEYDOWN, WM_MOUSEACTIVATE, WM_PAINT,
    WM_SETFONT, WM_TIMER, WNDCLASSEXW, WNDPROC, WS_CHILD, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    WS_VISIBLE,
};

/// Posted to the owner when the box closes; `take()` then gives the marker id and title
/// (None = closed without a title).
pub const WM_NOTE_DONE: u32 = WM_APP + 14;

const WIDTH: i32 = 440;
const PAD: i32 = 16;
const HEADING: i32 = 20;
const FIELD_HEIGHT: i32 = 34;
const ROW: i32 = 28;
const HINT: i32 = 18;
const GAP: i32 = 8;
/// Gamepad: the highlighted title is taken after this long without a press.
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
    /// Boxes closed and not yet collected by the owner (one may close as the next opens).
    results: Vec<(u64, Option<String>)>,
    /// When the box opened (ms tick): focus shuffles right after opening are not "clicked away".
    opened_at: u64,
    presets: Vec<String>,
    /// Highlighted preset; in gamepad mode `presets.len()` means "no title".
    selected: Option<usize>,
    /// Opened from the gamepad: no focus, no field, the presets are stepped through.
    pad: bool,
    /// When the gamepad selection last moved (ms tick), for the countdown bar.
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

/// Where things go, in design pixels: (field top, list top, hint top, height).
fn layout(pad_mode: bool, rows: usize) -> (i32, i32, i32, i32) {
    let field_top = PAD + HEADING + GAP;
    let list_top = if pad_mode { field_top } else { field_top + FIELD_HEIGHT + GAP };
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
        opened_at: 0,
        presets: Vec::new(),
        selected: None,
        pad: false,
        stepped_at: 0,
    })
}

/// Shows the box for marker `id`; `heading` says which marker ("Метка 3 · 12:41").
/// `pad`: opened from the gamepad (see the module notes). A box that is already open is
/// closed first, keeping what was chosen there.
pub fn open(owner: HWND, id: u64, heading: &str, presets: &[String], pad: bool) {
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
    let pad = pad && !presets.is_empty(); // nothing to pick from: the field is the only way
    let plan = STATE.with(|cell| unsafe {
        let mut slot = cell.borrow_mut();
        let s = slot.as_mut()?;
        s.owner = owner;
        s.id = Some(id);
        s.opened_at = GetTickCount64();
        s.stepped_at = s.opened_at;
        s.heading = heading.encode_utf16().collect();
        s.previous = GetForegroundWindow();
        s.pad = pad;
        s.selected = if pad { Some(0) } else { None };
        s.presets = presets;

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
        let rows = s.presets.len() + usize::from(pad); // gamepad: plus "Без заголовка"
        let (field_top, _, _, height) = layout(pad, rows);
        let (w, h) = (scale(WIDTH, s.dpi), scale(height, s.dpi));
        let x = (info.rcMonitor.left + info.rcMonitor.right - w) / 2;
        let y = info.rcMonitor.top + scale(48, s.dpi);
        let (pad_px, field) = (scale(PAD, s.dpi), scale(FIELD_HEIGHT, s.dpi));
        let inner = scale(8, s.dpi);
        let line = scale(20, s.dpi);
        let text_font = s.fonts.map(|(_, f, _)| f).unwrap_or_default();
        // The field: the edit control sits inside a drawn rounded box, padded.
        let edit_rc = (pad_px + inner, scale(field_top, s.dpi) + (field - line) / 2, w - 2 * (pad_px + inner), line);
        Some((s.hwnd, s.edit, text_font, edit_rc, (x, y, w, h), pad))
    });
    let Some((hwnd, edit, text_font, (ex, ey, ew, eh), (x, y, w, h), pad)) = plan else { return };
    unsafe {
        // A gamepad card must not take the focus from the game, not even by a click.
        let style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        let wanted = if pad { style | WS_EX_NOACTIVATE.0 } else { style & !WS_EX_NOACTIVATE.0 };
        if wanted != style {
            SetWindowLongW(hwnd, GWL_EXSTYLE, wanted as i32);
        }
        SendMessageW(edit, WM_SETFONT, Some(WPARAM(text_font.0 as usize)), Some(LPARAM(1)));
        let _ = SetWindowPos(edit, None, ex, ey, ew, eh, Default::default());
        let _ = SetWindowTextW(edit, w!(""));
        let _ = ShowWindow(edit, if pad { SW_HIDE } else { SW_SHOW });
        let _ = InvalidateRect(Some(hwnd), None, true);
        if pad {
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_SHOWWINDOW | SWP_NOACTIVATE);
            SetTimer(Some(hwnd), TIMER_PICK, PICK_MS, None);
            SetTimer(Some(hwnd), TIMER_BAR, 50, None);
        } else {
            let _ = KillTimer(Some(hwnd), TIMER_PICK);
            let _ = KillTimer(Some(hwnd), TIMER_BAR);
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_SHOWWINDOW);
            take_focus(hwnd);
            let _ = SetFocus(Some(edit));
        }
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

/// The gamepad card is open (its combination steps through the presets).
pub fn is_pad_open() -> bool {
    STATE.with(|cell| cell.borrow().as_ref().is_some_and(|s| s.id.is_some() && s.pad))
}

/// Gamepad: highlight the next preset (after the last one comes "no title") and start the
/// countdown again.
pub fn next() {
    let hwnd = STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let s = slot.as_mut().filter(|s| s.id.is_some() && s.pad)?;
        let count = s.presets.len() + 1;
        s.selected = Some((s.selected.unwrap_or(0) + 1) % count);
        s.stepped_at = unsafe { GetTickCount64() };
        Some(s.hwnd)
    });
    if let Some(hwnd) = hwnd {
        unsafe {
            SetTimer(Some(hwnd), TIMER_PICK, PICK_MS, None);
            let _ = InvalidateRect(Some(hwnd), None, true);
        }
    }
}

/// Keyboard: ↑/↓ put a preset into the field.
fn step_preset(up: bool) {
    let pick = STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let s = slot.as_mut().filter(|s| s.id.is_some() && !s.presets.is_empty())?;
        let n = s.presets.len();
        let i = match (s.selected, up) {
            (None, false) => 0,
            (None, true) => n - 1,
            (Some(i), false) => (i + 1) % n,
            (Some(i), true) => (i + n - 1) % n,
        };
        s.selected = Some(i);
        Some((s.hwnd, s.edit, s.presets[i].clone()))
    });
    if let Some((hwnd, edit, text)) = pick {
        let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        unsafe {
            let _ = SetWindowTextW(edit, PCWSTR(wide.as_ptr()));
            SendMessageW(edit, 0x00B1 /* EM_SETSEL */, Some(WPARAM(wide.len())), Some(LPARAM(wide.len() as isize)));
            let _ = InvalidateRect(Some(hwnd), None, true);
        }
    }
}

/// Closes the box. `keep`: store the choice (Enter, focus lost, gamepad countdown); else no
/// title (Esc).
fn finish(keep: bool) {
    // Taking the id first makes the box count as closed for the messages the calls below cause.
    let open = STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let s = slot.as_mut()?;
        let id = s.id.take()?;
        let picked = if s.pad { s.selected.and_then(|i| s.presets.get(i).cloned()) } else { None };
        Some((id, s.hwnd, s.edit, s.previous, s.owner, s.pad, picked))
    });
    let Some((id, hwnd, edit, previous, owner, pad, picked)) = open else { return };
    unsafe {
        let _ = KillTimer(Some(hwnd), TIMER_PICK);
        let _ = KillTimer(Some(hwnd), TIMER_BAR);
        let mut text = None;
        if keep {
            if pad {
                text = picked;
            } else {
                let len = GetWindowTextLengthW(edit);
                let mut buf = vec![0u16; len as usize + 1];
                let got = GetWindowTextW(edit, &mut buf);
                let typed = String::from_utf16_lossy(&buf[..got.max(0) as usize]).trim().to_string();
                if !typed.is_empty() {
                    text = Some(typed);
                }
            }
        }
        STATE.with(|cell| {
            if let Some(s) = cell.borrow_mut().as_mut() {
                s.results.push((id, text));
            }
        });
        let _ = ShowWindow(hwnd, SW_HIDE);
        // Back to the game (a gamepad card never had the focus).
        if !pad && !previous.is_invalid() && IsWindow(Some(previous)).as_bool() {
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
        let d = |v: i32| scale(v, s.dpi);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let bg = CreateSolidBrush(COLORREF(BACKGROUND));
        FillRect(dc, &rc, bg);
        let _ = DeleteObject(HGDIOBJ(bg.0));
        let Some((head_font, text_font, hint_font)) = s.fonts else { return };
        SetBkMode(dc, TRANSPARENT);
        let rows = s.presets.len() + usize::from(s.pad);
        let (field_top, list_top, hint_top, _) = layout(s.pad, rows);
        let flags = DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS;

        let old = SelectObject(dc, HGDIOBJ(head_font.0));
        SetTextColor(dc, COLORREF(TITLE));
        let mut heading = s.heading.clone();
        let mut top = RECT { left: d(PAD), top: d(PAD), right: rc.right - d(PAD), bottom: d(PAD + HEADING) };
        DrawTextW(dc, &mut heading, &mut top, flags);

        let old_pen = SelectObject(dc, GetStockObject(NULL_PEN));
        let old_brush = SelectObject(dc, HGDIOBJ(s.field_brush.0));
        if !s.pad {
            let r = d(8);
            let _ = RoundRect(dc, d(PAD), d(field_top), rc.right - d(PAD) + 1, d(field_top + FIELD_HEIGHT) + 1, r, r);
        }
        // The presets (gamepad: plus "no title" at the end), the highlighted one marked.
        SelectObject(dc, HGDIOBJ(text_font.0));
        let accent = CreateSolidBrush(COLORREF(ACCENT));
        for i in 0..rows {
            let label = s.presets.get(i).cloned().unwrap_or_else(|| "Без заголовка".to_string());
            let y = d(list_top + i as i32 * ROW);
            let row = RECT { left: d(PAD), top: y, right: rc.right - d(PAD), bottom: y + d(ROW - 2) };
            let chosen = s.selected == Some(i);
            if chosen {
                SelectObject(dc, HGDIOBJ(accent.0));
                let r = d(6);
                let _ = RoundRect(dc, row.left, row.top, row.right + 1, row.bottom + 1, r, r);
            }
            SetTextColor(dc, COLORREF(if chosen { TITLE } else if i < s.presets.len() { TITLE } else { DETAIL }));
            let mut text: Vec<u16> = label.encode_utf16().collect();
            let mut text_rc = RECT { left: row.left + d(10), right: row.right - d(10), ..row };
            DrawTextW(dc, &mut text, &mut text_rc, flags);
        }
        // Gamepad: a bar that runs out when the highlighted title is taken.
        if s.pad {
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
        let hint = if s.pad {
            "Сочетание ещё раз — следующий · через 3 с выберется выделенный"
        } else if s.presets.is_empty() {
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
            finish(true); // the gamepad countdown ran out: take the highlighted title
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_BAR => {
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_MOUSEACTIVATE if is_pad_open() => LRESULT(MA_NOACTIVATE as isize),
        WM_CTLCOLOREDIT => {
            let dc = HDC(wparam.0 as _);
            SetTextColor(dc, COLORREF(TITLE));
            SetBkColor(dc, COLORREF(FIELD));
            let brush = STATE.with(|cell| cell.borrow().as_ref().map(|s| s.field_brush)).unwrap_or_default();
            LRESULT(brush.0 as isize)
        }
        WM_ACTIVATE => {
            // Clicked away: keep what was typed.
            if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE && is_open() && !is_pad_open() {
                let opened = STATE.with(|cell| cell.borrow().as_ref().map_or(0, |s| s.opened_at));
                let age = GetTickCount64().saturating_sub(opened);
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
    if msg == WM_KEYDOWN && (wparam.0 == VK_UP.0 as usize || wparam.0 == VK_DOWN.0 as usize) {
        step_preset(wparam.0 == VK_UP.0 as usize);
        return LRESULT(0);
    }
    if msg == WM_CHAR && (wparam.0 == 13 || wparam.0 == 27) {
        return LRESULT(0); // no beep
    }
    let old = STATE.with(|cell| cell.borrow().as_ref().and_then(|s| s.old_edit_proc));
    CallWindowProcW(old, hwnd, msg, wparam, lparam)
}
