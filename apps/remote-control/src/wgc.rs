//! Windows.Graphics.Capture (WGC) of one display, with the pointer left out
//! of the image. DXGI duplication draws the pointer into the image while a
//! window is dragged, and the viewer, which shows a pointer of its own, then
//! shows two (observed on a 120 Hz laptop, 2026-11-19); WGC with cursor capture off never does.
//!
//! WGC does not start as SYSTEM (0x80070424) and cannot see the Winlogon
//! desktop (lock, login and UAC screens). So the service's helper (SYSTEM)
//! runs `remote-control capture-agent` as the console user, which captures
//! and hands every frame over in a texture shared on the graphics card,
//! guarded by a keyed mutex (key 0: the agent's turn, 1: the helper's). On
//! Winlogon the helper duplicates with DXGI as before (`capture`).
//!
//! Measured on a laptop (2880x1920, 120 Hz, 2026-11-19) with an animated
//! square, from drawn to in the helper's hands: DXGI 114.6 fps, median 8.5 ms,
//! p99 16.8; WGC through the agent 119.4 fps, median 12.1 ms, p99 18.2, and
//! about 34 ms of CPU per second more for both processes together.

use std::{
    io::{BufRead, BufReader, Read, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use windows::{
    Foundation::{TimeSpan, TypedEventHandler},
    Graphics::{
        Capture::{Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession},
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
    },
    Win32::{
        Foundation::{CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, HWND, S_OK, WAIT_OBJECT_0},
        Graphics::{
            Direct3D11::{
                D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX, D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_TEXTURE2D_DESC,
                ID3D11Device1, ID3D11Texture2D,
            },
            Dxgi::{
                Common::DXGI_FORMAT_R16G16B16A16_FLOAT, CreateDXGIFactory1, DXGI_SHARED_RESOURCE_READ,
                DXGI_SHARED_RESOURCE_WRITE, IDXGIDevice, IDXGIFactory1, IDXGIKeyedMutex, IDXGIOutput1,
                IDXGIResource1,
            },
            Gdi::HMONITOR,
        },
        System::{
            Performance::QueryPerformanceCounter,
            Threading::{CreateEventW, GetCurrentProcess, SetEvent, WaitForSingleObject},
            WinRT::{
                Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
                Graphics::Capture::IGraphicsCaptureItemInterop,
                RO_INIT_MULTITHREADED, RoInitialize,
            },
        },
    },
    core::{Interface, PCWSTR},
};

use crate::{capture::Grab, gpu::Gpu, log, term_proc};

fn qpc() -> i64 {
    let mut v = 0;
    unsafe {
        let _ = QueryPerformanceCounter(&mut v);
    }
    v
}

/// An event handle, closed when the last owner goes.
struct Event(HANDLE);

// Kernel handles may be used from any thread.
unsafe impl Send for Event {}
unsafe impl Sync for Event {}

impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub struct Wgc {
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    /// Set by WGC when a frame arrives or the captured item closes.
    arrived: Arc<Event>,
    closed: Arc<AtomicBool>,
    closed_token: i64,
    pub width: u32,
    pub height: u32,
    pub fp16: bool,
}

impl Wgc {
    /// Captures `monitor` onto `gpu`'s device. Fails where WGC cannot leave
    /// the pointer out or capture without its yellow border (Windows 10):
    /// DXGI then. When `want_fp16` is true (HDR display), creates an FP16 scRGB
    /// frame pool with seamless fallback to BGRA8 if unsupported.
    pub fn start(gpu: &Gpu, monitor: HMONITOR, want_fp16: bool) -> Result<Wgc> {
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().context("WGC")?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(monitor) }.context("CreateForMonitor")?;
        Self::start_item(gpu, item, want_fp16)
    }

    /// Captures a single top-level window `hwnd` onto `gpu`'s device via WGC
    /// (`CreateForWindow`), leaving the cursor and yellow border out.
    pub fn start_window(gpu: &Gpu, hwnd: HWND) -> Result<Wgc> {
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().context("WGC")?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(hwnd) }.context("CreateForWindow")?;
        Self::start_item(gpu, item, false)
    }

    fn start_item(gpu: &Gpu, item: GraphicsCaptureItem, want_fp16: bool) -> Result<Wgc> {
        let size = item.Size()?;
        if size.Width <= 0 || size.Height <= 0 {
            bail!("WGC item has zero size");
        }
        let dxgi: IDXGIDevice = gpu.device.cast()?;
        let d3d: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }?.cast()?;
        let (pool, fp16) = if want_fp16 {
            match Direct3D11CaptureFramePool::CreateFreeThreaded(
                &d3d,
                DirectXPixelFormat::R16G16B16A16Float,
                2,
                size,
            ) {
                Ok(p) => (p, true),
                Err(_) => (
                    Direct3D11CaptureFramePool::CreateFreeThreaded(
                        &d3d,
                        DirectXPixelFormat::B8G8R8A8UIntNormalized,
                        2,
                        size,
                    )?,
                    false,
                ),
            }
        } else {
            (
                Direct3D11CaptureFramePool::CreateFreeThreaded(
                    &d3d,
                    DirectXPixelFormat::B8G8R8A8UIntNormalized,
                    2,
                    size,
                )?,
                false,
            )
        };
        let session = pool.CreateCaptureSession(&item)?;
        let _ = session.SetIsCursorCaptureEnabled(false);
        let _ = session.SetIsBorderRequired(false);
        let _ = session.SetIncludeSecondaryWindows(true);
        // Every composed frame, not at most 60 a second (newer Windows 11 only).
        let _ = session.SetMinUpdateInterval(TimeSpan { Duration: 10_000 });
        let ev = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }.context("CreateEventW")?;
        let arrived = Arc::new(Event(ev));
        let a = arrived.clone();
        pool.FrameArrived(&TypedEventHandler::new(move |_, _| {
            unsafe {
                let _ = SetEvent(a.0);
            }
            Ok(())
        }))?;
        let closed = Arc::new(AtomicBool::new(false));
        let c = closed.clone();
        let a_closed = arrived.clone();
        let closed_token = item.Closed(&TypedEventHandler::new(move |_, _| {
            c.store(true, Ordering::SeqCst);
            unsafe {
                let _ = SetEvent(a_closed.0);
            }
            Ok(())
        }))?;
        if let Err(e) = session.StartCapture() {
            let _ = item.RemoveClosed(closed_token);
            return Err(e.into());
        }
        Ok(Wgc {
            item,
            pool,
            session,
            arrived,
            closed,
            closed_token,
            width: size.Width as u32,
            height: size.Height as u32,
            fp16,
        })
    }

    /// The newest waiting frame; older ones go back.
    fn newest(&self) -> Option<Direct3D11CaptureFrame> {
        let mut last: Option<Direct3D11CaptureFrame> = None;
        while let Ok(f) = self.pool.TryGetNextFrame() {
            if let Some(old) = last.replace(f) {
                let _ = old.Close();
            }
        }
        last
    }

    /// Waits up to `timeout_ms` for a new frame and passes its texture to `on_frame`.
    fn grab_with(&self, timeout_ms: u32, on_frame: impl FnOnce(&ID3D11Texture2D) -> Result<()>) -> Result<Grab> {
        if self.closed.load(Ordering::SeqCst) {
            return Ok(Grab::Lost);
        }
        let mut f = self.newest();
        if f.is_none() {
            if unsafe { WaitForSingleObject(self.arrived.0, timeout_ms) } != WAIT_OBJECT_0 {
                return Ok(if self.closed.load(Ordering::SeqCst) { Grab::Lost } else { Grab::Timeout });
            }
            if self.closed.load(Ordering::SeqCst) {
                return Ok(Grab::Lost);
            }
            f = self.newest();
        }
        let Some(frame) = f else { return Ok(Grab::Timeout) };
        let at = qpc();
        let s = frame.ContentSize()?;
        let r = if (s.Width as u32, s.Height as u32) != (self.width, self.height) {
            Ok(Grab::Lost)
        } else {
            (|| {
                let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
                let tex: ID3D11Texture2D = unsafe { access.GetInterface()? };
                on_frame(&tex)?;
                Ok(Grab::New { present_qpc: at })
            })()
        };
        let _ = frame.Close();
        r
    }

    /// Waits up to `timeout_ms` for a new frame and copies it into `image`.
    pub fn grab(&self, gpu: &Gpu, image: &ID3D11Texture2D, timeout_ms: u32) -> Result<Grab> {
        self.grab_with(timeout_ms, |tex| {
            unsafe { gpu.ctx.CopyResource(image, tex) };
            Ok(())
        })
    }
}

impl Drop for Wgc {
    fn drop(&mut self) {
        let _ = self.item.RemoveClosed(self.closed_token);
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

// ---- the agent: the console user's process, for the service's helper ----

/// Shared textures in the ring: one being filled, one being read, one spare.
const RING: usize = 3;
/// How long the helper waits for the agent's textures.
const START_WAIT: Duration = Duration::from_secs(5);

/// Agent -> helper, 24 bytes each: kind, slot, two values.
const TEXTURE: u32 = 1; // a: the shared handle in the agent, b: width | height << 32
const FRAME: u32 = 2; // a: when it was captured (QPC)
const RESIZED: u32 = 3; // the display changed size; the agent ends
const FAILED: u32 = 4; // the new target could not be captured; the agent ends

/// How long the agent waits for a frame before looking for a new target.
const COMMAND_POLL_MS: u32 = 30;

#[derive(Clone, Copy)]
struct Msg {
    kind: u32,
    slot: u32,
    a: u64,
    b: u64,
}

impl Msg {
    fn write(&self, out: &mut impl Write) -> std::io::Result<()> {
        let mut v = [0u8; 24];
        v[0..4].copy_from_slice(&self.kind.to_le_bytes());
        v[4..8].copy_from_slice(&self.slot.to_le_bytes());
        v[8..16].copy_from_slice(&self.a.to_le_bytes());
        v[16..24].copy_from_slice(&self.b.to_le_bytes());
        out.write_all(&v)?;
        out.flush()
    }

    fn read(r: &mut impl Read) -> Option<Msg> {
        let mut v = [0u8; 24];
        r.read_exact(&mut v).ok()?;
        Some(Msg {
            kind: u32::from_le_bytes(v[0..4].try_into().unwrap()),
            slot: u32::from_le_bytes(v[4..8].try_into().unwrap()),
            a: u64::from_le_bytes(v[8..16].try_into().unwrap()),
            b: u64::from_le_bytes(v[16..24].try_into().unwrap()),
        })
    }
}

/// Takes the keyed mutex at `key`. Its WAIT_TIMEOUT is a success code: only
/// S_OK means it is ours.
fn acquire(km: &IDXGIKeyedMutex, key: u64, ms: u32) -> bool {
    unsafe { (Interface::vtable(km).AcquireSync)(Interface::as_raw(km), key, ms) == S_OK }
}

/// The display named `device` (`\\.\DISPLAY1`). `gpu` is kept when it is
/// already on that display's adapter, else replaced by a device on it.
fn find_output(device: &str, gpu: &mut Option<Gpu>) -> Result<IDXGIOutput1> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut i = 0;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            i += 1;
            let mut j = 0;
            while let Ok(output) = adapter.EnumOutputs(j) {
                j += 1;
                let output: IDXGIOutput1 = output.cast()?;
                if crate::capture::device_name(&output)? == device {
                    let luid = adapter.GetDesc1()?.AdapterLuid;
                    let same = gpu
                        .as_ref()
                        .is_some_and(|g| g.luid.LowPart == luid.LowPart && g.luid.HighPart == luid.HighPart);
                    if !same {
                        *gpu = Some(Gpu::new(adapter)?);
                    }
                    return Ok(output);
                }
            }
        }
    }
    bail!("no display {device}")
}

/// Starts WGC on `target`: a display (`\\.\DISPLAY1`) or `window:<hwnd>|<display>`.
fn open_target(target: &str, gpu: &mut Option<Gpu>) -> Result<Wgc> {
    if let Some(rest) = target.strip_prefix("window:") {
        let (hwnd_str, disp) = rest.split_once('|').context("invalid window target")?;
        let hwnd_raw: u64 = hwnd_str.parse().context("invalid window hwnd")?;
        find_output(disp, gpu)?;
        Wgc::start_window(gpu.as_ref().expect("set by find_output"), HWND(hwnd_raw as usize as *mut _))
    } else {
        let output = find_output(target, gpu)?;
        let hdr_info = crate::hdr::query_display_hdr(target);
        let monitor = unsafe { output.GetDesc()? }.Monitor;
        Wgc::start(gpu.as_ref().expect("set by find_output"), monitor, hdr_info.hdr_enabled)
    }
}

/// `remote-control capture-agent <target>`, started by the helper as the
/// console user: captures the target into the shared ring and tells the
/// helper about each frame on stdout. A line `open <target>` on stdin moves
/// it to another display or window (new ring textures follow), so switching
/// displays skips starting a process and a graphics device. Ends when the
/// helper closes stdin.
pub fn serve(first: &str) -> Result<()> {
    // stdout carries the frames.
    crate::stats::QUIET.store(true, std::sync::atomic::Ordering::Relaxed);
    let _boost = crate::pipeline::StreamingBoost::enter();
    unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.context("RoInitialize")?;
    let (cmd_tx, cmd_rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(std::io::stdin()).lines().map_while(Result::ok) {
            if let Some(t) = line.strip_prefix("open ")
                && cmd_tx.send(t.trim().to_string()).is_err()
            {
                break;
            }
        }
        std::process::exit(0);
    });
    let mut out = std::io::stdout().lock();
    let mut gpu: Option<Gpu> = None;
    let mut target = first.to_string();
    loop {
        let wgc = match open_target(&target, &mut gpu) {
            Ok(w) => w,
            Err(e) => {
                let _ = Msg { kind: FAILED, slot: 0, a: 0, b: 0 }.write(&mut out);
                return Err(e);
            }
        };
        let gpu = gpu.as_ref().expect("set by open_target");
        match serve_target(gpu, &wgc, &mut out, &cmd_rx)? {
            Some(next) => target = next,
            None => return Ok(()),
        }
    }
}

/// Streams one target until the helper names another (returned) or the
/// capture or the helper is gone (`None`).
fn serve_target(gpu: &Gpu, wgc: &Wgc, out: &mut impl Write, cmds: &mpsc::Receiver<String>) -> Result<Option<String>> {
    let mut ring = Vec::new();
    for slot in 0..RING {
        let misc = (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0) as u32;
        let t = if wgc.fp16 {
            crate::capture::fp16_texture(gpu, wgc.width, wgc.height, misc)?
        } else {
            crate::capture::bgra_texture(gpu, wgc.width, wgc.height, misc)?
        };
        let access = DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0;
        let h = unsafe { t.cast::<IDXGIResource1>()?.CreateSharedHandle(None, access, PCWSTR::null())? };
        let size = wgc.width as u64 | (wgc.height as u64) << 32;
        Msg { kind: TEXTURE, slot: slot as u32, a: h.0 as u64, b: size }.write(out)?;
        let km = t.cast::<IDXGIKeyedMutex>()?;
        ring.push((t, km));
    }
    let mut next = 0;
    loop {
        // The newest target asked for, if any.
        if let Some(t) = cmds.try_iter().last() {
            return Ok(Some(t));
        }
        let mut sent_slot = None;
        let at = match wgc.grab_with(COMMAND_POLL_MS, |tex| {
            // A slot the helper is done with; none: it is behind, skip this frame.
            if let Some(slot) = (0..RING).map(|k| (next + k) % RING).find(|&s| acquire(&ring[s].1, 0, 0)) {
                unsafe {
                    gpu.ctx.CopyResource(&ring[slot].0, tex);
                    let _ = ring[slot].1.ReleaseSync(1);
                }
                sent_slot = Some(slot);
            }
            Ok(())
        })? {
            Grab::New { present_qpc } => present_qpc,
            Grab::Timeout => continue,
            Grab::Lost => {
                let _ = Msg { kind: RESIZED, slot: 0, a: 0, b: 0 }.write(out);
                return Ok(None);
            }
        };
        let Some(slot) = sent_slot else { continue };
        next = (slot + 1) % RING;
        if (Msg { kind: FRAME, slot: slot as u32, a: at as u64, b: 0 }).write(out).is_err() {
            return Ok(None);
        }
    }
}

/// The helper's end: the agent process and the textures it fills.
pub struct Agent {
    /// Its job is killed when dropped; closing its stdin ends it too.
    exec: term_proc::Exec,
    rx: mpsc::Receiver<Msg>,
    ring: Vec<(ID3D11Texture2D, IDXGIKeyedMutex)>,
    /// The slot consumed last (see `again_with`).
    last: std::cell::Cell<Option<usize>>,
    pub width: u32,
    pub height: u32,
    pub fp16: bool,
}

impl Agent {
    /// The agent on `target` (see `serve`): `prev` moved there when given and
    /// still running, else a new one.
    pub fn open(gpu: &Gpu, target: &str, prev: Option<Agent>) -> Result<Agent> {
        if let Some(p) = prev {
            match p.retarget(gpu, target) {
                Ok(a) => return Ok(a),
                Err(e) => log!("capture agent: {e:#}; starting a new one"),
            }
        }
        Self::start(gpu, target)
    }

    /// Moves the running agent to `target`: no new process, and its graphics
    /// device stays when the display is on the same adapter.
    fn retarget(mut self, gpu: &Gpu, target: &str) -> Result<Agent> {
        let t0 = Instant::now();
        (&self.exec.stdin)
            .write_all(format!("open {target}\n").as_bytes())
            .and_then(|()| (&self.exec.stdin).flush())
            .context("the capture agent is gone")?;
        self.ring.clear();
        self.last.set(None);
        self.receive_ring(gpu, true)?;
        log!(
            "capture agent moved to {target} in {} ms{}",
            t0.elapsed().as_millis(),
            if self.fp16 { " (HDR FP16)" } else { "" }
        );
        Ok(self)
    }

    /// Starts the agent as the console user on `target` and opens its
    /// textures on `gpu` (the adapter that drives that display).
    pub fn start(gpu: &Gpu, target: &str) -> Result<Agent> {
        let t0 = Instant::now();
        let exec = term_proc::self_piped(&format!("capture-agent \"{target}\""))?;
        let err = exec.stderr.try_clone()?;
        std::thread::Builder::new().name("capture-agent-err".into()).spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                log!("capture agent: {line}");
            }
        })?;
        let (tx, rx) = mpsc::channel();
        let mut out = exec.stdout.try_clone()?;
        std::thread::Builder::new().name("capture-agent".into()).spawn(move || {
            while let Some(m) = Msg::read(&mut out) {
                if tx.send(m).is_err() {
                    return;
                }
            }
        })?;
        let mut a = Agent { exec, rx, ring: Vec::new(), last: std::cell::Cell::new(None), width: 0, height: 0, fp16: false };
        a.receive_ring(gpu, false)?;
        log!("capture agent ready in {} ms{}", t0.elapsed().as_millis(), if a.fp16 { " (HDR FP16)" } else { "" });
        Ok(a)
    }

    /// Opens the agent's new ring textures. `skip_old`: frames and the end of
    /// the last target may come first.
    fn receive_ring(&mut self, gpu: &Gpu, skip_old: bool) -> Result<()> {
        let dev1: ID3D11Device1 = gpu.device.cast()?;
        let mut ring = Vec::new();
        let (mut width, mut height) = (0, 0);
        while ring.len() < RING {
            let m = match self.rx.recv_timeout(START_WAIT) {
                Ok(m) => m,
                Err(RecvTimeoutError::Timeout) => bail!("the capture agent did not start within {} s", START_WAIT.as_secs()),
                Err(RecvTimeoutError::Disconnected) => bail!("the capture agent ended"),
            };
            if m.kind == FAILED {
                bail!("the capture agent could not capture the target");
            }
            if m.kind != TEXTURE {
                if skip_old && ring.is_empty() {
                    continue;
                }
                bail!("the capture agent sent {} before its textures", m.kind);
            }
            if m.slot as usize != ring.len() {
                bail!("the capture agent sent texture {} out of order", m.slot);
            }
            let tex: ID3D11Texture2D = unsafe {
                let mut mine = HANDLE::default();
                let theirs = HANDLE(m.a as usize as *mut _);
                DuplicateHandle(self.exec.proc.handle(), theirs, GetCurrentProcess(), &mut mine, 0, false, DUPLICATE_SAME_ACCESS)
                    .context("DuplicateHandle")?;
                let t = dev1.OpenSharedResource1(mine);
                let _ = CloseHandle(mine);
                t.context("OpenSharedResource1")?
            };
            (width, height) = (m.b as u32, (m.b >> 32) as u32);
            let km = tex.cast()?;
            ring.push((tex, km));
        }
        let mut fp16 = false;
        if let Some((first_tex, _)) = ring.first() {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { first_tex.GetDesc(&mut desc) };
            fp16 = desc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT;
        }
        self.ring = ring;
        (self.width, self.height, self.fp16) = (width, height, fp16);
        Ok(())
    }

    /// Returns the shared ring textures opened on the helper's D3D11 device so
    /// the pipeline can run shaders directly from them without an extra copy.
    pub fn ring_textures(&self) -> Vec<ID3D11Texture2D> {
        self.ring.iter().map(|(t, _)| t.clone()).collect()
    }

    /// Waits up to `timeout_ms` for frames and invokes `on_slot` with the newest
    /// ring slot while holding its keyed mutex (key 1). Fails once the agent is gone.
    pub fn grab_with(&self, timeout_ms: u32, mut on_slot: impl FnMut(usize)) -> Result<Grab> {
        let first = match self.rx.recv_timeout(Duration::from_millis(timeout_ms as u64)) {
            Ok(m) => m,
            Err(RecvTimeoutError::Timeout) => return Ok(Grab::Timeout),
            Err(RecvTimeoutError::Disconnected) => return Err(anyhow!("the capture agent ended")),
        };
        let msgs: Vec<Msg> = std::iter::once(first).chain(self.rx.try_iter()).collect();
        if msgs.iter().any(|m| m.kind == RESIZED) {
            return Ok(Grab::Lost);
        }
        let frames: Vec<&Msg> = msgs.iter().filter(|m| m.kind == FRAME).collect();
        let mut got = Grab::Timeout;
        // Every slot goes back to the agent; only the newest is consumed.
        for (n, m) in frames.iter().enumerate() {
            let slot = m.slot as usize;
            let Some((_, km)) = self.ring.get(slot) else { continue };
            if !acquire(km, 1, 100) {
                continue;
            }
            if n + 1 == frames.len() {
                on_slot(slot);
                got = Grab::New { present_qpc: m.a as i64 };
                self.last.set(Some(slot));
            }
            unsafe {
                let _ = km.ReleaseSync(0);
            }
        }
        Ok(got)
    }

    /// Invokes `on_slot` again with the slot `grab_with` consumed last, if
    /// the agent has not taken it back since (then a newer frame is on its
    /// way). The agent only writes to a slot it holds with key 0, so holding
    /// it with key 0 here keeps the image as it is.
    pub fn again_with(&self, on_slot: impl FnOnce(usize)) -> bool {
        let Some(slot) = self.last.get() else { return false };
        let Some((_, km)) = self.ring.get(slot) else { return false };
        if !acquire(km, 0, 0) {
            return false;
        }
        on_slot(slot);
        unsafe {
            let _ = km.ReleaseSync(0);
        }
        true
    }

    /// Waits up to `timeout_ms` for frames and copies the newest into
    /// `image`. Fails once the agent is gone.
    pub fn grab(&self, gpu: &Gpu, image: &ID3D11Texture2D, timeout_ms: u32) -> Result<Grab> {
        self.grab_with(timeout_ms, |slot| unsafe {
            gpu.ctx.CopyResource(image, &self.ring[slot].0);
        })
    }
}
