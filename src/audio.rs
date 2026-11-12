//! Sound: WASAPI capture (what you hear, or the microphone) -> AAC, one track per source.
//!
//! Every track runs on the same clock as the video (QueryPerformanceCounter since the start
//! of the pipeline). Sample N of a track belongs at N / 48000 seconds on that clock; gaps
//! (nothing playing, device unplugged) are filled with silence so tracks never drift apart.

use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use windows::core::{Interface, GUID};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{IAgileObject, IAgileObject_Impl, BLOB};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows::Win32::Media::Audio::{
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
    IActivateAudioInterfaceCompletionHandler_Impl, AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
    eCapture, eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Performance::QueryPerformanceCounter;

use crate::encoder::{Pkt, StreamInfo};
use crate::ffi::*;

const RATE: i64 = 48_000;
/// A timestamp jump bigger than this is corrected (by silence or by dropping), in samples.
const MAX_SKEW: i64 = RATE * 40 / 1000;
/// While nothing arrives, silence is written up to this far behind "now", in samples.
const SILENCE_LAG: i64 = RATE * 200 / 1000;

/// libswresample: keep the resampler active even when the rates are equal.
const SWR_FLAG_RESAMPLE: i64 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Everything played through the default output device.
    System,
    /// The default recording device.
    Mic,
    /// Only what one program (and the processes it started) plays.
    App(AppTarget),
}

/// Which program to take the sound from. Resolved to a process again and again, so a game
/// that is restarted is picked up without restarting the recording.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppTarget {
    /// The program that owns this window.
    Window(isize),
    /// The first running process with this exe name.
    Exe(String),
}

impl AppTarget {
    fn pid(&self) -> Option<u32> {
        match self {
            AppTarget::Window(hwnd) => crate::sources::window_pid(*hwnd),
            AppTarget::Exe(exe) => crate::sources::find_process(exe),
        }
    }
}

/// Signals an event when Windows has finished activating a process audio client.
#[windows_core::implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct Activated(isize);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _operation: windows_core::Ref<IActivateAudioInterfaceAsyncOperation>) -> windows_core::Result<()> {
        unsafe { SetEvent(HANDLE(self.0 as _)) }
    }
}
impl IAgileObject_Impl for Activated_Impl {}

/// AAC encoder for one track, plus the converter from whatever the device delivers.
pub struct AacEncoder {
    ctx: *mut AVCodecContext,
    frame: *mut AVFrame,
    pkt: *mut AVPacket,
    swr: *mut SwrContext,
    fifo: [Vec<f32>; 2],
    scratch: [Vec<f32>; 2],
    /// Samples placed on the timeline so far.
    written: i64,
    /// Samples handed to the encoder so far.
    encoded: i64,
    /// The newest samples on the timeline are filler silence, not captured sound.
    after_silence: bool,
    frame_size: usize,
    stream: u8,
}
unsafe impl Send for AacEncoder {}

impl AacEncoder {
    pub fn new(stream: u8, bitrate: i64) -> Result<Self> {
        let mut e = AacEncoder {
            ctx: null_mut(),
            frame: null_mut(),
            pkt: null_mut(),
            swr: null_mut(),
            fifo: [Vec::new(), Vec::new()],
            scratch: [Vec::new(), Vec::new()],
            written: 0,
            encoded: 0,
            after_silence: true,
            frame_size: 1024,
            stream,
        };
        unsafe {
            let codec = avcodec_find_encoder(AV_CODEC_ID_AAC);
            if codec.is_null() {
                bail!("the FFmpeg build has no AAC encoder");
            }
            e.ctx = avcodec_alloc_context3(codec);
            if e.ctx.is_null() {
                bail!("avcodec_alloc_context3 failed");
            }
            let c = &mut *e.ctx;
            c.sample_fmt = AV_SAMPLE_FMT_FLTP;
            c.sample_rate = RATE as i32;
            av_channel_layout_default(&mut c.ch_layout, 2);
            c.bit_rate = bitrate;
            c.time_base = AVRational { num: 1, den: RATE as i32 };
            c.flags |= AV_CODEC_FLAG_GLOBAL_HEADER as i32;
            chk(avcodec_open2(e.ctx, codec, null_mut()), "cannot open the AAC encoder")?;
            e.frame_size = (*e.ctx).frame_size.max(1) as usize;

            e.frame = av_frame_alloc();
            e.pkt = av_packet_alloc();
            if e.frame.is_null() || e.pkt.is_null() {
                bail!("out of memory");
            }
            let f = &mut *e.frame;
            f.format = AV_SAMPLE_FMT_FLTP as i32;
            f.sample_rate = RATE as i32;
            f.nb_samples = e.frame_size as i32;
            chk(av_channel_layout_copy(&mut f.ch_layout, &(*e.ctx).ch_layout), "av_channel_layout_copy")?;
            chk(av_frame_get_buffer(e.frame, 0), "av_frame_get_buffer")?;
        }
        Ok(e)
    }

    pub fn info(&self, title: String) -> Result<Arc<StreamInfo>> {
        unsafe {
            let par = avcodec_parameters_alloc();
            if par.is_null() {
                bail!("avcodec_parameters_alloc failed");
            }
            let info = StreamInfo { par, time_base: (*self.ctx).time_base, tag: 0, title };
            chk(avcodec_parameters_from_context(par, self.ctx), "avcodec_parameters_from_context")?;
            // Files are cut from the middle of the stream, so there is no encoder priming to skip.
            (*par).initial_padding = 0;
            Ok(Arc::new(info))
        }
    }

    /// Describes the samples that `push` will receive from now on (interleaved 32-bit float).
    fn set_input(&mut self, rate: u32, channels: u32, mask: u32) -> Result<()> {
        unsafe {
            swr_free(&mut self.swr);
            let mut layout: AVChannelLayout = std::mem::zeroed();
            if mask == 0 || av_channel_layout_from_mask(&mut layout, mask as u64) < 0 || layout.nb_channels != channels as i32 {
                av_channel_layout_default(&mut layout, channels as i32);
            }
            chk(
                swr_alloc_set_opts2(
                    &mut self.swr,
                    &(*self.ctx).ch_layout,
                    AV_SAMPLE_FMT_FLTP,
                    RATE as i32,
                    &layout,
                    AV_SAMPLE_FMT_FLT,
                    rate as i32,
                    0,
                    null_mut(),
                ),
                "swr_alloc_set_opts2",
            )?;
            // Always go through the resampler, even 48 kHz -> 48 kHz: it is what lets
            // `compensate` nudge the speed later without a restart (and a click).
            av_opt_set_int(self.swr as *mut c_void, c"swr_flags".as_ptr(), SWR_FLAG_RESAMPLE, 0);
            chk(swr_init(self.swr), "swr_init")?;
        }
        Ok(())
    }

    /// The sound card's clock never matches the system clock exactly (tens of parts per
    /// million). `skew` is how many samples the track is short (+) or over (-); the speed is
    /// adjusted by an inaudible amount to close that gap over the next ten seconds.
    fn compensate(&mut self, skew: i64) {
        if self.swr.is_null() || skew.abs() < RATE / 2000 {
            return; // under half a millisecond: leave it alone
        }
        let delta = skew.clamp(-RATE / 100, RATE / 100) as i32; // at most 0.1 % speed change
        unsafe {
            swr_set_compensation(self.swr, delta, (RATE * 10) as i32);
        }
    }

    /// Appends `frames` captured sample frames to the timeline.
    unsafe fn push(&mut self, data: *const u8, frames: u32, out: &mut dyn FnMut(Pkt)) -> Result<()> {
        if self.swr.is_null() || frames == 0 {
            return Ok(());
        }
        let room = swr_get_out_samples(self.swr, frames as i32).max(0) as usize;
        self.scratch[0].resize(room, 0.0);
        self.scratch[1].resize(room, 0.0);
        let dst = [self.scratch[0].as_mut_ptr() as *mut u8, self.scratch[1].as_mut_ptr() as *mut u8];
        let src = [data];
        let n = chk(swr_convert(self.swr, dst.as_ptr(), room as i32, src.as_ptr(), frames as i32), "swr_convert")? as usize;
        self.fifo[0].extend_from_slice(&self.scratch[0][..n]);
        self.fifo[1].extend_from_slice(&self.scratch[1][..n]);
        self.written += n as i64;
        self.after_silence = false;
        self.encode_ready(out)
    }

    fn silence(&mut self, mut samples: i64, out: &mut dyn FnMut(Pkt)) -> Result<()> {
        self.after_silence = true;
        while samples > 0 {
            let n = samples.min(RATE) as usize; // in pieces, so a long pause costs no memory
            self.fifo[0].resize(self.fifo[0].len() + n, 0.0);
            self.fifo[1].resize(self.fifo[1].len() + n, 0.0);
            self.written += n as i64;
            samples -= n as i64;
            unsafe { self.encode_ready(out)? };
        }
        Ok(())
    }

    unsafe fn encode_ready(&mut self, out: &mut dyn FnMut(Pkt)) -> Result<()> {
        let n = self.frame_size;
        let mut used = 0;
        while self.fifo[0].len() - used >= n {
            chk(av_frame_make_writable(self.frame), "av_frame_make_writable")?;
            for ch in 0..2 {
                let dst = (*self.frame).data[ch] as *mut f32;
                std::ptr::copy_nonoverlapping(self.fifo[ch][used..].as_ptr(), dst, n);
            }
            used += n;
            (*self.frame).pts = self.encoded;
            self.encoded += n as i64;
            chk(avcodec_send_frame(self.ctx, self.frame), "avcodec_send_frame (audio)")?;
            loop {
                let r = avcodec_receive_packet(self.ctx, self.pkt);
                if r == AVERROR_EAGAIN || r == AVERROR_EOF {
                    break;
                }
                chk(r, "avcodec_receive_packet (audio)")?;
                let p = &*self.pkt;
                out(Pkt {
                    data: Arc::from(std::slice::from_raw_parts(p.data, p.size as usize)),
                    stream: self.stream,
                    pts: p.pts,
                    dts: p.dts,
                    dur: if p.duration > 0 { p.duration } else { n as i64 },
                    key: true,
                    us: p.pts * 1_000_000 / RATE,
                });
                av_packet_unref(self.pkt);
            }
        }
        self.fifo[0].drain(..used);
        self.fifo[1].drain(..used);
        Ok(())
    }
}

impl Drop for AacEncoder {
    fn drop(&mut self) {
        unsafe {
            // Let the encoder empty its queue; the tail (a few ms) is not needed.
            if !self.ctx.is_null() && !self.pkt.is_null() && avcodec_send_frame(self.ctx, null()) >= 0 {
                while avcodec_receive_packet(self.ctx, self.pkt) >= 0 {
                    av_packet_unref(self.pkt);
                }
            }
            swr_free(&mut self.swr);
            av_frame_free(&mut self.frame);
            av_packet_free(&mut self.pkt);
            avcodec_free_context(&mut self.ctx);
        }
    }
}

/// An opened capture device.
struct Device {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    id: String,
    rate: u32,
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

unsafe fn default_id(enumerator: &IMMDeviceEnumerator, kind: &Kind) -> Option<String> {
    if let Kind::App(target) = kind {
        return target.pid().map(|pid| format!("pid:{pid}"));
    }
    let flow = if *kind == Kind::System { eRender } else { eCapture };
    let device = enumerator.GetDefaultAudioEndpoint(flow, eConsole).ok()?;
    let raw = device.GetId().ok()?;
    let id = raw.to_string().ok();
    CoTaskMemFree(Some(raw.0 as *const c_void));
    id
}

/// Sound of one process and its children ("process loopback", Windows 10 2004 and later).
/// This kind of client has no mix format of its own: it is asked for 48 kHz float stereo.
unsafe fn open_app(target: &AppTarget, enc: &mut AacEncoder) -> Result<Device> {
    let Some(pid) = target.pid() else { bail!("the program is not running") };
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    // The blob points at `params` on this stack. PROPVARIANT's destructor would try to free
    // it (PropVariantClear), so it must never run: ManuallyDrop.
    let mut prop = std::mem::ManuallyDrop::new(PROPVARIANT::default());
    {
        let inner = &mut *prop.Anonymous.Anonymous;
        inner.vt = VT_BLOB;
        inner.Anonymous.blob = BLOB {
            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            pBlobData: &params as *const _ as *mut u8,
        };
    }
    let event = CreateEventW(None, false, false, None)?;
    let handler: IActivateAudioInterfaceCompletionHandler = Activated(event.0 as isize).into();
    let started = ActivateAudioInterfaceAsync(
        windows::core::w!("VAD\\Process_Loopback"),
        &IAudioClient::IID,
        Some(&*prop),
        &handler,
    );
    let operation = match started {
        Ok(op) => op,
        Err(e) => {
            let _ = CloseHandle(event);
            return Err(e.into());
        }
    };
    let waited = WaitForSingleObject(event, 5000);
    let _ = CloseHandle(event);
    if waited != WAIT_OBJECT_0 {
        bail!("Windows did not answer the request for the program's sound");
    }
    let mut result = windows::core::HRESULT(0);
    let mut unknown: Option<windows::core::IUnknown> = None;
    operation.GetActivateResult(&mut result, &mut unknown)?;
    result.ok()?;
    let client: IAudioClient = unknown.ok_or_else(|| anyhow::anyhow!("no audio client"))?.cast()?;

    const CHANNELS: u32 = 2;
    let want = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: CHANNELS as u16,
        nSamplesPerSec: RATE as u32,
        nAvgBytesPerSec: RATE as u32 * CHANNELS * 4,
        nBlockAlign: (CHANNELS * 4) as u16,
        wBitsPerSample: 32,
        cbSize: 0,
    };
    let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 5_000_000, 0, &want, None)?;
    enc.set_input(RATE as u32, CHANNELS, 0)?;
    let capture: IAudioCaptureClient = client.GetService()?;
    client.Start()?;
    Ok(Device { client, capture, id: format!("pid:{pid}"), rate: RATE as u32 })
}

unsafe fn open(enumerator: &IMMDeviceEnumerator, kind: &Kind, enc: &mut AacEncoder) -> Result<Device> {
    if let Kind::App(target) = kind {
        return open_app(target, enc);
    }
    let flow = if *kind == Kind::System { eRender } else { eCapture };
    let device = enumerator.GetDefaultAudioEndpoint(flow, eConsole)?;
    let raw = device.GetId()?;
    let id = raw.to_string().unwrap_or_default();
    CoTaskMemFree(Some(raw.0 as *const c_void));

    let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
    let mix = client.GetMixFormat()?;
    let wf = std::ptr::read_unaligned(mix);
    let (channels, rate) = (wf.nChannels as u32, wf.nSamplesPerSec);
    let (is_float, mask) = if wf.wFormatTag == WAVE_FORMAT_EXTENSIBLE && wf.cbSize >= 22 {
        let ext = std::ptr::read_unaligned(mix as *const WAVEFORMATEXTENSIBLE);
        let sub = ext.SubFormat;
        let mask = ext.dwChannelMask;
        (sub == SUBTYPE_IEEE_FLOAT, mask)
    } else {
        (wf.wFormatTag == WAVE_FORMAT_IEEE_FLOAT, 0)
    };

    let mut flags = if *kind == Kind::System { AUDCLNT_STREAMFLAGS_LOOPBACK } else { 0 };
    const BUFFER_100NS: i64 = 5_000_000; // half a second of headroom between our wake-ups
    let result = if is_float && wf.wBitsPerSample == 32 {
        let r = client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER_100NS, 0, mix, None);
        r.map(|_| mask)
    } else {
        // Unusual device format: let Windows convert to 32-bit float for us.
        flags |= AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        let want = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            nChannels: channels as u16,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * channels * 4,
            nBlockAlign: (channels * 4) as u16,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER_100NS, 0, &want, None).map(|_| 0)
    };
    CoTaskMemFree(Some(mix as *const c_void));
    let mask = result?;

    enc.set_input(rate, channels, mask)?;
    let capture: IAudioCaptureClient = client.GetService()?;
    client.Start()?;
    Ok(Device { client, capture, id, rate })
}

fn qpc() -> i64 {
    let mut v = 0i64;
    unsafe {
        let _ = QueryPerformanceCounter(&mut v);
    }
    v
}

/// The pipeline clock, as seen by an audio thread.
#[derive(Clone, Copy)]
pub struct Clock {
    /// QueryPerformanceCounter value at time zero.
    pub t0: i64,
    pub freq: i64,
}

impl Clock {
    /// Sample index that "now" corresponds to.
    fn sample_now(&self) -> i64 {
        ((qpc() - self.t0) as i128 * RATE as i128 / self.freq as i128) as i64
    }

    /// Sample index for a device timestamp (100 ns units of the performance counter), if it looks sane.
    fn sample_at(&self, stamp_100ns: u64) -> Option<i64> {
        let t0 = (self.t0 as i128 * 10_000_000 / self.freq as i128) as i64;
        let now = (qpc() as i128 * 10_000_000 / self.freq as i128) as i64;
        let stamp = stamp_100ns as i64;
        if (stamp - now).abs() > 20_000_000 {
            return None; // more than 2 s off: the driver reports nonsense
        }
        Some(((stamp - t0) as i128 * RATE as i128 / 10_000_000) as i64)
    }
}

/// Moves everything the device has queued onto the timeline. `last_skew` receives how far
/// the device's own timestamps are from our sample count (diagnostics).
unsafe fn drain(
    dev: &Device,
    enc: &mut AacEncoder,
    clock: Clock,
    last_skew: &mut Option<i64>,
    out: &mut dyn FnMut(Pkt),
) -> Result<()> {
    loop {
        if dev.capture.GetNextPacketSize()? == 0 {
            return Ok(());
        }
        let mut data: *mut u8 = null_mut();
        let mut frames = 0u32;
        let mut flags = 0u32;
        let mut stamp = 0u64;
        dev.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut stamp))?;

        let mut keep = true;
        if flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0 {
            if let Some(expected) = clock.sample_at(stamp) {
                let skew = expected - enc.written;
                *last_skew = Some(skew);
                if skew > MAX_SKEW || (skew > 0 && enc.after_silence) {
                    // A pause in the stream. Right after filler silence the sound is placed
                    // exactly where its timestamp says: nobody can hear silence being stretched.
                    enc.silence(skew, out)?;
                } else if skew < -MAX_SKEW {
                    keep = false; // we are ahead of the device: skip until it catches up
                }
            }
        }
        let result = if !keep {
            Ok(())
        } else if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
            enc.silence(frames as i64 * RATE / dev.rate as i64, out)
        } else {
            enc.push(data as *const u8, frames, out)
        };
        dev.capture.ReleaseBuffer(frames)?;
        result?;
    }
}

fn capture_thread(kind: Kind, mut enc: AacEncoder, clock: Clock, tx: Sender<Pkt>, stop: Arc<AtomicBool>) {
    let mut out = |p: Pkt| {
        let _ = tx.send(p);
    };
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let enumerator: Option<IMMDeviceEnumerator> = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok();
        let mut dev: Option<Device> = None;
        let mut open_at = Instant::now();
        let mut checked = Instant::now();
        let mut complained = false;
        // REWIND_AUDIO_DEBUG=1 prints the clock skew every 10 s.
        let debug = std::env::var_os("REWIND_AUDIO_DEBUG").is_some();
        let mut reported = Instant::now();
        let mut skew: Option<i64> = None;
        let mut shown = 0i64;
        let mut steered = Instant::now();

        while !stop.load(Ordering::Relaxed) {
            if let Some(en) = &enumerator {
                if dev.is_none() && Instant::now() >= open_at {
                    match open(en, &kind, &mut enc) {
                        Ok(d) => {
                            dev = Some(d);
                            complained = false;
                        }
                        Err(e) => {
                            if !complained {
                                crate::log::line(&format!("audio {kind:?}: no source ({e:#}); the track stays silent"));
                                complained = true;
                            }
                            // A program that is not running yet is looked for often, so a game's
                            // first sounds after launch are not lost; devices less often.
                            let wait = if matches!(kind, Kind::App(_)) { 500 } else { 2000 };
                            open_at = Instant::now() + Duration::from_millis(wait);
                        }
                    }
                }
                // Follow the default device when the user switches it (headphones, etc.).
                if checked.elapsed() >= Duration::from_secs(2) {
                    checked = Instant::now();
                    // ...or the program whose sound we take was restarted or closed.
                    if dev.as_ref().is_some_and(|d| default_id(en, &kind).as_deref() != Some(d.id.as_str())) {
                        dev = None;
                        open_at = Instant::now();
                        continue;
                    }
                }
            }
            if let Some(d) = &dev {
                if drain(d, &mut enc, clock, &mut skew, &mut out).is_err() {
                    dev = None; // unplugged or reconfigured: reopen shortly
                    open_at = Instant::now() + Duration::from_secs(1);
                }
            }
            // Keep the timeline moving while nothing is playing or no device exists.
            let behind = clock.sample_now() - enc.written;
            if behind > SILENCE_LAG {
                let _ = enc.silence(behind - SILENCE_LAG, &mut out);
            }
            // Once a second, steer the track towards the device's own timestamps.
            if steered.elapsed() >= Duration::from_secs(1) {
                steered = Instant::now();
                if let Some(s) = skew.take() {
                    shown = s;
                    enc.compensate(s);
                }
            }
            if debug && reported.elapsed() >= Duration::from_secs(10) {
                reported = Instant::now();
                crate::log::line(&format!("audio {kind:?}: skew {:+.2} ms", shown as f64 * 1000.0 / RATE as f64));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

/// A running capture thread.
pub struct AudioTrack {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AudioTrack {
    pub fn start(kind: Kind, enc: AacEncoder, clock: Clock, tx: Sender<Pkt>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = thread::spawn(move || capture_thread(kind, enc, clock, tx, flag));
        Self { stop, thread: Some(thread) }
    }
}

impl Drop for AudioTrack {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
