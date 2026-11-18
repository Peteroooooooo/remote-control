//! Test-only loss on the viewer (`view --sim-loss`): drops received video
//! datagrams, either each one on its own at random or in runs, because WiFi
//! loses packets in bursts. Runs follow a two-state (Gilbert) model: nothing
//! is lost in the good state, everything in the bad one; the mean run length
//! sets how long the bad state lasts.
//! Datagrams are dropped after QUIC accepted them, so the transport's
//! congestion control never sees this loss: it tests FEC, key frame recovery
//! and our rate control, not how BBR reacts to real loss.

pub struct LossSim {
    /// Probability of each datagram being lost, 0..1.
    p: f64,
    /// Per datagram: good -> bad, and bad -> good. None for independent loss.
    runs: Option<(f64, f64)>,
    bad: bool,
    rng: u64,
    pub seen: u64,
    pub dropped: u64,
}

impl LossSim {
    /// `pct`: average loss in percent; `burst`: mean number of datagrams lost
    /// in a row (1 or less: each datagram lost on its own).
    pub fn new(pct: f64, burst: f64, seed: u64) -> LossSim {
        let p = (pct / 100.0).clamp(0.0, 0.99);
        let runs = (burst > 1.0).then(|| {
            let leave = 1.0 / burst;
            (p * leave / (1.0 - p), leave)
        });
        LossSim { p, runs, bad: false, rng: seed | 1, seen: 0, dropped: 0 }
    }

    /// xorshift64*: uniform in 0..1.
    fn uniform(&mut self) -> f64 {
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        (self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Whether to drop the next datagram.
    pub fn drop_next(&mut self) -> bool {
        self.seen += 1;
        let r = self.uniform();
        let lose = match self.runs {
            None => r < self.p,
            Some((enter, leave)) => {
                self.bad = if self.bad { r >= leave } else { r < enter };
                self.bad
            }
        };
        self.dropped += lose as u64;
        lose
    }

    pub fn describe(&self) -> String {
        match self.runs {
            None => format!("{:.1}% random", self.p * 100.0),
            Some((_, leave)) => format!("{:.1}% in runs of {:.1} on average", self.p * 100.0, 1.0 / leave),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Loss fraction and mean run length over many datagrams.
    fn measure(sim: &mut LossSim, n: usize) -> (f64, f64) {
        let (mut runs, mut prev) = (0u64, false);
        for _ in 0..n {
            let d = sim.drop_next();
            if d && !prev {
                runs += 1;
            }
            prev = d;
        }
        (sim.dropped as f64 / n as f64, sim.dropped as f64 / runs.max(1) as f64)
    }

    #[test]
    fn random_loss_rate() {
        let (loss, run) = measure(&mut LossSim::new(5.0, 1.0, 42), 400_000);
        assert!((loss - 0.05).abs() < 0.002, "{loss}");
        assert!(run < 1.1, "independent losses rarely touch: {run}");
        assert_eq!(measure(&mut LossSim::new(0.0, 1.0, 1), 10_000).0, 0.0);
    }

    #[test]
    fn bursty_loss_rate_and_run_length() {
        let (loss, run) = measure(&mut LossSim::new(5.0, 8.0, 7), 2_000_000);
        assert!((loss - 0.05).abs() < 0.004, "{loss}");
        assert!((run - 8.0).abs() < 0.5, "{run}");
    }
}
