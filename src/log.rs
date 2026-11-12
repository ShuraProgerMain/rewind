//! Journal: a text file in the data folder (tray mode) or stderr (command-line mode).

use std::ffi::{c_char, c_int, c_void, CStr};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use windows::Win32::System::SystemInformation::GetLocalTime;

use crate::ffi;

static FILE: Mutex<Option<File>> = Mutex::new(None);
const MAX_SIZE: u64 = 1 << 20;

/// Starts writing to `path`; an oversized journal is kept once as `.old`.
pub fn init(path: &Path) {
    if std::fs::metadata(path).map(|m| m.len() > MAX_SIZE).unwrap_or(false) {
        let _ = std::fs::rename(path, path.with_extension("old"));
    }
    if let Ok(f) = OpenOptions::new().create(true).append(true).open(path) {
        *FILE.lock().unwrap() = Some(f);
    }
    std::panic::set_hook(Box::new(|info| line(&format!("PANIC: {info}"))));
    unsafe { ffi::av_log_set_callback(Some(ffmpeg_log)) };
}

pub fn line(msg: &str) {
    let mut guard = FILE.lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_mut() {
        Some(f) => {
            let t = unsafe { GetLocalTime() };
            let _ = writeln!(
                f,
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}  {msg}",
                t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
            );
        }
        None => eprintln!("{msg}"),
    }
}

/// FFmpeg's own warnings and errors would otherwise go to a console that does not exist.
unsafe extern "C" fn ffmpeg_log(ptr: *mut c_void, level: c_int, fmt: *const c_char, args: ffi::va_list) {
    if level > ffi::AV_LOG_WARNING as c_int {
        return;
    }
    let mut buf = [0 as c_char; 512];
    let mut prefix: c_int = 1;
    ffi::av_log_format_line2(ptr, level, fmt, args, buf.as_mut_ptr(), buf.len() as c_int, &mut prefix);
    let text = CStr::from_ptr(buf.as_ptr()).to_string_lossy();
    if text.contains("UDTA parsing failed") {
        return; // harmless, printed for every MP4 the recordings list reads
    }
    line(&format!("ffmpeg: {}", text.trim_end()));
}
