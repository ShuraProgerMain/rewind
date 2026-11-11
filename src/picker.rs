//! Area picker: dims one display and lets the user drag a rectangle on it.

use std::cell::RefCell;

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect, FrameRect, InvalidateRect,
    SelectObject, SetBkMode, SetTextColor, CLEARTYPE_QUALITY, DT_CENTER, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER,
    PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, SetFocus, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW, LoadCursorW,
    PostQuitMessage, RegisterClassExW, SetForegroundWindow, SetLayeredWindowAttributes, ShowWindow, TranslateMessage,
    IDC_CROSS, LWA_ALPHA, LWA_COLORKEY, MSG, SW_SHOW, WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEMOVE, WM_PAINT, WM_QUIT, WM_RBUTTONDOWN, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP,
};

/// This exact colour becomes a hole in the dimming layer.
const KEY: u32 = 0x00FF_00FF;
const DIM: u8 = 140;
const MIN_SIDE: i32 = 64;

#[derive(Default)]
struct State {
    start: Option<POINT>,
    now: POINT,
    /// Some(result) once the user finished or cancelled.
    done: Option<Option<RECT>>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn point(lparam: LPARAM) -> POINT {
    POINT { x: (lparam.0 & 0xFFFF) as i16 as i32, y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32 }
}

fn selection(s: &State) -> Option<RECT> {
    let a = s.start?;
    let b = s.now;
    Some(RECT { left: a.x.min(b.x), top: a.y.min(b.y), right: a.x.max(b.x), bottom: a.y.max(b.y) })
}

unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let dc = BeginPaint(hwnd, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let black = CreateSolidBrush(COLORREF(0));
    FillRect(dc, &rc, black);
    let _ = DeleteObject(black.into());

    let font = CreateFontW(-22, 0, 0, 0, 500, 0, 0, 0, Default::default(), Default::default(), Default::default(), CLEARTYPE_QUALITY, 0, w!("Segoe UI Variable Text"));
    let old = SelectObject(dc, font.into());
    SetBkMode(dc, TRANSPARENT);
    SetTextColor(dc, COLORREF(0x00FF_FFFF));
    let flags = DT_SINGLELINE | DT_CENTER | DT_VCENTER | DT_NOPREFIX;

    let sel = STATE.with(|s| selection(&s.borrow()));
    match sel {
        Some(r) if r.right > r.left && r.bottom > r.top => {
            let hole = CreateSolidBrush(COLORREF(KEY));
            FillRect(dc, &r, hole);
            let _ = DeleteObject(hole.into());
            let white = CreateSolidBrush(COLORREF(0x00FF_FFFF));
            let mut frame = RECT { left: r.left - 2, top: r.top - 2, right: r.right + 2, bottom: r.bottom + 2 };
            FrameRect(dc, &frame, white);
            frame = RECT { left: r.left - 1, top: r.top - 1, right: r.right + 1, bottom: r.bottom + 1 };
            FrameRect(dc, &frame, white);
            let _ = DeleteObject(white.into());
            // Size label under the selection (above it when there is no room below).
            let mut text: Vec<u16> = format!("{} × {}", r.right - r.left, r.bottom - r.top).encode_utf16().collect();
            let below = r.bottom + 44 <= rc.bottom;
            let mut label = RECT {
                left: r.left - 200,
                right: r.right + 200,
                top: if below { r.bottom + 8 } else { r.top - 40 },
                bottom: if below { r.bottom + 40 } else { r.top - 8 },
            };
            DrawTextW(dc, &mut text, &mut label, flags);
        }
        _ => {
            let mut text: Vec<u16> = "Выделите область мышью  ·  Esc — отмена".encode_utf16().collect();
            let mut top = RECT { left: rc.left, right: rc.right, top: rc.top + 60, bottom: rc.top + 110 };
            DrawTextW(dc, &mut text, &mut top, flags);
        }
    }
    SelectObject(dc, old);
    let _ = DeleteObject(font.into());
    let _ = EndPaint(hwnd, &ps);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let p = point(lparam);
            STATE.with(|s| {
                let mut s = s.borrow_mut();
                s.start = Some(p);
                s.now = p;
            });
            SetCapture(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let dragging = STATE.with(|s| {
                let mut s = s.borrow_mut();
                s.now = point(lparam);
                s.start.is_some()
            });
            if dragging {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let _ = ReleaseCapture();
            STATE.with(|s| {
                let mut s = s.borrow_mut();
                if s.start.is_some() {
                    s.now = point(lparam);
                    let r = selection(&s);
                    s.done = Some(r.filter(|r| r.right - r.left >= MIN_SIDE && r.bottom - r.top >= MIN_SIDE));
                }
            });
            LRESULT(0)
        }
        WM_RBUTTONDOWN | WM_KILLFOCUS => {
            STATE.with(|s| {
                let mut s = s.borrow_mut();
                if s.done.is_none() {
                    s.done = Some(None);
                }
            });
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 as u16 == VK_ESCAPE.0 => {
            STATE.with(|s| s.borrow_mut().done = Some(None));
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Lets the user select an area of the display `monitor` (screen coordinates). Returns
/// [x, y, width, height] relative to that display, or None if cancelled. Runs its own
/// message loop, so call it while no application state is borrowed.
pub fn pick_area(monitor: RECT) -> Option<[u32; 4]> {
    unsafe {
        let instance = GetModuleHandleW(None).ok()?;
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_CROSS).unwrap_or_default(),
            lpszClassName: w!("RewindAreaPicker"),
            ..Default::default()
        };
        RegisterClassExW(&class);
        STATE.with(|s| *s.borrow_mut() = State::default());

        let (w, h) = (monitor.right - monitor.left, monitor.bottom - monitor.top);
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            w!("RewindAreaPicker"),
            w!(""),
            WS_POPUP,
            monitor.left,
            monitor.top,
            w,
            h,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .ok()?;
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(KEY), DIM, LWA_COLORKEY | LWA_ALPHA);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));

        let mut msg = MSG::default();
        let result = loop {
            if let Some(done) = STATE.with(|s| s.borrow().done) {
                break done;
            }
            if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
                PostQuitMessage(msg.wParam.0 as i32); // the program is quitting: pass it on
                break None;
            }
            if msg.message == WM_QUIT {
                break None;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        };
        let _ = DestroyWindow(hwnd);

        let r = result?;
        // Video needs even dimensions.
        let (x, y) = (r.left.clamp(0, w), r.top.clamp(0, h));
        let (cw, ch) = (((r.right.min(w) - x) & !1), ((r.bottom.min(h) - y) & !1));
        (cw >= MIN_SIDE && ch >= MIN_SIDE).then_some([x as u32, y as u32, cw as u32, ch as u32])
    }
}
