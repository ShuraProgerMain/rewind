//! The user's clips and recordings: listing, preview pictures, renaming and deleting.
//! Everything that touches a file first checks that it lies inside the recordings folder.

use std::collections::HashMap;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use anyhow::{anyhow, bail, Result};
use windows::core::PCWSTR;
use windows::Win32::UI::Shell::{
    SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FO_DELETE, SHFILEOPSTRUCTW,
};

use crate::ffi::*;

/// Where the recordings and the preview cache are; set by the app, read by the page server.
static PLACES: Mutex<Option<(PathBuf, PathBuf)>> = Mutex::new(None);
/// Durations already read, by (path, modified, size).
static DURATIONS: Mutex<Option<HashMap<(PathBuf, u64, u64), f64>>> = Mutex::new(None);

const THUMB_WIDTH: usize = 256;

pub fn configure(root: PathBuf, cache: PathBuf) {
    *PLACES.lock().unwrap() = Some((root, cache));
}

fn places() -> Option<(PathBuf, PathBuf)> {
    PLACES.lock().unwrap().clone()
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Entry {
    pub path: String,
    /// File name without ".mp4".
    pub name: String,
    /// "clip", "rec" or "" (any other video in the folder).
    pub kind: &'static str,
    /// Program it was recorded from: its folder, or the part after " — " in the name.
    pub app: String,
    /// Modified, milliseconds since 1970 (JavaScript time).
    pub time: u64,
    pub bytes: u64,
    pub secs: f64,
}

/// Turns a program name (or a name the user typed) into something every file system accepts.
pub fn safe_name(name: &str) -> String {
    let cleaned: String =
        name.chars().map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { ' ' } else { c }).collect();
    let words: Vec<&str> = cleaned.split_whitespace().collect();
    let mut out: String = words.join(" ").chars().take(80).collect();
    out = out.trim_end_matches(['.', ' ']).to_string(); // Windows drops these silently
    if out.is_empty() {
        "Без названия".to_string()
    } else {
        out
    }
}

/// True when `path` is inside the recordings folder (after resolving `..` and links).
pub fn inside(path: &Path) -> bool {
    let Some((root, _)) = places() else { return false };
    match (std::fs::canonicalize(&root), std::fs::canonicalize(path)) {
        (Ok(root), Ok(path)) => path != root && path.starts_with(&root),
        _ => false,
    }
}

fn stamp(meta: &std::fs::Metadata) -> u64 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as u64)
}

/// Length of a video from its header (the MP4 index), without decoding anything.
fn duration(path: &Path) -> f64 {
    let Ok(cpath) = CString::new(path.to_string_lossy().as_bytes()) else { return 0.0 };
    unsafe {
        let mut fmt: *mut AVFormatContext = null_mut();
        if avformat_open_input(&mut fmt, cpath.as_ptr(), null(), null_mut()) < 0 {
            return 0.0;
        }
        // The tracks know their length from the MP4 index; the file total is only filled in
        // by a full stream analysis, which would read data.
        let mut longest = 0f64;
        for i in 0..(*fmt).nb_streams as usize {
            let st = *(*fmt).streams.add(i);
            if (*st).duration > 0 {
                let tb = (*st).time_base;
                longest = longest.max((*st).duration as f64 * tb.num as f64 / tb.den as f64);
            }
        }
        avformat_close_input(&mut fmt);
        longest
    }
}

/// Newest first: videos in the recordings folder and its sub-folders (one level deep).
pub fn list(limit: usize) -> Vec<Entry> {
    let Some((root, _)) = places() else { return Vec::new() };
    let mut files: Vec<(PathBuf, std::fs::Metadata)> = Vec::new();
    let mut dirs = vec![(root.clone(), 0)];
    while let Some((dir, depth)) = dirs.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
        for item in read.flatten() {
            let path = item.path();
            let Ok(meta) = item.metadata() else { continue };
            if meta.is_dir() {
                if depth == 0 {
                    dirs.push((path, 1));
                }
            } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp4")) {
                files.push((path, meta));
            }
        }
    }
    files.sort_by_key(|(_, meta)| std::cmp::Reverse(stamp(meta)));
    files.truncate(limit);

    let mut cache = DURATIONS.lock().unwrap();
    let cache = cache.get_or_insert_with(HashMap::new);
    files
        .into_iter()
        .map(|(path, meta)| {
            let name = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let key = (path.clone(), stamp(&meta), meta.len());
            let secs = *cache.entry(key).or_insert_with(|| duration(&path));
            let lower = name.to_lowercase();
            let kind = if lower.contains("клип") || lower.starts_with("clip") {
                "clip"
            } else if lower.contains("запись") || lower.starts_with("rec") {
                "rec"
            } else {
                ""
            };
            let app = match path.parent() {
                Some(parent) if parent != root => parent.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
                _ => name.split_once(" — ").map(|(_, app)| app.to_string()).unwrap_or_default(),
            };
            Entry { path: path.to_string_lossy().to_string(), name, kind, app, time: stamp(&meta), bytes: meta.len(), secs }
        })
        .collect()
}

/// Moves a file to the Recycle Bin (it can be restored from there).
pub fn recycle(path: &Path) -> Result<()> {
    if !inside(path) {
        bail!("not a file in the recordings folder");
    }
    let mut from: Vec<u16> = path.as_os_str().to_string_lossy().encode_utf16().collect();
    from.extend([0, 0]); // the list ends with two zeros
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI).0 as u16,
        ..Default::default()
    };
    let code = unsafe { SHFileOperationW(&mut op) };
    if code != 0 || op.fAnyOperationsAborted.as_bool() {
        bail!("Windows could not move the file to the Recycle Bin (code {code})");
    }
    Ok(())
}

/// Gives a file a new name in the same folder. Returns the new path.
pub fn rename(path: &Path, new_name: &str) -> Result<PathBuf> {
    if !inside(path) {
        bail!("not a file in the recordings folder");
    }
    let stem = safe_name(new_name.trim().trim_end_matches(".mp4"));
    let target = path.with_file_name(format!("{stem}.mp4"));
    if target == path {
        return Ok(target);
    }
    if target.exists() {
        bail!("a file named «{stem}» is already there");
    }
    std::fs::rename(path, &target)?;
    Ok(target)
}

/// A preview picture (BMP) from the middle of a video, cached on disk by file and date.
pub fn thumbnail(path: &Path) -> Result<Vec<u8>> {
    if !inside(path) {
        bail!("not a file in the recordings folder");
    }
    let (_, cache_dir) = places().ok_or_else(|| anyhow!("not configured"))?;
    let meta = std::fs::metadata(path)?;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a over path, date and size
    for b in path.to_string_lossy().bytes().chain(stamp(&meta).to_le_bytes()).chain(meta.len().to_le_bytes()) {
        hash = (hash ^ b as u64).wrapping_mul(0x0100_0000_01b3);
    }
    let cached = cache_dir.join(format!("{hash:016x}.bmp"));
    if let Ok(bytes) = std::fs::read(&cached) {
        return Ok(bytes);
    }
    let bmp = unsafe { decode_middle(path)? };
    let _ = std::fs::create_dir_all(&cache_dir);
    let _ = std::fs::write(&cached, &bmp);
    Ok(bmp)
}

unsafe fn decode_middle(path: &Path) -> Result<Vec<u8>> {
    let cpath = CString::new(path.to_string_lossy().as_bytes())?;
    let mut fmt: *mut AVFormatContext = null_mut();
    chk(avformat_open_input(&mut fmt, cpath.as_ptr(), null(), null_mut()), "open")?;
    struct Guard(*mut AVFormatContext, *mut AVCodecContext, *mut AVPacket, *mut AVFrame);
    impl Drop for Guard {
        fn drop(&mut self) {
            unsafe {
                av_frame_free(&mut self.3);
                av_packet_free(&mut self.2);
                avcodec_free_context(&mut self.1);
                avformat_close_input(&mut self.0);
            }
        }
    }
    let mut g = Guard(fmt, null_mut(), av_packet_alloc(), av_frame_alloc());
    chk(avformat_find_stream_info(g.0, null_mut()), "stream info")?;
    let mut decoder: *const AVCodec = null();
    let index = chk(av_find_best_stream(g.0, AVMEDIA_TYPE_VIDEO, -1, -1, &mut decoder, 0), "no video")?;
    let stream = *(*g.0).streams.add(index as usize);
    g.1 = avcodec_alloc_context3(decoder);
    chk(avcodec_parameters_to_context(g.1, (*stream).codecpar), "decoder setup")?;
    (*g.1).thread_count = 2;
    chk(avcodec_open2(g.1, decoder, null_mut()), "decoder")?;

    // The middle of the video, or the start if its length is unknown.
    if (*g.0).duration > 0 {
        let target = av_rescale_q((*g.0).duration / 2, AVRational { num: 1, den: AV_TIME_BASE as i32 }, (*stream).time_base);
        av_seek_frame(g.0, index, target, 1); // AVSEEK_FLAG_BACKWARD: the keyframe at or before
    }
    let mut got = false;
    let mut flushed = false;
    while !got {
        if !flushed && av_read_frame(g.0, g.2) >= 0 {
            if (*g.2).stream_index == index {
                avcodec_send_packet(g.1, g.2);
            }
            av_packet_unref(g.2);
        } else if !flushed {
            avcodec_send_packet(g.1, null());
            flushed = true;
        }
        let r = avcodec_receive_frame(g.1, g.3);
        if r >= 0 {
            got = true;
        } else if r != AVERROR_EAGAIN {
            bail!("no picture could be decoded");
        }
    }
    Ok(to_bmp(&*g.3))
}

/// Scales a decoded YUV 4:2:0 picture (planar or NV12) down to THUMB_WIDTH and writes it as
/// a 24-bit BMP. BT.709, studio range, as Rewind records.
unsafe fn to_bmp(f: &AVFrame) -> Vec<u8> {
    let (w, h) = (f.width.max(1) as usize, f.height.max(1) as usize);
    let tw = THUMB_WIDTH.min(w);
    let th = ((h * tw / w).max(1) + 1) & !1;
    let nv12 = f.format == AV_PIX_FMT_NV12 as i32;
    let at = |plane: usize, x: usize, y: usize| -> f32 {
        *f.data[plane].offset((y as isize) * f.linesize[plane] as isize + x as isize) as f32
    };
    let row = (tw * 3).div_ceil(4) * 4;
    let mut px = vec![0u8; row * th];
    for ty in 0..th {
        for tx in 0..tw {
            // Average a 2x2 grid of samples inside the source area of this pixel.
            let (mut yy, mut uu, mut vv) = (0f32, 0f32, 0f32);
            for (sx, sy) in [(1, 1), (3, 1), (1, 3), (3, 3)] {
                let x = ((tx * 4 + sx) * w / (tw * 4)).min(w - 1);
                let y = ((ty * 4 + sy) * h / (th * 4)).min(h - 1);
                yy += at(0, x, y);
                if nv12 {
                    uu += at(1, x / 2 * 2, y / 2);
                    vv += at(1, x / 2 * 2 + 1, y / 2);
                } else {
                    uu += at(1, x / 2, y / 2);
                    vv += at(2, x / 2, y / 2);
                }
            }
            let y = (yy / 4.0 - 16.0) * (255.0 / 219.0);
            let u = (uu / 4.0 - 128.0) * (255.0 / 224.0);
            let v = (vv / 4.0 - 128.0) * (255.0 / 224.0);
            let r = y + 1.5748 * v;
            let g = y - 0.1873 * u - 0.4681 * v;
            let b = y + 1.8556 * u;
            let o = (th - 1 - ty) * row + tx * 3; // BMP rows go bottom-up, pixels are BGR
            px[o] = b.clamp(0.0, 255.0) as u8;
            px[o + 1] = g.clamp(0.0, 255.0) as u8;
            px[o + 2] = r.clamp(0.0, 255.0) as u8;
        }
    }
    let mut bmp = Vec::with_capacity(54 + px.len());
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&((54 + px.len()) as u32).to_le_bytes());
    bmp.extend_from_slice(&[0u8; 4]);
    bmp.extend_from_slice(&54u32.to_le_bytes());
    bmp.extend_from_slice(&40u32.to_le_bytes());
    bmp.extend_from_slice(&(tw as i32).to_le_bytes());
    bmp.extend_from_slice(&(th as i32).to_le_bytes());
    bmp.extend_from_slice(&1u16.to_le_bytes());
    bmp.extend_from_slice(&24u16.to_le_bytes());
    bmp.extend_from_slice(&[0u8; 24]);
    bmp.extend_from_slice(&px);
    bmp
}

/// Reads a `%XX`-encoded query value (what encodeURIComponent produces).
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&text[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]); // encodeURIComponent never turns a space into '+'
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

