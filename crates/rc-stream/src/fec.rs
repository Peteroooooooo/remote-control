//! Forward error correction for one video frame.
//!
//! A frame is cut into equal data shards that each fit in one datagram, and
//! Reed-Solomon parity shards are added. The receiver can rebuild the frame from
//! any `data` of the `data + parity` shards, so a lost datagram does not cost a
//! retransmission round trip.

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use crate::wire::{Report, SHARD_HDR, ShardHeader};

/// Upper bound on data shards per frame (about 45 MB at 1400-byte datagrams).
const MAX_DATA_SHARDS: usize = 32_768;

/// Parity shards for `data` data shards at `parity_pct` percent; at least
/// `min_parity` when FEC is on, so one-datagram frames are protected too.
pub fn parity_count(data: usize, parity_pct: u32, min_parity: u16) -> usize {
    if parity_pct == 0 {
        return 0;
    }
    (data * parity_pct as usize).div_ceil(100).max(min_parity as usize).min(MAX_DATA_SHARDS)
}

/// Splits `data` into datagrams of at most `max_datagram` bytes (header included),
/// data shards first, then parity shards.
///
/// Panics if `max_datagram` leaves no room for payload or the frame would need
/// more than 32 768 data shards.
pub fn encode_frame(frame: u32, data: &[u8], max_datagram: usize, parity_pct: u32, min_parity: u16) -> Vec<Vec<u8>> {
    assert!(max_datagram >= SHARD_HDR + 2, "datagram too small");
    // The Reed-Solomon library needs shard sizes that are a multiple of 2.
    let max_shard = (max_datagram - SHARD_HDR) & !1;
    let k = data.len().div_ceil(max_shard).max(1);
    assert!(k <= MAX_DATA_SHARDS, "frame too large");
    let shard_len = (data.len().div_ceil(k).max(2) + 1) & !1;
    let m = parity_count(k, parity_pct, min_parity);

    let mut shards: Vec<Vec<u8>> = (0..k)
        .map(|i| {
            let start = (i * shard_len).min(data.len());
            let end = (start + shard_len).min(data.len());
            let mut s = data[start..end].to_vec();
            s.resize(shard_len, 0);
            s
        })
        .collect();
    if m > 0 {
        let parity = reed_solomon_simd::encode(k, m, &shards).expect("shard counts and sizes are valid");
        shards.extend(parity);
    }

    shards
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            let mut d = Vec::with_capacity(SHARD_HDR + s.len());
            ShardHeader { frame, index: i as u16, data: k as u16, parity: m as u16, frame_len: data.len() as u32 }
                .write(&mut d);
            d.extend_from_slice(&s);
            d
        })
        .collect()
}

/// A frame the receiver can hand to the decoder.
#[derive(Debug)]
pub struct Completed {
    pub frame: u32,
    pub data: Vec<u8>,
    /// Parity shards were needed.
    pub recovered: bool,
}

struct Partial {
    data: u16,
    parity: u16,
    frame_len: u32,
    shard_len: usize,
    shards: Vec<Option<Vec<u8>>>,
    seen: Vec<bool>,
    received: u16,
    data_received: u16,
    /// When the latest shard of this frame came in.
    last_at: Instant,
    /// `Some(recovered)` once the frame has been handed out.
    done: Option<bool>,
}

impl Partial {
    fn assemble(&mut self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.data as usize * self.shard_len);
        for s in self.shards.iter().take(self.data as usize) {
            out.extend_from_slice(s.as_deref().unwrap_or_default());
        }
        out.truncate(self.frame_len as usize);
        self.shards.iter_mut().for_each(|s| *s = None);
        out
    }

    fn try_recover(&mut self) -> Option<Vec<u8>> {
        let k = self.data as usize;
        let originals = self.shards[..k].iter().enumerate().filter_map(|(i, s)| s.as_deref().map(|s| (i, s)));
        let parity = self.shards[k..].iter().enumerate().filter_map(|(i, s)| s.as_deref().map(|s| (i, s)));
        let restored = reed_solomon_simd::decode(k, self.parity as usize, originals, parity).ok()?;
        for (i, s) in restored {
            self.shards[i] = Some(s);
        }
        Some(self.assemble())
    }
}

/// Collects shards and hands out each frame as soon as it can be decoded.
/// Nothing is held back for ordering: a frame that completes is returned at once.
pub struct Assembler {
    frames: HashMap<u32, Partial>,
    retired: HashSet<u32>,
    newest_retired: u32,
    linger: Duration,
    pending: Report,
}

impl Assembler {
    /// `linger`: how long a frame is kept after its latest shard, to count late
    /// shards, before it goes into the loss report. Counting from the latest
    /// shard rather than the first lets a large frame on a slow link finish
    /// arriving instead of being written off half way.
    pub fn new(linger: Duration) -> Self {
        Assembler { frames: HashMap::new(), retired: HashSet::new(), newest_retired: 0, linger, pending: Report::default() }
    }

    pub fn push(&mut self, datagram: &[u8], now: Instant) -> Option<Completed> {
        let (h, body) = ShardHeader::parse(datagram)?;
        if self.retired.contains(&h.frame) {
            return None;
        }
        let total = (h.data + h.parity) as usize;
        let p = self.frames.entry(h.frame).or_insert_with(|| Partial {
            data: h.data,
            parity: h.parity,
            frame_len: h.frame_len,
            shard_len: body.len(),
            shards: vec![None; total],
            seen: vec![false; total],
            received: 0,
            data_received: 0,
            last_at: now,
            done: None,
        });
        let i = h.index as usize;
        if p.data != h.data || p.parity != h.parity || p.shard_len != body.len() || p.seen[i] {
            return None;
        }
        p.seen[i] = true;
        p.last_at = now;
        p.received += 1;
        if i < p.data as usize {
            p.data_received += 1;
        }
        if p.done.is_some() {
            return None;
        }
        p.shards[i] = Some(body.to_vec());
        let (data, recovered) = if p.data_received == p.data {
            (p.assemble(), false)
        } else if p.parity > 0 && p.received >= p.data {
            (p.try_recover()?, true)
        } else {
            return None;
        };
        p.done = Some(recovered);
        Some(Completed { frame: h.frame, data, recovered })
    }

    /// Moves frames with no shard for more than `linger` into the report.
    pub fn expire(&mut self, now: Instant) {
        let linger = self.linger;
        let old: Vec<u32> =
            self.frames.iter().filter(|(_, p)| now.duration_since(p.last_at) > linger).map(|(f, _)| *f).collect();
        for f in old {
            let p = self.frames.remove(&f).expect("listed above");
            let r = &mut self.pending;
            r.shards_expected += (p.data + p.parity) as u32;
            r.shards_received += p.received as u32;
            match p.done {
                Some(false) => r.frames_ok += 1,
                Some(true) => r.frames_recovered += 1,
                None => r.frames_failed += 1,
            }
            self.retired.insert(f);
            self.newest_retired = self.newest_retired.max(f);
        }
        if self.retired.len() > 4096 {
            let floor = self.newest_retired.saturating_sub(2048);
            self.retired.retain(|f| *f >= floor);
        }
    }

    /// Counts since the last call.
    pub fn take_report(&mut self) -> Report {
        std::mem::take(&mut self.pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_bytes(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 + 3) as u8).collect()
    }

    #[test]
    fn splits_to_fit_datagrams() {
        let data = frame_bytes(41_666);
        let pkts = encode_frame(1, &data, 1200, 20, 2);
        let (h, _) = ShardHeader::parse(&pkts[0]).unwrap();
        assert!(pkts.iter().all(|p| p.len() <= 1200));
        assert_eq!(h.data as usize + h.parity as usize, pkts.len());
        assert_eq!(h.parity as usize, (h.data as usize * 20).div_ceil(100));
    }

    #[test]
    fn decodes_in_order_without_parity() {
        let data = frame_bytes(5000);
        let pkts = encode_frame(3, &data, 1200, 0, 2);
        let mut a = Assembler::new(Duration::from_millis(100));
        let now = Instant::now();
        let mut out = None;
        for p in &pkts {
            if let Some(c) = a.push(p, now) {
                out = Some(c);
            }
        }
        let c = out.expect("complete");
        assert_eq!(c.data, data);
        assert!(!c.recovered);
    }

    #[test]
    fn recovers_from_losses_up_to_parity() {
        let data = frame_bytes(41_666);
        let pkts = encode_frame(5, &data, 1400, 20, 2);
        let (h, _) = ShardHeader::parse(&pkts[0]).unwrap();
        let lose = h.parity as usize;
        let mut a = Assembler::new(Duration::from_millis(100));
        let t0 = Instant::now();
        let mut out = None;
        // Drop the first `lose` data shards, then deliver the rest in reverse order.
        for p in pkts.iter().skip(lose).rev() {
            if let Some(c) = a.push(p, t0) {
                out = Some(c);
            }
        }
        let c = out.expect("recovered");
        assert!(c.recovered);
        assert_eq!(c.data, data);
        a.expire(t0 + Duration::from_millis(200));
        let r = a.take_report();
        assert_eq!((r.frames_ok, r.frames_recovered, r.frames_failed), (0, 1, 0));
        assert_eq!(r.shards_expected as usize, pkts.len());
        assert_eq!(r.shards_received as usize, pkts.len() - lose);
    }

    #[test]
    fn slow_large_frame_is_not_written_off_while_arriving() {
        let data = frame_bytes(60_000);
        let pkts = encode_frame(4, &data, 1400, 0, 2);
        let mut a = Assembler::new(Duration::from_millis(100));
        let t0 = Instant::now();
        let mut out = None;
        // One shard every 10 ms: the whole frame takes far longer than `linger`.
        for (i, p) in pkts.iter().enumerate() {
            let t = t0 + Duration::from_millis(10 * i as u64);
            a.expire(t);
            if let Some(c) = a.push(p, t) {
                out = Some(c);
            }
        }
        assert!(pkts.len() * 10 > 300);
        assert_eq!(out.expect("completes").data, data);
        assert_eq!(a.take_report().frames_failed, 0);
    }

    #[test]
    fn reports_failed_frame_when_too_many_lost() {
        let data = frame_bytes(10_000);
        let pkts = encode_frame(9, &data, 1400, 20, 2);
        let (h, _) = ShardHeader::parse(&pkts[0]).unwrap();
        let mut a = Assembler::new(Duration::from_millis(100));
        let t0 = Instant::now();
        for p in pkts.iter().skip(h.parity as usize + 1) {
            assert!(a.push(p, t0).is_none());
        }
        a.expire(t0 + Duration::from_millis(200));
        assert_eq!(a.take_report().frames_failed, 1);
        // Shards arriving after the frame was reported are ignored.
        assert!(a.push(&pkts[0], t0 + Duration::from_millis(300)).is_none());
        a.expire(t0 + Duration::from_millis(500));
        assert_eq!(a.take_report(), Report::default());
    }

    #[test]
    fn small_frame_gets_min_parity() {
        let pkts = encode_frame(1, &[1, 2, 3], 1400, 20, 2);
        assert_eq!(pkts.len(), 3);
        let mut a = Assembler::new(Duration::from_millis(100));
        let c = a.push(&pkts[2], Instant::now()).expect("one parity shard is enough for one data shard");
        assert_eq!(c.data, vec![1, 2, 3]);
    }
}
