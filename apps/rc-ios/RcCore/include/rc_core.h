// C interface of the Rust controller engine (crates/rc-ios). Keep in sync with crates/rc-ios/src/lib.rs.
//
// Strings passed in are NUL-terminated UTF-8, borrowed for the call only.
// Strings and buffers handed to callbacks are valid only during the callback.
// Strings returned by rc_* functions must be released with rc_string_free.
// Callbacks run on engine threads, never on the main thread.

#ifndef RC_CORE_H
#define RC_CORE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct RcCallbacks {
    void *ctx;
    void (*on_devices_updated)(void *ctx, const char *devices_json);
    void (*on_session_state)(void *ctx, const char *state, const char *message);
    void (*on_stream_info)(void *ctx, int32_t width, int32_t height, const char *encoder, int32_t fps, int32_t max_kbps);
    void (*on_monitors)(void *ctx, const char *monitors_json, int32_t current_index);
    void (*on_windows)(void *ctx, const char *windows_json);
    void (*on_app_icon)(void *ctx, const char *app_key, const uint8_t *png, size_t len);
    void (*on_window_thumb)(void *ctx, uint64_t hwnd, const uint8_t *png, size_t len);
    void (*on_stats)(void *ctx, int32_t fps, int32_t kbps, int32_t rtt_ms, const char *path_kind);
    void (*on_hint)(void *ctx, int32_t code, const char *message);
    void (*on_notice)(void *ctx, const char *message);
    /// Premultiplied RGBA8888, width * height * 4 bytes.
    void (*on_cursor_shape)(void *ctx, int32_t id, int32_t width, int32_t height, int32_t hot_x, int32_t hot_y,
                            const uint8_t *rgba, size_t len);
    void (*on_cursor_use)(void *ctx, int32_t id);
    void (*on_cursor_hidden)(void *ctx);
    void (*on_cursor_meta)(void *ctx, int32_t kind);
    void (*on_text_focus)(void *ctx, bool editable);
    void (*on_virtual_display)(void *ctx, bool active, int32_t width, int32_t height, int32_t dpi_percent);
    void (*on_route_changed)(void *ctx, bool relay_only);
    /// codec: 0 = H.264, 1 = HEVC. data is one Annex B access unit.
    void (*on_video_frame)(void *ctx, int32_t codec, bool key, int32_t width, int32_t height, int64_t host_us,
                           const uint8_t *data, size_t len);
} RcCallbacks;

void rc_string_free(char *s);

/// Returns "" on success, otherwise the error text.
char *rc_init(const char *files_dir, const char *device_name, RcCallbacks callbacks);
void rc_set_broadcasts(const char *csv);
void rc_set_device_name(const char *name);
void rc_trigger_scan(void);
/// Returns JSON {"ok":bool,"kind":str,"message":str,"device_id":str|null}.
char *rc_paste_code(const char *text);
/// Returns "" on success, otherwise the error text.
char *rc_connect(const char *id_hex, int32_t fit_w, int32_t fit_h, int32_t fps, bool relay_only);
void rc_set_relay_only(bool relay_only);
void rc_disconnect(void);
void rc_select_monitor(int32_t index);
void rc_focus_window(uint64_t hwnd, int32_t view_width, int32_t view_height);
void rc_set_virtual_display(bool active, int32_t width, int32_t height, int32_t dpi_percent);
void rc_request_keyframe(void);
void rc_set_quality(int32_t fit_w, int32_t fit_h, int32_t short_side, int32_t fps, int32_t kbps, bool yuv444,
                    bool auto_fps);

/// x, y: 0..65535 across the streamed monitor.
void rc_send_mouse_move(int32_t x, int32_t y);
void rc_send_mouse_delta(int32_t dx, int32_t dy);
/// button: 0 left, 1 right.
void rc_send_mouse_button(int32_t button, bool down);
/// phase: 0 down, 1 move, 2 up, 3 cancel.
void rc_send_touch(int32_t id, int32_t phase, int32_t x, int32_t y);
void rc_send_touchpad(int32_t phase, int32_t dx, int32_t dy, int32_t scale_milli);
void rc_send_pen(int32_t phase, int32_t x, int32_t y, int32_t pressure, int32_t tilt_x, int32_t tilt_y, int32_t flags);
/// delta: 120 per notch, positive = up (or right when horizontal).
void rc_send_wheel(int32_t delta, bool horizontal);
/// scan: PC set 1 scan code, 0xE0xx for extended keys.
void rc_send_key(int32_t scan, bool down);
void rc_send_text(const char *text);
void rc_release_all(void);

void rc_remember_device(const char *id_hex);
void rc_forget_device(const char *id_hex);
void rc_agree_remember(const char *id_hex);
void rc_refuse_remember(const char *id_hex);
void rc_cancel_remember(const char *id_hex);
void rc_retry_device(const char *id_hex);

#ifdef __cplusplus
}
#endif

#endif
