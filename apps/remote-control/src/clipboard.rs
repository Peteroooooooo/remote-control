//! The local clipboard, on its own thread with a hidden message-only window:
//! reports text and image changes (AddClipboardFormatListener) and sets what
//! the other side copied. Kept apart from the network side (clipsync.rs)
//! because a host running as SYSTEM in the user's session (P1-4) has to run
//! this thread on the user's desktop.
//!
//! Text is CF_UNICODETEXT, sent as UTF-8. Images are read from CF_DIBV5 /
//! CF_DIB (24 or 32 bits per pixel), sent as PNG, and set as a 32-bit CF_DIB
//! (Windows makes CF_DIBV5 and CF_BITMAP from it). When both are on the
//! clipboard only the text is sent: apps such as Word put a picture of the
//! copied text next to it. Copied files and folders (CF_HDROP, what Explorer
//! puts there) come before both; the other side's are set as an OLE data
//! object of virtual files (`clipfiles`).

use std::{
    cell::RefCell,
    hash::{DefaultHasher, Hash, Hasher},
    path::PathBuf,
    sync::{Arc, mpsc},
    thread::JoinHandle,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::mpsc::UnboundedSender;
use windows::{
    Win32::{
        Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, S_OK, WPARAM},
        System::{
            DataExchange::{
                AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardOwner,
                IsClipboardFormatAvailable, OpenClipboard, RemoveClipboardFormatListener, SetClipboardData,
            },
            LibraryLoader::GetModuleHandleW,
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock},
            Com::IDataObject,
            Ole::{CF_DIB, CF_DIBV5, CF_HDROP, CF_UNICODETEXT, OleInitialize, OleSetClipboard, OleUninitialize},
        },
        UI::WindowsAndMessaging::*,
    },
    core::{Interface, w},
};

use crate::{
    clipfiles::{self, Entry, Source},
    log,
};

/// Largest text sent, in UTF-8 bytes.
pub const MAX_TEXT: usize = 16 << 20;
/// Largest image sent: 40 megapixels (an 8K screenshot is 33).
pub const MAX_PIXELS: u64 = 40_000_000;
/// Largest transfer accepted, after PNG compression.
pub const MAX_TRANSFER: usize = 64 << 20;

pub const FORMAT_TEXT: u8 = 1;
pub const FORMAT_PNG: u8 = 2;
/// A list of files and folders (`clipfiles::encode_list`); their data comes when pasted.
pub const FORMAT_FILES: u8 = 3;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// RGBA rows, top-down, straight alpha.
    pub rgba: Vec<u8>,
}

/// The other side's copied files, to paste here.
#[derive(Clone)]
pub struct Remote {
    pub src: Arc<Source>,
    pub entries: Arc<Vec<Entry>>,
}

impl std::fmt::Debug for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Remote(copy {}, {})", self.src.seq, clipfiles::describe(&self.entries))
    }
}

impl PartialEq for Remote {
    fn eq(&self, o: &Remote) -> bool {
        Arc::ptr_eq(&self.src, &o.src)
    }
}

impl Eq for Remote {}

impl Hash for Remote {
    fn hash<H: Hasher>(&self, h: &mut H) {
        self.src.seq.hash(h);
        self.entries.hash(h);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Content {
    Text(String),
    Image(Image),
    /// Files and folders copied here (their paths).
    Files(Vec<PathBuf>),
    Remote(Remote),
}

impl Content {
    pub fn describe(&self) -> String {
        match self {
            Content::Text(t) => format!("text ({} bytes)", t.len()),
            Content::Image(i) => format!("image {}x{}", i.width, i.height),
            Content::Files(p) => format!("{} copied files or folders", p.len()),
            Content::Remote(r) => clipfiles::describe(&r.entries),
        }
    }

    fn digest(&self) -> u64 {
        let mut h = DefaultHasher::new();
        self.hash(&mut h);
        h.finish()
    }

    /// Format byte and bytes on the wire (files are listed by `clipsync`).
    pub fn encode(&self) -> Result<(u8, Vec<u8>)> {
        Ok(match self {
            Content::Text(t) => (FORMAT_TEXT, t.as_bytes().to_vec()),
            Content::Image(i) => (FORMAT_PNG, png_encode(i)?),
            Content::Files(_) | Content::Remote(_) => bail!("files are not sent as clipboard data"),
        })
    }

    pub fn decode(format: u8, data: &[u8]) -> Result<Content> {
        Ok(match format {
            FORMAT_TEXT => {
                if data.len() > MAX_TEXT {
                    bail!("text of {} bytes is too long", data.len());
                }
                Content::Text(String::from_utf8_lossy(data).into_owned())
            }
            FORMAT_PNG => Content::Image(png_decode(data)?),
            f => bail!("unknown clipboard format {f}"),
        })
    }
}

fn check_size(width: u64, height: u64) -> Result<()> {
    if width == 0 || height == 0 || width * height > MAX_PIXELS {
        bail!("image {width}x{height} is empty or larger than {MAX_PIXELS} pixels");
    }
    Ok(())
}

pub fn png_encode(img: &Image) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, img.width, img.height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    enc.write_header()?.write_image_data(&img.rgba)?;
    Ok(out)
}

pub fn png_decode(data: &[u8]) -> Result<Image> {
    let mut dec = png::Decoder::new_with_limits(data, png::Limits { bytes: (MAX_PIXELS * 4) as usize });
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info()?;
    let (w, h) = (reader.info().width, reader.info().height);
    check_size(w as u64, h as u64)?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buf)?;
    let px = &buf[..frame.buffer_size()];
    let rgba = match frame.color_type {
        png::ColorType::Rgba => px.to_vec(),
        png::ColorType::Rgb => px.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => px.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Grayscale => px.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        c => bail!("PNG colour type {c:?} after expansion"),
    };
    Ok(Image { width: w, height: h, rgba })
}

fn u32_at(b: &[u8], i: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(b.get(i..i + 4).context("DIB too short")?.try_into().expect("4 bytes")))
}

/// A packed DIB (BITMAPINFOHEADER, V4 or V5, then the pixels) to RGBA.
/// 24 and 32 bits per pixel; 32-bit images with no alpha at all are opaque.
pub fn dib_to_image(dib: &[u8]) -> Result<Image> {
    let size = u32_at(dib, 0)? as usize;
    if size < 40 {
        bail!("unknown DIB header of {size} bytes");
    }
    let width = u32_at(dib, 4)? as i32;
    let height = u32_at(dib, 8)? as i32;
    let bits = u16::from_le_bytes([dib[14], dib[15]]);
    let compression = u32_at(dib, 16)?;
    let colors_used = u32_at(dib, 32)? as usize;
    if width <= 0 || height == 0 {
        bail!("DIB {width}x{height}");
    }
    let (w, h) = (width as usize, height.unsigned_abs() as usize);
    check_size(w as u64, h as u64)?;
    // BI_RGB = 0, BI_BITFIELDS = 3. A 40-byte header with BI_BITFIELDS has
    // its three masks after it; V4 and V5 headers hold them.
    let masks = match (compression, bits) {
        (0, _) => [0x00FF_0000, 0x0000_FF00, 0x0000_00FF],
        (3, 32) => [u32_at(dib, 40)?, u32_at(dib, 44)?, u32_at(dib, 48)?],
        (c, b) => bail!("DIB with {b} bits per pixel and compression {c} is not supported"),
    };
    if masks != [0x00FF_0000, 0x0000_FF00, 0x0000_00FF] {
        bail!("DIB colour masks {masks:08x?} are not supported");
    }
    if bits != 24 && bits != 32 {
        bail!("DIB with {bits} bits per pixel is not supported");
    }
    let extra = if compression == 3 && size == 40 { 12 } else { 0 };
    let start = size + extra + colors_used * 4;
    let stride = (w * bits as usize).div_ceil(32) * 4;
    let pixels = dib.get(start..start + stride * h).context("DIB shorter than its pixels")?;
    let bpp = bits as usize / 8;
    let mut rgba = Vec::with_capacity(w * h * 4);
    let mut any_alpha = false;
    for y in 0..h {
        let src = if height > 0 { h - 1 - y } else { y };
        let row = &pixels[src * stride..src * stride + w * bpp];
        for p in row.chunks_exact(bpp) {
            let a = if bpp == 4 { p[3] } else { 255 };
            any_alpha |= bpp == 4 && a != 0;
            rgba.extend_from_slice(&[p[2], p[1], p[0], a]);
        }
    }
    if bpp == 4 && !any_alpha {
        for p in rgba.chunks_exact_mut(4) {
            p[3] = 255;
        }
    }
    Ok(Image { width: w as u32, height: h as u32, rgba })
}

/// RGBA to a packed 32-bit bottom-up DIB for CF_DIB.
pub fn image_to_dib(img: &Image) -> Vec<u8> {
    let (w, h) = (img.width as usize, img.height as usize);
    let mut out = Vec::with_capacity(40 + w * h * 4);
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&((w * h * 4) as u32).to_le_bytes());
    out.extend_from_slice(&[0u8; 16]);
    for y in (0..h).rev() {
        for p in img.rgba[y * w * 4..(y + 1) * w * 4].chunks_exact(4) {
            out.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
    }
    out
}

static SERVICE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
const WM_APP_SET: u32 = WM_APP + 1;
const DEBOUNCE_TIMER: usize = 1;
/// Apps often update the clipboard in several steps; read once it settles.
const DEBOUNCE: Duration = Duration::from_millis(150);
/// Reads again when nothing known was on the clipboard (see `on_settled`).
const RETRIES: u8 = 5;

struct ThreadState {
    changed: UnboundedSender<Content>,
    incoming: mpsc::Receiver<Content>,
    /// What both sides have: last sent or last set. Not sent again.
    synced: Option<u64>,
    /// The other side's files, as last put on the clipboard here.
    files: Option<IDataObject>,
    /// Reads since the last update that found nothing known.
    retries: u8,
}

/// Whether the clipboard still holds the other side's files set here.
fn holds_ours(ts: &ThreadState) -> bool {
    // S_FALSE when it is not: the windows crate's wrapper calls that success.
    windows_core::link!("ole32.dll" "system" fn OleIsCurrentClipboard(pdataobj: *mut core::ffi::c_void) -> windows_core::HRESULT);
    ts.files.as_ref().is_some_and(|d| unsafe { OleIsCurrentClipboard(d.as_raw()) } == S_OK)
}

thread_local! {
    static TS: RefCell<Option<ThreadState>> = const { RefCell::new(None) };
}

/// The clipboard thread. Dropping it stops the thread.
pub struct Clipboard {
    hwnd: isize,
    set_tx: mpsc::Sender<Content>,
    thread: Option<JoinHandle<()>>,
}

impl Clipboard {
    /// Local changes arrive on `changed`.
    pub fn start(changed: UnboundedSender<Content>, service: bool) -> Result<Clipboard> {
        SERVICE.store(service, std::sync::atomic::Ordering::Relaxed);
        let (set_tx, set_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<isize>>();
        let thread = std::thread::Builder::new().name("clipboard".into()).spawn(move || {
            // The OLE clipboard (the other side's files) needs a single-threaded apartment.
            let ole = unsafe { OleInitialize(None) };
            if let Err(e) = &ole {
                log!("clipboard: OleInitialize: {e}; files cannot be pasted here");
            }
            let hwnd = match create_window() {
                Ok(h) => h,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            TS.set(Some(ThreadState { changed, incoming: set_rx, synced: None, files: None, retries: 0 }));
            let _ = ready_tx.send(Ok(hwnd.0 as isize));
            unsafe {
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    DispatchMessageW(&msg);
                }
                if ole.is_ok() {
                    OleUninitialize();
                }
            }
        })?;
        let hwnd = ready_rx.recv().map_err(|_| anyhow!("clipboard thread ended"))??;
        Ok(Clipboard { hwnd, set_tx, thread: Some(thread) })
    }

    /// Puts what the other side copied on the local clipboard.
    pub fn set(&self, c: Content) {
        if self.set_tx.send(c).is_ok() {
            unsafe {
                let _ = PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_APP_SET, WPARAM(0), LPARAM(0));
            }
        }
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        unsafe {
            let _ = PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        if let Some(t) = self.thread.take() {
            if tokio::runtime::Handle::try_current().is_ok() {
                let _ = std::thread::Builder::new().name("clipboard-join".into()).spawn(move || {
                    let _ = t.join();
                });
            } else {
                let _ = t.join();
            }
        }
    }
}

fn create_window() -> Result<HWND> {
    unsafe {
        let inst = GetModuleHandleW(None)?;
        let class = w!("remote-control-clipboard");
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: inst.into(), lpszClassName: class, ..Default::default() };
        // Fails harmlessly when an earlier session registered it.
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            w!(""),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(inst.into()),
            None,
        )?;
        AddClipboardFormatListener(hwnd)?;
        Ok(hwnd)
    }
}

/// The clipboard is shared by every program; another one may hold it for a moment.
fn open(hwnd: HWND) -> Result<()> {
    for _ in 0..20 {
        if unsafe { OpenClipboard(Some(hwnd)) }.is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    bail!("the clipboard stayed busy")
}

/// Bytes of a clipboard handle, while the clipboard is open.
fn global_bytes(h: HANDLE) -> Option<Vec<u8>> {
    let g = HGLOBAL(h.0);
    unsafe {
        let p = GlobalLock(g) as *const u8;
        if p.is_null() {
            return None;
        }
        let v = std::slice::from_raw_parts(p, GlobalSize(g)).to_vec();
        let _ = GlobalUnlock(g);
        Some(v)
    }
}

fn read(hwnd: HWND) -> Result<Option<Content>> {
    open(hwnd)?;
    let r = (|| unsafe {
        if IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_ok() {
            let b = GetClipboardData(CF_HDROP.0 as u32).ok().and_then(global_bytes).context("no file list")?;
            let paths = clipfiles::dropfiles_paths(&b);
            if !paths.is_empty() {
                return Ok(Some(Content::Files(paths)));
            }
            log!("clipboard: an empty or unreadable list of copied files ({} bytes)", b.len());
        }
        if IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).is_ok() {
            let b = GetClipboardData(CF_UNICODETEXT.0 as u32).ok().and_then(global_bytes).context("no text data")?;
            let units: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
            let text = String::from_utf16_lossy(&units);
            if text.len() > MAX_TEXT {
                bail!("text of {} bytes is too long to send", text.len());
            }
            return Ok(Some(Content::Text(text)));
        }
        for f in [CF_DIBV5, CF_DIB] {
            if IsClipboardFormatAvailable(f.0 as u32).is_ok()
                && let Some(b) = GetClipboardData(f.0 as u32).ok().and_then(global_bytes)
            {
                return Ok(Some(Content::Image(dib_to_image(&b)?)));
            }
        }
        Ok(None)
    })();
    unsafe {
        let _ = CloseClipboard();
    }
    r
}

/// Copied files on the clipboard, one path per line on stdout (`remote-control
/// clip-files`). Run as the user by a host running as SYSTEM, which Windows
/// does not show the files that programs copy through OLE (Explorer, for one).
pub fn print_copied_files() -> Result<()> {
    open(HWND::default())?;
    let paths = unsafe {
        let b = IsClipboardFormatAvailable(CF_HDROP.0 as u32).ok().and_then(|_| GetClipboardData(CF_HDROP.0 as u32).ok()).and_then(global_bytes);
        let _ = CloseClipboard();
        b.map(|b| clipfiles::dropfiles_paths(&b)).unwrap_or_default()
    };
    let out: String = paths.iter().map(|p| format!("{}\n", p.display())).collect();
    std::io::Write::write_all(&mut std::io::stdout(), out.as_bytes())?;
    Ok(())
}

/// Files the user copied, for a host running as SYSTEM: Windows does not show
/// it what programs copy through OLE, nor tell it which formats those are.
fn user_files() -> Option<Vec<PathBuf>> {
    match crate::term_proc::self_as_user("clip-files", Duration::from_secs(3)) {
        Ok(out) => {
            let paths: Vec<PathBuf> = String::from_utf8_lossy(&out).lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect();
            (!paths.is_empty()).then_some(paths)
        }
        Err(e) => {
            log!("clipboard: could not look for copied files as the user: {e:#}");
            None
        }
    }
}

fn write(hwnd: HWND, c: &Content) -> Result<()> {
    let (format, bytes) = match c {
        Content::Text(t) => {
            let mut b: Vec<u8> = t.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
            b.shrink_to_fit();
            (CF_UNICODETEXT.0 as u32, b)
        }
        Content::Image(i) => (CF_DIB.0 as u32, image_to_dib(i)),
        Content::Remote(r) => {
            let obj = clipfiles::data_object(r.src.clone(), r.entries.clone());
            let mut tries = 0;
            while let Err(e) = unsafe { OleSetClipboard(&obj) } {
                tries += 1;
                if tries == 20 {
                    bail!("OleSetClipboard: {e}");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            TS.with_borrow_mut(|ts| {
                if let Some(ts) = ts {
                    ts.files = Some(obj);
                }
            });
            return Ok(());
        }
        Content::Files(_) => bail!("files copied here are not set here"),
    };
    open(hwnd)?;
    let r = unsafe {
        (|| {
            EmptyClipboard()?;
            let g = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
            let p = GlobalLock(g) as *mut u8;
            if p.is_null() {
                let _ = GlobalFree(Some(g));
                bail!("GlobalLock failed");
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
            let _ = GlobalUnlock(g);
            if let Err(e) = SetClipboardData(format, Some(HANDLE(g.0))) {
                let _ = GlobalFree(Some(g));
                return Err(e.into());
            }
            Ok(())
        })()
    };
    unsafe {
        let _ = CloseClipboard();
    }
    r
}

fn on_settled(hwnd: HWND) {
    // Our own write: the other side has it already.
    if unsafe { GetClipboardOwner() }.is_ok_and(|o| o == hwnd) || TS.with_borrow(|ts| ts.as_ref().is_some_and(holds_ours)) {
        return;
    }
    let user = SERVICE.load(std::sync::atomic::Ordering::Relaxed).then(user_files).flatten();
    let content = match user.map(|p| Ok(Some(Content::Files(p)))).unwrap_or_else(|| read(hwnd)) {
        Ok(Some(c)) => c,
        Ok(None) => {
            // Programs that copy through OLE (PowerShell's Set-Clipboard, for
            // one) list the real formats a moment after the first update, and
            // a host running as SYSTEM is not told again: look once more.
            let again = TS.with_borrow_mut(|ts| {
                let ts = ts.as_mut()?;
                ts.retries += 1;
                (ts.retries <= RETRIES).then_some(ts.retries)
            });
            if let Some(n) = again {
                unsafe { SetTimer(Some(hwnd), DEBOUNCE_TIMER, (DEBOUNCE.as_millis() as u32) << n, None) };
            }
            return;
        }
        Err(e) => return log!("clipboard: not sent: {e:#}"),
    };
    let digest = content.digest();
    TS.with_borrow_mut(|ts| {
        let Some(ts) = ts else { return };
        // Files are listed again on each copy: they may have changed.
        if ts.synced == Some(digest) && !matches!(content, Content::Files(_)) {
            return;
        }
        ts.synced = Some(digest);
        let _ = ts.changed.send(content);
    });
}

fn on_set(hwnd: HWND) {
    let Some(c) = TS.with_borrow(|ts| ts.as_ref().and_then(|ts| ts.incoming.try_iter().last())) else { return };
    match write(hwnd, &c) {
        Ok(()) => {
            TS.with_borrow_mut(|ts| {
                if let Some(ts) = ts {
                    ts.synced = Some(c.digest());
                }
            });
            log!("clipboard: set {}", c.describe());
        }
        Err(e) => log!("clipboard: could not set {}: {e:#}", c.describe()),
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CLIPBOARDUPDATE => unsafe {
            TS.with_borrow_mut(|ts| {
                if let Some(ts) = ts {
                    ts.retries = 0;
                }
            });
            SetTimer(Some(hwnd), DEBOUNCE_TIMER, DEBOUNCE.as_millis() as u32, None);
        },
        WM_TIMER if wparam.0 == DEBOUNCE_TIMER => {
            unsafe {
                let _ = KillTimer(Some(hwnd), DEBOUNCE_TIMER);
            }
            on_settled(hwnd);
        }
        WM_APP_SET => on_set(hwnd),
        WM_DESTROY => unsafe {
            let _ = RemoveClipboardFormatListener(hwnd);
            // The other side's files cannot be pasted once the session is over.
            if TS.with_borrow(|ts| ts.as_ref().is_some_and(holds_ours)) {
                let _ = OleSetClipboard(None);
            }
            TS.set(None);
            PostQuitMessage(0);
        },
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
    LRESULT(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(w: u32, h: u32) -> Image {
        let rgba = (0..w * h).flat_map(|i| [(i % 251) as u8, (i / 7 % 256) as u8, 200, (i % 3 * 100) as u8]).collect();
        Image { width: w, height: h, rgba }
    }

    #[test]
    fn png_round_trip() {
        let img = sample(37, 21);
        let png = png_encode(&img).unwrap();
        assert_eq!(png_decode(&png).unwrap(), img);
        let c = Content::Image(img.clone());
        let (f, b) = c.encode().unwrap();
        assert_eq!(Content::decode(f, &b).unwrap(), c);
    }

    #[test]
    fn text_round_trip() {
        let c = Content::Text("剪贴板 clipboard ✓".into());
        let (f, b) = c.encode().unwrap();
        assert_eq!(f, FORMAT_TEXT);
        assert_eq!(Content::decode(f, &b).unwrap(), c);
        assert!(Content::decode(9, &b).is_err());
    }

    #[test]
    fn dib_round_trip_and_orientation() {
        let img = sample(5, 3);
        let dib = image_to_dib(&img);
        assert_eq!(dib.len(), 40 + 5 * 3 * 4);
        assert_eq!(dib_to_image(&dib).unwrap(), img);
        // Bottom-up: the first stored row is the image's last.
        assert_eq!(&dib[40..44], &[img.rgba[2 * 20 + 2], img.rgba[2 * 20 + 1], img.rgba[2 * 20], img.rgba[2 * 20 + 3]]);
    }

    #[test]
    fn dib_24_bit_top_down_and_no_alpha() {
        // 2x2, 24 bits: rows padded to 8 bytes; negative height = top-down.
        let mut dib = image_to_dib(&sample(1, 1))[..40].to_vec();
        dib[4..8].copy_from_slice(&2i32.to_le_bytes());
        dib[8..12].copy_from_slice(&(-2i32).to_le_bytes());
        dib[14..16].copy_from_slice(&24u16.to_le_bytes());
        dib.extend_from_slice(&[1, 2, 3, 4, 5, 6, 0, 0, 7, 8, 9, 10, 11, 12, 0, 0]);
        let img = dib_to_image(&dib).unwrap();
        assert_eq!(img.rgba, vec![3, 2, 1, 255, 6, 5, 4, 255, 9, 8, 7, 255, 12, 11, 10, 255]);
        // 32 bits with every alpha byte 0: opaque.
        let mut flat = sample(2, 1);
        for p in flat.rgba.chunks_exact_mut(4) {
            p[3] = 0;
        }
        let img = dib_to_image(&image_to_dib(&flat)).unwrap();
        assert!(img.rgba.chunks_exact(4).all(|p| p[3] == 255));
    }

    #[test]
    fn dib_bitfields_and_refusals() {
        let img = sample(2, 2);
        let plain = image_to_dib(&img);
        let mut bf = plain[..40].to_vec();
        bf[16..20].copy_from_slice(&3u32.to_le_bytes());
        for m in [0x00FF_0000u32, 0x0000_FF00, 0x0000_00FF] {
            bf.extend_from_slice(&m.to_le_bytes());
        }
        bf.extend_from_slice(&plain[40..]);
        assert_eq!(dib_to_image(&bf).unwrap(), img);
        let mut odd = bf.clone();
        odd[40..44].copy_from_slice(&0x0000_00FFu32.to_le_bytes());
        assert!(dib_to_image(&odd).is_err());
        let mut eight = plain.clone();
        eight[14..16].copy_from_slice(&8u16.to_le_bytes());
        assert!(dib_to_image(&eight).is_err());
        assert!(dib_to_image(&plain[..50]).is_err());
        let mut huge = plain.clone();
        huge[4..8].copy_from_slice(&100_000i32.to_le_bytes());
        huge[8..12].copy_from_slice(&100_000i32.to_le_bytes());
        assert!(dib_to_image(&huge).is_err());
    }
}
