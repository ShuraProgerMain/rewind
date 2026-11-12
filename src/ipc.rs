//! One instance per user, and a way to talk to it: a named pipe. The first launch owns the
//! pipe; any later launch (or `rewind.exe --send save`) writes a command into it and exits.

use std::path::Path;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Result};
use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, ERROR_PIPE_BUSY, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_SHARE_NONE,
    OPEN_EXISTING, PIPE_ACCESS_INBOUND,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_MESSAGE, PIPE_TYPE_MESSAGE, PIPE_WAIT,
};

/// Pipe name for this user and this data folder (a test build with its own data folder
/// does not collide with the installed program).
pub fn pipe_name(data_dir: &Path) -> String {
    let user: String = std::env::var("USERNAME")
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let mut hash: u32 = 0x811C_9DC5; // FNV-1a
    // The same folder must give the same name however its path was spelled.
    let folder = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    for b in folder.to_string_lossy().to_lowercase().bytes() {
        hash = (hash ^ b as u32).wrapping_mul(0x0100_0193);
    }
    format!(r"\\.\pipe\rewind-{user}-{hash:08x}")
}

/// Sends a command to the running instance. False when there is none.
pub fn send(name: &str, command: &str) -> bool {
    let wide = HSTRING::from(name);
    for _ in 0..50 {
        unsafe {
            match CreateFileW(&wide, GENERIC_WRITE.0, FILE_SHARE_NONE, None, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, None)
            {
                Ok(pipe) => {
                    let mut written = 0u32;
                    let ok = WriteFile(pipe, Some(command.as_bytes()), Some(&mut written), None).is_ok();
                    let _ = CloseHandle(pipe);
                    return ok;
                }
                // The instance is talking to another client: try again in a moment.
                Err(e) if e.code() == ERROR_PIPE_BUSY.to_hresult() => thread::sleep(Duration::from_millis(10)),
                Err(_) => return false,
            }
        }
    }
    false
}

struct Pipe(HANDLE);
unsafe impl Send for Pipe {}

/// Becomes the instance that owns the pipe (fails if another one already does) and calls
/// `on_command` from a background thread for every command received.
pub fn serve(name: &str, on_command: impl Fn(String) + Send + 'static) -> Result<()> {
    let wide = HSTRING::from(name);
    let pipe = unsafe {
        CreateNamedPipeW(
            &wide,
            PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
            1,
            0,
            16 * 1024,
            0,
            None,
        )
    };
    if pipe.is_invalid() {
        bail!("another instance is already running");
    }
    let pipe = Pipe(pipe);
    thread::spawn(move || {
        let pipe = pipe;
        loop {
            unsafe {
                // An error here just means the client connected before we started waiting.
                let _ = ConnectNamedPipe(pipe.0, None);
                let mut buf = vec![0u8; 16 * 1024]; // test scripts ("ui <js>") can be long
                let mut read = 0u32;
                if ReadFile(pipe.0, Some(&mut buf[..]), Some(&mut read), None).is_ok() && read > 0 {
                    // Only short plain-text commands are expected; anything else is ignored by the receiver.
                    if let Ok(text) = std::str::from_utf8(&buf[..read as usize]) {
                        on_command(text.trim().to_string());
                    }
                }
                let _ = DisconnectNamedPipe(pipe.0);
            }
        }
    });
    Ok(())
}
