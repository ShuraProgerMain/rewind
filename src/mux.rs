//! Writes encoded packets of all tracks into an MP4 file. No re-encoding happens here.

use std::ffi::{c_void, CString};
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::Arc;

use anyhow::{bail, Result};

use crate::encoder::{Pkt, StreamInfo};
use crate::ffi::*;

/// A chapter of the file: editors such as DaVinci Resolve show each one as a marker on the
/// clip when the file is imported.
#[derive(Clone, Debug, PartialEq)]
pub struct Chapter {
    pub start: f64,
    pub end: f64,
    pub title: String,
    /// A user's marker (not the "Начало" chapter that MP4 needs before the first one).
    pub marker: bool,
}

/// "Метки (3): Boss fight · Метка 2 · Метка 3": the file's comment, which Explorer shows in
/// its "Комментарии" column, so the markers can be seen without opening Rewind.
pub fn marker_comment(list: &[Chapter]) -> Option<String> {
    let titles: Vec<&str> = list.iter().filter(|c| c.marker).map(|c| c.title.as_str()).collect();
    if titles.is_empty() {
        return None;
    }
    let mut text = format!("Метки ({}): {}", titles.len(), titles.join(" · "));
    if text.chars().count() > 250 {
        text = text.chars().take(249).collect::<String>() + "…";
    }
    Some(text)
}

/// Sets the chapters of an output file. They must be set before its header is written: the
/// MP4 writer puts the chapter texts into the file then (that copy is what DaVinci Resolve
/// reads; the "chpl" list it writes at the end is not enough).
pub unsafe fn set_chapters(oc: *mut AVFormatContext, list: &[Chapter]) -> Result<()> {
    for i in 0..(*oc).nb_chapters as usize {
        let c = *(*oc).chapters.add(i);
        av_dict_free(&mut (*c).metadata);
        av_free(c as *mut c_void);
    }
    av_freep(&mut (*oc).chapters as *mut *mut *mut AVChapter as *mut c_void);
    (*oc).nb_chapters = 0;
    match marker_comment(list) {
        Some(text) => {
            let text = CString::new(text.replace('\0', " "))?;
            av_dict_set(&mut (*oc).metadata, c"comment".as_ptr(), text.as_ptr(), 0);
        }
        None => {
            av_dict_set(&mut (*oc).metadata, c"comment".as_ptr(), null(), 0);
        }
    }
    if list.is_empty() {
        return Ok(());
    }
    let array = av_mallocz(list.len() * std::mem::size_of::<*mut AVChapter>()) as *mut *mut AVChapter;
    if array.is_null() {
        bail!("out of memory");
    }
    for (i, c) in list.iter().enumerate() {
        let ch = av_mallocz(std::mem::size_of::<AVChapter>()) as *mut AVChapter;
        if ch.is_null() {
            bail!("out of memory");
        }
        (*ch).id = i as i64;
        (*ch).time_base = AVRational { num: 1, den: 1000 };
        (*ch).start = (c.start.max(0.0) * 1000.0).round() as i64;
        (*ch).end = ((c.end.max(c.start) * 1000.0).round() as i64).max((*ch).start + 1);
        let title = CString::new(c.title.replace('\0', " "))?;
        av_dict_set(&mut (*ch).metadata, c"title".as_ptr(), title.as_ptr(), 0);
        *array.add(i) = ch;
        (*oc).chapters = array;
        (*oc).nb_chapters = i as u32 + 1;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Track {
    src_tb: AVRational,
    dst_tb: AVRational,
    /// Timestamp (source time base) that becomes zero in the file.
    base: i64,
}

pub struct Muxer {
    oc: *mut AVFormatContext,
    pkt: *mut AVPacket,
    tracks: Vec<Track>,
    /// Set by the first packet, which has to be a video keyframe.
    started: bool,
    header_written: bool,
    crash_safe: bool,
    /// Makes the chapters once the start of the file is known (see `plan_chapters`).
    plan: Option<Box<dyn FnOnce(i64) -> Vec<Chapter> + Send>>,
    /// The moment (pipeline clock, microseconds) that is zero in the file.
    pub start_us: Option<i64>,
    pub bytes: u64,
    pub frames: u64,
}

impl Muxer {
    /// `streams[0]` is the video track. `crash_safe` writes the file in fragments, so it stays
    /// playable if the program or the PC dies mid-recording; on a normal stop it is converted
    /// to a regular MP4. The header is written with the first picture (so chapters can be
    /// placed by then).
    pub fn create(path: &Path, streams: &[Arc<StreamInfo>], crash_safe: bool) -> Result<Self> {
        let cpath = CString::new(path.to_string_lossy().as_bytes())?;
        let mut m = Muxer {
            oc: null_mut(),
            pkt: null_mut(),
            tracks: Vec::new(),
            started: false,
            start_us: None,
            header_written: false,
            crash_safe,
            plan: None,
            bytes: 0,
            frames: 0,
        };
        unsafe {
            chk(
                avformat_alloc_output_context2(&mut m.oc, null(), c"mp4".as_ptr(), cpath.as_ptr()),
                "avformat_alloc_output_context2",
            )?;
            for (i, info) in streams.iter().enumerate() {
                let st = avformat_new_stream(m.oc, null());
                if st.is_null() {
                    bail!("avformat_new_stream failed");
                }
                chk(avcodec_parameters_copy((*st).codecpar, info.par), "avcodec_parameters_copy")?;
                (*(*st).codecpar).codec_tag = info.tag;
                (*st).time_base = info.time_base;
                if i == 0 {
                    let rate = AVRational { num: info.time_base.den, den: info.time_base.num };
                    (*st).avg_frame_rate = rate;
                    (*st).r_frame_rate = rate;
                }
                let title = CString::new(info.title.as_str())?;
                av_dict_set(&mut (*st).metadata, c"title".as_ptr(), title.as_ptr(), 0);
                av_dict_set(&mut (*st).metadata, c"handler_name".as_ptr(), title.as_ptr(), 0);
                m.tracks.push(Track { src_tb: info.time_base, dst_tb: info.time_base, base: 0 });
            }

            chk(avio_open(&mut (*m.oc).pb, cpath.as_ptr(), AVIO_FLAG_WRITE as i32), "cannot create file")?;
            m.pkt = av_packet_alloc();
        }
        Ok(m)
    }

    /// Chapters for the file, made from the moment that becomes its zero (pipeline clock, us),
    /// which is known only when the first picture arrives.
    pub fn plan_chapters(&mut self, plan: impl FnOnce(i64) -> Vec<Chapter> + Send + 'static) {
        self.plan = Some(Box::new(plan));
    }

    fn write_header(&mut self) -> Result<()> {
        unsafe {
            let mut opts: *mut AVDictionary = null_mut();
            if self.crash_safe {
                av_dict_set(&mut opts, c"movflags".as_ptr(), c"+hybrid_fragmented+frag_keyframe".as_ptr(), 0);
            }
            let r = avformat_write_header(self.oc, &mut opts);
            av_dict_free(&mut opts);
            chk(r, "avformat_write_header")?;
            self.header_written = true;
            for (i, t) in self.tracks.iter_mut().enumerate() {
                t.dst_tb = (**(*self.oc).streams.add(i)).time_base;
            }
        }
        Ok(())
    }

    /// Packets before the first video keyframe, and sound that belongs before it, are skipped.
    pub fn write(&mut self, p: &Pkt) -> Result<()> {
        let idx = p.stream as usize;
        if idx >= self.tracks.len() {
            return Ok(());
        }
        if !self.started {
            if !p.is_video_key() {
                return Ok(());
            }
            self.started = true;
            // The file starts at this picture. With B-frames its decode time is earlier than
            // its display time; the video track has to start at the decode time (timestamps
            // cannot go negative), so every track starts at that same moment, otherwise the
            // sound would run ahead of the picture by the B-frame delay.
            let us = AVRational { num: 1, den: 1_000_000 };
            let video_base = p.pts.min(p.dts);
            let start_us = p.us - unsafe { av_rescale_q(p.pts - video_base, self.tracks[0].src_tb, us) };
            for (i, t) in self.tracks.iter_mut().enumerate() {
                t.base = if i == 0 { video_base } else { unsafe { av_rescale_q(start_us, us, t.src_tb) } };
            }
            self.start_us = Some(start_us);
            if let Some(plan) = self.plan.take() {
                unsafe { set_chapters(self.oc, &plan(start_us))? };
            }
            self.write_header()?;
        }
        let Track { src_tb, dst_tb, base } = self.tracks[idx];
        if p.pts < base || p.dts < base {
            return Ok(());
        }
        unsafe {
            chk(av_new_packet(self.pkt, p.data.len() as i32), "av_new_packet")?;
            std::ptr::copy_nonoverlapping(p.data.as_ptr(), (*self.pkt).data, p.data.len());
            let k = &mut *self.pkt;
            k.pts = p.pts - base;
            k.dts = p.dts - base;
            k.duration = p.dur;
            k.stream_index = p.stream as i32;
            k.flags = if p.key { AV_PKT_FLAG_KEY as i32 } else { 0 };
            av_packet_rescale_ts(self.pkt, src_tb, dst_tb);
            chk(av_interleaved_write_frame(self.oc, self.pkt), "write packet")?;
        }
        self.bytes += p.data.len() as u64;
        if p.stream == 0 {
            self.frames += 1;
        }
        Ok(())
    }

    /// Finalizes the file. Dropping a Muxer without calling this leaves it as it is.
    pub fn finish(mut self) -> Result<()> {
        if !self.header_written {
            self.write_header()?; // not a single picture came: still a valid (empty) file
        }
        unsafe {
            self.header_written = false;
            chk(av_write_trailer(self.oc), "av_write_trailer")?;
        }
        Ok(())
    }
}

impl Drop for Muxer {
    fn drop(&mut self) {
        unsafe {
            if self.header_written {
                // Not finished explicitly (error path): still try to leave a readable file.
                av_write_trailer(self.oc);
            }
            av_packet_free(&mut self.pkt);
            if !self.oc.is_null() {
                if !(*self.oc).pb.is_null() {
                    avio_closep(&mut (*self.oc).pb);
                }
                avformat_free_context(self.oc);
            }
        }
    }
}
