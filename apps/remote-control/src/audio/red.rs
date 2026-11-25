use std::collections::VecDeque;

use bytes::Bytes;
use rc_stream::wire::{T_AUDIO, T_AUDIO_RED};

/// Maximum total datagram size (in bytes) when attaching redundant audio copies.
pub const MAX_RED_DATAGRAM: usize = 1150;

/// Duration (in ms) that a path switch holds redundancy at level 2.
pub const PATH_BOOST_MS: u64 = 3000;

const SILENCE_GAP_MS: u64 = 100;
const DECAY_2_TO_1_MS: u64 = 8000;
const DECAY_1_TO_0_MS: u64 = 6000;
const TIMEOUT_ESCALATE_MS: u64 = 2000;

/// A single decoded Opus frame extracted from an audio datagram.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedFrame {
    pub seq: u16,
    pub ch: u8,
    pub payload: Bytes,
    /// `false` for the primary (newest) frame in the datagram, `true` for historical copies.
    pub is_redundant: bool,
}

/// Stateful encoder that bundles up to 2 prior Opus frames into `T_AUDIO_RED` datagrams,
/// falling back to plain `T_AUDIO` when redundancy is 0, unavailable, or exceeds MTU.
#[derive(Debug, Default)]
pub struct RedEncoder {
    /// Recent frames stored newest-first (`[seq - 1, seq - 2]`).
    history: VecDeque<Vec<u8>>,
    last_seq: Option<u16>,
    last_ms: Option<u64>,
    last_ch: Option<u8>,
}

impl RedEncoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.history.clear();
        self.last_seq = None;
        self.last_ms = None;
        self.last_ch = None;
    }

    pub fn encode(
        &mut self,
        seq: u16,
        ch: u8,
        payload: &[u8],
        level: u8,
        now_ms: u64,
    ) -> Vec<u8> {
        let discontinuity = self.last_seq.is_some_and(|prev| seq != prev.wrapping_add(1))
            || self.last_ms.is_some_and(|prev| now_ms.saturating_sub(prev) > SILENCE_GAP_MS)
            || self.last_ch.is_some_and(|prev| prev != ch);
        if discontinuity {
            self.history.clear();
        }

        let max_copies = (level as usize).min(2).min(self.history.len());
        let mut copies = 0usize;
        let mut red_len = 5usize.saturating_add(2).saturating_add(payload.len());

        if !payload.is_empty() && payload.len() <= u16::MAX as usize {
            for copy in self.history.iter().take(max_copies) {
                if copy.is_empty() || copy.len() > u16::MAX as usize {
                    break;
                }
                let next_len = red_len.saturating_add(2).saturating_add(copy.len());
                if next_len > MAX_RED_DATAGRAM {
                    break;
                }
                red_len = next_len;
                copies += 1;
            }
        }

        let out = if copies == 0 {
            let mut pkt = Vec::with_capacity(4 + payload.len());
            pkt.push(T_AUDIO);
            pkt.extend_from_slice(&seq.to_be_bytes());
            pkt.push(ch);
            pkt.extend_from_slice(payload);
            pkt
        } else {
            let count = (1 + copies) as u8;
            let mut pkt = Vec::with_capacity(red_len);
            pkt.push(T_AUDIO_RED);
            pkt.extend_from_slice(&seq.to_be_bytes());
            pkt.push(ch);
            pkt.push(count);
            pkt.extend_from_slice(&(payload.len() as u16).to_be_bytes());
            pkt.extend_from_slice(payload);
            for copy in self.history.iter().take(copies) {
                pkt.extend_from_slice(&(copy.len() as u16).to_be_bytes());
                pkt.extend_from_slice(copy);
            }
            pkt
        };

        if !payload.is_empty() {
            if self.history.len() == 2 {
                self.history.pop_back();
            }
            self.history.push_front(payload.to_vec());
        } else {
            self.history.clear();
        }
        self.last_seq = Some(seq);
        self.last_ms = Some(now_ms);
        self.last_ch = Some(ch);

        out
    }
}

/// Decodes a `T_AUDIO` or `T_AUDIO_RED` datagram into frames ordered oldest-to-newest.
pub fn decode_audio_datagram(d: &Bytes) -> Option<Vec<RedFrame>> {
    let tag = *d.first()?;
    match tag {
        T_AUDIO => {
            if d.len() < 4 {
                return None;
            }
            let seq = u16::from_be_bytes([d[1], d[2]]);
            let ch = d[3].clamp(1, 2);
            Some(vec![RedFrame {
                seq,
                ch,
                payload: d.slice(4..),
                is_redundant: false,
            }])
        }
        T_AUDIO_RED => {
            if d.len() < 5 {
                return None;
            }
            let seq = u16::from_be_bytes([d[1], d[2]]);
            let ch = d[3].clamp(1, 2);
            let count = d[4];
            if count == 0 || count > 3 {
                return None;
            }

            let mut frames = Vec::with_capacity(count as usize);
            let mut cursor = 5usize;
            for i in 0..count as usize {
                if d.len() - cursor < 2 {
                    return None;
                }
                let len = u16::from_be_bytes([d[cursor], d[cursor + 1]]) as usize;
                cursor += 2;
                if len == 0 || d.len() - cursor < len {
                    return None;
                }
                frames.push(RedFrame {
                    seq: seq.wrapping_sub(i as u16),
                    ch,
                    payload: d.slice(cursor..cursor + len),
                    is_redundant: i > 0,
                });
                cursor += len;
            }

            if cursor != d.len() {
                return None;
            }

            frames.reverse();
            Some(frames)
        }
        _ => None,
    }
}

/// Adaptive redundancy level controller (`0..=2`) with hysteresis and path-switch boost.
#[derive(Debug, Default)]
pub struct RedLevelCtrl {
    level: u8,
    last_high_loss_ms: Option<u64>,
    last_any_loss_ms: Option<u64>,
    path_boost_until_ms: Option<u64>,
    last_timeout_ms: Option<u64>,
}

impl RedLevelCtrl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn level(&self) -> u8 {
        self.level
    }

    pub fn on_path_change(&mut self, now_ms: u64) {
        self.level = 2;
        let until = now_ms.saturating_add(PATH_BOOST_MS);
        self.path_boost_until_ms = Some(
            self.path_boost_until_ms
                .map_or(until, |prev| prev.max(until)),
        );
    }

    pub fn on_report(&mut self, expected: u32, lost: u32, max_run: u8, now_ms: u64) {
        let loss = if expected > 0 {
            lost as f64 / expected as f64
        } else {
            0.0
        };

        if loss >= 0.05 || max_run >= 2 {
            self.level = 2;
            self.last_high_loss_ms = Some(now_ms);
            self.last_any_loss_ms = Some(now_ms);
        } else if loss >= 0.01 || lost > 0 || max_run >= 1 {
            self.level = self.level.max(1);
            self.last_any_loss_ms = Some(now_ms);
            if loss >= 0.02 {
                self.last_high_loss_ms = Some(now_ms);
            }
        }

        self.tick(now_ms);
    }

    pub fn on_timeout(&mut self, now_ms: u64) {
        let repeat = self
            .last_timeout_ms
            .is_some_and(|prev| now_ms.saturating_sub(prev) <= TIMEOUT_ESCALATE_MS);
        self.last_timeout_ms = Some(now_ms);
        self.last_any_loss_ms = Some(now_ms);
        if repeat {
            self.level = 2;
            self.last_high_loss_ms = Some(now_ms);
        } else {
            self.level = self.level.max(1);
        }
    }

    pub fn tick(&mut self, now_ms: u64) -> u8 {
        if let Some(until) = self.path_boost_until_ms {
            if now_ms < until {
                return self.level;
            }
            self.path_boost_until_ms = None;
        }

        if self.level == 2 {
            let can_step_down = self
                .last_high_loss_ms
                .is_none_or(|t| now_ms.saturating_sub(t) >= DECAY_2_TO_1_MS);
            if can_step_down {
                self.level = 1;
                // Hold level 1 for at least DECAY_1_TO_0_MS before stepping down to 0.
                self.last_any_loss_ms = Some(
                    self.last_any_loss_ms
                        .map_or(now_ms, |prev| prev.max(now_ms)),
                );
            }
        } else if self.level == 1 {
            let can_step_down = self
                .last_any_loss_ms
                .is_none_or(|t| now_ms.saturating_sub(t) >= DECAY_1_TO_0_MS);
            if can_step_down {
                self.level = 0;
            }
        }

        self.level
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_zero_emits_plain_t_audio() {
        let mut enc = RedEncoder::new();
        let p1 = enc.encode(10, 2, b"abc", 0, 100);
        assert_eq!(p1, vec![T_AUDIO, 0, 10, 2, b'a', b'b', b'c']);

        let p2 = enc.encode(11, 2, b"defg", 0, 120);
        assert_eq!(p2, vec![T_AUDIO, 0, 11, 2, b'd', b'e', b'f', b'g']);

        let decoded = decode_audio_datagram(&Bytes::from(p2)).unwrap();
        assert_eq!(
            decoded,
            vec![RedFrame {
                seq: 11,
                ch: 2,
                payload: Bytes::from_static(b"defg"),
                is_redundant: false,
            }]
        );
    }

    #[test]
    fn level_one_and_two_round_trip_with_wraparound() {
        let mut enc = RedEncoder::new();

        // First packet has no history -> plain T_AUDIO.
        let p0 = enc.encode(u16::MAX - 1, 2, b"f0", 2, 0);
        assert_eq!(p0[0], T_AUDIO);

        // Second packet at level 1 -> primary + 1 historical copy.
        let p1 = enc.encode(u16::MAX, 2, b"f1", 1, 20);
        assert_eq!(p1[0], T_AUDIO_RED);
        let d1 = decode_audio_datagram(&Bytes::from(p1)).unwrap();
        assert_eq!(
            d1,
            vec![
                RedFrame {
                    seq: u16::MAX - 1,
                    ch: 2,
                    payload: Bytes::from_static(b"f0"),
                    is_redundant: true,
                },
                RedFrame {
                    seq: u16::MAX,
                    ch: 2,
                    payload: Bytes::from_static(b"f1"),
                    is_redundant: false,
                },
            ]
        );

        // Third packet wraps seq to 0 at level 2 -> oldest-to-newest: [MAX-1, MAX, 0].
        let p2 = enc.encode(0, 2, b"f2", 2, 40);
        assert_eq!(p2[0], T_AUDIO_RED);
        let d2 = decode_audio_datagram(&Bytes::from(p2)).unwrap();
        assert_eq!(
            d2,
            vec![
                RedFrame {
                    seq: u16::MAX - 1,
                    ch: 2,
                    payload: Bytes::from_static(b"f0"),
                    is_redundant: true,
                },
                RedFrame {
                    seq: u16::MAX,
                    ch: 2,
                    payload: Bytes::from_static(b"f1"),
                    is_redundant: true,
                },
                RedFrame {
                    seq: 0,
                    ch: 2,
                    payload: Bytes::from_static(b"f2"),
                    is_redundant: false,
                },
            ]
        );

        // Fourth packet wraps seq to 1 at level 2 -> oldest-to-newest: [MAX, 0, 1].
        let p3 = enc.encode(1, 2, b"f3", 2, 60);
        let d3 = decode_audio_datagram(&Bytes::from(p3)).unwrap();
        assert_eq!(
            d3,
            vec![
                RedFrame {
                    seq: u16::MAX,
                    ch: 2,
                    payload: Bytes::from_static(b"f1"),
                    is_redundant: true,
                },
                RedFrame {
                    seq: 0,
                    ch: 2,
                    payload: Bytes::from_static(b"f2"),
                    is_redundant: true,
                },
                RedFrame {
                    seq: 1,
                    ch: 2,
                    payload: Bytes::from_static(b"f3"),
                    is_redundant: false,
                },
            ]
        );
    }

    #[test]
    fn silence_gap_seq_jump_channel_switch_and_reset_clear_history() {
        let mut enc = RedEncoder::new();

        // Build up 2 frames of history.
        assert_eq!(enc.encode(1, 2, b"a", 2, 0)[0], T_AUDIO);
        assert_eq!(enc.encode(2, 2, b"b", 2, 20)[0], T_AUDIO_RED);

        // Exactly 100ms is NOT a silence gap.
        let ok = enc.encode(3, 2, b"c", 2, 120);
        assert_eq!(ok[0], T_AUDIO_RED);
        assert_eq!(decode_audio_datagram(&Bytes::from(ok)).unwrap().len(), 3);

        // >100ms clears history: first post-silence packet is plain T_AUDIO.
        let after_gap = enc.encode(4, 2, b"d", 2, 221);
        assert_eq!(after_gap[0], T_AUDIO);
        let after_gap2 = enc.encode(5, 2, b"e", 2, 241);
        assert_eq!(after_gap2[0], T_AUDIO_RED);
        assert_eq!(
            decode_audio_datagram(&Bytes::from(after_gap2))
                .unwrap()
                .iter()
                .map(|f| f.seq)
                .collect::<Vec<_>>(),
            vec![4, 5]
        );

        // Seq jump clears history.
        let after_jump = enc.encode(7, 2, b"f", 2, 261);
        assert_eq!(after_jump[0], T_AUDIO);

        // Channel switch clears history.
        let after_ch = enc.encode(8, 1, b"g", 2, 281);
        assert_eq!(after_ch[0], T_AUDIO);

        // Explicit reset clears history.
        let p9 = enc.encode(9, 1, b"h", 2, 301);
        assert_eq!(p9[0], T_AUDIO_RED);
        enc.reset();
        let p10 = enc.encode(10, 1, b"i", 2, 321);
        assert_eq!(p10[0], T_AUDIO);
    }

    #[test]
    fn oversized_payload_respects_max_red_datagram() {
        let mut enc = RedEncoder::new();

        // Three 500-byte frames: primary (5+2+500 = 507) + 1 copy (502) = 1009 <= 1150,
        // adding a 2nd copy (502) = 1511 > 1150 -> truncates to 1 historical copy (count = 2).
        let big = vec![0xAAu8; 500];
        let _ = enc.encode(1, 2, &big, 2, 0);
        let _ = enc.encode(2, 2, &big, 2, 20);
        let pkt = enc.encode(3, 2, &big, 2, 40);
        assert!(pkt.len() <= MAX_RED_DATAGRAM);
        assert_eq!(pkt[0], T_AUDIO_RED);
        let decoded = decode_audio_datagram(&Bytes::from(pkt)).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].seq, 2);
        assert!(decoded[0].is_redundant);
        assert_eq!(decoded[1].seq, 3);
        assert!(!decoded[1].is_redundant);

        // If even primary + 1 historical copy exceeds MAX_RED_DATAGRAM, emit plain T_AUDIO.
        let mut enc2 = RedEncoder::new();
        let huge = vec![0xBBu8; 700];
        let _ = enc2.encode(10, 2, &huge, 2, 0);
        let pkt2 = enc2.encode(11, 2, &huge, 2, 20);
        assert_eq!(pkt2[0], T_AUDIO);
        assert_eq!(pkt2.len(), 4 + 700);
    }

    #[test]
    fn malformed_packets_return_none() {
        assert!(decode_audio_datagram(&Bytes::new()).is_none());
        assert!(decode_audio_datagram(&Bytes::from_static(&[0x00, 0, 1, 2])).is_none());
        assert!(decode_audio_datagram(&Bytes::from_static(&[T_AUDIO, 0, 1])).is_none());
        assert!(decode_audio_datagram(&Bytes::from_static(&[T_AUDIO_RED, 0, 1, 2])).is_none());

        // count == 0 and count == 4
        assert!(decode_audio_datagram(&Bytes::from_static(&[T_AUDIO_RED, 0, 1, 2, 0])).is_none());
        assert!(
            decode_audio_datagram(&Bytes::from_static(&[
                T_AUDIO_RED, 0, 1, 2, 4, 0, 1, 1, 0, 1, 2, 0, 1, 3, 0, 1, 4
            ]))
            .is_none()
        );

        // Zero-length frame inside RED
        assert!(
            decode_audio_datagram(&Bytes::from_static(&[T_AUDIO_RED, 0, 1, 2, 1, 0, 0])).is_none()
        );

        // Truncated length prefix / truncated payload
        assert!(
            decode_audio_datagram(&Bytes::from_static(&[T_AUDIO_RED, 0, 1, 2, 1, 0])).is_none()
        );
        assert!(
            decode_audio_datagram(&Bytes::from_static(&[T_AUDIO_RED, 0, 1, 2, 1, 0, 3, 1, 2]))
                .is_none()
        );

        // Trailing junk bytes after valid RED frames
        assert!(
            decode_audio_datagram(&Bytes::from_static(&[T_AUDIO_RED, 0, 1, 2, 1, 0, 1, 9, 99]))
                .is_none()
        );
    }

    #[test]
    fn level_ctrl_escalation_hysteresis_and_path_boost() {
        let mut ctrl = RedLevelCtrl::new();
        assert_eq!(ctrl.level(), 0);

        // Minor loss -> level 1.
        ctrl.on_report(100, 1, 1, 1000);
        assert_eq!(ctrl.level(), 1);

        // Clean for 5999 ms -> still 1; at 6000 ms -> decays to 0.
        ctrl.on_report(100, 0, 0, 6999);
        assert_eq!(ctrl.level(), 1);
        ctrl.on_report(100, 0, 0, 7000);
        assert_eq!(ctrl.level(), 0);

        // High loss (>= 5% or max_run >= 2) -> immediate level 2.
        ctrl.on_report(100, 5, 1, 10_000);
        assert_eq!(ctrl.level(), 2);

        // Moderate loss (2..5%) keeps level 2 from decaying.
        ctrl.on_report(100, 3, 1, 15_000);
        assert_eq!(ctrl.level(), 2);
        assert_eq!(ctrl.tick(22_999), 2);
        assert_eq!(ctrl.tick(23_000), 1);

        // Then 6000 ms clean -> decays 1 -> 0.
        assert_eq!(ctrl.tick(28_999), 1);
        assert_eq!(ctrl.tick(29_000), 0);

        // Path switch boosts to level 2 for at least 3000 ms.
        ctrl.on_path_change(30_000);
        assert_eq!(ctrl.level(), 2);
        ctrl.on_report(100, 0, 0, 32_999);
        assert_eq!(ctrl.level(), 2);
        assert_eq!(ctrl.tick(33_000), 1);
        assert_eq!(ctrl.tick(39_000), 0);

        // Single timeout -> 1; repeat timeout within 2000 ms -> 2.
        ctrl.on_timeout(40_000);
        assert_eq!(ctrl.level(), 1);
        ctrl.on_timeout(41_500);
        assert_eq!(ctrl.level(), 2);
        assert_eq!(ctrl.tick(49_499), 2);
        assert_eq!(ctrl.tick(49_500), 1);
        assert_eq!(ctrl.tick(55_500), 0);
    }
}
