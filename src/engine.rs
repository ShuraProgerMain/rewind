//! The recording engine: one thread that paces frames, feeds the encoder and routes packets
//! to the replay buffer and to the file writer. When neither the buffer nor a recording is
//! active, the whole pipeline is torn down and the thread sleeps on the command channel.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{
    CreateWaitableTimerExW, GetCurrentThread, SetThreadPriority, SetWaitableTimer, WaitForSingleObject,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, INFINITE, THREAD_PRIORITY_ABOVE_NORMAL, TIMER_ALL_ACCESS,
};
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

use crate::audio::{AacEncoder, AudioTrack, Clock, Kind};
use crate::capture::{Capture, Source};
use crate::d3d::create_gpu;
use crate::encoder::{Encoder, Pkt, StreamInfo, VideoCfg};
use crate::mux::Muxer;
use crate::ring::Ring;

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub source: Source,
    pub fps: u32,
    pub video: VideoCfg,
    /// Length of the replay buffer in seconds.
    pub buffer_secs: u32,
    /// Start with the replay buffer running.
    pub buffer_on: bool,
    /// Memory limit for the replay buffer.
    pub ring_mb: usize,
    pub out_dir: PathBuf,
    /// Record only this area of the source: x, y, width, height.
    pub crop: Option<(u32, u32, u32, u32)>,
    pub cursor: bool,
    /// Record what plays through the default output device (own track).
    pub audio_system: bool,
    /// Record the default microphone (own track).
    pub audio_mic: bool,
    /// Bitrate of each sound track, bits per second.
    pub audio_bitrate: i64,
    pub force_vendor: Option<u32>,
    pub verbose: bool,
}

#[derive(Clone, Debug)]
pub enum Cmd {
    /// Save the replay buffer to a file.
    SaveClip,
    ToggleRecord,
    ToggleBuffer,
    /// Apply new settings. A running pipeline is restarted (the buffer starts over, an
    /// ongoing recording is finalized first).
    SetConfig(Box<Config>),
    Quit,
}

/// What the engine tells the outside world (tray, notifications, console).
#[derive(Clone, Debug)]
pub enum Event {
    /// Capture and encoding are running.
    PipelineOn { width: u32, height: u32, fps: u32, encoder: String, gpu: String },
    /// Nothing is being captured: zero load.
    PipelineOff,
    /// The replay buffer was switched on or off.
    Buffer(bool),
    /// The replay buffer was given up on after repeated failures.
    BufferStopped,
    RecordingStarted(PathBuf),
    RecordingSaved { path: PathBuf, secs: f64, bytes: u64 },
    ClipSaved { path: PathBuf, secs: f64, bytes: u64, ms: u128 },
    /// "Save" was pressed while the replay buffer is off or still empty.
    NothingToSave,
    /// Periodic numbers for the console (only with `verbose`).
    Stat(String),
    Error(String),
}

pub type Emit = Arc<dyn Fn(Event) + Send + Sync>;

enum Exit {
    Quit,
    Idle,
    Restart(Config),
}

enum WMsg {
    Pkt(Pkt),
    StartRec(PathBuf),
    StopRec,
    Save(Vec<Pkt>, PathBuf),
    Quit,
}

struct State {
    buffer_on: bool,
    want_rec: bool,
}

pub fn run(mut cfg: Config, rx: Receiver<Cmd>, emit: Emit) {
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
    let mut st = State { buffer_on: cfg.buffer_on && cfg.buffer_secs > 0, want_rec: false };
    let mut failures = 0;
    loop {
        if !st.buffer_on && !st.want_rec {
            emit(Event::PipelineOff);
            match rx.recv() {
                Ok(Cmd::ToggleBuffer) => {
                    st.buffer_on = cfg.buffer_secs > 0;
                    emit(Event::Buffer(st.buffer_on));
                }
                Ok(Cmd::ToggleRecord) => st.want_rec = true,
                Ok(Cmd::SaveClip) => emit(Event::NothingToSave),
                Ok(Cmd::SetConfig(c)) => cfg = *c,
                Ok(Cmd::Quit) | Err(_) => return,
            }
            continue;
        }
        match pipeline(&cfg, &rx, &mut st, &emit) {
            Ok(Exit::Quit) => return,
            Ok(Exit::Idle) => failures = 0,
            Ok(Exit::Restart(c)) => {
                cfg = c;
                st.want_rec = false;
                if cfg.buffer_secs == 0 {
                    st.buffer_on = false;
                }
                failures = 0;
            }
            Err(e) => {
                emit(Event::Error(format!("{e:#}")));
                st.want_rec = false;
                failures += 1;
                if failures >= 3 {
                    // Do not retry forever; the user can switch the buffer back on.
                    if st.buffer_on {
                        st.buffer_on = false;
                        emit(Event::BufferStopped);
                    }
                    failures = 0;
                } else {
                    thread::sleep(Duration::from_secs(2));
                }
            }
        }
    }
}

/// Runs capture + encode until both modes are switched off, new settings arrive, or Quit.
fn pipeline(cfg: &Config, rx: &Receiver<Cmd>, st: &mut State, emit: &Emit) -> Result<Exit> {
    let monitor = match cfg.source {
        Source::Monitor(h) => h,
        Source::Window(h) => unsafe { MonitorFromWindow(HWND(h as _), MONITOR_DEFAULTTONEAREST).0 as isize },
    };
    let gpu = create_gpu(monitor, cfg.force_vendor)?;
    let mut cap = Capture::new(&gpu, cfg.source, cfg.fps, cfg.cursor)?;
    let (sw, sh) = cap.size();
    let (ox, oy, w, h) = match cfg.crop {
        Some((x, y, w, h)) => {
            let x = x.min(sw.saturating_sub(2));
            let y = y.min(sh.saturating_sub(2));
            (x, y, w.min(sw - x), h.min(sh - y))
        }
        None => (0, 0, sw, sh),
    };
    let (w, h) = (w & !1, h & !1); // 4:2:0 video needs even dimensions
    anyhow::ensure!(w >= 16 && h >= 16, "capture area is too small ({w}x{h})");

    let mut enc = Encoder::new(&gpu, (sw, sh), w, h, cfg.fps, &cfg.video, (ox, oy))?;
    emit(Event::PipelineOn { width: w, height: h, fps: cfg.fps, encoder: enc.name.clone(), gpu: gpu.name.clone() });

    // Track 0 is the picture; sound tracks follow in a fixed order.
    let mut infos = vec![enc.info()?];
    let mut sound: Vec<(Kind, AacEncoder)> = Vec::new();
    for (on, kind, title) in
        [(cfg.audio_system, Kind::System, "System sound"), (cfg.audio_mic, Kind::Mic, "Microphone")]
    {
        if on {
            let aac = AacEncoder::new(infos.len() as u8, cfg.audio_bitrate)?;
            infos.push(aac.info(title)?);
            sound.push((kind, aac));
        }
    }
    let (wtx, wrx) = channel::<WMsg>();
    let fps = cfg.fps;
    let writer_emit = emit.clone();
    let writer = thread::spawn(move || writer_thread(wrx, infos, fps, writer_emit));

    let span_us = cfg.buffer_secs as i64 * 1_000_000;
    let mut ring = Ring::new(span_us, cfg.ring_mb << 20);
    let ticker = Ticker::new(cfg.fps)?;
    // Sound runs on the same clock as the picture, each source on its own thread.
    let (atx, arx) = channel::<Pkt>();
    let clock = Clock { t0: ticker.t0, freq: ticker.freq };
    let tracks: Vec<AudioTrack> =
        sound.into_iter().map(|(kind, aac)| AudioTrack::start(kind, aac, clock, atx.clone())).collect();
    drop(atx);
    let mut recording = false;
    let mut force_key = false;
    let mut last_idx: i64 = -1;
    let mut exit = Exit::Idle;
    // "Save" pressed: the encoder still holds the last few pictures (B-frames and its own
    // queue), so the clip is cut once the picture of that moment has come out. Holds the
    // moment of the press (pipeline clock, us) and the latest tick to give up waiting at.
    let mut pending_save: Option<(i64, i64)> = None;

    // Counters for the periodic status line.
    let (mut n_new, mut n_dup, mut n_skipped, mut n_bytes) = (0u32, 0u32, 0i64, 0usize);
    let mut stat_at = Instant::now();

    let result: Result<()> = (|| {
        loop {
            // Start/stop requests that arrived while the pipeline was down or since the last tick.
            if st.want_rec != recording {
                if st.want_rec {
                    let path = out_path(&cfg.out_dir, "Rec")?;
                    let _ = wtx.send(WMsg::StartRec(path));
                    force_key = true; // the file must begin with a keyframe: make one right now
                } else {
                    let _ = wtx.send(WMsg::StopRec);
                }
                recording = st.want_rec;
            }
            if !st.buffer_on && !recording {
                return Ok(());
            }

            let mut idx = last_idx + 1;
            ticker.wait_for(idx);
            let now = ticker.index_now();
            if now > idx {
                n_skipped += now - idx; // we were late: keep the timeline honest
                idx = now;
            }

            while let Ok(cmd) = rx.try_recv() {
                match cmd {
                    Cmd::SaveClip => {
                        if !st.buffer_on || ring.bytes() == 0 {
                            emit(Event::NothingToSave);
                        } else if pending_save.is_none() {
                            let pressed_us = idx * 1_000_000 / cfg.fps as i64;
                            pending_save = Some((pressed_us, idx + cfg.fps as i64)); // wait at most 1 s
                        }
                    }
                    Cmd::ToggleRecord => st.want_rec = !st.want_rec,
                    Cmd::ToggleBuffer => {
                        st.buffer_on = !st.buffer_on && cfg.buffer_secs > 0;
                        ring.clear();
                        force_key = st.buffer_on;
                        emit(Event::Buffer(st.buffer_on));
                    }
                    Cmd::SetConfig(c) => {
                        exit = Exit::Restart(*c);
                        return Ok(());
                    }
                    Cmd::Quit => {
                        exit = Exit::Quit;
                        return Ok(());
                    }
                }
            }
            if cap.is_closed() {
                anyhow::bail!("the captured window or display is gone");
            }

            let frame = cap.latest()?;
            if frame.is_some() {
                n_new += 1;
            } else {
                n_dup += 1;
            }
            let buffer_on = st.buffer_on;
            let sent = enc.encode(frame.as_ref(), idx, force_key, &mut |p: Pkt| {
                n_bytes += p.data.len();
                if recording {
                    let _ = wtx.send(WMsg::Pkt(p.clone()));
                }
                if buffer_on {
                    ring.push(p);
                }
            })?;
            drop(frame);
            while let Ok(p) = arx.try_recv() {
                n_bytes += p.data.len();
                if recording {
                    let _ = wtx.send(WMsg::Pkt(p.clone()));
                }
                if buffer_on {
                    ring.push(p);
                }
            }
            if let Some((pressed_us, give_up)) = pending_save {
                if ring.last_us() >= pressed_us || idx >= give_up || !st.buffer_on {
                    pending_save = None;
                    let pk = if st.buffer_on { ring.snapshot(span_us) } else { Vec::new() };
                    if pk.is_empty() {
                        emit(Event::NothingToSave);
                    } else {
                        let _ = wtx.send(WMsg::Save(pk, out_path(&cfg.out_dir, "Clip")?));
                    }
                }
            }
            if sent {
                force_key = false;
            }
            last_idx = idx;

            if cfg.verbose && stat_at.elapsed() >= Duration::from_secs(5) {
                let secs = stat_at.elapsed().as_secs_f64();
                emit(Event::Stat(format!(
                    "new {:.1} fps, repeated {:.1} fps, skipped {}, {:.1} Mbit/s, buffer {:.0} s / {:.1} MB",
                    n_new as f64 / secs,
                    n_dup as f64 / secs,
                    n_skipped,
                    n_bytes as f64 * 8.0 / secs / 1e6,
                    ring.span() as f64 / 1e6,
                    ring.bytes() as f64 / 1048576.0
                )));
                (n_new, n_dup, n_skipped, n_bytes) = (0, 0, 0, 0);
                stat_at = Instant::now();
            }
        }
    })();

    // Always finalize an open recording, even after an error.
    drop(tracks); // stops and joins the sound threads
    while let Ok(p) = arx.try_recv() {
        if recording {
            let _ = wtx.send(WMsg::Pkt(p));
        }
    }
    let _ = enc.finish(&mut |p: Pkt| {
        if recording {
            let _ = wtx.send(WMsg::Pkt(p));
        }
    });
    let _ = wtx.send(WMsg::Quit);
    let _ = writer.join();
    result?;
    Ok(exit)
}

/// Disk work lives here so a slow drive can never stall the frame loop.
fn writer_thread(rx: Receiver<WMsg>, infos: Vec<Arc<StreamInfo>>, fps: u32, emit: Emit) {
    struct Rec {
        mux: Muxer,
        path: PathBuf,
    }
    let mut rec: Option<Rec> = None;
    let finish = |r: Rec| {
        let (frames, bytes) = (r.mux.frames, r.mux.bytes);
        match r.mux.finish() {
            Ok(()) => emit(Event::RecordingSaved { path: r.path, secs: frames as f64 / fps as f64, bytes }),
            Err(e) => emit(Event::Error(format!("could not finalize the recording: {e:#}"))),
        }
    };
    for msg in rx {
        match msg {
            WMsg::Pkt(p) => {
                if let Some(r) = rec.as_mut() {
                    if let Err(e) = r.mux.write(&p) {
                        emit(Event::Error(format!("recording stopped, write failed: {e:#}")));
                        rec = None; // one message, not one per packet
                    }
                }
            }
            WMsg::StartRec(path) => match Muxer::create(&path, &infos, true) {
                Ok(mux) => {
                    emit(Event::RecordingStarted(path.clone()));
                    rec = Some(Rec { mux, path });
                }
                Err(e) => emit(Event::Error(format!("cannot start recording: {e:#}"))),
            },
            WMsg::StopRec => {
                if let Some(r) = rec.take() {
                    finish(r);
                }
            }
            WMsg::Save(packets, path) => {
                let t = Instant::now();
                let res: Result<(u64, u64)> = (|| {
                    let mut m = Muxer::create(&path, &infos, false)?;
                    for p in &packets {
                        m.write(p)?;
                    }
                    let out = (m.frames, m.bytes);
                    m.finish()?;
                    Ok(out)
                })();
                match res {
                    Ok((frames, bytes)) => emit(Event::ClipSaved {
                        path,
                        secs: frames as f64 / fps as f64,
                        bytes,
                        ms: t.elapsed().as_millis(),
                    }),
                    Err(e) => emit(Event::Error(format!("could not save the clip: {e:#}"))),
                }
            }
            WMsg::Quit => break,
        }
    }
    if let Some(r) = rec.take() {
        finish(r);
    }
}

fn out_path(dir: &PathBuf, prefix: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let t = unsafe { GetLocalTime() };
    let stem = format!(
        "{prefix} {:04}-{:02}-{:02} {:02}-{:02}-{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    );
    let mut path = dir.join(format!("{stem}.mp4"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{stem} ({n}).mp4"));
        n += 1;
    }
    Ok(path)
}

/// Fixed-rate clock on a high-resolution waitable timer (no busy waiting).
struct Ticker {
    timer: HANDLE,
    freq: i64,
    t0: i64,
    period: f64,
}

fn qpc() -> i64 {
    let mut v = 0i64;
    unsafe {
        let _ = QueryPerformanceCounter(&mut v);
    }
    v
}

impl Ticker {
    fn new(fps: u32) -> Result<Self> {
        let mut freq = 0i64;
        unsafe {
            QueryPerformanceFrequency(&mut freq)?;
            let timer = CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )?;
            Ok(Self { timer, freq, t0: qpc(), period: freq as f64 / fps as f64 })
        }
    }

    /// Index of the tick that "now" belongs to.
    fn index_now(&self) -> i64 {
        ((qpc() - self.t0) as f64 / self.period) as i64
    }

    /// Sleeps until tick `idx` begins.
    fn wait_for(&self, idx: i64) {
        let target = self.t0 + (idx as f64 * self.period) as i64;
        let left = target - qpc();
        if left <= 0 {
            return;
        }
        let due = -(left * 10_000_000 / self.freq).max(1);
        unsafe {
            if SetWaitableTimer(self.timer, &due, 0, None, None, false).is_ok() {
                WaitForSingleObject(self.timer, INFINITE);
            }
        }
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.timer);
        }
    }
}
