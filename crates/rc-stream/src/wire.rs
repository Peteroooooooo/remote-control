//! Datagram layout. All integers are big-endian.
//!
//! - Shard (sender -> receiver): type, frame, shard index, data shard count,
//!   parity shard count, frame length in bytes, then the shard bytes.
//!   Indexes `0..data` are data shards, `data..data + parity` are parity shards.
//! - Ack (receiver -> sender): type, frame, recovered flag. Sent as soon as a frame
//!   can be decoded.
//! - Report (receiver -> sender, every 100 ms): shard and frame counts for frames
//!   the receiver has finished with since the last report.

pub const T_SHARD: u8 = 0x10;
pub const T_ACK: u8 = 0x11;
pub const T_REPORT: u8 = 0x12;

pub const SHARD_HDR: usize = 1 + 4 + 2 + 2 + 2 + 4;
pub const ACK_LEN: usize = 1 + 4 + 1;
pub const REPORT_LEN: usize = 1 + 4 + 4 + 2 + 2 + 2;

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

    /// Returns the header and the shard bytes, or `None` if this is not a valid shard.
    pub fn parse(b: &[u8]) -> Option<(ShardHeader, &[u8])> {
        if b.len() <= SHARD_HDR || b[0] != T_SHARD {
            return None;
        }
        let h = ShardHeader {
            frame: u32::from_be_bytes(b[1..5].try_into().ok()?),
            index: u16::from_be_bytes(b[5..7].try_into().ok()?),
            data: u16::from_be_bytes(b[7..9].try_into().ok()?),
            parity: u16::from_be_bytes(b[9..11].try_into().ok()?),
            frame_len: u32::from_be_bytes(b[11..15].try_into().ok()?),
        };
        if h.data == 0 || h.index >= h.data.saturating_add(h.parity) {
            return None;
        }
        Some((h, &b[SHARD_HDR..]))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ack {
    pub frame: u32,
    /// The frame needed parity shards to decode.
    pub recovered: bool,
}

impl Ack {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(ACK_LEN);
        v.push(T_ACK);
        v.extend_from_slice(&self.frame.to_be_bytes());
        v.push(self.recovered as u8);
        v
    }

    pub fn parse(b: &[u8]) -> Option<Ack> {
        if b.len() < ACK_LEN || b[0] != T_ACK {
            return None;
        }
        Some(Ack { frame: u32::from_be_bytes(b[1..5].try_into().ok()?), recovered: b[5] != 0 })
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
}

impl Report {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(REPORT_LEN);
        v.push(T_REPORT);
        v.extend_from_slice(&self.shards_expected.to_be_bytes());
        v.extend_from_slice(&self.shards_received.to_be_bytes());
        v.extend_from_slice(&self.frames_ok.to_be_bytes());
        v.extend_from_slice(&self.frames_recovered.to_be_bytes());
        v.extend_from_slice(&self.frames_failed.to_be_bytes());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let h = ShardHeader { frame: 7, index: 3, data: 30, parity: 6, frame_len: 41_666 };
        let mut v = Vec::new();
        h.write(&mut v);
        v.extend_from_slice(&[1, 2]);
        assert_eq!(ShardHeader::parse(&v), Some((h, &[1u8, 2][..])));

        let a = Ack { frame: 9, recovered: true };
        assert_eq!(Ack::parse(&a.encode()), Some(a));

        let r = Report { shards_expected: 100, shards_received: 90, frames_ok: 2, frames_recovered: 1, frames_failed: 0 };
        assert_eq!(Report::parse(&r.encode()), Some(r));
        assert!((r.shard_loss() - 0.1).abs() < 1e-9);
    }

    #[test]
    fn rejects_bad_shards() {
        let mut v = Vec::new();
        ShardHeader { frame: 1, index: 5, data: 3, parity: 2, frame_len: 10 }.write(&mut v);
        v.push(0);
        assert!(ShardHeader::parse(&v).is_none(), "index past data + parity");
        assert!(ShardHeader::parse(&[T_SHARD; 4]).is_none(), "too short");
    }
}
