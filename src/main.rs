//! Rewind: screen recorder with a replay buffer.
//!
//!   rewind.exe                 tray program (or hands over to the one already running)
//!   rewind.exe --background    same, but silent if it is already running (autostart)
//!   rewind.exe --data DIR      keep settings and the journal in DIR
//!   rewind.exe --send CMD      tell the running program: save | record | screenshot | buffer | quit
//!   rewind.exe cli [options]   console test harness for the engine (see `cli --help`)
#![windows_subsystem = "windows"]

mod app;
mod audio;
mod capture;
mod cli;
mod convert;
mod d3d;
mod encoder;
mod engine;
mod gamepad;
mod ffi;
mod hotkey;
mod icons;
mod indicator;
mod integration;
mod ipc;
mod library;
mod log;
mod mux;
mod picker;
mod ring;
mod settings;
mod shot;
mod settings_window;
mod sources;
mod toast;
mod updater;

use std::path::PathBuf;

use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

fn main() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        ffi::av_log_set_level(ffi::AV_LOG_WARNING as i32);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.first().map(String::as_str) == Some("cli") {
        if let Err(e) = cli::run(&args[1..]) {
            eprintln!("Error: {e:#}");
            std::process::exit(1);
        }
        return;
    }

    let mut data: Option<PathBuf> = None;
    let mut background = false;
    let mut send: Option<String> = None;
    let mut wait_pid: Option<u32> = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--data" => data = it.next().map(PathBuf::from),
            "--background" => background = true,
            "--send" => send = it.next(),
            // Set by the program itself when it restarts after an update.
            "--wait-pid" => wait_pid = it.next().and_then(|p| p.parse().ok()),
            _ => {}
        }
    }

    if let Some(command) = send {
        let pipe = ipc::pipe_name(&settings::data_dir(data).0);
        std::process::exit(if ipc::send(&pipe, &command) { 0 } else { 1 });
    }

    if let Err(e) = app::run(data, background, wait_pid) {
        log::line(&format!("fatal: {e:#}"));
        std::process::exit(1);
    }
}
