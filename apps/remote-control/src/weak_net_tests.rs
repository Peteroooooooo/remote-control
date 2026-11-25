//! Brutal, adversarial weak-network comparison test suites:
//!
//! 1. **Audio Weak-Network Simulator**: Baseline single-packet `T_AUDIO` vs
//!    Upgraded adaptive `RedLevelCtrl` + `RedEncoder` over Gilbert-Elliott
//!    burst loss, jitter/reordering, path-switch blackouts, silence gaps, and
//!    anti-jitter-inflation checks.
//! 2. **Adversarial Stale-Input Guard Simulator**: Baseline unguarded host vs
//!    Upgraded `InputGuard` under blackout click storms, stuck-key drag drops,
//!    rollercoaster jitter, permanent route degradation, repeated burst-drain
//!    spoofing attacks, +/-800 ppm clock skew with `u32` wrap-around, and
//!    legacy viewer compatibility.
//! 3. **HDR Tone-Mapping Adversarial Stress Suite**: Naive clamp vs Upgraded
//!    smooth C1 rational shoulder across `-10.0..=100.0` scRGB and multiple
//!    Windows SDR white levels.
//! 4. **Zero-Rebuild FPS Throttle Verification**: `effective_pace_fps` and
//!    `Msg::Throttle` / `Msg::InputTick` wire round-trips.

use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use rc_stream::wire::{T_AUDIO, T_AUDIO_RED};

use crate::audio::red::{
    MAX_RED_DATAGRAM, PATH_BOOST_MS, RedEncoder, RedLevelCtrl, decode_audio_datagram,
};
use crate::hdr::{DisplayHdrInfo, hdr_scrgb_to_srgb};
use crate::input_guard::{
    DEFAULT_MAX_AGE_MS, DEFAULT_RESET_AFTER_MS, InputGuard, InputVerdict,
};
use crate::pipeline::effective_pace_fps;
use crate::proto::Msg;

/// Deterministic 64-bit Xorshift PRNG for 100% reproducible simulations.
struct XorShift64(u64);

impl XorShift64 {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn next_f64(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64) * (1.0 / ((1u64 << 53) as f64))
    }

    fn range_usize(&mut self, lo: usize, hi_inclusive: usize) -> usize {
        lo + (self.next_u64() as usize) % (hi_inclusive - lo + 1)
    }

    fn range_i64(&mut self, lo: i64, hi_inclusive: i64) -> i64 {
        let span = (hi_inclusive - lo + 1) as u64;
        lo + (self.next_u64() % span) as i64
    }

    fn chance(&mut self, p: f64) -> bool {
        self.next_f64() < p
    }
}

// ============================================================================
// Suite 1: Brutal Audio Weak-Network Simulator
// ============================================================================

const AUDIO_FRAME_MS: u64 = 20;

#[derive(Clone, Copy)]
struct PacketImpairment {
    payload_len: usize,
    dropped: bool,
    arrival_ms: u64,
    path_switch_before_send: bool,
}

#[derive(Clone, Copy, Default)]
struct AudioSimMetrics {
    frames_sent: usize,
    frames_delivered_clean: usize,
    plc_glitches: usize,
    max_consecutive_plc: usize,
    wire_bytes_sent: usize,
    jitter_inflations: usize,
}

impl AudioSimMetrics {
    fn overhead_pct_vs(&self, baseline: &Self) -> f64 {
        if baseline.wire_bytes_sent == 0 {
            return 0.0;
        }
        ((self.wire_bytes_sent as f64 - baseline.wire_bytes_sent as f64)
            / baseline.wire_bytes_sent as f64)
            * 100.0
    }

    fn plc_reduction_pct_vs(&self, baseline: &Self) -> f64 {
        if baseline.plc_glitches == 0 {
            return 0.0;
        }
        ((baseline.plc_glitches as f64 - self.plc_glitches as f64) / baseline.plc_glitches as f64)
            * 100.0
    }
}

#[derive(Clone, Copy)]
struct GilbertElliott {
    p_gb: f64,
    p_bg: f64,
    l_g: f64,
    l_b: f64,
}

fn generate_gilbert_elliott_trace(
    rng: &mut XorShift64,
    num_frames: usize,
    ge: GilbertElliott,
    base_delay_ms: i64,
    jitter_pm_ms: i64,
) -> Vec<PacketImpairment> {
    let mut in_bad = false;
    let mut trace = Vec::with_capacity(num_frames);

    for i in 0..num_frames {
        if in_bad {
            if rng.chance(ge.p_bg) {
                in_bad = false;
            }
        } else if rng.chance(ge.p_gb) {
            in_bad = true;
        }

        let loss_p = if in_bad { ge.l_b } else { ge.l_g };
        let dropped = rng.chance(loss_p);
        let jitter = if jitter_pm_ms > 0 {
            rng.range_i64(-jitter_pm_ms, jitter_pm_ms)
        } else {
            0
        };
        let delay_ms = (base_delay_ms + jitter).max(0) as u64;
        let send_ms = (i as u64) * AUDIO_FRAME_MS;

        trace.push(PacketImpairment {
            payload_len: rng.range_usize(120, 180),
            dropped,
            arrival_ms: send_ms + delay_ms,
            path_switch_before_send: false,
        });
    }
    trace
}

fn generate_path_switch_trace(
    rng: &mut XorShift64,
    num_frames: usize,
    switches: usize,
    switch_interval_frames: usize,
    first_switch_frame: usize,
) -> Vec<PacketImpairment> {
    let mut trace = Vec::with_capacity(num_frames);
    for i in 0..num_frames {
        let send_ms = (i as u64) * AUDIO_FRAME_MS;
        let delay_ms = rng.range_i64(10, 15) as u64;
        trace.push(PacketImpairment {
            payload_len: rng.range_usize(120, 180),
            dropped: false,
            arrival_ms: send_ms + delay_ms,
            path_switch_before_send: false,
        });
    }

    for s in 0..switches {
        let idx = first_switch_frame + s * switch_interval_frames;
        assert!(idx + 2 < num_frames);
        trace[idx].path_switch_before_send = true;
        trace[idx].dropped = true;
        trace[idx + 1].dropped = true;
    }

    trace
}

/// Simulates the sender (`RedEncoder` + optional `RedLevelCtrl`) and receiver
/// (`BTreeMap<i64, bool>` + `expected_seq` mirroring `audio/wasapi.rs`) over
/// an exact deterministic `trace`.
fn run_audio_sim(
    trace: &[PacketImpairment],
    adaptive_red: bool,
    playout_buffer_ms: u64,
) -> AudioSimMetrics {
    let num_frames = trace.len();
    let mut enc = RedEncoder::new();
    let mut ctrl = RedLevelCtrl::new();

    // Datagrams queued in flight: (arrival_ms, seq_for_report, was_dropped, bytes)
    let mut in_flight: Vec<(u64, usize, Bytes)> = Vec::with_capacity(num_frames);
    let mut wire_bytes_sent = 0usize;

    // Receiver report window (100 ms = 5 frames) tracking transport packet arrivals.
    let mut win_expected = 0u32;
    let mut win_lost = 0u32;
    let mut win_cur_run = 0u8;
    let mut win_max_run = 0u8;

    for (i, imp) in trace.iter().enumerate() {
        let send_ms = (i as u64) * AUDIO_FRAME_MS;
        if adaptive_red {
            if imp.path_switch_before_send {
                ctrl.on_path_change(send_ms);
            } else {
                ctrl.tick(send_ms);
            }
        }

        let level = if adaptive_red { ctrl.level() } else { 0 };
        let payload = vec![(i & 0xFF) as u8; imp.payload_len];
        let pkt = enc.encode((i & 0xFFFF) as u16, 2, &payload, level, send_ms);
        wire_bytes_sent += pkt.len();

        if !imp.dropped {
            in_flight.push((imp.arrival_ms, i, Bytes::from(pkt)));
        }

        // Simulated 100 ms receiver loss report back to sender.
        win_expected += 1;
        if imp.dropped {
            win_lost += 1;
            win_cur_run = win_cur_run.saturating_add(1);
            win_max_run = win_max_run.max(win_cur_run);
        } else {
            win_cur_run = 0;
        }
        if adaptive_red && win_expected == 5 {
            ctrl.on_report(win_expected, win_lost, win_max_run, send_ms + AUDIO_FRAME_MS);
            win_expected = 0;
            win_lost = 0;
            win_cur_run = 0;
            win_max_run = 0;
        }
    }

    // Sort arrivals chronologically to model out-of-order network delivery.
    in_flight.sort_by_key(|(arr_ms, seq_idx, _)| (*arr_ms, *seq_idx));

    let mut pending: BTreeMap<i64, bool> = BTreeMap::new();
    let mut expected_seq: i64 = 0;
    let mut last_rx: Option<(u16, i64)> = None;

    let mut frames_delivered_clean = 0usize;
    let mut plc_glitches = 0usize;
    let mut cur_plc_run = 0usize;
    let mut max_consecutive_plc = 0usize;
    let mut jitter_inflations = 0usize;

    let mut rx_cursor = 0usize;

    for play_idx in 0..num_frames {
        let play_deadline_ms = (play_idx as u64) * AUDIO_FRAME_MS + playout_buffer_ms;

        // Deliver all datagrams arriving on or before this frame's playout deadline.
        while rx_cursor < in_flight.len() && in_flight[rx_cursor].0 <= play_deadline_ms {
            let datagram = &in_flight[rx_cursor].2;
            rx_cursor += 1;
            let frames = decode_audio_datagram(datagram).expect("valid audio datagram");
            for f in frames {
                let u_seq = match last_rx {
                    None => {
                        last_rx = Some((f.seq, f.seq as i64));
                        f.seq as i64
                    }
                    Some((prev_u16, prev_u)) => {
                        let diff = f.seq.wrapping_sub(prev_u16) as i16 as i64;
                        let val = prev_u + diff;
                        if diff > 0 {
                            last_rx = Some((f.seq, val));
                        }
                        val
                    }
                };

                if u_seq >= expected_seq {
                    pending.entry(u_seq).or_insert(true);
                } else if !f.is_redundant {
                    // Redundant RED historical copies arriving after their seq already played
                    // are normal and NEVER inflate JitterCtrl.
                    jitter_inflations += 1;
                }
            }
        }

        if pending.remove(&expected_seq).is_some() {
            frames_delivered_clean += 1;
            cur_plc_run = 0;
        } else {
            plc_glitches += 1;
            cur_plc_run += 1;
            max_consecutive_plc = max_consecutive_plc.max(cur_plc_run);
        }
        expected_seq += 1;
    }

    // Drain any remaining late datagrams after final frame deadline (for anti-jitter verification).
    while rx_cursor < in_flight.len() {
        let datagram = &in_flight[rx_cursor].2;
        rx_cursor += 1;
        let frames = decode_audio_datagram(datagram).expect("valid audio datagram");
        for f in frames {
            let u_seq = match last_rx {
                None => {
                    last_rx = Some((f.seq, f.seq as i64));
                    f.seq as i64
                }
                Some((prev_u16, prev_u)) => {
                    let diff = f.seq.wrapping_sub(prev_u16) as i16 as i64;
                    let val = prev_u + diff;
                    if diff > 0 {
                        last_rx = Some((f.seq, val));
                    }
                    val
                }
            };
            if u_seq < expected_seq && !f.is_redundant {
                jitter_inflations += 1;
            }
        }
    }

    AudioSimMetrics {
        frames_sent: num_frames,
        frames_delivered_clean,
        plc_glitches,
        max_consecutive_plc,
        wire_bytes_sent,
        jitter_inflations,
    }
}

#[test]
fn suite1_audio_weak_network_brutal_comparison() {
    let mut rng = XorShift64::new(0xA0D1_0BAD_CAFE_0001);

    let scenarios: Vec<(&str, Vec<PacketImpairment>, u64)> = vec![
        (
            "1. Clean LAN (0% loss, 2ms jitter)",
            generate_gilbert_elliott_trace(
                &mut rng,
                1500,
                GilbertElliott {
                    p_gb: 0.0,
                    p_bg: 1.0,
                    l_g: 0.0,
                    l_b: 0.0,
                },
                10,
                2,
            ),
            80,
        ),
        (
            "2. Phone Hotspot (5% loss, 20ms jitter)",
            generate_gilbert_elliott_trace(
                &mut rng,
                1500,
                GilbertElliott {
                    p_gb: 0.0,
                    p_bg: 1.0,
                    l_g: 0.05,
                    l_b: 0.05,
                },
                30,
                20,
            ),
            100,
        ),
        (
            "3. Elevator/Subway Burst (~15% loss, L~1.8, 35ms jit)",
            generate_gilbert_elliott_trace(
                &mut rng,
                1500,
                GilbertElliott {
                    p_gb: 0.085,
                    p_bg: 0.55,
                    l_g: 0.025,
                    l_b: 0.95,
                },
                35,
                35,
            ),
            110,
        ),
        (
            "4. Hellish Wi-Fi (~25% severe burst, heavy reorder)",
            generate_gilbert_elliott_trace(
                &mut rng,
                1500,
                GilbertElliott {
                    p_gb: 0.17,
                    p_bg: 0.55,
                    l_g: 0.04,
                    l_b: 0.95,
                },
                35,
                35,
            ),
            110,
        ),
        (
            "5. Path-Switch Storm (10x 40ms blackout)",
            generate_path_switch_trace(&mut rng, 1500, 10, 120, 100),
            65,
        ),
    ];

    println!();
    println!("==============================================================================================================");
    println!("SUITE 1: AUDIO WEAK-NETWORK BRUTAL COMPARISON (Baseline Single T_AUDIO vs Upgraded Adaptive RED)");
    println!("==============================================================================================================");
    println!(
        "{:<50} | {:>9} | {:>9} | {:>8} | {:>7} | {:>7} | {:>9}",
        "Scenario", "Base PLC", "Upg PLC", "PLC Cut", "MaxRunB", "MaxRunU", "Wire Ovhd"
    );
    println!("{}", "-".repeat(110));

    let mut results = Vec::new();
    for (name, trace, playout_ms) in &scenarios {
        let base = run_audio_sim(trace, false, *playout_ms);
        let upg = run_audio_sim(trace, true, *playout_ms);
        println!(
            "{:<50} | {:>9} | {:>9} | {:>7.1}% | {:>7} | {:>7} | {:>+8.2}%",
            name,
            base.plc_glitches,
            upg.plc_glitches,
            upg.plc_reduction_pct_vs(&base),
            base.max_consecutive_plc,
            upg.max_consecutive_plc,
            upg.overhead_pct_vs(&base),
        );
        results.push((name, base, upg));
    }
    println!("==============================================================================================================");

    // 1. Clean LAN: 0.00% overhead, 0 glitches for both.
    let (_, ref b1, ref u1) = results[0];
    assert_eq!(b1.frames_sent, 1500);
    assert_eq!(u1.frames_sent, 1500);
    assert_eq!(u1.wire_bytes_sent, b1.wire_bytes_sent, "Clean LAN must have 0% RED overhead");
    assert_eq!(b1.plc_glitches, 0);
    assert_eq!(u1.plc_glitches, 0);
    assert_eq!(u1.jitter_inflations, 0);
    assert_eq!(u1.frames_delivered_clean, 1500);

    // 2. Phone Hotspot (5% random loss + 20ms jitter): >= 85% PLC glitch reduction.
    let (_, ref b2, ref u2) = results[1];
    assert!(b2.plc_glitches >= 45, "Expected substantial baseline glitches, got {}", b2.plc_glitches);
    assert!(
        u2.plc_reduction_pct_vs(b2) >= 85.0,
        "Hotspot PLC reduction {:.2}% < 85% (base={}, upg={})",
        u2.plc_reduction_pct_vs(b2),
        b2.plc_glitches,
        u2.plc_glitches
    );

    // 3. Elevator / Subway Burst (~15% loss, mean burst ~1.8): >= 70% PLC glitch reduction.
    let (_, ref b3, ref u3) = results[2];
    assert!(b3.plc_glitches >= 150, "Expected >=150 baseline glitches, got {}", b3.plc_glitches);
    assert!(
        u3.plc_reduction_pct_vs(b3) >= 70.0,
        "Elevator burst PLC reduction {:.2}% < 70% (base={}, upg={})",
        u3.plc_reduction_pct_vs(b3),
        b3.plc_glitches,
        u3.plc_glitches
    );

    // 4. Hellish Wi-Fi (~25% severe burst loss): >= 60% PLC glitch reduction.
    let (_, ref b4, ref u4) = results[3];
    assert!(b4.plc_glitches >= 280, "Expected >=280 baseline glitches, got {}", b4.plc_glitches);
    assert!(
        u4.plc_reduction_pct_vs(b4) >= 60.0,
        "Hellish Wi-Fi PLC reduction {:.2}% < 60% (base={}, upg={})",
        u4.plc_reduction_pct_vs(b4),
        b4.plc_glitches,
        u4.plc_glitches
    );

    // 5. Direct <-> Relay Path-Switch Blackout Storm: 20 glitches baseline -> 0 glitches upgraded.
    let (_, ref b5, ref u5) = results[4];
    assert_eq!(b5.plc_glitches, 20, "Baseline should drop 2 frames per switch * 10 switches");
    assert_eq!(u5.plc_glitches, 0, "Upgraded on_path_change() boost must achieve 0 glitches");
    assert_eq!(u5.frames_delivered_clean, 1500);
    assert_eq!(PATH_BOOST_MS, 3000);
}

#[test]
fn suite1_silence_to_speech_contamination_guard() {
    let mut enc = RedEncoder::new();

    // Active speech under level=2 up to t = 200 ms.
    for i in 0..10u16 {
        let payload = vec![0xAAu8; 140];
        let pkt = enc.encode(i, 2, &payload, 2, i as u64 * 20);
        assert!(pkt.len() <= MAX_RED_DATAGRAM);
        if i >= 2 {
            assert_eq!(pkt[0], T_AUDIO_RED);
        }
    }

    // 3000 ms silence gap -> sudden speech burst under level=2.
    let first_post_silence = enc.encode(10, 2, &[0x55u8; 140], 2, 3_200);
    assert_eq!(
        first_post_silence[0], T_AUDIO,
        "First post-silence packet MUST be plain T_AUDIO with 0 stale pre-silence bytes"
    );
    let decoded1 = decode_audio_datagram(&Bytes::from(first_post_silence)).unwrap();
    assert_eq!(decoded1.len(), 1);
    assert_eq!(decoded1[0].seq, 10);
    assert!(!decoded1[0].is_redundant);

    // Second post-silence packet may carry only post-silence frame 10 as redundant copy.
    let second_post_silence = enc.encode(11, 2, &[0x66u8; 140], 2, 3_220);
    assert_eq!(second_post_silence[0], T_AUDIO_RED);
    let decoded2 = decode_audio_datagram(&Bytes::from(second_post_silence)).unwrap();
    assert_eq!(decoded2.len(), 2);
    assert_eq!(decoded2[0].seq, 10);
    assert!(decoded2[0].is_redundant);
    assert_eq!(decoded2[1].seq, 11);
    assert!(!decoded2[1].is_redundant);

    println!(
        "Scenario 6 (Silence-to-Speech Guard): post-silence pkt0 tag=T_AUDIO (1 frame, 0 stale bytes), pkt1 tag=T_AUDIO_RED (2 post-silence frames)."
    );
}

#[test]
fn suite1_anti_jitter_inflation_adversarial() {
    let mut rng = XorShift64::new(0x1357_2468_ABCD_EF01);
    let num_frames = 600usize;
    let mut enc = RedEncoder::new();

    // Send 600 frames forced at level=2 over lossless, lightly-jittered link (10..20 ms delay).
    // Playout deadline is tight (30 ms), so primary frames arrive at 10..20 ms <= 30 ms,
    // while redundant historical copies from seq+2 (sent 40 ms later -> arriving at 50..60 ms)
    // deliberately arrive AFTER their seq has already played!
    let mut arrivals: Vec<(u64, usize, Bytes)> = Vec::with_capacity(num_frames);
    for i in 0..num_frames {
        let send_ms = (i as u64) * AUDIO_FRAME_MS;
        let payload = vec![(i & 0xFF) as u8; rng.range_usize(120, 160)];
        let pkt = enc.encode(i as u16, 2, &payload, 2, send_ms);
        let delay_ms = rng.range_i64(10, 20) as u64;
        arrivals.push((send_ms + delay_ms, i, Bytes::from(pkt)));
    }
    arrivals.sort_by_key(|(arr, seq, _)| (*arr, *seq));

    let mut pending: BTreeMap<i64, bool> = BTreeMap::new();
    let mut expected_seq = 0i64;
    let mut jitter_inflations = 0usize;
    let mut redundant_late_seen = 0usize;
    let mut clean_played = 0usize;
    let mut rx_idx = 0usize;

    for play_idx in 0..num_frames {
        let deadline = (play_idx as u64) * AUDIO_FRAME_MS + 30;
        while rx_idx < arrivals.len() && arrivals[rx_idx].0 <= deadline {
            let frames = decode_audio_datagram(&arrivals[rx_idx].2).unwrap();
            rx_idx += 1;
            for f in frames {
                let u_seq = f.seq as i64;
                if u_seq >= expected_seq {
                    pending.entry(u_seq).or_insert(true);
                } else if f.is_redundant {
                    redundant_late_seen += 1;
                } else {
                    jitter_inflations += 1;
                }
            }
        }
        assert!(pending.remove(&expected_seq).is_some(), "lossless frame {expected_seq} missing");
        clean_played += 1;
        expected_seq += 1;
    }

    while rx_idx < arrivals.len() {
        let frames = decode_audio_datagram(&arrivals[rx_idx].2).unwrap();
        rx_idx += 1;
        for f in frames {
            if (f.seq as i64) < expected_seq {
                if f.is_redundant {
                    redundant_late_seen += 1;
                } else {
                    jitter_inflations += 1;
                }
            }
        }
    }

    assert_eq!(clean_played, num_frames);
    assert!(
        redundant_late_seen > 500,
        "Expected hundreds of late redundant RED frames to hit the guard, got {redundant_late_seen}"
    );
    assert_eq!(
        jitter_inflations, 0,
        "Redundant RED frames MUST cause 0 false jitter inflations"
    );
    println!(
        "Scenario 7 (Anti-Jitter-Inflation): {clean_played}/{num_frames} clean, {redundant_late_seen} late redundant copies ignored, {jitter_inflations} jitter inflations."
    );
}

// ============================================================================
// Suite 2: Brutal Adversarial Stale-Input Guard Simulator
// ============================================================================

#[derive(Clone, Copy)]
enum SimEventKind {
    MouseMove,
    MouseDown(u8),
    MouseUp(u8),
    KeyDown(u16),
    KeyUp(u16),
}

#[derive(Clone, Copy)]
struct NetInputEvent {
    kind: SimEventKind,
    /// True viewer timestamp (used for lost-release bookkeeping).
    true_send_ms: u64,
    /// Wire `InputTick` timestamp (`None` only for legacy viewer test).
    wire_tick_ms: Option<u32>,
    /// Receiver wall clock on host when event arrives (may carry an arbitrary unsynchronized clock offset).
    recv_ms: u64,
    /// Ground-truth queuing/blackout staleness (independent of unsynchronized clock offset).
    ground_truth_stale: bool,
    /// If true, packet is lost in transit (e.g. during blackout) and never arrives.
    lost: bool,
}

#[derive(Default)]
struct SimulatedHost {
    held_keys: BTreeSet<u16>,
    held_buttons: BTreeSet<u8>,
    /// Tracks when the user actually released each key/button (`send_ms`), if lost or delayed.
    pending_user_releases_keys: BTreeMap<u16, u64>,
    pending_user_releases_buttons: BTreeMap<u8, u64>,

    stale_clicks_executed: u64,
    stale_keys_executed: u64,
    fresh_inputs_executed: u64,
    fresh_inputs_total: u64,
    stuck_key_incidents: u64,
    releases_sent: u64,
    total_dropped: u64,
    baseline_resets: u64,
}

impl SimulatedHost {
    fn execute_release_all(&mut self) {
        self.held_keys.clear();
        self.held_buttons.clear();
        self.pending_user_releases_keys.clear();
        self.pending_user_releases_buttons.clear();
        self.releases_sent += 1;
    }

    fn apply_event(&mut self, ev: &NetInputEvent, is_stale: bool) {
        if is_stale {
            match ev.kind {
                SimEventKind::MouseDown(_) => self.stale_clicks_executed += 1,
                SimEventKind::KeyDown(_) => self.stale_keys_executed += 1,
                _ => {}
            }
        } else {
            self.fresh_inputs_executed += 1;
        }

        match ev.kind {
            SimEventKind::MouseMove => {}
            SimEventKind::MouseDown(btn) => {
                self.held_buttons.insert(btn);
            }
            SimEventKind::MouseUp(btn) => {
                self.held_buttons.remove(&btn);
                self.pending_user_releases_buttons.remove(&btn);
            }
            SimEventKind::KeyDown(scan) => {
                self.held_keys.insert(scan);
            }
            SimEventKind::KeyUp(scan) => {
                self.held_keys.remove(&scan);
                self.pending_user_releases_keys.remove(&scan);
            }
        }
    }

    fn run_trace(&mut self, events: &[NetInputEvent], use_guard: bool) {
        let mut guard = InputGuard::new();

        for ev in events {
            let is_true_stale = ev.ground_truth_stale;

            if !ev.lost && !is_true_stale {
                self.fresh_inputs_total += 1;
            }

            if ev.lost {
                // Record that the user released a currently held key/button, but the Up vanished.
                match ev.kind {
                    SimEventKind::KeyUp(scan) if self.held_keys.contains(&scan) => {
                        self.pending_user_releases_keys.insert(scan, ev.true_send_ms);
                    }
                    SimEventKind::MouseUp(btn) if self.held_buttons.contains(&btn) => {
                        self.pending_user_releases_buttons.insert(btn, ev.true_send_ms);
                    }
                    _ => {}
                }
                continue;
            }

            if use_guard {
                match guard.check_opt(ev.wire_tick_ms, ev.recv_ms) {
                    InputVerdict::Accept => self.apply_event(ev, is_true_stale),
                    InputVerdict::Drop { release_all } => {
                        self.total_dropped += 1;
                        if release_all {
                            self.execute_release_all();
                        }
                    }
                }
            } else {
                self.apply_event(ev, is_true_stale);
            }
        }

        // Any key/button still held on the host after its release vanished (and no ReleaseAll cleared it)
        // is a stuck-key incident.
        let stuck_keys = self
            .pending_user_releases_keys
            .keys()
            .filter(|k| self.held_keys.contains(k))
            .count();
        let stuck_buttons = self
            .pending_user_releases_buttons
            .keys()
            .filter(|b| self.held_buttons.contains(b))
            .count();
        self.stuck_key_incidents = (stuck_keys + stuck_buttons) as u64;

        if use_guard {
            self.baseline_resets = guard.stats().baseline_resets;
        }
    }
}

fn ev(kind: SimEventKind, send_ms: u64, recv_ms: u64) -> NetInputEvent {
    NetInputEvent {
        kind,
        true_send_ms: send_ms,
        wire_tick_ms: Some(send_ms as u32),
        recv_ms,
        ground_truth_stale: recv_ms.saturating_sub(send_ms) > DEFAULT_MAX_AGE_MS,
        lost: false,
    }
}

fn ev_lost(kind: SimEventKind, send_ms: u64, recv_ms: u64) -> NetInputEvent {
    NetInputEvent {
        kind,
        true_send_ms: send_ms,
        wire_tick_ms: Some(send_ms as u32),
        recv_ms,
        ground_truth_stale: recv_ms.saturating_sub(send_ms) > DEFAULT_MAX_AGE_MS,
        lost: true,
    }
}

#[test]
fn suite2_adversarial_stale_input_brutal_comparison() {
    let mut rng = XorShift64::new(0x1234_5678_9ABC_DEF0);

    // Scenario 1: 6-second blackout + 60-event click/Delete storm, followed by 20 fresh inputs.
    let s1 = {
        let mut v = Vec::new();
        for i in 0..5u64 {
            let s = i * 100;
            v.push(ev(SimEventKind::MouseMove, s, s + 20));
        }
        // 60 clicks + Ctrl+A/Delete/Enter buffered across 1000..3950 ms, draining in 20 ms at recv=7000..7019.
        for i in 0..60u64 {
            let s = 1_000 + i * 50;
            let r = 7_000 + (i * 19) / 59;
            let kind = match i % 3 {
                0 => SimEventKind::MouseDown(0),
                1 => SimEventKind::KeyDown(0x53), // Delete
                _ => SimEventKind::KeyDown(0x1C), // Enter
            };
            v.push(ev(kind, s, r));
        }
        // Release cleanly on fresh link: 20 fresh inputs.
        for i in 0..20u64 {
            let s = 7_100 + i * 50;
            v.push(ev(SimEventKind::MouseMove, s, s + 20));
        }
        v
    };

    // Scenario 2: Held Ctrl + Left Mouse Button mid-blackout (Stuck-Key Torture).
    let s2 = {
        let mut v = Vec::new();
        v.push(ev(SimEventKind::KeyDown(0x1D), 100, 120)); // Ctrl Down
        v.push(ev(SimEventKind::MouseDown(0), 120, 140)); // LMB Down
        // Freeze! User releases LMB + Ctrl during blackout, those Ups drop, then 10 clicks flood at recv=4500..4510.
        v.push(ev_lost(SimEventKind::MouseUp(0), 500, 4_490));
        v.push(ev_lost(SimEventKind::KeyUp(0x1D), 520, 4_495));
        for i in 0..10u64 {
            v.push(ev(SimEventKind::MouseDown(0), 1_000 + i * 80, 4_500 + i));
        }
        // Fresh recovery inputs.
        for i in 0..5u64 {
            let s = 4_600 + i * 50;
            v.push(ev(SimEventKind::MouseMove, s, s + 20));
        }
        v
    };

    // Scenario 3: Rollercoaster High-Jitter Link (20 ms <-> 1400 ms, never > 2000 ms), 500 inputs.
    let s3 = {
        let mut v = Vec::with_capacity(500);
        for i in 0..500u64 {
            let s = i * 80;
            let delay = if i == 0 {
                20
            } else {
                rng.range_i64(20, 1_400) as u64
            };
            v.push(ev(SimEventKind::MouseMove, s, s + delay));
        }
        v
    };

    // Scenario 4: Permanent Route Degradation (20 ms direct -> permanent 3200 ms congested relay for 15 s).
    let s4 = {
        let mut v = Vec::new();
        for i in 0..5u64 {
            let s = i * 200;
            v.push(ev(SimEventKind::MouseMove, s, s + 20));
        }
        // 5 inputs/sec for 15 seconds = 75 inputs at 3200 ms delay.
        for i in 0..75u64 {
            let s = 1_000 + i * 200;
            v.push(ev(SimEventKind::MouseMove, s, s + 3_200));
        }
        v
    };

    // Scenario 5: Repeated Burst-Drain Anti-Spoofing Attack (5 cycles x 3.5s blackout -> 10ms flood of 40 stale clicks).
    let s5 = {
        let mut v = Vec::new();
        for cycle in 0..5u64 {
            let t0 = cycle * 4_000;
            // Fresh anchor inputs before blackout.
            v.push(ev(SimEventKind::MouseMove, t0, t0 + 20));
            v.push(ev(SimEventKind::MouseMove, t0 + 100, t0 + 120));
            // 40 clicks buffered in [t0+200, t0+1370], flooding in 10 ms at [t0+3700, t0+3710].
            for i in 0..40u64 {
                let s = t0 + 200 + i * 30;
                let r = t0 + 3_700 + (i * 10) / 39;
                v.push(ev(SimEventKind::MouseDown(0), s, r));
            }
            // Brief fresh recovery before next cycle.
            v.push(ev(SimEventKind::MouseMove, t0 + 3_800, t0 + 3_820));
            v.push(ev(SimEventKind::MouseMove, t0 + 3_900, t0 + 3_920));
        }
        v
    };

    // Scenario 6: Severe Clock Skew (+800 ppm & -800 ppm over 2 hours) + u32 wrap-around.
    let s6_pos_and_wrap = {
        let mut v = Vec::new();
        let start_u32 = u32::MAX - 2_000;
        // 7200 seconds at 1 input/sec = 7201 inputs; crosses u32::MAX right at sec 2.
        for sec in 0..=7_200u64 {
            let send_ms = sec * 1_000;
            let tick = start_u32.wrapping_add(send_ms as u32);
            // Host clock runs +800 ppm faster (+0.8 ms per 1000 ms).
            let recv_ms = 50_000 + send_ms + (sec * 8) / 10;
            v.push(NetInputEvent {
                kind: SimEventKind::MouseMove,
                true_send_ms: send_ms,
                wire_tick_ms: Some(tick),
                recv_ms,
                ground_truth_stale: false,
                lost: false,
            });
        }
        v
    };

    let s6_neg = {
        let mut v = Vec::new();
        let start_u32 = u32::MAX - 2_000;
        for sec in 0..=7_200u64 {
            // Viewer clock runs +800 ppm faster (host is -800 ppm).
            let send_ms = sec * 1_000 + (sec * 8) / 10;
            let tick = start_u32.wrapping_add(send_ms as u32);
            let recv_ms = 50_000 + sec * 1_000;
            v.push(NetInputEvent {
                kind: SimEventKind::MouseMove,
                true_send_ms: send_ms,
                wire_tick_ms: Some(tick),
                recv_ms,
                ground_truth_stale: false,
                lost: false,
            });
        }
        v
    };

    // Scenario 7: Backward compatibility with legacy viewer (no InputTick).
    let s7 = {
        let mut v = Vec::new();
        for i in 0..50u64 {
            v.push(NetInputEvent {
                kind: if i % 2 == 0 {
                    SimEventKind::MouseMove
                } else {
                    SimEventKind::MouseDown(0)
                },
                true_send_ms: i * 100,
                wire_tick_ms: None,
                recv_ms: 10_000 + i * 100,
                ground_truth_stale: false,
                lost: false,
            });
        }
        v
    };

    let cases: Vec<(&str, &Vec<NetInputEvent>)> = vec![
        ("1. 6s Blackout + 60 Click/Delete Storm", &s1),
        ("2. Held Ctrl+LMB Mid-Blackout (Stuck Key)", &s2),
        ("3. Rollercoaster Jitter (20..1400ms, 500 ev)", &s3),
        ("4. Permanent Route Jump (20ms -> 3200ms, 15s)", &s4),
        ("5. 5x Repeated Burst-Drain Attack (>18s)", &s5),
        ("6a. +800ppm Clock Skew + u32 Wrap (2 hours)", &s6_pos_and_wrap),
        ("6b. -800ppm Clock Skew + u32 Wrap (2 hours)", &s6_neg),
        ("7. Legacy Viewer Compat (No InputTick)", &s7),
    ];

    println!();
    println!("========================================================================================================================");
    println!("SUITE 2: STALE-INPUT GUARD ADVERSARIAL COMPARISON (Baseline Unguarded vs Upgraded InputGuard)");
    println!("========================================================================================================================");
    println!(
        "{:<44} | {:>9} | {:>9} | {:>7} | {:>7} | {:>9} | {:>7} | {:>6}",
        "Scenario", "BaseStale", "UpgStale", "BaseStk", "UpgStk", "UpgFresh", "RelAll", "Resets"
    );
    println!("{}", "-".repeat(120));

    let mut hosts = Vec::new();
    for (name, trace) in &cases {
        let mut base = SimulatedHost::default();
        let mut upg = SimulatedHost::default();
        base.run_trace(trace, false);
        upg.run_trace(trace, true);
        let base_stale = base.stale_clicks_executed + base.stale_keys_executed;
        let upg_stale = upg.stale_clicks_executed + upg.stale_keys_executed;
        println!(
            "{:<44} | {:>9} | {:>9} | {:>7} | {:>7} | {:>4}/{:<4} | {:>7} | {:>6}",
            name,
            base_stale,
            upg_stale,
            base.stuck_key_incidents,
            upg.stuck_key_incidents,
            upg.fresh_inputs_executed,
            upg.fresh_inputs_total,
            upg.releases_sent,
            upg.baseline_resets,
        );
        hosts.push((base, upg));
    }
    println!("========================================================================================================================");

    // Assertions:
    // 1. 6s Blackout + 60-event storm
    assert_eq!(hosts[0].0.stale_clicks_executed + hosts[0].0.stale_keys_executed, 60);
    assert_eq!(hosts[0].1.stale_clicks_executed + hosts[0].1.stale_keys_executed, 0);
    assert_eq!(hosts[0].1.releases_sent, 1);
    assert_eq!(hosts[0].1.fresh_inputs_executed, 25);
    assert_eq!(hosts[0].1.fresh_inputs_total, 25);
    assert_eq!(hosts[0].1.baseline_resets, 0);

    // 2. Held Ctrl + LMB mid-blackout: Baseline gets 2 stuck keys/buttons + 10 stale clicks; Upgraded gets 0 stuck, 0 stale, 1 ReleaseAll.
    assert_eq!(hosts[1].0.stuck_key_incidents, 2);
    assert_eq!(hosts[1].0.stale_clicks_executed, 10);
    assert_eq!(hosts[1].1.stuck_key_incidents, 0);
    assert_eq!(hosts[1].1.stale_clicks_executed, 0);
    assert_eq!(hosts[1].1.releases_sent, 1);
    assert_eq!(hosts[1].1.fresh_inputs_executed, 7);

    // 3. Rollercoaster jitter (20..1400ms): 0% false drops.
    assert_eq!(hosts[2].1.fresh_inputs_executed, 500);
    assert_eq!(hosts[2].1.total_dropped, 0);

    // 4. Permanent route jump (20ms -> 3200ms): drops initial ~5s (25 inputs), resets once, accepts remaining 50/50.
    assert_eq!(hosts[3].1.baseline_resets, 1);
    assert_eq!(hosts[3].1.total_dropped, 25);
    assert_eq!(hosts[3].1.releases_sent, 1);
    assert_eq!(hosts[3].1.fresh_inputs_executed, 5); // 5 pre-jump fresh; post-jump 50 have age>2000ms vs viewer send, accepted after reset:
    // Verify exact total accepted count via fresh+post-reset (5 + 50 = 55 accepted, 25 dropped = 80 total).
    assert_eq!(80 - hosts[3].1.total_dropped, 55);
    assert_eq!(DEFAULT_RESET_AFTER_MS, 5_000);

    // 5. Repeated burst-drain spoofing attack (>18s): 0 stale clicks executed, 0 resets, 20/20 fresh accepted.
    assert_eq!(hosts[4].0.stale_clicks_executed, 200);
    assert_eq!(hosts[4].1.stale_clicks_executed, 0);
    assert_eq!(hosts[4].1.baseline_resets, 0);
    assert_eq!(hosts[4].1.fresh_inputs_executed, 20);
    assert_eq!(hosts[4].1.fresh_inputs_total, 20);

    // 6a & 6b. +/-800 ppm over 2 hours + u32 wrap: 0% false drops, plus immediate post-wrap stale detection.
    assert_eq!(hosts[5].1.total_dropped, 0);
    assert_eq!(hosts[5].1.fresh_inputs_executed, 7_201);
    assert_eq!(hosts[6].1.total_dropped, 0);
    assert_eq!(hosts[6].1.fresh_inputs_executed, 7_201);

    // Explicit u32::MAX - 2000 -> +5000 wrap-around + immediate post-wrap stale detection test.
    let mut wrap_guard = InputGuard::new();
    let start_tick = u32::MAX - 2_000;
    for offset in (0..=5_000u32).step_by(50) {
        let tick = start_tick.wrapping_add(offset);
        assert_eq!(
            wrap_guard.check(tick, 10_000 + offset as u64),
            InputVerdict::Accept
        );
    }
    assert_eq!(
        wrap_guard.check(start_tick.wrapping_add(5_050), 10_000 + 5_050 + 3_000),
        InputVerdict::Drop { release_all: true }
    );

    // 7. Legacy viewer compatibility (None): 100% accepted without mutating baseline.
    assert_eq!(hosts[7].1.total_dropped, 0);
    assert_eq!(hosts[7].1.fresh_inputs_executed, 50);
    let mut legacy_guard = InputGuard::new();
    for t in (0..10_000u64).step_by(200) {
        assert_eq!(legacy_guard.check_opt(None, t), InputVerdict::Accept);
    }
    assert_eq!(legacy_guard.stats().accepted, 50);
    assert_eq!(legacy_guard.stats().dropped, 0);
    assert_eq!(legacy_guard.stats().baseline_resets, 0);
    assert_eq!(legacy_guard.stats().releases_triggered, 0);
    // Subsequent timestamped input establishes clean baseline immediately.
    assert_eq!(legacy_guard.check_opt(Some(500), 25_020), InputVerdict::Accept);
    assert_eq!(
        legacy_guard.check_opt(Some(600), 25_020 + 100 + 2_500),
        InputVerdict::Drop { release_all: true }
    );
}

// ============================================================================
// Suite 3: HDR Tone-Mapping Adversarial Stress Suite
// ============================================================================

fn naive_clamp_srgb(rgb: [f32; 3], inv_sdr_white: f32) -> [f32; 3] {
    rgb.map(|v| {
        let c = (v.max(0.0) * inv_sdr_white).clamp(0.0, 1.0);
        if c <= 0.0031308 {
            c * 12.92
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        }
    })
}

fn srgb_oetf(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

#[test]
fn suite3_hdr_tonemapping_adversarial_stress() {
    let sdr_nits_sliders = [80.0f32, 200.0, 240.0, 400.0];

    println!();
    println!("========================================================================================================================");
    println!("SUITE 3: HDR TONE-MAPPING ADVERSARIAL STRESS (Naive Saturate Clamp vs Upgraded C1 Smooth Rolloff)");
    println!("========================================================================================================================");
    println!(
        "{:<12} | {:>10} | {:>10} | {:>10} | {:>12} | {:>12} | {:>12}",
        "SDR Slider", "Upg 1.5x", "Upg 3.0x", "Upg 6.0x", "Upg Spread", "Naive Spread", "MaxSDR Err"
    );
    println!("{}", "-".repeat(100));

    for &sdr_nits in &sdr_nits_sliders {
        let scale = sdr_nits / 80.0;
        let info = DisplayHdrInfo {
            hdr_enabled: true,
            sdr_white_scale: scale,
        };
        let inv = info.inv_sdr_white();
        assert!((inv - 1.0 / scale).abs() < 1e-6);

        // 1. Sweep 1000 linear scRGB values from -10.0 to 100.0 (0 to 8000 nits).
        let mut prev_lin_out = -1.0f32;
        let mut max_sdr_err = 0.0f32;
        for i in 0..1000usize {
            let v = -10.0 + 110.0 * (i as f32) / 999.0;
            let out = hdr_scrgb_to_srgb([v, v, v], inv);
            for ch in out {
                assert!(ch.is_finite(), "Non-finite output at v={v}, slider={sdr_nits}");
                assert!(
                    (0.0..=1.0).contains(&ch),
                    "Out of [0,1] at v={v}, slider={sdr_nits}: {ch}"
                );
            }
            if v <= 0.0 {
                assert_eq!(out, [0.0, 0.0, 0.0]);
            } else {
                assert!(
                    out[0] >= prev_lin_out,
                    "Monotonicity violated at v={v}, slider={sdr_nits}: {} < {prev_lin_out}",
                    out[0]
                );
                // Strictly increasing up to 40x SDR white (where f32 ULP > step delta).
                if v <= 40.0 * scale {
                    assert!(
                        out[0] > prev_lin_out,
                        "Strict monotonicity violated at v={v}, slider={sdr_nits}: {} <= {prev_lin_out}",
                        out[0]
                    );
                }
            }
            prev_lin_out = out[0];

            // Exact SDR linear accuracy below knee (0.0 <= v <= 0.85 * sdr_white).
            if v >= 0.0 && v <= 0.85 * scale {
                let expected = srgb_oetf(v * inv);
                let err = (out[0] - expected).abs();
                max_sdr_err = max_sdr_err.max(err);
                assert!(
                    err < 1e-6,
                    "SDR accuracy violated below knee at v={v}, slider={sdr_nits}: err={err}"
                );
            }
        }

        // 2. Log-spaced 1000-point sweep across [0.0, 100.0] proving strict monotonicity
        //    all the way to 8000 nits (100.0 scRGB).
        let mut prev_log = -1.0f32;
        for i in 0..1000usize {
            let v = if i == 0 {
                0.0
            } else {
                0.001 * 100_000.0f32.powf((i - 1) as f32 / 998.0)
            };
            let out = hdr_scrgb_to_srgb([v, v, v], inv);
            assert!(
                out[0] > prev_log,
                "Log sweep strict monotonicity violated at v={v}, slider={sdr_nits}: {} <= {prev_log}",
                out[0]
            );
            prev_log = out[0];
        }

        // 3. Highlight separation at 400 nits (5.0), 1000 nits (12.5), 4000 nits (50.0).
        let o400 = hdr_scrgb_to_srgb([5.0, 5.0, 5.0], inv)[0];
        let o1000 = hdr_scrgb_to_srgb([12.5, 12.5, 12.5], inv)[0];
        let o4000 = hdr_scrgb_to_srgb([50.0, 50.0, 50.0], inv)[0];
        assert!(o400 < o1000 && o1000 < o4000);

        // 4. Compare Naive Clamp vs Upgraded at 1.5x, 3.0x, 6.0x SDR white.
        let u_15 = hdr_scrgb_to_srgb([1.5 * scale; 3], inv)[0];
        let u_30 = hdr_scrgb_to_srgb([3.0 * scale; 3], inv)[0];
        let u_60 = hdr_scrgb_to_srgb([6.0 * scale; 3], inv)[0];
        let n_15 = naive_clamp_srgb([1.5 * scale; 3], inv)[0];
        let _n_30 = naive_clamp_srgb([3.0 * scale; 3], inv)[0];
        let n_60 = naive_clamp_srgb([6.0 * scale; 3], inv)[0];

        let upg_spread = u_60 - u_15;
        let naive_spread = n_60 - n_15;
        assert_eq!(naive_spread, 0.0, "Naive clamp must flat-clip above 1.0x SDR white");
        assert!(u_15 < u_30 && u_30 < u_60);
        assert!(upg_spread > 0.008, "Upgraded must preserve visible highlight contrast");

        println!(
            "{:>4.0} nits    | {:>10.6} | {:>10.6} | {:>10.6} | {:>12.6} | {:>12.6} | {:>12.2e}",
            sdr_nits, u_15, u_30, u_60, upg_spread, naive_spread, max_sdr_err
        );
    }
    println!("========================================================================================================================");
}

// ============================================================================
// Suite 4: Minimize Zero-Rebuild FPS Throttle Verification
// ============================================================================

#[test]
fn suite4_zero_rebuild_fps_throttle_verification() {
    let encoder_fps_list = [30u32, 60, 120, 144, 240];
    let throttle_list = [0u8, 5, 10];

    println!();
    println!("==========================================================================================");
    println!("SUITE 4: MINIMIZE ZERO-REBUILD FPS THROTTLE VERIFICATION");
    println!("==========================================================================================");
    println!(
        "{:<12} | {:<12} | {:<15} | {:<18}",
        "Encoder FPS", "Throttle", "Effective FPS", "Encode/BW Cut"
    );
    println!("{}", "-".repeat(66));

    for &fps in &encoder_fps_list {
        for &thr in &throttle_list {
            let eff = effective_pace_fps(fps, thr);
            let expected = if thr == 0 {
                fps
            } else {
                fps.min(thr as u32).max(1)
            };
            assert_eq!(eff, expected);

            let cut_pct = ((fps as f64 - eff as f64) / fps as f64) * 100.0;
            println!(
                "{:<12} | {:<12} | {:<15} | {:>16.2}%",
                fps, thr, eff, cut_pct
            );

            if fps == 120 && thr == 5 {
                assert!((cut_pct - 95.833333).abs() < 1e-3);
            }
        }
    }
    println!("==========================================================================================");

    // Edge cases for effective_pace_fps.
    assert_eq!(effective_pace_fps(3, 5), 3);
    assert_eq!(effective_pace_fps(0, 5), 1);

    // Wire round-trip verification for Msg::Throttle and Msg::InputTick.
    let wire_msgs = [
        Msg::Throttle { fps: 0 },
        Msg::Throttle { fps: 5 },
        Msg::Throttle { fps: 10 },
        Msg::Throttle { fps: 255 },
        Msg::InputTick { ms: 0 },
        Msg::InputTick { ms: 123_456_789 },
        Msg::InputTick { ms: u32::MAX },
    ];
    for m in wire_msgs {
        let encoded = m.encode();
        let len = u32::from_be_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]) as usize;
        assert_eq!(len, encoded.len() - 4);
        assert_eq!(Msg::decode(&encoded[4..]).unwrap(), m);
    }
}
