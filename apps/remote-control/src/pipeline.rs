//! Host capture loop, on its own thread: wait for a screen update, ask the
//! sender how big the frame may be, convert to NV12 (AYUV for 4:4:4, see
//! `yuv444`), encode, hand over.
//! At most `fps` frames per second; an update that arrives early waits for
//! its slot, and a newer update replaces it while it waits.
//!
//! The picture's size, frame rate and highest bitrate follow the viewer's
//! choice (`proto::Quality`); a new choice rebuilds the encoder, and goes on
//! with the same capture (see `Next`).
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
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use tokio::sync::mpsc::UnboundedSender;
use windows::Win32::{
    Foundation::RECT,
    Graphics::{
        Direct3D11::{D3D11_BIND_RENDER_TARGET, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Texture2D},
        Dxgi::{
            Common::{DXGI_FORMAT_AYUV, DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC},
            IDXGIOutput1,
        },
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
    pub want_key: Arc<AtomicBool>,
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
    /// Optional lightweight capture FPS cap from `Msg::Throttle` (`0` = full
    /// chosen fps, `>0` = throttle pacing without rebuilding the encoder).
    pub throttle_fps: AtomicU8,
}

/// After a screen change, the same picture is encoded again for at most this
/// long (see the module notes)...
const REFINE: Duration = Duration::from_secs(4);
/// ...waiting this long after the last real screen update before starting to
/// re-encode the still picture, so refining never races a live 60 Hz drag or
/// scroll and steals its encode slot.
const REFINE_IDLE: Duration = Duration::from_millis(80);
/// ...or until ~330 ms of consecutive frames came out nearly empty
/// (`refine_done`). The NVIDIA encoder above paused for up to 10 nearly
/// empty frames before it went on refining.
fn refine_tiny_run(fps: u32) -> u32 {
    (fps / 3).max(10)
}

/// How long to wait in `cap.grab` given whether a frame is already pending and
/// when the still-frame refiner should next run.
fn capture_wait(has_pending: bool, refine_armed: bool, since_new: Duration, next_slot: Duration) -> Duration {
    if has_pending {
        return next_slot;
    }
    if refine_armed {
        if since_new >= REFINE_IDLE {
            return next_slot.max(Duration::from_millis(1));
        }
        return REFINE_IDLE
            .saturating_sub(since_new)
            .clamp(Duration::from_millis(1), Duration::from_millis(50));
    }
    Duration::from_millis(50)
}

/// A frame this small adds nothing: a tenth of its budget, at least 250 bytes.
fn refine_done(bytes: usize, budget: usize) -> bool {
    bytes < (budget / 10).max(250)
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

/// A picture needs this share of `auto_bps` to look sharp enough: about 10
/// Mbps for 1440p at 60 frames, 5.6 for 1080p, 2.5 for 720p.
const NEED_SHARE: f64 = 0.28;
/// The rate below a picture's need this long makes the picture smaller...
const SHRINK_AFTER: Duration = Duration::from_secs(3);
/// ...and this much over the larger picture's need, this long, makes it
/// larger again, but not sooner than `GROW_GAP` after the last change: each
/// change starts a new encoder and a key frame.
const GROW_MARGIN: f64 = 1.3;
const GROW_AFTER: Duration = Duration::from_secs(10);
const GROW_GAP: Duration = Duration::from_secs(15);

/// Lowest frame rate a slow link drops to when frame rate is 自动.
const AUTO_FPS_FLOOR: u32 = 30;

/// A slow link streams a smaller or slower picture than the one chosen, only
/// changing the items the viewer left on 自动. At 3-5 Mbps (a phone hotspot's
/// upload, 2026-11-24) a 1440p picture turns to blocks as soon as it moves and
/// its frames overshoot their budget; 1080p or 720p at the same rate is
/// sharper, and if even 720p is starved, 30 fps gives each frame twice the bits.
#[derive(Default)]
struct AutoSize {
    /// The picture the steps start from; a new choice starts at it.
    chosen: Option<(Quality, (u32, u32), u32)>,
    /// 0 = the chosen picture, then smaller resolutions (if 自动), then 30 fps (if 自动).
    level: usize,
    /// The step the rate points to, and since when.
    pending: Option<(usize, Instant)>,
    changed: Option<Instant>,
}

impl AutoSize {
    /// The pictures to stream at, best first, for `q`, `full` size and `full_fps`.
    fn steps(q: Quality, full: (u32, u32), full_fps: u32) -> Vec<((u32, u32), u32)> {
        let mut sizes = vec![full];
        if q.fit_w > 0 && q.fit_h > 0 {
            for short in [1080, 720] {
                let p = picture_size(full.0, full.1, &Quality { short, ..Quality::default() }, 0);
                let last = sizes[sizes.len() - 1];
                if (p.0 as u64 * p.1 as u64) * 10 < (last.0 as u64 * last.1 as u64) * 8 {
                    sizes.push(p);
                }
            }
        }
        let smallest = sizes[sizes.len() - 1];
        let mut steps: Vec<((u32, u32), u32)> = sizes.into_iter().map(|s| (s, full_fps)).collect();
        if q.auto_fps && full_fps > AUTO_FPS_FLOOR {
            steps.push((smallest, AUTO_FPS_FLOOR));
        }
        steps
    }

    /// The size and frame rate to stream now, for the viewer's `q` and full choice.
    fn pick(&mut self, q: Quality, full: (u32, u32), full_fps: u32) -> ((u32, u32), u32) {
        if self.chosen != Some((q, full, full_fps)) {
            *self = AutoSize { chosen: Some((q, full, full_fps)), ..AutoSize::default() };
        }
        let steps = Self::steps(q, full, full_fps);
        self.level = self.level.min(steps.len() - 1);
        steps[self.level]
    }

    /// Whether the rate now asks for another picture (then `pick` gives it).
    /// Only a link that holds the rate under `max_bps` counts: a highest
    /// bitrate the viewer chose keeps its picture.
    fn step(&mut self, rate: f64, max_bps: f64, now: Instant) -> bool {
        let Some((q, full, full_fps)) = self.chosen else { return false };
        let steps = Self::steps(q, full, full_fps);
        let need = |i: usize| auto_bps(steps[i].0.0, steps[i].0.1, steps[i].1) as f64 * NEED_SHARE;
        let free = rate >= 0.9 * max_bps;
        let want = if self.level + 1 < steps.len() && rate < need(self.level) && !free {
            Some((self.level + 1, SHRINK_AFTER))
        } else if self.level > 0 && (free || rate >= GROW_MARGIN * need(self.level - 1)) {
            Some((self.level - 1, GROW_AFTER))
        } else {
            None
        };
        let Some((level, after)) = want else {
            self.pending = None;
            return false;
        };
        let since = match self.pending {
            Some((l, t)) if l == level => t,
            _ => self.pending.insert((level, now)).1,
        };
        let soon = level < self.level && self.changed.is_some_and(|t| now.duration_since(t) < GROW_GAP);
        if now.duration_since(since) < after || soon {
            return false;
        }
        log!(
            "rate {:.1} Mbps: streaming {}x{}@{} instead of {}x{}@{}",
            rate / 1e6,
            steps[level].0.0,
            steps[level].0.1,
            steps[level].1,
            steps[self.level].0.0,
            steps[self.level].0.1,
            steps[self.level].1,
        );
        self.level = level;
        self.pending = None;
        self.changed = Some(now);
        true
    }
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

/// Raises process priority, disables Windows 11 EcoQoS power throttling, and
/// opts out of Windows 11 background timer-resolution ignoring while a
/// streaming session is active, restoring defaults on drop.
/// Background/service processes on hybrid CPUs (e.g. Intel Core Ultra) are
/// otherwise scheduled on LP-E cores with half-clock VDBOX (~2x slower encode).
pub(crate) struct StreamingBoost {
    _timer: crate::audio::TimerPeriodGuard,
}

impl StreamingBoost {
    pub(crate) fn enter() -> Self {
        use windows::Win32::System::Threading::{
            GetCurrentProcess, HIGH_PRIORITY_CLASS, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling,
            SetPriorityClass, SetProcessInformation,
        };
        // Win11 22H2+ (processthreadsapi.h): keep timeBeginPeriod(1) effective
        // even when this process has no foreground window.
        const IGNORE_TIMER_RESOLUTION: u32 = 0x4;
        let _timer = crate::audio::TimerPeriodGuard::enter(1);
        unsafe {
            let p = GetCurrentProcess();
            let _ = SetPriorityClass(p, HIGH_PRIORITY_CLASS);
            let state = PROCESS_POWER_THROTTLING_STATE {
                Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
                ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED | IGNORE_TIMER_RESOLUTION,
                StateMask: 0,
            };
            let _ = SetProcessInformation(
                p,
                ProcessPowerThrottling,
                &state as *const _ as *const _,
                std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            );
        }
        StreamingBoost { _timer }
    }
}

impl Drop for StreamingBoost {
    fn drop(&mut self) {
        use windows::Win32::System::Threading::{
            GetCurrentProcess, NORMAL_PRIORITY_CLASS, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling, SetPriorityClass, SetProcessInformation,
        };
        unsafe {
            let p = GetCurrentProcess();
            let _ = SetPriorityClass(p, NORMAL_PRIORITY_CLASS);
            let state = PROCESS_POWER_THROTTLING_STATE {
                Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
                ControlMask: 0,
                StateMask: 0,
            };
            let _ = SetProcessInformation(
                p,
                ProcessPowerThrottling,
                &state as *const _ as *const _,
                std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            );
        }
    }
}

/// Runs until `sh.stop` is set. Rebuilds everything after a desktop switch,
/// mode change or error.
pub fn run(cfg: PipeConfig, tx: Arc<VideoTx>, sh: Arc<PipeShared>, info: UnboundedSender<Msg>) {
    if let Err(e) = mf::init_thread() {
        log!("media foundation: {e:#}");
        return;
    }
    let _boost = StreamingBoost::enter();
    let _wlan = crate::wlan::LowLatency::enter();
    // Keep the machine and its screen awake while someone is connected.
    unsafe {
        SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED);
    }
    let mut last_err = String::new();
    // What this machine's encoder does on a bitrate change; kept when the
    // encoder is rebuilt after a desktop switch.
    let mut quirks = RateQuirks::default();
    let mut auto = AutoSize::default();
    while !sh.stop.load(Ordering::Relaxed) {
        match run_once(cfg, &tx, &sh, &info, &mut quirks, &mut auto) {
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
    /// WGC failed: DXGI duplication until then (see `open_capture`).
    wgc_after: Option<Instant>,
}

/// After WGC fails, how long to capture with DXGI before trying it again.
const WGC_RETRY: Duration = Duration::from_secs(30);

/// WGC where it can be used, so the pointer stays out of the image; DXGI
/// duplication on the Winlogon desktop and for a while after WGC failed.
/// `desktop` is the input desktop the service's helper follows: there WGC
/// runs in an agent as the console user (see `wgc`).
fn open_capture(gpu: &Gpu, output: &IDXGIOutput1, desktop: Option<&str>, wgc_after: &mut Option<Instant>) -> Result<Capture> {
    let users_desktop = desktop.is_none_or(|d| d.eq_ignore_ascii_case("Default"));
    if users_desktop && wgc_after.is_none_or(|t| Instant::now() >= t) {
        let r = if desktop.is_some() { Capture::agent(gpu, output) } else { Capture::wgc(gpu, output) };
        match r {
            Ok(c) => return Ok(c),
            Err(e) => {
                log!("WGC: {e:#}; DXGI duplication for at least {} s", WGC_RETRY.as_secs());
                *wgc_after = Some(Instant::now() + WGC_RETRY);
            }
        }
    }
    Capture::dxgi(gpu, output)
}

/// Whether this graphics card encodes HEVC 4:4:4 from AYUV and can make the
/// AYUV picture. Found out once per card, with a small encoder.
fn can_encode_444(gpu: &Gpu) -> bool {
    static SEEN: Mutex<Vec<((u32, i32), bool)>> = Mutex::new(Vec::new());
    let id = (gpu.luid.LowPart, gpu.luid.HighPart);
    if let Some(&(_, ok)) = SEEN.lock().unwrap().iter().find(|(l, _)| *l == id) {
        return ok;
    }
    let ok = crate::encode::encoder_name(gpu.luid, Codec::Hevc).is_some_and(|n| !n.contains("Intel"))
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
    /// Zero-copy path for `wgc::Agent`: runs `ToNv12` straight from the
    /// acquired shared ring texture into `pic` during grab (no intermediate
    /// 22 MB BGRA copy into `cap.image`).
    Nv12Agent(crate::nv12::ToNv12, Vec<crate::nv12::Nv12Views>),
    Nv12(crate::nv12::ToNv12, crate::nv12::Nv12Views),
    Vp(Vp, vp::VpViews),
    /// Zero-copy 4:4:4 path for `wgc::Agent`: runs `ToAyuv` straight from the
    /// acquired shared ring texture into `pic` during grab.
    AyuvAgent(crate::yuv444::ToAyuv, Vec<crate::yuv444::AyuvViews>),
    Ayuv(crate::yuv444::ToAyuv, crate::yuv444::AyuvViews),
}

/// Computes the active capture pacing FPS given the encoder's configured `fps`
/// and optional lightweight `throttle_fps` (`0` = unthrottled).
pub(crate) fn effective_pace_fps(fps: u32, throttle_fps: u8) -> u32 {
    if throttle_fps > 0 {
        fps.min(throttle_fps as u32).max(1)
    } else {
        fps.max(1)
    }
}

/// Minimum spacing between encoded frames. When streaming at the display's
/// full refresh rate, capture already paces to VSync, so a looser guard avoids
/// slipping a frame whenever the previous encode finishes slightly late. When
/// down-sampling (e.g. 60 fps on a 120 Hz display), keep a tight guard so
/// intermediate VSync frames are cleanly skipped.
fn frame_min_gap(fps: u32, refresh_hz: u32) -> Duration {
    let frac = if fps >= refresh_hz { 0.5 } else { 0.85 };
    Duration::from_secs_f64(frac / fps.max(1) as f64)
}

/// When the next frame may start encoding. Below the display's refresh rate
/// the slots are a frame interval apart: the guard alone let 128 frames a
/// second through when 120 were asked of a 180 Hz display (measured
/// 2026-11-20), more than the viewer's 120 Hz screen shows. A late frame
/// moves the slots on, by no more than the guard.
struct Pace {
    interval: Duration,
    guard: Duration,
    slots: bool,
}

impl Pace {
    fn new(fps: u32, refresh_hz: u32) -> Pace {
        Pace {
            interval: Duration::from_secs_f64(1.0 / fps.max(1) as f64),
            guard: frame_min_gap(fps, refresh_hz),
            slots: fps < refresh_hz,
        }
    }

    /// A frame starts now; `due` was its slot.
    fn next(&self, due: Option<Instant>, now: Instant) -> Instant {
        match due {
            Some(d) if self.slots => (d + self.interval).max(now + self.guard),
            _ => now + self.guard,
        }
    }
}

/// Whether to use this HEVC encoder when both ends can. Intel's Media
/// Foundation HEVC MFT (Arc, driver 31.0.101.5382) dies within half a second
/// in sessions and poisons later H.264 encoders (P1-2), whereas native Intel
/// oneVPL QSV bypasses that MFT cleanly and encodes ~2x faster than H.264.
fn hevc_trusted(encoder: &str) -> bool {
    !encoder.contains("Intel") || encoder.contains("oneVPL")
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

/// Why the stream of one picture ended.
enum Next {
    /// Another picture (size, frame rate, 4:4:4, encoder) from the same
    /// capture: a new capture would cost 0.4 s more (0.48 s from the viewer's
    /// choice to the new picture with the service's agent, 2026-11-24), most
    /// of it the agent starting again and the graphics device being made.
    Picture,
    /// The capture is opened again: a desktop switch, another display, WGC
    /// failed, the image was lost.
    Capture,
}

fn run_once(
    cfg: PipeConfig,
    tx: &VideoTx,
    sh: &PipeShared,
    info: &UnboundedSender<Msg>,
    quirks: &mut RateQuirks,
    auto: &mut AutoSize,
) -> Result<()> {
    // Service mode: capture the desktop that is shown now (see desktop.rs).
    let desktop = crate::desktop::follow()?;
    let (gpu, output, rect) = crate::monitors::open(&sh.monitors)?;
    sh.no_display.store(false, Ordering::Relaxed);
    *sh.display.lock().unwrap() = rect;
    let _ = info.send(crate::monitors::message(&sh.monitors));
    let mut cap = open_capture(&gpu, &output, desktop.as_deref(), &mut quirks.wgc_after)?;
    while !sh.stop.load(Ordering::Relaxed) {
        if let Next::Capture = stream(cfg, tx, sh, info, quirks, auto, &gpu, &mut cap, desktop.as_deref())? {
            break;
        }
    }
    Ok(())
}

/// Streams one picture from `cap` until something asks for another one.
#[allow(clippy::too_many_arguments)]
fn stream(
    cfg: PipeConfig,
    tx: &VideoTx,
    sh: &PipeShared,
    info: &UnboundedSender<Msg>,
    quirks: &mut RateQuirks,
    auto: &mut AutoSize,
    gpu: &Gpu,
    cap: &mut Capture,
    desktop: Option<&str>,
) -> Result<Next> {
    let (cw, ch) = cap.upright_size();
    let quality = {
        let g = sh.quality.lock().unwrap();
        sh.quality_changed.store(false, Ordering::Release);
        *g
    };
    let full = picture_size(cw, ch, &quality, cfg.max_width);
    let refresh = crate::monitors::refresh_hz(&sh.monitors);
    let full_fps = stream_fps(&quality, &cfg, refresh);
    let max_bps = stream_bps(&quality, &cfg, full.0, full.1, full_fps);
    let ((w, h), fps) = auto.pick(quality, full, full_fps);
    tx.configure(fps, max_bps as f64);
    // The shader that makes the 4:4:4 picture does not turn it.
    let mut can444 = cfg.viewer444 && !quirks.no_hevc && !quirks.no_444 && cap.rotation == 0 && can_encode_444(gpu);
    let yuv444 = can444 && quality.yuv444;
    // Mid-session (another picture) at the rate the link carries, not the
    // highest: the encoder's first frame is a key frame.
    let start_bps = (tx.bitrate() as u32).clamp(1_000_000, max_bps);
    let mut enc = match new_encoder(gpu, w, h, fps, &cfg, quirks, start_bps, yuv444) {
        Err(e) if yuv444 => {
            log!("4:4:4 encoder: {e:#}; 4:2:0 from now on");
            quirks.no_444 = true;
            return Ok(Next::Picture);
        }
        r => r?,
    };
    if !yuv444 && cfg.hevc && enc.format != Codec::Hevc {
        // HEVC encoder was unavailable or refused on this adapter; don't advertise 4:4:4 HEVC.
        can444 = false;
    }
    let (pic_w, pic_h) = if yuv444 { (w, h) } else { enc.preferred_input_size().unwrap_or((w, h)) };
    let pic = yuv_texture(gpu, pic_w, pic_h, yuv444)?;
    let inv_sdr_white = cap.hdr_info.inv_sdr_white();
    let vp_in_cs = if cap.fp16 { vp::RGB_SCRGB } else { vp::RGB_FULL };
    let conv = if yuv444 {
        let ayuv_res = crate::yuv444::ToAyuv::shader_with_hdr(gpu, (w, h), inv_sdr_white).and_then(|shader| {
            if let Some(ring) = cap.agent_ring() {
                let views = ring
                    .iter()
                    .map(|t| shader.make_views(gpu, t, &pic))
                    .collect::<Result<Vec<_>>>()?;
                Ok(Convert::AyuvAgent(shader, views))
            } else {
                let img = cap.image(gpu)?;
                let views = shader.make_views(gpu, img, &pic)?;
                Ok(Convert::Ayuv(shader, views))
            }
        });
        match ayuv_res {
            Ok(c) => c,
            Err(e) => {
                log!("4:4:4: {e:#}; 4:2:0 from now on");
                quirks.no_444 = true;
                return Ok(Next::Picture);
            }
        }
    } else if cap.rotation == 0 {
        let nv12_res = crate::nv12::ToNv12::new_with_hdr(gpu, (w, h), inv_sdr_white).and_then(|shader| {
            if let Some(ring) = cap.agent_ring() {
                let views = ring
                    .iter()
                    .map(|t| shader.make_views(gpu, t, &pic))
                    .collect::<Result<Vec<_>>>()?;
                Ok(Convert::Nv12Agent(shader, views))
            } else {
                let img = cap.image(gpu)?;
                let views = shader.make_views(gpu, img, &pic)?;
                Ok(Convert::Nv12(shader, views))
            }
        });
        match nv12_res {
            Ok(c) => c,
            Err(e) => {
                log!("nv12 shader: {e:#}; falling back to VideoProcessor");
                let vp = Vp::new(gpu, (cap.width, cap.height), (w, h), vp_in_cs, vp::YUV_709)?;
                let img = cap.image(gpu)?;
                let views = vp.make_views(img, 0, &pic)?;
                Convert::Vp(vp, views)
            }
        }
    } else {
        let vp = Vp::new(gpu, (cap.width, cap.height), (w, h), vp_in_cs, vp::YUV_709)?;
        if !vp.rotate(cap.rotation) {
            log!("this video processor cannot rotate: the {}° display is sent sideways", cap.rotation);
        }
        let img = cap.image(gpu)?;
        let views = vp.make_views(img, 0, &pic)?;
        Convert::Vp(vp, views)
    };
    let hdr_tag = if cap.fp16 {
        format!(" [HDR FP16, SDR white {:.2}x]", cap.hdr_info.sdr_white_scale)
    } else {
        String::new()
    };
    log!(
        "capturing {}x{} at {refresh} Hz on {} with {}{hdr_tag}, encoding {w}x{h} {} at up to {fps} fps and {:.1} Mbps with {} ({quality:?})",
        cap.width,
        cap.height,
        gpu.name,
        cap.kind(),
        if yuv444 { "4:4:4" } else { "4:2:0" },
        max_bps as f64 / 1e6,
        enc.name
    );
    let info_msg = |enc: &Encoder, can444: bool| Msg::Info {
        width: w as u16,
        height: h as u16,
        encoder: enc.name.clone(),
        streaming: Some(Streaming { fps: fps.min(255) as u8, max_kbps: max_bps / 1000, can444, yuv444, resends: true }),
    };
    let _ = info.send(info_msg(&enc, can444));
    if let Some(name) = desktop {
        log!("capturing desktop {name}");
        let _ = info.send(Msg::Desktop { name: name.to_string() });
    }
    sh.want_key.store(true, Ordering::Relaxed);

    let freq = qpc_freq();
    let mut cur_pace_fps = effective_pace_fps(fps, sh.throttle_fps.load(Ordering::Relaxed));
    let mut pace = Pace::new(cur_pace_fps, refresh);
    let full = RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
    // A capture that goes on from the last picture brings no new image while
    // the screen is still: start from the last one.
    let again = match &conv {
        Convert::Nv12Agent(shader, views) => cap.again(|slot| shader.run(gpu, &views[slot])),
        Convert::AyuvAgent(shader, views) => cap.again(|slot| shader.run_views(gpu, &views[slot])),
        _ => cap.has_image(),
    };
    if again {
        log!("the new picture starts from the last image");
    }
    let mut pending: Option<i64> = again.then(qpc);
    let mut have_image = again;
    let mut pic_dirty = again;
    // Encoding the still picture again until then (see the module notes),
    // and how many nearly empty frames came out of that in a row.
    let mut refine_until: Option<Instant> = None;
    let mut tiny_run = 0u32;
    let mut refining = false;
    // This round of refining so far: frames, bytes, since when (for the log).
    let mut refined = (0u32, 0usize, Instant::now());
    let mut due: Option<Instant> = None;
    let mut last_new_frame = Instant::now();
    let mut last_change = Instant::now();
    let mut tried = quirks.rekeys || quirks.dies;
    // When the encoder's bitrate was last changed in place.
    let mut changed_at: Option<Instant> = None;
    // Backoff after a failed or clamped set_bitrate so we don't spin Reset every frame.
    let mut rate_cooldown_until: Option<Instant> = None;
    // Since when the encoder has been handing back nothing.
    let mut no_output_since: Option<Instant> = None;
    let mut frames = 0u64;
    let mut desktop_checked = Instant::now();
    while !sh.stop.load(Ordering::Relaxed) {
        if sh.quality_changed.load(Ordering::Acquire) {
            log!("the viewer chose another picture");
            return Ok(Next::Picture);
        }
        let throttle = sh.throttle_fps.load(Ordering::Relaxed);
        let eff_fps = effective_pace_fps(fps, throttle);
        if eff_fps != cur_pace_fps {
            let restoring = eff_fps > cur_pace_fps;
            cur_pace_fps = eff_fps;
            pace = Pace::new(eff_fps, refresh);
            if restoring {
                // Zero-latency restore when unminimizing: do not wait out a
                // leftover 200 ms throttled slot.
                due = None;
            }
        }
        if throttle == 0 && frames > 0 && auto.step(tx.bitrate(), max_bps as f64, Instant::now()) {
            return Ok(Next::Picture);
        }
        // Service mode: do not count on the duplication to report a desktop
        // switch (the Winlogon desktop is often still); look every 200 ms.
        if desktop_checked.elapsed() >= Duration::from_millis(200) {
            desktop_checked = Instant::now();
            if crate::desktop::switched() || crate::monitors::should_reopen(&sh.monitors) {
                return Ok(Next::Capture);
            }
        }
        if tx.is_paused() {
            pending = None;
            refine_until = None;
            no_output_since = None;
            due = None;
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        if throttle > 0 {
            refine_until = None;
        }
        if refine_until.is_some_and(|t| Instant::now() >= t) {
            log_refined(&refined, "time up");
            refine_until = None;
        }
        let refine_armed = refine_until.is_some();
        let since_new = last_new_frame.elapsed();
        let next_slot = || due.map_or(Duration::ZERO, |d| d.saturating_duration_since(Instant::now()));
        let wait = capture_wait(pending.is_some(), refine_armed, since_new, next_slot());
        // Cap grab wait while throttled so unminimizing wakes and restores full FPS promptly.
        let wait = if throttle > 0 { wait.min(Duration::from_millis(50)) } else { wait };
        let wait_ms = wait.as_micros().div_ceil(1000) as u32;
        let grab_res = match &conv {
            Convert::Nv12Agent(shader, views) => cap
                .grab_agent_with(wait_ms, |slot| shader.run(gpu, &views[slot]))
                .expect("Nv12Agent is only constructed for Agent capture"),
            Convert::AyuvAgent(shader, views) => cap
                .grab_agent_with(wait_ms, |slot| shader.run_views(gpu, &views[slot]))
                .expect("AyuvAgent is only constructed for Agent capture"),
            _ => cap.grab(gpu, wait_ms),
        };
        let grab = match grab_res {
            Ok(g) => g,
            // Leaving Winlogon ends its duplication with DXGI_ERROR_INVALID_CALL
            // (test laptop 2026-11-08): rebuild now rather than after the error pause.
            Err(_) if crate::desktop::switched() => return Ok(Next::Capture),
            Err(e) if cap.is_wgc() => {
                log!("WGC: {e:#}; DXGI duplication for at least {} s", WGC_RETRY.as_secs());
                quirks.wgc_after = Some(Instant::now() + WGC_RETRY);
                return Ok(Next::Capture);
            }
            Err(e) => return Err(e),
        };
        match grab {
            Grab::New { present_qpc } => {
                pending = Some(present_qpc);
                have_image = true;
                pic_dirty = true;
                last_new_frame = Instant::now();
                refining = false;
            }
            Grab::Timeout => {}
            Grab::Lost => return Ok(Next::Capture),
        }
        if sh.want_key.load(Ordering::Relaxed) && tx.key_frame_queued() {
            sh.want_key.store(false, Ordering::Relaxed);
            log!("a key frame was asked for while the last one is still on its way: not sending another");
        }
        if pending.is_none() && have_image && sh.want_key.load(Ordering::Relaxed) {
            // A key frame was asked for while the screen is still: send the last image again.
            pending = Some(qpc());
        }
        if pending.is_none() && have_image && refine_armed && last_new_frame.elapsed() >= REFINE_IDLE {
            pending = Some(qpc());
            refining = true;
        }
        let Some(shown_at) = pending else { continue };
        if due.is_some_and(|d| Instant::now() < d) {
            continue;
        }
        let expensive = enc.rekeys_on_rate_change || quirks.dies;
        let shrink = cfg.shrink && !expensive;
        let Plan::Encode { chosen, bitrate, normal, shrunk } = tx.plan(enc.bitrate, shrink) else {
            // Drop this frame slot and pace to the next frame interval so we
            // don't spin-call tx.plan() while the send buffer drains.
            pending = None;
            due = Some(pace.next(due, Instant::now()));
            continue;
        };
        due = Some(pace.next(due, Instant::now()));
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
        } else if rate_cooldown_until.is_none_or(|t| Instant::now() >= t)
            && should_change(enc.bitrate, if shrunk { bitrate } else { chosen }, last_change.elapsed(), expensive)
        {
            let rate = if shrunk { bitrate } else { chosen };
            if quirks.dies {
                // A new encoder starts with a key frame, like a change on an
                // encoder that rekeys anyway.
                enc = new_encoder(gpu, w, h, fps, &cfg, quirks, rate, yuv444)?;
                no_output_since = None;
            } else if let Some(top) =
                enc.max_bitrate().filter(|&top| !shrunk && rate > top && should_change(top, rate, last_change.elapsed(), true))
            {
                // QSV goes no higher than the rate it was made with, and one
                // made mid-session starts at the link's rate then (see above):
                // once the link carries more, only a new encoder can follow.
                // It starts with a key frame, so only for a clearly higher
                // rate. Room for twice the rate, so a climbing rate needs few of them.
                let room = (rate as u64 * 2).min(max_bps as u64) as u32;
                log!("encoder tops out at {:.1} Mbps, the rate is {:.1}: new encoder for up to {:.1}", top as f64 / 1e6, rate as f64 / 1e6, room as f64 / 1e6);
                enc = new_encoder(gpu, w, h, fps, &cfg, quirks, room, yuv444)?;
                if room > rate {
                    enc.set_bitrate(rate);
                }
                no_output_since = None;
                changed_at = None;
                rate_cooldown_until = None;
                if !yuv444 && enc.preferred_input_size().unwrap_or((w, h)) != (pic_w, pic_h) {
                    return Ok(Next::Picture);
                }
            } else {
                let prev = enc.bitrate;
                enc.set_bitrate(rate);
                if enc.bitrate != prev {
                    changed_at = Some(Instant::now());
                }
                if should_change(enc.bitrate, rate, Duration::MAX, false) {
                    rate_cooldown_until = Some(Instant::now() + Duration::from_secs(3));
                } else {
                    rate_cooldown_until = None;
                }
            }
            last_change = Instant::now();
        }
        if sh.want_key.swap(false, Ordering::Relaxed) {
            enc.request_key();
        }
        let t_enc = Instant::now();
        let enc_start_qpc = qpc();
        match &conv {
            Convert::Nv12Agent(_, _) | Convert::AyuvAgent(_, _) => pic_dirty = false,
            Convert::Nv12(s, views) if pic_dirty => {
                s.run(gpu, views);
                pic_dirty = false;
            }
            Convert::Vp(v, views) if pic_dirty => {
                v.blt_views(views, full)?;
                pic_dirty = false;
            }
            Convert::Ayuv(a, views) if pic_dirty => {
                a.run_views(gpu, views);
                pic_dirty = false;
            }
            Convert::Nv12(_, _) | Convert::Vp(_, _) | Convert::Ayuv(_, _) => {}
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
                return Ok(Next::Picture);
            }
            Err(err) if enc.format == Codec::Hevc => {
                log!("HEVC encoder failed ({err:#}); using H.264 from now on");
                quirks.no_hevc = true;
                changed_at = None;
                rate_cooldown_until = None;
                no_output_since = None;
                let prev_bitrate = enc.bitrate;
                let init_bitrate = if quirks.dies { prev_bitrate } else { max_bps };
                enc = new_encoder(gpu, w, h, fps, &cfg, quirks, init_bitrate, false)?;
                if !quirks.dies && prev_bitrate < max_bps {
                    enc.set_bitrate(prev_bitrate);
                }
                if enc.preferred_input_size().unwrap_or((w, h)) != (pic_w, pic_h) {
                    return Ok(Next::Picture);
                }
                let _ = info.send(info_msg(&enc, false));
                pending = Some(shown_at);
                continue;
            }
            Err(err) if changed_at.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) => {
                log!("encoder died after a bitrate change ({err:#}); changing the bitrate by rebuilding it from now on");
                quirks.dies = true;
                changed_at = None;
                rate_cooldown_until = None;
                no_output_since = None;
                tried = true;
                let prev_bitrate = enc.bitrate;
                enc = new_encoder(gpu, w, h, fps, &cfg, quirks, prev_bitrate, yuv444)?;
                if !yuv444 && enc.preferred_input_size().unwrap_or((w, h)) != (pic_w, pic_h) {
                    return Ok(Next::Picture);
                }
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
            if tiny_run >= refine_tiny_run(fps) {
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
    Ok(Next::Capture)
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
    use std::time::{Duration, Instant};

    #[test]
    fn a_slow_link_streams_a_smaller_picture() {
        use crate::proto::Quality;
        let q = Quality { fit_w: 2560, fit_h: 1440, short: 0, fps: 60, kbps: 0, yuv444: false, auto_fps: true };
        let full = super::picture_size(2880, 1920, &q, 0);
        assert_eq!(full, (2160, 1440));
        assert_eq!(
            super::AutoSize::steps(q, full, 60),
            vec![((2160, 1440), 60), ((1620, 1080), 60), ((1080, 720), 60), ((1080, 720), 30)]
        );
        let max = super::auto_bps(2160, 1440, 60) as f64;
        let mut a = super::AutoSize::default();
        assert_eq!(a.pick(q, full, 60), (full, 60));
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mbps = 1e6;
        // 1440p@60 needs about 8.4 Mbps: 3 s under it gives 1080p@60.
        assert!(!a.step(6.0 * mbps, max, at(0)));
        assert!(!a.step(6.0 * mbps, max, at(2_900)));
        assert!(a.step(6.0 * mbps, max, at(3_000)));
        assert_eq!(a.pick(q, full, 60), ((1620, 1080), 60));
        // 1080p@60 needs about 4.7: a dip that recovers within 3 s changes nothing.
        assert!(!a.step(3.0 * mbps, max, at(4_000)));
        assert!(!a.step(6.0 * mbps, max, at(6_000)));
        assert!(!a.step(3.0 * mbps, max, at(7_000)));
        assert!(a.step(3.0 * mbps, max, at(10_000)));
        assert_eq!(a.pick(q, full, 60), ((1080, 720), 60));
        // 720p@60 needs 2.1 Mbps: still lower drops the frame rate to 30 fps.
        assert!(!a.step(1.5 * mbps, max, at(11_000)));
        assert!(!a.step(1.5 * mbps, max, at(13_900)));
        assert!(a.step(1.5 * mbps, max, at(14_000)));
        assert_eq!(a.pick(q, full, 60), ((1080, 720), 30));
        // Recovery reverses the order: 30->60 fps first (needs 1.3 * 2.1 = 2.73 Mbps),
        // respecting GROW_AFTER (10 s) and GROW_GAP (15 s since last change at 14 s).
        assert!(!a.step(3.0 * mbps, max, at(15_000)));
        assert!(!a.step(3.0 * mbps, max, at(25_000)), "11 s after last change < 15 s GROW_GAP");
        assert!(!a.step(3.0 * mbps, max, at(28_900)), "14.9 s after last change");
        assert!(a.step(3.0 * mbps, max, at(29_000)));
        assert_eq!(a.pick(q, full, 60), ((1080, 720), 60));
        // Then 720p->1080p (needs 1.3 * 4.725 = 6.14 Mbps).
        assert!(!a.step(6.5 * mbps, max, at(30_000)));
        assert!(!a.step(6.5 * mbps, max, at(40_000)), "10 s at rate, but 11 s since change < 15 s");
        assert!(a.step(6.5 * mbps, max, at(44_000)));
        assert_eq!(a.pick(q, full, 60), ((1620, 1080), 60));
        // Then 1080p->full (at max = free link, respecting GROW_AFTER and GROW_GAP).
        assert!(!a.step(max, max, at(50_000)));
        assert!(!a.step(max, max, at(59_000)), "9 s at rate");
        assert!(a.step(max, max, at(60_000)));
        assert_eq!(a.pick(q, full, 60), (full, 60));
        // A highest bitrate the viewer chose below the need (rate == max) keeps the picture...
        let low = 5.0 * mbps;
        assert!(!a.step(low, low, at(70_000)));
        assert!(!a.step(low, low, at(80_000)));
        assert_eq!(a.pick(q, full, 60), (full, 60));
        // ...and another choice starts at the chosen picture again.
        assert!(a.step(1.0 * mbps, max, at(81_000)) || a.step(1.0 * mbps, max, at(84_000)));
        let q720 = Quality { short: 720, fit_w: 0, fit_h: 0, ..q };
        let small = super::picture_size(2880, 1920, &q720, 0);
        assert_eq!(a.pick(q720, small, 60), (small, 60));
        assert_eq!(super::AutoSize::steps(q720, small, 60), vec![(small, 60), (small, 30)], "fixed 720p only steps fps");
        assert_eq!(super::AutoSize::steps(q720, small, 30), vec![(small, 30)], "never below 30 fps");
    }

    #[test]
    fn auto_degrade_respects_fixed_choices() {
        use crate::proto::Quality;
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mbps = 1e6;

        // Fixed resolution (short: 1440, fit 0) + auto fps: only fps drops to 30, never size.
        let q_fixed_res = Quality { fit_w: 0, fit_h: 0, short: 1440, fps: 60, kbps: 0, yuv444: false, auto_fps: true };
        let full = super::picture_size(2880, 1920, &q_fixed_res, 0);
        assert_eq!(full, (2160, 1440));
        let max = super::auto_bps(full.0, full.1, 60) as f64;
        let mut a = super::AutoSize::default();
        assert_eq!(a.pick(q_fixed_res, full, 60), (full, 60));
        assert!(!a.step(2.0 * mbps, max, at(0)));
        assert!(a.step(2.0 * mbps, max, at(3_000)));
        assert_eq!(a.pick(q_fixed_res, full, 60), (full, 30));
        // Stays at full@30 even at 1 Mbps.
        assert!(!a.step(1.0 * mbps, max, at(4_000)));
        assert!(!a.step(1.0 * mbps, max, at(10_000)));
        assert_eq!(a.pick(q_fixed_res, full, 60), (full, 30));

        // Fixed resolution + fixed fps: nothing ever changes.
        let q_fixed_both = Quality { auto_fps: false, ..q_fixed_res };
        let mut a = super::AutoSize::default();
        assert_eq!(a.pick(q_fixed_both, full, 60), (full, 60));
        assert!(!a.step(1.0 * mbps, max, at(0)));
        assert!(!a.step(1.0 * mbps, max, at(10_000)));
        assert_eq!(a.pick(q_fixed_both, full, 60), (full, 60));

        // Auto resolution + fixed fps: only the size changes, fps never drops.
        let q_auto_res = Quality { fit_w: 2560, fit_h: 1440, short: 0, fps: 60, kbps: 0, yuv444: false, auto_fps: false };
        let mut a = super::AutoSize::default();
        assert_eq!(a.pick(q_auto_res, full, 60), (full, 60));
        assert!(!a.step(1.0 * mbps, max, at(0)));
        assert!(a.step(1.0 * mbps, max, at(3_000)));
        assert_eq!(a.pick(q_auto_res, full, 60), ((1620, 1080), 60));
        assert!(!a.step(1.0 * mbps, max, at(3_100)));
        assert!(a.step(1.0 * mbps, max, at(6_100)));
        assert_eq!(a.pick(q_auto_res, full, 60), ((1080, 720), 60));
        assert!(!a.step(1.0 * mbps, max, at(6_200)));
        assert!(!a.step(1.0 * mbps, max, at(20_000)));
        assert_eq!(a.pick(q_auto_res, full, 60), ((1080, 720), 60));
    }

    #[test]
    fn encode_size_is_even_and_scaled() {
        assert_eq!(super::encode_size(2880, 1920, 0), (2880, 1920));
        assert_eq!(super::encode_size(1365, 767, 0), (1364, 766));
        assert_eq!(super::encode_size(3840, 2160, 1920), (1920, 1080));
    }

    #[test]
    fn picture_size_follows_the_viewers_choice() {
        use crate::proto::Quality;
        let q = |fit_w, fit_h, short| Quality { fit_w, fit_h, short, fps: 60, kbps: 0, yuv444: false, auto_fps: false };
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
        // At 2 Mbps / 120 fps (~2 KB budget), real 500 B detail deltas
        // still refine; only frames under 250 B count as empty.
        assert!(!super::refine_done(500, 2_083));
        assert!(super::refine_done(200, 2_083));
        assert_eq!(super::refine_tiny_run(60), 20);
        assert_eq!(super::refine_tiny_run(120), 40);
        assert_eq!(super::refine_tiny_run(144), 48);
        assert_eq!(super::refine_tiny_run(15), 10);
    }

    #[test]
    fn refine_waits_for_motion_to_settle() {
        let ms = Duration::from_millis;
        // During active 60 Hz motion (e.g. 16 ms since last new frame), do not
        // wake up at next_slot (~2 ms) to re-encode a stale frame: wait for
        // either a new DWM frame or the remaining idle window (capped at 50 ms).
        assert_eq!(super::capture_wait(false, true, ms(16), ms(2)), ms(50));
        assert_eq!(super::capture_wait(false, true, ms(60), ms(2)), ms(20));
        // Once idle >= REFINE_IDLE, pace refine frames by next_slot (min 1 ms).
        assert_eq!(super::capture_wait(false, true, ms(80), ms(0)), ms(1));
        assert_eq!(super::capture_wait(false, true, ms(120), ms(12)), ms(12));
        // Pending real frames always honor next_slot directly.
        assert_eq!(super::capture_wait(true, true, ms(5), ms(4)), ms(4));
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

    #[test]
    fn frame_min_gap_avoids_phase_slip_at_native_refresh() {
        // Streaming at native 120 Hz: allows a slightly late frame without
        // delaying the next VSync (8.33 ms) frame.
        let g120 = super::frame_min_gap(120, 120);
        assert!(g120 < Duration::from_millis(5));
        // Down-sampling 60 fps on a 120 Hz screen: still skips the 8.33 ms
        // intermediate VSync frame and encodes every 16.67 ms frame.
        let g60 = super::frame_min_gap(60, 120);
        assert!(g60 > Duration::from_millis(12) && g60 < Duration::from_millis(16));
    }

    /// Frames sent in 10 s from a display showing a new image at every
    /// refresh: an image waits for its slot unless the next image comes first.
    fn sent_per_second(fps: u32, hz: u32) -> f64 {
        let pace = super::Pace::new(fps, hz);
        let t0 = Instant::now();
        let refresh = Duration::from_secs_f64(1.0 / hz as f64);
        let (mut due, mut sent) = (None, 0);
        for k in 0..hz * 10 {
            let image = t0 + refresh * k;
            let at = due.map_or(image, |d: Instant| d.max(image));
            if at < image + refresh {
                due = Some(pace.next(due, at));
                sent += 1;
            }
        }
        sent as f64 / 10.0
    }

    #[test]
    fn pacing_keeps_to_the_frame_rate() {
        for (fps, hz) in [(120, 180), (60, 120), (30, 60), (90, 144), (120, 120), (144, 120)] {
            let want = fps.min(hz) as f64;
            let got = sent_per_second(fps, hz);
            assert!((got - want).abs() <= 1.0, "{fps} fps from a {hz} Hz display: {got} a second");
        }
    }
}
