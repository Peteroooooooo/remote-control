//! H.264 and HEVC encoding with the GPU vendor's Media Foundation encoder
//! (NVIDIA NVENC, Intel Quick Sync, AMD AMF all ship one). These are
//! asynchronous MFTs: they ask for input and announce output through events.
//! We keep one frame in flight and wait for its output, which is the lowest
//! latency.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::{
    Win32::{
        Foundation::LUID,
        Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D},
        Media::MediaFoundation::*,
    },
    core::{GUID, Interface},
};

use crate::mf::{self, Codec, pack};

pub struct Encoded {
    /// Annex B, with the parameter sets in front of key frames.
    pub data: Vec<u8>,
    pub key: bool,
    /// The quantiser the encoder reports for this frame, if it does.
    pub qp: Option<u32>,
    /// Zero bytes the encoder padded the frame with, already removed.
    pub padding: usize,
}

struct MfEncoder {
    mft: IMFTransform,
    events: IMFMediaEventGenerator,
    codec: ICodecAPI,
    _mgr: IMFDXGIDeviceManager,
    credits: u32,
    ready: std::collections::VecDeque<Encoded>,
    seq_header: Vec<u8>,
    format: Codec,
    fps: u32,
    index: i64,
    key_next: bool,
    rate_changed: bool,
    /// Per frame given to the encoder and not yet out: (key frame asked for,
    /// bitrate changed just before it).
    inputs: std::collections::VecDeque<(bool, bool)>,
}

enum Backend {
    Mf(MfEncoder),
    Qsv(Box<crate::qsv::QsvEncoder>),
}

pub struct Encoder {
    backend: Backend,
    pub name: String,
    pub format: Codec,
    /// Takes AYUV and encodes 4:4:4 (see `EncOptions::yuv444`).
    pub yuv444: bool,
    pub bitrate: u32,
    /// The encoder answered a bitrate change with a key frame nobody asked
    /// for (Intel Quick Sync MFT does). Bitrate changes are then expensive: a
    /// key frame costs several normal frames' worth of quality.
    pub rekeys_on_rate_change: bool,
}

/// Hardware encoders for `codec` on the adapter with `luid`, best first.
fn find_encoders(luid: LUID, codec: Codec) -> Result<Vec<IMFActivate>> {
    mf::find_mfts(
        MFT_CATEGORY_VIDEO_ENCODER,
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
        MFVideoFormat_NV12,
        codec.subtype(),
        Some(luid),
    )
}

/// The name of the hardware encoder for `codec` on that adapter, if it has one.
pub fn encoder_name(luid: LUID, codec: Codec) -> Option<String> {
    let mf_name = find_encoders(luid, codec).ok()?.first().map(mf::friendly_name)?;
    if crate::qsv::is_available(luid) {
        return Some(format!("Intel QSV {} (oneVPL)", codec.name()));
    }
    Some(mf_name)
}

/// Settings that can be tried per machine.
#[derive(Clone, Copy, Debug)]
pub struct EncOptions {
    pub codec: Codec,
    /// Quality-versus-speed, 0 (fastest) to 100 (best quality); `None` keeps
    /// the encoder's default.
    pub speed: Option<u32>,
    /// Rate buffer, in frames' worth of bits; `None` picks it per encoder
    /// (`default_vbv_frames`).
    pub vbv_frames: Option<f64>,
    /// Set the temporal layer count to one explicitly (Chromium does this
    /// for Intel encoders).
    pub layers_hint: bool,
    /// Tell the encoder this is desktop streaming (Chromium does this for Intel).
    pub scenario_hint: bool,
    /// Choose the quantiser for every frame ourselves (see `qprate`) instead
    /// of letting the encoder hold a bitrate.
    pub qp_control: bool,
    /// HEVC only: full colour resolution (Main 4:4:4 profile, AYUV input)
    /// instead of 4:2:0 (NV12), so coloured text stays sharp.
    pub yuv444: bool,
}

impl Default for EncOptions {
    fn default() -> Self {
        EncOptions { codec: Codec::H264, speed: None, vbv_frames: None, layers_hint: true, scenario_hint: true, qp_control: false, yuv444: false }
    }
}

/// Intel Quick Sync encodes key frames at a quantiser set by the rate buffer
/// alone (QP 17 at one frame; the bitrate does not move it), so its key frames
/// are 4-6 times a frame's budget. Three quarters of a frame makes them a
/// quarter smaller for 0.3 dB on the frames between. Half a frame halves
/// them, but then the encoder dies after bitrate changes in sessions. NVIDIA
/// loses 2 dB on its key frames at three quarters, so it keeps one frame.
fn default_vbv_frames(encoder: &str) -> f64 {
    if encoder.contains("Intel") { 0.75 } else { 1.0 }
}

impl Encoder {
    pub fn new(
        device: &ID3D11Device,
        luid: LUID,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        opts: EncOptions,
    ) -> Result<Encoder> {
        Encoder::new_with(device, luid, width, height, fps, bitrate, opts, &[])
    }

    /// `extra`: codec API settings applied before the media types (enc-test).
    #[allow(clippy::too_many_arguments)]
    pub fn new_with(
        device: &ID3D11Device,
        luid: LUID,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        opts: EncOptions,
        extra: &[(&'static str, GUID, u64)],
    ) -> Result<Encoder> {
        if extra.is_empty() && !opts.yuv444 && !opts.qp_control && crate::qsv::is_available(luid) {
            match crate::qsv::QsvEncoder::try_new(device, luid, width, height, fps, bitrate, opts) {
                Ok(qsv) => {
                    let name = qsv.name.clone();
                    let format = qsv.format;
                    return Ok(Encoder {
                        backend: Backend::Qsv(Box::new(qsv)),
                        name,
                        format,
                        yuv444: false,
                        bitrate,
                        rekeys_on_rate_change: false,
                    });
                }
                Err(e) => {
                    crate::log!("oneVPL QSV {} unavailable ({e:#}); falling back to Media Foundation", opts.codec.name());
                }
            }
        }
        mf::init_thread()?;
        let list = find_encoders(luid, opts.codec)?;
        let Some(act) = list.first() else { bail!("no hardware {} encoder on this graphics adapter", opts.codec.name()) };
        let name = mf::friendly_name(act);
        let vbv_frames = opts.vbv_frames.unwrap_or_else(|| default_vbv_frames(&name));
        unsafe {
            let mft: IMFTransform = act.ActivateObject().context("ActivateObject")?;
            let attrs = mft.GetAttributes()?;
            if attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0 {
                attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            }
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            let events: IMFMediaEventGenerator = mft.cast().context("encoder is not asynchronous")?;
            let mgr = mf::attach_device(&mft, device)?;
            let codec: ICodecAPI = mft.cast().context("no ICodecAPI")?;
            // Not every encoder takes every setting; the essential ones are the
            // rate control mode, the bitrate and low latency.
            let mode = if opts.qp_control { eAVEncCommonRateControlMode_Quality } else { eAVEncCommonRateControlMode_CBR };
            mf::set_u32(&codec, &CODECAPI_AVEncCommonRateControlMode, mode.0 as u32).context("rate control mode")?;
            mf::set_u32(&codec, &CODECAPI_AVEncCommonMeanBitRate, bitrate).context("bitrate")?;
            mf::set_bool(&codec, &CODECAPI_AVLowLatencyMode, true).context("low latency mode")?;
            // A small rate buffer keeps every frame near its share of the
            // bitrate. With the default (often a second's worth) one frame
            // after a still period can be hundreds of kilobytes, which a slow
            // link needs a long time to carry.
            match mf::set_u32(&codec, &CODECAPI_AVEncCommonBufferSize, vbv_bits(bitrate, fps, vbv_frames)) {
                Ok(()) => crate::log!("encoder rate buffer: {vbv_frames} frame(s)"),
                Err(e) => crate::log!("encoder ignores the rate buffer size: {e}"),
            }
            let _ = mf::set_u32(&codec, &CODECAPI_AVEncMPVDefaultBPictureCount, 0);
            let _ = mf::set_u32(&codec, &CODECAPI_AVEncMPVGOPSize, fps * 60);
            let hint = |on: bool, key, v: u32| match on {
                false => "not set",
                true if mf::set_u32(&codec, key, v).is_ok() => "taken",
                true => "ignored",
            };
            let layers = hint(opts.layers_hint, &CODECAPI_AVEncVideoTemporalLayerCount, 1);
            let scenario =
                hint(opts.scenario_hint, &CODECAPI_AVScenarioInfo, eAVScenarioInfo_DisplayRemoting.0 as u32);
            crate::log!("encoder hints: one temporal layer {layers}, desktop streaming {scenario}");
            let speed = opts.speed.unwrap_or(0).min(100);
            match mf::set_u32(&codec, &CODECAPI_AVEncCommonQualityVsSpeed, speed) {
                Ok(()) => crate::log!("encoder quality-vs-speed set to {speed}"),
                Err(e) => crate::log!("encoder ignores quality-vs-speed: {e}"),
            }
            for (n, key, v) in extra {
                let r = if *v > u32::MAX as u64 || *key == CODECAPI_AVEncVideoEncodeFrameTypeQP {
                    mf::set_u64(&codec, key, *v)
                } else {
                    mf::set_u32(&codec, key, *v as u32)
                };
                crate::log!("encoder setting {n} = {v}: {}", r.map_or_else(|e| format!("refused ({e})"), |_| "taken".into()));
            }

            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &opts.codec.subtype())?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            out.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            let yuv444 = opts.yuv444 && opts.codec == Codec::Hevc;
            let profile = match opts.codec {
                Codec::H264 => eAVEncH264VProfile_High.0 as u32,
                Codec::Hevc if yuv444 => eAVEncH265VProfile_Main_444_8.0 as u32,
                Codec::Hevc => eAVEncH265VProfile_Main_420_8.0 as u32,
            };
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, profile)?;
            out.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
            out.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
            mft.SetOutputType(0, &out, 0).context("SetOutputType")?;

            let format = if yuv444 { MFVideoFormat_AYUV } else { MFVideoFormat_NV12 };
            let input = (0..)
                .map_while(|i| mft.GetInputAvailableType(0, i).ok())
                .find(|t| mf::subtype(t) == Some(format))
                .with_context(|| format!("encoder does not take {}", if yuv444 { "AYUV" } else { "NV12" }))?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            input.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            mft.SetInputType(0, &input, 0).context("SetInputType")?;

            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let seq_header = sequence_header(&mft);
            let mf_enc = MfEncoder {
                mft,
                events,
                codec,
                _mgr: mgr,
                credits: 0,
                ready: Default::default(),
                seq_header,
                format: opts.codec,
                fps,
                index: 0,
                key_next: true,
                rate_changed: false,
                inputs: Default::default(),
            };
            Ok(Encoder {
                backend: Backend::Mf(mf_enc),
                name,
                format: opts.codec,
                yuv444,
                bitrate,
                rekeys_on_rate_change: false,
            })
        }
    }

    /// Takes effect from the next frame (if the encoder honours it; see P1-2).
    pub fn set_bitrate(&mut self, bps: u32) {
        match &mut self.backend {
            Backend::Qsv(q) => {
                q.set_bitrate(bps);
                self.bitrate = q.bitrate;
            }
            Backend::Mf(m) => {
                if mf::set_u32(&m.codec, &CODECAPI_AVEncCommonMeanBitRate, bps).is_ok() {
                    self.bitrate = bps;
                    m.rate_changed = true;
                }
            }
        }
    }

    pub fn request_key(&mut self) {
        match &mut self.backend {
            Backend::Qsv(q) => q.request_key(),
            Backend::Mf(m) => m.key_next = true,
        }
    }

    /// For enc-test experiments with Media Foundation settings.
    pub fn codec_api(&self) -> Option<&ICodecAPI> {
        match &self.backend {
            Backend::Mf(m) => Some(&m.codec),
            Backend::Qsv(_) => None,
        }
    }

    /// Encodes one NV12 (AYUV if `yuv444`) frame and waits up to `timeout` for the result.
    /// `None` means the encoder has not produced it yet; it comes with a later call.
    /// `qp`: the quantiser for this frame, in QP-control mode.
    pub fn encode(&mut self, nv12: &ID3D11Texture2D, timeout: Duration, qp: Option<u32>) -> Result<Option<Encoded>> {
        match &mut self.backend {
            Backend::Qsv(q) => q.encode(nv12, timeout, qp),
            Backend::Mf(m) => m.encode(nv12, timeout, qp, &mut self.rekeys_on_rate_change),
        }
    }
}

impl MfEncoder {
    fn encode(
        &mut self,
        nv12: &ID3D11Texture2D,
        timeout: Duration,
        qp: Option<u32>,
        rekeys_on_rate_change: &mut bool,
    ) -> Result<Option<Encoded>> {
        let deadline = Instant::now() + timeout;
        while self.credits == 0 {
            if !self.pump(Some(deadline), rekeys_on_rate_change)? {
                return Ok(self.ready.pop_front());
            }
        }
        let asked = std::mem::take(&mut self.key_next);
        if asked {
            let _ = mf::set_u32(&self.codec, &CODECAPI_AVEncVideoForceKeyFrame, 1);
        }
        let dur = 10_000_000 / self.fps as i64;
        let sample = mf::sample_from_texture(nv12, self.index * dur, dur)?;
        if let Some(qp) = qp {
            let _ = mf::set_u64(&self.codec, &CODECAPI_AVEncVideoEncodeQP, qp as u64);
            unsafe { sample.SetUINT64(&MFSampleExtension_VideoEncodeQP, qp as u64)? };
        }
        self.index += 1;
        loop {
            match unsafe { self.mft.ProcessInput(0, &sample, 0) } {
                Ok(()) => break,
                // Some encoders (Intel after a bitrate change) take back input
                // requests they already made; wait for a fresh one.
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    self.credits = 0;
                    while self.credits == 0 {
                        if !self.pump(Some(deadline), rekeys_on_rate_change)? {
                            return Ok(self.ready.pop_front());
                        }
                    }
                }
                Err(e) => return Err(e).context("ProcessInput"),
            }
        }
        self.credits -= 1;
        self.inputs.push_back((asked, std::mem::take(&mut self.rate_changed)));
        while self.ready.is_empty() {
            if !self.pump(Some(deadline), rekeys_on_rate_change)? {
                break;
            }
        }
        Ok(self.ready.pop_front())
    }

    /// Handles one event. With a deadline, waits for it; returns false when the
    /// deadline passed without an event.
    fn pump(&mut self, deadline: Option<Instant>, rekeys_on_rate_change: &mut bool) -> Result<bool> {
        loop {
            let ev = match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(ev) => ev,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if deadline.is_none_or(|d| Instant::now() >= d) {
                        return Ok(false);
                    }
                    std::thread::sleep(Duration::from_micros(200));
                    continue;
                }
                Err(e) => return Err(e).context("GetEvent"),
            };
            let t = unsafe { ev.GetType()? };
            if t == METransformNeedInput.0 as u32 {
                self.credits += 1;
            } else if t == METransformHaveOutput.0 as u32 {
                self.pull(rekeys_on_rate_change)?;
            } else if t == MEError.0 as u32 {
                // Intel sends this, with a status that says success, after a
                // bitrate change it cannot take. The frame in flight never
                // comes out and the encoder asks for no more input (or the
                // next ProcessInput fails with E_FAIL): it is dead.
                let hr = unsafe { ev.GetStatus()? };
                bail!("encoder error event (status {hr:?})");
            }
            return Ok(true);
        }
    }

    fn pull(&mut self, rekeys_on_rate_change: &mut bool) -> Result<()> {
        unsafe {
            let mut buf = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, ..Default::default() }];
            let mut status = 0u32;
            let r = self.mft.ProcessOutput(0, &mut buf, &mut status);
            let sample = std::mem::ManuallyDrop::take(&mut buf[0].pSample);
            drop(std::mem::ManuallyDrop::take(&mut buf[0].pEvents));
            match r {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    let t = self.mft.GetOutputAvailableType(0, 0)?;
                    self.mft.SetOutputType(0, &t, 0)?;
                    self.seq_header = sequence_header(&self.mft);
                    return Ok(());
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) => return Err(e).context("ProcessOutput"),
            }
            let sample = sample.context("encoder gave no sample")?;
            let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let qp = sample.GetUINT64(&MFSampleExtension_VideoEncodeQP).ok().map(|q| (q & 0xffff) as u32);
            let (asked, after_change) = self.inputs.pop_front().unwrap_or_default();
            if key && !asked && after_change && !*rekeys_on_rate_change {
                *rekeys_on_rate_change = true;
                crate::log!("encoder starts a key frame on every bitrate change; changing the bitrate rarely from now on");
            }
            let mut data = mf::bytes_from_sample(&sample)?;
            // Intel's CBR pads every frame with zero bytes up to its share of
            // the bitrate (trailing_zero_8bits after the last NAL unit). They
            // carry nothing, and no NAL unit ends in a zero byte.
            let padding = data.iter().rev().take_while(|&&b| b == 0).count();
            data.truncate(data.len() - padding);
            let has_header = mf::nal_types(&data, self.format).iter().any(|&t| self.format.is_stream_header(t));
            if key && !has_header && !self.seq_header.is_empty() {
                let mut v = self.seq_header.clone();
                v.append(&mut data);
                data = v;
            }
            self.ready.push_back(Encoded { data, key, qp, padding });
            Ok(())
        }
    }
}

impl Drop for MfEncoder {
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

fn vbv_bits(bitrate: u32, fps: u32, frames: f64) -> u32 {
    (bitrate as f64 / fps.max(1) as f64 * frames).max(8_000.0) as u32
}

/// SPS/PPS the encoder publishes on its output type, if any.
fn sequence_header(mft: &IMFTransform) -> Vec<u8> {
    unsafe {
        let Ok(t) = mft.GetOutputCurrentType(0) else { return Vec::new() };
        let Ok(n) = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) else { return Vec::new() };
        let mut v = vec![0u8; n as usize];
        if t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut v, None).is_err() {
            return Vec::new();
        }
        v
    }
}
