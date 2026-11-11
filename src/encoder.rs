//! Hardware video encoder fed straight from D3D11 textures (no readback to system memory).

use std::ffi::{c_void, CString};
use std::ptr::{null, null_mut};
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{ID3D11Texture2D, D3D11_BIND_RENDER_TARGET};

use crate::capture::Frame;
use crate::convert::Converter;
use crate::d3d::{Gpu, VENDOR_AMD, VENDOR_INTEL, VENDOR_NVIDIA};
use crate::ffi::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Hevc,
    H264,
    Av1,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoCfg {
    pub codec: Codec,
    /// Constant-quality level, lower is better (roughly 18 = near lossless, 28 = small files).
    pub quality: u32,
    /// Upper limit for the bitrate, bits per second.
    pub max_bitrate: i64,
    /// Distance between keyframes in seconds; also the precision of "last N seconds".
    pub gop_secs: u32,
}

/// One encoded packet. The payload is shared, so the ring buffer and the file writer
/// can both hold it without copying.
#[derive(Clone)]
pub struct Pkt {
    pub data: Arc<[u8]>,
    /// Track number: 0 is video, audio tracks follow.
    pub stream: u8,
    /// Timestamps in the track's own time base.
    pub pts: i64,
    pub dts: i64,
    pub dur: i64,
    pub key: bool,
    /// Presentation time in microseconds on the clock shared by all tracks.
    pub us: i64,
}

impl Pkt {
    pub fn is_video_key(&self) -> bool {
        self.stream == 0 && self.key
    }
}

/// Everything a muxer needs to know about one track.
pub struct StreamInfo {
    pub par: *mut AVCodecParameters,
    pub time_base: AVRational,
    /// Container tag override, 0 = let the muxer choose.
    pub tag: u32,
    /// Track name shown by editors and players.
    pub title: &'static str,
}
unsafe impl Send for StreamInfo {}
unsafe impl Sync for StreamInfo {}
impl Drop for StreamInfo {
    fn drop(&mut self) {
        unsafe { avcodec_parameters_free(&mut self.par) }
    }
}

pub struct Encoder {
    ctx: *mut AVCodecContext,
    dev_ref: *mut AVBufferRef,
    frames_ref: *mut AVBufferRef,
    pkt: *mut AVPacket,
    /// Last picture sent; re-sent as is when the screen did not change.
    prev: *mut AVFrame,
    conv: Converter,
    width: u32,
    height: u32,
    origin: (u32, u32),
    codec: Codec,
    fps: u32,
    pub name: String,
}

fn encoder_name(vendor: u32, codec: Codec) -> Result<&'static str> {
    Ok(match (vendor, codec) {
        (VENDOR_NVIDIA, Codec::Hevc) => "hevc_nvenc",
        (VENDOR_NVIDIA, Codec::H264) => "h264_nvenc",
        (VENDOR_NVIDIA, Codec::Av1) => "av1_nvenc",
        (VENDOR_AMD, Codec::Hevc) => "hevc_amf",
        (VENDOR_AMD, Codec::H264) => "h264_amf",
        (VENDOR_AMD, Codec::Av1) => "av1_amf",
        (VENDOR_INTEL, _) => bail!("Intel Quick Sync is not supported yet"),
        _ => bail!("this graphics adapter has no supported hardware encoder"),
    })
}

unsafe fn opt(obj: *mut c_void, key: &str, value: &str) {
    let k = CString::new(key).unwrap();
    let v = CString::new(value).unwrap();
    let r = av_opt_set(obj, k.as_ptr(), v.as_ptr(), 0);
    if r < 0 {
        crate::log::line(&format!("encoder option {key}={value} rejected: {}", err_text(r)));
    }
}

impl Encoder {
    /// `origin` is the top-left corner of the recorded area inside the captured picture,
    /// `source` is the size of that picture.
    pub fn new(
        gpu: &Gpu,
        source: (u32, u32),
        width: u32,
        height: u32,
        fps: u32,
        cfg: &VideoCfg,
        origin: (u32, u32),
    ) -> Result<Self> {
        let name = encoder_name(gpu.vendor, cfg.codec)?;
        let mut enc = Encoder {
            ctx: null_mut(),
            dev_ref: null_mut(),
            frames_ref: null_mut(),
            pkt: null_mut(),
            prev: null_mut(),
            conv: Converter::new(gpu, source.0, source.1, width, height, fps)?,
            width,
            height,
            origin,
            codec: cfg.codec,
            fps,
            name: name.to_string(),
        };
        unsafe {
            let cname = CString::new(name).unwrap();
            let codec = avcodec_find_encoder_by_name(cname.as_ptr());
            if codec.is_null() {
                bail!("encoder {name} is missing from the FFmpeg build");
            }

            // Wrap our own D3D11 device so FFmpeg allocates textures on it.
            enc.dev_ref = av_hwdevice_ctx_alloc(AV_HWDEVICE_TYPE_D3D11VA);
            if enc.dev_ref.is_null() {
                bail!("av_hwdevice_ctx_alloc failed");
            }
            let dev = (*enc.dev_ref).data as *mut AVHWDeviceContext;
            let dev_hw = (*dev).hwctx as *mut AVD3D11VADeviceContext;
            (*dev_hw).device = gpu.device.clone().into_raw(); // FFmpeg releases this reference
            chk(av_hwdevice_ctx_init(enc.dev_ref), "av_hwdevice_ctx_init")?;

            enc.frames_ref = av_hwframe_ctx_alloc(enc.dev_ref);
            if enc.frames_ref.is_null() {
                bail!("av_hwframe_ctx_alloc failed");
            }
            let frames = (*enc.frames_ref).data as *mut AVHWFramesContext;
            (*frames).format = AV_PIX_FMT_D3D11;
            (*frames).sw_format = AV_PIX_FMT_NV12;
            (*frames).width = width as i32;
            (*frames).height = height as i32;
            (*frames).initial_pool_size = 0; // textures are created on demand and reused
            let frames_hw = (*frames).hwctx as *mut AVD3D11VAFramesContext;
            (*frames_hw).BindFlags = D3D11_BIND_RENDER_TARGET.0 as u32;
            chk(av_hwframe_ctx_init(enc.frames_ref), "av_hwframe_ctx_init")?;

            enc.ctx = avcodec_alloc_context3(codec);
            if enc.ctx.is_null() {
                bail!("avcodec_alloc_context3 failed");
            }
            let c = &mut *enc.ctx;
            c.width = width as i32;
            c.height = height as i32;
            c.time_base = AVRational { num: 1, den: fps as i32 };
            c.framerate = AVRational { num: fps as i32, den: 1 };
            c.pix_fmt = AV_PIX_FMT_D3D11;
            c.gop_size = (fps * cfg.gop_secs.max(1)) as i32;
            c.max_b_frames = 0;
            c.bit_rate = 0;
            c.rc_max_rate = cfg.max_bitrate;
            c.rc_buffer_size = (cfg.max_bitrate * 2).min(i32::MAX as i64) as i32;
            c.color_range = AVCOL_RANGE_MPEG;
            c.colorspace = AVCOL_SPC_BT709;
            c.color_primaries = AVCOL_PRI_BT709;
            c.color_trc = AVCOL_TRC_BT709;
            c.flags |= AV_CODEC_FLAG_GLOBAL_HEADER as i32;
            c.hw_frames_ctx = av_buffer_ref(enc.frames_ref);

            let q = cfg.quality.to_string();
            match gpu.vendor {
                VENDOR_NVIDIA => {
                    opt(c.priv_data, "preset", "p4");
                    opt(c.priv_data, "tune", "hq");
                    opt(c.priv_data, "rc", "vbr");
                    opt(c.priv_data, "cq", &q);
                    opt(c.priv_data, "forced-idr", "1");
                    opt(c.priv_data, "delay", "0");
                }
                VENDOR_AMD => {
                    opt(c.priv_data, "usage", "transcoding");
                    opt(c.priv_data, "quality", "balanced");
                    opt(c.priv_data, "rc", "qvbr");
                    opt(c.priv_data, "qvbr_quality_level", &q);
                }
                _ => {}
            }

            chk(avcodec_open2(enc.ctx, codec, null_mut()), &format!("cannot open {name}"))?;
            enc.pkt = av_packet_alloc();
        }
        Ok(enc)
    }

    pub fn info(&self) -> Result<Arc<StreamInfo>> {
        unsafe {
            let par = avcodec_parameters_alloc();
            if par.is_null() {
                bail!("avcodec_parameters_alloc failed");
            }
            // hvc1 (not hev1) is what QuickTime, Resolve and phones expect for HEVC.
            let tag = if self.codec == Codec::Hevc { mktag(b'h', b'v', b'c', b'1') } else { 0 };
            let info = StreamInfo { par, time_base: (*self.ctx).time_base, tag, title: "Video" };
            chk(avcodec_parameters_from_context(par, self.ctx), "avcodec_parameters_from_context")?;
            Ok(Arc::new(info))
        }
    }

    /// Encodes one tick of the timeline. `frame` is a fresh picture, or None to repeat the
    /// previous one (the screen did not change). Emits zero or more packets through `out`.
    /// Returns false when there was nothing to encode yet (no picture has arrived so far).
    pub fn encode(
        &mut self,
        frame: Option<&Frame>,
        pts: i64,
        force_key: bool,
        out: &mut dyn FnMut(Pkt),
    ) -> Result<bool> {
        unsafe {
            if let Some(f) = frame {
                let mut av = av_frame_alloc();
                if av.is_null() {
                    bail!("av_frame_alloc failed");
                }
                if let Err(e) = chk(av_hwframe_get_buffer(self.frames_ref, av, 0), "av_hwframe_get_buffer") {
                    av_frame_free(&mut av);
                    return Err(e);
                }
                let raw = (*av).data[0] as *mut c_void;
                let converted = match ID3D11Texture2D::from_raw_borrowed(&raw) {
                    Some(dst) => {
                        let (ox, oy) = self.origin;
                        let w = self.width.min(f.width.saturating_sub(ox));
                        let h = self.height.min(f.height.saturating_sub(oy));
                        let area = RECT {
                            left: ox as i32,
                            top: oy as i32,
                            right: (ox + w) as i32,
                            bottom: (oy + h) as i32,
                        };
                        if w > 0 && h > 0 {
                            self.conv.convert(&f.texture, f.width, f.height, area, dst)
                        } else {
                            Ok(())
                        }
                    }
                    None => Err(anyhow!("encoder frame has no texture")),
                };
                if let Err(e) = converted {
                    av_frame_free(&mut av);
                    return Err(e);
                }
                av_frame_free(&mut self.prev);
                self.prev = av;
            }
            if self.prev.is_null() {
                return Ok(false); // nothing captured yet
            }
            (*self.prev).pts = pts;
            (*self.prev).pict_type = if force_key { AV_PICTURE_TYPE_I } else { AV_PICTURE_TYPE_NONE };
            chk(avcodec_send_frame(self.ctx, self.prev), "avcodec_send_frame")?;
            self.drain(out)?;
            Ok(true)
        }
    }

    /// Flushes whatever the encoder still holds. Call once before dropping.
    pub fn finish(&mut self, out: &mut dyn FnMut(Pkt)) -> Result<()> {
        unsafe {
            let r = avcodec_send_frame(self.ctx, null());
            if r < 0 && r != AVERROR_EOF {
                chk(r, "flush")?;
            }
            self.drain(out)
        }
    }

    unsafe fn drain(&mut self, out: &mut dyn FnMut(Pkt)) -> Result<()> {
        loop {
            let r = avcodec_receive_packet(self.ctx, self.pkt);
            if r == AVERROR_EAGAIN || r == AVERROR_EOF {
                return Ok(());
            }
            chk(r, "avcodec_receive_packet")?;
            let p = &*self.pkt;
            let data = std::slice::from_raw_parts(p.data, p.size as usize);
            out(Pkt {
                data: Arc::from(data),
                stream: 0,
                us: p.pts * 1_000_000 / self.fps as i64,
                pts: p.pts,
                dts: p.dts,
                dur: if p.duration > 0 { p.duration } else { 1 },
                key: p.flags & AV_PKT_FLAG_KEY as i32 != 0,
            });
            av_packet_unref(self.pkt);
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            av_frame_free(&mut self.prev);
            av_packet_free(&mut self.pkt);
            avcodec_free_context(&mut self.ctx);
            av_buffer_unref(&mut self.frames_ref);
            av_buffer_unref(&mut self.dev_ref);
        }
    }
}
