//! Spreads one frame's datagrams over part of the frame interval instead of
//! sending them in one burst. Bursts overflow shallow router and WiFi queues;
//! spreading them costs at most `spread` of extra delay for the last datagram.

use std::time::Duration;

/// Splits `packets` datagrams into batches sent `gap` apart, finishing within
/// `spread`. Returns (offset from frame start, batch size) pairs; sizes sum to `packets`.
/// `spread` of zero sends everything at once.
pub fn plan(packets: usize, spread: Duration, gap: Duration) -> Vec<(Duration, usize)> {
    if packets == 0 {
        return Vec::new();
    }
    let slots = if gap.is_zero() { 1 } else { (spread.as_nanos() / gap.as_nanos()) as usize + 1 };
    let batches = slots.clamp(1, packets);
    let step = if batches > 1 { spread / (batches - 1) as u32 } else { Duration::ZERO };
    (0..batches)
        .map(|i| {
            // Batch i gets packets [i*n/b, (i+1)*n/b).
            let size = (i + 1) * packets / batches - i * packets / batches;
            (step * i as u32, size)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_when_spread_is_zero() {
        assert_eq!(plan(30, Duration::ZERO, Duration::from_millis(1)), vec![(Duration::ZERO, 30)]);
    }

    #[test]
    fn spreads_evenly() {
        let p = plan(36, Duration::from_millis(8), Duration::from_millis(1));
        assert_eq!(p.len(), 9);
        assert_eq!(p.iter().map(|b| b.1).sum::<usize>(), 36);
        assert_eq!(p.last().unwrap().0, Duration::from_millis(8));
        assert!(p.iter().all(|b| b.1 == 4));
    }

    #[test]
    fn fewer_packets_than_slots() {
        let p = plan(3, Duration::from_millis(8), Duration::from_millis(1));
        assert_eq!(p.len(), 3);
        assert_eq!(p.iter().map(|b| b.1).sum::<usize>(), 3);
    }
}
