//! Host capture loop, on its own thread: wait for a screen update, ask the
//! sender how big the frame may be, convert to NV12, encode, hand over.
//! At most `fps` frames per second; an update that arrives early waits for
//! its slot, and a newer update replaces it while it waits.

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
        Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC},
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
    proto::{FrameHeader, Msg},
    video_tx::{Plan, VideoTx},
    vp::{self, Vp},
};

#[derive(Clone, Copy)]
pub struct PipeConfig {
    pub fps: u32,
    pub max_bps: u32,
    /// 0 = encode at the display's size.
    pub max_width: u32,
    pub enc: EncOptions,
    /// Ask for a smaller single frame instead of skipping (see `video_tx::SHRINKING`).
    pub shrink: bool,
    /// Encode HEVC if this adapter has a hardware HEVC encoder (the viewer
    /// can decode it); H.264 otherwise or if it fails to start.
    pub hevc: bool,
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
    let (w2, h2) = if max_width > 0 && w > max_width {
        (max_width, (h as u64 * max_width as u64 / w as u64) as u32)
    } else {
        (w, h)
    };
    (w2 & !1, h2 & !1)
}

pub fn nv12_texture(gpu: &Gpu, w: u32, h: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut t = None;
    unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut t))? };
    t.context("CreateTexture2D NV12")
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
}

/// Whether to use this HEVC encoder when both ends can. Not Intel's (Arc,
/// driver 31.0.101.5382): in sessions it dies within half a second, bitrate
/// changes or not, and H.264 encoders made in the same process afterwards
/// keep dying too (P1-2).
fn hevc_trusted(encoder: &str) -> bool {
    !encoder.contains("Intel")
}

/// `bitrate` is the starting rate; the rate buffer is sized for it.
fn new_encoder(gpu: &Gpu, w: u32, h: u32, cfg: &PipeConfig, quirks: &RateQuirks, bitrate: u32) -> Result<Encoder> {
    let make = |codec| {
        let opts = EncOptions { codec, ..cfg.enc };
        Encoder::new(&gpu.device, gpu.luid, w, h, cfg.fps, bitrate, opts)
    };
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
    let (w, h) = encode_size(cw, ch, cfg.max_width);
    let nv12 = nv12_texture(&gpu, w, h)?;
    let conv = Vp::new(&gpu, (cap.width, cap.height), (w, h), vp::RGB_FULL, vp::YUV_709)?;
    if !conv.rotate(cap.rotation) {
        log!("this video processor cannot rotate: the {}° display is sent sideways", cap.rotation);
    }
    let mut enc = new_encoder(&gpu, w, h, &cfg, quirks, cfg.max_bps)?;
    log!("capturing {}x{} on {}, encoding {w}x{h} with {}", cap.width, cap.height, gpu.name, enc.name);
    let _ = info.send(Msg::Info { width: w as u16, height: h as u16, encoder: enc.name.clone() });
    if let Some(name) = desktop {
        log!("capturing desktop {name}");
        let _ = info.send(Msg::Desktop { name });
    }
    sh.want_key.store(true, Ordering::Relaxed);

    let freq = qpc_freq();
    let min_gap = Duration::from_secs_f64(0.95 / cfg.fps as f64);
    let full = RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
    let mut pending: Option<i64> = None;
    let mut have_image = false;
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
        // Service mode: do not count on the duplication to report a desktop
        // switch (the Winlogon desktop is often still); look every 200 ms.
        if desktop_checked.elapsed() >= Duration::from_millis(200) {
            desktop_checked = Instant::now();
            if crate::desktop::switched() || crate::monitors::should_reopen(&sh.monitors) {
                return Ok(());
            }
        }
        let wait = match (pending, last_start) {
            (Some(_), Some(t)) => (t + min_gap).saturating_duration_since(Instant::now()),
            (Some(_), None) => Duration::ZERO,
            (None, _) => Duration::from_millis(50),
        };
        let grab = match cap.grab(&gpu, wait.as_micros().div_ceil(1000) as u32) {
            Ok(g) => g,
            // Leaving Winlogon ends its duplication with DXGI_ERROR_INVALID_CALL
            // (test laptop 2026-11-08): rebuild now rather than after the error pause.
            Err(_) if crate::desktop::switched() => return Ok(()),
            Err(e) => return Err(e),
        };
        match grab {
            Grab::New { present_qpc } => {
                pending = Some(present_qpc);
                have_image = true;
            }
            Grab::Timeout => {}
            Grab::Lost => return Ok(()),
        }
        if pending.is_none() && have_image && sh.want_key.load(Ordering::Relaxed) {
            // A key frame was asked for while the screen is still: send the last image again.
            pending = Some(qpc());
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
                enc = new_encoder(&gpu, w, h, &cfg, quirks, rate)?;
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
        conv.blt(&cap.image, 0, &nv12, full)?;
        pending = None;
        let r = match enc.encode(&nv12, Duration::from_millis(100), None) {
            Ok(None) if no_output_since.get_or_insert_with(Instant::now).elapsed() > Duration::from_secs(1) => {
                Err(anyhow!("the encoder gave no frame for 1 s"))
            }
            r => r,
        };
        let e = match r {
            Ok(Some(e)) => e,
            Ok(None) => continue,
            Err(err) if enc.format == Codec::Hevc => {
                log!("HEVC encoder failed ({err:#}); using H.264 from now on");
                quirks.no_hevc = true;
                changed_at = None;
                no_output_since = None;
                enc = new_encoder(&gpu, w, h, &cfg, quirks, enc.bitrate)?;
                let _ = info.send(Msg::Info { width: w as u16, height: h as u16, encoder: enc.name.clone() });
                pending = Some(shown_at);
                continue;
            }
            Err(err) if changed_at.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) => {
                log!("encoder died after a bitrate change ({err:#}); changing the bitrate by rebuilding it from now on");
                quirks.dies = true;
                changed_at = None;
                no_output_since = None;
                tried = true;
                enc = new_encoder(&gpu, w, h, &cfg, quirks, enc.bitrate)?;
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
        let budget = (enc.bitrate as usize / 8 / cfg.fps as usize).max(1);
        let mut payload = Vec::with_capacity(e.data.len() + crate::proto::FRAME_HDR);
        let hevc = enc.format == Codec::Hevc;
        FrameHeader { key: e.key, hevc, width: w as u16, height: h as u16, host_us }.write(&mut payload);
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
