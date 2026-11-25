//! Datagram layout. All integers are big-endian.
//!
//! - Shard (sender -> receiver): type, frame, shard index, data shard count,
//!   parity shard count, frame length in bytes, then the shard bytes.
//!   Indexes `0..data` are data shards, `data..data + parity` are parity shards.
//!   A shard sent again on request has type `T_SHARD_RESENT`, otherwise the same.
//! - Ack (receiver -> sender): type, frame, flags (bit 0: parity was needed,
//!   bit 1: shards sent again were needed), and an optional 4-byte `prev_mask`
//!   bitmap of recent prior frames already assembled. Sent as soon as a frame
//!   can be decoded.
//! - Resend request (receiver -> sender): type, frame, then the shard indexes
//!   wanted, 2 bytes each; none means all data shards (nothing of the frame came).
//! - Report (receiver -> sender, every 100 ms): shard and frame counts for frames
//!   the receiver has finished with since the last report, and optionally the
//!   longest run of consecutive shards missing from any one of them.
//! - Probe (sender -> receiver): type, probe number, then padding (see `probe`).
//! - Probe result (receiver -> sender): type, probe number, bytes received
//!   after the probe's first datagram up to its last one, and the time between
//!   them in microseconds.

pub const T_SHARD: u8 = 0x10;
pub const T_ACK: u8 = 0x11;
pub const T_REPORT: u8 = 0x12;
pub const T_AUDIO: u8 = 0x13;
pub const T_AUDIO_PCM24: u8 = 0x14;
pub const T_PROBE: u8 = 0x15;
pub const T_PROBE_RESULT: u8 = 0x16;
pub const T_RESEND: u8 = 0x17;
pub const T_SHARD_RESENT: u8 = 0x18;
/// Opus audio datagram carrying primary + 1–2 historical redundant copies (RED).
pub const T_AUDIO_RED: u8 = 0x19;

/// Most shard indexes one resend request names.
pub const RESEND_MAX: usize = 256;

pub const PROBE_HDR: usize = 3;
pub const PROBE_RESULT_LEN: usize = 1 + 2 + 4 + 4;

pub const SHARD_HDR: usize = 1 + 4 + 2 + 2 + 2 + 4;
pub const ACK_LEN: usize = 6;
pub const ACK_EXT_LEN: usize = 10;
pub const REPORT_LEN: usize = 1 + 4 + 4 + 2 + 2 + 2;
pub const REPORT_EXT_LEN: usize = REPORT_LEN + 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShardHeader {
    pub frame: u32,
    pub index: u16,
    pub data: u16,
    pub parity: u16,
    pub frame_len: u32,
}

impl ShardHeader {
    pub fn write(&self, out: &mut Vec<u8>) {
        out.push(T_SHARD);
        out.extend_from_slice(&self.frame.to_be_bytes());
        out.extend_from_slice(&self.index.to_be_bytes());
        out.extend_from_slice(&self.data.to_be_bytes());
        out.extend_from_slice(&self.parity.to_be_bytes());
        out.extend_from_slice(&self.frame_len.to_be_bytes());
    }

    /// Returns the header and the shard bytes, or `None` if this is not a
    /// valid shard (sent first or again).
    pub fn parse(b: &[u8]) -> Option<(ShardHeader, &[u8])> {
        if b.len() <= SHARD_HDR || !is_shard(b) {
            return None;
        }
        let h = ShardHeader {
            frame: u32::from_be_bytes(b[1..5].try_into().ok()?),
            index: u16::from_be_bytes(b[5..7].try_into().ok()?),
            data: u16::from_be_bytes(b[7..9].try_into().ok()?),
            parity: u16::from_be_bytes(b[9..11].try_into().ok()?),
            frame_len: u32::from_be_bytes(b[11..15].try_into().ok()?),
        };
        let total = h.data as u32 + h.parity as u32;
        if h.data == 0 || total > u16::MAX as u32 || (h.index as u32) >= total {
            return None;
        }
        Some((h, &b[SHARD_HDR..]))
    }
}

/// A shard, sent first or again.
pub fn is_shard(b: &[u8]) -> bool {
    matches!(b.first(), Some(&T_SHARD | &T_SHARD_RESENT))
}

/// The same shard, marked as sent again.
pub fn resent_shard(b: &[u8]) -> Vec<u8> {
    let mut v = b.to_vec();
    v[0] = T_SHARD_RESENT;
    v
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ack {
    pub frame: u32,
    /// The frame needed parity shards to decode.
    pub recovered: bool,
    /// The frame needed shards sent again (its ack says nothing about the
    /// path's delay). An older receiver never says so.
    pub resent: bool,
    /// Bit `i` (`0..32`) is 1 if `frame.wrapping_sub(i + 1)` has already been assembled.
    pub prev_mask: u32,
}

impl Ack {
    pub fn encode(&self) -> [u8; ACK_EXT_LEN] {
        let mut out = [0u8; ACK_EXT_LEN];
        out[0] = T_ACK;
        out[1..5].copy_from_slice(&self.frame.to_be_bytes());
        out[5] = self.recovered as u8 | (self.resent as u8) << 1;
        out[6..10].copy_from_slice(&self.prev_mask.to_be_bytes());
        out
    }

    pub fn parse(b: &[u8]) -> Option<Ack> {
        if b.len() < ACK_LEN || b[0] != T_ACK {
            return None;
        }
        let prev_mask = if b.len() >= ACK_EXT_LEN {
            u32::from_be_bytes(b[6..10].try_into().unwrap())
        } else {
            0
        };
        Some(Ack {
            frame: u32::from_be_bytes(b[1..5].try_into().ok()?),
            recovered: b[5] & 1 != 0,
            resent: b[5] & 2 != 0,
            prev_mask,
        })
    }
}

/// Asks the sender for shards of `frame` again; no `shards` means all its data shards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resend {
    pub frame: u32,
    pub shards: Vec<u16>,
}

impl Resend {
    pub fn encode(&self) -> Vec<u8> {
        let n = self.shards.len().min(RESEND_MAX);
        let mut v = Vec::with_capacity(5 + 2 * n);
        v.push(T_RESEND);
        v.extend_from_slice(&self.frame.to_be_bytes());
        for s in &self.shards[..n] {
            v.extend_from_slice(&s.to_be_bytes());
        }
        v
    }

    pub fn parse(b: &[u8]) -> Option<Resend> {
        if b.len() < 5 || b[0] != T_RESEND {
            return None;
        }
        let shards = b[5..].chunks_exact(2).take(RESEND_MAX).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        Some(Resend { frame: u32::from_be_bytes(b[1..5].try_into().ok()?), shards })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Shards the sender put on the wire for the finished frames.
    pub shards_expected: u32,
    /// Shards that arrived for those frames.
    pub shards_received: u32,
    /// Frames decoded from data shards alone.
    pub frames_ok: u16,
    /// Frames that needed parity shards.
    pub frames_recovered: u16,
    /// Frames that never had enough shards.
    pub frames_failed: u16,
    /// Longest run of consecutive shards missing from one frame (0 from an
    /// older receiver).
    pub max_run: u8,
}

impl Report {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(REPORT_EXT_LEN);
        v.push(T_REPORT);
        v.extend_from_slice(&self.shards_expected.to_be_bytes());
        v.extend_from_slice(&self.shards_received.to_be_bytes());
        v.extend_from_slice(&self.frames_ok.to_be_bytes());
        v.extend_from_slice(&self.frames_recovered.to_be_bytes());
        v.extend_from_slice(&self.frames_failed.to_be_bytes());
        v.push(self.max_run);
        v
    }

    pub fn parse(b: &[u8]) -> Option<Report> {
        if b.len() < REPORT_LEN || b[0] != T_REPORT {
            return None;
        }
        Some(Report {
            shards_expected: u32::from_be_bytes(b[1..5].try_into().ok()?),
            shards_received: u32::from_be_bytes(b[5..9].try_into().ok()?),
            frames_ok: u16::from_be_bytes(b[9..11].try_into().ok()?),
            frames_recovered: u16::from_be_bytes(b[11..13].try_into().ok()?),
            frames_failed: u16::from_be_bytes(b[13..15].try_into().ok()?),
            max_run: b.get(REPORT_LEN).copied().unwrap_or(0),
        })
    }

    /// Fraction of shards lost, 0.0..=1.0.
    pub fn shard_loss(&self) -> f64 {
        if self.shards_expected == 0 {
            return 0.0;
        }
        1.0 - (self.shards_received.min(self.shards_expected) as f64 / self.shards_expected as f64)
    }
}

/// A probe datagram of `len` bytes (at least `PROBE_HDR`).
pub fn probe_datagram(id: u16, len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len.max(PROBE_HDR)];
    v[0] = T_PROBE;
    v[1..3].copy_from_slice(&id.to_be_bytes());
    v
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeResult {
    pub id: u16,
    pub bytes: u32,
    pub span_us: u32,
}

impl ProbeResult {
    pub fn encode(&self) -> [u8; PROBE_RESULT_LEN] {
        let mut out = [0u8; PROBE_RESULT_LEN];
        out[0] = T_PROBE_RESULT;
        out[1..3].copy_from_slice(&self.id.to_be_bytes());
        out[3..7].copy_from_slice(&self.bytes.to_be_bytes());
        out[7..11].copy_from_slice(&self.span_us.to_be_bytes());
        out
    }

    pub fn parse(b: &[u8]) -> Option<ProbeResult> {
        if b.len() < PROBE_RESULT_LEN || b[0] != T_PROBE_RESULT {
            return None;
        }
        Some(ProbeResult {
            id: u16::from_be_bytes(b[1..3].try_into().ok()?),
            bytes: u32::from_be_bytes(b[3..7].try_into().ok()?),
            span_us: u32::from_be_bytes(b[7..11].try_into().ok()?),
        })
    }

    /// Bits per second received while the probe arrived.
    pub fn bps(&self) -> f64 {
        self.bytes as f64 * 8.0 / (self.span_us.max(1) as f64 / 1e6)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let p = ProbeResult { id: 3, bytes: 100_000, span_us: 200_000 };
        assert_eq!(ProbeResult::parse(&p.encode()), Some(p));
        assert_eq!(p.bps(), 4e6);
        let d = probe_datagram(7, 1200);
        assert_eq!((d.len(), d[0], u16::from_be_bytes([d[1], d[2]])), (1200, T_PROBE, 7));

        let h = ShardHeader { frame: 7, index: 3, data: 30, parity: 6, frame_len: 41_666 };
        let mut v = Vec::new();
        h.write(&mut v);
        v.extend_from_slice(&[1, 2]);
        assert_eq!(ShardHeader::parse(&v), Some((h, &[1u8, 2][..])));

        let a = Ack { frame: 9, recovered: true, resent: false, prev_mask: 0xdead_beef };
        assert_eq!(Ack::parse(&a.encode()), Some(a));
        let a = Ack { frame: 9, recovered: false, resent: true, prev_mask: 0 };
        assert_eq!(Ack::parse(&a.encode()), Some(a));

        let q = Resend { frame: 12, shards: vec![0, 3, 7] };
        assert_eq!(Resend::parse(&q.encode()), Some(q));
        let all = Resend { frame: 13, shards: vec![] };
        assert_eq!(Resend::parse(&all.encode()), Some(all));

        let mut s = Vec::new();
        h.write(&mut s);
        s.extend_from_slice(&[1, 2]);
        let again = resent_shard(&s);
        assert_eq!((again[0], ShardHeader::parse(&again)), (T_SHARD_RESENT, Some((h, &[1u8, 2][..]))));

        let r = Report { shards_expected: 100, shards_received: 90, frames_ok: 2, frames_recovered: 1, frames_failed: 0, max_run: 4 };
        assert_eq!(Report::parse(&r.encode()), Some(r));
        assert_eq!(Report::parse(&r.encode()[..REPORT_LEN]), Some(Report { max_run: 0, ..r }), "an older receiver");
        assert!((r.shard_loss() - 0.1).abs() < 1e-9);
    }

    #[test]
    fn rejects_bad_shards() {
        let mut v = Vec::new();
        ShardHeader { frame: 1, index: 5, data: 3, parity: 2, frame_len: 10 }.write(&mut v);
        v.push(0);
        assert!(ShardHeader::parse(&v).is_none(), "index past data + parity");

        let mut overflow = Vec::new();
        ShardHeader { frame: 1, index: 0, data: 32_768, parity: 32_768, frame_len: 10 }.write(&mut overflow);
        overflow.push(0);
        assert!(ShardHeader::parse(&overflow).is_none(), "data + parity overflows u16");

        assert!(ShardHeader::parse(&[T_SHARD; 4]).is_none(), "too short");
    }

    #[test]
    fn ack_parses_legacy_and_extended_formats() {
        let mut legacy = [0u8; ACK_LEN];
        legacy[0] = T_ACK;
        legacy[1..5].copy_from_slice(&42u32.to_be_bytes());
        legacy[5] = 1;
        assert_eq!(Ack::parse(&legacy), Some(Ack { frame: 42, recovered: true, resent: false, prev_mask: 0 }));

        // Packets between ACK_LEN and ACK_EXT_LEN ignore partial trailing bytes.
        let mut mid = [0u8; ACK_LEN + 2];
        mid[..ACK_LEN].copy_from_slice(&legacy);
        mid[5] = 0;
        assert_eq!(Ack::parse(&mid), Some(Ack { frame: 42, recovered: false, resent: false, prev_mask: 0 }));

        let ext = Ack { frame: 42, recovered: true, resent: false, prev_mask: 0b1010_0101 };
        let encoded = ext.encode();
        assert_eq!(encoded.len(), ACK_EXT_LEN);
        assert_eq!(Ack::parse(&encoded), Some(ext));

        assert!(Ack::parse(&legacy[..ACK_LEN - 1]).is_none());
        let mut wrong = legacy;
        wrong[0] = T_REPORT;
        assert!(Ack::parse(&wrong).is_none());
    }
}
