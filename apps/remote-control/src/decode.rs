//! H.264 and HEVC decoding with the Media Foundation decoders that ship with Windows,
//! running on the GPU (D3D11). Low-latency mode makes it hand out each frame
//! as soon as it is decoded instead of keeping a reorder buffer.

use anyhow::{Context, Result, bail};
use windows::{
    Win32::{
        Graphics::{
            Direct3D11::{
                D3D11_BIND_DECODER, D3D11_BIND_SHADER_RESOURCE, D3D11_FORMAT_SUPPORT_SHADER_SAMPLE,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
                D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT, D3D11_VIDEO_USAGE_OPTIMAL_SPEED, ID3D11Device,
                ID3D11Texture2D, ID3D11VideoDevice,
            },
            Dxgi::Common::{DXGI_FORMAT_AYUV, DXGI_FORMAT_NV12, DXGI_RATIONAL},
        },
        Media::MediaFoundation::*,
        System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance},
    },
    core::{GUID, Interface},
};

use crate::mf::{self, Codec, pack};

/// A decoded frame: one slice of the decoder's texture array.
pub struct Picture {
    pub tex: ID3D11Texture2D,
    pub slice: u32,
    /// Holding the sample keeps the decoder from reusing the texture.
    _sample: IMFSample,
}

pub struct Decoder {
    mft: IMFTransform,
    _mgr: IMFDXGIDeviceManager,
    device: ID3D11Device,
    size: (u32, u32),
    index: i64,
}

impl Decoder {
    pub fn new(device: &ID3D11Device, width: u32, height: u32, codec: Codec) -> Result<Decoder> {
        mf::init_thread()?;
        unsafe {
            let mft = create_mft(codec)?;
            let attrs = mft.GetAttributes()?;
            if attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
                bail!("the {} decoder cannot use D3D11", codec.name());
            }
            attrs.SetUINT32(&MF_LOW_LATENCY, 1)?;
            let mgr = mf::attach_device(&mft, device)?;

            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &codec.subtype())?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            input.SetUINT64(&MF_MT_FRAME_RATE, pack(60, 1))?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            mft.SetInputType(0, &input, 0).context("decoder SetInputType")?;
            let mut d = Decoder { mft, _mgr: mgr, device: device.clone(), size: (width, height), index: 0 };
            d.pick_output()?;
            d.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            d.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(d)
        }
    }

    /// Whether this machine can decode `codec` on `device` (for HEVC Windows
    /// needs the "HEVC Video Extensions" package from the Store).
    pub fn available(device: &ID3D11Device, codec: Codec) -> Result<()> {
        Decoder::new(device, 1920, 1080, codec).map(drop)
    }

    /// Whether the graphics card decodes HEVC Main 4:4:4 into AYUV and its
    /// video processor draws AYUV (the HEVC decoder itself must be there too).
    pub fn can_444(device: &ID3D11Device) -> bool {
        const HEVC_MAIN_444: GUID = GUID::from_u128(0x4008018f_f537_4b36_98cf_61af8a2c1a33);
        unsafe {
            let Ok(vd) = device.cast::<ID3D11VideoDevice>() else { return false };
            let has = (0..vd.GetVideoDecoderProfileCount()).any(|i| vd.GetVideoDecoderProfile(i).is_ok_and(|g| g == HEVC_MAIN_444));
            if !has || !vd.CheckVideoDecoderFormat(&HEVC_MAIN_444, DXGI_FORMAT_AYUV).is_ok_and(|b| b.as_bool()) {
                return false;
            }
            let rate = DXGI_RATIONAL { Numerator: 60, Denominator: 1 };
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: 1920,
                InputHeight: 1080,
                OutputFrameRate: rate,
                OutputWidth: 1920,
                OutputHeight: 1080,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let Ok(en) = vd.CreateVideoProcessorEnumerator(&desc) else { return false };
            en.CheckVideoProcessorFormat(DXGI_FORMAT_AYUV)
                .is_ok_and(|f| f & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32 != 0)
        }
    }

    /// NV12 for 4:2:0 streams; a 4:4:4 stream (HEVC Main 4:4:4) offers no
    /// NV12 once its first frame is parsed, and comes out as AYUV.
    fn pick_output(&mut self) -> Result<()> {
        unsafe {
            let types: Vec<IMFMediaType> = (0..).map_while(|i| self.mft.GetOutputAvailableType(0, i).ok()).collect();
            let (t, format) = [(MFVideoFormat_NV12, DXGI_FORMAT_NV12), (MFVideoFormat_AYUV, DXGI_FORMAT_AYUV)]
                .iter()
                .find_map(|(f, d)| types.iter().find(|t| mf::subtype(t) == Some(*f)).map(|t| (t, *d)))
                .context("decoder has no NV12 or AYUV output")?;
            // Pictures the viewer's shader can read without a copy, where the
            // graphics card allows it.
            let readable = self.device.CheckFormatSupport(format).is_ok_and(|s| s & D3D11_FORMAT_SUPPORT_SHADER_SAMPLE.0 as u32 != 0);
            let bind = D3D11_BIND_DECODER.0 as u32 | if readable { D3D11_BIND_SHADER_RESOURCE.0 as u32 } else { 0 };
            if let Ok(a) = self.mft.GetOutputStreamAttributes(0) {
                let _ = a.SetUINT32(&MF_SA_D3D11_BINDFLAGS, bind);
            }
            self.mft.SetOutputType(0, t, 0).context("decoder SetOutputType")?;
            if let Ok(v) = t.GetUINT64(&MF_MT_FRAME_SIZE) {
                self.size = ((v >> 32) as u32, v as u32);
            }
            Ok(())
        }
    }

    /// Feeds one frame; returns the newest decoded picture, if any came out.
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<Picture>> {
        let sample = mf::sample_from_bytes(data, self.index * 166_667)?;
        self.index += 1;
        let mut last = None;
        unsafe {
            if let Err(e) = self.mft.ProcessInput(0, &sample, 0) {
                if e.code() != MF_E_NOTACCEPTING {
                    return Err(e).context("decoder ProcessInput");
                }
                last = self.drain()?;
                self.mft.ProcessInput(0, &sample, 0).context("decoder ProcessInput")?;
            }
        }
        Ok(self.drain()?.or(last))
    }

    fn drain(&mut self) -> Result<Option<Picture>> {
        let mut last = None;
        loop {
            unsafe {
                let mut buf = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, ..Default::default() }];
                let mut status = 0u32;
                let r = self.mft.ProcessOutput(0, &mut buf, &mut status);
                let sample = std::mem::ManuallyDrop::take(&mut buf[0].pSample);
                drop(std::mem::ManuallyDrop::take(&mut buf[0].pEvents));
                match r {
                    Ok(()) => {}
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(last),
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        self.pick_output()?;
                        continue;
                    }
                    Err(e) => return Err(e).context("decoder ProcessOutput"),
                }
                let sample = sample.context("decoder gave no sample")?;
                let dxgi: IMFDXGIBuffer = sample.GetBufferByIndex(0)?.cast().context("decoder output is not on the GPU")?;
                let mut p = std::ptr::null_mut();
                dxgi.GetResource(&ID3D11Texture2D::IID, &mut p)?;
                let tex = ID3D11Texture2D::from_raw(p);
                let slice = dxgi.GetSubresourceIndex()?;
                last = Some(Picture { tex, slice, _sample: sample });
            }
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(s) = self.mft.cast::<IMFShutdown>() {
                let _ = s.Shutdown();
            }
        }
    }
}

/// The decoder Windows has for `codec`, best first. For HEVC that is the one
/// from the "HEVC Video Extensions" package (Microsoft Store); without the
/// package there is none.
fn create_mft(codec: Codec) -> Result<IMFTransform> {
    if codec == Codec::H264 {
        return unsafe { CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER) }.context("H.264 decoder");
    }
    let list = mf::find_mfts(MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_ALL, codec.subtype(), MFVideoFormat_NV12, None)?;
    let mut err = anyhow::anyhow!("no {} decoder installed", codec.name());
    for a in &list {
        match unsafe { a.ActivateObject::<IMFTransform>() } {
            Ok(m) => return Ok(m),
            Err(e) => err = anyhow::Error::from(e).context(format!("{} decoder {}", codec.name(), mf::friendly_name(a))),
        }
    }
    Err(err)
}
