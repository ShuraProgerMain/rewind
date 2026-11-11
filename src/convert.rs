//! BGRA -> NV12 on the GPU (D3D11 video processor), with an explicit BT.709 matrix.
//! Encoders convert RGB with BT.601 on their own, which shifts colours in editors that
//! (rightly) assume BT.709 for HD video. The same pass also crops, and can scale.

use std::collections::HashMap;
use std::mem::ManuallyDrop;

use anyhow::{anyhow, Result};
use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT_B8G8R8A8_UNORM,
    DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};

use crate::d3d::Gpu;

pub struct Converter {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    vp: ID3D11VideoProcessor,
    /// Views of capture buffers, newest last. The texture is kept alive so its address stays unique.
    in_views: Vec<(usize, ID3D11Texture2D, ID3D11VideoProcessorInputView)>,
    out_views: HashMap<usize, ID3D11VideoProcessorOutputView>,
    /// Used only if the driver refuses to read capture buffers directly.
    staging: Option<(ID3D11Texture2D, ID3D11VideoProcessorInputView, u32, u32)>,
    direct: bool,
    out_w: u32,
    out_h: u32,
}

impl Converter {
    pub fn new(gpu: &Gpu, in_w: u32, in_h: u32, out_w: u32, out_h: u32, fps: u32) -> Result<Self> {
        unsafe {
            let vdev: ID3D11VideoDevice = gpu.device.cast()?;
            let vctx: ID3D11VideoContext = gpu.context.cast()?;
            let rate = DXGI_RATIONAL { Numerator: fps, Denominator: 1 };
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: in_w,
                InputHeight: in_h,
                OutputFrameRate: rate,
                OutputWidth: out_w,
                OutputHeight: out_h,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let enumerator = vdev.CreateVideoProcessorEnumerator(&desc)?;
            let vp = vdev.CreateVideoProcessor(&enumerator, 0)?;

            // A plain colour conversion: no driver "enhancements" (denoise, sharpening...).
            vctx.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);
            vctx.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            vctx.VideoProcessorSetOutputTargetRect(
                &vp,
                true,
                Some(&RECT { left: 0, top: 0, right: out_w as i32, bottom: out_h as i32 }),
            );
            match vctx.cast::<ID3D11VideoContext1>() {
                Ok(v1) => {
                    v1.VideoProcessorSetStreamColorSpace1(&vp, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
                    v1.VideoProcessorSetOutputColorSpace1(&vp, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
                }
                Err(_) => {
                    // Legacy bit field: input = full-range RGB; output = BT.709 matrix (bit 2), 16-235 (bits 4-5 = 1).
                    let input = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
                    let output = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: (1 << 2) | (1 << 4) };
                    vctx.VideoProcessorSetStreamColorSpace(&vp, 0, &input);
                    vctx.VideoProcessorSetOutputColorSpace(&vp, &output);
                }
            }

            Ok(Self {
                device: gpu.device.clone(),
                ctx: gpu.context.clone(),
                vdev,
                vctx,
                enumerator,
                vp,
                in_views: Vec::new(),
                out_views: HashMap::new(),
                staging: None,
                direct: true,
                out_w,
                out_h,
            })
        }
    }

    unsafe fn make_input_view(&self, tex: &ID3D11Texture2D) -> Result<ID3D11VideoProcessorInputView> {
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
            },
        };
        let mut view = None;
        self.vdev.CreateVideoProcessorInputView(tex, &self.enumerator, &desc, Some(&mut view))?;
        view.ok_or_else(|| anyhow!("no video processor input view"))
    }

    unsafe fn input_view(&mut self, src: &ID3D11Texture2D, w: u32, h: u32) -> Result<ID3D11VideoProcessorInputView> {
        if self.direct {
            let key = src.as_raw() as usize;
            if let Some((_, _, v)) = self.in_views.iter().find(|(k, _, _)| *k == key) {
                return Ok(v.clone());
            }
            match self.make_input_view(src) {
                Ok(v) => {
                    if self.in_views.len() >= 4 {
                        self.in_views.remove(0);
                    }
                    self.in_views.push((key, src.clone(), v.clone()));
                    return Ok(v);
                }
                Err(_) => self.direct = false, // fall through to the copy path, permanently
            }
        }
        // Copy into a texture of our own that the video processor is guaranteed to accept.
        if !matches!(&self.staging, Some((_, _, sw, sh)) if *sw == w && *sh == h) {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut tex = None;
            self.device.CreateTexture2D(&desc, None, Some(&mut tex))?;
            let tex = tex.ok_or_else(|| anyhow!("cannot create staging texture"))?;
            let view = self.make_input_view(&tex)?;
            self.staging = Some((tex, view, w, h));
        }
        let (tex, view, _, _) = self.staging.as_ref().unwrap();
        let area = D3D11_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 };
        self.ctx.CopySubresourceRegion(tex, 0, 0, 0, 0, src, 0, Some(&area));
        Ok(view.clone())
    }

    /// Converts the `area` of `src` (whose valid content is `src_w` x `src_h`) into the NV12
    /// texture `dst`, placing it at the top-left corner; anything left over is black.
    pub fn convert(
        &mut self,
        src: &ID3D11Texture2D,
        src_w: u32,
        src_h: u32,
        area: RECT,
        dst: &ID3D11Texture2D,
    ) -> Result<()> {
        unsafe {
            let in_view = self.input_view(src, src_w, src_h)?;
            let key = dst.as_raw() as usize;
            if !self.out_views.contains_key(&key) {
                let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
                };
                let mut view = None;
                self.vdev.CreateVideoProcessorOutputView(dst, &self.enumerator, &desc, Some(&mut view))?;
                self.out_views.insert(key, view.ok_or_else(|| anyhow!("no video processor output view"))?);
            }
            let out_view = &self.out_views[&key];

            let w = (area.right - area.left).min(self.out_w as i32);
            let h = (area.bottom - area.top).min(self.out_h as i32);
            let src_rect = RECT { left: area.left, top: area.top, right: area.left + w, bottom: area.top + h };
            let dst_rect = RECT { left: 0, top: 0, right: w, bottom: h };
            self.vctx.VideoProcessorSetStreamSourceRect(&self.vp, 0, true, Some(&src_rect));
            self.vctx.VideoProcessorSetStreamDestRect(&self.vp, 0, true, Some(&dst_rect));

            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(in_view)),
                ..Default::default()
            };
            let result = self.vctx.VideoProcessorBlt(&self.vp, out_view, 0, std::slice::from_ref(&stream));
            ManuallyDrop::drop(&mut stream.pInputSurface);
            result?;
            // Submit before the capture buffer goes back to the compositor.
            self.ctx.Flush();
            Ok(())
        }
    }
}
