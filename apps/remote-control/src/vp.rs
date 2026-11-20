//! D3D11 video processor: colour conversion and scaling on the GPU.
//! Host: desktop BGRA -> NV12 for the encoder. Viewer: decoded NV12 -> BGRA
//! back buffer, letterboxed to the window.

use std::mem::ManuallyDrop;

use anyhow::{Context, Result};
use windows::{
    Win32::{
        Foundation::RECT,
        Graphics::{
            Direct3D11::{
                D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_0, D3D11_VIDEO_COLOR_RGBA,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
                D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
                D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
                D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D,
                D3D11_VIDEO_PROCESSOR_FEATURE_CAPS_ROTATION, D3D11_VIDEO_PROCESSOR_ROTATION_90,
                D3D11_VIDEO_PROCESSOR_ROTATION_180, D3D11_VIDEO_PROCESSOR_ROTATION_270,
                D3D11_VIDEO_PROCESSOR_ROTATION_IDENTITY,
                D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Texture2D, ID3D11VideoContext1, ID3D11VideoDevice,
                ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
                ID3D11VideoProcessorOutputView,
            },
            Dxgi::Common::{DXGI_COLOR_SPACE_TYPE, DXGI_RATIONAL},
        },
    },
    core::Interface,
};

use crate::gpu::Gpu;

pub use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709 as RGB_FULL, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709 as YUV_709,
};

pub struct Vp {
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext1,
    en: ID3D11VideoProcessorEnumerator,
    vp: ID3D11VideoProcessor,
    pub input: (u32, u32),
    pub output: (u32, u32),
}

impl Vp {
    pub fn new(
        gpu: &Gpu,
        input: (u32, u32),
        output: (u32, u32),
        in_cs: DXGI_COLOR_SPACE_TYPE,
        out_cs: DXGI_COLOR_SPACE_TYPE,
    ) -> Result<Vp> {
        unsafe {
            let vdev: ID3D11VideoDevice = gpu.device.cast().context("no video device")?;
            let vctx: ID3D11VideoContext1 = gpu.ctx.cast().context("no ID3D11VideoContext1")?;
            let rate = DXGI_RATIONAL { Numerator: 60, Denominator: 1 };
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: input.0,
                InputHeight: input.1,
                OutputFrameRate: rate,
                OutputWidth: output.0,
                OutputHeight: output.1,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let en = vdev.CreateVideoProcessorEnumerator(&desc).context("CreateVideoProcessorEnumerator")?;
            let vp = vdev.CreateVideoProcessor(&en, 0).context("CreateVideoProcessor")?;
            vctx.VideoProcessorSetStreamColorSpace1(&vp, 0, in_cs);
            vctx.VideoProcessorSetOutputColorSpace1(&vp, out_cs);
            vctx.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            // No driver "enhancements": they cost time and change text.
            vctx.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);
            let black = D3D11_VIDEO_COLOR {
                Anonymous: D3D11_VIDEO_COLOR_0 { RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 } },
            };
            vctx.VideoProcessorSetOutputBackgroundColor(&vp, false, &black);
            Ok(Vp { vdev, vctx, en, vp, input, output })
        }
    }

    /// Turns the input `degrees` clockwise (0, 90, 180, 270) before scaling it
    /// to the output. False if this video processor cannot rotate.
    pub fn rotate(&self, degrees: u16) -> bool {
        let r = match degrees {
            90 => D3D11_VIDEO_PROCESSOR_ROTATION_90,
            180 => D3D11_VIDEO_PROCESSOR_ROTATION_180,
            270 => D3D11_VIDEO_PROCESSOR_ROTATION_270,
            _ => D3D11_VIDEO_PROCESSOR_ROTATION_IDENTITY,
        };
        unsafe {
            let mut caps = Default::default();
            let caps = match self.en.GetVideoProcessorCaps(&mut caps) {
                Ok(()) => caps.FeatureCaps,
                Err(_) => 0,
            };
            if degrees != 0 && caps & D3D11_VIDEO_PROCESSOR_FEATURE_CAPS_ROTATION.0 as u32 == 0 {
                return false;
            }
            self.vctx.VideoProcessorSetStreamRotation(&self.vp, 0, degrees != 0, r);
        }
        true
    }

    /// Pre-built views for a fixed `(src, slice) -> dst` pair (such as the host
    /// capture loop), avoiding per-frame COM view creation/destruction.
    pub fn make_views(&self, src: &ID3D11Texture2D, slice: u32, dst: &ID3D11Texture2D) -> Result<VpViews> {
        unsafe {
            let iv_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: slice },
                },
            };
            let mut iv: Option<ID3D11VideoProcessorInputView> = None;
            self.vdev
                .CreateVideoProcessorInputView(src, &self.en, &iv_desc, Some(&mut iv))
                .context("CreateVideoProcessorInputView")?;
            let iv = iv.context("no input view")?;
            let ov_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let mut ov: Option<ID3D11VideoProcessorOutputView> = None;
            self.vdev
                .CreateVideoProcessorOutputView(dst, &self.en, &ov_desc, Some(&mut ov))
                .context("CreateVideoProcessorOutputView")?;
            let ov = ov.context("no output view")?;
            Ok(VpViews { iv, ov })
        }
    }

    pub fn blt_views(&self, views: &VpViews, dest: RECT) -> Result<()> {
        unsafe {
            let src_rect = RECT { left: 0, top: 0, right: self.input.0 as i32, bottom: self.input.1 as i32 };
            let full = RECT { left: 0, top: 0, right: self.output.0 as i32, bottom: self.output.1 as i32 };
            self.vctx.VideoProcessorSetStreamSourceRect(&self.vp, 0, true, Some(&src_rect));
            self.vctx.VideoProcessorSetStreamDestRect(&self.vp, 0, true, Some(&dest));
            self.vctx.VideoProcessorSetOutputTargetRect(&self.vp, true, Some(&full));
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(views.iv.clone())),
                ..Default::default()
            };
            let r = self.vctx.VideoProcessorBlt(&self.vp, &views.ov, 0, std::slice::from_ref(&stream));
            ManuallyDrop::drop(&mut stream.pInputSurface);
            r.context("VideoProcessorBlt")
        }
    }

    /// Draws array slice `slice` of `src` into `dest` of `dst`; the rest of
    /// `dst` becomes black.
    pub fn blt(&self, src: &ID3D11Texture2D, slice: u32, dst: &ID3D11Texture2D, dest: RECT) -> Result<()> {
        let views = self.make_views(src, slice, dst)?;
        self.blt_views(&views, dest)
    }
}

pub struct VpViews {
    iv: ID3D11VideoProcessorInputView,
    ov: ID3D11VideoProcessorOutputView,
}

/// The largest rectangle with the aspect of `src` that fits in `dst`, centred.
pub fn letterbox(src: (u32, u32), dst: (u32, u32)) -> RECT {
    let (sw, sh) = (src.0.max(1) as f64, src.1.max(1) as f64);
    let (dw, dh) = (dst.0 as f64, dst.1 as f64);
    let scale = (dw / sw).min(dh / sh);
    let (w, h) = ((sw * scale).round(), (sh * scale).round());
    let (x, y) = (((dw - w) / 2.0).floor(), ((dh - h) / 2.0).floor());
    RECT { left: x as i32, top: y as i32, right: (x + w) as i32, bottom: (y + h) as i32 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letterbox_fits_and_centres() {
        let r = letterbox((2880, 1920), (1920, 1080));
        assert_eq!((r.right - r.left, r.bottom - r.top), (1620, 1080));
        assert_eq!(r.left, 150);
        let r = letterbox((3840, 2160), (1000, 1000));
        assert_eq!((r.left, r.top, r.right, r.bottom), (0, 218, 1000, 781));
    }
}
