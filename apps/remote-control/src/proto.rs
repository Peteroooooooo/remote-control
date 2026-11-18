//! Messages on the control stream and the header in front of each video frame.
//! All integers are big-endian.
//!
//! Control stream (reliable, opened by the viewer): each message is
//! `u32 length` + `u8 type` + body, where length counts the type byte and body.
//! Type numbers: 1-19 core, 20-29 clipboard, 30-39 cursor, 40-49 keyboard,
//! 50-59 service mode, 60-69 meeting (on its own ALPN, see `meet`), 70-79 displays.

use anyhow::{Result, bail};

use crate::cursor::CursorImage;

/// 3: the hello carries a device proof; the host may ask its user first.
pub const ALPN: &[u8] = b"rc/desktop/3";

/// Largest control message accepted. Bulk data (clipboard) goes on its own streams.
pub const MAX_MSG: usize = 1 << 20;

/// First byte of a unidirectional stream: what it carries.
pub const STREAM_CLIPBOARD: u8 = 1;

/// Video payload header: flags, stream width, stream height, host time from
/// the screen update to the encoded frame in microseconds.
pub const FRAME_HDR: usize = 1 + 2 + 2 + 4;
const F_KEY: u8 = 1;
const F_HEVC: u8 = 2;

/// Hello capability bits: what the viewer can decode besides H.264.
pub const CAN_HEVC: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    pub key: bool,
    /// HEVC instead of H.264.
    pub hevc: bool,
    pub width: u16,
    pub height: u16,
    pub host_us: u32,
}

impl FrameHeader {
    pub fn write(&self, out: &mut Vec<u8>) {
        out.push(if self.key { F_KEY } else { 0 } | if self.hevc { F_HEVC } else { 0 });
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
            width: u16::from_be_bytes([b[1], b[2]]),
            height: u16::from_be_bytes([b[3], b[4]]),
            host_us: u32::from_be_bytes([b[5], b[6], b[7], b[8]]),
        };
        Some((h, &b[FRAME_HDR..]))
    }
}

/// Once a second from host to viewer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostStats {
    pub fps: u16,
    pub kbps: u32,
    /// Per mille of due frames.
    pub skipped_pm: u16,
    pub shrunk_pm: u16,
    pub frame_rtt_p50_us: u32,
    pub frame_rtt_p95_us: u32,
    /// Screen update to encoded frame.
    pub host_p50_us: u32,
    pub host_p95_us: u32,
    /// Of that: colour conversion plus encoding.
    pub encode_p50_us: u32,
    pub encode_p95_us: u32,
    /// 0 = none, 1 = direct, 2 = relay.
    pub path: u8,
    pub path_rtt_us: u32,
    /// The bitrate the rate controller allows right now (what is sent can be
    /// far lower when the screen is still).
    pub rate_kbps: u32,
    /// Encoded frame size as a percentage of the size asked for: median and largest.
    pub size_p50_pct: u16,
    pub size_max_pct: u16,
}

/// One of the host's displays, as placed on its virtual desktop.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MonitorDesc {
    /// The DXGI device name, e.g. `\\.\DISPLAY2`.
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
    /// Degrees clockwise: 0, 90, 180 or 270.
    pub rotation: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Msg {
    /// Viewer -> host, first message. caps: CAN_HEVC. `proof`: an `rcd1`
    /// device proof when the viewer connects with a session key of its own
    /// (see `ident`), empty when it connects with its device key.
    Hello { max_kbps: u32, fps: u8, caps: u8, proof: String },
    /// Host -> viewer: the host's user is being asked to allow this session.
    Waiting,
    /// Host -> viewer: the picture stops (on) because the only way left goes
    /// through a public relay, or goes on again (off).
    Paused { on: bool },
    /// Viewer -> host. Position over the whole stream, 0..=65535 on each axis.
    MouseMove { x: u16, y: u16 },
    /// 0 left, 1 right, 2 middle, 3 back, 4 forward.
    MouseButton { button: u8, down: bool },
    /// Wheel delta in WHEEL_DELTA units (120 per notch).
    Wheel { delta: i16, horizontal: bool },
    /// Set-1 scan code; 0xE0xx for extended keys.
    Key { scan: u16, down: bool },
    /// Viewer lost focus: let go of every key and button.
    ReleaseAll,
    RequestKeyframe,
    Ping { t_us: u64 },
    Pong { t_us: u64 },
    /// Host -> viewer, sent before the first frame and whenever the stream changes.
    Info { width: u16, height: u16, encoder: String },
    Stats(HostStats),
    /// Viewer -> host: move the cursor here and report where it ended up.
    Probe { x: u16, y: u16 },
    ProbeReply { x: u16, y: u16 },
    /// Viewer -> host: press Ctrl+Alt+Del (service mode only).
    SendSas,
    /// Host -> viewer: the input desktop the host now shows ("Default", or
    /// "Winlogon" for the UAC prompt, lock, login and Ctrl+Alt+Del screens).
    Desktop { name: String },
    /// Host -> viewer: a new cursor image, shown from now on and kept under `id`.
    CursorShape { id: u32, image: CursorImage },
    /// Host -> viewer: show the image sent earlier under `id`.
    CursorUse { id: u32 },
    CursorHidden,
    /// Either way: the sender's clipboard changed (its copy `seq`, made after
    /// hearing of the receiver's copy `seen`); the data follows on its own stream.
    ClipOffer { seq: u32, seen: u32, format: u8, bytes: u32 },
    /// On the meet ALPN, from the one who connected: who I am, where I am
    /// reachable (my home relay), the token of the other side's connection
    /// code if I am redeeming one, a change to its place in my device
    /// list (`meet::EVENT_*`), and the relays I give it (`grant`).
    MeetHi { name: String, home: String, token: Option<[u8; 8]>, event: u8, grant: Option<String> },
    /// The answer: the other side knows me. Anything after `home` on the
    /// wire other than a grant (an older version's door sent more) is left
    /// unread.
    MeetWelcome { name: String, home: String, grant: Option<String> },
    /// The answer: it does not (`meet::REFUSE_*`).
    MeetRefuse { reason: u8 },
    /// Host -> viewer, on connect and whenever the displays or the streamed one
    /// change: all displays, left to right, and the index of the streamed one.
    Monitors { list: Vec<MonitorDesc>, current: u8 },
    /// Viewer -> host: stream display `index` of the latest list.
    SelectMonitor { index: u8 },
    /// Host -> viewer: why there is no picture now, or what did not work
    /// (`HINT_*`); `HINT_NONE` when the picture is back.
    Hint { what: u8 },
}

pub const HINT_NONE: u8 = 0;
/// No display to capture (a laptop with its lid closed and no screen attached).
pub const HINT_NO_DISPLAY: u8 = 1;
/// No service: a UAC prompt is up, which only the host's user can answer.
pub const HINT_UAC: u8 = 2;
/// No service: the host is locked (or at another secure screen).
pub const HINT_LOCKED: u8 = 3;
/// No service: Ctrl+Alt+Del cannot be sent.
pub const HINT_NO_SAS: u8 = 10;

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
    /// `u8 length` + UTF-8, cut at 200 bytes on a character boundary.
    fn str(&mut self, v: &str) -> &mut Self {
        let mut n = v.len().min(200);
        while !v.is_char_boundary(n) {
            n -= 1;
        }
        self.u8(n as u8);
        self.0.extend_from_slice(&v.as_bytes()[..n]);
        self
    }
    /// `u16 length` + UTF-8, for longer text (codes).
    fn long_str(&mut self, v: &str) -> &mut Self {
        let mut n = v.len().min(u16::MAX as usize);
        while !v.is_char_boundary(n) {
            n -= 1;
        }
        self.u16(n as u16);
        self.0.extend_from_slice(&v.as_bytes()[..n]);
        self
    }
    /// A meet grant: `GRANT_MARK`, then the text; nothing for `None`.
    fn grant(&mut self, g: &Option<String>) -> &mut Self {
        if let Some(g) = g {
            self.u8(GRANT_MARK).long_str(g);
        }
        self
    }
}

/// Starts a grant at the end of a meet message. Not 0, which an older
/// version's welcome had there.
const GRANT_MARK: u8 = 0xA7;

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
    /// A u32 length, then that many bytes.
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
    /// What `W::grant` wrote, or `None` (an older version sent none).
    fn grant(&mut self) -> Result<Option<String>> {
        if self.0.first() != Some(&GRANT_MARK) {
            return Ok(None);
        }
        self.u8()?;
        Ok(Some(self.long_str()?))
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
            Msg::SendSas => 50,
            Msg::Desktop { .. } => 51,
            Msg::CursorShape { .. } => 30,
            Msg::CursorUse { .. } => 31,
            Msg::CursorHidden => 32,
            Msg::ClipOffer { .. } => 20,
            Msg::MeetHi { .. } => 60,
            Msg::MeetWelcome { .. } => 61,
            Msg::MeetRefuse { .. } => 62,
            Msg::Monitors { .. } => 70,
            Msg::SelectMonitor { .. } => 71,
            Msg::Hint { .. } => 16,
        }
    }

    /// The full framed message, length prefix included.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = W(vec![0, 0, 0, 0, self.tag()]);
        match self {
            Msg::Hello { max_kbps, fps, caps, proof } => {
                w.u32(*max_kbps).u8(*fps).u8(*caps).long_str(proof);
            }
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
            Msg::ReleaseAll | Msg::RequestKeyframe | Msg::SendSas | Msg::CursorHidden => {}
            Msg::Ping { t_us } | Msg::Pong { t_us } => {
                w.u64(*t_us);
            }
            Msg::Info { width, height, encoder } => {
                let name = &encoder.as_bytes()[..encoder.len().min(200)];
                w.u16(*width).u16(*height).u8(name.len() as u8);
                w.0.extend_from_slice(name);
            }
            Msg::Desktop { name } => {
                let name = &name.as_bytes()[..name.len().min(200)];
                w.u8(name.len() as u8);
                w.0.extend_from_slice(name);
            }
            Msg::MeetHi { name, home, token, event, grant } => {
                w.str(name).long_str(home).u8(*event).u8(token.is_some() as u8);
                if let Some(t) = token {
                    w.0.extend_from_slice(t);
                }
                w.grant(grant);
            }
            Msg::MeetWelcome { name, home, grant } => {
                w.str(name).long_str(home).grant(grant);
            }
            Msg::MeetRefuse { reason } => {
                w.u8(*reason);
            }
            Msg::Stats(s) => {
                w.u16(s.fps).u32(s.kbps).u16(s.skipped_pm).u16(s.shrunk_pm);
                w.u32(s.frame_rtt_p50_us).u32(s.frame_rtt_p95_us).u32(s.host_p50_us).u32(s.host_p95_us);
                w.u32(s.encode_p50_us).u32(s.encode_p95_us);
                w.u8(s.path).u32(s.path_rtt_us).u32(s.rate_kbps).u16(s.size_p50_pct).u16(s.size_max_pct);
            }
            Msg::CursorShape { id, image: i } => {
                w.u32(*id).u16(i.width).u16(i.height).u16(i.hot_x).u16(i.hot_y);
                w.u32(i.mask.len() as u32).0.extend_from_slice(&i.mask);
                w.u32(i.color.len() as u32).0.extend_from_slice(&i.color);
            }
            Msg::CursorUse { id } => {
                w.u32(*id);
            }
            Msg::ClipOffer { seq, seen, format, bytes } => {
                w.u32(*seq).u32(*seen).u8(*format).u32(*bytes);
            }
            Msg::Monitors { list, current } => {
                let list = &list[..list.len().min(255)];
                w.u8(*current).u8(list.len() as u8);
                for m in list {
                    w.u32(m.x as u32).u32(m.y as u32).u32(m.width).u32(m.height);
                    w.u8(m.primary as u8).u16(m.rotation).str(&m.name);
                }
            }
            Msg::SelectMonitor { index } | Msg::Hint { what: index } => {
                w.u8(*index);
            }
        }
        let len = (w.0.len() - 4) as u32;
        w.0[..4].copy_from_slice(&len.to_be_bytes());
        w.0
    }

    /// Parses one message without its length prefix (type byte first).
    pub fn decode(b: &[u8]) -> Result<Msg> {
        let (&tag, body) = b.split_first().ok_or_else(|| anyhow::anyhow!("empty message"))?;
        let mut r = R(body);
        Ok(match tag {
            1 => Msg::Hello { max_kbps: r.u32()?, fps: r.u8()?, caps: r.u8()?, proof: r.long_str()? },
            2 => Msg::MouseMove { x: r.u16()?, y: r.u16()? },
            3 => Msg::MouseButton { button: r.u8()?, down: r.u8()? != 0 },
            4 => Msg::Wheel { delta: r.u16()? as i16, horizontal: r.u8()? != 0 },
            5 => Msg::Key { scan: r.u16()?, down: r.u8()? != 0 },
            6 => Msg::ReleaseAll,
            7 => Msg::RequestKeyframe,
            8 => Msg::Ping { t_us: r.u64()? },
            9 => Msg::Pong { t_us: r.u64()? },
            10 => {
                let (width, height, n) = (r.u16()?, r.u16()?, r.u8()? as usize);
                if r.0.len() < n {
                    bail!("message too short");
                }
                Msg::Info { width, height, encoder: String::from_utf8_lossy(&r.0[..n]).into_owned() }
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
            12 => Msg::Probe { x: r.u16()?, y: r.u16()? },
            13 => Msg::ProbeReply { x: r.u16()?, y: r.u16()? },
            14 => Msg::Waiting,
            15 => Msg::Paused { on: r.u8()? != 0 },
            50 => Msg::SendSas,
            51 => {
                let n = r.u8()? as usize;
                if r.0.len() < n {
                    bail!("message too short");
                }
                Msg::Desktop { name: String::from_utf8_lossy(&r.0[..n]).into_owned() }
            }
            30 => {
                let id = r.u32()?;
                let (width, height, hot_x, hot_y) = (r.u16()?, r.u16()?, r.u16()?, r.u16()?);
                let mask = r.bytes()?;
                let color = r.bytes()?;
                let image = CursorImage { width, height, hot_x, hot_y, color, mask };
                if !image.valid() {
                    bail!("bad cursor image {width}x{height}");
                }
                Msg::CursorShape { id, image }
            }
            31 => Msg::CursorUse { id: r.u32()? },
            32 => Msg::CursorHidden,
            20 => Msg::ClipOffer { seq: r.u32()?, seen: r.u32()?, format: r.u8()?, bytes: r.u32()? },
            60 => {
                let (name, home, event) = (r.str()?, r.long_str()?, r.u8()?);
                let token = if r.u8()? != 0 { Some(r.take()?) } else { None };
                Msg::MeetHi { name, home, token, event, grant: r.grant()? }
            }
            61 => {
                let (name, home) = (r.str()?, r.long_str()?);
                Msg::MeetWelcome { name, home, grant: r.grant()? }
            }
            62 => Msg::MeetRefuse { reason: r.u8()? },
            70 => {
                let (current, n) = (r.u8()?, r.u8()?);
                let mut list = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    let (x, y, width, height) = (r.u32()? as i32, r.u32()? as i32, r.u32()?, r.u32()?);
                    let (primary, rotation, name) = (r.u8()? != 0, r.u16()?, r.str()?);
                    list.push(MonitorDesc { name, x, y, width, height, primary, rotation });
                }
                Msg::Monitors { list, current }
            }
            71 => Msg::SelectMonitor { index: r.u8()? },
            16 => Msg::Hint { what: r.u8()? },
            t => bail!("unknown message type {t}"),
        })
    }
}

/// Reads one message from the control stream.
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
            // From a newer version: not for this one.
            Err(e) if e.to_string().starts_with("unknown message type") => continue,
            r => return r,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        let all = [
            Msg::Hello { max_kbps: 20_000, fps: 60, caps: CAN_HEVC, proof: String::new() },
            Msg::Hello { max_kbps: 20_000, fps: 60, caps: 0, proof: format!("rcd1{}", "x".repeat(300)) },
            Msg::Waiting,
            Msg::Paused { on: true },
            Msg::Paused { on: false },
            Msg::MouseMove { x: 1, y: 65535 },
            Msg::MouseButton { button: 2, down: true },
            Msg::Wheel { delta: -120, horizontal: false },
            Msg::Key { scan: 0xE01D, down: false },
            Msg::ReleaseAll,
            Msg::RequestKeyframe,
            Msg::Ping { t_us: 1 << 40 },
            Msg::Pong { t_us: 7 },
            Msg::Info { width: 2880, height: 1920, encoder: "Intel® H.264".into() },
            Msg::Stats(HostStats { fps: 60, kbps: 19_000, path: 1, host_p95_us: 9_000, rate_kbps: 20_000, size_max_pct: 340, ..Default::default() }),
            Msg::Probe { x: 100, y: 200 },
            Msg::ProbeReply { x: 101, y: 199 },
            Msg::SendSas,
            Msg::Desktop { name: "Winlogon".into() },
            Msg::CursorShape {
                id: 3,
                image: CursorImage { width: 2, height: 1, hot_x: 1, hot_y: 0, color: vec![1; 8], mask: vec![0x40, 0] },
            },
            Msg::CursorUse { id: 3 },
            Msg::CursorHidden,
            Msg::ClipOffer { seq: 9, seen: 4, format: 2, bytes: 1 << 24 },
            Msg::MeetHi { name: "书房".into(), home: "https://aps1-1.relay.n0.iroh.link./".into(), token: None, event: 0, grant: None },
            Msg::MeetHi { name: "x".repeat(200), home: String::new(), token: Some([7; 8]), event: 2, grant: Some(String::new()) },
            Msg::MeetHi { name: "y".into(), home: String::new(), token: None, event: 0, grant: Some("https://a/,https://b/ https://c/".into()) },
            Msg::MeetWelcome { name: "laptop".into(), home: String::new(), grant: None },
            Msg::MeetWelcome { name: "书房".into(), home: "https://a/,https://b/".into(), grant: Some("https://a/".into()) },
            Msg::Hint { what: HINT_LOCKED },
            Msg::MeetRefuse { reason: 3 },
            Msg::Monitors {
                list: vec![
                    MonitorDesc { name: r"\\.\DISPLAY2".into(), x: -1080, y: -200, width: 1080, height: 1920, primary: false, rotation: 90 },
                    MonitorDesc { name: r"\\.\DISPLAY1".into(), x: 0, y: 0, width: 2560, height: 1440, primary: true, rotation: 0 },
                ],
                current: 1,
            },
            Msg::Monitors { list: vec![], current: 0 },
            Msg::SelectMonitor { index: 7 },
        ];
        for m in all {
            let e = m.encode();
            assert_eq!(u32::from_be_bytes([e[0], e[1], e[2], e[3]]) as usize, e.len() - 4);
            assert_eq!(Msg::decode(&e[4..]).unwrap(), m);
        }
    }

    #[test]
    fn long_names_are_cut_on_a_character_boundary() {
        let e = Msg::MeetWelcome { name: "设".repeat(100), home: String::new(), grant: None }.encode();
        let Msg::MeetWelcome { name, .. } = Msg::decode(&e[4..]).unwrap() else { panic!() };
        assert_eq!(name, "设".repeat(66));
    }

    #[test]
    fn a_welcome_with_more_after_home_still_reads() {
        let mut e = Msg::MeetWelcome { name: "书房".into(), home: "https://a/".into(), grant: None }.encode();
        e.extend_from_slice(&[0, 0, 0, 3, b'r', b'c', b'd']);
        let len = (e.len() - 4) as u32;
        e[..4].copy_from_slice(&len.to_be_bytes());
        assert_eq!(Msg::decode(&e[4..]).unwrap(), Msg::MeetWelcome { name: "书房".into(), home: "https://a/".into(), grant: None });
    }

    #[test]
    fn frame_header_round_trips() {
        let h = FrameHeader { key: true, hevc: true, width: 3840, height: 2160, host_us: 4_321 };
        let mut v = Vec::new();
        h.write(&mut v);
        v.push(9);
        assert_eq!(FrameHeader::parse(&v), Some((h, &[9u8][..])));
        assert!(FrameHeader::parse(&v[..4]).is_none());
    }
}
