//! Frame reordering, loss detection, shard resend triggering, and keyframe requests.

use std::{
    collections::{BTreeMap, HashSet},
    time::{Duration, Instant},
};

use crate::proto::FrameHeader;

pub const FRAME_LINGER: Duration = Duration::from_millis(150);
const GAP_WAIT: Duration = Duration::from_millis(12);
const RESEND_AFTER: Duration = Duration::from_millis(3);
const TAIL_WAIT: Duration = Duration::from_millis(50);
pub const RESEND_SLACK: Duration = Duration::from_millis(25);
const STUCK_WAIT: Duration = Duration::from_millis(120);
const KEY_REQUEST_GAP: Duration = Duration::from_millis(300);
pub const CHECK_EVERY: Duration = Duration::from_millis(4);
pub const REPORT_EVERY: Duration = Duration::from_millis(100);
pub const PROBE_DONE: Duration = Duration::from_millis(50);
const BEHIND_MAX: u32 = 8;

pub struct Delivered {
    pub header: FrameHeader,
    pub data: Vec<u8>,
    pub complete_at: Instant,
    pub frame: u32,
}

#[derive(Default)]
pub struct Reorder {
    next: Option<u32>,
    pending: BTreeMap<u32, Delivered>,
    gap_since: Option<Instant>,
    incomplete: BTreeMap<u32, Instant>,
    need_key: bool,
    first_seen: Option<Instant>,
    last_request: Option<Instant>,
    pub key_requests: u64,
    pub lost: u64,
    pub rtt: Duration,
    pub resend: bool,
    behind: BTreeMap<u32, Instant>,
    asked: HashSet<u32>,
    newest: Option<u32>,
}

impl Reorder {
    pub fn on_shard(&mut self, frame: u32, now: Instant) {
        self.first_seen.get_or_insert(now);
        if self.next.is_some_and(|n| frame < n) || self.pending.contains_key(&frame) {
            return;
        }
        self.incomplete.insert(frame, now);
        if let Some(n) = self.next
            && self.newest.is_none_or(|m| frame > m)
            && frame - n <= BEHIND_MAX
        {
            for f in n..frame {
                if !self.pending.contains_key(&f) {
                    self.behind.entry(f).or_insert(now);
                }
            }
        }
        if self.newest.is_none_or(|m| frame > m) {
            self.newest = Some(frame);
        }
    }

    pub fn resends(&mut self, now: Instant) -> Vec<u32> {
        if !self.resend || self.need_key {
            self.behind.clear();
            return Vec::new();
        }
        if let Some(n) = self.next {
            self.behind.retain(|f, _| *f >= n);
            self.asked.retain(|f| *f >= n);
        }
        let mut out: Vec<u32> = self
            .behind
            .iter()
            .filter(|&(f, t)| now.duration_since(*t) >= RESEND_AFTER && !self.asked.contains(f))
            .map(|(f, _)| *f)
            .collect();
        if let Some((&f, &t)) = self.incomplete.last_key_value()
            && self.newest == Some(f)
            && now.duration_since(t) >= TAIL_WAIT
            && !self.asked.contains(&f)
        {
            out.push(f);
        }
        self.asked.extend(&out);
        out
    }

    fn waits(&self, f: u32) -> (Duration, Duration) {
        if self.asked.contains(&f) {
            let back = self.rtt.mul_f64(1.5) + RESEND_SLACK;
            (
                GAP_WAIT.max(RESEND_AFTER + back),
                STUCK_WAIT.max(TAIL_WAIT + back),
            )
        } else {
            (GAP_WAIT, STUCK_WAIT)
        }
    }

    pub fn push(&mut self, frame: u32, d: Delivered, now: Instant) -> Vec<Delivered> {
        self.incomplete.remove(&frame);
        self.behind.remove(&frame);
        self.asked.remove(&frame);
        let mut out = Vec::new();
        let behind = self.next.is_some_and(|n| frame < n);
        if d.header.key && !behind {
            self.pending.retain(|f, _| *f > frame);
            self.need_key = false;
            self.last_request = None;
            out.push(d);
            self.next = Some(frame.wrapping_add(1));
        } else if self.need_key || behind {
            return out;
        } else {
            match self.next {
                Some(n) if frame == n => {
                    out.push(d);
                    self.next = Some(n.wrapping_add(1));
                }
                Some(_) => {
                    self.pending.insert(frame, d);
                }
                None => return out,
            }
        }
        while let Some(n) = self.next {
            let Some(d) = self.pending.remove(&n) else {
                break;
            };
            out.push(d);
            self.next = Some(n.wrapping_add(1));
        }
        self.gap_since = match (self.pending.is_empty(), out.is_empty()) {
            (true, _) => None,
            (false, false) => Some(now),
            (false, true) => Some(self.gap_since.unwrap_or(now)),
        };
        out
    }

    pub fn tick(&mut self, now: Instant) -> bool {
        if let Some(n) = self.next {
            self.incomplete.retain(|f, _| *f >= n);
        }
        let gap = self.gap_since.is_some_and(|t| {
            let missing = self.next.map_or(GAP_WAIT, |n| self.waits(n).0);
            now.duration_since(t) > missing
        });
        let stuck = self
            .incomplete
            .iter()
            .any(|(f, t)| now.duration_since(*t) > self.waits(*f).1);
        if (gap || stuck) && !self.need_key {
            self.need_key = true;
            self.lost += 1;
        }
        if gap || stuck {
            self.pending.clear();
            self.incomplete.clear();
            self.behind.clear();
            self.asked.clear();
            self.gap_since = None;
        }
        let no_start = self.next.is_none()
            && self
                .first_seen
                .is_some_and(|t| now.duration_since(t) > STUCK_WAIT);
        let want = self.need_key || no_start;
        let gap_wait = KEY_REQUEST_GAP.max(self.rtt * 2);
        if want && self.last_request.is_none_or(|t| now.duration_since(t) >= gap_wait) {
            self.last_request = Some(now);
            self.key_requests += 1;
            return true;
        }
        false
    }
}
