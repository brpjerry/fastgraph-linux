//! Sweep stimulus: exponential sine sweep (Farina), markers, wake noise, WAV export.

use std::f64::consts::PI;
use std::path::Path;

use anyhow::Context;
use realfft::RealFftPlanner;
use realfft::num_complex::Complex;
use serde::{Deserialize, Serialize};

/// The sweep's start frequency.
pub const SWEEP_F1: f64 = 10.0;
/// The sweep's end frequency.
pub const SWEEP_F2: f64 = 20_000.0;
/// The lowest capture rate a sweep can be measured at: below it the 20 kHz
/// reference sweep aliases.
pub const MIN_SWEEP_RATE: u32 = 44_100;
const SWEEP_PEAK: f32 = 0.5;
const FADE_SECONDS: f64 = 0.005;
/// Long enough for Bluetooth sinks that mute or fade in on stream start.
const WAKE_SECONDS: f64 = 1.5;
const MARKER_SECONDS: f64 = 0.1;
/// Gap between marker A and the sweep, and between the sweep and marker B.
const GAP_SECONDS: f64 = 0.5;
const TAIL_SECONDS: f64 = 0.3;
const WAKE_LEVEL_DB: f64 = -20.0;
const PINK_F_LO: f64 = 10.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SweepLength {
    Short,
    #[default]
    Normal,
    Long,
}

impl SweepLength {
    pub fn seconds(self) -> f64 {
        match self {
            SweepLength::Short => 3.0,
            SweepLength::Normal => 6.0,
            SweepLength::Long => 11.0,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SweepLength::Short => "Short",
            SweepLength::Normal => "Normal",
            SweepLength::Long => "Long",
        }
    }
}

/// Rendered measurement signal with the sample positions of its parts.
pub struct Stimulus {
    pub rate: u32,
    pub samples: Vec<f32>,
    pub marker_a: usize,
    pub sweep_start: usize,
    pub sweep_len: usize,
    pub marker_b: usize,
}

fn secs_to_samples(seconds: f64, rate: u32) -> usize {
    (seconds * rate as f64).round() as usize
}

/// Seconds from the start of marker A to the start of marker B.
pub fn expected_spacing(length: SweepLength) -> f64 {
    length.seconds() + MARKER_SECONDS + 2.0 * GAP_SECONDS
}

fn peak_normalise(x: &mut [f32]) {
    let peak = x.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
    if peak > 0.0 {
        x.iter_mut().for_each(|v| *v /= peak);
    }
}

/// Exponential sine sweep from `f1` to `f2` over `seconds`, unfaded, unit peak.
pub fn ess(f1: f64, f2: f64, seconds: f64, rate: u32) -> Vec<f32> {
    let n = secs_to_samples(seconds, rate);
    let k = (f2 / f1).ln();
    let mut x: Vec<f32> = (0..n)
        .map(|i| {
            let t = i as f64 / rate as f64;
            (2.0 * PI * f1 * seconds / k * ((t * k / seconds).exp() - 1.0)).sin() as f32
        })
        .collect();
    peak_normalise(&mut x);
    x
}

/// Down-chirp 10 kHz to 500 Hz, 0.1 s, peak 0.5, 10 % Tukey taper.
pub fn marker(rate: u32) -> Vec<f32> {
    let mut x = ess(10_000.0, 500.0, MARKER_SECONDS, rate);
    let n = x.len();
    let taper = n / 20; // 5 % per side makes a 10 % Tukey window
    for i in 0..taper {
        let w = 0.5 * (1.0 - (PI * i as f64 / taper as f64).cos()) as f32;
        x[i] *= w;
        x[n - 1 - i] *= w;
    }
    x.iter_mut().for_each(|v| *v *= 0.5);
    x
}

fn raised_cosine_fades(x: &mut [f32], rate: u32) {
    let n = x.len();
    let f = secs_to_samples(FADE_SECONDS, rate).min(n / 2);
    for i in 0..f {
        let w = 0.5 * (1.0 - (PI * i as f64 / f as f64).cos()) as f32;
        x[i] *= w;
        x[n - 1 - i] *= w;
    }
}

fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// Pink noise (1/sqrt(f) magnitude, random phase) synthesised in the frequency domain, unit RMS.
/// Band-limited to 10 Hz up to 20 kHz (or 0.98 x Nyquist if lower): bins outside are zero.
pub fn pink_noise(len: usize, rate: u32, seed: u64) -> Vec<f32> {
    if len < 2 {
        return vec![0.0; len];
    }
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for _ in 0..4 {
        xorshift(&mut state);
    }
    let bins = len / 2 + 1;
    let mut spectrum = vec![Complex::new(0.0f64, 0.0); bins];
    let f_hi = 20_000.0_f64.min(0.98 * 0.5 * rate as f64);
    for (k, bin) in spectrum.iter_mut().enumerate().skip(1) {
        let f = k as f64 * rate as f64 / len as f64;
        if (PINK_F_LO..=f_hi).contains(&f) {
            let phase = (xorshift(&mut state) >> 11) as f64 / (1u64 << 53) as f64 * 2.0 * PI;
            *bin = Complex::from_polar(1.0 / f.sqrt(), phase);
        }
    }
    if len.is_multiple_of(2) {
        spectrum[bins - 1].im = 0.0;
    }
    let ifft = RealFftPlanner::<f64>::new().plan_fft_inverse(len);
    let mut out = ifft.make_output_vec();
    ifft.process(&mut spectrum, &mut out)
        .expect("buffer sizes match the plan");
    let rms = (out.iter().map(|v| v * v).sum::<f64>() / len as f64).sqrt();
    let scale = if rms > 0.0 { 1.0 / rms } else { 0.0 };
    out.iter().map(|&v| (v * scale) as f32).collect()
}

/// Wake noise, marker A, sweep, marker B and a tail.
pub fn sweep_stimulus(length: SweepLength, rate: u32) -> Stimulus {
    let l = length.seconds();
    let marker_a = secs_to_samples(WAKE_SECONDS, rate);
    let sweep_start = secs_to_samples(WAKE_SECONDS + MARKER_SECONDS + GAP_SECONDS, rate);
    let marker_b = secs_to_samples(WAKE_SECONDS + MARKER_SECONDS + 2.0 * GAP_SECONDS + l, rate);
    let m = marker(rate);
    let mut sweep = ess(SWEEP_F1, SWEEP_F2, l, rate);
    raised_cosine_fades(&mut sweep, rate);
    sweep.iter_mut().for_each(|v| *v *= SWEEP_PEAK);
    let sweep_len = sweep.len();
    let total = marker_b + m.len() + secs_to_samples(TAIL_SECONDS, rate);

    let sweep_rms =
        (sweep.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / sweep_len as f64).sqrt();
    let wake_gain = sweep_rms * 10f64.powf(WAKE_LEVEL_DB / 20.0);
    let mut samples = vec![0.0f32; total];
    for (dst, src) in samples[..marker_a]
        .iter_mut()
        .zip(pink_noise(marker_a, rate, 1))
    {
        *dst = (src as f64 * wake_gain) as f32;
    }
    // Fade the noise in and out so it does not step into marker A.
    raised_cosine_fades(&mut samples[..marker_a], rate);
    samples[marker_a..marker_a + m.len()].copy_from_slice(&m);
    samples[sweep_start..sweep_start + sweep_len].copy_from_slice(&sweep);
    samples[marker_b..marker_b + m.len()].copy_from_slice(&m);
    Stimulus {
        rate,
        samples,
        marker_a,
        sweep_start,
        sweep_len,
        marker_b,
    }
}

/// Writes 24-bit PCM with the same signal on every channel, via a new temporary file and rename.
pub fn write_wav(path: &Path, samples: &[f32], rate: u32, channels: u16) -> anyhow::Result<()> {
    let spec = hound::WavSpec {
        channels,
        sample_rate: rate,
        bits_per_sample: 24,
        sample_format: hound::SampleFormat::Int,
    };
    let mut tmp = crate::fsutil::temp_for(path)?;
    (|| -> anyhow::Result<()> {
        let mut buffered = std::io::BufWriter::new(tmp.as_file_mut());
        let mut w = hound::WavWriter::new(&mut buffered, spec)?;
        for &s in samples {
            let v = (s.clamp(-1.0, 1.0) as f64 * 8_388_607.0).round() as i32;
            for _ in 0..channels {
                w.write_sample(v)?;
            }
        }
        w.finalize()?;
        std::io::Write::flush(&mut buffered)?;
        drop(buffered);
        tmp.as_file().sync_all()?;
        Ok(())
    })()
    .with_context(|| format!("writing {}", path.display()))?;
    crate::fsutil::persist(tmp, path)
}

/// e.g. `fastgraph-sweep-normal-48k.wav`, `fastgraph-sweep-short-44k1.wav`.
pub fn sweep_file_name(length: SweepLength, rate: u32) -> String {
    let tag = if rate.is_multiple_of(1000) {
        format!("{}k", rate / 1000)
    } else if rate.is_multiple_of(100) {
        format!("{}k{}", rate / 1000, rate % 1000 / 100)
    } else {
        rate.to_string()
    };
    format!(
        "fastgraph-sweep-{}-{tag}.wav",
        length.label().to_lowercase()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn layout_matches_spec() {
        let s = sweep_stimulus(SweepLength::Normal, 48_000);
        // Marker A at 1.5 s, the sweep at 2.1 s, marker B at 2.6 s + L.
        assert_eq!(s.marker_a, 72_000);
        assert_eq!(s.sweep_start, 100_800);
        assert_eq!(s.sweep_len, 288_000);
        assert_eq!(s.marker_b, 124_800 + 288_000);
        assert_eq!(s.samples.len(), s.marker_b + 4_800 + 14_400);
        let spacing = (s.marker_b - s.marker_a) as f64 / 48_000.0;
        assert_eq!(spacing, expected_spacing(SweepLength::Normal));
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn sweep_peak_and_fades() {
        let s = sweep_stimulus(SweepLength::Short, 48_000);
        let seg = &s.samples[s.sweep_start..s.sweep_start + s.sweep_len];
        let peak = seg.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
        assert!((peak - 0.5).abs() < 0.01, "peak {peak}");
        assert!(seg[0].abs() < 1e-3);
        assert!(seg[seg.len() - 1].abs() < 1e-3);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn wake_noise_is_20db_below_sweep() {
        let s = sweep_stimulus(SweepLength::Normal, 48_000);
        let seg = &s.samples[s.sweep_start..s.sweep_start + s.sweep_len];
        let db = 20.0 * (rms(&s.samples[..s.marker_a]) / rms(seg)).log10();
        assert!((db + 20.0).abs() < 1.0, "{db} dB");
    }

    fn freq_near(x: &[f32], rate: u32, t: f64) -> f64 {
        let start = (t * rate as f64) as usize;
        let mut crossings = Vec::new();
        for i in start..x.len() - 1 {
            if x[i] <= 0.0 && x[i + 1] > 0.0 {
                let frac = -x[i] as f64 / (x[i + 1] - x[i]) as f64;
                crossings.push(i as f64 + frac);
                if crossings.len() == 2 {
                    break;
                }
            }
        }
        rate as f64 / (crossings[1] - crossings[0])
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn ess_instantaneous_frequency_is_rate_independent() {
        let a = freq_near(&ess(10.0, 20_000.0, 6.0, 44_100), 44_100, 3.0);
        let b = freq_near(&ess(10.0, 20_000.0, 6.0, 96_000), 96_000, 3.0);
        assert!((a / b - 1.0).abs() < 0.01, "{a} vs {b}");
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn wake_noise_fades_in_and_out() {
        let s = sweep_stimulus(SweepLength::Normal, 48_000);
        let wake = &s.samples[..s.marker_a];
        let level = rms(wake);
        assert!(wake[0].abs() < 1e-6, "starts at {}", wake[0]);
        assert!(
            wake[wake.len() - 1].abs() < 1e-6,
            "ends at {}",
            wake[wake.len() - 1]
        );
        // Within the last 1 ms the 5 ms fade keeps the noise well below its RMS.
        let edge = wake[wake.len() - 48..]
            .iter()
            .fold(0.0f32, |m, &v| m.max(v.abs()));
        assert!(f64::from(edge) < 0.5 * level, "edge {edge} vs rms {level}");
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn ess_absolute_frequency() {
        let f = freq_near(&ess(10.0, 20_000.0, 6.0, 48_000), 48_000, 3.0);
        let want = 10.0 * 2000f64.sqrt();
        assert!((f / want - 1.0).abs() < 0.01, "{f} Hz, want {want}");
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn marker_shape() {
        let m = marker(48_000);
        let peak = m.iter().fold(0.0f32, |p, &v| p.max(v.abs()));
        assert!((peak - 0.5).abs() < 0.01, "peak {peak}");
        assert!(m[0].abs() < 1e-3);
        assert!(m[m.len() - 1].abs() < 1e-3);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn pink_noise_unit_rms_and_deterministic() {
        let a = pink_noise(48_000, 48_000, 3);
        assert!((rms(&a) - 1.0).abs() < 0.01, "rms {}", rms(&a));
        assert_eq!(a, pink_noise(48_000, 48_000, 3));
        assert_ne!(a, pink_noise(48_000, 48_000, 4));
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn wav_round_trips_24bit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.wav");
        let input: Vec<f32> = (0..1000).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
        write_wav(&path, &input, 48_000, 2).unwrap();
        let mut r = hound::WavReader::open(&path).unwrap();
        let spec = r.spec();
        assert_eq!(spec.bits_per_sample, 24);
        assert_eq!(spec.channels, 2);
        assert_eq!(spec.sample_rate, 48_000);
        let out: Vec<i32> = r.samples::<i32>().map(|v| v.unwrap()).collect();
        assert_eq!(out.len(), input.len() * 2);
        for (i, &x) in input.iter().enumerate() {
            for c in 0..2 {
                let y = out[i * 2 + c] as f32 / 8_388_607.0;
                assert!((x - y).abs() <= 1.0 / 8_388_608.0, "{x} vs {y}");
            }
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn wav_export_leaves_an_existing_tmp_sibling_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.wav");
        let sibling = dir.path().join("t.wav.tmp");
        std::fs::write(&sibling, b"keep me").unwrap();
        write_wav(&path, &[0.0, 0.5], 48_000, 1).unwrap();
        assert_eq!(std::fs::read(&sibling).unwrap(), b"keep me");
        assert_eq!(hound::WavReader::open(&path).unwrap().len(), 2);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn file_names() {
        assert_eq!(
            sweep_file_name(SweepLength::Short, 44_100),
            "fastgraph-sweep-short-44k1.wav"
        );
        assert_eq!(
            sweep_file_name(SweepLength::Long, 96_000),
            "fastgraph-sweep-long-96k.wav"
        );
    }
}
