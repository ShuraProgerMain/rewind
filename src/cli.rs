//! Command-line harness for the recording engine: `rewind.exe cli [options]`.
//! Used for tests and measurements; everyday use goes through the tray.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use windows::core::BOOL;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Console::{AttachConsole, SetConsoleCtrlHandler, ATTACH_PARENT_PROCESS};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::RegisterHotKey;
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, PostThreadMessageW, MSG, WM_HOTKEY, WM_QUIT};

use crate::capture::Source;
use crate::d3d;
use crate::encoder::{Codec, VideoCfg};
use crate::engine::{self, Cmd, Config, Event, SoundFrom};
use crate::hotkey;
use crate::icons;
use crate::sources::{find_window, monitors, windows_list};

static MAIN_THREAD: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

const HELP: &str = "\
rewind cli - recording engine test harness

  --list                 show displays and windows, then exit
  --monitor N            record display N (0 = primary, default)
  --window TEXT          record the first window whose title contains TEXT
  --crop X,Y,W,H         record only this area of the source
  --fps N                frames per second (default 60)
  --codec hevc|h264|av1  (default hevc)
  --quality N            constant quality, lower = better (default 20)
  --max-mbit N           bitrate ceiling in Mbit/s (default 100)
  --buffer SECONDS       replay buffer length, 0 = off (default 600)
  --ring-mb N            memory limit of the replay buffer (default 4096)
  --out DIR              output folder (default: Videos\\Rewind)
  --no-cursor            do not record the mouse pointer
  --no-sound             do not record system sound
  --no-mic               do not record the microphone
  --sound-app EXE        take sound only from this program (e.g. game.exe)
  --sound-window         with --window: take sound only from that window's program
  --audio-kbit N         bitrate of each sound track (default 192)
  --gpu nvidia|amd       force the encoder vendor
  --script LIST          timed commands for tests, e.g. 5:save,8:rec,14:rec,18:quit
  --quiet                no periodic status line
  --dump-icons DIR       write the tray icons as .bmp files, then exit
  --make-ico FILE        write the program icon (.ico), then exit

Hotkeys:  Ctrl+Alt+F10 save the replay buffer   Ctrl+Alt+F9 start/stop recording
          Ctrl+Alt+F8 replay buffer on/off      Ctrl+C quit";

fn parse_args(args: &[String]) -> Result<Option<(Config, Vec<(f64, Cmd)>)>> {
    let mut cfg = Config {
        source: Source::Monitor(0),
        fps: 60,
        video: VideoCfg { codec: Codec::Hevc, quality: 20, max_bitrate: 100_000_000, gop_secs: 2 },
        buffer_secs: 600,
        buffer_on: true,
        ring_mb: 4096,
        out_dir: PathBuf::new(),
        crop: None,
        cursor: true,
        audio_system: true,
        audio_from: SoundFrom::All,
        audio_mic: true,
        audio_bitrate: 192_000,
        force_vendor: None,
        verbose: true,
    };
    let mut script = Vec::new();
    let mut monitor_idx = 0usize;
    let mut window: Option<String> = None;

    let mut args = args.iter().cloned();
    while let Some(a) = args.next() {
        let mut val = || args.next().ok_or_else(|| anyhow!("{a} needs a value"));
        match a.as_str() {
            "-h" | "--help" => {
                println!("{HELP}");
                return Ok(None);
            }
            "--list" => {
                for (i, m) in monitors().iter().enumerate() {
                    println!(
                        "display {i}: {} {}x{}{}",
                        m.name,
                        m.width(),
                        m.height(),
                        if m.primary { " (primary)" } else { "" }
                    );
                }
                for (_, title) in windows_list() {
                    println!("window: {title}");
                }
                return Ok(None);
            }
            "--dump-icons" => {
                icons::dump(&PathBuf::from(val()?))?;
                return Ok(None);
            }
            "--make-ico" => {
                icons::write_ico(&PathBuf::from(val()?))?;
                return Ok(None);
            }
            "--monitor" => monitor_idx = val()?.parse().context("--monitor")?,
            "--window" => window = Some(val()?),
            "--crop" => {
                let v: Vec<u32> =
                    val()?.split(',').map(|s| s.trim().parse()).collect::<Result<_, _>>().context("--crop")?;
                if v.len() != 4 {
                    bail!("--crop needs X,Y,W,H");
                }
                cfg.crop = Some((v[0], v[1], v[2], v[3]));
            }
            "--fps" => cfg.fps = val()?.parse::<u32>().context("--fps")?.clamp(1, 240),
            "--codec" => {
                cfg.video.codec = match val()?.as_str() {
                    "hevc" | "h265" => Codec::Hevc,
                    "h264" | "avc" => Codec::H264,
                    "av1" => Codec::Av1,
                    other => bail!("unknown codec {other}"),
                }
            }
            "--quality" => cfg.video.quality = val()?.parse().context("--quality")?,
            "--max-mbit" => cfg.video.max_bitrate = val()?.parse::<i64>().context("--max-mbit")? * 1_000_000,
            "--buffer" => cfg.buffer_secs = val()?.parse().context("--buffer")?,
            "--ring-mb" => cfg.ring_mb = val()?.parse().context("--ring-mb")?,
            "--out" => cfg.out_dir = PathBuf::from(val()?),
            "--no-cursor" => cfg.cursor = false,
            "--no-sound" => cfg.audio_system = false,
            "--no-mic" => cfg.audio_mic = false,
            "--sound-app" => {
                let exe = val()?;
                cfg.audio_from = SoundFrom::App { name: exe.clone(), exe };
            }
            "--sound-window" => cfg.audio_from = SoundFrom::Source,
            "--audio-kbit" => {
                cfg.audio_bitrate = val()?.parse::<i64>().context("--audio-kbit")?.clamp(32, 512) * 1000
            }
            "--quiet" => cfg.verbose = false,
            "--gpu" => {
                cfg.force_vendor = Some(match val()?.as_str() {
                    "nvidia" => d3d::VENDOR_NVIDIA,
                    "amd" => d3d::VENDOR_AMD,
                    other => bail!("unknown gpu {other}"),
                })
            }
            "--script" => {
                for item in val()?.split(',') {
                    let (t, c) = item.split_once(':').ok_or_else(|| anyhow!("bad --script item {item}"))?;
                    let cmd = match c {
                        "save" => Cmd::SaveClip,
                        "rec" => Cmd::ToggleRecord,
                        "buffer" => Cmd::ToggleBuffer,
                        "quit" => Cmd::Quit,
                        other => bail!("unknown script command {other}"),
                    };
                    script.push((t.parse::<f64>().context("--script time")?, cmd));
                }
            }
            other => bail!("unknown option {other} (try: rewind cli --help)"),
        }
    }

    cfg.source = match window {
        Some(text) => {
            let (h, title) =
                find_window(&text).ok_or_else(|| anyhow!("no visible window with \"{text}\" in its title"))?;
            println!("window: {title}");
            Source::Window(h)
        }
        None => {
            let list = monitors();
            let m = list.get(monitor_idx).ok_or_else(|| anyhow!("there is no display {monitor_idx}"))?;
            Source::Monitor(m.handle)
        }
    };
    cfg.buffer_on = cfg.buffer_secs > 0;
    if cfg.out_dir.as_os_str().is_empty() {
        cfg.out_dir = crate::settings::Settings::default().output_dir();
    }
    Ok(Some((cfg, script)))
}

fn print_event(ev: Event) {
    const MB: f64 = 1048576.0;
    match ev {
        Event::PipelineOn { width, height, fps, encoder, gpu } => {
            println!("[on] {width}x{height} @ {fps} fps, {encoder} on {gpu}")
        }
        Event::PipelineOff => println!("[idle] capture is off"),
        Event::Buffer(on) => println!("replay buffer {}", if on { "on" } else { "off" }),
        Event::BufferStopped => eprintln!("giving up; replay buffer turned off"),
        Event::RecordingStarted(path) => println!("[rec] recording to {}", path.display()),
        Event::RecordingSaved { path, secs, bytes } => {
            println!("[rec] saved {} ({secs:.1} s, {:.1} MB)", path.display(), bytes as f64 / MB)
        }
        Event::ClipSaved { path, secs, bytes, ms } => {
            println!("[clip] saved {} ({secs:.1} s, {:.1} MB) in {ms} ms", path.display(), bytes as f64 / MB)
        }
        Event::NothingToSave => println!("replay buffer is off or empty, nothing to save"),
        Event::Stat(text) => println!("[stat] {text}"),
        Event::Error(text) => eprintln!("error: {text}"),
    }
}

unsafe extern "system" fn ctrl_handler(_: u32) -> BOOL {
    let _ = PostThreadMessageW(MAIN_THREAD.load(Ordering::Relaxed), WM_QUIT, WPARAM(0), LPARAM(0));
    // Closing the console kills the process as soon as this returns: wait for files to be finalized.
    for _ in 0..40 {
        if DONE.load(Ordering::Relaxed) {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    true.into()
}

pub fn run(args: &[String]) -> Result<()> {
    unsafe {
        // The exe is a windowed program; borrow the console it was started from, if any.
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
    let Some((cfg, script)) = parse_args(args)? else { return Ok(()) };

    let (tx, rx) = channel::<Cmd>();
    let engine = thread::spawn(move || engine::run(cfg, rx, Arc::new(print_event)));

    unsafe {
        MAIN_THREAD.store(GetCurrentThreadId(), Ordering::Relaxed);
        let _ = SetConsoleCtrlHandler(Some(ctrl_handler), true);
        for (id, keys, what) in
            [(1, "Ctrl+Alt+F10", "save clip"), (2, "Ctrl+Alt+F9", "record"), (3, "Ctrl+Alt+F8", "buffer on/off")]
        {
            let hk = hotkey::parse(keys).expect("built-in hotkey");
            if RegisterHotKey(None, id, hk.mods, hk.vk).is_err() {
                eprintln!("warning: hotkey for \"{what}\" is taken by another program");
            }
        }
    }

    if !script.is_empty() {
        let tx = tx.clone();
        let main_thread = MAIN_THREAD.load(Ordering::Relaxed);
        thread::spawn(move || {
            let start = std::time::Instant::now();
            for (at, cmd) in script {
                let wait = at - start.elapsed().as_secs_f64();
                if wait > 0.0 {
                    thread::sleep(Duration::from_secs_f64(wait));
                }
                if matches!(cmd, Cmd::Quit) {
                    unsafe {
                        let _ = PostThreadMessageW(main_thread, WM_QUIT, WPARAM(0), LPARAM(0));
                    }
                } else {
                    let _ = tx.send(cmd);
                }
            }
        });
    }

    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == WM_HOTKEY {
                let _ = tx.send(match msg.wParam.0 {
                    1 => Cmd::SaveClip,
                    2 => Cmd::ToggleRecord,
                    _ => Cmd::ToggleBuffer,
                });
            }
        }
    }

    let _ = tx.send(Cmd::Quit);
    let _ = engine.join();
    DONE.store(true, Ordering::Relaxed);
    Ok(())
}
