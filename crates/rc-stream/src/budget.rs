//! Decides how big the next video frame may be while earlier frames are still
//! waiting to go out.
//!
//! Skipping a whole frame drops the frame rate; sending it anyway makes every
//! later frame wait behind it. In between, the encoder can be asked for a smaller
//! frame (lower quality for that frame only) that still fits under the queue limit.

/// What to do with the next frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Frame {
    /// Encode it with at most this many bytes.
    Send(usize),
    Skip,
}

#[derive(Clone, Copy, Debug)]
pub struct Budget {
    /// Queued bytes allowed right after this frame is handed over, in frames.
    pub queue_limit: f64,
    /// Smallest frame worth sending, as a share of a normal frame. 1.0 = never shrink.
    pub min_share: f64,
}

impl Budget {
    /// `normal`: bytes the current bitrate gives one frame. `queued`: bytes not sent yet.
    pub fn decide(&self, normal: usize, queued: usize) -> Frame {
        let limit = (normal as f64 * self.queue_limit) as usize;
        let size = limit.saturating_sub(queued).min(normal);
        if size == 0 || (size as f64) < normal as f64 * self.min_share - 0.5 {
            Frame::Skip
        } else {
            Frame::Send(size)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const N: usize = 40_000;

    #[test]
    fn full_frame_when_queue_is_short() {
        let b = Budget { queue_limit: 2.0, min_share: 0.25 };
        assert_eq!(b.decide(N, 0), Frame::Send(N));
        assert_eq!(b.decide(N, N), Frame::Send(N));
    }

    #[test]
    fn shrinks_before_skipping() {
        let b = Budget { queue_limit: 2.0, min_share: 0.25 };
        assert_eq!(b.decide(N, N + N / 2), Frame::Send(N / 2));
        assert_eq!(b.decide(N, 2 * N - N / 4), Frame::Send(N / 4));
        assert_eq!(b.decide(N, 2 * N - N / 5), Frame::Skip);
        assert_eq!(b.decide(N, 3 * N), Frame::Skip);
    }

    #[test]
    fn min_share_one_matches_plain_skipping() {
        // Old rule: skip when more than one frame is queued.
        let b = Budget { queue_limit: 2.0, min_share: 1.0 };
        assert_eq!(b.decide(N, N), Frame::Send(N));
        assert_eq!(b.decide(N, N + 1), Frame::Skip);
    }

    #[test]
    fn never_exceeds_queue_limit() {
        let b = Budget { queue_limit: 1.5, min_share: 0.25 };
        for q in (0..2 * N).step_by(997) {
            if let Frame::Send(s) = b.decide(N, q) {
                assert!(q + s <= N * 3 / 2, "queued {q} + frame {s}");
            }
        }
    }
}
