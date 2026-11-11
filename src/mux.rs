//! Writes encoded packets of all tracks into an MP4 file. No re-encoding happens here.

use std::ffi::CString;
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::Arc;

use anyhow::{bail, Result};

use crate::encoder::{Pkt, StreamInfo};
use crate::ffi::*;

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
    pub bytes: u64,
    pub frames: u64,
}

impl Muxer {
    /// `streams[0]` is the video track. `crash_safe` writes the file in fragments, so it stays
    /// playable if the program or the PC dies mid-recording; on a normal stop it is converted
    /// to a regular MP4.
    pub fn create(path: &Path, streams: &[Arc<StreamInfo>], crash_safe: bool) -> Result<Self> {
        let cpath = CString::new(path.to_string_lossy().as_bytes())?;
        let mut m = Muxer {
            oc: null_mut(),
            pkt: null_mut(),
            tracks: Vec::new(),
            started: false,
            header_written: false,
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
                let title = CString::new(info.title)?;
                av_dict_set(&mut (*st).metadata, c"title".as_ptr(), title.as_ptr(), 0);
                av_dict_set(&mut (*st).metadata, c"handler_name".as_ptr(), title.as_ptr(), 0);
                m.tracks.push(Track { src_tb: info.time_base, dst_tb: info.time_base, base: 0 });
            }

            chk(avio_open(&mut (*m.oc).pb, cpath.as_ptr(), AVIO_FLAG_WRITE as i32), "cannot create file")?;

            let mut opts: *mut AVDictionary = null_mut();
            if crash_safe {
                av_dict_set(&mut opts, c"movflags".as_ptr(), c"+hybrid_fragmented+frag_keyframe".as_ptr(), 0);
            }
            let r = avformat_write_header(m.oc, &mut opts);
            av_dict_free(&mut opts);
            chk(r, "avformat_write_header")?;
            m.header_written = true;
            for (i, t) in m.tracks.iter_mut().enumerate() {
                t.dst_tb = (**(*m.oc).streams.add(i)).time_base;
            }
            m.pkt = av_packet_alloc();
        }
        Ok(m)
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
            // The file starts at this picture: line every track up with it.
            let us = AVRational { num: 1, den: 1_000_000 };
            for (i, t) in self.tracks.iter_mut().enumerate() {
                t.base = if i == 0 { p.pts.min(p.dts) } else { unsafe { av_rescale_q(p.us, us, t.src_tb) } };
            }
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
