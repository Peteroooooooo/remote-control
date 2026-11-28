//! C ABI for the iOS / iPadOS controller app (`apps/rc-ios`).
//!
//! It mirrors the JNI shim in `rc-android`: one engine per process, every call is a no-op until
//! [`rc_init`] succeeded, and callbacks arrive on engine threads (never the main thread).
//! The matching header is `apps/rc-ios/RcCore/include/rc_core.h`; keep the two in sync.
//!
//! Strings passed in are NUL-terminated UTF-8 and only borrowed for the call. Strings and byte
//! buffers passed to callbacks are only valid during the callback; copy what you keep.
//! Strings returned by this library must be freed with [`rc_string_free`].

use std::{
    ffi::{CStr, CString, c_char, c_void},
    net::Ipv4Addr,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
};

use rc_android::{
    engine::{Engine, PasteResult},
    session::Callback,
};

/// Callback table filled in by the app. `ctx` is handed back unchanged to every function.
/// All function pointers must be non-null.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RcCallbacks {
    pub ctx: *mut c_void,
    pub on_devices_updated: extern "C" fn(ctx: *mut c_void, devices_json: *const c_char),
    pub on_session_state: extern "C" fn(ctx: *mut c_void, state: *const c_char, message: *const c_char),
    pub on_stream_info: extern "C" fn(
        ctx: *mut c_void,
        width: i32,
        height: i32,
        encoder: *const c_char,
        fps: i32,
        max_kbps: i32,
    ),
    pub on_monitors: extern "C" fn(ctx: *mut c_void, monitors_json: *const c_char, current_index: i32),
    pub on_windows: extern "C" fn(ctx: *mut c_void, windows_json: *const c_char),
    pub on_app_icon: extern "C" fn(ctx: *mut c_void, app_key: *const c_char, png: *const u8, len: usize),
    pub on_window_thumb: extern "C" fn(ctx: *mut c_void, hwnd: u64, png: *const u8, len: usize),
    pub on_stats: extern "C" fn(ctx: *mut c_void, fps: i32, kbps: i32, rtt_ms: i32, path_kind: *const c_char),
    pub on_hint: extern "C" fn(ctx: *mut c_void, code: i32, message: *const c_char),
    pub on_notice: extern "C" fn(ctx: *mut c_void, message: *const c_char),
    /// `rgba` is premultiplied RGBA8888, `width * height * 4` bytes.
    pub on_cursor_shape: extern "C" fn(
        ctx: *mut c_void,
        id: i32,
        width: i32,
        height: i32,
        hot_x: i32,
        hot_y: i32,
        rgba: *const u8,
        len: usize,
    ),
    pub on_cursor_use: extern "C" fn(ctx: *mut c_void, id: i32),
    pub on_cursor_hidden: extern "C" fn(ctx: *mut c_void),
    pub on_cursor_meta: extern "C" fn(ctx: *mut c_void, kind: i32),
    pub on_text_focus: extern "C" fn(ctx: *mut c_void, editable: bool),
    pub on_virtual_display: extern "C" fn(ctx: *mut c_void, active: bool, width: i32, height: i32, dpi_percent: i32),
    pub on_route_changed: extern "C" fn(ctx: *mut c_void, relay_only: bool),
    /// `codec`: 0 = H.264, 1 = HEVC. `data` is one Annex B access unit.
    pub on_video_frame: extern "C" fn(
        ctx: *mut c_void,
        codec: i32,
        key: bool,
        width: i32,
        height: i32,
        host_us: i64,
        data: *const u8,
        len: usize,
    ),
}

struct CCallback(RcCallbacks);

// SAFETY: the app promises `ctx` may be used from any thread (it points at a thread-safe
// object that lives for the rest of the process).
unsafe impl Send for CCallback {}
unsafe impl Sync for CCallback {}

/// A C string for a callback argument; interior NULs (never expected) become an empty string.
fn cs(s: &str) -> CString {
    CString::new(s).unwrap_or_default()
}

impl Callback for CCallback {
    fn on_devices_updated(&self, devices_json: &str) {
        let j = cs(devices_json);
        (self.0.on_devices_updated)(self.0.ctx, j.as_ptr());
    }

    fn on_session_state(&self, state: &str, message: &str) {
        let (s, m) = (cs(state), cs(message));
        (self.0.on_session_state)(self.0.ctx, s.as_ptr(), m.as_ptr());
    }

    fn on_stream_info(&self, width: i32, height: i32, encoder: &str, fps: i32, max_kbps: i32) {
        let e = cs(encoder);
        (self.0.on_stream_info)(self.0.ctx, width, height, e.as_ptr(), fps, max_kbps);
    }

    fn on_monitors(&self, monitors_json: &str, current_index: i32) {
        let j = cs(monitors_json);
        (self.0.on_monitors)(self.0.ctx, j.as_ptr(), current_index);
    }

    fn on_windows(&self, windows_json: &str) {
        let j = cs(windows_json);
        (self.0.on_windows)(self.0.ctx, j.as_ptr());
    }

    fn on_app_icon(&self, app_key: &str, png: &[u8]) {
        let k = cs(app_key);
        (self.0.on_app_icon)(self.0.ctx, k.as_ptr(), png.as_ptr(), png.len());
    }

    fn on_window_thumb(&self, hwnd: u64, png: &[u8]) {
        (self.0.on_window_thumb)(self.0.ctx, hwnd, png.as_ptr(), png.len());
    }

    fn on_stats(&self, fps: i32, kbps: i32, rtt_ms: i32, path_kind: &str) {
        let p = cs(path_kind);
        (self.0.on_stats)(self.0.ctx, fps, kbps, rtt_ms, p.as_ptr());
    }

    fn on_hint(&self, code: i32, message: &str) {
        let m = cs(message);
        (self.0.on_hint)(self.0.ctx, code, m.as_ptr());
    }

    fn on_notice(&self, message: &str) {
        let m = cs(message);
        (self.0.on_notice)(self.0.ctx, m.as_ptr());
    }

    fn on_cursor_shape(&self, id: i32, width: i32, height: i32, hot_x: i32, hot_y: i32, rgba: &[u8]) {
        (self.0.on_cursor_shape)(self.0.ctx, id, width, height, hot_x, hot_y, rgba.as_ptr(), rgba.len());
    }

    fn on_cursor_use(&self, id: i32) {
        (self.0.on_cursor_use)(self.0.ctx, id);
    }

    fn on_cursor_hidden(&self) {
        (self.0.on_cursor_hidden)(self.0.ctx);
    }

    fn on_cursor_meta(&self, kind: i32) {
        (self.0.on_cursor_meta)(self.0.ctx, kind);
    }

    fn on_text_focus(&self, editable: bool) {
        (self.0.on_text_focus)(self.0.ctx, editable);
    }

    fn on_virtual_display(&self, active: bool, width: i32, height: i32, dpi_percent: i32) {
        (self.0.on_virtual_display)(self.0.ctx, active, width, height, dpi_percent);
    }

    fn on_route_changed(&self, relay_only: bool) {
        (self.0.on_route_changed)(self.0.ctx, relay_only);
    }

    fn on_video_frame(&self, codec: i32, key: bool, width: i32, height: i32, host_us: i64, data: &[u8]) {
        (self.0.on_video_frame)(self.0.ctx, codec, key, width, height, host_us, data.as_ptr(), data.len());
    }
}

static ENGINE: OnceLock<Mutex<Option<Arc<Engine>>>> = OnceLock::new();

fn engine_slot() -> &'static Mutex<Option<Arc<Engine>>> {
    ENGINE.get_or_init(|| Mutex::new(None))
}

fn get_engine() -> Option<Arc<Engine>> {
    engine_slot().lock().unwrap().clone()
}

/// Borrowed C string to an owned Rust string; null becomes empty.
///
/// # Safety
/// `p` must be null or a valid NUL-terminated string.
unsafe fn rstr(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// Owned string for the caller; free with [`rc_string_free`].
fn out(s: &str) -> *mut c_char {
    cs(s).into_raw()
}

fn u16c(v: i32) -> u16 {
    v.clamp(0, u16::MAX as i32) as u16
}

fn i16c(v: i32) -> i16 {
    v.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

/// Frees a string returned by this library. Null is fine.
///
/// # Safety
/// `s` must be null or a pointer returned by this library that was not freed yet.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rc_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { CString::from_raw(s) });
    }
}

/// Starts the engine (or swaps the callbacks of the running one). Returns an empty string on
/// success, otherwise the error text. `files_dir` holds the identity key and saved computers.
///
/// # Safety
/// String arguments must be valid NUL-terminated strings; see [`RcCallbacks`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rc_init(
    files_dir: *const c_char,
    device_name: *const c_char,
    callbacks: RcCallbacks,
) -> *mut c_char {
    let dir = PathBuf::from(unsafe { rstr(files_dir) });
    let name = unsafe { rstr(device_name) };
    let cb = Arc::new(CCallback(callbacks));

    let mut slot = engine_slot().lock().unwrap();
    if let Some(eng) = slot.clone() {
        drop(slot);
        eng.replace_callback(cb);
        return out("");
    }
    match Engine::start(dir, name, cb) {
        Ok(eng) => {
            *slot = Some(Arc::new(eng));
            out("")
        }
        Err(e) => out(&format!("{e:#}")),
    }
}

/// Extra LAN discovery targets, comma or space separated IPv4 addresses. Without the iOS
/// multicast entitlement broadcasts are dropped, so the app passes the subnet's unicast hosts.
///
/// # Safety
/// `csv` must be a valid NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rc_set_broadcasts(csv: *const c_char) {
    let raw = unsafe { rstr(csv) };
    let addrs: Vec<Ipv4Addr> = raw
        .split([',', ' '])
        .map(str::trim)
        .filter_map(|s| s.parse::<Ipv4Addr>().ok())
        .collect();
    if let Some(eng) = get_engine() {
        eng.set_broadcast_addrs(addrs);
    }
}

/// # Safety
/// `name` must be a valid NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rc_set_device_name(name: *const c_char) {
    let name = unsafe { rstr(name) };
    if let Some(eng) = get_engine() {
        eng.set_device_name(&name);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rc_trigger_scan() {
    if let Some(eng) = get_engine() {
        eng.trigger_scan();
    }
}

/// Adds a computer (`rc1…`) or a self-hosted relay (`rcr1…`). Returns JSON
/// `{"ok":bool,"kind":str,"message":str,"device_id":str|null}`.
///
/// # Safety
/// `text` must be a valid NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rc_paste_code(text: *const c_char) -> *mut c_char {
    let code = unsafe { rstr(text) };
    let res = match get_engine() {
        Some(eng) => eng.paste_code(&code),
        None => PasteResult {
            ok: false,
            kind: "none".into(),
            message: "引擎尚未初始化".into(),
            device_id: None,
        },
    };
    out(&serde_json::to_string(&res).unwrap_or_else(|_| r#"{"ok":false}"#.into()))
}

/// Returns an empty string on success, otherwise the error text.
///
/// # Safety
/// `id_hex` must be a valid NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rc_connect(
    id_hex: *const c_char,
    fit_w: i32,
    fit_h: i32,
    fps: i32,
    relay_only: bool,
) -> *mut c_char {
    let id = unsafe { rstr(id_hex) };
    let res = match get_engine() {
        Some(eng) => eng.connect(&id, u16c(fit_w), u16c(fit_h), fps.clamp(0, 120) as u8, relay_only),
        None => Err(anyhow::anyhow!("引擎尚未初始化")),
    };
    match res {
        Ok(()) => out(""),
        Err(e) => out(&format!("{e:#}")),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rc_set_relay_only(relay_only: bool) {
    if let Some(eng) = get_engine() {
        eng.set_relay_only(relay_only);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rc_disconnect() {
    if let Some(eng) = get_engine() {
        eng.disconnect();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rc_select_monitor(index: i32) {
    if let Some(eng) = get_engine() {
        eng.select_monitor(index.clamp(0, 255) as u8);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rc_focus_window(hwnd: u64, view_width: i32, view_height: i32) {
    if let Some(eng) = get_engine() {
        eng.focus_window(hwnd, u16c(view_width), u16c(view_height));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rc_set_virtual_display(active: bool, width: i32, height: i32, dpi_percent: i32) {
    if let Some(eng) = get_engine() {
        eng.set_virtual_display(active, u16c(width), u16c(height), u16c(dpi_percent));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rc_request_keyframe() {
    if let Some(eng) = get_engine() {
        eng.request_keyframe();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rc_set_quality(
    fit_w: i32,
    fit_h: i32,
    short_side: i32,
    fps: i32,
    kbps: i32,
    yuv444: bool,
    auto_fps: bool,
) {
    if let Some(eng) = get_engine() {
        eng.set_quality(
            u16c(fit_w),
            u16c(fit_h),
            u16c(short_side),
            fps.clamp(0, u8::MAX as i32) as u8,
            kbps.max(0) as u32,
            yuv444,
            auto_fps,
        );
    }
}

/// `x`, `y`: 0..=65535 across the streamed monitor.
#[unsafe(no_mangle)]
pub extern "C" fn rc_send_mouse_move(x: i32, y: i32) {
    if let Some(eng) = get_engine() {
        eng.send_mouse_move(u16c(x), u16c(y));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rc_send_mouse_delta(dx: i32, dy: i32) {
    if let Some(eng) = get_engine() {
        eng.send_mouse_delta(i16c(dx), i16c(dy));
    }
}

/// `button`: 0 left, 1 right (0..=4 accepted, as on Android).
#[unsafe(no_mangle)]
pub extern "C" fn rc_send_mouse_button(button: i32, down: bool) {
    if let Some(eng) = get_engine() {
        eng.send_mouse_button(button.clamp(0, 4) as u8, down);
    }
}

/// `phase`: 0 down, 1 move, 2 up, 3 cancel. `x`, `y`: 0..=65535.
#[unsafe(no_mangle)]
pub extern "C" fn rc_send_touch(id: i32, phase: i32, x: i32, y: i32) {
    if let Some(eng) = get_engine() {
        eng.send_touch(id.clamp(0, 9) as u8, phase.clamp(0, 3) as u8, u16c(x), u16c(y));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rc_send_touchpad(phase: i32, dx: i32, dy: i32, scale_milli: i32) {
    if let Some(eng) = get_engine() {
        eng.send_touchpad(phase.clamp(0, 5) as u8, i16c(dx), i16c(dy), u16c(scale_milli));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rc_send_pen(phase: i32, x: i32, y: i32, pressure: i32, tilt_x: i32, tilt_y: i32, flags: i32) {
    if let Some(eng) = get_engine() {
        eng.send_pen(
            phase.clamp(0, 4) as u8,
            u16c(x),
            u16c(y),
            pressure.clamp(0, 1024) as u16,
            tilt_x.clamp(-90, 90) as i8,
            tilt_y.clamp(-90, 90) as i8,
            flags.clamp(0, 255) as u8,
        );
    }
}

/// `delta`: 120 per notch; positive scrolls up (or right when `horizontal`).
#[unsafe(no_mangle)]
pub extern "C" fn rc_send_wheel(delta: i32, horizontal: bool) {
    if let Some(eng) = get_engine() {
        eng.send_wheel(i16c(delta), horizontal);
    }
}

/// `scan`: PC set 1 scan code, `0xE0xx` for extended keys.
#[unsafe(no_mangle)]
pub extern "C" fn rc_send_key(scan: i32, down: bool) {
    if let Some(eng) = get_engine() {
        eng.send_key(scan.clamp(0, 0xFFFF) as u16, down);
    }
}

/// # Safety
/// `text` must be a valid NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rc_send_text(text: *const c_char) {
    let s = unsafe { rstr(text) };
    if !s.is_empty()
        && let Some(eng) = get_engine()
    {
        eng.send_text(&s);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rc_release_all() {
    if let Some(eng) = get_engine() {
        eng.send_release_all();
    }
}

macro_rules! id_call {
    ($(#[$doc:meta])* $name:ident => $method:ident) => {
        $(#[$doc])*
        ///
        /// # Safety
        /// `id_hex` must be a valid NUL-terminated string.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(id_hex: *const c_char) {
            let id = unsafe { rstr(id_hex) };
            if let Some(eng) = get_engine() {
                eng.$method(&id);
            }
        }
    };
}

id_call!(rc_remember_device => remember_device);
id_call!(rc_forget_device => forget_device);
id_call!(rc_agree_remember => agree_remember);
id_call!(rc_refuse_remember => refuse_remember);
id_call!(rc_cancel_remember => cancel_remember);
id_call!(rc_retry_device => retry_device);
