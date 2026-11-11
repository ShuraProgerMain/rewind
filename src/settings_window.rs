//! The settings window: an HTML page (ui/index.html, YT Fast look) in the system WebView2.
//! It is created when the user opens it and destroyed when they close it, so no browser
//! engine stays in memory while the program just sits in the tray.

use std::borrow::Cow;
use std::ffi::c_void;
use std::num::NonZeroIsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicIsize, Ordering};

use anyhow::{anyhow, Result};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_SYSTEMBACKDROP_TYPE,
    DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, GetStockObject, MonitorFromPoint, BLACK_BRUSH, HBRUSH, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows::Win32::UI::Shell::{
    FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS,
    SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, GetSystemMetrics, IsIconic, LoadCursorW, LoadImageW,
    PostMessageW, IMAGE_ICON, LR_SHARED, SM_CXICON, SM_CXSMICON,
    RegisterClassExW, SendMessageW, SetForegroundWindow, SetWindowPos, ShowWindow, HTCAPTION, ICON_BIG,
    ICON_SMALL, IDC_ARROW, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, SW_RESTORE, SW_SHOW, WM_CLOSE, WM_DPICHANGED,
    SC_KEYMENU, WM_ERASEBKGND, WM_NCCALCSIZE, WM_NCHITTEST, WM_SYSCOMMAND, WM_NCLBUTTONDOWN, WM_SETFOCUS, WM_SETICON, WNDCLASSEXW,
    WS_EX_APPWINDOW, WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU, HTCLIENT,
};
use wry::http::{Request, Response};
use wry::raw_window_handle::{HandleError, HasWindowHandle, RawWindowHandle, Win32WindowHandle, WindowHandle};
use wry::{Theme, WebContext, WebView, WebViewBuilder, WebViewBuilderExtWindows};


// Design size in 96-dpi pixels.
const WIDTH: i32 = 600;
const HEIGHT: i32 = 444;

const PAGE: &str = include_str!("../ui/index.html");
const FONT: &[u8] = include_bytes!("../ui/Inter.ttf");

/// Messages the window posts to its owner (the tray window).
pub const WM_UI_CLOSE: u32 = 0x8000 + 5;
pub const WM_UI_FOCUS: u32 = 0x8000 + 6;

static OWNER: AtomicIsize = AtomicIsize::new(0);

struct Host(HWND);
impl HasWindowHandle for Host {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let raw = NonZeroIsize::new(self.0 .0 as isize).ok_or(HandleError::Unavailable)?;
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Win32(Win32WindowHandle::new(raw))) })
    }
}

pub struct SettingsWindow {
    webview: Option<WebView>,
    _context: WebContext,
    hwnd: HWND,
}

fn apps_use_light_theme() -> bool {
    let mut value = 1u32;
    let mut size = 4u32;
    unsafe {
        let _ = RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut c_void),
            Some(&mut size),
        );
    }
    value != 0
}

fn serve(request: Request<Vec<u8>>) -> Response<Cow<'static, [u8]>> {
    let (mime, body): (&str, &'static [u8]) = match request.uri().path() {
        "/" | "/index.html" => ("text/html; charset=utf-8", PAGE.as_bytes()),
        "/Inter.ttf" => ("font/ttf", FONT),
        _ => {
            return Response::builder().status(404).body(Cow::Borrowed(&[][..])).expect("static response");
        }
    };
    Response::builder().header("Content-Type", mime).body(Cow::Borrowed(body)).expect("static response")
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let owner = HWND(OWNER.load(Ordering::Relaxed) as _);
    match msg {
        // No title bar and no frame: the page draws everything. The window still keeps its
        // system shadow, rounded corners and Alt+Tab entry.
        WM_NCCALCSIZE => LRESULT(0),
        WM_ERASEBKGND => LRESULT(1),
        // A lone Alt press would put the window into "menu mode", although it has no menu:
        // the next key would go nowhere.
        WM_SYSCOMMAND if (wparam.0 & 0xFFF0) == SC_KEYMENU as usize => LRESULT(0),
        // Fixed size: the edges are not resize handles.
        WM_NCHITTEST => LRESULT(HTCLIENT as isize),
        WM_CLOSE => {
            let _ = PostMessageW(Some(owner), WM_UI_CLOSE, WPARAM(0), LPARAM(0));
            LRESULT(0)
        }
        WM_SETFOCUS => {
            let _ = PostMessageW(Some(owner), WM_UI_FOCUS, WPARAM(0), LPARAM(0));
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let r = &*(lparam.0 as *const RECT);
            let _ = SetWindowPos(
                hwnd,
                None,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

impl SettingsWindow {
    /// Creates the window hidden; call `show` once the page reports that it is ready.
    /// `on_message` receives what the page sends through `window.ipc.postMessage`.
    pub fn open(owner: HWND, data_dir: &Path, on_message: impl Fn(String) + 'static) -> Result<Self> {
        OWNER.store(owner.0 as isize, Ordering::Relaxed);
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
                lpszClassName: w!("RewindSettings"),
                ..Default::default()
            };
            RegisterClassExW(&class); // fails harmlessly when already registered

            // Centre of the work area of the screen the pointer is on.
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            let _ = GetMonitorInfoW(monitor, &mut info);
            let (mut dpi, mut dpi_y) = (96u32, 96u32);
            let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut dpi_y);
            let (w, h) = (WIDTH * dpi as i32 / 96, HEIGHT * dpi as i32 / 96);
            let x = (info.rcWork.left + info.rcWork.right - w) / 2;
            let y = (info.rcWork.top + info.rcWork.bottom - h) / 2;

            let hwnd = CreateWindowExW(
                WS_EX_APPWINDOW,
                w!("RewindSettings"),
                w!("Rewind"),
                // A bare popup: with WS_CAPTION or WS_THICKFRAME Windows paints its title-bar
                // colour over the top of the page whenever the window is active.
                WS_POPUP | WS_SYSMENU | WS_MINIMIZEBOX,
                x,
                y,
                w,
                h,
                None,
                None,
                Some(instance.into()),
                None,
            )?;

            // Apply the frameless layout (the first size calculation happened before our
            // window procedure could answer it).
            let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);

            // The "material": Windows blurs whatever is behind the window (acrylic) and the
            // page tints it. Dark or light follows the system setting for apps.
            // REWIND_THEME=dark|light forces a theme (to check both looks on one system).
            let forced = std::env::var("REWIND_THEME").ok();
            let light = match forced.as_deref() {
                Some("dark") => false,
                Some("light") => true,
                _ => apps_use_light_theme(),
            };
            let dark: i32 = (!light) as i32;
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const i32 as *const c_void, 4);
            let round: u32 = 2; // DWMWCP_ROUND: a bare popup is square unless asked
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &round as *const u32 as *const c_void, 4);
            let acrylic: u32 = 3; // DWMSBT_TRANSIENTWINDOW
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, &acrylic as *const u32 as *const c_void, 4);
            let edge: u32 = if light { 0x00C8_C8C8 } else { 0x0010_1010 };
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &edge as *const u32 as *const c_void, 4);
            let glass = MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 };
            let _ = DwmExtendFrameIntoClientArea(hwnd, &glass);

            // Taskbar and Alt+Tab icon: the program's own, from the exe.
            for (metric, which) in [(SM_CXSMICON, ICON_SMALL), (SM_CXICON, ICON_BIG)] {
                let size = GetSystemMetrics(metric);
                if let Ok(icon) = LoadImageW(Some(instance.into()), PCWSTR(1 as _), IMAGE_ICON, size, size, LR_SHARED) {
                    SendMessageW(hwnd, WM_SETICON, Some(WPARAM(which as usize)), Some(LPARAM(icon.0 as isize)));
                }
            }

            let mut context = WebContext::new(Some(data_dir.join("webview")));
            let built = WebViewBuilder::new_with_web_context(&mut context)
                .with_transparent(true)
                .with_custom_protocol("rewind".into(), |_id, request| serve(request))
                .with_url("rewind://localhost/index.html")
                .with_ipc_handler(move |request| on_message(request.into_body()))
                .with_browser_accelerator_keys(false)
                .with_default_context_menus(false)
                .with_hotkeys_zoom(false)
                .with_focused(true)
                .with_theme(match forced.as_deref() {
                    Some("dark") => Theme::Dark,
                    Some("light") => Theme::Light,
                    _ => Theme::Auto,
                })
                .build(&Host(hwnd));
            match built {
                Ok(webview) => Ok(Self { webview: Some(webview), _context: context, hwnd }),
                Err(e) => {
                    let _ = DestroyWindow(hwnd);
                    Err(anyhow!("cannot create the settings window: {e}"))
                }
            }
        }
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// Shows the window and brings it to the front.
    pub fn show(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, if IsIconic(self.hwnd).as_bool() { SW_RESTORE } else { SW_SHOW });
            let _ = SetForegroundWindow(self.hwnd);
        }
        self.focus();
    }

    pub fn hide(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    pub fn focus(&self) {
        if let Some(webview) = &self.webview {
            let _ = webview.focus();
        }
    }

    /// Hands the page a fresh state object.
    pub fn update(&self, state_json: &str) {
        if let Some(webview) = &self.webview {
            let _ = webview.evaluate_script(&format!("window.rewind&&window.rewind.update({state_json})"));
        }
    }

    /// Runs a script in the page (test aid).
    pub fn eval(&self, script: &str) {
        if let Some(webview) = &self.webview {
            let _ = webview.evaluate_script(script);
        }
    }
}

impl Drop for SettingsWindow {
    fn drop(&mut self) {
        self.webview = None; // the browser goes first, then its window
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Lets the user drag the frameless window by its header.
pub fn start_drag(hwnd: HWND) {
    unsafe {
        let _ = ReleaseCapture();
        SendMessageW(hwnd, WM_NCLBUTTONDOWN, Some(WPARAM(HTCAPTION as usize)), Some(LPARAM(0)));
    }
}

/// The system "choose a folder" dialog. None when cancelled.
pub fn pick_folder(owner: HWND, start: &Path) -> Option<PathBuf> {
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_ALL).ok()?;
        let options = dialog.GetOptions().ok()?;
        dialog.SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
        if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(start.as_os_str()), None) {
            let _ = dialog.SetFolder(&item);
        }
        dialog.Show(Some(owner)).ok()?;
        let raw = dialog.GetResult().ok()?.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = raw.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(raw.0 as *const c_void));
        path
    }
}
