//! Opus codec wrapper, downmix/resampling, and silence gate.

use anyhow::{Result, bail};
use rusty_opus::{Application, Bandwidth, OpusDecoder, OpusEncoder, SignalType};

pub const SAMPLE_RATE: u32 = 48_000;
/// 20 ms at 48 kHz.
pub const FRAME_SAMPLES: usize = 960;
/// Largest Opus packet expected for a 20 ms frame.
pub const MAX_PACKET_BYTES: usize = 1500;
pub const CODEC_OPUS: u8 = 0;
pub const CODEC_PCM24: u8 = 1;

/// Snaps a device sample rate to a clean standard PCM rate for lossless transport.
pub fn normalize_lossless_rate(rate: u32) -> u32 {
    match rate {
        44_100 | 48_000 | 88_200 | 96_000 | 176_400 | 192_000 => rate,
        _ => SAMPLE_RATE,
    }
}

/// Encodes interleaved f32 samples (-1.0..=1.0) into packed 24-bit signed little-endian PCM.
pub fn encode_pcm24(pcm: &[f32], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(pcm.len() * 3);
    for &s in pcm {
        let q = (s.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32;
        let b = q.to_le_bytes();
        out.extend_from_slice(&b[..3]);
    }
}

/// Decodes packed 24-bit signed little-endian PCM into f32 samples.
pub fn decode_pcm24(bytes: &[u8], out: &mut Vec<f32>) {
    out.clear();
    out.reserve(bytes.len() / 3);
    for tri in bytes.chunks_exact(3) {
        let sign = if tri[2] & 0x80 != 0 { 0xFF } else { 0x00 };
        let q = i32::from_le_bytes([tri[0], tri[1], tri[2], sign]);
        out.push(q as f32 / 8_388_608.0);
    }
}

pub struct Encoder {
    enc: OpusEncoder,
    channels: u8,
}

impl Encoder {
    pub fn new(channels: u8) -> Result<Self> {
        let channels = channels.clamp(1, 2);
        let mut enc = OpusEncoder::new(SAMPLE_RATE as i32, channels as usize, Application::Audio)?;
        enc.bitrate_bps = if channels == 1 { 128_000 } else { 256_000 };
        enc.complexity = 10;
        enc.max_bandwidth = Bandwidth::Fullband;
        enc.force_bandwidth = Some(Bandwidth::Fullband);
        enc.signal_type = Some(SignalType::Music);
        Ok(Self { enc, channels })
    }

    pub fn encode(&mut self, channels: u8, pcm: &[f32], out: &mut [u8]) -> Result<usize> {
        let channels = channels.clamp(1, 2);
        if self.channels != channels {
            *self = Self::new(channels)?;
        }
        let need = FRAME_SAMPLES * channels as usize;
        if pcm.len() < need {
            bail!("pcm frame too short ({} < {need})", pcm.len());
        }
        Ok(self.enc.encode(&pcm[..need], FRAME_SAMPLES, out)?)
    }
}

pub struct Decoder {
    dec: OpusDecoder,
    channels: u8,
}

impl Decoder {
    pub fn new(channels: u8) -> Result<Self> {
        let channels = channels.clamp(1, 2);
        let dec = OpusDecoder::new(SAMPLE_RATE as i32, channels as usize)?;
        Ok(Self { dec, channels })
    }

    pub fn channels(&self) -> u8 {
        self.channels
    }

    pub fn decode(&mut self, channels: u8, packet: &[u8], out: &mut [f32]) -> Result<usize> {
        let channels = channels.clamp(1, 2);
        if self.channels != channels {
            *self = Self::new(channels)?;
        }
        let need = FRAME_SAMPLES * channels as usize;
        if out.len() < need {
            bail!("pcm output buffer too small");
        }
        Ok(self.dec.decode(packet, FRAME_SAMPLES, &mut out[..need])?)
    }

    pub fn decode_plc(&mut self, out: &mut [f32]) -> Result<usize> {
        let need = FRAME_SAMPLES * self.channels as usize;
        if out.len() < need {
            bail!("pcm output buffer too small");
        }
        Ok(self.dec.decode(&[], FRAME_SAMPLES, &mut out[..need])?)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleFmt {
    F32,
    I16,
    I24,
    I32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcmFormat {
    pub rate: u32,
    pub channels: u16,
    pub sample: SampleFmt,
}

/// Converts interleaved PCM across sample formats, channel layouts, and sample rates,
/// keeping fractional resampling phase across calls.
#[derive(Default)]
pub struct DownmixResampler {
    has_prev: bool,
    prev: [f32; 2],
    phase: f64,
    last_rates: (u32, u32),
    scratch: Vec<f32>,
    stereo_buf: Vec<[f32; 2]>,
}

impl DownmixResampler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.has_prev = false;
        self.prev = [0.0; 2];
        self.phase = 0.0;
    }

    pub fn process_raw(&mut self, bytes: &[u8], fmt: PcmFormat, dst_rate: u32, dst_ch: usize, out: &mut Vec<f32>) {
        self.scratch.clear();
        match fmt.sample {
            SampleFmt::F32 => {
                for ch in bytes.chunks_exact(4) {
                    self.scratch.push(f32::from_le_bytes([ch[0], ch[1], ch[2], ch[3]]));
                }
            }
            SampleFmt::I16 => {
                for ch in bytes.chunks_exact(2) {
                    self.scratch.push(i16::from_le_bytes([ch[0], ch[1]]) as f32 / 32768.0);
                }
            }
            SampleFmt::I24 => {
                for ch in bytes.chunks_exact(3) {
                    let sign = if ch[2] & 0x80 != 0 { 0xFF } else { 0x00 };
                    let q = i32::from_le_bytes([ch[0], ch[1], ch[2], sign]);
                    self.scratch.push(q as f32 / 8_388_608.0);
                }
            }
            SampleFmt::I32 => {
                for ch in bytes.chunks_exact(4) {
                    let q = i32::from_le_bytes([ch[0], ch[1], ch[2], ch[3]]);
                    self.scratch.push((q as f64 / 2_147_483_648.0) as f32);
                }
            }
        }
        let src_buf = std::mem::take(&mut self.scratch);
        self.process_f32(&src_buf, fmt.rate, fmt.channels as usize, dst_rate, dst_ch, out);
        self.scratch = src_buf;
    }

    pub fn process_f32(
        &mut self,
        src: &[f32],
        src_rate: u32,
        src_ch: usize,
        dst_rate: u32,
        dst_ch: usize,
        out: &mut Vec<f32>,
    ) {
        if src_ch == 0 || dst_ch == 0 || src_rate == 0 || dst_rate == 0 {
            return;
        }
        let n_in = src.len() / src_ch;
        if n_in == 0 {
            return;
        }
        if self.last_rates != (src_rate, dst_rate) {
            self.reset();
            self.last_rates = (src_rate, dst_rate);
        }

        if src_rate == dst_rate {
            for frame in src.chunks_exact(src_ch) {
                let [l, r] = to_stereo(frame);
                push_channels(out, l, r, dst_ch);
            }
            self.prev = to_stereo(&src[(n_in - 1) * src_ch..n_in * src_ch]);
            self.has_prev = true;
            self.phase = 0.0;
            return;
        }

        self.stereo_buf.clear();
        self.stereo_buf.reserve(n_in);
        for frame in src.chunks_exact(src_ch) {
            self.stereo_buf.push(to_stereo(frame));
        }

        let step = src_rate as f64 / dst_rate as f64;
        let limit = (n_in - 1) as f64;
        let mut pos = self.phase;
        if !self.has_prev && pos < 0.0 {
            pos = 0.0;
        }

        while pos < limit {
            let [l, r] = if pos < 0.0 {
                let frac = (pos + 1.0) as f32;
                let next = self.stereo_buf[0];
                [
                    self.prev[0] + (next[0] - self.prev[0]) * frac,
                    self.prev[1] + (next[1] - self.prev[1]) * frac,
                ]
            } else {
                let idx = pos as usize;
                let frac = (pos - idx as f64) as f32;
                let a = self.stereo_buf[idx];
                let b = self.stereo_buf[(idx + 1).min(n_in - 1)];
                [a[0] + (b[0] - a[0]) * frac, a[1] + (b[1] - a[1]) * frac]
            };
            push_channels(out, l, r, dst_ch);
            pos += step;
        }

        self.prev = self.stereo_buf[n_in - 1];
        self.has_prev = true;
        self.phase = pos - n_in as f64;
    }
}

#[cfg(test)]
pub fn convert_pcm(src: &[f32], src_rate: u32, src_ch: usize, dst_rate: u32, dst_ch: usize) -> Vec<f32> {
    let mut r = DownmixResampler::new();
    let mut out = Vec::new();
    r.process_f32(src, src_rate, src_ch, dst_rate, dst_ch, &mut out);
    out
}

#[inline]
fn to_stereo(frame: &[f32]) -> [f32; 2] {
    const SQRT1_2: f32 = std::f32::consts::FRAC_1_SQRT_2;
    match frame.len() {
        0 => [0.0, 0.0],
        1 => [frame[0], frame[0]],
        2 => [frame[0], frame[1]],
        4 => [
            (frame[0] + SQRT1_2 * frame[2]).clamp(-1.0, 1.0),
            (frame[1] + SQRT1_2 * frame[3]).clamp(-1.0, 1.0),
        ],
        6 => [
            (frame[0] + SQRT1_2 * frame[2] + SQRT1_2 * frame[4]).clamp(-1.0, 1.0),
            (frame[1] + SQRT1_2 * frame[2] + SQRT1_2 * frame[5]).clamp(-1.0, 1.0),
        ],
        n if n >= 8 => [
            (frame[0] + SQRT1_2 * frame[2] + SQRT1_2 * frame[4] + 0.5 * frame[6]).clamp(-1.0, 1.0),
            (frame[1] + SQRT1_2 * frame[2] + SQRT1_2 * frame[5] + 0.5 * frame[7]).clamp(-1.0, 1.0),
        ],
        _ => [
            (frame[0] + SQRT1_2 * frame[2]).clamp(-1.0, 1.0),
            (frame[1] + SQRT1_2 * frame[2]).clamp(-1.0, 1.0),
        ],
    }
}

#[inline]
fn push_channels(out: &mut Vec<f32>, l: f32, r: f32, dst_ch: usize) {
    match dst_ch {
        1 => out.push(0.5 * (l + r)),
        2 => {
            out.push(l);
            out.push(r);
        }
        n => {
            out.push(l);
            out.push(r);
            out.resize(out.len() + n - 2, 0.0);
        }
    }
}

/// Drops silent periods after 25 consecutive quiet 20 ms frames (500 ms).
#[derive(Default)]
pub struct SilenceGate {
    quiet_frames: u32,
}

impl SilenceGate {
    const THRESHOLD: f32 = 5e-5;
    #[cfg(test)]
    const HANGOVER_FRAMES: u32 = 25;

    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.quiet_frames = 0;
    }

    #[cfg(test)]
    pub fn check(&mut self, pcm: &[f32]) -> bool {
        self.check_hangover(pcm, Self::HANGOVER_FRAMES)
    }

    pub fn check_hangover(&mut self, pcm: &[f32], hangover_frames: u32) -> bool {
        let peak = pcm.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        if peak >= Self::THRESHOLD {
            self.quiet_frames = 0;
            true
        } else {
            self.quiet_frames = self.quiet_frames.saturating_add(1);
            self.quiet_frames <= hangover_frames
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, channels: usize, frames: usize) -> Vec<f32> {
        let mut v = Vec::with_capacity(frames * channels);
        for i in 0..frames {
            let s = 0.5 * (2.0 * std::f32::consts::PI * freq * i as f32 / SAMPLE_RATE as f32).sin();
            for c in 0..channels {
                v.push(if c == 0 { s } else { -s });
            }
        }
        v
    }

    #[test]
    fn opus_stereo_and_mono_round_trip_and_plc() {
        for ch in [1u8, 2u8] {
            let mut enc = Encoder::new(ch).unwrap();
            let mut dec = Decoder::new(ch).unwrap();
            let mut pkt = [0u8; MAX_PACKET_BYTES];
            let mut out = vec![0.0f32; FRAME_SAMPLES * ch as usize];

            // Warm up two frames so MDCT overlap produces full output energy.
            let pcm = sine(440.0, ch as usize, FRAME_SAMPLES);
            for _ in 0..3 {
                let n = enc.encode(ch, &pcm, &mut pkt).unwrap();
                assert!(n > 2);
                let decoded = dec.decode(ch, &pkt[..n], &mut out).unwrap();
                assert_eq!(decoded, FRAME_SAMPLES);
            }
            let rms = (out.iter().map(|s| s * s).sum::<f32>() / out.len() as f32).sqrt();
            assert!(rms > 0.1, "ch={ch} decoded rms too low: {rms}");

            // PLC on empty packet.
            let plc = dec.decode_plc(&mut out).unwrap();
            assert_eq!(plc, FRAME_SAMPLES);
            let plc2 = dec.decode(ch, &[], &mut out).unwrap();
            assert_eq!(plc2, FRAME_SAMPLES);
        }
    }

    #[test]
    fn opus_stereo_separation_and_hf_fidelity() {
        let mut enc = Encoder::new(2).unwrap();
        let mut dec = Decoder::new(2).unwrap();
        let mut pkt = [0u8; MAX_PACKET_BYTES];
        let mut out = vec![0.0f32; FRAME_SAMPLES * 2];

        // Feed 15 frames (300 ms): Left = 440 Hz + 12 kHz, Right = 1 kHz + 15 kHz
        let mut all_out_l = Vec::new();
        let mut all_out_r = Vec::new();
        let mut total_bytes = 0usize;
        let mut last_toc = 0u8;
        for f in 0..15 {
            let mut pcm = Vec::with_capacity(FRAME_SAMPLES * 2);
            for i in 0..FRAME_SAMPLES {
                let t = (f * FRAME_SAMPLES + i) as f32 / SAMPLE_RATE as f32;
                let l = 0.35 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
                    + 0.35 * (2.0 * std::f32::consts::PI * 12_000.0 * t).sin();
                let r = 0.35 * (2.0 * std::f32::consts::PI * 1_000.0 * t).sin()
                    + 0.35 * (2.0 * std::f32::consts::PI * 15_000.0 * t).sin();
                pcm.push(l);
                pcm.push(r);
            }
            let n = enc.encode(2, &pcm, &mut pkt).unwrap();
            last_toc = pkt[0];
            total_bytes += n;
            dec.decode(2, &pkt[..n], &mut out).unwrap();
            if f >= 5 {
                for ch_pair in out.chunks_exact(2) {
                    all_out_l.push(ch_pair[0]);
                    all_out_r.push(ch_pair[1]);
                }
            }
        }
        // Goertzel / DFT magnitude at specific frequencies
        let mag = |sig: &[f32], freq: f32| -> f32 {
            let mut re = 0.0f64;
            let mut im = 0.0f64;
            for (i, &s) in sig.iter().enumerate() {
                let ph = 2.0 * std::f64::consts::PI * freq as f64 * i as f64 / SAMPLE_RATE as f64;
                re += s as f64 * ph.cos();
                im += s as f64 * ph.sin();
            }
            (2.0 * (re * re + im * im).sqrt() / sig.len() as f64) as f32
        };
        let l440 = mag(&all_out_l, 440.0);
        let l1k = mag(&all_out_l, 1_000.0);
        let l12k = mag(&all_out_l, 12_000.0);
        let l15k = mag(&all_out_l, 15_000.0);
        let r440 = mag(&all_out_r, 440.0);
        let r1k = mag(&all_out_r, 1_000.0);
        let r12k = mag(&all_out_r, 12_000.0);
        let r15k = mag(&all_out_r, 15_000.0);
        eprintln!(
            "OPUS_DIAG: avg_bytes={} toc=0x{:02X} | L: 440={:.3} 1k={:.3} 12k={:.3} 15k={:.3} | R: 440={:.3} 1k={:.3} 12k={:.3} 15k={:.3}",
            total_bytes / 15, last_toc, l440, l1k, l12k, l15k, r440, r1k, r12k, r15k
        );
        assert!(l440 > 0.25 && l12k > 0.20, "Left channel lost energy: 440={l440} 12k={l12k}");
        assert!(r1k > 0.25 && r15k > 0.20, "Right channel lost energy: 1k={r1k} 15k={r15k}");
        assert!(l1k < 0.05 && r440 < 0.05, "Severe stereo crosstalk: l1k={l1k} r440={r440}");

        // Broadband uncorrelated stereo cymbal/transient test (PRNG high-passed > 6 kHz on L vs R)
        let mut enc2 = Encoder::new(2).unwrap();
        let mut dec2 = Decoder::new(2).unwrap();
        let mut seed_l: u64 = 0x1234_5678_9abc_def0;
        let mut seed_r: u64 = 0xfedc_ba98_7654_3210;
        let next_f = |s: &mut u64| -> f32 {
            *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((*s >> 33) as i32 as f32 / i32::MAX as f32) * 0.25
        };
        let mut in_l = Vec::new();
        let mut in_r = Vec::new();
        let mut out_l = Vec::new();
        let mut out_r = Vec::new();
        let mut prev_l = 0.0f32;
        let mut prev_r = 0.0f32;
        for f in 0..15 {
            let mut pcm = Vec::with_capacity(FRAME_SAMPLES * 2);
            for i in 0..FRAME_SAMPLES {
                let nl = next_f(&mut seed_l);
                let nr = next_f(&mut seed_r);
                // Simple 1st-order high-pass (emphasizes 8-20 kHz cymbals) + transient drum hit
                let mut l = nl - prev_l;
                let mut r = nr - prev_r;
                prev_l = nl;
                prev_r = nr;
                if i < 40 && f % 2 == 0 {
                    l += 0.4 * ((i as f32) * 0.3).sin();
                }
                if (480..520).contains(&i) && f % 2 == 1 {
                    r += 0.4 * ((i as f32) * 0.5).sin();
                }
                pcm.push(l);
                pcm.push(r);
                in_l.push(l);
                in_r.push(r);
            }
            let n = enc2.encode(2, &pcm, &mut pkt).unwrap();
            dec2.decode(2, &pkt[..n], &mut out).unwrap();
            for pair in out.chunks_exact(2) {
                out_l.push(pair[0]);
                out_r.push(pair[1]);
            }
        }
        // Find best alignment lag (Opus algorithmic delay ~312 samples)
        let mut best_lag = 0usize;
        let mut best_corr = -1.0f64;
        for lag in 0..600 {
            let mut dot = 0.0f64;
            for i in 4800..10000 {
                dot += in_l[i] as f64 * out_l[i + lag] as f64;
            }
            if dot > best_corr {
                best_corr = dot;
                best_lag = lag;
            }
        }
        let mut sig_p = 0.0f64;
        let mut err_p = 0.0f64;
        let mut cross_dot = 0.0f64;
        let mut out_lp = 0.0f64;
        let mut out_rp = 0.0f64;
        for i in 4800..12000 {
            let il = in_l[i] as f64;
            let ol = out_l[i + best_lag] as f64;
            let ir = in_r[i] as f64;
            let or = out_r[i + best_lag] as f64;
            sig_p += il * il + ir * ir;
            err_p += (il - ol).powi(2) + (ir - or).powi(2);
            cross_dot += ol * or;
            out_lp += ol * ol;
            out_rp += or * or;
        }
        let snr_db = 10.0 * (sig_p / err_p.max(1e-12)).log10();
        let lr_corr = cross_dot / (out_lp * out_rp).sqrt().max(1e-12);
        let energy_ratio = (out_lp + out_rp) / sig_p;
        eprintln!(
            "OPUS_BROADBAND_DIAG: lag={} snr={:.2} dB lr_corr={:.4} energy_ratio={:.3}",
            best_lag, snr_db, lr_corr, energy_ratio
        );
    }

    #[test]
    fn downmix_51_and_71_and_mono() {
        // 5.1: FL=0.4, FR=0.2, FC=0.2, LFE=0.9 (ignored), BL=0.2, BR=0.1
        let src51 = [0.4f32, 0.2, 0.2, 0.9, 0.2, 0.1];
        let st51 = convert_pcm(&src51, 48_000, 6, 48_000, 2);
        let exp_l = 0.4 + std::f32::consts::FRAC_1_SQRT_2 * 0.2 + std::f32::consts::FRAC_1_SQRT_2 * 0.2;
        let exp_r = 0.2 + std::f32::consts::FRAC_1_SQRT_2 * 0.2 + std::f32::consts::FRAC_1_SQRT_2 * 0.1;
        assert!((st51[0] - exp_l).abs() < 1e-5);
        assert!((st51[1] - exp_r).abs() < 1e-5);

        // 7.1 adds SL=0.1, SR=0.2 * 0.5
        let src71 = [0.4f32, 0.2, 0.2, 0.9, 0.2, 0.1, 0.1, 0.2];
        let st71 = convert_pcm(&src71, 48_000, 8, 48_000, 2);
        assert!((st71[0] - (exp_l + 0.05)).abs() < 1e-5);
        assert!((st71[1] - (exp_r + 0.10)).abs() < 1e-5);

        // Stereo -> mono.
        let mono = convert_pcm(&[0.6, -0.2], 48_000, 2, 48_000, 1);
        assert!((mono[0] - 0.2).abs() < 1e-6);
    }

    #[test]
    fn resample_44100_to_48000_continuous_across_chunks() {
        let total_in = 4410; // 100 ms at 44.1 kHz
        let mut src = Vec::with_capacity(total_in * 2);
        for i in 0..total_in {
            let v = (i as f32) / 1000.0;
            src.push(v);
            src.push(-v);
        }
        let whole = convert_pcm(&src, 44_100, 2, 48_000, 2);
        let mut resampler = DownmixResampler::new();
        let mut chunked = Vec::new();
        for chunk in src.chunks(441 * 2) {
            resampler.process_f32(chunk, 44_100, 2, 48_000, 2, &mut chunked);
        }
        assert_eq!(whole.len(), chunked.len());
        // 100 ms at 48 kHz is ~4800 frames (minus 1 sample boundary).
        let out_frames = whole.len() / 2;
        assert!((4795..=4805).contains(&out_frames), "out_frames={out_frames}");
        for (a, b) in whole.iter().zip(chunked.iter()) {
            assert!((a - b).abs() < 1e-5, "chunked mismatch: {a} vs {b}");
        }
    }

    #[test]
    fn pcm24_round_trip_precision() {
        let src = [-1.0f32, -0.5, -0.0001, 0.0, 0.0001, 0.25, 0.75, 1.0];
        let mut encoded = Vec::new();
        encode_pcm24(&src, &mut encoded);
        assert_eq!(encoded.len(), src.len() * 3);
        let mut decoded = Vec::new();
        decode_pcm24(&encoded, &mut decoded);
        assert_eq!(decoded.len(), src.len());
        for (&a, &b) in src.iter().zip(decoded.iter()) {
            assert!((a - b).abs() < 3e-7, "pcm24 mismatch: {a} vs {b}");
        }
    }

    #[test]
    fn i24_and_i32_process_raw_round_trip() {
        let src = [-1.0f32, -0.5, -0.0001, 0.0, 0.0001, 0.25, 0.75, 0.999999];
        let mut resampler = DownmixResampler::new();

        // 24-bit packed LE integer PCM
        let mut raw_i24 = Vec::new();
        encode_pcm24(&src, &mut raw_i24);
        let mut out_i24 = Vec::new();
        resampler.process_raw(
            &raw_i24,
            PcmFormat { rate: 48_000, channels: 2, sample: SampleFmt::I24 },
            48_000,
            2,
            &mut out_i24,
        );
        assert_eq!(out_i24.len(), src.len());
        for (&a, &b) in src.iter().zip(out_i24.iter()) {
            assert!((a - b).abs() < 3e-7, "i24 mismatch: {a} vs {b}");
        }

        // 32-bit LE integer PCM
        let mut raw_i32 = Vec::with_capacity(src.len() * 4);
        for &s in &src {
            let q = (s as f64 * 2_147_483_647.0).round() as i32;
            raw_i32.extend_from_slice(&q.to_le_bytes());
        }
        let mut out_i32 = Vec::new();
        resampler.process_raw(
            &raw_i32,
            PcmFormat { rate: 48_000, channels: 2, sample: SampleFmt::I32 },
            48_000,
            2,
            &mut out_i32,
        );
        assert_eq!(out_i32.len(), src.len());
        for (&a, &b) in src.iter().zip(out_i32.iter()) {
            assert!((a - b).abs() < 1e-6, "i32 mismatch: {a} vs {b}");
        }

        // 24-in-32 MSB-aligned LE integer PCM (lower 8 bits zero)
        let mut raw_24in32 = Vec::with_capacity(src.len() * 4);
        for &s in &src {
            let q24 = (s.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32;
            raw_24in32.extend_from_slice(&(q24 << 8).to_le_bytes());
        }
        let mut out_24in32 = Vec::new();
        resampler.process_raw(
            &raw_24in32,
            PcmFormat { rate: 48_000, channels: 2, sample: SampleFmt::I32 },
            48_000,
            2,
            &mut out_24in32,
        );
        assert_eq!(out_24in32.len(), src.len());
        for (&a, &b) in src.iter().zip(out_24in32.iter()) {
            assert!((a - b).abs() < 3e-7, "24-in-32 mismatch: {a} vs {b}");
        }
    }

    #[test]
    fn silence_gate_hangover_and_resume() {
        let mut g = SilenceGate::new();
        let quiet = vec![1e-5f32; FRAME_SAMPLES * 2];
        let loud = vec![1e-2f32; FRAME_SAMPLES * 2];
        assert!(g.check(&loud));
        for _ in 0..25 {
            assert!(g.check(&quiet), "hangover frame should pass");
        }
        assert!(!g.check(&quiet), "frame 26 of silence should be gated");
        assert!(!g.check(&quiet));
        assert!(g.check(&loud), "active audio resumes immediately");
    }
}
