//! Media Foundation helpers shared by the encoder and decoder.

use std::sync::Once;

use anyhow::{Context, Result};
use windows::{
    Win32::{
        Foundation::LUID,
        Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D},
        Media::MediaFoundation::{
            ICodecAPI, IMFActivate, IMFAttributes, IMFDXGIDeviceManager, IMFMediaBuffer, IMFMediaType, IMFSample,
            IMFTransform, MFCreateAttributes, MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer,
            MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFSTARTUP_NOSOCKET, MFStartup, MFTEnum2,
            MFT_ENUM_ADAPTER_LUID, MFT_ENUM_FLAG, MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_SET_D3D_MANAGER,
            MFT_REGISTER_TYPE_INFO, MFVideoFormat_H264, MFVideoFormat_HEVC, MF_VERSION,
        },
        System::{
            Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree},
            Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4, VT_UI8},
        },
    },
    core::{GUID, Interface, PWSTR},
};

/// COM (multithreaded) for this thread, Media Foundation for the process.
pub fn init_thread() -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    static START: Once = Once::new();
    let mut r = Ok(());
    START.call_once(|| r = unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }.context("MFStartup"));
    r
}

fn variant(vt: windows::Win32::System::Variant::VARENUM, val: VARIANT_0_0_0) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(VARIANT_0_0 {
                vt,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: val,
            }),
        },
    }
}

pub fn set_u32(codec: &ICodecAPI, key: &GUID, v: u32) -> Result<()> {
    let var = variant(VT_UI4, VARIANT_0_0_0 { ulVal: v });
    unsafe { codec.SetValue(key, &var) }.map_err(Into::into)
}

pub fn set_u64(codec: &ICodecAPI, key: &GUID, v: u64) -> Result<()> {
    let var = variant(VT_UI8, VARIANT_0_0_0 { ullVal: v });
    unsafe { codec.SetValue(key, &var) }.map_err(Into::into)
}

pub fn set_bool(codec: &ICodecAPI, key: &GUID, v: bool) -> Result<()> {
    let var = variant(VT_BOOL, VARIANT_0_0_0 { boolVal: windows::Win32::Foundation::VARIANT_BOOL(if v { -1 } else { 0 }) });
    unsafe { codec.SetValue(key, &var) }.map_err(Into::into)
}

/// The integer in a VARIANT the codec API hands out, if it holds one.
fn variant_u64(v: &VARIANT) -> Option<u64> {
    unsafe {
        let inner = &v.Anonymous.Anonymous;
        let val = &inner.Anonymous;
        Some(match inner.vt {
            VT_UI4 => val.ulVal as u64,
            VT_UI8 => val.ullVal,
            VT_BOOL => (val.boolVal.0 != 0) as u64,
            windows::Win32::System::Variant::VT_I4 => val.lVal as u64,
            _ => return None,
        })
    }
}

pub fn get_u64(codec: &ICodecAPI, key: &GUID) -> Option<u64> {
    unsafe { codec.GetValue(key) }.ok().as_ref().and_then(variant_u64)
}

/// (supported, can be changed while encoding). Both answer S_FALSE for "no",
/// which the windows crate would count as success.
pub fn support(codec: &ICodecAPI, key: &GUID) -> (bool, bool) {
    unsafe {
        let vt = Interface::vtable(codec);
        let s = (vt.IsSupported)(Interface::as_raw(codec), key) == windows::Win32::Foundation::S_OK;
        let m = (vt.IsModifiable)(Interface::as_raw(codec), key) == windows::Win32::Foundation::S_OK;
        (s, m)
    }
}

/// (min, max, step) the codec reports for a property with a linear range.
pub fn range(codec: &ICodecAPI, key: &GUID) -> Option<(u64, u64, u64)> {
    let (mut lo, mut hi, mut step) = (VARIANT::default(), VARIANT::default(), VARIANT::default());
    unsafe { codec.GetParameterRange(key, &mut lo, &mut hi, &mut step) }.ok()?;
    Some((variant_u64(&lo)?, variant_u64(&hi)?, variant_u64(&step).unwrap_or(0)))
}

/// Lets `mft` use `device` for hardware work. Keep the manager alive as long as the MFT.
pub fn attach_device(mft: &IMFTransform, device: &ID3D11Device) -> Result<IMFDXGIDeviceManager> {
    unsafe {
        let mut token = 0u32;
        let mut mgr = None;
        MFCreateDXGIDeviceManager(&mut token, &mut mgr).context("MFCreateDXGIDeviceManager")?;
        let mgr: IMFDXGIDeviceManager = mgr.context("no device manager")?;
        mgr.ResetDevice(device, token).context("ResetDevice")?;
        mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr.as_raw() as usize).context("SET_D3D_MANAGER")?;
        Ok(mgr)
    }
}

pub fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

pub fn sample_from_texture(tex: &ID3D11Texture2D, time_100ns: i64, dur_100ns: i64) -> Result<IMFSample> {
    unsafe {
        let buf = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false).context("MFCreateDXGISurfaceBuffer")?;
        let s = MFCreateSample()?;
        s.AddBuffer(&buf)?;
        s.SetSampleTime(time_100ns)?;
        s.SetSampleDuration(dur_100ns)?;
        Ok(s)
    }
}

pub fn sample_from_bytes(data: &[u8], time_100ns: i64) -> Result<IMFSample> {
    unsafe {
        let buf: IMFMediaBuffer = MFCreateMemoryBuffer(data.len() as u32)?;
        let mut p = std::ptr::null_mut();
        buf.Lock(&mut p, None, None)?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
        buf.Unlock()?;
        buf.SetCurrentLength(data.len() as u32)?;
        let s = MFCreateSample()?;
        s.AddBuffer(&buf)?;
        s.SetSampleTime(time_100ns)?;
        Ok(s)
    }
}

pub fn bytes_from_sample(s: &IMFSample) -> Result<Vec<u8>> {
    unsafe {
        let buf = s.ConvertToContiguousBuffer()?;
        let mut p = std::ptr::null_mut();
        let mut len = 0u32;
        buf.Lock(&mut p, None, Some(&mut len))?;
        let v = std::slice::from_raw_parts(p, len as usize).to_vec();
        buf.Unlock()?;
        Ok(v)
    }
}

pub fn subtype(t: &IMFMediaType) -> Option<GUID> {
    unsafe { t.GetGUID(&windows::Win32::Media::MediaFoundation::MF_MT_SUBTYPE).ok() }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}

impl Codec {
    pub fn subtype(self) -> GUID {
        match self {
            Codec::H264 => MFVideoFormat_H264,
            Codec::Hevc => MFVideoFormat_HEVC,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::Hevc => "HEVC",
        }
    }

    /// The NAL unit type of a header byte.
    pub fn nal_type(self, header: u8) -> u8 {
        match self {
            Codec::H264 => header & 0x1f,
            Codec::Hevc => (header >> 1) & 0x3f,
        }
    }

    /// Whether `nal_type` is the first parameter set in front of a key frame
    /// (SPS for H.264, VPS for HEVC).
    pub fn is_stream_header(self, nal_type: u8) -> bool {
        match self {
            Codec::H264 => nal_type == 7,
            Codec::Hevc => nal_type == 32,
        }
    }
}

/// MFTs of `category` that take `input` and give `output`, best first;
/// with `luid`, only those on that graphics adapter.
pub fn find_mfts(category: GUID, flags: MFT_ENUM_FLAG, input: GUID, output: GUID, luid: Option<LUID>) -> Result<Vec<IMFActivate>> {
    unsafe {
        let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: input };
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: output };
        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 1)?;
        let attrs: IMFAttributes = attrs.context("no attributes")?;
        if let Some(luid) = luid {
            let b = std::slice::from_raw_parts(&luid as *const LUID as *const u8, size_of::<LUID>());
            attrs.SetBlob(&MFT_ENUM_ADAPTER_LUID, b)?;
        }
        let mut pp: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut n = 0u32;
        MFTEnum2(category, flags, Some(&input), Some(&output), &attrs, &mut pp, &mut n).context("MFTEnum2")?;
        let mut out = Vec::new();
        for i in 0..n as usize {
            if let Some(a) = std::ptr::read(pp.add(i)) {
                out.push(a);
            }
        }
        if !pp.is_null() {
            CoTaskMemFree(Some(pp as *const _));
        }
        Ok(out)
    }
}

pub fn friendly_name(a: &IMFActivate) -> String {
    unsafe {
        let mut p = PWSTR::null();
        let mut len = 0u32;
        if a.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut len).is_err() {
            return "unknown".into();
        }
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

/// Bytes taken by filler data NAL units (type 12), start codes included.
/// CBR encoders pad frames with them; a decoder ignores them.
pub fn filler_bytes(b: &[u8]) -> usize {
    nal_units(b).filter(|u| u.first().is_some_and(|h| h & 0x1f == 12)).map(|u| u.len() + 3).sum()
}

/// The NAL units of an Annex B stream, without start codes.
pub fn nal_units(b: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 2 < b.len() {
        if b[i] == 0 && b[i + 1] == 0 && b[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let ends: Vec<usize> = starts
        .iter()
        .skip(1)
        .map(|&s| {
            // A four-byte start code leaves one more zero at the end of the unit before.
            let e = s - 3;
            if e > 0 && b[e - 1] == 0 { e - 1 } else { e }
        })
        .chain(std::iter::once(b.len()))
        .collect();
    starts.into_iter().zip(ends).map(move |(s, e)| &b[s..e.max(s)])
}

/// Types of the NAL units in an Annex B byte stream.
pub fn nal_types(b: &[u8], codec: Codec) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 < b.len() {
        if b[i] == 0 && b[i + 1] == 0 && b[i + 2] == 1 {
            out.push(codec.nal_type(b[i + 3]));
            i += 3;
        } else {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn finds_nal_types() {
        use super::Codec;
        let b = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9];
        assert_eq!(super::nal_types(&b, Codec::H264), vec![7, 8, 5]);
        // VPS, SPS, PPS, IDR_W_RADL
        let h = [0, 0, 0, 1, 0x40, 1, 0, 0, 1, 0x42, 1, 0, 0, 1, 0x44, 1, 0, 0, 1, 0x26, 1];
        assert_eq!(super::nal_types(&h, Codec::Hevc), vec![32, 33, 34, 19]);
        assert!(Codec::Hevc.is_stream_header(32) && Codec::H264.is_stream_header(7));
    }

    #[test]
    fn counts_filler() {
        let b = [0, 0, 0, 1, 0x65, 9, 9, 0, 0, 0, 1, 0x0c, 0xff, 0xff, 0x80];
        let units: Vec<&[u8]> = super::nal_units(&b).collect();
        assert_eq!(units, vec![&[0x65, 9, 9][..], &[0x0c, 0xff, 0xff, 0x80][..]]);
        assert_eq!(super::filler_bytes(&b), 7);
    }
}
