//! JNI entrypoints for the Android controller app (`com.rc.android.nativebridge.RcNative`).

mod engine;
mod net_util;
mod session;
mod store;

use std::{
    net::Ipv4Addr,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
};

use jni::{
    JNIEnv, JavaVM,
    objects::{GlobalRef, JClass, JObject, JString, JValue},
    sys::{jboolean, jint, jlong, jstring},
};

use crate::{engine::Engine, session::Callback};

static ENGINE: OnceLock<Mutex<Option<Arc<Engine>>>> = OnceLock::new();

fn engine_slot() -> &'static Mutex<Option<Arc<Engine>>> {
    ENGINE.get_or_init(|| Mutex::new(None))
}

fn get_engine() -> Option<Arc<Engine>> {
    engine_slot().lock().unwrap().clone()
}

struct JniCallback {
    vm: JavaVM,
    obj: GlobalRef,
}

impl JniCallback {
    fn with_env<F: FnOnce(&mut JNIEnv<'_>, &JObject<'_>)>(&self, f: F) {
        let Ok(mut env) = self.vm.attach_current_thread() else {
            return;
        };
        let obj = self.obj.as_obj();
        f(&mut env, obj);
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_describe();
            let _ = env.exception_clear();
        }
    }
}

impl Callback for JniCallback {
    fn on_devices_updated(&self, devices_json: &str) {
        self.with_env(|env, obj| {
            if let Ok(js) = env.new_string(devices_json) {
                let _ = env.call_method(
                    obj,
                    "onDevicesUpdated",
                    "(Ljava/lang/String;)V",
                    &[JValue::Object(&js)],
                );
            }
        });
    }

    fn on_session_state(&self, state: &str, message: &str) {
        self.with_env(|env, obj| {
            if let (Ok(s), Ok(m)) = (env.new_string(state), env.new_string(message)) {
                let _ = env.call_method(
                    obj,
                    "onSessionState",
                    "(Ljava/lang/String;Ljava/lang/String;)V",
                    &[JValue::Object(&s), JValue::Object(&m)],
                );
            }
        });
    }

    fn on_stream_info(&self, width: i32, height: i32, encoder: &str, fps: i32, max_kbps: i32) {
        self.with_env(|env, obj| {
            if let Ok(enc) = env.new_string(encoder) {
                let _ = env.call_method(
                    obj,
                    "onStreamInfo",
                    "(IILjava/lang/String;II)V",
                    &[
                        JValue::Int(width),
                        JValue::Int(height),
                        JValue::Object(&enc),
                        JValue::Int(fps),
                        JValue::Int(max_kbps),
                    ],
                );
            }
        });
    }

    fn on_monitors(&self, monitors_json: &str, current_index: i32) {
        self.with_env(|env, obj| {
            if let Ok(js) = env.new_string(monitors_json) {
                let _ = env.call_method(
                    obj,
                    "onMonitors",
                    "(Ljava/lang/String;I)V",
                    &[JValue::Object(&js), JValue::Int(current_index)],
                );
            }
        });
    }

    fn on_windows(&self, windows_json: &str) {
        self.with_env(|env, obj| {
            if let Ok(js) = env.new_string(windows_json) {
                let _ = env.call_method(
                    obj,
                    "onWindows",
                    "(Ljava/lang/String;)V",
                    &[JValue::Object(&js)],
                );
            }
        });
    }

    fn on_app_icon(&self, app_key: &str, png: &[u8]) {
        self.with_env(|env, obj| {
            if let (Ok(k), Ok(arr)) = (env.new_string(app_key), env.byte_array_from_slice(png)) {
                let _ = env.call_method(
                    obj,
                    "onAppIcon",
                    "(Ljava/lang/String;[B)V",
                    &[JValue::Object(&k), JValue::Object(&arr)],
                );
            }
        });
    }

    fn on_window_thumb(&self, hwnd: u64, png: &[u8]) {
        self.with_env(|env, obj| {
            if let Ok(arr) = env.byte_array_from_slice(png) {
                let _ = env.call_method(
                    obj,
                    "onWindowThumb",
                    "(J[B)V",
                    &[JValue::Long(hwnd as i64), JValue::Object(&arr)],
                );
            }
        });
    }

    fn on_stats(&self, fps: i32, kbps: i32, rtt_ms: i32, path_kind: &str) {
        self.with_env(|env, obj| {
            if let Ok(pk) = env.new_string(path_kind) {
                let _ = env.call_method(
                    obj,
                    "onStats",
                    "(IIILjava/lang/String;)V",
                    &[
                        JValue::Int(fps),
                        JValue::Int(kbps),
                        JValue::Int(rtt_ms),
                        JValue::Object(&pk),
                    ],
                );
            }
        });
    }

    fn on_hint(&self, code: i32, message: &str) {
        self.with_env(|env, obj| {
            if let Ok(msg) = env.new_string(message) {
                let _ = env.call_method(
                    obj,
                    "onHint",
                    "(ILjava/lang/String;)V",
                    &[JValue::Int(code), JValue::Object(&msg)],
                );
            }
        });
    }

    fn on_notice(&self, message: &str) {
        self.with_env(|env, obj| {
            if let Ok(msg) = env.new_string(message) {
                let _ = env.call_method(
                    obj,
                    "onNotice",
                    "(Ljava/lang/String;)V",
                    &[JValue::Object(&msg)],
                );
            }
        });
    }

    fn on_cursor_shape(&self, id: i32, width: i32, height: i32, hot_x: i32, hot_y: i32, rgba: &[u8]) {
        self.with_env(|env, obj| {
            if let Ok(arr) = env.byte_array_from_slice(rgba) {
                let _ = env.call_method(
                    obj,
                    "onCursorShape",
                    "(IIIII[B)V",
                    &[
                        JValue::Int(id),
                        JValue::Int(width),
                        JValue::Int(height),
                        JValue::Int(hot_x),
                        JValue::Int(hot_y),
                        JValue::Object(&arr),
                    ],
                );
            }
        });
    }

    fn on_cursor_use(&self, id: i32) {
        self.with_env(|env, obj| {
            let _ = env.call_method(obj, "onCursorUse", "(I)V", &[JValue::Int(id)]);
        });
    }

    fn on_cursor_hidden(&self) {
        self.with_env(|env, obj| {
            let _ = env.call_method(obj, "onCursorHidden", "()V", &[]);
        });
    }

    fn on_cursor_meta(&self, kind: i32) {
        self.with_env(|env, obj| {
            let _ = env.call_method(obj, "onCursorMeta", "(I)V", &[JValue::Int(kind)]);
        });
    }

    fn on_text_focus(&self, editable: bool) {
        self.with_env(|env, obj| {
            let _ = env.call_method(
                obj,
                "onTextFocus",
                "(Z)V",
                &[JValue::Bool(if editable { 1 } else { 0 })],
            );
        });
    }

    fn on_virtual_display(&self, active: bool, width: i32, height: i32, dpi_percent: i32) {
        self.with_env(|env, obj| {
            let _ = env.call_method(
                obj,
                "onVirtualDisplay",
                "(ZIII)V",
                &[
                    JValue::Bool(if active { 1 } else { 0 }),
                    JValue::Int(width),
                    JValue::Int(height),
                    JValue::Int(dpi_percent),
                ],
            );
        });
    }

    fn on_route_changed(&self, relay_only: bool) {
        self.with_env(|env, obj| {
            let _ = env.call_method(
                obj,
                "onRouteChanged",
                "(Z)V",
                &[JValue::Bool(if relay_only { 1 } else { 0 })],
            );
        });
    }

    fn on_video_frame(
        &self,
        codec: i32,
        key: bool,
        width: i32,
        height: i32,
        host_us: i64,
        data: &[u8],
    ) {
        self.with_env(|env, obj| {
            if let Ok(arr) = env.byte_array_from_slice(data) {
                let _ = env.call_method(
                    obj,
                    "onVideoFrame",
                    "(IZIIJ[B)V",
                    &[
                        JValue::Int(codec),
                        JValue::Bool(if key { 1 } else { 0 }),
                        JValue::Int(width),
                        JValue::Int(height),
                        JValue::Long(host_us),
                        JValue::Object(&arr),
                    ],
                );
            }
        });
    }
}

fn jstr(env: &mut JNIEnv<'_>, s: JString<'_>) -> String {
    env.get_string(&s)
        .map(|g| g.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn to_jstring(env: &mut JNIEnv<'_>, s: &str) -> jstring {
    env.new_string(s)
        .map(|js| js.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativeInit(
    mut env: JNIEnv<'_>,
    _cls: JClass<'_>,
    files_dir: JString<'_>,
    device_name: JString<'_>,
    callback: JObject<'_>,
) -> jstring {
    #[cfg(target_os = "android")]
    {
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(log::LevelFilter::Info)
                .with_tag("rc-android"),
        );
    }

    let dir = PathBuf::from(jstr(&mut env, files_dir));
    let name = jstr(&mut env, device_name);
    let Ok(vm) = env.get_java_vm() else {
        return to_jstring(&mut env, "failed to acquire JavaVM");
    };
    let Ok(global_cb) = env.new_global_ref(callback) else {
        return to_jstring(&mut env, "failed to create global ref");
    };
    let cb = Arc::new(JniCallback { vm, obj: global_cb });

    if let Some(eng) = get_engine() {
        eng.replace_callback(cb);
        return to_jstring(&mut env, "");
    }

    let mut slot = engine_slot().lock().unwrap();
    if let Some(eng) = slot.clone() {
        drop(slot);
        eng.replace_callback(cb);
        return to_jstring(&mut env, "");
    }
    match Engine::start(dir, name, cb) {
        Ok(eng) => {
            *slot = Some(Arc::new(eng));
            to_jstring(&mut env, "")
        }
        Err(e) => to_jstring(&mut env, &format!("{e:#}")),
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativeSetBroadcasts(
    mut env: JNIEnv<'_>,
    _cls: JClass<'_>,
    csv: JString<'_>,
) {
    let raw = jstr(&mut env, csv);
    let addrs: Vec<Ipv4Addr> = raw
        .split([',', ' '])
        .map(str::trim)
        .filter_map(|s| s.parse::<Ipv4Addr>().ok())
        .collect();
    if let Some(eng) = get_engine() {
        eng.set_broadcast_addrs(addrs);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativeSetDeviceName(
    mut env: JNIEnv<'_>,
    _cls: JClass<'_>,
    name: JString<'_>,
) {
    let name = jstr(&mut env, name);
    if let Some(eng) = get_engine() {
        eng.set_device_name(&name);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativeTriggerScan(
    _env: JNIEnv<'_>,
    _cls: JClass<'_>,
) {
    if let Some(eng) = get_engine() {
        eng.trigger_scan();
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativePasteCode(
    mut env: JNIEnv<'_>,
    _cls: JClass<'_>,
    text: JString<'_>,
) -> jstring {
    let code = jstr(&mut env, text);
    let res = match get_engine() {
        Some(eng) => eng.paste_code(&code),
        None => engine::PasteResult {
            ok: false,
            kind: "none".into(),
            message: "引擎尚未初始化".into(),
            device_id: None,
        },
    };
    let json = serde_json::to_string(&res).unwrap_or_else(|_| r#"{"ok":false}"#.into());
    to_jstring(&mut env, &json)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativeConnect(
    mut env: JNIEnv<'_>,
    _cls: JClass<'_>,
    id_hex: JString<'_>,
    fit_w: jint,
    fit_h: jint,
    fps: jint,
    relay_only: jboolean,
) -> jstring {
    let id = jstr(&mut env, id_hex);
    let res = match get_engine() {
        Some(eng) => eng.connect(
            &id,
            fit_w.clamp(0, u16::MAX as i32) as u16,
            fit_h.clamp(0, u16::MAX as i32) as u16,
            fps.clamp(0, 120) as u8,
            relay_only != 0,
        ),
        None => Err(anyhow::anyhow!("引擎尚未初始化")),
    };
    match res {
        Ok(()) => to_jstring(&mut env, ""),
        Err(e) => to_jstring(&mut env, &format!("{e:#}")),
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativeSetRelayOnly(
    _env: JNIEnv<'_>,
    _cls: JClass<'_>,
    relay_only: jboolean,
) {
    if let Some(eng) = get_engine() {
        eng.set_relay_only(relay_only != 0);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativeDisconnect(
    _env: JNIEnv<'_>,
    _cls: JClass<'_>,
) {
    if let Some(eng) = get_engine() {
        eng.disconnect();
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativeSelectMonitor(
    _env: JNIEnv<'_>,
    _cls: JClass<'_>,
    index: jint,
) {
    if let Some(eng) = get_engine() {
        eng.select_monitor(index.clamp(0, 255) as u8);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativeFocusWindow(
    _env: JNIEnv<'_>,
    _cls: JClass<'_>,
    hwnd: jlong,
    view_width: jint,
    view_height: jint,
) {
    if let Some(eng) = get_engine() {
        eng.focus_window(
            hwnd as u64,
            view_width.clamp(0, u16::MAX as i32) as u16,
            view_height.clamp(0, u16::MAX as i32) as u16,
        );
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativeSendText(
    mut env: JNIEnv<'_>,
    _cls: JClass<'_>,
    text: JString<'_>,
) {
    let s = jstr(&mut env, text);
    if !s.is_empty()
        && let Some(eng) = get_engine()
    {
        eng.send_text(&s);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativeRequestKeyframe(
    _env: JNIEnv<'_>,
    _cls: JClass<'_>,
) {
    if let Some(eng) = get_engine() {
        eng.request_keyframe();
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativeSetQuality(
    _env: JNIEnv<'_>,
    _cls: JClass<'_>,
    fit_w: jint,
    fit_h: jint,
    short_side: jint,
    fps: jint,
    kbps: jint,
    yuv444: jboolean,
    auto_fps: jboolean,
) {
    if let Some(eng) = get_engine() {
        eng.set_quality(
            fit_w.clamp(0, u16::MAX as i32) as u16,
            fit_h.clamp(0, u16::MAX as i32) as u16,
            short_side.clamp(0, u16::MAX as i32) as u16,
            fps.clamp(0, u8::MAX as i32) as u8,
            kbps.max(0) as u32,
            yuv444 != 0,
            auto_fps != 0,
        );
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativeSendMouseMove(
    _env: JNIEnv<'_>,
    _cls: JClass<'_>,
    x: jint,
    y: jint,
) {
    if let Some(eng) = get_engine() {
        eng.send_mouse_move(x.clamp(0, 65535) as u16, y.clamp(0, 65535) as u16);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativeSendMouseButton(
    _env: JNIEnv<'_>,
    _cls: JClass<'_>,
    button: jint,
    down: jboolean,
) {
    if let Some(eng) = get_engine() {
        eng.send_mouse_button(button.clamp(0, 4) as u8, down != 0);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativeSendTouch(
    _env: JNIEnv<'_>,
    _cls: JClass<'_>,
    id: jint,
    phase: jint,
    x: jint,
    y: jint,
) {
    if let Some(eng) = get_engine() {
        eng.send_touch(
            id.clamp(0, 9) as u8,
            phase.clamp(0, 3) as u8,
            x.clamp(0, 65535) as u16,
            y.clamp(0, 65535) as u16,
        );
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativeSendTouchpad(
    _env: JNIEnv<'_>,
    _cls: JClass<'_>,
    phase: jint,
    dx: jint,
    dy: jint,
    scale_milli: jint,
) {
    if let Some(eng) = get_engine() {
        eng.send_touchpad(
            phase.clamp(0, 5) as u8,
            dx.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            dy.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            scale_milli.clamp(0, u16::MAX as i32) as u16,
        );
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativeSendPen(
    _env: JNIEnv<'_>,
    _cls: JClass<'_>,
    phase: jint,
    x: jint,
    y: jint,
    pressure: jint,
    tilt_x: jint,
    tilt_y: jint,
    flags: jint,
) {
    if let Some(eng) = get_engine() {
        eng.send_pen(
            phase.clamp(0, 4) as u8,
            x.clamp(0, 65535) as u16,
            y.clamp(0, 65535) as u16,
            pressure.clamp(0, 1024) as u16,
            tilt_x.clamp(-90, 90) as i8,
            tilt_y.clamp(-90, 90) as i8,
            flags.clamp(0, 255) as u8,
        );
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativeSendMouseDelta(
    _env: JNIEnv<'_>,
    _cls: JClass<'_>,
    dx: jint,
    dy: jint,
) {
    if let Some(eng) = get_engine() {
        eng.send_mouse_delta(
            dx.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            dy.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        );
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativeSetVirtualDisplay(
    _env: JNIEnv<'_>,
    _cls: JClass<'_>,
    active: jboolean,
    width: jint,
    height: jint,
    dpi_percent: jint,
) {
    if let Some(eng) = get_engine() {
        eng.set_virtual_display(
            active != 0,
            width.clamp(0, u16::MAX as i32) as u16,
            height.clamp(0, u16::MAX as i32) as u16,
            dpi_percent.clamp(0, u16::MAX as i32) as u16,
        );
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativeSendWheel(
    _env: JNIEnv<'_>,
    _cls: JClass<'_>,
    delta: jint,
    horizontal: jboolean,
) {
    if let Some(eng) = get_engine() {
        eng.send_wheel(
            delta.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            horizontal != 0,
        );
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativeSendKey(
    _env: JNIEnv<'_>,
    _cls: JClass<'_>,
    scan: jint,
    down: jboolean,
) {
    if let Some(eng) = get_engine() {
        eng.send_key(scan.clamp(0, 0xFFFF) as u16, down != 0);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativeReleaseAll(
    _env: JNIEnv<'_>,
    _cls: JClass<'_>,
) {
    if let Some(eng) = get_engine() {
        eng.send_release_all();
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativeRememberDevice(
    mut env: JNIEnv<'_>,
    _cls: JClass<'_>,
    id_hex: JString<'_>,
) {
    let id = jstr(&mut env, id_hex);
    if let Some(eng) = get_engine() {
        eng.remember_device(&id);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativeForgetDevice(
    mut env: JNIEnv<'_>,
    _cls: JClass<'_>,
    id_hex: JString<'_>,
) {
    let id = jstr(&mut env, id_hex);
    if let Some(eng) = get_engine() {
        eng.forget_device(&id);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativeAgreeRemember(
    mut env: JNIEnv<'_>,
    _cls: JClass<'_>,
    id_hex: JString<'_>,
) {
    let id = jstr(&mut env, id_hex);
    if let Some(eng) = get_engine() {
        eng.agree_remember(&id);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativeRefuseRemember(
    mut env: JNIEnv<'_>,
    _cls: JClass<'_>,
    id_hex: JString<'_>,
) {
    let id = jstr(&mut env, id_hex);
    if let Some(eng) = get_engine() {
        eng.refuse_remember(&id);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativeCancelRemember(
    mut env: JNIEnv<'_>,
    _cls: JClass<'_>,
    id_hex: JString<'_>,
) {
    let id = jstr(&mut env, id_hex);
    if let Some(eng) = get_engine() {
        eng.cancel_remember(&id);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rc_android_nativebridge_RcNative_nativeRetryDevice(
    mut env: JNIEnv<'_>,
    _cls: JClass<'_>,
    id_hex: JString<'_>,
) {
    let id = jstr(&mut env, id_hex);
    if let Some(eng) = get_engine() {
        eng.retry_device(&id);
    }
}
