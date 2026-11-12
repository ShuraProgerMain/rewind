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
    /// File name without the extension.
    pub name: String,
    /// "clip", "rec", "shot" (a PNG screenshot) or "" (any other video in the folder).
    pub kind: &'static str,
    /// Program it was recorded from: its folder, or the part after " — " in the name.
    pub app: String,
    /// Modified, milliseconds since 1970 (JavaScript time).
    pub time: u64,
    pub bytes: u64,
    pub secs: f64,
    /// Marked moments (from `<name>.markers.json`).
    pub markers: Vec<crate::marks::Marker>,
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
            } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp4") || e.eq_ignore_ascii_case("png")) {
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
            let picture = is_picture(&path);
            let secs = if picture { 0.0 } else { *cache.entry(key).or_insert_with(|| duration(&path)) };
            let lower = name.to_lowercase();
            let kind = if picture {
                "shot"
            } else if lower.contains("клип") || lower.starts_with("clip") {
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
            let markers = if picture { Vec::new() } else { crate::marks::load(&path) };
            Entry { path: path.to_string_lossy().to_string(), name, kind, app, time: stamp(&meta), bytes: meta.len(), secs, markers }
        })
        .collect()
}

/// Moves a file to the Recycle Bin (it can be restored from there).
pub fn recycle(path: &Path) -> Result<()> {
    if !inside(path) {
        bail!("not a file in the recordings folder");
    }
    // The video and its marker files go together; the list is zero-separated and ends with two zeros.
    let mut from: Vec<u16> = Vec::new();
    for p in std::iter::once(path.to_path_buf()).chain(crate::marks::companions(path)) {
        from.extend(p.as_os_str().to_string_lossy().encode_utf16());
        from.push(0);
    }
    from.push(0);
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
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_else(|| "mp4".into());
    let stem = safe_name(new_name.trim().trim_end_matches(&format!(".{ext}")));
    let target = path.with_file_name(format!("{stem}.{ext}"));
    if target == path {
        return Ok(target);
    }
    if target.exists() {
        bail!("a file named «{stem}» is already there");
    }
    let markers = crate::marks::load(path);
    let companions = crate::marks::companions(path);
    std::fs::rename(path, &target)?;
    // The marker files follow; the timeline file names the video, so it is written anew.
    for old in companions {
        let _ = std::fs::remove_file(old);
    }
    if let Err(e) = crate::marks::save(&target, &markers) {
        crate::log::line(&format!("markers not moved to {}: {e:#}", target.display()));
    }
    Ok(target)
}

/// A screenshot (PNG) rather than a video.
pub fn is_picture(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("png"))
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
    if is_picture(path) {
        return Ok(std::fs::read(path)?); // a screenshot is its own preview
    }
    let cached = cache_dir.join(format!("{hash:016x}.bmp"));
    if let Ok(bytes) = std::fs::read(&cached) {
        return Ok(bytes);
    }
    let bmp = unsafe { decode_at(path, None)? };
    let _ = std::fs::create_dir_all(&cache_dir);
    let _ = std::fs::write(&cached, &bmp);
    Ok(bmp)
}

/// Decodes the picture at `at` seconds (from the keyframe before it, forward), or from the
/// middle of the video when `at` is None.
unsafe fn decode_at(path: &Path, at: Option<f64>) -> Result<Vec<u8>> {
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

    // The asked moment, else the middle of the video (or its start if the length is unknown).
    let tb = (*stream).time_base;
    let wanted: Option<i64> = match at {
        Some(t) => Some((t.max(0.0) * tb.den as f64 / tb.num as f64) as i64),
        None if (*g.0).duration > 0 => {
            Some(av_rescale_q((*g.0).duration / 2, AVRational { num: 1, den: AV_TIME_BASE as i32 }, tb))
        }
        None => None,
    };
    if let Some(target) = wanted {
        av_seek_frame(g.0, index, target, 1); // AVSEEK_FLAG_BACKWARD: the keyframe at or before
    }
    // For an exact moment, decode forward from that keyframe up to the picture asked for.
    let exact = at.map(|_| wanted.unwrap_or(0));
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
            let ts = (*g.3).best_effort_timestamp;
            got = match exact {
                Some(target) if !flushed && ts != AV_NOPTS_VALUE && ts < target => false,
                _ => true,
            };
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

/// A piece of a video for the page's player, which asks for byte ranges while it plays and
/// seeks. Returns the bytes and the file's total size. At most 4 MB per answer.
pub fn read_range(path: &Path, start: u64, end: Option<u64>) -> Result<(Vec<u8>, u64)> {
    use std::io::{Read, Seek, SeekFrom};
    if !inside(path) {
        bail!("not a file in the recordings folder");
    }
    let mut file = std::fs::File::open(path)?;
    let total = file.metadata()?.len();
    if start >= total {
        bail!("range outside the file");
    }
    let last = end.unwrap_or(total - 1).min(total - 1).min(start + (4 << 20) - 1);
    file.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; (last - start + 1) as usize];
    file.read_exact(&mut buf)?;
    Ok((buf, total))
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


/// Result of a trim: the new file and the part it really covers (the start moves back to the
/// keyframe at or before the chosen moment, because nothing is re-encoded).
#[derive(Clone, Debug, serde::Serialize)]
pub struct Trimmed {
    pub path: String,
    pub start: f64,
    pub end: f64,
}

/// A free name next to `path`: "<stem> (обрезка).mp4", then "(обрезка 2)" and so on.
fn trimmed_name(path: &Path) -> PathBuf {
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let mut n = 1;
    loop {
        let label = if n == 1 { "(обрезка)".to_string() } else { format!("(обрезка {n})") };
        let candidate = path.with_file_name(format!("{stem} {label}.mp4"));
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

/// Copies the part from `start` to `end` seconds into a new file next to the original,
/// without re-encoding: instant and lossless. The original stays as it is.
pub fn trim(path: &Path, start: f64, end: f64) -> Result<Trimmed> {
    if !inside(path) {
        bail!("not a file in the recordings folder");
    }
    if end <= start {
        bail!("the end must be after the start");
    }
    let out = trimmed_name(path);
    let result = unsafe { cut(path, &out, start, end) };
    if result.is_err() {
        let _ = std::fs::remove_file(&out); // never leave half a file behind
    }
    let (s, e) = result?;
    // Markers inside the kept part come along, shifted to the new start.
    let kept: Vec<crate::marks::Marker> = crate::marks::load(path)
        .into_iter()
        .filter(|m| m.t >= s - 0.001 && m.t <= e + 0.001)
        .map(|m| crate::marks::Marker { t: (m.t - s).max(0.0), ..m })
        .collect();
    if let Err(e) = crate::marks::save(&out, &kept) {
        crate::log::line(&format!("markers not copied to the trimmed file: {e:#}"));
    }
    Ok(Trimmed { path: out.to_string_lossy().to_string(), start: s, end: e })
}

struct CutFiles(*mut AVFormatContext, *mut AVFormatContext, *mut AVPacket, bool);

impl Drop for CutFiles {
    fn drop(&mut self) {
        unsafe {
            av_packet_free(&mut self.2);
            if !self.1.is_null() {
                if self.3 {
                    av_write_trailer(self.1);
                }
                if !(*self.1).pb.is_null() {
                    avio_closep(&mut (*self.1).pb);
                }
                avformat_free_context(self.1);
            }
            avformat_close_input(&mut self.0);
        }
    }
}

unsafe fn seconds(st: *mut AVStream, ts: i64) -> f64 {
    ts as f64 * (*st).time_base.num as f64 / (*st).time_base.den as f64
}

unsafe fn cut(src: &Path, dst: &Path, start: f64, end: f64) -> Result<(f64, f64)> {
    let csrc = CString::new(src.to_string_lossy().as_bytes())?;
    let cdst = CString::new(dst.to_string_lossy().as_bytes())?;
    let mut f = CutFiles(null_mut(), null_mut(), av_packet_alloc(), false);
    chk(avformat_open_input(&mut f.0, csrc.as_ptr(), null(), null_mut()), "open")?;
    chk(avformat_find_stream_info(f.0, null_mut()), "stream info")?;
    chk(avformat_alloc_output_context2(&mut f.1, null(), c"mp4".as_ptr(), cdst.as_ptr()), "output")?;

    let n = (*f.0).nb_streams as usize;
    let mut video: Option<usize> = None;
    let mut wanted = vec![false; n]; // video and sound tracks are copied, anything else is not
    for i in 0..n {
        let ist = *(*f.0).streams.add(i);
        let kind = (*(*ist).codecpar).codec_type;
        wanted[i] = kind == AVMEDIA_TYPE_VIDEO || kind == AVMEDIA_TYPE_AUDIO;
        let ost = avformat_new_stream(f.1, null());
        if ost.is_null() {
            bail!("avformat_new_stream failed");
        }
        chk(avcodec_parameters_copy((*ost).codecpar, (*ist).codecpar), "copy parameters")?;
        (*ost).time_base = (*ist).time_base;
        (*ost).avg_frame_rate = (*ist).avg_frame_rate;
        (*ost).r_frame_rate = (*ist).r_frame_rate;
        av_dict_copy(&mut (*ost).metadata, (*ist).metadata, 0); // track names
        (*ost).disposition = (*ist).disposition;
        if kind == AVMEDIA_TYPE_VIDEO && video.is_none() {
            video = Some(i);
        }
    }
    let video = video.ok_or_else(|| anyhow!("the file has no video"))?;
    chk(avio_open(&mut (*f.1).pb, cdst.as_ptr(), AVIO_FLAG_WRITE as i32), "cannot create the file")?;
    let mut opts: *mut AVDictionary = null_mut();
    av_dict_set(&mut opts, c"movflags".as_ptr(), c"+faststart".as_ptr(), 0);
    let r = avformat_write_header(f.1, &mut opts);
    av_dict_free(&mut opts);
    chk(r, "write header")?;
    f.3 = true;

    let vst = *(*f.0).streams.add(video);
    let target = (start.max(0.0) / seconds(vst, 1)) as i64; // in the video time base
    av_seek_frame(f.0, video as i32, target, 1); // AVSEEK_FLAG_BACKWARD

    // Every track starts at the decode time of the first video keyframe kept (as in mux.rs).
    let mut base: Option<f64> = None;
    let mut first_shown = 0f64;
    let mut last_end = 0f64;
    let mut done: Vec<bool> = wanted.iter().map(|w| !w).collect();
    while !done.iter().all(|d| *d) {
        if av_read_frame(f.0, f.2) < 0 {
            break;
        }
        let i = (*f.2).stream_index as usize;
        let ist = *(*f.0).streams.add(i);
        let pts = (*f.2).pts;
        let dts = if (*f.2).dts == AV_NOPTS_VALUE { pts } else { (*f.2).dts };
        if base.is_none() {
            // Wait for a video keyframe; drop anything before it.
            if i != video || (*f.2).flags & AV_PKT_FLAG_KEY as i32 == 0 {
                av_packet_unref(f.2);
                continue;
            }
            base = Some(seconds(ist, pts.min(dts)));
            first_shown = seconds(ist, pts);
        }
        let b = base.unwrap_or(0.0);
        // Video stops by decode order (the last pictures still have what they refer to),
        // sound by time.
        let past = if i == video { seconds(ist, dts) > end } else { seconds(ist, pts) > end };
        if past {
            done[i] = true;
        }
        if past || done[i] || seconds(ist, pts) < b {
            av_packet_unref(f.2); // after the end, or sound from before the start
            continue;
        }
        let shift = (b / seconds(ist, 1)).round() as i64;
        (*f.2).pts = pts - shift;
        (*f.2).dts = dts - shift;
        (*f.2).pos = -1;
        if i == video {
            last_end = last_end.max(seconds(ist, pts + (*f.2).duration));
        }
        let ost = *(*f.1).streams.add(i);
        av_packet_rescale_ts(f.2, (*ist).time_base, (*ost).time_base);
        chk(av_interleaved_write_frame(f.1, f.2), "write")?;
    }
    if base.is_none() {
        bail!("nothing to keep in that range");
    }
    f.3 = false;
    chk(av_write_trailer(f.1), "finish the file")?;
    Ok((first_shown, last_end.min(end).max(first_shown)))
}

/// One picture at a given moment, for players that cannot show the video itself.
pub fn frame_at(path: &Path, at: f64) -> Result<Vec<u8>> {
    if !inside(path) {
        bail!("not a file in the recordings folder");
    }
    unsafe { decode_at(path, Some(at)) }
}
