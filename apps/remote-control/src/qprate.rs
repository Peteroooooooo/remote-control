//! Frame-size control by choosing the quantiser (QP) for every frame, for
//! encoders that cannot change their bitrate from one frame to the next
//! (Intel Quick Sync through Media Foundation restarts with a key frame on
//! every bitrate change). Model: bytes = complexity * 2^(-QP/6), the usual
//! H.264 rule that six QP steps halve the size. The complexity comes from
//! the frame before, so a sudden change of content costs one frame.

pub const MIN_QP: u32 = 12;
pub const MAX_QP: u32 = 51;
/// Largest step towards a better picture per frame; steps towards a smaller
/// frame are not limited, so a shrink request is met at once.
const MAX_DROP: f64 = 4.0;

pub struct QpRate {
    complexity: Option<f64>,
    last_qp: f64,
}

impl Default for QpRate {
    fn default() -> Self {
        QpRate { complexity: None, last_qp: 30.0 }
    }
}

impl QpRate {
    /// QP for a frame that should come out at about `target` bytes.
    pub fn next_qp(&mut self, target: f64) -> u32 {
        let qp = match self.complexity {
            None => self.last_qp,
            Some(c) => (6.0 * (c / target.max(1.0)).log2()).max(self.last_qp - MAX_DROP),
        };
        let qp = qp.round().clamp(MIN_QP as f64, MAX_QP as f64);
        self.last_qp = qp;
        qp as u32
    }

    /// The frame encoded with `qp` came out at `bytes`.
    pub fn update(&mut self, qp: u32, bytes: usize, key: bool) {
        // Key frames cost several times a delta frame of the same content;
        // learning from them would make the next delta frames too coarse.
        if key {
            return;
        }
        self.complexity = Some(bytes.max(1) as f64 * 2f64.powf(qp as f64 / 6.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pretend encoder whose frames follow the model exactly.
    fn encode(content: f64, qp: u32) -> usize {
        (content * 2f64.powf(-(qp as f64) / 6.0)) as usize
    }

    #[test]
    fn settles_on_the_target() {
        let mut r = QpRate::default();
        let content = 4.0e6;
        let mut last = 0;
        for _ in 0..10 {
            let qp = r.next_qp(40_000.0);
            last = encode(content, qp);
            r.update(qp, last, false);
        }
        assert!((last as f64 / 40_000.0 - 1.0).abs() < 0.15, "{last}");
    }

    #[test]
    fn a_shrunk_frame_is_small_at_once_and_quality_returns_gently() {
        let mut r = QpRate::default();
        let content = 4.0e6;
        for _ in 0..10 {
            let qp = r.next_qp(40_000.0);
            r.update(qp, encode(content, qp), false);
        }
        let base = r.last_qp;
        let qp = r.next_qp(10_000.0);
        let small = encode(content, qp);
        assert!(small < 13_000, "{small}");
        r.update(qp, small, false);
        let back = r.next_qp(40_000.0);
        assert_eq!(back as f64, (qp as f64 - MAX_DROP).max(base));
    }

    #[test]
    fn stays_in_range() {
        let mut r = QpRate::default();
        r.update(30, 1, false);
        assert_eq!(r.next_qp(1e9), MIN_QP.max(30 - MAX_DROP as u32));
        r.update(30, 100_000_000, false);
        assert_eq!(r.next_qp(1.0), MAX_QP);
    }
}
