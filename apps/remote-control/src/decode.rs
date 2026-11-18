//! H.264 and HEVC decoding with the Media Foundation decoders that ship with Windows,
//! running on the GPU (D3D11). Low-latency mode makes it hand out each frame
//! as soon as it is decoded instead of keeping a reorder buffer.

use anyhow::{Context, Result, bail};
use windows::{
    Win32::{
        Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D},
        Media::MediaFoundation::*,
        System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance},
    },
    core::Interface,
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
            let mut d = Decoder { mft, _mgr: mgr, size: (width, height), index: 0 };
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

    fn pick_output(&mut self) -> Result<()> {
        unsafe {
            let t = (0..)
                .map_while(|i| self.mft.GetOutputAvailableType(0, i).ok())
                .find(|t| mf::subtype(t) == Some(MFVideoFormat_NV12))
                .context("decoder has no NV12 output")?;
            self.mft.SetOutputType(0, &t, 0).context("decoder SetOutputType")?;
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
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
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
