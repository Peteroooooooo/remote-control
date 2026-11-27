//! Wire protocol shared between desktop host/viewer and mobile controllers.
//! All integers are big-endian. Byte-for-byte compatible with `apps/remote-control/src/proto.rs`.

use anyhow::{Result, bail};

pub const ALPN: &[u8] = b"rc/desktop/3";
pub const MEET_ALPN: &[u8] = b"rc/meet/1";

pub const MAX_MSG: usize = 1 << 20;

pub const STREAM_CLIPBOARD: u8 = 1;
pub const STREAM_FILE: u8 = 2;
pub const STREAM_AUDIO: u8 = 3;

pub const FRAME_HDR: usize = 1 + 2 + 2 + 4;
const F_KEY: u8 = 1;
const F_HEVC: u8 = 2;
const F_444: u8 = 4;

pub const CAN_HEVC: u8 = 1;
pub const CAN_HEVC444: u8 = 2;

pub const EVENT_NONE: u8 = 0;
pub const EVENT_REMEMBER: u8 = 1;
pub const EVENT_FORGET: u8 = 2;
pub const EVENT_ASK: u8 = 3;
pub const EVENT_NO: u8 = 4;

pub const REFUSE_UNKNOWN: u8 = 1;
pub const REFUSE_FORGOTTEN: u8 = 2;
pub const REFUSE_CODE_EXPIRED: u8 = 3;
pub const REFUSE_CODE_USED: u8 = 4;
pub const REFUSE_CODE_UNKNOWN: u8 = 5;

pub const CLOSE_NOT_MET: u32 = 1;
pub const CLOSE_BUSY: u32 = 2;
pub const CLOSE_SHUTDOWN: u32 = 3;
pub const CLOSE_STOPPED: u32 = 4;
pub const CLOSE_DENIED: u32 = 5;
pub const CLOSE_NO_ANSWER: u32 = 6;
pub const CLOSE_LOCKED: u32 = 7;
pub const CLOSE_NO_DIRECT: u32 = 8;
pub const CLOSE_DISCONNECTED: u32 = 9;
pub const CLOSE_HANDOFF: u32 = 10;

pub const HINT_NONE: u8 = 0;
pub const HINT_NO_DISPLAY: u8 = 1;
pub const HINT_UAC: u8 = 2;
pub const HINT_LOCKED: u8 = 3;
pub const HINT_NO_SAS: u8 = 10;

pub const MAX_CURSOR_SIDE: u16 = 256;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CursorImage {
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    pub color: Vec<u8>,
    pub mask: Vec<u8>,
}

impl CursorImage {
    pub fn stride(width: u16) -> usize {
        (width as usize).div_ceil(16) * 2
    }

    pub fn is_color(&self) -> bool {
        !self.color.is_empty()
    }

    pub fn mask_rows(&self) -> usize {
        if self.is_color() {
            self.height as usize
        } else {
            self.height as usize * 2
        }
    }

    pub fn valid(&self) -> bool {
        if self.width == 0
            || self.height == 0
            || self.width > MAX_CURSOR_SIDE
            || self.height > MAX_CURSOR_SIDE
        {
            return false;
        }
        let mask_ok = self.mask.len() == Self::stride(self.width) * self.mask_rows();
        let color_ok =
            self.color.is_empty() || self.color.len() == self.width as usize * self.height as usize * 4;
        mask_ok && color_ok
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Quality {
    pub fit_w: u16,
    pub fit_h: u16,
    pub short: u16,
    pub fps: u8,
    pub kbps: u32,
    pub yuv444: bool,
    pub auto_fps: bool,
}

impl Quality {
    fn write(&self, w: &mut W) {
        w.u16(self.fit_w)
            .u16(self.fit_h)
            .u16(self.short)
            .u8(self.fps)
            .u32(self.kbps)
            .u8(self.yuv444 as u8)
            .u8(self.auto_fps as u8);
    }

    fn read(r: &mut R) -> Result<Quality> {
        let mut q = Quality {
            fit_w: r.u16()?,
            fit_h: r.u16()?,
            short: r.u16()?,
            fps: r.u8()?,
            kbps: r.u32()?,
            yuv444: false,
            auto_fps: false,
        };
        q.yuv444 = r.u8()? != 0;
        if let Ok(b) = r.u8() {
            q.auto_fps = b != 0;
        }
        Ok(q)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Streaming {
    pub fps: u8,
    pub max_kbps: u32,
    pub can444: bool,
    pub yuv444: bool,
    pub resends: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioCfg {
    pub enabled: bool,
    pub channels: u8,
    pub mute_host: bool,
    pub lossless: bool,
    pub max_rate_hz: u32,
}

impl Default for AudioCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            channels: 2,
            mute_host: false,
            lossless: false,
            max_rate_hz: 48_000,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    pub key: bool,
    pub hevc: bool,
    pub yuv444: bool,
    pub width: u16,
    pub height: u16,
    pub host_us: u32,
}

impl FrameHeader {
    pub fn write(&self, out: &mut Vec<u8>) {
        out.push(
            if self.key { F_KEY } else { 0 }
                | if self.hevc { F_HEVC } else { 0 }
                | if self.yuv444 { F_444 } else { 0 },
        );
        out.extend_from_slice(&self.width.to_be_bytes());
        out.extend_from_slice(&self.height.to_be_bytes());
        out.extend_from_slice(&self.host_us.to_be_bytes());
    }

    pub fn parse(b: &[u8]) -> Option<(FrameHeader, &[u8])> {
        if b.len() < FRAME_HDR {
            return None;
        }
        let h = FrameHeader {
            key: b[0] & F_KEY != 0,
            hevc: b[0] & F_HEVC != 0,
            yuv444: b[0] & F_444 != 0,
            width: u16::from_be_bytes([b[1], b[2]]),
            height: u16::from_be_bytes([b[3], b[4]]),
            host_us: u32::from_be_bytes([b[5], b[6], b[7], b[8]]),
        };
        Some((h, &b[FRAME_HDR..]))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostStats {
    pub fps: u16,
    pub kbps: u32,
    pub skipped_pm: u16,
    pub shrunk_pm: u16,
    pub frame_rtt_p50_us: u32,
    pub frame_rtt_p95_us: u32,
    pub host_p50_us: u32,
    pub host_p95_us: u32,
    pub encode_p50_us: u32,
    pub encode_p95_us: u32,
    pub path: u8,
    pub path_rtt_us: u32,
    pub rate_kbps: u32,
    pub size_p50_pct: u16,
    pub size_max_pct: u16,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MonitorDesc {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
    pub rotation: u16,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WindowDesc {
    pub hwnd: u64,
    pub app_key: String,
    pub app_name: String,
    pub title: String,
    pub monitor: u8,
    pub minimized: bool,
    /// Normalized `0..=65535` bounds relative to `monitor` (`0` = unknown).
    pub rx: u16,
    pub ry: u16,
    pub rw: u16,
    pub rh: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Msg {
    Hello {
        max_kbps: u32,
        fps: u8,
        caps: u8,
        proof: String,
        quality: Option<Quality>,
    },
    Quality(Quality),
    Waiting,
    Paused {
        on: bool,
    },
    MouseMove {
        x: u16,
        y: u16,
    },
    MouseButton {
        button: u8,
        down: bool,
    },
    Wheel {
        delta: i16,
        horizontal: bool,
    },
    Key {
        scan: u16,
        down: bool,
    },
    TextInput {
        text: String,
    },
    TextFocus {
        editable: bool,
    },
    Touch {
        id: u8,
        phase: u8,
        x: u16,
        y: u16,
    },
    /// Precision touchpad gesture (`phase`: 0 scroll_start, 1 scroll_move, 2 scroll_end,
    /// 3 pinch_start, 4 pinch_update, 5 pinch_end).
    Touchpad {
        phase: u8,
        dx: i16,
        dy: i16,
        scale_milli: u16,
    },
    /// Stylus/pen input (`phase`: 0 hover, 1 down, 2 move, 3 up, 4 cancel; `flags`: bit0 barrel, bit1 eraser).
    Pen {
        phase: u8,
        x: u16,
        y: u16,
        pressure: u16,
        tilt_x: i8,
        tilt_y: i8,
        flags: u8,
    },
    /// External physical mouse relative delta (Android `requestPointerCapture`).
    MouseDelta {
        dx: i16,
        dy: i16,
    },
    ReleaseAll,
    RequestKeyframe,
    Ping {
        t_us: u64,
    },
    Pong {
        t_us: u64,
    },
    Info {
        width: u16,
        height: u16,
        encoder: String,
        streaming: Option<Streaming>,
    },
    Stats(HostStats),
    Probe {
        x: u16,
        y: u16,
    },
    ProbeReply {
        x: u16,
        y: u16,
    },
    SendSas,
    Desktop {
        name: String,
    },
    CursorShape {
        id: u32,
        image: CursorImage,
    },
    CursorUse {
        id: u32,
    },
    CursorHidden,
    /// Host -> viewer: semantic cursor kind (`0` normal, `1` interactive/hand/resize, `2` text/ibeam).
    CursorMeta {
        kind: u8,
    },
    ClipOffer {
        seq: u32,
        seen: u32,
        format: u8,
        bytes: u32,
    },
    MeetHi {
        name: String,
        home: String,
        token: Option<[u8; 8]>,
        event: u8,
        grant: Option<String>,
        /// What the sender is ([`PLATFORM_PC`] when not said).
        platform: u8,
    },
    MeetWelcome {
        name: String,
        home: String,
        grant: Option<String>,
        event: u8,
        /// What the sender is ([`PLATFORM_PC`] when not said).
        platform: u8,
    },
    MeetRefuse {
        reason: u8,
    },
    Monitors {
        list: Vec<MonitorDesc>,
        current: u8,
    },
    SelectMonitor {
        index: u8,
    },
    Windows {
        list: Vec<WindowDesc>,
    },
    FocusWindow {
        hwnd: u64,
        vw: u16,
        vh: u16,
    },
    AppIcon {
        app_key: String,
        png: Vec<u8>,
    },
    WindowThumb {
        hwnd: u64,
        png: Vec<u8>,
    },
    /// Bidirectional: request (client -> host) or report (host -> client) phone virtual display state.
    VirtualDisplay {
        active: bool,
        width: u16,
        height: u16,
        dpi_percent: u16,
    },
    Hint {
        what: u8,
    },
    Audio(AudioCfg),
    InputTick {
        ms: u32,
    },
    Throttle {
        fps: u8,
    },
}

struct W(Vec<u8>);

impl W {
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn str(&mut self, v: &str) -> &mut Self {
        let mut n = v.len().min(200);
        while !v.is_char_boundary(n) {
            n -= 1;
        }
        self.u8(n as u8);
        self.0.extend_from_slice(&v.as_bytes()[..n]);
        self
    }
    fn long_str(&mut self, v: &str) -> &mut Self {
        let mut n = v.len().min(u16::MAX as usize);
        while !v.is_char_boundary(n) {
            n -= 1;
        }
        self.u16(n as u16);
        self.0.extend_from_slice(&v.as_bytes()[..n]);
        self
    }
    fn grant(&mut self, g: &Option<String>) -> &mut Self {
        if let Some(g) = g {
            self.u8(GRANT_MARK).long_str(g);
        }
        self
    }
    fn platform(&mut self, p: u8) -> &mut Self {
        if p != PLATFORM_PC {
            self.u8(PLATFORM_MARK).u8(p);
        }
        self
    }
}

const GRANT_MARK: u8 = 0xA7;
const EVENT_MARK: u8 = 0xA8;
const QUALITY_MARK: u8 = 0xA9;
const STREAMING_MARK: u8 = 0xAA;
/// Starts the platform byte at the very end of a meet hello or welcome.
/// Older versions stop reading before it.
const PLATFORM_MARK: u8 = 0xAB;

/// A computer, or a sender too old to say.
pub const PLATFORM_PC: u8 = 0;
/// An Android phone.
pub const PLATFORM_ANDROID: u8 = 1;

struct R<'a>(&'a [u8]);

impl R<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N]> {
        if self.0.len() < N {
            bail!("message too short");
        }
        let (a, b) = self.0.split_at(N);
        self.0 = b;
        Ok(a.try_into().expect("split at N"))
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take::<1>()?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take()?))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take()?))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take()?))
    }
    fn bytes(&mut self) -> Result<Vec<u8>> {
        let n = self.u32()? as usize;
        if self.0.len() < n {
            bail!("message too short");
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a.to_vec())
    }
    fn str(&mut self) -> Result<String> {
        let n = self.u8()? as usize;
        self.text(n)
    }
    fn long_str(&mut self) -> Result<String> {
        let n = self.u16()? as usize;
        self.text(n)
    }
    fn grant(&mut self) -> Result<Option<String>> {
        if self.0.first() != Some(&GRANT_MARK) {
            return Ok(None);
        }
        self.u8()?;
        Ok(Some(self.long_str()?))
    }
    /// The platform at the end of a meet message; [`PLATFORM_PC`] without one.
    fn platform(&mut self) -> u8 {
        match self.0 {
            [PLATFORM_MARK, p, rest @ ..] => {
                let p = *p;
                self.0 = rest;
                p
            }
            _ => PLATFORM_PC,
        }
    }
    fn text(&mut self, n: usize) -> Result<String> {
        if self.0.len() < n {
            bail!("message too short");
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(String::from_utf8_lossy(a).into_owned())
    }
}

impl Msg {
    fn tag(&self) -> u8 {
        match self {
            Msg::Hello { .. } => 1,
            Msg::MouseMove { .. } => 2,
            Msg::MouseButton { .. } => 3,
            Msg::Wheel { .. } => 4,
            Msg::Key { .. } => 5,
            Msg::ReleaseAll => 6,
            Msg::RequestKeyframe => 7,
            Msg::Ping { .. } => 8,
            Msg::Pong { .. } => 9,
            Msg::Info { .. } => 10,
            Msg::Stats(_) => 11,
            Msg::Probe { .. } => 12,
            Msg::ProbeReply { .. } => 13,
            Msg::Waiting => 14,
            Msg::Paused { .. } => 15,
            Msg::Hint { .. } => 16,
            Msg::Quality(_) => 17,
            Msg::InputTick { .. } => 18,
            Msg::Throttle { .. } => 19,
            Msg::ClipOffer { .. } => 20,
            Msg::CursorShape { .. } => 30,
            Msg::CursorUse { .. } => 31,
            Msg::CursorHidden => 32,
            Msg::CursorMeta { .. } => 33,
            Msg::TextInput { .. } => 40,
            Msg::TextFocus { .. } => 41,
            Msg::Touch { .. } => 42,
            Msg::Touchpad { .. } => 43,
            Msg::Pen { .. } => 44,
            Msg::MouseDelta { .. } => 45,
            Msg::SendSas => 50,
            Msg::Desktop { .. } => 51,
            Msg::MeetHi { .. } => 60,
            Msg::MeetWelcome { .. } => 61,
            Msg::MeetRefuse { .. } => 62,
            Msg::Monitors { .. } => 70,
            Msg::SelectMonitor { .. } => 71,
            Msg::Windows { .. } => 72,
            Msg::FocusWindow { .. } => 73,
            Msg::AppIcon { .. } => 74,
            Msg::WindowThumb { .. } => 75,
            Msg::VirtualDisplay { .. } => 76,
            Msg::Audio(_) => 80,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = W(vec![0, 0, 0, 0, self.tag()]);
        match self {
            Msg::Hello {
                max_kbps,
                fps,
                caps,
                proof,
                quality,
            } => {
                w.u32(*max_kbps).u8(*fps).u8(*caps).long_str(proof);
                if let Some(q) = quality {
                    w.u8(QUALITY_MARK);
                    q.write(&mut w);
                }
            }
            Msg::Quality(q) => q.write(&mut w),
            Msg::Waiting => {}
            Msg::Paused { on } => {
                w.u8(*on as u8);
            }
            Msg::MouseMove { x, y } | Msg::Probe { x, y } | Msg::ProbeReply { x, y } => {
                w.u16(*x).u16(*y);
            }
            Msg::MouseButton { button, down } => {
                w.u8(*button).u8(*down as u8);
            }
            Msg::Wheel { delta, horizontal } => {
                w.u16(*delta as u16).u8(*horizontal as u8);
            }
            Msg::Key { scan, down } => {
                w.u16(*scan).u8(*down as u8);
            }
            Msg::TextInput { text } => {
                w.long_str(text);
            }
            Msg::TextFocus { editable } => {
                w.u8(*editable as u8);
            }
            Msg::Touch { id, phase, x, y } => {
                w.u8(*id).u8(*phase).u16(*x).u16(*y);
            }
            Msg::Touchpad {
                phase,
                dx,
                dy,
                scale_milli,
            } => {
                w.u8(*phase)
                    .u16(*dx as u16)
                    .u16(*dy as u16)
                    .u16(*scale_milli);
            }
            Msg::Pen {
                phase,
                x,
                y,
                pressure,
                tilt_x,
                tilt_y,
                flags,
            } => {
                w.u8(*phase)
                    .u16(*x)
                    .u16(*y)
                    .u16(*pressure)
                    .u8(*tilt_x as u8)
                    .u8(*tilt_y as u8)
                    .u8(*flags);
            }
            Msg::MouseDelta { dx, dy } => {
                w.u16(*dx as u16).u16(*dy as u16);
            }
            Msg::CursorMeta { kind } => {
                w.u8(*kind);
            }
            Msg::ReleaseAll | Msg::RequestKeyframe | Msg::SendSas | Msg::CursorHidden => {}
            Msg::Ping { t_us } | Msg::Pong { t_us } => {
                w.u64(*t_us);
            }
            Msg::Info {
                width,
                height,
                encoder,
                streaming,
            } => {
                let name = &encoder.as_bytes()[..encoder.len().min(200)];
                w.u16(*width).u16(*height).u8(name.len() as u8);
                w.0.extend_from_slice(name);
                if let Some(s) = streaming {
                    w.u8(STREAMING_MARK)
                        .u8(s.fps)
                        .u32(s.max_kbps)
                        .u8(s.can444 as u8 | (s.yuv444 as u8) << 1 | (s.resends as u8) << 2);
                }
            }
            Msg::Desktop { name } => {
                let name = &name.as_bytes()[..name.len().min(200)];
                w.u8(name.len() as u8);
                w.0.extend_from_slice(name);
            }
            Msg::MeetHi {
                name,
                home,
                token,
                event,
                grant,
                platform,
            } => {
                w.str(name)
                    .long_str(home)
                    .u8(*event)
                    .u8(token.is_some() as u8);
                if let Some(t) = token {
                    w.0.extend_from_slice(t);
                }
                w.grant(grant).platform(*platform);
            }
            Msg::MeetWelcome {
                name,
                home,
                grant,
                event,
                platform,
            } => {
                w.str(name).long_str(home).grant(grant);
                if *event != 0 {
                    w.u8(EVENT_MARK).u8(*event);
                }
                w.platform(*platform);
            }
            Msg::MeetRefuse { reason } => {
                w.u8(*reason);
            }
            Msg::Stats(s) => {
                w.u16(s.fps)
                    .u32(s.kbps)
                    .u16(s.skipped_pm)
                    .u16(s.shrunk_pm);
                w.u32(s.frame_rtt_p50_us)
                    .u32(s.frame_rtt_p95_us)
                    .u32(s.host_p50_us)
                    .u32(s.host_p95_us);
                w.u32(s.encode_p50_us).u32(s.encode_p95_us);
                w.u8(s.path)
                    .u32(s.path_rtt_us)
                    .u32(s.rate_kbps)
                    .u16(s.size_p50_pct)
                    .u16(s.size_max_pct);
            }
            Msg::CursorShape { id, image: i } => {
                w.u32(*id)
                    .u16(i.width)
                    .u16(i.height)
                    .u16(i.hot_x)
                    .u16(i.hot_y);
                w.u32(i.mask.len() as u32).0.extend_from_slice(&i.mask);
                w.u32(i.color.len() as u32).0.extend_from_slice(&i.color);
            }
            Msg::CursorUse { id } => {
                w.u32(*id);
            }
            Msg::ClipOffer {
                seq,
                seen,
                format,
                bytes,
            } => {
                w.u32(*seq).u32(*seen).u8(*format).u32(*bytes);
            }
            Msg::Monitors { list, current } => {
                let list = &list[..list.len().min(255)];
                w.u8(*current).u8(list.len() as u8);
                for m in list {
                    w.u32(m.x as u32)
                        .u32(m.y as u32)
                        .u32(m.width)
                        .u32(m.height);
                    w.u8(m.primary as u8).u16(m.rotation).str(&m.name);
                }
            }
            Msg::SelectMonitor { index } | Msg::Hint { what: index } => {
                w.u8(*index);
            }
            Msg::Windows { list } => {
                let list = &list[..list.len().min(64)];
                w.u8(list.len() as u8);
                for win in list {
                    w.u64(win.hwnd)
                        .str(&win.app_key)
                        .str(&win.app_name)
                        .str(&win.title)
                        .u8(win.monitor)
                        .u8(win.minimized as u8)
                        .u16(win.rx)
                        .u16(win.ry)
                        .u16(win.rw)
                        .u16(win.rh);
                }
            }
            Msg::FocusWindow { hwnd, vw, vh } => {
                w.u64(*hwnd).u16(*vw).u16(*vh);
            }
            Msg::AppIcon { app_key, png } => {
                w.str(app_key).u32(png.len() as u32).0.extend_from_slice(png);
            }
            Msg::WindowThumb { hwnd, png } => {
                w.u64(*hwnd).u32(png.len() as u32).0.extend_from_slice(png);
            }
            Msg::VirtualDisplay {
                active,
                width,
                height,
                dpi_percent,
            } => {
                w.u8(*active as u8)
                    .u16(*width)
                    .u16(*height)
                    .u16(*dpi_percent);
            }
            Msg::Audio(c) => {
                w.u8(c.enabled as u8)
                    .u8(c.channels.clamp(1, 2))
                    .u8(c.mute_host as u8)
                    .u8(c.lossless as u8)
                    .u32(c.max_rate_hz);
            }
            Msg::InputTick { ms } => {
                w.u32(*ms);
            }
            Msg::Throttle { fps } => {
                w.u8(*fps);
            }
        }
        let len = (w.0.len() - 4) as u32;
        w.0[..4].copy_from_slice(&len.to_be_bytes());
        w.0
    }

    pub fn decode(b: &[u8]) -> Result<Msg> {
        let (&tag, body) = b
            .split_first()
            .ok_or_else(|| anyhow::anyhow!("empty message"))?;
        let mut r = R(body);
        Ok(match tag {
            1 => {
                let (max_kbps, fps, caps, proof) = (r.u32()?, r.u8()?, r.u8()?, r.long_str()?);
                let quality = if r.0.first() == Some(&QUALITY_MARK) {
                    r.u8()?;
                    Some(Quality::read(&mut r)?)
                } else {
                    None
                };
                Msg::Hello {
                    max_kbps,
                    fps,
                    caps,
                    proof,
                    quality,
                }
            }
            2 => Msg::MouseMove {
                x: r.u16()?,
                y: r.u16()?,
            },
            3 => Msg::MouseButton {
                button: r.u8()?,
                down: r.u8()? != 0,
            },
            4 => Msg::Wheel {
                delta: r.u16()? as i16,
                horizontal: r.u8()? != 0,
            },
            5 => Msg::Key {
                scan: r.u16()?,
                down: r.u8()? != 0,
            },
            6 => Msg::ReleaseAll,
            7 => Msg::RequestKeyframe,
            8 => Msg::Ping { t_us: r.u64()? },
            9 => Msg::Pong { t_us: r.u64()? },
            10 => {
                let (width, height, n) = (r.u16()?, r.u16()?, r.u8()? as usize);
                let encoder = r.text(n)?;
                let streaming = if r.0.first() == Some(&STREAMING_MARK) {
                    r.u8()?;
                    let (fps, max_kbps, f) = (r.u8()?, r.u32()?, r.u8()?);
                    Some(Streaming {
                        fps,
                        max_kbps,
                        can444: f & 1 != 0,
                        yuv444: f & 2 != 0,
                        resends: f & 4 != 0,
                    })
                } else {
                    None
                };
                Msg::Info {
                    width,
                    height,
                    encoder,
                    streaming,
                }
            }
            11 => Msg::Stats(HostStats {
                fps: r.u16()?,
                kbps: r.u32()?,
                skipped_pm: r.u16()?,
                shrunk_pm: r.u16()?,
                frame_rtt_p50_us: r.u32()?,
                frame_rtt_p95_us: r.u32()?,
                host_p50_us: r.u32()?,
                host_p95_us: r.u32()?,
                encode_p50_us: r.u32()?,
                encode_p95_us: r.u32()?,
                path: r.u8()?,
                path_rtt_us: r.u32()?,
                rate_kbps: r.u32()?,
                size_p50_pct: r.u16()?,
                size_max_pct: r.u16()?,
            }),
            12 => Msg::Probe {
                x: r.u16()?,
                y: r.u16()?,
            },
            13 => Msg::ProbeReply {
                x: r.u16()?,
                y: r.u16()?,
            },
            14 => Msg::Waiting,
            15 => Msg::Paused { on: r.u8()? != 0 },
            16 => Msg::Hint { what: r.u8()? },
            17 => Msg::Quality(Quality::read(&mut r)?),
            18 => Msg::InputTick { ms: r.u32()? },
            19 => Msg::Throttle { fps: r.u8()? },
            20 => Msg::ClipOffer {
                seq: r.u32()?,
                seen: r.u32()?,
                format: r.u8()?,
                bytes: r.u32()?,
            },
            30 => {
                let id = r.u32()?;
                let (width, height, hot_x, hot_y) = (r.u16()?, r.u16()?, r.u16()?, r.u16()?);
                let mask = r.bytes()?;
                let color = r.bytes()?;
                let image = CursorImage {
                    width,
                    height,
                    hot_x,
                    hot_y,
                    color,
                    mask,
                };
                if !image.valid() {
                    bail!("bad cursor image {width}x{height}");
                }
                Msg::CursorShape { id, image }
            }
            31 => Msg::CursorUse { id: r.u32()? },
            32 => Msg::CursorHidden,
            33 => Msg::CursorMeta { kind: r.u8()? },
            40 => Msg::TextInput {
                text: r.long_str()?,
            },
            41 => Msg::TextFocus {
                editable: r.u8()? != 0,
            },
            42 => Msg::Touch {
                id: r.u8()?,
                phase: r.u8()?,
                x: r.u16()?,
                y: r.u16()?,
            },
            43 => Msg::Touchpad {
                phase: r.u8()?,
                dx: r.u16()? as i16,
                dy: r.u16()? as i16,
                scale_milli: r.u16()?,
            },
            44 => Msg::Pen {
                phase: r.u8()?,
                x: r.u16()?,
                y: r.u16()?,
                pressure: r.u16()?,
                tilt_x: r.u8()? as i8,
                tilt_y: r.u8()? as i8,
                flags: r.u8()?,
            },
            45 => Msg::MouseDelta {
                dx: r.u16()? as i16,
                dy: r.u16()? as i16,
            },
            50 => Msg::SendSas,
            51 => {
                let n = r.u8()? as usize;
                Msg::Desktop { name: r.text(n)? }
            }
            60 => {
                let (name, home, event) = (r.str()?, r.long_str()?, r.u8()?);
                let token = if r.u8()? != 0 { Some(r.take()?) } else { None };
                let grant = r.grant()?;
                Msg::MeetHi {
                    name,
                    home,
                    token,
                    event,
                    grant,
                    platform: r.platform(),
                }
            }
            61 => {
                let (name, home) = (r.str()?, r.long_str()?);
                let grant = r.grant()?;
                let event = match r.0 {
                    [EVENT_MARK, e, rest @ ..] => {
                        let e = *e;
                        r.0 = rest;
                        e
                    }
                    _ => 0,
                };
                Msg::MeetWelcome {
                    name,
                    home,
                    grant,
                    event,
                    platform: r.platform(),
                }
            }
            62 => Msg::MeetRefuse { reason: r.u8()? },
            70 => {
                let (current, n) = (r.u8()?, r.u8()?);
                let mut list = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    let (x, y, width, height) =
                        (r.u32()? as i32, r.u32()? as i32, r.u32()?, r.u32()?);
                    let (primary, rotation, name) = (r.u8()? != 0, r.u16()?, r.str()?);
                    list.push(MonitorDesc {
                        name,
                        x,
                        y,
                        width,
                        height,
                        primary,
                        rotation,
                    });
                }
                Msg::Monitors { list, current }
            }
            71 => Msg::SelectMonitor { index: r.u8()? },
            72 => {
                let n = r.u8()? as usize;
                let mut list = Vec::with_capacity(n);
                for _ in 0..n {
                    let hwnd = r.u64()?;
                    let app_key = r.str()?;
                    let app_name = r.str()?;
                    let title = r.str()?;
                    let monitor = r.u8()?;
                    let minimized = r.u8()? != 0;
                    let rx = r.u16()?;
                    let ry = r.u16()?;
                    let rw = r.u16()?;
                    let rh = r.u16()?;
                    list.push(WindowDesc {
                        hwnd,
                        app_key,
                        app_name,
                        title,
                        monitor,
                        minimized,
                        rx,
                        ry,
                        rw,
                        rh,
                    });
                }
                Msg::Windows { list }
            }
            73 => Msg::FocusWindow {
                hwnd: r.u64()?,
                vw: r.u16().unwrap_or(0),
                vh: r.u16().unwrap_or(0),
            },
            74 => Msg::AppIcon {
                app_key: r.str()?,
                png: r.bytes()?,
            },
            75 => Msg::WindowThumb {
                hwnd: r.u64()?,
                png: r.bytes()?,
            },
            76 => Msg::VirtualDisplay {
                active: r.u8()? != 0,
                width: r.u16()?,
                height: r.u16()?,
                dpi_percent: r.u16()?,
            },
            80 => Msg::Audio(AudioCfg {
                enabled: r.u8()? != 0,
                channels: r.u8()?.clamp(1, 2),
                mute_host: r.u8()? != 0,
                lossless: r.u8().unwrap_or(0) != 0,
                max_rate_hz: r.u32().unwrap_or(0),
            }),
            t => bail!("unknown message type {t}"),
        })
    }
}

pub async fn read_msg(recv: &mut iroh::endpoint::RecvStream) -> Result<Msg> {
    loop {
        let mut len = [0u8; 4];
        recv.read_exact(&mut len).await?;
        let len = u32::from_be_bytes(len) as usize;
        if len > MAX_MSG {
            bail!("control message of {len} bytes is too large");
        }
        let mut buf = vec![0u8; len];
        recv.read_exact(&mut buf).await?;
        match Msg::decode(&buf) {
            Err(e) if e.to_string().starts_with("unknown message type") => continue,
            r => return r,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_core_messages() {
        let msgs = [
            Msg::Hello {
                max_kbps: 20_000,
                fps: 60,
                caps: CAN_HEVC,
                proof: String::new(),
                quality: Some(Quality {
                    fit_w: 1080,
                    fit_h: 2412,
                    short: 0,
                    fps: 60,
                    kbps: 0,
                    yuv444: false,
                    auto_fps: true,
                }),
            },
            Msg::Waiting,
            Msg::Paused { on: true },
            Msg::Info {
                width: 2560,
                height: 1440,
                encoder: "NVIDIA HEVC".into(),
                streaming: Some(Streaming {
                    fps: 60,
                    max_kbps: 20_000,
                    can444: true,
                    yuv444: false,
                    resends: true,
                }),
            },
            Msg::Monitors {
                current: 0,
                list: vec![MonitorDesc {
                    name: r"\\.\DISPLAY1".into(),
                    x: 0,
                    y: 0,
                    width: 2560,
                    height: 1440,
                    primary: true,
                    rotation: 0,
                }],
            },
            Msg::SelectMonitor { index: 1 },
            Msg::TextInput {
                text: "你好世界 👋 hello\nline2".into(),
            },
            Msg::Windows {
                list: vec![WindowDesc {
                    hwnd: 0x0012_3456,
                    app_key: "code".into(),
                    app_name: "VS Code".into(),
                    title: "rc-android - Visual Studio Code".into(),
                    monitor: 1,
                    minimized: false,
                    rx: 6554,
                    ry: 3277,
                    rw: 39321,
                    rh: 52428,
                }],
            },
            Msg::FocusWindow {
                hwnd: 0x0012_3456,
                vw: 1080,
                vh: 2412,
            },
            Msg::AppIcon {
                app_key: "code".into(),
                png: vec![0x89, b'P', b'N', b'G', 1, 2, 3],
            },
            Msg::WindowThumb {
                hwnd: 0x0012_3456,
                png: vec![0x89, b'P', b'N', b'G', 4, 5, 6],
            },
            Msg::MeetHi {
                name: "OPPO Find X5 Pro".into(),
                home: String::new(),
                token: Some([1, 2, 3, 4, 5, 6, 7, 8]),
                event: EVENT_ASK,
                grant: Some(String::new()),
                platform: PLATFORM_ANDROID,
            },
            Msg::MeetWelcome {
                name: "DESKTOP".into(),
                home: "https://aps1-1.relay.n0.iroh.link./".into(),
                grant: None,
                event: EVENT_REMEMBER,
                platform: PLATFORM_PC,
            },
        ];
        for m in msgs {
            let enc = m.encode();
            let dec = Msg::decode(&enc[4..]).unwrap();
            assert_eq!(m, dec);
        }
    }

    fn meets(platform: u8) -> Vec<Msg> {
        let hi = |token, grant: Option<&str>, event| Msg::MeetHi {
            name: "OPPO PFEM10".into(),
            home: "https://203.0.113.7:7843/?rc-cert=abc".into(),
            token,
            event,
            grant: grant.map(String::from),
            platform,
        };
        let welcome = |grant: Option<&str>, event| Msg::MeetWelcome {
            name: "OPPO PFEM10".into(),
            home: String::new(),
            grant: grant.map(String::from),
            event,
            platform,
        };
        vec![
            hi(None, None, EVENT_NONE),
            hi(Some([9; 8]), Some(""), EVENT_ASK),
            hi(None, Some("https://a/,https://b/"), EVENT_REMEMBER),
            welcome(None, EVENT_NONE),
            welcome(Some(""), EVENT_NONE),
            welcome(None, EVENT_FORGET),
            welcome(Some("https://a/"), EVENT_REMEMBER),
        ]
    }

    /// Body bytes as an older version wrote them: the same message without
    /// any platform tail.
    fn body(m: &Msg) -> Vec<u8> {
        m.encode()[4..].to_vec()
    }

    #[test]
    fn meet_platform_round_trips_and_is_only_a_tail() {
        for (phone, pc) in meets(PLATFORM_ANDROID).into_iter().zip(meets(PLATFORM_PC)) {
            assert_eq!(Msg::decode(&body(&phone)).unwrap(), phone);
            // A computer writes exactly what older versions wrote, and reads as one.
            assert_eq!(Msg::decode(&body(&pc)).unwrap(), pc);
            // A phone adds the mark and the platform at the very end, so an
            // older reader, which stops before it, sees the same message.
            let (p, c) = (body(&phone), body(&pc));
            assert_eq!(p.len(), c.len() + 2, "{phone:?}");
            assert_eq!(&p[..c.len()], &c[..]);
            assert_eq!(&p[c.len()..], &[PLATFORM_MARK, PLATFORM_ANDROID]);
        }
    }

    #[test]
    fn meet_platform_unknown_value_and_cut_tail() {
        // A newer platform number is kept as it came.
        let mut b = body(&meets(PLATFORM_PC)[0]);
        b.extend_from_slice(&[PLATFORM_MARK, 7]);
        assert!(matches!(Msg::decode(&b).unwrap(), Msg::MeetHi { platform: 7, .. }));
        // A mark without its byte is no platform.
        let mut b = body(&meets(PLATFORM_PC)[5]);
        b.push(PLATFORM_MARK);
        assert!(matches!(Msg::decode(&b).unwrap(), Msg::MeetWelcome { platform: PLATFORM_PC, event: EVENT_FORGET, .. }));
    }
}
