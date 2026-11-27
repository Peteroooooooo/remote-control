//! Replays the viewer's mouse, keyboard, multi-touch, stylus pen, and Windows 11
//! native Precision Touchpad input on the host.
//!
//! Coordinates arrive as 0..=65535 over the streamed display and are mapped to
//! that display's place on the virtual desktop. Keys use scan codes, so the
//! host's keyboard layout decides what a key types, like a real keyboard.

use std::{
    collections::{BTreeMap, HashSet},
    time::{Duration, Instant},
};

use windows::{
    Win32::{
        Foundation::{POINT, RECT},
        System::LibraryLoader::{GetModuleHandleW, GetProcAddress},
        UI::{
            Controls::{
                CreateSyntheticPointerDevice, DestroySyntheticPointerDevice, HSYNTHETICPOINTERDEVICE,
                POINTER_FEEDBACK_DEFAULT, POINTER_FEEDBACK_NONE, POINTER_TYPE_INFO, POINTER_TYPE_INFO_0,
            },
            Input::{
                KeyboardAndMouse::{
                    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
                    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE,
                    MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
                    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
                    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
                    MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput,
                },
                Pointer::{
                    InjectSyntheticPointerInput, InjectTouchInput, InitializeTouchInjection,
                    POINTER_FLAG_CANCELED, POINTER_FLAG_CONFIDENCE, POINTER_FLAG_DOWN, POINTER_FLAG_INCONTACT,
                    POINTER_FLAG_INRANGE, POINTER_FLAG_PRIMARY, POINTER_FLAG_UP, POINTER_FLAG_UPDATE,
                    POINTER_FLAGS, POINTER_INFO, POINTER_PEN_INFO, POINTER_TOUCH_INFO, TOUCH_FEEDBACK_DEFAULT,
                },
            },
            WindowsAndMessaging::{
                GetCursorPos, GetSystemMetrics, PEN_FLAG_BARREL, PEN_FLAG_ERASER, PEN_FLAG_NONE,
                PEN_MASK_NONE, PEN_MASK_PRESSURE, PEN_MASK_TILT_X, PEN_MASK_TILT_Y, PT_PEN, PT_TOUCH,
                PT_TOUCHPAD, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
                TOUCH_FLAG_NONE, TOUCH_MASK_CONTACTAREA, TOUCH_MASK_NONE, TOUCH_MASK_ORIENTATION,
                TOUCH_MASK_PRESSURE, XBUTTON1, XBUTTON2,
            },
        },
    },
    core::{PCSTR, w},
};

const SDCO_PHYSICAL_SIZE: u32 = 0x1;
const SDCO_TOUCHPAD_GESTURE_ONLY: u32 = 0x2;

const PAD_WIDTH_HM: i32 = 10_000; // 100 mm in HIMETRIC
const PAD_HEIGHT_HM: i32 = 7_000; // 70 mm in HIMETRIC
const PAD_CENTER_X: i32 = PAD_WIDTH_HM / 2;
const PAD_CENTER_Y: i32 = PAD_HEIGHT_HM / 2;
const PAD_BASE_HALF_SPAN: i32 = 1_200; // 12 mm half-span between 2 fingers

#[repr(C)]
struct SyntheticDeviceCreationParams {
    pointer_type: u32,
    max_count: u32,
    feedback_mode: u32,
    hmonitor: isize,
    device_width: u32,
    device_height: u32,
    options: u32,
}

type CreateSyntheticPointerDevice2Fn =
    unsafe extern "system" fn(*const SyntheticDeviceCreationParams) -> HSYNTHETICPOINTERDEVICE;

fn resolve_create_synthetic_pointer_device2() -> Option<CreateSyntheticPointerDevice2Fn> {
    unsafe {
        let hmod = GetModuleHandleW(w!("user32.dll")).ok()?;
        let proc = GetProcAddress(hmod, PCSTR(c"CreateSyntheticPointerDevice2".as_ptr() as *const u8))?;
        Some(std::mem::transmute::<unsafe extern "system" fn() -> isize, CreateSyntheticPointerDevice2Fn>(proc))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TouchBackend {
    Synthetic(HSYNTHETICPOINTERDEVICE),
    Legacy,
    None,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PadMode {
    Idle,
    Scroll,
    Pinch,
}

pub struct Injector {
    /// The streamed display in desktop pixels.
    display: RECT,
    /// Cached virtual desktop bounds (refreshed every 1 s).
    virt: RECT,
    virt_at: Instant,
    keys: HashSet<u16>,
    buttons: [bool; 5],
    touch_backend: Option<TouchBackend>,
    active_touches: BTreeMap<u8, (i32, i32)>,
    primary_touch_id: Option<u8>,
    /// Last touch event from the viewer, and last touch frame injected (event or keep-alive).
    touch_event_at: Instant,
    touch_frame_at: Instant,
    pen_device: Option<Option<HSYNTHETICPOINTERDEVICE>>,
    pen_down: bool,
    pen_last_pos: (i32, i32),
    pad_device: Option<Option<HSYNTHETICPOINTERDEVICE>>,
    pad_mode: PadMode,
    pad_cx: i32,
    pad_cy: i32,
    pad_half_span: i32,
    pad_fallback_accum_x: f32,
    pad_fallback_accum_y: f32,
    pad_fallback_pinch_scale: f32,
}

static REFUSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TOUCH_REFUSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Re-inject held touch contacts this often (Windows cancels them after ~1 s without a frame).
const TOUCH_KEEPALIVE: Duration = Duration::from_millis(50);
/// Stop keeping a touch alive after this long without any touch event from the viewer.
const TOUCH_HOLD_MAX: Duration = Duration::from_secs(10);

fn follow_desktop(force: bool) {
    static FAILED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let r = if force {
        crate::desktop::follow()
    } else {
        crate::desktop::follow_cached(Duration::from_millis(100))
    };
    if let Err(e) = r {
        let count = FAILED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if count.is_power_of_two() {
            crate::log!("input: cannot follow the input desktop ({count} times): {e:#}");
        }
    }
}

fn send(inputs: &[INPUT]) {
    follow_desktop(false);
    let mut n = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    if n as usize != inputs.len() && crate::desktop::enabled() {
        follow_desktop(true);
        n = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    }
    if n as usize != inputs.len() {
        let err = windows::core::Error::from_thread();
        let count = REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if count.is_power_of_two() {
            crate::log!("input refused by Windows ({count} times): {err}");
        }
    }
}

fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

/// Moves the mouse one pixel and back: input, without a visible change.
pub fn nudge() {
    send(&[mouse(1, 0, 0, MOUSEEVENTF_MOVE), mouse(-1, 0, 0, MOUSEEVENTF_MOVE)]);
}

fn query_virtual_desktop() -> RECT {
    unsafe {
        let (vx, vy) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
        RECT {
            left: vx,
            top: vy,
            right: vx + GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
            bottom: vy + GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
        }
    }
}

impl Injector {
    pub fn new(display: RECT) -> Injector {
        Injector {
            display,
            virt: query_virtual_desktop(),
            virt_at: Instant::now(),
            keys: HashSet::new(),
            buttons: [false; 5],
            touch_backend: None,
            active_touches: BTreeMap::new(),
            primary_touch_id: None,
            touch_event_at: Instant::now(),
            touch_frame_at: Instant::now(),
            pen_device: None,
            pen_down: false,
            pen_last_pos: (0, 0),
            pad_device: None,
            pad_mode: PadMode::Idle,
            pad_cx: PAD_CENTER_X,
            pad_cy: PAD_CENTER_Y,
            pad_half_span: PAD_BASE_HALF_SPAN,
            pad_fallback_accum_x: 0.0,
            pad_fallback_accum_y: 0.0,
            pad_fallback_pinch_scale: 1.0,
        }
    }

    fn ensure_touch_backend(&mut self) -> TouchBackend {
        if let Some(b) = self.touch_backend {
            return b;
        }
        follow_desktop(true);
        if let Ok(dev) = unsafe { CreateSyntheticPointerDevice(PT_TOUCH, 10, POINTER_FEEDBACK_DEFAULT) }
            && !dev.is_invalid()
        {
            crate::log!("input: initialized modern CreateSyntheticPointerDevice(PT_TOUCH, 10)");
            self.touch_backend = Some(TouchBackend::Synthetic(dev));
            return TouchBackend::Synthetic(dev);
        }
        if unsafe { InitializeTouchInjection(10, TOUCH_FEEDBACK_DEFAULT) }.is_ok() {
            crate::log!("input: initialized legacy InitializeTouchInjection(10)");
            self.touch_backend = Some(TouchBackend::Legacy);
            return TouchBackend::Legacy;
        }
        let err = windows::core::Error::from_thread();
        crate::log!("input: touch injection init failed (falling back to mouse): {err}");
        self.touch_backend = Some(TouchBackend::None);
        TouchBackend::None
    }

    fn ensure_pen_device(&mut self) -> Option<HSYNTHETICPOINTERDEVICE> {
        if let Some(dev) = self.pen_device {
            return dev;
        }
        follow_desktop(true);
        let dev = unsafe { CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT) }
            .ok()
            .filter(|h| !h.is_invalid());
        if dev.is_some() {
            crate::log!("input: initialized CreateSyntheticPointerDevice(PT_PEN)");
        }
        self.pen_device = Some(dev);
        dev
    }

    fn ensure_pad_device(&mut self) -> Option<HSYNTHETICPOINTERDEVICE> {
        if let Some(dev) = self.pad_device {
            return dev;
        }
        follow_desktop(true);
        let dev = resolve_create_synthetic_pointer_device2().and_then(|create2| {
            let params = SyntheticDeviceCreationParams {
                pointer_type: PT_TOUCHPAD.0 as u32,
                max_count: 2,
                feedback_mode: POINTER_FEEDBACK_NONE.0 as u32,
                hmonitor: 0,
                device_width: PAD_WIDTH_HM as u32,
                device_height: PAD_HEIGHT_HM as u32,
                options: SDCO_PHYSICAL_SIZE | SDCO_TOUCHPAD_GESTURE_ONLY,
            };
            let handle = unsafe { create2(&params) };
            if !handle.is_invalid() {
                crate::log!("input: initialized Windows 11 native Precision Touchpad (PT_TOUCHPAD)");
                Some(handle)
            } else {
                let err = windows::core::Error::from_thread();
                crate::log!("input: CreateSyntheticPointerDevice2(PT_TOUCHPAD) unavailable ({err}); using wheel fallback");
                None
            }
        });
        self.pad_device = Some(dev);
        dev
    }

    fn send_touch_contacts(&mut self, contacts: &[POINTER_TOUCH_INFO]) -> bool {
        let backend = self.ensure_touch_backend();
        follow_desktop(false);
        match backend {
            TouchBackend::Synthetic(dev) => {
                let items: Vec<POINTER_TYPE_INFO> = contacts
                    .iter()
                    .copied()
                    .map(|ti| POINTER_TYPE_INFO {
                        r#type: PT_TOUCH,
                        Anonymous: POINTER_TYPE_INFO_0 { touchInfo: ti },
                    })
                    .collect();
                let mut r = unsafe { InjectSyntheticPointerInput(dev, &items) };
                if r.is_err() && crate::desktop::enabled() {
                    follow_desktop(true);
                    r = unsafe { InjectSyntheticPointerInput(dev, &items) };
                }
                if let Err(err) = r {
                    let count = TOUCH_REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    if count.is_power_of_two() {
                        crate::log!("synthetic touch refused ({count} times): {err}");
                    }
                    return false;
                }
                true
            }
            TouchBackend::Legacy => {
                let mut r = unsafe { InjectTouchInput(contacts) };
                if r.is_err() && crate::desktop::enabled() {
                    follow_desktop(true);
                    let _ = unsafe { InitializeTouchInjection(10, TOUCH_FEEDBACK_DEFAULT) };
                    r = unsafe { InjectTouchInput(contacts) };
                }
                if let Err(err) = r {
                    let count = TOUCH_REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    if count.is_power_of_two() {
                        crate::log!("legacy touch refused ({count} times): {err}");
                    }
                    return false;
                }
                true
            }
            TouchBackend::None => false,
        }
    }

    /// Refreshes cached virtual desktop bounds and recreates synthetic pointer devices
    /// if the monitor topology changed (e.g. virtual display added/removed) while idle.
    fn refresh_virtual_desktop(&mut self, force: bool) {
        if !force && self.virt_at.elapsed() < Duration::from_secs(1) {
            return;
        }
        let virt_now = query_virtual_desktop();
        let changed = virt_now != self.virt;
        self.virt = virt_now;
        self.virt_at = Instant::now();
        if changed {
            if self.active_touches.is_empty()
                && let Some(TouchBackend::Synthetic(dev)) = self.touch_backend.take()
            {
                unsafe { DestroySyntheticPointerDevice(dev) };
            }
            if !self.pen_down
                && let Some(Some(dev)) = self.pen_device.take()
            {
                unsafe { DestroySyntheticPointerDevice(dev) };
            }
        }
    }

    /// Injects a native Windows multi-touch contact (`id` 0..10; `phase`: 0 down, 1 move, 2 up, 3 cancel).
    pub fn touch(&mut self, id: u8, phase: u8, x: u16, y: u16) {
        if id >= 10 {
            return;
        }
        self.refresh_virtual_desktop(false);
        let backend = self.ensure_touch_backend();
        if backend == TouchBackend::None {
            if id == 0 {
                match phase {
                    0 => {
                        self.move_to(x, y);
                        self.button(0, true);
                    }
                    1 => self.move_to(x, y),
                    2 | 3 => {
                        self.move_to(x, y);
                        self.button(0, false);
                    }
                    _ => {}
                }
            }
            return;
        }
        let (px, py) = self.to_desktop(x, y);
        // Windows rejects an UP frame whose location differs from the contact's last frame
        // (ERROR_INVALID_PARAMETER) and cancels the contact, which turns a tap into nothing.
        // Move the contact to the lift position first, then lift it there.
        if phase == 2 && self.active_touches.get(&id).is_some_and(|&p| p != (px, py)) {
            self.touch(id, 1, x, y);
        }
        let is_up_or_cancel = phase == 2 || phase == 3;
        let is_new_down = match phase {
            0 | 1 => self.active_touches.insert(id, (px, py)).is_none(),
            2 | 3 => {
                if !self.active_touches.contains_key(&id) {
                    return;
                }
                if phase == 2 {
                    self.active_touches.insert(id, (px, py));
                }
                false
            }
            _ => return,
        };

        if is_new_down && self.primary_touch_id.is_none() {
            self.primary_touch_id = Some(id);
        }
        let changed_flags = match phase {
            0 => POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_CONFIDENCE,
            1 if is_new_down => {
                POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_CONFIDENCE
            }
            1 => POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_CONFIDENCE,
            2 => POINTER_FLAG_UP,
            _ => POINTER_FLAG_UP | POINTER_FLAG_CANCELED,
        };
        let contacts = self.touch_frame(Some((id, changed_flags, is_up_or_cancel)));
        self.touch_event_at = Instant::now();
        self.touch_frame_at = self.touch_event_at;
        if !contacts.is_empty() {
            let ok = self.send_touch_contacts(&contacts);
            // On UAC / secure desktop where touch injection is blocked, degrade single finger cleanly to mouse.
            if !ok && id == 0 && self.active_touches.len() == 1 {
                match phase {
                    0 => {
                        self.move_to(x, y);
                        self.button(0, true);
                    }
                    1 => self.move_to(x, y),
                    2 | 3 => {
                        self.move_to(x, y);
                        self.button(0, false);
                    }
                    _ => {}
                }
            }
        }
        if is_up_or_cancel {
            self.active_touches.remove(&id);
            if self.primary_touch_id == Some(id) || self.active_touches.is_empty() {
                self.primary_touch_id = None;
            }
        }
    }

    /// One injection frame holding every active contact. `changed` is the contact this frame
    /// is about (its flags and whether it is being lifted); all others repeat as UPDATE.
    fn touch_frame(&self, changed: Option<(u8, POINTER_FLAGS, bool)>) -> Vec<POINTER_TOUCH_INFO> {
        let is_synthetic = matches!(self.touch_backend, Some(TouchBackend::Synthetic(_)));
        let virt = self.virt;
        let virt_w = (virt.right - virt.left).max(1);
        let virt_h = (virt.bottom - virt.top).max(1);
        let held = POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_CONFIDENCE;
        let mut contacts = Vec::with_capacity(self.active_touches.len());
        for (&cid, &(cx, cy)) in &self.active_touches {
            let (mut flags, releasing) = match changed {
                Some((id, f, rel)) if id == cid => (f, rel),
                _ => (held, false),
            };
            if self.primary_touch_id == Some(cid) {
                flags |= POINTER_FLAG_PRIMARY;
            }
            // Windows `InjectSyntheticPointerInput(PT_TOUCH)` measures `ptPixelLocation` and
            // `rcContact` relative to the virtual screen top-left (`SM_XVIRTUALSCREEN`, `SM_YVIRTUALSCREEN`),
            // NOT primary monitor (0, 0). Legacy `InjectTouchInput` uses desktop pixels directly.
            let (tx, ty) = if is_synthetic {
                desktop_to_synthetic_pointer(&virt, cx, cy)
            } else {
                (cx, cy)
            };
            let rc_contact = if releasing {
                RECT::default()
            } else if is_synthetic {
                RECT {
                    left: (tx - 6).max(0),
                    top: (ty - 6).max(0),
                    right: (tx + 6).min(virt_w),
                    bottom: (ty + 6).min(virt_h),
                }
            } else {
                RECT { left: tx - 6, top: ty - 6, right: tx + 6, bottom: ty + 6 }
            };
            contacts.push(POINTER_TOUCH_INFO {
                pointerInfo: POINTER_INFO {
                    pointerType: PT_TOUCH,
                    pointerId: cid as u32,
                    pointerFlags: flags,
                    ptPixelLocation: POINT { x: tx, y: ty },
                    ..Default::default()
                },
                touchFlags: TOUCH_FLAG_NONE,
                touchMask: if releasing {
                    TOUCH_MASK_NONE
                } else {
                    TOUCH_MASK_CONTACTAREA | TOUCH_MASK_ORIENTATION | TOUCH_MASK_PRESSURE
                },
                rcContact: rc_contact,
                orientation: if releasing { 0 } else { 90 },
                pressure: if releasing { 0 } else { 512 },
                ..Default::default()
            });
        }
        contacts
    }

    /// How long the inject loop may block before [`Self::touch_keepalive`] is due, or `None`
    /// when no finger is held. Windows cancels an injected contact that gets no frame for
    /// about a second, so a finger resting still (press-and-hold to select text or right-click)
    /// would otherwise be dropped mid-gesture.
    pub fn touch_keepalive_wait(&self) -> Option<Duration> {
        if self.active_touches.is_empty() || self.touch_event_at.elapsed() > TOUCH_HOLD_MAX {
            return None;
        }
        Some(TOUCH_KEEPALIVE.saturating_sub(self.touch_frame_at.elapsed()))
    }

    /// Repeats the held contacts in place once [`TOUCH_KEEPALIVE`] has passed since the last frame.
    /// Stops after [`TOUCH_HOLD_MAX`] without news from the viewer, so a dropped connection
    /// lets Windows cancel the touch instead of holding it forever.
    pub fn touch_keepalive(&mut self) {
        if self.touch_keepalive_wait().is_none_or(|w| !w.is_zero()) {
            return;
        }
        let contacts = self.touch_frame(None);
        self.touch_frame_at = Instant::now();
        if !contacts.is_empty() {
            let _ = self.send_touch_contacts(&contacts);
        }
    }

    /// Injects native Windows stylus/pen input (`phase`: 0 hover, 1 down, 2 move, 3 up, 4 cancel;
    /// `flags`: bit 0 barrel button, bit 1 eraser).
    pub fn pen(&mut self, phase: u8, x: u16, y: u16, pressure: u16, tilt_x: i8, tilt_y: i8, flags: u8) {
        self.refresh_virtual_desktop(false);
        let Some(dev) = self.ensure_pen_device() else {
            // Fallback to mouse for devices without pen injection.
            match phase {
                0 => self.move_to(x, y),
                1 => {
                    self.move_to(x, y);
                    self.button(0, true);
                }
                2 => self.move_to(x, y),
                3 | 4 => {
                    self.move_to(x, y);
                    self.button(0, false);
                }
                _ => {}
            }
            return;
        };

        let (px, py) = self.to_desktop(x, y);
        let (tx, ty) = desktop_to_synthetic_pointer(&self.virt, px, py);
        self.pen_last_pos = (tx, ty);
        let pflags = match phase {
            0 => POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_PRIMARY,
            1 => {
                self.pen_down = true;
                POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_PRIMARY
            }
            2 => {
                if self.pen_down {
                    POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_PRIMARY
                } else {
                    self.pen_down = true;
                    POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_PRIMARY
                }
            }
            3 => {
                if !self.pen_down {
                    return;
                }
                self.pen_down = false;
                POINTER_FLAG_UP | POINTER_FLAG_INRANGE | POINTER_FLAG_PRIMARY
            }
            4 => {
                if !self.pen_down {
                    return;
                }
                self.pen_down = false;
                POINTER_FLAG_UP | POINTER_FLAG_CANCELED | POINTER_FLAG_PRIMARY
            }
            _ => return,
        };

        let mut pen_flags = PEN_FLAG_NONE;
        if flags & 1 != 0 {
            pen_flags |= PEN_FLAG_BARREL;
        }
        if flags & 2 != 0 {
            pen_flags |= PEN_FLAG_ERASER;
        }
        let releasing = phase == 3 || phase == 4;
        let info = POINTER_TYPE_INFO {
            r#type: PT_PEN,
            Anonymous: POINTER_TYPE_INFO_0 {
                penInfo: POINTER_PEN_INFO {
                    pointerInfo: POINTER_INFO {
                        pointerType: PT_PEN,
                        pointerId: 0,
                        pointerFlags: pflags,
                        ptPixelLocation: POINT { x: tx, y: ty },
                        ..Default::default()
                    },
                    penFlags: pen_flags,
                    penMask: if releasing {
                        PEN_MASK_NONE
                    } else {
                        PEN_MASK_PRESSURE | PEN_MASK_TILT_X | PEN_MASK_TILT_Y
                    },
                    pressure: if releasing { 0 } else { (pressure as u32).min(1024) },
                    rotation: 0,
                    tiltX: (tilt_x as i32).clamp(-90, 90),
                    tiltY: (tilt_y as i32).clamp(-90, 90),
                },
            },
        };
        follow_desktop(false);
        let _ = unsafe { InjectSyntheticPointerInput(dev, &[info]) };
    }

    /// Injects Windows 11 native Precision Touchpad smooth 2-finger scroll & pinch-zoom,
    /// with seamless fallback to smooth `MOUSEEVENTF_WHEEL`/`HWHEEL` and `Ctrl+Wheel`.
    ///
    /// `phase`: 0 scroll_start, 1 scroll_move, 2 scroll_end, 3 pinch_start, 4 pinch_update, 5 pinch_end.
    /// `dx, dy`: 1/10th dp delta (positive dy = finger moving down).
    /// `scale_milli`: cumulative pinch scale * 1000 (e.g. 1250 = 1.25x).
    pub fn touchpad(&mut self, phase: u8, dx: i16, dy: i16, scale_milli: u16) {
        if let Some(dev) = self.ensure_pad_device() {
            if self.inject_ptp_frame(dev, phase, dx, dy, scale_milli) {
                return;
            }
        }
        // Fallback path (Windows 10 or secure desktop): smooth wheel / Ctrl+wheel.
        self.touchpad_fallback(phase, dx, dy, scale_milli);
    }

    fn inject_ptp_frame(
        &mut self,
        dev: HSYNTHETICPOINTERDEVICE,
        phase: u8,
        dx: i16,
        dy: i16,
        scale_milli: u16,
    ) -> bool {
        let make_pair = |x0: i32, y0: i32, x1: i32, y1: i32, flags: windows::Win32::UI::Input::Pointer::POINTER_FLAGS| -> [POINTER_TYPE_INFO; 2] {
            let mk = |id: u32, x: i32, y: i32, primary: bool| POINTER_TYPE_INFO {
                r#type: PT_TOUCHPAD,
                Anonymous: POINTER_TYPE_INFO_0 {
                    touchInfo: POINTER_TOUCH_INFO {
                        pointerInfo: POINTER_INFO {
                            pointerType: PT_TOUCHPAD,
                            pointerId: id,
                            pointerFlags: if primary { flags | POINTER_FLAG_PRIMARY } else { flags },
                            ptHimetricLocation: POINT {
                                x: x.clamp(100, PAD_WIDTH_HM - 100),
                                y: y.clamp(100, PAD_HEIGHT_HM - 100),
                            },
                            ..Default::default()
                        },
                        touchFlags: TOUCH_FLAG_NONE,
                        touchMask: TOUCH_MASK_NONE,
                        ..Default::default()
                    },
                },
            };
            [mk(0, x0, y0, true), mk(1, x1, y1, false)]
        };

        let down_flags =
            POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_CONFIDENCE;
        let move_flags =
            POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_CONFIDENCE;
        let up_flags = POINTER_FLAG_UP;

        follow_desktop(false);
        match phase {
            0 => {
                // Scroll start
                if self.pad_mode != PadMode::Idle {
                    self.end_ptp(dev);
                }
                self.pad_mode = PadMode::Scroll;
                self.pad_cx = PAD_CENTER_X;
                self.pad_cy = PAD_CENTER_Y;
                self.pad_half_span = PAD_BASE_HALF_SPAN;
                let pair = make_pair(
                    self.pad_cx - self.pad_half_span,
                    self.pad_cy,
                    self.pad_cx + self.pad_half_span,
                    self.pad_cy,
                    down_flags,
                );
                unsafe { InjectSyntheticPointerInput(dev, &pair) }.is_ok()
            }
            1 => {
                // Scroll move: convert 1/10 dp into HIMETRIC (~1.6 mm per 10 dp => 16 hm per unit)
                if self.pad_mode != PadMode::Scroll {
                    let _ = self.inject_ptp_frame(dev, 0, 0, 0, 1000);
                }
                let step_x = (dx as i32) * 16;
                let step_y = (dy as i32) * 16;
                let next_cx = self.pad_cx + step_x;
                let next_cy = self.pad_cy + step_y;

                // Re-anchor smoothly if approaching virtual pad boundary during long continuous scrolls.
                if next_cx < 1_600
                    || next_cx > PAD_WIDTH_HM - 1_600
                    || next_cy < 900
                    || next_cy > PAD_HEIGHT_HM - 900
                {
                    self.end_ptp(dev);
                    self.pad_mode = PadMode::Scroll;
                    self.pad_cx = PAD_CENTER_X;
                    self.pad_cy = PAD_CENTER_Y;
                    let down_pair = make_pair(
                        self.pad_cx - self.pad_half_span,
                        self.pad_cy,
                        self.pad_cx + self.pad_half_span,
                        self.pad_cy,
                        down_flags,
                    );
                    let _ = unsafe { InjectSyntheticPointerInput(dev, &down_pair) };
                    self.pad_cx = (self.pad_cx + step_x).clamp(1_600, PAD_WIDTH_HM - 1_600);
                    self.pad_cy = (self.pad_cy + step_y).clamp(900, PAD_HEIGHT_HM - 900);
                } else {
                    self.pad_cx = next_cx;
                    self.pad_cy = next_cy;
                }

                let pair = make_pair(
                    self.pad_cx - self.pad_half_span,
                    self.pad_cy,
                    self.pad_cx + self.pad_half_span,
                    self.pad_cy,
                    move_flags,
                );
                unsafe { InjectSyntheticPointerInput(dev, &pair) }.is_ok()
            }
            2 => {
                // Scroll end
                if self.pad_mode == PadMode::Scroll {
                    let pair = make_pair(
                        self.pad_cx - self.pad_half_span,
                        self.pad_cy,
                        self.pad_cx + self.pad_half_span,
                        self.pad_cy,
                        up_flags,
                    );
                    self.pad_mode = PadMode::Idle;
                    unsafe { InjectSyntheticPointerInput(dev, &pair) }.is_ok()
                } else {
                    true
                }
            }
            3 => {
                // Pinch start
                if self.pad_mode != PadMode::Idle {
                    self.end_ptp(dev);
                }
                self.pad_mode = PadMode::Pinch;
                self.pad_cx = PAD_CENTER_X;
                self.pad_cy = PAD_CENTER_Y;
                self.pad_half_span = PAD_BASE_HALF_SPAN;
                let pair = make_pair(
                    self.pad_cx - self.pad_half_span,
                    self.pad_cy,
                    self.pad_cx + self.pad_half_span,
                    self.pad_cy,
                    down_flags,
                );
                unsafe { InjectSyntheticPointerInput(dev, &pair) }.is_ok()
            }
            4 => {
                // Pinch update: scale_milli around 1000
                if self.pad_mode != PadMode::Pinch {
                    let _ = self.inject_ptp_frame(dev, 3, 0, 0, 1000);
                }
                let s = (scale_milli.clamp(250, 4000) as f32) / 1000.0;
                self.pad_half_span = ((PAD_BASE_HALF_SPAN as f32) * s)
                    .round()
                    .clamp(350.0, (PAD_WIDTH_HM / 2 - 300) as f32) as i32;
                let pair = make_pair(
                    self.pad_cx - self.pad_half_span,
                    self.pad_cy,
                    self.pad_cx + self.pad_half_span,
                    self.pad_cy,
                    move_flags,
                );
                unsafe { InjectSyntheticPointerInput(dev, &pair) }.is_ok()
            }
            5 => {
                // Pinch end
                if self.pad_mode == PadMode::Pinch {
                    let pair = make_pair(
                        self.pad_cx - self.pad_half_span,
                        self.pad_cy,
                        self.pad_cx + self.pad_half_span,
                        self.pad_cy,
                        up_flags,
                    );
                    self.pad_mode = PadMode::Idle;
                    unsafe { InjectSyntheticPointerInput(dev, &pair) }.is_ok()
                } else {
                    true
                }
            }
            _ => true,
        }
    }

    fn end_ptp(&mut self, dev: HSYNTHETICPOINTERDEVICE) {
        if self.pad_mode == PadMode::Idle {
            return;
        }
        let mk = |id: u32, x: i32, primary: bool| POINTER_TYPE_INFO {
            r#type: PT_TOUCHPAD,
            Anonymous: POINTER_TYPE_INFO_0 {
                touchInfo: POINTER_TOUCH_INFO {
                    pointerInfo: POINTER_INFO {
                        pointerType: PT_TOUCHPAD,
                        pointerId: id,
                        pointerFlags: if primary {
                            POINTER_FLAG_UP | POINTER_FLAG_CANCELED | POINTER_FLAG_PRIMARY
                        } else {
                            POINTER_FLAG_UP | POINTER_FLAG_CANCELED
                        },
                        ptHimetricLocation: POINT {
                            x: x.clamp(100, PAD_WIDTH_HM - 100),
                            y: self.pad_cy.clamp(100, PAD_HEIGHT_HM - 100),
                        },
                        ..Default::default()
                    },
                    touchFlags: TOUCH_FLAG_NONE,
                    touchMask: TOUCH_MASK_NONE,
                    ..Default::default()
                },
            },
        };
        let pair = [
            mk(0, self.pad_cx - self.pad_half_span, true),
            mk(1, self.pad_cx + self.pad_half_span, false),
        ];
        let _ = unsafe { InjectSyntheticPointerInput(dev, &pair) };
        self.pad_mode = PadMode::Idle;
    }

    fn touchpad_fallback(&mut self, phase: u8, dx: i16, dy: i16, scale_milli: u16) {
        match phase {
            0 => {
                self.pad_fallback_accum_x = 0.0;
                self.pad_fallback_accum_y = 0.0;
            }
            1 => {
                // dx/dy are 1/10th dp; ~32 dp = 120 wheel units => 0.375 wheel units per 0.1 dp
                self.pad_fallback_accum_y += (dy as f32) * 0.375;
                self.pad_fallback_accum_x += -(dx as f32) * 0.375;
                if self.pad_fallback_accum_y.abs() >= 12.0 {
                    let s = (self.pad_fallback_accum_y as i32).clamp(-240, 240);
                    if s != 0 {
                        self.wheel(s as i16, false);
                        self.pad_fallback_accum_y -= s as f32;
                    }
                }
                if self.pad_fallback_accum_x.abs() >= 12.0 {
                    let s = (self.pad_fallback_accum_x as i32).clamp(-240, 240);
                    if s != 0 {
                        self.wheel(s as i16, true);
                        self.pad_fallback_accum_x -= s as f32;
                    }
                }
            }
            2 => {
                self.pad_fallback_accum_x = 0.0;
                self.pad_fallback_accum_y = 0.0;
            }
            3 => {
                self.pad_fallback_pinch_scale = 1.0;
            }
            4 => {
                let s = (scale_milli.clamp(250, 4000) as f32) / 1000.0;
                let ratio = s / self.pad_fallback_pinch_scale.max(0.1);
                if ratio >= 1.12 {
                    self.key(0x1D, true); // Left Ctrl
                    self.wheel(120, false);
                    self.key(0x1D, false);
                    self.pad_fallback_pinch_scale = s;
                } else if ratio <= 0.89 {
                    self.key(0x1D, true);
                    self.wheel(-120, false);
                    self.key(0x1D, false);
                    self.pad_fallback_pinch_scale = s;
                }
            }
            5 => {
                self.pad_fallback_pinch_scale = 1.0;
            }
            _ => {}
        }
    }

    pub fn set_display(&mut self, display: RECT) {
        if self.display != display {
            self.display = display;
            self.refresh_virtual_desktop(true);
        }
    }

    /// Stream position (0..=65535) to desktop pixels.
    fn to_desktop(&self, x: u16, y: u16) -> (i32, i32) {
        stream_to_desktop(&self.display, x, y)
    }

    pub fn move_to(&mut self, x: u16, y: u16) {
        let (px, py) = self.to_desktop(x, y);
        self.refresh_virtual_desktop(false);
        let (ax, ay) = desktop_to_absolute(&self.virt, px, py);
        send(&[mouse(ax, ay, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)]);
    }

    /// Relative physical mouse movement (for Android `requestPointerCapture`).
    pub fn move_delta(&mut self, dx: i16, dy: i16) {
        if dx == 0 && dy == 0 {
            return;
        }
        send(&[mouse(dx as i32, dy as i32, 0, MOUSEEVENTF_MOVE)]);
    }

    pub fn button(&mut self, button: u8, down: bool) {
        let (flags, data) = match (button, down) {
            (0, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (0, false) => (MOUSEEVENTF_LEFTUP, 0),
            (1, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (1, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (2, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (2, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (3, d) => (if d { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, XBUTTON1 as i32),
            (4, d) => (if d { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, XBUTTON2 as i32),
            _ => return,
        };
        self.buttons[button as usize] = down;
        send(&[mouse(0, 0, data, flags)]);
    }

    pub fn wheel(&self, delta: i16, horizontal: bool) {
        let flags = if horizontal { MOUSEEVENTF_HWHEEL } else { MOUSEEVENTF_WHEEL };
        send(&[mouse(0, 0, delta as i32, flags)]);
    }

    pub fn key(&mut self, scan: u16, down: bool) {
        let mut flags: KEYBD_EVENT_FLAGS = KEYEVENTF_SCANCODE;
        if scan & 0xFF00 == 0xE000 {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !down {
            flags |= KEYEVENTF_KEYUP;
        }
        if down {
            self.keys.insert(scan);
        } else {
            self.keys.remove(&scan);
        }
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: Default::default(),
                    wScan: scan & 0xFF,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        send(&[input]);
    }

    /// Injects arbitrary UTF-8 text directly via `KEYEVENTF_UNICODE`.
    pub fn text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut batch = Vec::<INPUT>::with_capacity(64);
        let flush = |buf: &mut Vec<INPUT>| {
            if !buf.is_empty() {
                send(buf);
                buf.clear();
            }
        };
        for ch in text.chars() {
            if ch == '\r' {
                continue;
            }
            if ch == '\n' {
                flush(&mut batch);
                self.key(0x1C, true);
                self.key(0x1C, false);
                continue;
            }
            let mut u16buf = [0u16; 2];
            for unit in ch.encode_utf16(&mut u16buf).iter().copied() {
                batch.push(INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: Default::default(),
                            wScan: unit,
                            dwFlags: KEYEVENTF_UNICODE,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                });
                batch.push(INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: Default::default(),
                            wScan: unit,
                            dwFlags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                });
                if batch.len() >= 64 {
                    flush(&mut batch);
                }
            }
        }
        flush(&mut batch);
    }

    /// Lets go of everything the viewer pressed (focus lost or disconnected).
    pub fn release_all(&mut self) {
        if !self.active_touches.is_empty() {
            let is_synthetic = matches!(self.touch_backend, Some(TouchBackend::Synthetic(_)));
            let virt = self.virt;
            let primary_id = self.primary_touch_id;
            let contacts: Vec<POINTER_TOUCH_INFO> = self
                .active_touches
                .iter()
                .map(|(&cid, &(cx, cy))| {
                    let (tx, ty) = if is_synthetic {
                        desktop_to_synthetic_pointer(&virt, cx, cy)
                    } else {
                        (cx, cy)
                    };
                    let mut flags = POINTER_FLAG_UP | POINTER_FLAG_CANCELED;
                    if primary_id == Some(cid) {
                        flags |= POINTER_FLAG_PRIMARY;
                    }
                    POINTER_TOUCH_INFO {
                        pointerInfo: POINTER_INFO {
                            pointerType: PT_TOUCH,
                            pointerId: cid as u32,
                            pointerFlags: flags,
                            ptPixelLocation: POINT { x: tx, y: ty },
                            ..Default::default()
                        },
                        touchFlags: TOUCH_FLAG_NONE,
                        touchMask: TOUCH_MASK_NONE,
                        ..Default::default()
                    }
                })
                .collect();
            let _ = self.send_touch_contacts(&contacts);
            self.active_touches.clear();
            self.primary_touch_id = None;
        }
        if self.pen_down
            && let Some(Some(dev)) = self.pen_device
        {
            let (px, py) = self.pen_last_pos;
            let info = POINTER_TYPE_INFO {
                r#type: PT_PEN,
                Anonymous: POINTER_TYPE_INFO_0 {
                    penInfo: POINTER_PEN_INFO {
                        pointerInfo: POINTER_INFO {
                            pointerType: PT_PEN,
                            pointerId: 0,
                            pointerFlags: POINTER_FLAG_UP | POINTER_FLAG_CANCELED | POINTER_FLAG_PRIMARY,
                            ptPixelLocation: POINT { x: px, y: py },
                            ..Default::default()
                        },
                        penFlags: PEN_FLAG_NONE,
                        penMask: PEN_MASK_NONE,
                        ..Default::default()
                    },
                },
            };
            let _ = unsafe { InjectSyntheticPointerInput(dev, &[info]) };
            self.pen_down = false;
        }
        if let Some(Some(dev)) = self.pad_device {
            self.end_ptp(dev);
        }
        for scan in std::mem::take(&mut self.keys) {
            self.key(scan, false);
            self.keys.remove(&scan);
        }
        for b in 0..5u8 {
            if self.buttons[b as usize] {
                self.button(b, false);
            }
        }
    }

    /// Where the pointer is, as a stream position.
    pub fn cursor(&self) -> (u16, u16) {
        follow_desktop(false);
        let mut p = POINT::default();
        if unsafe { GetCursorPos(&mut p) }.is_err() {
            follow_desktop(true);
            let _ = unsafe { GetCursorPos(&mut p) };
        }
        desktop_to_stream(&self.display, p.x, p.y)
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        self.release_all();
        if let Some(TouchBackend::Synthetic(dev)) = self.touch_backend.take() {
            unsafe { DestroySyntheticPointerDevice(dev) };
        }
        if let Some(Some(dev)) = self.pen_device.take() {
            unsafe { DestroySyntheticPointerDevice(dev) };
        }
        if let Some(Some(dev)) = self.pad_device.take() {
            unsafe { DestroySyntheticPointerDevice(dev) };
        }
    }
}

/// Stream position (0..=65535 over `display`) to a desktop pixel.
fn stream_to_desktop(d: &RECT, x: u16, y: u16) -> (i32, i32) {
    let w = (d.right - d.left - 1).max(0) as i64;
    let h = (d.bottom - d.top - 1).max(0) as i64;
    (d.left + (x as i64 * w / 65535) as i32, d.top + (y as i64 * h / 65535) as i32)
}

/// Desktop pixel to stream position, clamped to `display`.
fn desktop_to_stream(d: &RECT, px: i32, py: i32) -> (u16, u16) {
    let w = (d.right - d.left - 1).max(1) as i64;
    let h = (d.bottom - d.top - 1).max(1) as i64;
    let x = ((px - d.left).clamp(0, w as i32) as i64 * 65535 / w) as u16;
    let y = ((py - d.top).clamp(0, h as i32) as i64 * 65535 / h) as u16;
    (x, y)
}

/// Desktop pixel to SendInput's absolute 0..=65535 over the virtual desktop `virt`.
fn desktop_to_absolute(virt: &RECT, px: i32, py: i32) -> (i32, i32) {
    let (vw, vh) = ((virt.right - virt.left).max(2) as i64, (virt.bottom - virt.top).max(2) as i64);
    let ax = (((px - virt.left) as i64 * 65536 + vw - 1) / vw).clamp(0, 65535) as i32;
    let ay = (((py - virt.top) as i64 * 65536 + vh - 1) / vh).clamp(0, 65535) as i32;
    (ax, ay)
}

/// Desktop pixel to `InjectSyntheticPointerInput` (`PT_TOUCH` / `PT_PEN`) coordinates,
/// which Windows measures relative to the virtual screen top-left (`SM_XVIRTUALSCREEN`, `SM_YVIRTUALSCREEN`).
fn desktop_to_synthetic_pointer(virt: &RECT, px: i32, py: i32) -> (i32, i32) {
    let max_x = (virt.right - virt.left - 1).max(0);
    let max_y = (virt.bottom - virt.top - 1).max(0);
    ((px - virt.left).clamp(0, max_x), (py - virt.top).clamp(0, max_y))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn windows_pixel(virt: &RECT, ax: i32, ay: i32) -> (i32, i32) {
        let (vw, vh) = ((virt.right - virt.left) as i64, (virt.bottom - virt.top) as i64);
        (virt.left + (ax as i64 * vw / 65536) as i32, virt.top + (ay as i64 * vh / 65536) as i32)
    }

    #[test]
    fn maps_onto_a_display_left_of_and_above_the_primary() {
        let left = RECT { left: -1080, top: -300, right: 0, bottom: 1620 };
        let primary = RECT { left: 0, top: 0, right: 2560, bottom: 1440 };
        let virt = RECT { left: -1080, top: -300, right: 2560, bottom: 1620 };
        for (d, corners) in [
            (&left, [(-1080, -300), (-1, 1619)]),
            (&primary, [(0, 0), (2559, 1439)]),
        ] {
            assert_eq!(stream_to_desktop(d, 0, 0), corners[0]);
            assert_eq!(stream_to_desktop(d, 65535, 65535), corners[1]);
            for (x, y) in [(0u16, 0u16), (32768, 100), (65535, 65535), (12345, 54321)] {
                let (px, py) = stream_to_desktop(d, x, y);
                assert!(px >= d.left && px < d.right && py >= d.top && py < d.bottom);
                let (ax, ay) = desktop_to_absolute(&virt, px, py);
                assert_eq!(windows_pixel(&virt, ax, ay), (px, py), "stream ({x}, {y}) on {d:?}");
                let (sx, sy) = desktop_to_stream(d, px, py);
                assert!((sx as i32 - x as i32).abs() <= 65535 / (d.right - d.left - 1) + 1);
                assert!((sy as i32 - y as i32).abs() <= 65535 / (d.bottom - d.top - 1) + 1);
            }
        }
        assert_eq!(desktop_to_stream(&primary, -500, 700).0, 0);
        assert_eq!(desktop_to_stream(&left, 100, 2000), (65535, 65535));
    }

    #[test]
    fn every_pixel_of_a_wide_virtual_desktop_is_reachable() {
        let virt = RECT { left: -1920, top: 0, right: 3840, bottom: 1080 };
        for px in (-1920..3840).step_by(7).chain([3839]) {
            let (ax, _) = desktop_to_absolute(&virt, px, 0);
            assert_eq!(windows_pixel(&virt, ax, 0).0, px);
        }
    }

    #[test]
    fn synthetic_pointer_subtracts_negative_virtual_screen_origin() {
        // Exact layout of multi-monitor desktop with rotated DISPLAY2 above primary + ZakoVDD DISPLAY5
        let virt = RECT { left: 0, top: -666, right: 6412, bottom: 1894 };
        // DISPLAY1 (0, 0) 2560x1440
        assert_eq!(desktop_to_synthetic_pointer(&virt, 700, 900), (700, 1566));
        assert_eq!(desktop_to_synthetic_pointer(&virt, 0, 0), (0, 666));
        // DISPLAY2 (2560, -666) 1440x2560
        assert_eq!(desktop_to_synthetic_pointer(&virt, 3200, -200), (3200, 466));
        assert_eq!(desktop_to_synthetic_pointer(&virt, 2560, -666), (2560, 0));
        // DISPLAY5 ZakoVDD (4000, 0) 2412x1080
        assert_eq!(desktop_to_synthetic_pointer(&virt, 5206, 540), (5206, 1206));
        // Negative X monitor layout
        let virt_neg_x = RECT { left: -1920, top: -200, right: 2560, bottom: 1440 };
        assert_eq!(desktop_to_synthetic_pointer(&virt_neg_x, -1500, 300), (420, 500));
    }

    #[test]
    fn synthetic_multi_touch_and_win11_touchpad_init() {
        let mut inj = Injector::new(RECT { left: 0, top: 0, right: 2560, bottom: 1440 });
        assert_ne!(inj.ensure_touch_backend(), TouchBackend::None);
        // 2-finger simultaneous down -> move -> cancel
        inj.touch(0, 0, 1000, 1000);
        inj.touch(1, 0, 2000, 1000);
        inj.touch(0, 1, 1050, 1020);
        inj.touch(1, 1, 2050, 1020);
        inj.touch(1, 3, 2050, 1020);
        inj.touch(0, 3, 1050, 1020);
        // Check Win11 native Precision Touchpad creation & cancel
        let has_ptp = inj.ensure_pad_device().is_some();
        println!("Win11 PT_TOUCHPAD available: {has_ptp}");
        inj.touchpad(0, 0, 0, 1000);
        inj.touchpad(1, 0, 20, 1000);
        inj.touchpad(2, 0, 0, 1000);
    }
}
