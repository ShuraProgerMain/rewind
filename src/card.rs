//! Capture cards (and other video capture devices): an HDMI source such as a console, read
//! through Media Foundation. The card's MJPEG is decoded and turned into BGRA by Windows on
//! the CPU, then uploaded into a texture, so the rest of the pipeline (convert, encode) is the
//! same as for a screen. The card's sound is a recording device of its own (see audio.rs).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread::{self, JoinHandle};

use anyhow::{anyhow, bail, Context, Result};
use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};

use crate::d3d::Gpu;

/// A card to record from: device name, mode, the sound endpoint (WASAPI id) and the name for
/// the recordings folder.
#[derive(Clone, Debug, PartialEq)]
pub struct CardSpec {
    pub name: String,
    pub mode: Mode,
    pub audio: Option<String>,
    pub label: String,
}

/// How the card is read: picture size and frames per second.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Mode {
    /// "1080p30", "720p60"; anything else falls back to 1080p30.
    pub fn parse(text: &str) -> Self {
        match text {
            "720p60" => Mode { width: 1280, height: 720, fps: 60 },
            "1080p50" => Mode { width: 1920, height: 1080, fps: 50 },
            _ => Mode { width: 1920, height: 1080, fps: 30 },
        }
    }
}

fn startup() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
    });
}

unsafe fn string(activate: &IMFActivate, key: &GUID) -> Option<String> {
    let mut raw = PWSTR::null();
    let mut len = 0u32;
    activate.GetAllocatedString(key, &mut raw, &mut len).ok()?;
    let text = raw.to_string().ok();
    CoTaskMemFree(Some(raw.0 as *const _));
    text
}

/// Capture devices of one kind (video or audio), as (friendly name, activation object).
unsafe fn devices_of(kind: GUID) -> Vec<(String, IMFActivate)> {
    startup();
    let mut attrs: Option<IMFAttributes> = None;
    if MFCreateAttributes(&mut attrs, 1).is_err() {
        return Vec::new();
    }
    let Some(attrs) = attrs else { return Vec::new() };
    if attrs.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &kind).is_err() {
        return Vec::new();
    }
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    if MFEnumDeviceSources(&attrs, &mut list, &mut count).is_err() || list.is_null() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for i in 0..count as usize {
        if let Some(activate) = std::ptr::read(list.add(i)) {
            if let Some(name) = string(&activate, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME) {
                out.push((name, activate));
            }
        }
    }
    CoTaskMemFree(Some(list as *const _));
    out
}

/// Video capture devices by name. Virtual cameras (OBS, VR headsets) are listed too; the
/// user picks.
pub fn devices() -> Vec<String> {
    let mut names: Vec<String> = unsafe { devices_of(MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID) }.into_iter().map(|(n, _)| n).collect();
    names.dedup();
    names
}

/// The recording endpoint that carries a card's sound: one whose name shares the card's
/// first word ("fifine Video Capture" -> "fifine Audio Capture"), preferring "capture" over a
/// microphone. Returns the endpoint id for WASAPI and its name.
pub fn audio_for(card: &str) -> Option<(String, String)> {
    let brand = card.split_whitespace().next()?.to_lowercase();
    let mut best: Option<(i32, String, String)> = None;
    for (name, activate) in unsafe { devices_of(MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_AUDCAP_GUID) } {
        let lower = name.to_lowercase();
        if !lower.contains(&brand) {
            continue;
        }
        let score = lower.contains("capture") as i32 * 2 + lower.contains("hdmi") as i32 - lower.contains("microphone") as i32;
        let Some(id) = (unsafe { string(&activate, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_AUDCAP_ENDPOINT_ID) }) else { continue };
        if best.as_ref().is_none_or(|b| score > b.0) {
            best = Some((score, id, name));
        }
    }
    best.map(|(_, id, name)| (id, name))
}

/// The newest decoded picture: BGRA rows, top-down, `width * 4` bytes each.
struct Picture {
    data: Vec<u8>,
    width: u32,
    height: u32,
    serial: u64,
}

/// The last picture any running card capture produced, for the source picker (the device can
/// be opened by one reader at a time).
static LAST: Mutex<Option<(String, Arc<Picture>)>> = Mutex::new(None);

/// Opens the device and sets it to MJPEG at the mode (or the closest it has), decoded to RGB32.
unsafe fn open_reader(name: &str, mode: Mode) -> Result<(IMFSourceReader, IMFMediaSource, u32, u32)> {
    let (_, activate) = devices_of(MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID)
        .into_iter()
        .find(|(n, _)| n == name)
        .ok_or_else(|| anyhow!("capture device \"{name}\" is not connected"))?;
    let source: IMFMediaSource = activate.ActivateObject().context("cannot open the capture device (is another program using it?)")?;
    let mut attrs: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attrs, 2)?;
    let attrs = attrs.ok_or_else(|| anyhow!("no attributes"))?;
    // Lets the reader decode MJPEG and convert to RGB32 for us.
    attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
    let reader = MFCreateSourceReaderFromMediaSource(&source, &attrs)?;
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

    // Native formats: the one that matches size and rate, MJPEG preferred; else the largest
    // MJPEG at the highest rate it offers.
    let mut chosen: Option<(i64, IMFMediaType, u32, u32)> = None;
    for i in 0.. {
        let Ok(t) = reader.GetNativeMediaType(stream, i) else { break };
        let sub = t.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default();
        let size = t.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
        let rate = t.GetUINT64(&MF_MT_FRAME_RATE).unwrap_or(0);
        let (w, h) = ((size >> 32) as u32, size as u32);
        let fps = if rate as u32 == 0 { 0 } else { ((rate >> 32) as u32 + (rate as u32) / 2) / rate as u32 };
        let exact = w == mode.width && h == mode.height && fps == mode.fps;
        let score = exact as i64 * 1_000_000_000
            + (sub == MFVideoFormat_MJPG) as i64 * 100_000_000
            + (w as i64 * h as i64).min(1920 * 1080) * 10
            + fps.min(mode.fps) as i64;
        if chosen.as_ref().is_none_or(|c| score > c.0) {
            chosen = Some((score, t, w, h));
        }
    }
    let (_, native, w, h) = chosen.ok_or_else(|| anyhow!("the capture device offers no picture format"))?;
    reader.SetCurrentMediaType(stream, None, &native)?;
    let out = MFCreateMediaType()?;
    out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
    out.SetUINT64(&MF_MT_FRAME_SIZE, (w as u64) << 32 | h as u64)?;
    reader.SetCurrentMediaType(stream, None, &out).context("Windows cannot decode this capture format")?;
    Ok((reader, source, w, h))
}

fn log_once(kind: &str, pitch: i32) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| crate::log::line(&format!("card: first picture from a {kind}, row pitch {pitch}")));
}

/// Copies one RGB32 sample into a top-down BGRA picture (opaque alpha).
unsafe fn take(sample: &IMFSample, w: u32, h: u32, into: &mut Vec<u8>) -> Result<()> {
    let buffer = sample.ConvertToContiguousBuffer()?;
    let row = w as usize * 4;
    into.resize(row * h as usize, 0);
    if let Ok(two) = buffer.cast::<IMF2DBuffer>() {
        let mut line0: *mut u8 = std::ptr::null_mut();
        let mut pitch = 0i32;
        two.Lock2D(&mut line0, &mut pitch)?;
        if pitch.unsigned_abs() as usize != row {
            crate::log::line(&format!("card: unexpected row pitch {pitch} for {w}x{h}"));
        }
        if pitch.unsigned_abs() < row as u32 {
            let _ = two.Unlock2D();
            bail!("row pitch {pitch} too small");
        }
        // Windows hands RGB32 bottom-up here: the first line in memory is the bottom one.
        for y in 0..h as usize {
            let src = line0.offset((h as usize - 1 - y) as isize * pitch as isize);
            std::ptr::copy_nonoverlapping(src, into.as_mut_ptr().add(y * row), row);
        }
        log_once("2D buffer", pitch);
        let _ = two.Unlock2D();
    } else {
        let mut data: *mut u8 = std::ptr::null_mut();
        let mut len = 0u32;
        buffer.Lock(&mut data, None, Some(&mut len))?;
        // The video processor's plain buffer is already top-down (checked on a capture card).
        let n = (len as usize).min(into.len());
        std::ptr::copy_nonoverlapping(data, into.as_mut_ptr(), n);
        log_once("plain buffer", row as i32);
        let _ = buffer.Unlock();
    }
    for px in into.chunks_exact_mut(4) {
        px[3] = 255;
    }
    Ok(())
}

pub struct CardCapture {
    newest: Arc<Mutex<Option<Arc<Picture>>>>,
    shown: u64,
    texture: ID3D11Texture2D,
    gpu_context: windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
    width: u32,
    height: u32,
    closed: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Kept open for as long as the capture lives.
    _device: SharedSource,
}

impl CardCapture {
    pub fn new(gpu: &Gpu, name: &str, mode: Mode) -> Result<Self> {
        startup();
        // Open once here to learn the size and to fail early; the reader moves to its thread.
        let (reader, source, width, height) = unsafe { open_reader(name, mode)? };
        let reader = Shared(reader);
        let device = SharedSource(source);
        let newest = Arc::new(Mutex::new(None));
        let closed = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (n, c, s, label) = (newest.clone(), closed.clone(), stop.clone(), name.to_string());
        let thread = thread::Builder::new().name("card".into()).spawn(move || {
            let reader = reader;
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                let mut serial = 0u64;
                while !s.load(Ordering::Relaxed) {
                    let mut flags = 0u32;
                    let mut sample: Option<IMFSample> = None;
                    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
                    let read = reader.0.ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample));
                    if read.is_err() || flags & (MF_SOURCE_READERF_ERROR.0 | MF_SOURCE_READERF_ENDOFSTREAM.0) as u32 != 0 {
                        c.store(true, Ordering::Relaxed); // unplugged or taken away
                        break;
                    }
                    let Some(sample) = sample else { continue };
                    let mut data = Vec::new();
                    if take(&sample, width, height, &mut data).is_ok() {
                        serial += 1;
                        let picture = Arc::new(Picture { data, width, height, serial });
                        *n.lock().unwrap() = Some(picture.clone());
                        *LAST.lock().unwrap() = Some((label.clone(), picture));
                    }
                }
            }
        })?;

        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        let mut texture = None;
        unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut texture))? };
        let texture = texture.ok_or_else(|| anyhow!("no texture"))?;
        let gpu_context = unsafe { gpu.device.GetImmediateContext()? };
        Ok(Self { newest, shown: 0, texture, gpu_context, width, height, closed, stop, thread: Some(thread), _device: device })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// The newest picture since the last call, uploaded into the texture.
    pub fn latest(&mut self) -> Option<(ID3D11Texture2D, u32, u32)> {
        let picture = self.newest.lock().unwrap().clone()?;
        if picture.serial == self.shown {
            return None;
        }
        self.shown = picture.serial;
        unsafe {
            self.gpu_context.UpdateSubresource(&self.texture, 0, None, picture.data.as_ptr() as *const _, picture.width * 4, 0);
        }
        Some((self.texture.clone(), self.width, self.height))
    }
}

impl Drop for CardCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // The reader returns at the next frame (or with an error). With no signal on the card
        // none may come: wait a second at most, then let the thread finish on its own.
        // (Shutting the device down under a waiting read locks up Media Foundation.)
        if let Some(t) = self.thread.take() {
            let until = std::time::Instant::now() + std::time::Duration::from_secs(1);
            while !t.is_finished() && std::time::Instant::now() < until {
                thread::sleep(std::time::Duration::from_millis(10));
            }
            if t.is_finished() {
                let _ = t.join();
            }
        }
    }
}

/// The source reader is free-threaded; it only moves to the reading thread.
struct Shared(IMFSourceReader);
unsafe impl Send for Shared {}
#[allow(dead_code)] // held, not read
struct SharedSource(IMFMediaSource);
unsafe impl Send for SharedSource {}

/// A small BMP of the card's picture for the source picker: the running capture's latest
/// picture if there is one, else one frame read now.
pub fn preview(name: &str, width: u32) -> Option<Vec<u8>> {
    let running = LAST.lock().unwrap().as_ref().filter(|(n, _)| n == name).map(|(_, p)| p.clone());
    let picture = match running {
        Some(p) => p,
        None => unsafe {
            let (reader, source, w, h) = open_reader(name, Mode::parse("720p60")).ok()?;
            let _keep = source;
            let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
            let mut data = Vec::new();
            let mut got = false;
            // The first frames of a freshly opened card can be empty: try a few.
            for _ in 0..10 {
                let mut flags = 0u32;
                let mut sample: Option<IMFSample> = None;
                reader.ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample)).ok()?;
                if let Some(s) = sample {
                    if take(&s, w, h, &mut data).is_ok() {
                        got = true;
                    }
                    if got {
                        break;
                    }
                }
            }
            if !got {
                return None;
            }
            Arc::new(Picture { data, width: w, height: h, serial: 0 })
        },
    };
    Some(scale_bmp(&picture, width))
}

/// Nearest-neighbour shrink to `tw` wide, as a 24-bit bottom-up BMP.
fn scale_bmp(p: &Picture, tw: u32) -> Vec<u8> {
    let th = (tw * p.height / p.width.max(1)).max(1);
    let row = ((tw * 3 + 3) & !3) as usize;
    let mut px = vec![0u8; row * th as usize];
    for y in 0..th {
        let sy = (y * p.height / th) as usize;
        for x in 0..tw {
            let sx = (x * p.width / tw) as usize;
            let s = (sy * p.width as usize + sx) * 4;
            let o = (th - 1 - y) as usize * row + x as usize * 3;
            px[o..o + 3].copy_from_slice(&p.data[s..s + 3]);
        }
    }
    let mut out = Vec::with_capacity(54 + px.len());
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((54 + px.len()) as u32).to_le_bytes());
    out.extend_from_slice(&[0u8; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(tw as i32).to_le_bytes());
    out.extend_from_slice(&(th as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&[0u8; 24]);
    out.extend_from_slice(&px);
    out
}

/// Fails with a readable reason when the device cannot be read (for the settings page).
#[allow(dead_code)]
pub fn check(name: &str) -> Result<()> {
    unsafe { open_reader(name, Mode::parse("1080p30")).map(|_| ()) }.or_else(|e| bail!("{e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes() {
        assert_eq!(Mode::parse("720p60"), Mode { width: 1280, height: 720, fps: 60 });
        assert_eq!(Mode::parse("anything"), Mode { width: 1920, height: 1080, fps: 30 });
    }

    #[test]
    fn shrink() {
        let p = Picture { data: vec![10, 20, 30, 255].repeat(64 * 36), width: 64, height: 36, serial: 0 };
        let bmp = scale_bmp(&p, 32);
        assert_eq!(&bmp[0..2], b"BM");
        assert_eq!(bmp[54..57], [10, 20, 30]);
    }
}
