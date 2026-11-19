//! `remote-control enc-test`: checks, on this machine's encoder, how closely
//! encoded frames follow the bitrate the sender asks for: after a lasting
//! change, and for a single frame. No network. The content is the screen
//! captured once and then scrolled a little every frame (like scrolling a
//! page), so every machine gets the same kind of motion; `--live` uses the
//! live screen instead. Must run in the desktop session, like the host.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use clap::Args;
use windows::{
    Win32::{
        Foundation::RECT,
        Graphics::Direct3D11::{
            D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_USAGE_STAGING,
            ID3D11Texture2D,
        },
        Media::MediaFoundation::*,
    },
    core::GUID,
};

use crate::{
    capture::{Capture, Grab},
    decode::Decoder,
    encode::{EncOptions, Encoder},
    gpu::Gpu,
    log,
    mf::{self, Codec},
    pipeline::{encode_size, nv12_texture},
    qprate::QpRate,
    stats::percentile,
    vp::{self, Vp},
};

#[derive(Args)]
pub struct EncTestArgs {
    #[arg(long, default_value_t = 20.0)]
    mbps: f64,
    #[arg(long, default_value_t = 60)]
    fps: u32,
    /// Length of each bitrate step.
    #[arg(long, default_value_t = 2.0)]
    step_seconds: f64,
    /// Pixels the picture scrolls per frame.
    #[arg(long, default_value_t = 6)]
    scroll: u32,
    /// Encode the live screen instead of the scrolled snapshot.
    #[arg(long)]
    live: bool,
    #[arg(long, value_parser = clap::value_parser!(u32).range(0..=100))]
    speed: Option<u32>,
    /// Rate buffer in frames; default as in the session (per encoder).
    #[arg(long)]
    vbv: Option<f64>,
    /// Leave out the "one temporal layer" hint to the encoder.
    #[arg(long)]
    no_layers_hint: bool,
    /// Leave out the "desktop streaming" hint to the encoder.
    #[arg(long)]
    no_scenario_hint: bool,
    /// Pick the quantiser for every frame ourselves instead of setting bitrates.
    #[arg(long)]
    qp: bool,
    /// List the codec API settings this encoder supports, then exit.
    #[arg(long)]
    probe: bool,
    /// Report which codecs this machine can encode and decode, then exit.
    #[arg(long)]
    codecs: bool,
    /// Encode HEVC instead of H.264.
    #[arg(long)]
    hevc: bool,
    /// Key frame test: hold `--mbps`, force a key frame every `--key-every`
    /// frames and report key frame size and picture quality (Y PSNR).
    #[arg(long)]
    keys: bool,
    #[arg(long, default_value_t = 60)]
    key_every: u32,
    #[arg(long, default_value_t = 6.0)]
    seconds: f64,
    /// Codec API setting `name=value` applied just before each forced key
    /// frame and put back right after it.
    #[arg(long = "key-set", value_parser = parse_setting)]
    key_settings: Vec<(&'static str, GUID, u64)>,
    /// With --keys: one line per frame.
    #[arg(long)]
    dump: bool,
    /// Use this saved screen (from --save-image on the same machine) instead
    /// of capturing one, so runs can be compared.
    #[arg(long)]
    image: Option<std::path::PathBuf>,
    #[arg(long)]
    save_image: Option<std::path::PathBuf>,
    /// With --qp: instead of following bitrates, ask for these quantisers in
    /// turn, each for `--qp-hold` frames (1 = a different one every frame),
    /// and report size and quality per quantiser asked for.
    #[arg(long, value_delimiter = ',')]
    qp_sweep: Vec<u32>,
    #[arg(long, default_value_t = 20)]
    qp_hold: u32,
    /// Bitrate change test: for each interval (in frames), change the bitrate
    /// by `--churn-step` every that many frames for `--seconds`, and count
    /// encoder errors (the encoder is rebuilt after one) and unasked key frames.
    #[arg(long, value_delimiter = ',')]
    churn: Vec<u32>,
    #[arg(long, default_value_t = 0.8)]
    churn_step: f64,
    /// With --churn: what to change: `mean` (the bitrate only, as the session
    /// does), `both` (the bitrate, then the rate buffer, as the session did
    /// before P1-2) or `buffer` (the rate buffer only, sized as for the
    /// alternating bitrate).
    #[arg(long, default_value = "mean", value_parser = ["mean", "both", "buffer"])]
    churn_what: String,
    /// Codec API setting before the media types, `name=value` (names as in --probe).
    #[arg(long = "set", value_parser = parse_setting)]
    settings: Vec<(&'static str, GUID, u64)>,
    /// Still-screen test: scroll at `--mbps` for `--seconds`, then encode the
    /// last picture again `--refine-frames` times, and report size and
    /// picture quality (Y PSNR) per frame.
    #[arg(long)]
    refine: bool,
    #[arg(long, default_value_t = 30)]
    refine_frames: u32,
    /// With --refine: the bitrate while the picture is still (default: `--mbps`).
    #[arg(long)]
    refine_mbps: Option<f64>,
    /// Colour test: encode the still screen with HEVC at `--mbps` for
    /// `--seconds`, once in 4:2:0 and once in 4:4:4, decode both, and save
    /// full-size crops (`--crop x,y,w,h`) of the source and both results as
    /// BMPs into `--out`.
    #[arg(long)]
    yuv444: bool,
    #[arg(long, value_delimiter = ',', default_value = "0,0,800,400")]
    crop: Vec<u32>,
    #[arg(long, default_value = ".")]
    out: std::path::PathBuf,
}

/// Codec API properties enc-test can show and set.
const PROPS: &[(&str, GUID)] = &[
    ("rc-mode", CODECAPI_AVEncCommonRateControlMode),
    ("mean-bitrate", CODECAPI_AVEncCommonMeanBitRate),
    ("max-bitrate", CODECAPI_AVEncCommonMaxBitRate),
    ("buffer-size", CODECAPI_AVEncCommonBufferSize),
    ("buffer-in-level", CODECAPI_AVEncCommonBufferInLevel),
    ("quality", CODECAPI_AVEncCommonQuality),
    ("quality-vs-speed", CODECAPI_AVEncCommonQualityVsSpeed),
    ("low-latency", CODECAPI_AVLowLatencyMode),
    ("min-qp", CODECAPI_AVEncVideoMinQP),
    ("max-qp", CODECAPI_AVEncVideoMaxQP),
    ("encode-qp", CODECAPI_AVEncVideoEncodeQP),
    ("frame-type-qp", CODECAPI_AVEncVideoEncodeFrameTypeQP),
    ("intra-refresh", CODECAPI_AVEncVideoGradualIntraRefresh),
    ("gop-size", CODECAPI_AVEncMPVGOPSize),
    ("b-frames", CODECAPI_AVEncMPVDefaultBPictureCount),
    ("max-ref-frames", CODECAPI_AVEncVideoMaxNumRefFrame),
    ("ltr-buffer", CODECAPI_AVEncVideoLTRBufferControl),
    ("mark-ltr", CODECAPI_AVEncVideoMarkLTRFrame),
    ("use-ltr", CODECAPI_AVEncVideoUseLTRFrame),
    ("temporal-layers", CODECAPI_AVEncVideoTemporalLayerCount),
    ("max-temporal-layers", CODECAPI_AVEncVideoMaxTemporalLayers),
    ("scenario", CODECAPI_AVScenarioInfo),
    ("content-type", CODECAPI_AVEncVideoContentType),
    ("slice-mode", CODECAPI_AVEncSliceControlMode),
    ("slice-size", CODECAPI_AVEncSliceControlSize),
    ("roi", CODECAPI_AVEncVideoROIEnabled),
    ("dirty-rect", CODECAPI_AVEncVideoDirtyRectEnabled),
    ("supported-controls", CODECAPI_AVEncVideoSupportedControls),
    ("rc-params", CODECAPI_AVEncVideoRateControlParams),
    ("cbr-motion-tradeoff", CODECAPI_AVEncVideoCBRMotionTradeoff),
    ("adaptive-mode", CODECAPI_AVEncAdaptiveMode),
    ("allow-frame-drops", CODECAPI_AVEncCommonAllowFrameDrops),
    ("low-power", CODECAPI_AVEncLowPowerEncoder),
    ("deblock", CODECAPI_AVEnableInLoopDeblockFilter),
    ("cabac", CODECAPI_AVEncH264CABACEnable),
    ("force-key", CODECAPI_AVEncVideoForceKeyFrame),
];

fn parse_setting(s: &str) -> Result<(&'static str, GUID, u64), String> {
    let (n, v) = s.split_once('=').ok_or("expected name=value")?;
    let &(name, key) = PROPS.iter().find(|p| p.0 == n).ok_or_else(|| format!("unknown setting {n}"))?;
    let v = match v.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16),
        None => v.parse(),
    }
    .map_err(|e| e.to_string())?;
    Ok((name, key, v))
}

fn probe(enc: &Encoder) {
    let c = enc.codec_api();
    for (name, key) in PROPS {
        let (sup, modi) = mf::support(c, key);
        let val = mf::get_u64(c, key).map_or("-".into(), |v| format!("{v} (0x{v:x})"));
        let range = mf::range(c, key).map_or(String::new(), |(a, b, s)| format!(", range {a}..={b} step {s}"));
        log!("{name:>20}: supported {sup:<5} modifiable {modi:<5} value {val}{range}");
    }
}

/// The Y plane of `tex` (array slice `slice`), `w` x `h`, read back through
/// `staging`, which is made to match on first use.
fn luma(gpu: &Gpu, tex: &ID3D11Texture2D, slice: u32, w: u32, h: u32, staging: &mut Option<ID3D11Texture2D>) -> Result<Vec<u8>> {
    unsafe {
        if staging.is_none() {
            let mut d = Default::default();
            tex.GetDesc(&mut d);
            d.ArraySize = 1;
            d.MipLevels = 1;
            d.Usage = D3D11_USAGE_STAGING;
            d.BindFlags = 0;
            d.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            d.MiscFlags = 0;
            let mut t = None;
            gpu.device.CreateTexture2D(&d, None, Some(&mut t))?;
            *staging = t;
        }
        let st = staging.as_ref().context("staging texture")?;
        gpu.ctx.CopySubresourceRegion(st, 0, 0, 0, 0, tex, slice, None);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        gpu.ctx.Map(st, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
        let mut out = Vec::with_capacity((w * h) as usize);
        for y in 0..h as usize {
            let row = std::slice::from_raw_parts((m.pData as *const u8).add(y * m.RowPitch as usize), w as usize);
            out.extend_from_slice(row);
        }
        gpu.ctx.Unmap(st, 0);
        Ok(out)
    }
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let se: u64 = a.iter().zip(b).map(|(&x, &y)| ((x as i32 - y as i32).pow(2)) as u64).sum();
    let mse = se as f64 / a.len().max(1) as f64;
    if mse == 0.0 { 99.0 } else { 10.0 * (255.0 * 255.0 / mse).log10() }
}

/// Copies `src` into `dst` moved up by `off` rows, wrapping around.
fn scrolled(gpu: &Gpu, src: &ID3D11Texture2D, dst: &ID3D11Texture2D, w: u32, h: u32, off: u32) {
    let b = |top, bottom| D3D11_BOX { left: 0, top, front: 0, right: w, bottom, back: 1 };
    unsafe {
        gpu.ctx.CopySubresourceRegion(dst, 0, 0, 0, 0, src, 0, Some(&b(off, h)));
        if off > 0 {
            gpu.ctx.CopySubresourceRegion(dst, 0, 0, h - off, 0, src, 0, Some(&b(0, off)));
        }
    }
}

/// The four letters of a video subtype GUID.
fn fourcc(g: &GUID) -> String {
    let b = g.data1.to_le_bytes();
    if b.iter().all(|c| c.is_ascii_graphic() || *c == b' ') {
        String::from_utf8_lossy(&b).into_owned()
    } else {
        format!("{:#x}", g.data1)
    }
}

/// What this machine can do for YUV 4:4:4 (sharp coloured text): hardware
/// encoders registered for 4:4:4 input, whether the H.264 and HEVC encoders
/// take a 4:4:4 profile and what input they then offer, and the decoder's
/// 4:4:4 profiles.
fn probe_444(gpu: &Gpu) {
    use windows::Win32::Graphics::{Direct3D11::ID3D11VideoDevice, Dxgi::Common::DXGI_FORMAT_AYUV};
    use windows::core::Interface;
    for c in [Codec::H264, Codec::Hevc] {
        for (n, sub) in [("AYUV", MFVideoFormat_AYUV), ("ARGB32", MFVideoFormat_ARGB32), ("RGB32", MFVideoFormat_RGB32)] {
            let flags = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER;
            let found = mf::find_mfts(MFT_CATEGORY_VIDEO_ENCODER, flags, sub, c.subtype(), Some(gpu.luid));
            let names: Vec<String> = found.map(|l| l.iter().map(mf::friendly_name).collect()).unwrap_or_default();
            log!("4:4:4 {:>5}: encoders registered for {n} input: {}", c.name(), if names.is_empty() { "none".into() } else { names.join(", ") });
        }
        let Ok(list) = mf::find_mfts(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            MFVideoFormat_NV12,
            c.subtype(),
            Some(gpu.luid),
        ) else {
            continue;
        };
        let Some(act) = list.first() else { continue };
        let name = mf::friendly_name(act);
        let profiles: &[(&str, u32)] = match c {
            Codec::H264 => &[("High", eAVEncH264VProfile_High.0 as u32), ("High 4:4:4", eAVEncH264VProfile_444.0 as u32)],
            Codec::Hevc => &[("Main", eAVEncH265VProfile_Main_420_8.0 as u32), ("Main 4:4:4", eAVEncH265VProfile_Main_444_8.0 as u32)],
        };
        for (pn, profile) in profiles {
            let r: Result<Vec<String>> = (|| unsafe {
                let mft: IMFTransform = act.ActivateObject()?;
                let attrs = mft.GetAttributes()?;
                if attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0 {
                    attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
                }
                let _mgr = mf::attach_device(&mft, &gpu.device)?;
                let out = MFCreateMediaType()?;
                out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                out.SetGUID(&MF_MT_SUBTYPE, &c.subtype())?;
                out.SetUINT32(&MF_MT_AVG_BITRATE, 20_000_000)?;
                out.SetUINT64(&MF_MT_FRAME_SIZE, mf::pack(1920, 1080))?;
                out.SetUINT64(&MF_MT_FRAME_RATE, mf::pack(60, 1))?;
                out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, mf::pack(1, 1))?;
                out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
                out.SetUINT32(&MF_MT_MPEG2_PROFILE, *profile)?;
                mft.SetOutputType(0, &out, 0).context("SetOutputType")?;
                let inputs = (0..).map_while(|i| mft.GetInputAvailableType(0, i).ok()).filter_map(|t| mf::subtype(&t)).map(|g| fourcc(&g)).collect();
                let _ = act.ShutdownObject();
                Ok(inputs)
            })();
            match r {
                Ok(inputs) => log!("4:4:4 {name}: profile {pn} taken; inputs then {}", inputs.join(", ")),
                Err(e) => log!("4:4:4 {name}: profile {pn} refused ({e:#})"),
            }
        }
    }
    // DXVA_ModeHEVC_VLD_Main_444 and DXVA_ModeH264_VLD_... have no 4:4:4 for H.264.
    const HEVC_MAIN_444: GUID = GUID::from_u128(0x4008018f_f537_4b36_98cf_61af8a2c1a33);
    const HEVC_MAIN10_444: GUID = GUID::from_u128(0x0dabeffa_4458_4602_bc03_0795659d617c);
    let Ok(vd) = gpu.device.cast::<ID3D11VideoDevice>() else {
        log!("4:4:4 decode: no D3D11 video device");
        return;
    };
    unsafe {
        let n = vd.GetVideoDecoderProfileCount();
        let mut seen = Vec::new();
        for i in 0..n {
            if let Ok(g) = vd.GetVideoDecoderProfile(i) {
                seen.push(g);
            }
        }
        for (pn, g) in [("HEVC Main 4:4:4", HEVC_MAIN_444), ("HEVC Main10 4:4:4", HEVC_MAIN10_444)] {
            let has = seen.contains(&g);
            let ayuv = has && vd.CheckVideoDecoderFormat(&g, DXGI_FORMAT_AYUV).is_ok_and(|b| b.as_bool());
            log!("4:4:4 decode on {}: {pn} {}; AYUV output {}", gpu.name, if has { "offered" } else { "not offered" }, ayuv);
        }
        log!("4:4:4 decode: {} decoder profiles in all", seen.len());
    }
}

pub fn run(a: EncTestArgs) -> Result<()> {
    mf::init_thread()?;
    let (gpu, output, _) = Gpu::for_primary_output()?;
    if a.codecs {
        for c in [Codec::H264, Codec::Hevc] {
            let enc = crate::encode::encoder_name(gpu.luid, c).unwrap_or_else(|| "none".into());
            let dec = Decoder::available(&gpu.device, c).map_or_else(|e| format!("no ({e:#})"), |_| "yes".into());
            log!("{:>5} on {}: hardware encoder {enc}; decoder {dec}", c.name(), gpu.name);
        }
        probe_444(&gpu);
        return Ok(());
    }
    let mut cap = Capture::new(&gpu, &output)?;
    let (cw, ch) = (cap.width, cap.height);
    let (w, h) = encode_size(cw, ch, 0);
    let nv12 = nv12_texture(&gpu, w, h)?;
    let conv = Vp::new(&gpu, (cw, ch), (w, h), vp::RGB_FULL, vp::YUV_709)?;
    let t_first = Instant::now();
    while !matches!(cap.grab(&gpu, 100)?, Grab::New { .. }) {
        if t_first.elapsed() > Duration::from_secs(3) {
            bail!("no screen image within 3 s");
        }
    }
    if let Some(p) = &a.image {
        let b = std::fs::read(p).with_context(|| format!("read {}", p.display()))?;
        let dims = (u32::from_le_bytes(b[0..4].try_into()?), u32::from_le_bytes(b[4..8].try_into()?));
        if dims != (cw, ch) || b.len() != 8 + (cw * ch * 4) as usize {
            bail!("{} is {}x{}, the screen {cw}x{ch}", p.display(), dims.0, dims.1);
        }
        unsafe { gpu.ctx.UpdateSubresource(&cap.image, 0, None, b[8..].as_ptr() as _, cw * 4, 0) };
    }
    if let Some(p) = &a.save_image {
        let mut staging = None;
        let mut v = Vec::with_capacity(8 + (cw * ch * 4) as usize);
        v.extend_from_slice(&cw.to_le_bytes());
        v.extend_from_slice(&ch.to_le_bytes());
        // Four bytes per pixel: read it as a "luma" plane four times as wide.
        v.extend(luma(&gpu, &cap.image, 0, cw * 4, ch, &mut staging)?);
        std::fs::write(p, v)?;
        log!("screen saved to {}", p.display());
    }
    let moving: ID3D11Texture2D = unsafe {
        let mut desc = Default::default();
        cap.image.GetDesc(&mut desc);
        let mut t = None;
        gpu.device.CreateTexture2D(&desc, None, Some(&mut t))?;
        t.context("CreateTexture2D")?
    };
    let max = (a.mbps * 1e6) as u32;
    let opts = EncOptions {
        codec: if a.hevc { Codec::Hevc } else { Codec::H264 },
        speed: a.speed,
        vbv_frames: a.vbv,
        layers_hint: !a.no_layers_hint,
        scenario_hint: !a.no_scenario_hint,
        qp_control: a.qp,
        yuv444: false,
    };
    let mut enc = Encoder::new_with(&gpu.device, gpu.luid, w, h, a.fps, max, opts, &a.settings)?;
    log!(
        "{w}x{h} on {} with {}, content: {} ({opts:?})",
        gpu.name,
        enc.name,
        if a.live { "live screen".to_string() } else { format!("screen scrolled {} px per frame", a.scroll) }
    );
    if a.probe {
        probe(&enc);
        return Ok(());
    }
    let full = RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
    if !a.churn.is_empty() {
        let src = Source { live: a.live, scroll: a.scroll, cw, ch, moving: &moving };
        drop(enc);
        let mk = || Encoder::new_with(&gpu.device, gpu.luid, w, h, a.fps, max, opts, &a.settings);
        return run_churn(&a, &gpu, &mut cap, &src, &conv, &nv12, &mk, full);
    }
    if !a.qp_sweep.is_empty() {
        if !a.qp {
            bail!("--qp-sweep needs --qp");
        }
        let src = Source { live: a.live, scroll: a.scroll, cw, ch, moving: &moving };
        return run_qp_sweep(&a, &gpu, &mut cap, &src, &conv, &nv12, &mut enc, (w, h), full);
    }
    if a.keys {
        let src = Source { live: a.live, scroll: a.scroll, cw, ch, moving: &moving };
        return run_keys(&a, &gpu, &mut cap, &src, &conv, &nv12, &mut enc, (w, h), full);
    }
    if a.yuv444 {
        drop(enc);
        return run_yuv444(&a, &gpu, &cap, (w, h));
    }
    if a.refine {
        let src = Source { live: false, scroll: a.scroll, cw, ch, moving: &moving };
        return run_refine(&a, &gpu, &mut cap, &src, &conv, &nv12, &mut enc, (w, h), full);
    }
    // (name, share of the top bitrate, every 10th frame at a quarter of it)
    let steps = [
        ("full", 1.0, false),
        ("quarter", 0.25, false),
        ("full", 1.0, false),
        ("eighth", 0.125, false),
        ("full", 1.0, false),
        ("dips", 1.0, true),
    ];
    let mut warm = true;
    let mut n = 0u32;
    let mut qr = QpRate::default();
    for (name, share, dips) in steps {
        let rate = (max as f64 * share) as u32;
        if !a.qp {
            enc.set_bitrate(rate);
        }
        let t0 = Instant::now();
        let (mut normal, mut dipped, mut after, mut enc_ms, mut qps) = (vec![], vec![], vec![], vec![], vec![]);
        let (mut i, mut keys) = (0u32, 0u32);
        while t0.elapsed().as_secs_f64() < a.step_seconds {
            let src = if a.live {
                match cap.grab(&gpu, 50)? {
                    Grab::New { .. } => {}
                    Grab::Timeout => continue,
                    Grab::Lost => bail!("capture lost (desktop switch?)"),
                }
                &cap.image
            } else {
                n += 1;
                scrolled(&gpu, &cap.image, &moving, cw, ch, (n * a.scroll) % ch);
                &moving
            };
            let dip = dips && i % 10 == 5;
            let r = if dip { rate / 4 } else { rate };
            if r != enc.bitrate && !a.qp {
                enc.set_bitrate(r);
            }
            let budget = r as f64 / 8.0 / a.fps as f64;
            let qp = a.qp.then(|| qr.next_qp(budget));
            let t = Instant::now();
            conv.blt(src, 0, &nv12, full)?;
            let Some(e) = enc.encode(&nv12, Duration::from_millis(200), qp)? else { continue };
            enc_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            if let Some(q) = qp {
                qr.update(q, e.data.len(), e.key);
                qps.push(q as f64);
            }
            let pct = e.data.len() as f64 / budget * 100.0;
            if e.key || warm {
                keys += e.key as u32;
                warm = false;
            } else if dip {
                dipped.push(pct);
            } else if dips && i % 10 == 6 {
                after.push(pct);
            } else {
                normal.push(pct);
            }
            i += 1;
        }
        log!(
            "{name:>7} {:>5.1} Mbps: {i:>3} frames ({keys} key), size of budget p50 {:>4.0}% p90 {:>4.0}% max {:>5.0}%, encode p50 {:.1} ms{}",
            rate as f64 / 1e6,
            percentile(&normal, 50.0),
            percentile(&normal, 90.0),
            normal.iter().copied().fold(0.0, f64::max),
            percentile(&enc_ms, 50.0),
            if a.qp { format!(", QP p50 {:.0}", percentile(&qps, 50.0)) } else { String::new() },
        );
        if dips {
            log!(
                "        single frames at a quarter: p50 {:.0}% of their own (small) budget; the frame after: p50 {:.0}%",
                percentile(&dipped, 50.0),
                percentile(&after, 50.0),
            );
        }
    }
    Ok(())
}

/// Where the pictures come from: the snapshot scrolled a little per frame, or
/// the live screen.
struct Source<'a> {
    live: bool,
    scroll: u32,
    cw: u32,
    ch: u32,
    moving: &'a ID3D11Texture2D,
}

impl Source<'_> {
    /// The next picture; `None` while the live screen has not changed.
    fn next(&self, gpu: &Gpu, cap: &mut Capture, n: &mut u32) -> Result<Option<ID3D11Texture2D>> {
        if self.live {
            return match cap.grab(gpu, 50)? {
                Grab::New { .. } => Ok(Some(cap.image.clone())),
                Grab::Timeout => Ok(None),
                Grab::Lost => bail!("capture lost (desktop switch?)"),
            };
        }
        *n += 1;
        scrolled(gpu, &cap.image, self.moving, self.cw, self.ch, (*n * self.scroll) % self.ch);
        Ok(Some(self.moving.clone()))
    }
}

struct Row {
    idx: u32,
    key: bool,
    bytes: usize,
    filler: usize,
    qp: Option<u32>,
    psnr: Option<f64>,
}

#[allow(clippy::too_many_arguments)]
fn run_keys(
    a: &EncTestArgs,
    gpu: &Gpu,
    cap: &mut Capture,
    src: &Source,
    conv: &Vp,
    nv12: &ID3D11Texture2D,
    enc: &mut Encoder,
    (w, h): (u32, u32),
    full: RECT,
) -> Result<()> {
    // 0 = never force one; frames are still grouped in 60s for the report.
    let every = if a.key_every == 0 { 60 } else { a.key_every.max(8) };
    let budget = a.mbps * 1e6 / 8.0 / a.fps as f64;
    let mut dec = Decoder::new(&gpu.device, w, h, enc.format)?;
    let restore: Vec<(GUID, Option<u64>)> =
        a.key_settings.iter().map(|(_, k, _)| (*k, mf::get_u64(enc.codec_api(), k))).collect();
    let mut set_failed = 0;
    let (mut st_src, mut st_dec) = (None, None);
    let mut pending: VecDeque<(u32, Option<Vec<u8>>)> = VecDeque::new();
    let mut rows = Vec::new();
    let mut dec_ms = Vec::new();
    let (mut i, mut n) = (0u32, 0u32);
    let t0 = Instant::now();
    while t0.elapsed().as_secs_f64() < a.seconds {
        let Some(tex) = src.next(gpu, cap, &mut n)? else { continue };
        let phase = i % every;
        let forced = a.key_every > 0 && i > 0 && phase == 0;
        if forced {
            enc.request_key();
            for (_, k, v) in &a.key_settings {
                set_failed += mf::set_u32(enc.codec_api(), k, *v as u32).is_err() as u32;
            }
        }
        conv.blt(&tex, 0, nv12, full)?;
        let measure = phase <= 3 || phase == every - 1 || i % 10 == 5;
        let y = if measure { Some(luma(gpu, nv12, 0, w, h, &mut st_src)?) } else { None };
        pending.push_back((i, y));
        i += 1;
        let out = enc.encode(nv12, Duration::from_millis(200), None)?;
        if forced {
            for (k, v) in &restore {
                if let Some(v) = v {
                    set_failed += mf::set_u32(enc.codec_api(), k, *v as u32).is_err() as u32;
                }
            }
        }
        let Some(e) = out else { continue };
        let (idx, y) = pending.pop_front().unwrap_or((u32::MAX, None));
        let t = Instant::now();
        let pic = dec.decode(&e.data)?;
        dec_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        let psnr = match (pic, y) {
            (Some(p), Some(y)) => Some(psnr(&y, &luma(gpu, &p.tex, p.slice, w, h, &mut st_dec)?)),
            _ => None,
        };
        rows.push(Row { idx, key: e.key, bytes: e.data.len(), filler: mf::filler_bytes(&e.data) + e.padding, qp: e.qp, psnr });
    }
    let secs = t0.elapsed().as_secs_f64();
    let total: usize = rows.iter().map(|r| r.bytes).sum();
    let filler: usize = rows.iter().map(|r| r.filler).sum();
    log!(
        "{} frames in {secs:.1} s, {:.1} Mbps sent (asked {:.1}), padding removed {:.1} Mbps, budget {:.1} KB per frame, key frame every {every}",
        rows.len(),
        total as f64 * 8.0 / secs / 1e6,
        a.mbps,
        filler as f64 * 8.0 / secs / 1e6,
        budget / 1000.0
    );
    log!("decode p50 {:.1} ms p95 {:.1} ms", percentile(&dec_ms, 50.0), percentile(&dec_ms, 95.0));
    let fmt = |v: Option<f64>, d: usize| v.map_or("-".to_string(), |x| format!("{x:.d$}"));
    let mut unasked = 0;
    if a.dump {
        for r in &rows {
            log!(
                "frame {:>4}{} {:>6.1} KB (padding {:>5.1}) QP {} PSNR {}",
                r.idx,
                if r.key { " key" } else { "    " },
                r.bytes as f64 / 1000.0,
                r.filler as f64 / 1000.0,
                r.qp.map_or("-".into(), |q| q.to_string()),
                fmt(r.psnr, 2)
            );
        }
    }
    for r in rows.iter().filter(|r| r.key) {
        let asked = r.idx == 0 || (a.key_every > 0 && r.idx % every == 0);
        if !asked {
            unasked += 1;
        }
        log!(
            "key frame {:>4}: {:>6.1} KB = {:>4.0}% of budget, QP {}, PSNR {} dB{}",
            r.idx,
            r.bytes as f64 / 1000.0,
            r.bytes as f64 / budget * 100.0,
            r.qp.map_or("-".into(), |q| q.to_string()),
            fmt(r.psnr, 2),
            if asked { "" } else { " (not asked for)" }
        );
    }
    let group = |name: &str, sel: &dyn Fn(u32) -> bool| {
        let g: Vec<&Row> = rows.iter().filter(|r| !r.key && r.idx != u32::MAX && sel(r.idx % every)).collect();
        let size: Vec<f64> = g.iter().map(|r| r.bytes as f64 / budget * 100.0).collect();
        let qp: Vec<f64> = g.iter().filter_map(|r| r.qp.map(|q| q as f64)).collect();
        let ps: Vec<f64> = g.iter().filter_map(|r| r.psnr).collect();
        let fill: Vec<f64> = g.iter().map(|r| r.filler as f64 / budget * 100.0).collect();
        log!(
            "{name:>14}: {:>4} frames, size of budget p50 {:>4.0}% p90 {:>4.0}% max {:>4.0}% (padding removed p50 {:.0}%), QP p50 {}, PSNR p50 {} dB (min {})",
            g.len(),
            percentile(&size, 50.0),
            percentile(&size, 90.0),
            size.iter().copied().fold(0.0, f64::max),
            percentile(&fill, 50.0),
            fmt((!qp.is_empty()).then(|| percentile(&qp, 50.0)), 0),
            fmt((!ps.is_empty()).then(|| percentile(&ps, 50.0)), 2),
            fmt(ps.iter().copied().reduce(f64::min), 2),
        );
    };
    group("1st after key", &|p| p == 1);
    group("2nd-3rd after", &|p| p == 2 || p == 3);
    group("before key", &|p| p == every - 1);
    group("steady", &|p| p >= 8 && p < every - 1);
    if unasked > 0 {
        log!("{unasked} key frame(s) nobody asked for");
    }
    if set_failed > 0 {
        log!("{set_failed} --key-set change(s) refused");
    }
    Ok(())
}

/// See `EncTestArgs::yuv444`.
fn run_yuv444(a: &EncTestArgs, gpu: &Gpu, cap: &Capture, (w, h): (u32, u32)) -> Result<()> {
    let [x, y, cw, ch] = a.crop[..] else { bail!("--crop x,y,w,h") };
    std::fs::create_dir_all(&a.out)?;
    // The source: BGRA, read as a plane four bytes per pixel.
    let mut st = None;
    let bgra = luma(gpu, &cap.image, 0, cap.width * 4, cap.height, &mut st)?;
    let (cw, ch) = (cw.min(w - x.min(w)), ch.min(h - y.min(h)));
    let row = (cw as usize * 3).div_ceil(4) * 4;
    let mut px = vec![0u8; row * ch as usize];
    for r in 0..ch as usize {
        let line = &bgra[(y as usize + r) * cap.width as usize * 4..];
        for c in 0..cw as usize {
            let p = &line[(x as usize + c) * 4..];
            px[(ch as usize - 1 - r) * row + 3 * c..][..3].copy_from_slice(&p[..3]);
        }
    }
    write_bmp(&a.out.join("source.bmp"), cw, ch, &px)?;
    let frames = (a.seconds * a.fps as f64).max(1.0) as u32;
    for yuv444 in [false, true] {
        let name = if yuv444 { "444" } else { "420" };
        let opts = EncOptions { codec: Codec::Hevc, yuv444, ..Default::default() };
        let mut enc = Encoder::new_with(&gpu.device, gpu.luid, w, h, a.fps, (a.mbps * 1e6) as u32, opts, &a.settings)?;
        let tex = crate::pipeline::yuv_texture(gpu, w, h, yuv444)?;
        if yuv444 {
            crate::yuv444::ToAyuv::new(gpu, &cap.image, &tex, (w, h))?.run(gpu);
        } else {
            let conv = Vp::new(gpu, (cap.width, cap.height), (w, h), vp::RGB_FULL, vp::YUV_709)?;
            conv.blt(&cap.image, 0, &tex, RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 })
                .with_context(|| format!("{name}: video processor into the encoder's picture"))?;
        }
        let mut dec = Decoder::new(&gpu.device, w, h, Codec::Hevc)?;
        let (mut bytes, mut last) = (Vec::new(), None);
        let t0 = Instant::now();
        let mut enc_ms = Vec::new();
        for _ in 0..frames {
            let t = Instant::now();
            let e = loop {
                if let Some(e) = enc.encode(&tex, Duration::from_millis(200), None)? {
                    break e;
                }
            };
            enc_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            bytes.push(e.data.len() as f64);
            if let Some(p) = dec.decode(&e.data).with_context(|| format!("{name}: decode"))? {
                last = Some(p);
            }
        }
        let p = last.context("nothing decoded")?;
        let mut d = Default::default();
        unsafe { p.tex.GetDesc(&mut d) };
        log!(
            "{name} with {}: {frames} frames in {:.1} s, first {:.0} KB, then p50 {:.1} KB; encode p50 {:.1} ms; decoded as {:?}",
            enc.name,
            t0.elapsed().as_secs_f64(),
            bytes[0] / 1000.0,
            percentile(&bytes[1..], 50.0) / 1000.0,
            percentile(&enc_ms, 50.0),
            d.Format
        );
        crate::snapshot::save_region(gpu, &p, (x, y, cw, ch), 1, &a.out.join(format!("{name}.bmp")))?;
        // What the viewer shows: its video processor turns the decoded picture into BGRA.
        let shown = bgra_texture(gpu, w, h)?;
        Vp::new(gpu, (w, h), (w, h), vp::YUV_709, vp::RGB_FULL)?
            .blt(&p.tex, p.slice, &shown, RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 })
            .with_context(|| format!("{name}: the viewer's video processor"))?;
        let mut st = None;
        let px = luma(gpu, &shown, 0, w * 4, h, &mut st)?;
        let mut out = vec![0u8; row * ch as usize];
        for r in 0..ch as usize {
            let line = &px[(y as usize + r) * w as usize * 4..];
            for c in 0..cw as usize {
                out[(ch as usize - 1 - r) * row + 3 * c..][..3].copy_from_slice(&line[(x as usize + c) * 4..][..3]);
            }
        }
        write_bmp(&a.out.join(format!("{name}-shown.bmp")), cw, ch, &out)?;
    }
    log!("crops in {}", a.out.display());
    Ok(())
}

fn bgra_texture(gpu: &Gpu, w: u32, h: u32) -> Result<ID3D11Texture2D> {
    let desc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: windows::Win32::Graphics::Direct3D11::D3D11_USAGE_DEFAULT,
        BindFlags: windows::Win32::Graphics::Direct3D11::D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut t = None;
    unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut t))? };
    t.context("BGRA texture")
}

fn write_bmp(path: &std::path::Path, w: u32, h: u32, px: &[u8]) -> Result<()> {
    let mut f = Vec::with_capacity(54 + px.len());
    f.extend_from_slice(b"BM");
    f.extend_from_slice(&((54 + px.len()) as u32).to_le_bytes());
    f.extend_from_slice(&[0, 0, 0, 0]);
    f.extend_from_slice(&54u32.to_le_bytes());
    f.extend_from_slice(&40u32.to_le_bytes());
    f.extend_from_slice(&(w as i32).to_le_bytes());
    f.extend_from_slice(&(h as i32).to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&24u16.to_le_bytes());
    f.extend_from_slice(&[0u8; 24]);
    f.extend_from_slice(px);
    std::fs::write(path, f).with_context(|| format!("write {}", path.display()))
}

/// See `EncTestArgs::refine`.
#[allow(clippy::too_many_arguments)]
fn run_refine(
    a: &EncTestArgs,
    gpu: &Gpu,
    cap: &mut Capture,
    src: &Source,
    conv: &Vp,
    nv12: &ID3D11Texture2D,
    enc: &mut Encoder,
    (w, h): (u32, u32),
    full: RECT,
) -> Result<()> {
    let budget = a.mbps * 1e6 / 8.0 / a.fps as f64;
    let mut dec = Decoder::new(&gpu.device, w, h, enc.format)?;
    let (mut st_src, mut st_dec) = (None, None);
    let mut n = 0u32;
    let mut still: Option<Vec<u8>> = None;
    let mut moving_bytes = Vec::new();
    let mut last_moving_psnr = None;
    let frames = (a.seconds * a.fps as f64) as u32;
    // Moving, then still: every frame is decoded (each depends on the one before).
    for i in 0..frames + a.refine_frames {
        let refining = i >= frames;
        if i == frames {
            if let Some(m) = a.refine_mbps {
                enc.set_bitrate((m * 1e6) as u32);
            }
            still = Some(luma(gpu, nv12, 0, w, h, &mut st_src)?);
        }
        if !refining {
            let tex = src.next(gpu, cap, &mut n)?.context("no picture")?;
            conv.blt(&tex, 0, nv12, full)?;
        }
        let measure_moving = i + 1 == frames;
        let y = if measure_moving { Some(luma(gpu, nv12, 0, w, h, &mut st_src)?) } else { None };
        let e = loop {
            if let Some(e) = enc.encode(nv12, Duration::from_millis(200), None)? {
                break e;
            }
        };
        let pic = dec.decode(&e.data)?;
        let reference = if refining { still.as_ref() } else { y.as_ref() };
        let q = match (pic, reference) {
            (Some(p), Some(r)) => Some(psnr(r, &luma(gpu, &p.tex, p.slice, w, h, &mut st_dec)?)),
            _ => None,
        };
        if !refining {
            if !e.key {
                moving_bytes.push(e.data.len() as f64);
            }
            if measure_moving {
                last_moving_psnr = q;
                log!(
                    "moving: {frames} frames at {:.1} Mbps, size p50 {:.1} KB ({:.0}% of budget), last frame PSNR {} dB",
                    a.mbps,
                    percentile(&moving_bytes, 50.0) / 1000.0,
                    percentile(&moving_bytes, 50.0) / budget * 100.0,
                    q.map_or("-".into(), |v| format!("{v:.2}"))
                );
            }
            continue;
        }
        log!(
            "still {:>3}{}: {:>7.1} KB ({:>4.0}% of budget) QP {} PSNR {} dB{}",
            i - frames + 1,
            if e.key { " key" } else { "    " },
            e.data.len() as f64 / 1000.0,
            e.data.len() as f64 / budget * 100.0,
            e.qp.map_or("-".into(), |v| v.to_string()),
            q.map_or("-".into(), |v| format!("{v:.2}")),
            match (q, last_moving_psnr) {
                (Some(v), Some(m)) => format!(" ({:+.2})", v - m),
                _ => String::new(),
            }
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_qp_sweep(
    a: &EncTestArgs,
    gpu: &Gpu,
    cap: &mut Capture,
    src: &Source,
    conv: &Vp,
    nv12: &ID3D11Texture2D,
    enc: &mut Encoder,
    (w, h): (u32, u32),
    full: RECT,
) -> Result<()> {
    let hold = a.qp_hold.max(1);
    let mut dec = Decoder::new(&gpu.device, w, h, enc.format)?;
    let (mut st_src, mut st_dec) = (None, None);
    let mut pending: VecDeque<(u32, u32, Option<Vec<u8>>)> = VecDeque::new();
    // Per QP asked for: (bytes, reported QP, PSNR) of frames not right after a change.
    let mut by_qp: std::collections::BTreeMap<u32, Vec<(usize, Option<u32>, Option<f64>)>> = Default::default();
    let mut first_after: std::collections::BTreeMap<u32, Vec<usize>> = Default::default();
    let (mut i, mut n) = (0u32, 0u32);
    let t0 = Instant::now();
    while t0.elapsed().as_secs_f64() < a.seconds {
        let Some(tex) = src.next(gpu, cap, &mut n)? else { continue };
        let qp = a.qp_sweep[((i / hold) as usize) % a.qp_sweep.len()];
        conv.blt(&tex, 0, nv12, full)?;
        let y = if i % 4 == 0 { Some(luma(gpu, nv12, 0, w, h, &mut st_src)?) } else { None };
        pending.push_back((i, qp, y));
        i += 1;
        let Some(e) = enc.encode(nv12, Duration::from_millis(200), Some(qp))? else { continue };
        let (idx, asked, y) = pending.pop_front().unwrap_or((u32::MAX, 0, None));
        let pic = dec.decode(&e.data)?;
        if e.key || idx == u32::MAX {
            continue;
        }
        let psnr = match (pic, y) {
            (Some(p), Some(y)) => Some(psnr(&y, &luma(gpu, &p.tex, p.slice, w, h, &mut st_dec)?)),
            _ => None,
        };
        let bytes = e.data.len() - mf::filler_bytes(&e.data);
        if hold > 1 && idx % hold == 0 {
            first_after.entry(asked).or_default().push(bytes);
        } else {
            by_qp.entry(asked).or_default().push((bytes, e.qp, psnr));
        }
    }
    log!("QP sweep {:?}, each held {hold} frame(s), {i} frames in {:.1} s", a.qp_sweep, t0.elapsed().as_secs_f64());
    let fmt = |v: f64, d: usize| if v.is_nan() { "-".to_string() } else { format!("{v:.d$}") };
    for (q, v) in &by_qp {
        let kb: Vec<f64> = v.iter().map(|r| r.0 as f64 / 1000.0).collect();
        let rep: Vec<f64> = v.iter().filter_map(|r| r.1.map(|x| x as f64)).collect();
        let ps: Vec<f64> = v.iter().filter_map(|r| r.2).collect();
        let first: Vec<f64> = first_after.get(q).map_or(vec![], |f| f.iter().map(|&b| b as f64 / 1000.0).collect());
        log!(
            "asked QP {q:>2}: {:>4} frames, size p50 {:>7.1} KB (p10 {:.1}, p90 {:.1}), reported QP p50 {}, PSNR p50 {} dB; first frame after the change p50 {} KB",
            v.len(),
            percentile(&kb, 50.0),
            percentile(&kb, 10.0),
            percentile(&kb, 90.0),
            fmt(percentile(&rep, 50.0), 0),
            fmt(percentile(&ps, 50.0), 2),
            fmt(percentile(&first, 50.0), 1),
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_churn(
    a: &EncTestArgs,
    gpu: &Gpu,
    cap: &mut Capture,
    src: &Source,
    conv: &Vp,
    nv12: &ID3D11Texture2D,
    mk: &dyn Fn() -> Result<Encoder>,
    full: RECT,
) -> Result<()> {
    let max = (a.mbps * 1e6) as u32;
    let mut n = 0u32;
    for &every in &a.churn {
        let every = every.max(1);
        let mut enc = mk()?;
        let (mut frames, mut changes, mut keys, mut errors, mut stalls) = (0u32, 0u32, 0u32, 0u32, 0u32);
        let mut since_change = 0u32;
        let mut last_change = Instant::now();
        let mut first_key_pending = true;
        let mut err_lines: Vec<String> = Vec::new();
        // Encoded size (padding included) over the bitrate in force, per
        // frame from the 2nd after a change on: does the encoder follow it?
        let (mut follow_hi, mut follow_lo) = (Vec::new(), Vec::new());
        let t0 = Instant::now();
        let mut i = 0u32;
        while t0.elapsed().as_secs_f64() < a.seconds {
            let Some(tex) = src.next(gpu, cap, &mut n)? else { continue };
            if i > 0 && i % every == 0 {
                let r = if enc.bitrate == max { (max as f64 * a.churn_step) as u32 } else { max };
                let c = enc.codec_api().clone();
                let mean = || mf::set_u32(&c, &CODECAPI_AVEncCommonMeanBitRate, r).is_ok();
                let buffer = || {
                    let bits = (r as f64 / a.fps as f64 * a.vbv.unwrap_or(1.0)) as u32;
                    mf::set_u32(&c, &CODECAPI_AVEncCommonBufferSize, bits).is_ok()
                };
                // The bitrate kept in `enc` only serves to alternate here.
                let ok = match a.churn_what.as_str() {
                    "both" => mean() && buffer(),
                    "buffer" => buffer(),
                    _ => mean(),
                };
                if ok {
                    enc.bitrate = r;
                }
                changes += 1;
                since_change = 0;
                last_change = Instant::now();
            }
            i += 1;
            conv.blt(&tex, 0, nv12, full)?;
            match enc.encode(nv12, Duration::from_millis(200), None) {
                Ok(Some(e)) => {
                    frames += 1;
                    if since_change >= 1 && !e.key {
                        let budget = enc.bitrate as f64 / 8.0 / a.fps as f64;
                        let r = (e.data.len() + e.padding) as f64 / budget * 100.0;
                        if enc.bitrate == max { follow_hi.push(r) } else { follow_lo.push(r) }
                    }
                    if e.key && !std::mem::take(&mut first_key_pending) {
                        keys += 1;
                    }
                    first_key_pending = false;
                }
                Ok(None) => stalls += 1,
                Err(e) => {
                    errors += 1;
                    if err_lines.len() < 4 {
                        err_lines.push(format!(
                            "{e:#} at frame {i}, {since_change} frame(s) / {:.1} ms after a change",
                            last_change.elapsed().as_secs_f64() * 1000.0
                        ));
                    }
                    drop(enc);
                    enc = mk()?;
                    first_key_pending = true;
                }
            }
            since_change += 1;
        }
        log!(
            "change {} every {every:>3} frame(s): {frames} frames, {changes} changes, {keys} unasked key frames, {errors} errors, {stalls} frames with no output in 200 ms, size of the rate in force p50 {:.0}% at the high rate, {:.0}% at the low",
            a.churn_what,
            percentile(&follow_hi, 50.0),
            percentile(&follow_lo, 50.0),
        );
        for l in err_lines {
            log!("    {l}");
        }
    }
    Ok(())
}
