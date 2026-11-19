//! Host capture loop, on its own thread: wait for a screen update, ask the
//! sender how big the frame may be, convert to NV12 (AYUV for 4:4:4, see
//! `yuv444`), encode, hand over.
//! At most `fps` frames per second; an update that arrives early waits for
//! its slot, and a newer update replaces it while it waits.
//!
//! The picture's size, frame rate and highest bitrate follow the viewer's
//! choice (`proto::Quality`); a new choice rebuilds the capture and encoder.
//!
//! After the screen changes, the same picture keeps being encoded at the
//! frame rate for a while (`REFINE`): on a slow link the frames of a moving
//! picture are coarse, and the frames after it fill in the detail, so text
//! that was scrolled turns sharp once it stops. They cost no more than
//! motion would: each gets the same budget, and once the picture is as good
//! as the bitrate makes it they are nearly empty and stop. Measured with
//! `enc-test --refine` on NVIDIA H.264 at 2560x1440, scrolling 40 px per
//! frame (2026-11-19): at 8 Mbps the last moving frame was 44.7 dB and 20
//! frames later 48.8; at 2 Mbps 25.4 dB, 30.7 after 30 frames, 42.6 after
//! 150. Raising the bitrate for these frames got there in fewer frames but
//! sent several times a frame's budget per frame, which a slow link cannot
//! carry without delaying the next moving frame.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use tokio::sync::mpsc::UnboundedSender;
use windows::Win32::{
    Foundation::RECT,
    Graphics::{
        Direct3D11::{D3D11_BIND_RENDER_TARGET, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Texture2D},
        Dxgi::Common::{DXGI_FORMAT_AYUV, DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC},
    },
    System::{
        Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
        Power::{ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState},
    },
};

use crate::{
    capture::{Capture, Grab},
    encode::{EncOptions, Encoder},
    gpu::Gpu,
    log,
    mf::{self, Codec},
    proto::{FrameHeader, Msg, Quality, Streaming},
    video_tx::{Plan, VideoTx},
    vp::{self, Vp},
};

#[derive(Clone, Copy)]
pub struct PipeConfig {
    /// Highest frame rate this host sends; the viewer chooses below it.
    pub max_fps: u32,
    /// Highest bitrate this host sends; `None`: what the viewer chose.
    pub max_bps: Option<u32>,
    /// 0 = encode at the display's size.
    pub max_width: u32,
    pub enc: EncOptions,
    /// Ask for a smaller single frame instead of skipping (see `video_tx::SHRINKING`).
    pub shrink: bool,
    /// Encode HEVC if this adapter has a hardware HEVC encoder (the viewer
    /// can decode it); H.264 otherwise or if it fails to start.
    pub hevc: bool,
    /// The viewer decodes and draws HEVC 4:4:4 (its 文字清晰 choice can be met).
    pub viewer444: bool,
}

#[derive(Default)]
pub struct PipeShared {
    pub want_key: AtomicBool,
    pub stop: AtomicBool,
    /// The captured display in desktop pixels, for mapping input.
    pub display: Mutex<RECT>,
    /// Which display to capture.
    pub monitors: crate::monitors::Selection,
    /// The desktop has no display (a laptop with its lid closed): nothing to capture.
    pub no_display: AtomicBool,
    /// What the viewer wants the picture to be, and whether that changed
    /// since the capture loop last started.
    pub quality: Mutex<Quality>,
    pub quality_changed: AtomicBool,
    /// The pointer is drawn into the captured image (see `Capture::pointer_apart`):
    /// the viewer shows no cursor of its own then, or there would be two.
    pub pointer_in_image: AtomicBool,
}

/// After a screen change, the same picture is encoded again for at most this
/// long (see the module notes)...
const REFINE: Duration = Duration::from_secs(4);
/// ...or until this many frames in a row came out nearly empty
/// (`REFINE_DONE`). The NVIDIA encoder above paused for up to 10 nearly
/// empty frames before it went on refining.
const REFINE_TINY_RUN: u32 = 20;

/// A frame this small adds nothing: a tenth of its budget, at least 1 KB.
fn refine_done(bytes: usize, budget: usize) -> bool {
    bytes < (budget / 10).max(1000)
}

fn qpc() -> i64 {
    let mut v = 0;
    unsafe {
        let _ = QueryPerformanceCounter(&mut v);
    }
    v
}

fn qpc_freq() -> i64 {
    let mut v = 0;
    unsafe {
        let _ = QueryPerformanceFrequency(&mut v);
    }
    v.max(1)
}

/// Even dimensions (NV12 needs them), scaled down to `max_width` if set.
pub fn encode_size(w: u32, h: u32, max_width: u32) -> (u32, u32) {
    picture_size(w, h, &Quality::default(), max_width)
}

/// The size to encode a `w` x `h` display at: the largest within every limit
/// `q` sets and `max_width`, never larger than the display, even dimensions.
pub fn picture_size(w: u32, h: u32, q: &Quality, max_width: u32) -> (u32, u32) {
    let (wf, hf) = (w.max(1) as f64, h.max(1) as f64);
    let mut scale: f64 = 1.0;
    if q.fit_w > 0 && q.fit_h > 0 {
        scale = scale.min(q.fit_w as f64 / wf).min(q.fit_h as f64 / hf);
    }
    if q.short > 0 {
        scale = scale.min(q.short as f64 / wf.min(hf));
    }
    if max_width > 0 {
        scale = scale.min(max_width as f64 / wf);
    }
    let size = |v: f64| ((v * scale).round() as u32).max(2) & !1;
    (size(wf), size(hf))
}

/// The highest bitrate for 自动, as Moonlight's default: 20 Mbps for 1080p at
/// 60 frames, in proportion to the pixels, with the frame rate's share
/// growing more slowly above 60.
pub fn auto_bps(w: u32, h: u32, fps: u32) -> u32 {
    let pixels = w as f64 * h as f64 / (1920.0 * 1080.0);
    let fps = fps.max(1) as f64;
    let per_fps = if fps <= 60.0 { fps / 30.0 } else { (fps / 60.0).sqrt() * 2.0 };
    (10e6 * pixels * per_fps).clamp(2e6, 150e6) as u32
}

/// The frame rate to stream: what the viewer asked for, at most what this
/// host allows and what the display shows.
fn stream_fps(q: &Quality, cfg: &PipeConfig, refresh_hz: u32) -> u32 {
    let wanted = if q.fps == 0 { 60 } else { q.fps as u32 };
    wanted.min(cfg.max_fps).min(refresh_hz).max(1)
}

/// The highest bitrate to stream at for a `w` x `h` picture at `fps`.
fn stream_bps(q: &Quality, cfg: &PipeConfig, w: u32, h: u32, fps: u32) -> u32 {
    let chosen = if q.kbps == 0 { auto_bps(w, h, fps) } else { q.kbps.saturating_mul(1000) };
    chosen.min(cfg.max_bps.unwrap_or(u32::MAX)).max(1_000_000)
}

pub fn nv12_texture(gpu: &Gpu, w: u32, h: u32) -> Result<ID3D11Texture2D> {
    yuv_texture(gpu, w, h, false)
}

/// The encoder's input picture: NV12, or AYUV for 4:4:4.
pub fn yuv_texture(gpu: &Gpu, w: u32, h: u32, yuv444: bool) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: if yuv444 { DXGI_FORMAT_AYUV } else { DXGI_FORMAT_NV12 },
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32 | if yuv444 { crate::yuv444::AYUV_BIND } else { 0 },
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut t = None;
    unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut t))? };
    t.context("CreateTexture2D for the encoder")
}

/// Runs until `sh.stop` is set. Rebuilds everything after a desktop switch,
/// mode change or error.
pub fn run(cfg: PipeConfig, tx: Arc<VideoTx>, sh: Arc<PipeShared>, info: UnboundedSender<Msg>) {
    if let Err(e) = mf::init_thread() {
        log!("media foundation: {e:#}");
        return;
    }
    // Keep the machine and its screen awake while someone is connected.
    unsafe {
        SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED);
    }
    let mut last_err = String::new();
    // What this machine's encoder does on a bitrate change; kept when the
    // encoder is rebuilt after a desktop switch.
    let mut quirks = RateQuirks::default();
    while !sh.stop.load(Ordering::Relaxed) {
        match run_once(cfg, &tx, &sh, &info, &mut quirks) {
            Ok(()) => last_err.clear(),
            Err(e) => {
                let s = format!("{e:#}");
                sh.no_display.store(s.contains(crate::monitors::NO_DISPLAY), Ordering::Relaxed);
                if s != last_err {
                    log!("capture: {s} (retrying)");
                    last_err = s;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
    unsafe {
        SetThreadExecutionState(ES_CONTINUOUS);
    }
}

#[derive(Default)]
struct RateQuirks {
    /// It starts a key frame on every change (`Encoder::rekeys_on_rate_change`).
    rekeys: bool,
    /// It dies on some changes (Intel, see `Encoder::set_bitrate`): change the
    /// bitrate by building a new encoder instead.
    dies: bool,
    /// The HEVC encoder failed: H.264 for the rest of the session.
    no_hevc: bool,
    /// 4:4:4 failed: 4:2:0 for the rest of the session.
    no_444: bool,
}

/// Whether this graphics card encodes HEVC 4:4:4 from AYUV and can make the
/// AYUV picture. Found out once per card, with a small encoder.
fn can_encode_444(gpu: &Gpu) -> bool {
    static SEEN: Mutex<Vec<((u32, i32), bool)>> = Mutex::new(Vec::new());
    let id = (gpu.luid.LowPart, gpu.luid.HighPart);
    if let Some(&(_, ok)) = SEEN.lock().unwrap().iter().find(|(l, _)| *l == id) {
        return ok;
    }
    let ok = crate::encode::encoder_name(gpu.luid, Codec::Hevc).is_some_and(|n| hevc_trusted(&n))
        && crate::yuv444::can_write(gpu)
        && {
            let opts = EncOptions { codec: Codec::Hevc, yuv444: true, ..Default::default() };
            match Encoder::new(&gpu.device, gpu.luid, 640, 360, 30, 2_000_000, opts) {
                Ok(_) => true,
                Err(e) => {
                    log!("no 4:4:4 on {}: {e:#}", gpu.name);
                    false
                }
            }
        };
    SEEN.lock().unwrap().push((id, ok));
    ok
}

/// The picture for the encoder, made from the captured image.
enum Convert {
    Vp(Vp),
    Ayuv(crate::yuv444::ToAyuv),
}

/// Whether to use this HEVC encoder when both ends can. Not Intel's (Arc,
/// driver 31.0.101.5382): in sessions it dies within half a second, bitrate
/// changes or not, and H.264 encoders made in the same process afterwards
/// keep dying too (P1-2).
fn hevc_trusted(encoder: &str) -> bool {
    !encoder.contains("Intel")
}

/// `bitrate` is the starting rate; the rate buffer is sized for it. With
/// `yuv444` only HEVC 4:4:4: the picture is AYUV, which H.264 does not take.
#[allow(clippy::too_many_arguments)]
fn new_encoder(
    gpu: &Gpu,
    w: u32,
    h: u32,
    fps: u32,
    cfg: &PipeConfig,
    quirks: &RateQuirks,
    bitrate: u32,
    yuv444: bool,
) -> Result<Encoder> {
    let make = |codec| {
        let opts = EncOptions { codec, yuv444, ..cfg.enc };
        Encoder::new(&gpu.device, gpu.luid, w, h, fps, bitrate, opts)
    };
    if yuv444 {
        let mut e = make(Codec::Hevc)?;
        e.rekeys_on_rate_change |= quirks.rekeys;
        return Ok(e);
    }
    let hevc = cfg.hevc && !quirks.no_hevc;
    if let Some(name) = crate::encode::encoder_name(gpu.luid, Codec::Hevc).filter(|n| hevc && hevc_trusted(n)) {
        match make(Codec::Hevc) {
            Ok(mut e) => {
                e.rekeys_on_rate_change |= quirks.rekeys;
                return Ok(e);
            }
            Err(e) => log!("{name}: {e:#}; using H.264"),
        }
    }
    let mut e = make(Codec::H264)?;
    e.rekeys_on_rate_change |= quirks.rekeys;
    Ok(e)
}

fn run_once(
    cfg: PipeConfig,
    tx: &VideoTx,
    sh: &PipeShared,
    info: &UnboundedSender<Msg>,
    quirks: &mut RateQuirks,
) -> Result<()> {
    // Service mode: capture the desktop that is shown now (see desktop.rs).
    let desktop = crate::desktop::follow()?;
    let (gpu, output, rect) = crate::monitors::open(&sh.monitors)?;
    sh.no_display.store(false, Ordering::Relaxed);
    *sh.display.lock().unwrap() = rect;
    let _ = info.send(crate::monitors::message(&sh.monitors));
    let mut cap = Capture::new(&gpu, &output)?;
    let (cw, ch) = cap.upright_size();
    let quality = *sh.quality.lock().unwrap();
    sh.quality_changed.store(false, Ordering::Relaxed);
    let (w, h) = picture_size(cw, ch, &quality, cfg.max_width);
    let refresh = crate::monitors::refresh_hz(&sh.monitors);
    let fps = stream_fps(&quality, &cfg, refresh);
    let max_bps = stream_bps(&quality, &cfg, w, h, fps);
    tx.configure(fps, max_bps as f64);
    // The shader that makes the 4:4:4 picture does not turn it.
    let can444 = cfg.viewer444 && !quirks.no_hevc && !quirks.no_444 && cap.rotation == 0 && can_encode_444(&gpu);
    let yuv444 = can444 && quality.yuv444;
    let pic = yuv_texture(&gpu, w, h, yuv444)?;
    let conv = if yuv444 {
        match crate::yuv444::ToAyuv::new(&gpu, &cap.image, &pic, (w, h)) {
            Ok(c) => Convert::Ayuv(c),
            Err(e) => {
                log!("4:4:4: {e:#}; 4:2:0 from now on");
                quirks.no_444 = true;
                return Ok(());
            }
        }
    } else {
        let vp = Vp::new(&gpu, (cap.width, cap.height), (w, h), vp::RGB_FULL, vp::YUV_709)?;
        if !vp.rotate(cap.rotation) {
            log!("this video processor cannot rotate: the {}° display is sent sideways", cap.rotation);
        }
        Convert::Vp(vp)
    };
    let mut enc = match new_encoder(&gpu, w, h, fps, &cfg, quirks, max_bps, yuv444) {
        Err(e) if yuv444 => {
            log!("4:4:4 encoder: {e:#}; 4:2:0 from now on");
            quirks.no_444 = true;
            return Ok(());
        }
        r => r?,
    };
    log!(
        "capturing {}x{} at {refresh} Hz on {}, encoding {w}x{h} {} at up to {fps} fps and {:.1} Mbps with {} ({quality:?})",
        cap.width,
        cap.height,
        gpu.name,
        if yuv444 { "4:4:4" } else { "4:2:0" },
        max_bps as f64 / 1e6,
        enc.name
    );
    let streaming = Some(Streaming { fps: fps.min(255) as u8, max_kbps: max_bps / 1000, can444, yuv444 });
    let info_msg = |enc: &Encoder| Msg::Info { width: w as u16, height: h as u16, encoder: enc.name.clone(), streaming };
    let _ = info.send(info_msg(&enc));
    if let Some(name) = desktop {
        log!("capturing desktop {name}");
        let _ = info.send(Msg::Desktop { name });
    }
    sh.want_key.store(true, Ordering::Relaxed);

    let freq = qpc_freq();
    let min_gap = Duration::from_secs_f64(0.95 / fps as f64);
    let full = RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
    let mut pending: Option<i64> = None;
    let mut have_image = false;
    // Encoding the still picture again until then (see the module notes),
    // and how many nearly empty frames came out of that in a row.
    let mut refine_until: Option<Instant> = None;
    let mut tiny_run = 0u32;
    let mut refining = false;
    // This round of refining so far: frames, bytes, since when (for the log).
    let mut refined = (0u32, 0usize, Instant::now());
    let mut last_start: Option<Instant> = None;
    let mut last_change = Instant::now();
    let mut tried = quirks.rekeys || quirks.dies;
    // When the encoder's bitrate was last changed in place.
    let mut changed_at: Option<Instant> = None;
    // Since when the encoder has been handing back nothing.
    let mut no_output_since: Option<Instant> = None;
    let mut frames = 0u64;
    let mut desktop_checked = Instant::now();
    while !sh.stop.load(Ordering::Relaxed) {
        if sh.quality_changed.load(Ordering::Relaxed) {
            log!("the viewer chose another picture");
            return Ok(());
        }
        // Service mode: do not count on the duplication to report a desktop
        // switch (the Winlogon desktop is often still); look every 200 ms.
        if desktop_checked.elapsed() >= Duration::from_millis(200) {
            desktop_checked = Instant::now();
            if crate::desktop::switched() || crate::monitors::should_reopen(&sh.monitors) {
                return Ok(());
            }
        }
        if refine_until.is_some_and(|t| Instant::now() >= t) {
            log_refined(&refined, "time up");
            refine_until = None;
        }
        let refine_due = refine_until.is_some();
        let next_slot = || last_start.map_or(Duration::ZERO, |t| (t + min_gap).saturating_duration_since(Instant::now()));
        let wait = match pending {
            Some(_) => next_slot(),
            None if refine_due => next_slot().max(Duration::from_millis(1)),
            None => Duration::from_millis(50),
        };
        let grab = match cap.grab(&gpu, wait.as_micros().div_ceil(1000) as u32) {
            Ok(g) => g,
            // Leaving Winlogon ends its duplication with DXGI_ERROR_INVALID_CALL
            // (test laptop 2026-11-08): rebuild now rather than after the error pause.
            Err(_) if crate::desktop::switched() => return Ok(()),
            Err(e) => return Err(e),
        };
        sh.pointer_in_image.store(!cap.pointer_apart, Ordering::Relaxed);
        match grab {
            Grab::New { present_qpc } => {
                pending = Some(present_qpc);
                have_image = true;
                refining = false;
            }
            Grab::Timeout => {}
            Grab::Lost => return Ok(()),
        }
        if pending.is_none() && have_image && sh.want_key.load(Ordering::Relaxed) {
            // A key frame was asked for while the screen is still: send the last image again.
            pending = Some(qpc());
        }
        if pending.is_none() && have_image && refine_due {
            pending = Some(qpc());
            refining = true;
        }
        let Some(shown_at) = pending else { continue };
        if last_start.is_some_and(|t| t.elapsed() < min_gap) {
            continue;
        }
        last_start = Some(Instant::now());
        let expensive = enc.rekeys_on_rate_change || quirks.dies;
        let shrink = cfg.shrink && !expensive;
        let Plan::Encode { chosen, bitrate, normal, shrunk } = tx.plan(enc.bitrate, shrink) else { continue };
        if enc.rekeys_on_rate_change && !quirks.rekeys {
            quirks.rekeys = true;
            // Possibly found on a frame shrunk for that frame only: back to
            // the chosen rate now rather than after the 3 s wait.
            if should_change(enc.bitrate, chosen, Duration::MAX, false) {
                enc.set_bitrate(chosen);
            }
            last_change = Instant::now();
        } else if !tried && frames > 0 {
            // A 2% change on the second frame shows whether this encoder
            // starts a key frame on every change. Better to find out now,
            // while the first picture is still arriving, than mid-session.
            enc.set_bitrate((enc.bitrate as f64 * 0.98) as u32);
            changed_at = Some(Instant::now());
            tried = true;
        } else if should_change(enc.bitrate, if shrunk { bitrate } else { chosen }, last_change.elapsed(), expensive) {
            let rate = if shrunk { bitrate } else { chosen };
            if quirks.dies {
                // A new encoder starts with a key frame, like a change on an
                // encoder that rekeys anyway.
                enc = new_encoder(&gpu, w, h, fps, &cfg, quirks, rate, yuv444)?;
                no_output_since = None;
            } else {
                enc.set_bitrate(rate);
                changed_at = Some(Instant::now());
            }
            last_change = Instant::now();
        }
        if sh.want_key.swap(false, Ordering::Relaxed) {
            enc.request_key();
        }
        let t_enc = Instant::now();
        let enc_start_qpc = qpc();
        match &conv {
            Convert::Vp(v) => v.blt(&cap.image, 0, &pic, full)?,
            Convert::Ayuv(a) => a.run(&gpu),
        }
        pending = None;
        let r = match enc.encode(&pic, Duration::from_millis(100), None) {
            Ok(None) if no_output_since.get_or_insert_with(Instant::now).elapsed() > Duration::from_secs(1) => {
                Err(anyhow!("the encoder gave no frame for 1 s"))
            }
            r => r,
        };
        let e = match r {
            Ok(Some(e)) => e,
            Ok(None) => continue,
            Err(err) if enc.yuv444 => {
                log!("4:4:4 encoder failed ({err:#}); 4:2:0 from now on");
                quirks.no_444 = true;
                return Ok(());
            }
            Err(err) if enc.format == Codec::Hevc => {
                log!("HEVC encoder failed ({err:#}); using H.264 from now on");
                quirks.no_hevc = true;
                changed_at = None;
                no_output_since = None;
                enc = new_encoder(&gpu, w, h, fps, &cfg, quirks, enc.bitrate, false)?;
                let _ = info.send(info_msg(&enc));
                pending = Some(shown_at);
                continue;
            }
            Err(err) if changed_at.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) => {
                log!("encoder died after a bitrate change ({err:#}); changing the bitrate by rebuilding it from now on");
                quirks.dies = true;
                changed_at = None;
                no_output_since = None;
                tried = true;
                enc = new_encoder(&gpu, w, h, fps, &cfg, quirks, enc.bitrate, yuv444)?;
                last_change = Instant::now();
                pending = Some(shown_at);
                continue;
            }
            Err(err) => return Err(err),
        };
        no_output_since = None;
        let encoded_qpc = qpc();
        frames += 1;
        if !tried {
            // Encode the same image again for that check, even on a still screen.
            pending = Some(shown_at);
        }
        let encode_us = t_enc.elapsed().as_micros() as u32;
        let host_us = ((qpc() - shown_at).max(0) as i128 * 1_000_000 / freq as i128) as u32;
        let budget = (enc.bitrate as usize / 8 / fps as usize).max(1);
        if refining {
            tiny_run = if refine_done(e.data.len(), budget) { tiny_run + 1 } else { 0 };
            refined.0 += 1;
            refined.1 += e.data.len();
            if tiny_run >= REFINE_TINY_RUN {
                log_refined(&refined, "nothing left to add");
                refine_until = None;
            }
        } else if !refine_done(e.data.len(), budget) {
            // Something changed that is worth filling in (a blinking caret is
            // not, and does not stop the filling in of what changed before).
            if refine_until.is_none() {
                refined = (0, 0, Instant::now());
            }
            refine_until = Some(Instant::now() + REFINE);
            tiny_run = 0;
        }
        let mut payload = Vec::with_capacity(e.data.len() + crate::proto::FRAME_HDR);
        let hevc = enc.format == Codec::Hevc;
        FrameHeader { key: e.key, hevc, yuv444: enc.yuv444, width: w as u16, height: h as u16, host_us }.write(&mut payload);
        payload.extend_from_slice(&e.data);
        let f = tx.submit(&payload, normal, budget, shrunk, host_us, encode_us);
        if crate::trace::on() {
            // The encoder hands frames out in order, one in flight, so the
            // start time belongs to this frame unless it came late (rare).
            crate::trace::mark(f, "update", Some(shown_at));
            crate::trace::mark(f, "enc_start", Some(enc_start_qpc));
            crate::trace::mark(f, "encoded", Some(encoded_qpc));
            crate::trace::mark(f, "queued", None);
            crate::trace::value(f, "bytes", payload.len() as i64);
        }
    }
    Ok(())
}

fn log_refined(r: &(u32, usize, Instant), why: &str) {
    if r.0 > 0 {
        log!("refined the still picture with {} frames, {} KB in {:.1} s ({why})", r.0, r.1 / 1000, r.2.elapsed().as_secs_f64());
    }
}

/// Whether to hand the encoder a new bitrate now. Most encoders follow every
/// change, even for a single frame. One that starts a key frame on each change
/// (`Encoder::rekeys_on_rate_change`) only gets large, lasting changes: down
/// by a fifth at most once a second, up by a quarter at most every 3 s.
fn should_change(current: u32, wanted: u32, since_last: Duration, expensive: bool) -> bool {
    let ratio = wanted as f64 / current.max(1) as f64;
    if !expensive {
        return (ratio - 1.0).abs() > 0.03;
    }
    (ratio < 0.8 && since_last >= Duration::from_secs(1)) || (ratio > 1.25 && since_last >= Duration::from_secs(3))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    #[test]
    fn encode_size_is_even_and_scaled() {
        assert_eq!(super::encode_size(2880, 1920, 0), (2880, 1920));
        assert_eq!(super::encode_size(1365, 767, 0), (1364, 766));
        assert_eq!(super::encode_size(3840, 2160, 1920), (1920, 1080));
    }

    #[test]
    fn picture_size_follows_the_viewers_choice() {
        use crate::proto::Quality;
        let q = |fit_w, fit_h, short| Quality { fit_w, fit_h, short, fps: 60, kbps: 0, yuv444: false };
        let size = super::picture_size;
        assert_eq!(size(3840, 2160, &q(2560, 1440, 0), 0), (2560, 1440), "自动 fits the viewer's screen");
        assert_eq!(size(1920, 1080, &q(2560, 1440, 0), 0), (1920, 1080), "never larger than the display");
        assert_eq!(size(2560, 1600, &q(1920, 1080, 0), 0), (1728, 1080), "fits both sides");
        assert_eq!(size(3840, 2160, &q(0, 0, 1080), 0), (1920, 1080), "1080p limits the shorter side");
        assert_eq!(size(2160, 3840, &q(0, 0, 720), 0), (720, 1280), "a portrait display too");
        assert_eq!(size(3840, 2160, &q(0, 0, 0), 0), (3840, 2160), "原生");
        assert_eq!(size(3840, 2160, &q(0, 0, 1440), 1920), (1920, 1080), "the host's limit still holds");
    }

    #[test]
    fn auto_bitrate_as_moonlight() {
        let mbps = |w, h, fps| super::auto_bps(w, h, fps) as f64 / 1e6;
        assert!((mbps(1920, 1080, 60) - 20.0).abs() < 0.01);
        assert!((mbps(1920, 1080, 30) - 10.0).abs() < 0.01);
        assert!((mbps(2560, 1440, 60) - 35.6).abs() < 0.1);
        assert!((mbps(1920, 1080, 120) - 28.3).abs() < 0.1);
        assert_eq!(mbps(640, 360, 30), 2.0, "at least 2 Mbps");
        assert_eq!(mbps(7680, 4320, 144), 150.0, "at most 150 Mbps");
    }

    #[test]
    fn refining_stops_on_nearly_empty_frames() {
        // 20 Mbps at 60 fps: a 41 KB budget.
        assert!(super::refine_done(3_000, 41_666));
        assert!(!super::refine_done(5_000, 41_666));
        // A tiny budget still counts frames under 1 KB as empty.
        assert!(super::refine_done(900, 4_000));
    }

    #[test]
    fn expensive_rate_changes_are_rare_and_large() {
        let s = |n| Duration::from_secs(n);
        let change = super::should_change;
        assert!(change(20_000_000, 19_000_000, s(0), false));
        assert!(!change(20_000_000, 19_800_000, s(9), false));
        assert!(!change(20_000_000, 17_000_000, s(9), true), "a 15% cut waits for a bigger one");
        assert!(!change(20_000_000, 15_000_000, Duration::from_millis(500), true));
        assert!(change(20_000_000, 15_000_000, s(1), true));
        assert!(!change(10_000_000, 13_000_000, s(2), true));
        assert!(change(10_000_000, 13_000_000, s(3), true));
    }
}
