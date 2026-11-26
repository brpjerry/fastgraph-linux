//! Sweep deconvolution: drift-corrected resampling, regularised division, gating, IR to response.

use std::f64::consts::PI;
use std::sync::OnceLock;

use anyhow::{bail, ensure};
use realfft::RealFftPlanner;
use realfft::num_complex::Complex;

use crate::response::{GRID_POINTS_PER_OCTAVE, Response, log_grid};
use crate::signal::{MIN_SWEEP_RATE, SweepLength, sweep_stimulus};
pub use crate::signal::{SWEEP_F1, SWEEP_F2};

/// Half the resampling kernel length (64 taps in all).
const HALF_TAPS: i64 = 32;
/// Kaiser window β for the resampling kernel. With 64 taps the passband is flat within 0.001 dB
/// up to 20 kHz at 44.1 kHz for every fractional delay (a 32-tap Blackman kernel drooped
/// −0.9 dB there at half-sample delay).
const KAISER_BETA: f64 = 8.0;
/// Kaiser window table resolution, entries per sample of |t|.
const WINDOW_TABLE_STEPS: usize = 1024;
/// Drift at or below this is ignored (no resampling).
const DRIFT_TOLERANCE_PPM: f64 = 20.0;
/// Recording kept before the expected sweep start, for harmonics and early arrivals.
const PRE_SWEEP_SECONDS: f64 = 0.1;
/// Extra recording beyond the sweep length, pre-sweep part included: the segment ends 50 ms
/// before marker B's nominal start (0.5 s after the sweep).
const EXTRA_SECONDS: f64 = 0.55;
/// Regularisation: ε = REG_FLOOR·max|X|² inside REG_LO..REG_HI, rising to REG_CEILING·max|X|²
/// outside over REG_TRANSITION_OCTAVES.
const REG_FLOOR: f64 = 1e-6;
/// Out-of-band ε as a fraction of max|X|². ε is real, so the band limit it imposes is a
/// zero-phase filter whose kernel is symmetric about the IR peak. With ε rising to 1·max|X|² the
/// high-pass near 11 Hz is so strong that its kernel spreads about ±100 ms. The gate starts at
/// −5 ms, so it cuts the pre-peak half of that kernel, and the bass reads about 0.8 dB low at
/// 30 Hz. At 1e-3 the residual kernel is small enough (≤ 0.04 dB from 30 Hz up) while the gain
/// below 10 Hz stays capped.
const REG_CEILING: f64 = 1e-3;
const REG_LO_HZ: f64 = 15.0;
const REG_HI_HZ: f64 = 19_500.0;
const REG_TRANSITION_OCTAVES: f64 = 0.5;
const GATE_PRE_SECONDS: f64 = 0.005;
const GATE_POST_SECONDS: f64 = 0.100;
const GATE_LEFT_TAPER_SECONDS: f64 = 0.001;
const GATE_RIGHT_TAPER_SECONDS: f64 = 0.020;
/// The long IR starts this long before H9's arrival.
const LONG_IR_LEAD_SECONDS: f64 = 0.05;
/// The long IR ends this long after the peak, past the gate's end, for the noise window.
const LONG_IR_POST_SECONDS: f64 = 0.3;
/// The response FFT is at least this long, for ≤ 4 Hz bin spacing.
const RESPONSE_MIN_SECONDS: f64 = 0.25;

/// An impulse response with the index of its main peak.
#[derive(Clone, Debug, PartialEq)]
pub struct ImpulseResponse {
    pub rate: u32,
    pub samples: Vec<f32>,
    pub peak: usize,
}

fn bessel_i0(x: f64) -> f64 {
    let q = 0.25 * x * x;
    let (mut sum, mut term, mut k) = (1.0, 1.0, 1.0);
    while term > 1e-17 * sum {
        term *= q / (k * k);
        sum += term;
        k += 1.0;
    }
    sum
}

/// Kaiser window over |t| in `0..=HALF_TAPS`, sampled every `1/WINDOW_TABLE_STEPS`.
fn kaiser_table() -> &'static [f64] {
    static TABLE: OnceLock<Vec<f64>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let half = HALF_TAPS as f64;
        let norm = bessel_i0(KAISER_BETA);
        (0..=HALF_TAPS as usize * WINDOW_TABLE_STEPS + 1)
            .map(|i| {
                let r = (i as f64 / WINDOW_TABLE_STEPS as f64 / half).min(1.0);
                bessel_i0(KAISER_BETA * (1.0 - r * r).sqrt()) / norm
            })
            .collect()
    })
}

fn kaiser(table: &[f64], t: f64) -> f64 {
    let x = t.abs() * WINDOW_TABLE_STEPS as f64;
    let i = x as usize;
    let f = x - i as f64;
    table[i] + f * (table[i + 1] - table[i])
}

/// Band-limited value of `x` at the fractional position `pos` (zero outside `x`), with the
/// sinc scaled to `cutoff` (1 = Nyquist) and a Kaiser window over 64 taps.
fn interpolate(x: &[f32], pos: f64, cutoff: f64) -> f64 {
    let table = kaiser_table();
    let base = pos.floor() as i64;
    let frac = pos - base as f64;
    // sin(π·cutoff·t) for t = k − frac, advanced by rotation as k steps by one.
    let t0 = (1 - HALF_TAPS) as f64 - frac;
    let (mut sin, mut cos) = (PI * cutoff * t0).sin_cos();
    let (step_sin, step_cos) = (PI * cutoff).sin_cos();
    let mut acc = 0.0;
    for k in (1 - HALF_TAPS)..=HALF_TAPS {
        let t = k as f64 - frac;
        let i = base + k;
        if i >= 0 && i < x.len() as i64 {
            let sinc = if t == 0.0 {
                1.0
            } else {
                sin / (PI * cutoff * t)
            };
            acc += f64::from(x[i as usize]) * cutoff * sinc * kaiser(table, t);
        }
        (sin, cos) = (
            sin * step_cos + cos * step_sin,
            cos * step_cos - sin * step_sin,
        );
    }
    acc
}

/// `out[n] = x(start + n·ratio)`.
fn resample_from(x: &[f32], start: f64, ratio: f64, len: usize) -> Vec<f32> {
    let cutoff = (1.0 / ratio).min(1.0);
    (0..len)
        .map(|n| interpolate(x, start + n as f64 * ratio, cutoff) as f32)
        .collect()
}

/// `y[n] = x(n·ratio)` by 64-tap Kaiser-windowed sinc; length `floor((len−1)/ratio)+1`.
/// When `ratio > 1` the sinc cutoff drops to `1/ratio` of Nyquist to avoid aliasing.
pub fn resample(x: &[f32], ratio: f64) -> Vec<f32> {
    if x.is_empty() || !(ratio.is_finite() && ratio > 0.0) {
        return Vec::new();
    }
    let len = ((x.len() - 1) as f64 / ratio).floor() as usize + 1;
    resample_from(x, 0.0, ratio, len)
}

/// Fraction of max|X|² used as ε at `f`: `REG_FLOOR` inside the band, rising to `REG_CEILING`
/// over half an octave outside it with a raised-cosine weight on log ε.
fn regularisation(f: f64) -> f64 {
    let octaves_out = if f <= 0.0 {
        f64::INFINITY
    } else if f < REG_LO_HZ {
        (REG_LO_HZ / f).log2()
    } else if f > REG_HI_HZ {
        (f / REG_HI_HZ).log2()
    } else {
        0.0
    };
    let t = (octaves_out / REG_TRANSITION_OCTAVES).min(1.0);
    let w = 0.5 * (1.0 - (PI * t).cos());
    REG_FLOOR.powf(1.0 - w) * REG_CEILING.powf(w)
}

/// Deconvolves the sweep found in `recording`, with marker A starting at `marker_a` (a
/// sub-sample position). The recording is re-mapped onto the stimulus timeline as
/// `y[n] = recording(marker_a + n·drift_ratio)` (plain integer offset when the drift is within
/// ±20 ppm). The IR peak of a system without delay lands 0.1 s into the result; arrivals before
/// that (and harmonic distortion products) wrap to the end of the circular buffer.
pub fn deconvolve(
    recording: &[f32],
    marker_a: f64,
    drift_ratio: f64,
    length: SweepLength,
    rate: u32,
) -> anyhow::Result<ImpulseResponse> {
    ensure!(
        marker_a.is_finite() && marker_a >= 0.0,
        "invalid marker position {marker_a}"
    );
    ensure!(
        drift_ratio.is_finite() && drift_ratio > 0.0,
        "invalid drift ratio {drift_ratio}"
    );
    ensure!(
        rate >= MIN_SWEEP_RATE,
        "a {rate} Hz capture is too slow for the sweep"
    );
    let stim = sweep_stimulus(length, rate);
    let reference = &stim.samples[stim.sweep_start..stim.sweep_start + stim.sweep_len];
    let fs = f64::from(rate);
    let pre = (PRE_SWEEP_SECONDS * fs).round() as usize;
    let seg_len = stim.sweep_len + (EXTRA_SECONDS * fs).round() as usize;
    // Segment start on the stimulus timeline, relative to marker A.
    let offset = stim.sweep_start - stim.marker_a - pre;

    let drift_ppm = (drift_ratio - 1.0) * 1e6;
    let segment = if drift_ppm.abs() <= DRIFT_TOLERANCE_PPM {
        let start = marker_a.round() as usize + offset;
        match recording.get(start..start + seg_len) {
            Some(s) => s.to_vec(),
            None => bail!(
                "recording too short: sweep segment ends at sample {} of {}",
                start + seg_len,
                recording.len()
            ),
        }
    } else {
        let start = marker_a + offset as f64 * drift_ratio;
        let last = start + (seg_len - 1) as f64 * drift_ratio;
        if last > (recording.len() as f64 - 1.0) {
            bail!(
                "recording too short: sweep segment ends at sample {last:.0} of {}",
                recording.len()
            );
        }
        resample_from(recording, start, drift_ratio, seg_len)
    };

    let n = (seg_len + stim.sweep_len).next_power_of_two();
    let mut planner = RealFftPlanner::<f64>::new();
    let forward = planner.plan_fft_forward(n);
    let inverse = planner.plan_fft_inverse(n);
    let spectrum = |x: &[f32]| -> anyhow::Result<Vec<Complex<f64>>> {
        let mut input = forward.make_input_vec();
        input
            .iter_mut()
            .zip(x)
            .for_each(|(d, &s)| *d = f64::from(s));
        let mut out = forward.make_output_vec();
        forward
            .process(&mut input, &mut out)
            .map_err(|e| anyhow::anyhow!("forward FFT: {e}"))?;
        Ok(out)
    };
    let x = spectrum(reference)?;
    let mut y = spectrum(&segment)?;
    let max_power = x.iter().map(|c| c.norm_sqr()).fold(0.0, f64::max);
    ensure!(max_power > 0.0, "empty sweep reference");
    for (k, (yk, xk)) in y.iter_mut().zip(&x).enumerate() {
        let f = k as f64 * fs / n as f64;
        let eps = regularisation(f) * max_power;
        *yk = *yk * xk.conj() / (xk.norm_sqr() + eps);
    }
    y[0].im = 0.0;
    let last = y.len() - 1;
    y[last].im = 0.0;
    let mut h = inverse.make_output_vec();
    inverse
        .process(&mut y, &mut h)
        .map_err(|e| anyhow::anyhow!("inverse FFT: {e}"))?;
    let scale = 1.0 / n as f64;
    let samples: Vec<f32> = h.iter().map(|&v| (v * scale) as f32).collect();
    let peak = samples
        .iter()
        .enumerate()
        .fold((0, 0.0f32), |best, (i, &v)| {
            if v.abs() > best.1 { (i, v.abs()) } else { best }
        })
        .0;
    Ok(ImpulseResponse {
        rate,
        samples,
        peak,
    })
}

/// The gate's lengths in samples at `rate`: (pre, post, left taper, right taper).
pub(crate) fn gate_lengths(rate: u32) -> (usize, usize, usize, usize) {
    let fs = f64::from(rate);
    let samples = |seconds: f64| (seconds * fs).round() as usize;
    (
        samples(GATE_PRE_SECONDS),
        samples(GATE_POST_SECONDS),
        samples(GATE_LEFT_TAPER_SECONDS),
        samples(GATE_RIGHT_TAPER_SECONDS),
    )
}

/// The weight of sample `i` of a `total`-sample window with half-Hann tapers over the first
/// `left` and the last `right` samples.
pub(crate) fn taper_weight(i: usize, total: usize, left: usize, right: usize) -> f64 {
    let mut w = 1.0;
    if i < left {
        w = 0.5 * (1.0 - (PI * i as f64 / left as f64).cos());
    }
    let from_end = total - 1 - i;
    if from_end < right {
        w = 0.5 * (1.0 - (PI * from_end as f64 / right as f64).cos());
    }
    w
}

/// −5 ms to +100 ms around the peak (read circularly), half-Hann tapers of 1 ms (left) and
/// 20 ms (right). The result's `peak` is the peak's index in the gated buffer.
pub fn gate(ir: &ImpulseResponse) -> ImpulseResponse {
    let (pre, post, left, right) = gate_lengths(ir.rate);
    let len = ir.samples.len();
    if len == 0 {
        return ir.clone();
    }
    let total = pre + post + 1;
    let samples = (0..total)
        .map(|i| {
            let src = (ir.peak + len * (pre / len + 1) + i - pre) % len;
            (f64::from(ir.samples[src]) * taper_weight(i, total, left, right)) as f32
        })
        .collect();
    ImpulseResponse {
        rate: ir.rate,
        samples,
        peak: pre,
    }
}

/// Seconds by which harmonic `k`'s impulse response arrives before the linear one in a
/// deconvolved sweep of `sweep_seconds` (Farina): `T·ln(k)/ln(f2/f1)`.
pub fn harmonic_delay(k: u32, sweep_seconds: f64) -> f64 {
    sweep_seconds * f64::from(k).ln() / (SWEEP_F2 / SWEEP_F1).ln()
}

/// The part of the circular `raw` IR that holds the harmonics and a quiet stretch after the
/// linear IR: from `peak − harmonic_delay(9) − 50 ms` to `peak + 300 ms`, read circularly,
/// without a taper. The result's `peak` is the peak's index in it.
pub fn long_ir(raw: &ImpulseResponse, sweep_seconds: f64) -> ImpulseResponse {
    let len = raw.samples.len();
    if len == 0 {
        return raw.clone();
    }
    let fs = f64::from(raw.rate);
    let lead =
        harmonic_delay(crate::distortion::MAX_HARMONIC, sweep_seconds) + LONG_IR_LEAD_SECONDS;
    let post = ((LONG_IR_POST_SECONDS * fs).round() as usize).min(len - 1);
    // Never longer than the circular buffer itself (`as` saturates a bad `lead` to 0 or max).
    let pre = ((lead * fs).round() as usize).min(len - 1 - post);
    let first = (raw.peak % len + len - pre) % len;
    let samples = (0..=pre + post)
        .map(|i| raw.samples[(first + i) % len])
        .collect();
    ImpulseResponse {
        rate: raw.rate,
        samples,
        peak: pre,
    }
}

/// The mean power and complex value over bins `[a, b]` (fractional bin
/// positions), with the spectrum read as linear between bins. Each bin
/// interval is integrated on its own, so quiet cells keep their precision.
fn cell_mean(bins: &[Complex<f64>], a: f64, b: f64) -> (f64, Complex<f64>) {
    let mut power = 0.0;
    let mut sum = Complex::new(0.0, 0.0);
    let end = (b.ceil() as usize).min(bins.len() - 1);
    for k in a.floor() as usize..end {
        let left = a.max(k as f64);
        let right = b.min((k + 1) as f64);
        let width = right - left;
        let t = (left + right) / 2.0 - k as f64;
        let (w0, w1) = (width * (1.0 - t), width * t);
        power += bins[k].norm_sqr() * w0 + bins[k + 1].norm_sqr() * w1;
        sum += bins[k] * w0 + bins[k + 1] * w1;
    }
    (power / (b - a), sum / (b - a))
}

/// The spectrum of `ir` zero-padded to at least 0.25 s, with the IR rotated so its peak is at
/// time zero (the pre-peak part wraps to the end of the FFT buffer), and the bins per Hz.
pub(crate) fn ir_spectrum(ir: &ImpulseResponse) -> (Vec<Complex<f64>>, f64) {
    let fs = f64::from(ir.rate);
    let n = ((RESPONSE_MIN_SECONDS * fs).ceil() as usize)
        .max(ir.samples.len())
        .next_power_of_two();
    let fft = RealFftPlanner::<f64>::new().plan_fft_forward(n);
    let mut input = fft.make_input_vec();
    let peak = ir.peak.min(ir.samples.len());
    for (i, &v) in ir.samples[peak..].iter().enumerate() {
        input[i] = f64::from(v);
    }
    for (i, &v) in ir.samples[..peak].iter().enumerate() {
        input[n - peak + i] = f64::from(v);
    }
    let mut bins = fft.make_output_vec();
    fft.process(&mut input, &mut bins)
        .expect("buffer sizes match the plan");
    (bins, n as f64 / fs)
}

/// The mean power and complex value of `bins` over the 1/96 octave cell around `f` Hz.
///
/// Each grid point is the mean over its own cell of the spectrum, read as linear between bins,
/// so detail finer than the grid is averaged rather than sampled (sampling aliases it). Within
/// a single bin interval the mean is its midpoint value. A cell is cut at Nyquist, so it
/// averages only what the FFT covers.
pub(crate) fn cell_at(bins: &[Complex<f64>], bin_per_hz: f64, f: f64) -> (f64, Complex<f64>) {
    let half_cell = (0.5 / GRID_POINTS_PER_OCTAVE).exp2();
    let a = f / half_cell * bin_per_hz;
    let b = (f * half_cell * bin_per_hz).min((bins.len() - 1) as f64);
    cell_mean(bins, a, b)
}

/// Magnitude (dB) and phase (degrees) on `log_grid(rate)`. The IR is rotated so its peak is at
/// time zero (the pre-peak part wraps to the end of the zero-padded FFT buffer), so the phase is
/// referenced to the peak.
pub fn response_from_ir(ir: &ImpulseResponse) -> Response {
    let (bins, bin_per_hz) = ir_spectrum(ir);
    let grid = log_grid(ir.rate);
    let mut db = Vec::with_capacity(grid.len());
    let mut phase = Vec::with_capacity(grid.len());
    for &f in &grid {
        let (power, mean) = cell_at(&bins, bin_per_hz, f);
        db.push(10.0 * power.max(1e-30).log10());
        phase.push(mean.arg().to_degrees());
    }
    Response {
        freq: grid,
        db,
        phase: Some(phase),
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::f64::consts::PI;

    fn peak_coeffs(f0: f64, gain_db: f64, q: f64, rate: u32) -> ([f64; 3], [f64; 3]) {
        let a = 10f64.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * f0 / f64::from(rate);
        let alpha = w0.sin() / (2.0 * q);
        let c = w0.cos();
        let a0 = 1.0 + alpha / a;
        (
            [
                (1.0 + alpha * a) / a0,
                -2.0 * c / a0,
                (1.0 - alpha * a) / a0,
            ],
            [1.0, -2.0 * c / a0, (1.0 - alpha / a) / a0],
        )
    }

    /// RBJ cookbook peaking EQ as a sample-by-sample filter (direct form I, f64 state).
    pub(crate) fn biquad_peak(f0: f64, gain_db: f64, q: f64, rate: u32) -> impl FnMut(f32) -> f32 {
        let (b, a) = peak_coeffs(f0, gain_db, q, rate);
        let (mut x1, mut x2, mut y1, mut y2) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
        move |x| {
            let x = f64::from(x);
            let y = b[0] * x + b[1] * x1 + b[2] * x2 - a[1] * y1 - a[2] * y2;
            (x2, x1, y2, y1) = (x1, x, y1, y);
            y as f32
        }
    }

    /// Analytic magnitude in dB of [`biquad_peak`] at `f`.
    pub(crate) fn biquad_peak_db(f: f64, f0: f64, gain_db: f64, q: f64, rate: u32) -> f64 {
        let (b, a) = peak_coeffs(f0, gain_db, q, rate);
        let w = 2.0 * PI * f / f64::from(rate);
        let eval = |c: [f64; 3]| {
            let re = c[0] + c[1] * w.cos() + c[2] * (2.0 * w).cos();
            let im = -c[1] * w.sin() - c[2] * (2.0 * w).sin();
            re.hypot(im)
        };
        20.0 * (eval(b) / eval(a)).log10()
    }

    /// Deterministic uniform white noise with unit RMS.
    pub(crate) fn white_noise(len: usize, seed: u64) -> Vec<f32> {
        let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..len)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                let u = (s >> 11) as f64 / (1u64 << 53) as f64;
                ((2.0 * u - 1.0) * 3f64.sqrt()) as f32
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;
    use crate::signal::sweep_stimulus;
    use std::f64::consts::PI;

    /// Positions of rising zero crossings, linearly interpolated.
    fn rising_crossings(x: &[f32]) -> Vec<f64> {
        (0..x.len() - 1)
            .filter(|&i| x[i] <= 0.0 && x[i + 1] > 0.0)
            .map(|i| i as f64 + f64::from(-x[i]) / f64::from(x[i + 1] - x[i]))
            .collect()
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn resample_identity_and_tone() {
        let rate = 48_000.0;
        let x: Vec<f32> = (0..96_000)
            .map(|i| (2.0 * PI * 1000.0 * i as f64 / rate).sin() as f32 * 0.7)
            .collect();

        let same = resample(&x, 1.0);
        assert_eq!(same.len(), x.len());
        for i in 32..x.len() - 32 {
            assert!(
                (same[i] - x[i]).abs() < 1e-6,
                "index {i}: {} vs {}",
                same[i],
                x[i]
            );
        }

        let y = resample(&x, 1.0003);
        assert_eq!(
            y.len(),
            ((x.len() - 1) as f64 / 1.0003).floor() as usize + 1
        );
        let c = rising_crossings(&y[64..y.len() - 64]);
        let (first, last) = (c[0], c[c.len() - 1]);
        let freq = (c.len() - 1) as f64 * rate / (last - first);
        assert!((freq - 1000.3).abs() < 0.05, "{freq} Hz");
    }

    /// One simulated capture: stimulus → optional peak filter → foreign clock (`drift`) →
    /// `delay` samples of silence → white noise at −60 dB re sweep RMS (plus an optional 4 Hz
    /// rumble at `rumble_db` re sweep RMS) → deconvolve → gate → response.
    struct Scenario {
        rate: u32,
        length: SweepLength,
        drift: f64,
        delay: usize,
        filter: bool,
        rumble_db: Option<f64>,
    }

    impl Scenario {
        fn new(rate: u32) -> Self {
            Scenario {
                rate,
                length: SweepLength::Normal,
                drift: 1.0,
                delay: (0.37 * f64::from(rate)).round() as usize,
                filter: true,
                rumble_db: None,
            }
        }

        fn run(&self) -> Response {
            let rate = self.rate;
            let stim = sweep_stimulus(self.length, rate);
            let mut played = stim.samples.clone();
            if self.filter {
                let mut bq = biquad_peak(1000.0, 6.0, 1.0, rate);
                played.iter_mut().for_each(|v| *v = bq(*v));
            }
            let captured = if self.drift == 1.0 {
                played
            } else {
                resample(&played, 1.0 / self.drift)
            };
            let mut rec = vec![0.0f32; self.delay];
            rec.extend_from_slice(&captured);
            let sweep = &stim.samples[stim.sweep_start..stim.sweep_start + stim.sweep_len];
            let sweep_rms = (sweep.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>()
                / sweep.len() as f64)
                .sqrt();
            let noise_gain = sweep_rms * 10f64.powf(-60.0 / 20.0);
            let rumble_amp = self
                .rumble_db
                .map_or(0.0, |db| sweep_rms * 10f64.powf(db / 20.0) * 2f64.sqrt());
            let noise = white_noise(rec.len(), 7);
            for (i, (r, n)) in rec.iter_mut().zip(noise).enumerate() {
                let t = i as f64 / f64::from(rate);
                let rumble = rumble_amp * (2.0 * PI * 4.0 * t + 0.3).sin();
                *r += (f64::from(n) * noise_gain + rumble) as f32;
            }
            let marker_a = self.delay as f64 + stim.marker_a as f64 * self.drift;
            let ir = deconvolve(&rec, marker_a, self.drift, self.length, rate).unwrap();
            response_from_ir(&gate(&ir))
        }
    }

    fn measure(rate: u32, drift: f64, delay: usize, filter: bool) -> Response {
        Scenario {
            drift,
            delay,
            filter,
            ..Scenario::new(rate)
        }
        .run()
    }

    /// Largest |measured − analytic| (both aligned at 200 Hz) over `range`.
    fn filter_error_in(
        resp: &Response,
        rate: u32,
        range: std::ops::RangeInclusive<f64>,
    ) -> (f64, f64) {
        let resp = resp.aligned(200.0);
        let reference = biquad_peak_db(200.0, 1000.0, 6.0, 1.0, rate);
        let mut worst = (0.0f64, 0.0f64);
        for (f, d) in resp.freq.iter().zip(&resp.db) {
            if range.contains(f) {
                let want = biquad_peak_db(*f, 1000.0, 6.0, 1.0, rate) - reference;
                let err = (d - want).abs();
                assert!(!err.is_nan(), "no data at {f} Hz");
                if err > worst.0 {
                    worst = (err, *f);
                }
            }
        }
        worst
    }

    /// Largest |measured − analytic| (both aligned at 200 Hz) over 30 Hz–16 kHz.
    fn max_filter_error(resp: &Response, rate: u32) -> f64 {
        let (err, f) = filter_error_in(resp, rate, 30.0..=16_000.0);
        let (low, low_f) = filter_error_in(resp, rate, 20.0..=30.0);
        eprintln!(
            "rate {rate}: max error {err:.4} dB at {f:.1} Hz (20–30 Hz: {low:.4} dB at {low_f:.1} Hz)"
        );
        err
    }

    // Bugs caught: 1, False positives: 0
    #[test]
    fn recovers_known_filter() {
        let rate = 48_000;
        let resp = measure(
            rate,
            1.0003,
            (0.37 * f64::from(rate)).round() as usize,
            true,
        );
        let err = max_filter_error(&resp, rate);
        assert!(err <= 0.3, "max error {err} dB");
    }

    // Bugs caught: 1, False positives: 0
    #[test]
    fn recovers_response_at_44k_and_96k() {
        let errors: Vec<(u32, f64)> = [44_100, 96_000]
            .into_iter()
            .map(|rate| {
                let resp = measure(rate, 1.0, (0.37 * f64::from(rate)).round() as usize, true);
                assert_eq!(resp.freq, crate::response::log_grid(rate));
                (rate, max_filter_error(&resp, rate))
            })
            .collect();
        for (rate, err) in errors {
            assert!(err <= 0.3, "rate {rate}: max error {err} dB");
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn phase_referenced_to_peak() {
        let resp = measure(48_000, 1.0, 1234, false);
        let phase = resp.phase.as_ref().expect("phase");
        let mut worst = 0.0f64;
        for (f, p) in resp.freq.iter().zip(phase) {
            if (30.0..=10_000.0).contains(f) {
                assert!(p.abs() < 10.0, "phase {p}° at {f} Hz");
                worst = worst.max(p.abs());
            }
        }
        eprintln!("max |phase| {worst:.4}°");
    }

    /// Magnitude alone barely notices 300 ppm of drift (it deconvolves to ~1.8 ms of dispersion,
    /// which the gate keeps); phase referenced to the peak does (≈180° error uncorrected).
    // Bugs caught: 0, False positives: 0
    #[test]
    fn drift_correction_keeps_phase() {
        let resp = measure(48_000, 1.0003, 17_760, false);
        let phase = resp.phase.as_ref().expect("phase");
        for (f, p) in resp.freq.iter().zip(phase) {
            if (30.0..=10_000.0).contains(f) {
                assert!(p.abs() < 10.0, "phase {p}° at {f} Hz");
            }
        }
    }

    // Detail finer than the 96 PPO grid (an echo 50 ms late combs the response every 20 Hz)
    // must be averaged into each grid point, not sampled: sampling aliases the comb into
    // a jagged trace that smoothing on the grid cannot repair.
    // Bugs caught: 1, False positives: 0
    #[test]
    fn grid_points_average_fine_detail() {
        let rate = 48_000;
        let mut samples = vec![0.0f32; 6000];
        samples[0] = 1.0;
        samples[2400] = 0.5;
        let resp = response_from_ir(&ImpulseResponse {
            rate,
            samples,
            peak: 0,
        });
        // The power mean of |1 + 0.5 e^-jwt| is 1 + 0.25.
        let mean = 10.0 * 1.25f64.log10();
        for (f, d) in resp.freq.iter().zip(&resp.db) {
            if (8000.0..=20_000.0).contains(f) {
                assert!((d - mean).abs() < 0.5, "{f} Hz: {d} dB, want {mean}");
            }
        }
    }

    // At 44.1 kHz the last grid cell reaches past Nyquist: only the covered
    // part may count, so a unit impulse reads 0 dB right to the top.
    // Bugs caught: 1, False positives: 0
    #[test]
    fn unit_impulse_is_flat_to_the_last_point() {
        for rate in [44_100, 48_000] {
            let mut samples = vec![0.0f32; 4800];
            samples[0] = 1.0;
            let resp = response_from_ir(&ImpulseResponse {
                rate,
                samples,
                peak: 0,
            });
            for (f, d) in resp.freq.iter().zip(&resp.db) {
                assert!(d.abs() < 0.01, "{rate}: {f} Hz reads {d} dB");
            }
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn identity_reads_0db() {
        for length in [SweepLength::Short, SweepLength::Long] {
            for rate in [44_100, 96_000] {
                let resp = Scenario {
                    length,
                    filter: false,
                    ..Scenario::new(rate)
                }
                .run();
                let level = resp.level_at(1000.0);
                eprintln!("{length:?} @ {rate}: {level:+.4} dB at 1 kHz");
                assert!(level.abs() <= 0.1, "{length:?} @ {rate}: {level} dB");
            }
        }
    }

    // Bugs caught: 0, False positives: 1
    #[test]
    fn rumble_does_not_leak() {
        // Measured tolerance: −25 dB → 0.27 dB; stronger infrasonic rejection in ε would
        // re-add the gate-truncation error (zero-phase band limit cut by the −5 ms gate).
        let rate = 48_000;
        let resp = Scenario {
            drift: 1.0003,
            rumble_db: Some(-30.0),
            ..Scenario::new(rate)
        }
        .run();
        let (err, f) = filter_error_in(&resp, rate, 20.0..=100.0);
        eprintln!("rumble: max error 20–100 Hz {err:.4} dB at {f:.1} Hz");
        assert!(err <= 0.3, "max error {err} dB at {f} Hz");
    }

    // Bugs caught: 0, False positives: 1
    #[test]
    fn segment_ends_50ms_before_marker_b() {
        let rate = 48_000;
        let stim = sweep_stimulus(SweepLength::Normal, rate);
        let marker_a = stim.marker_a as f64;
        // Integer path: the segment's last sample is 50 ms before marker B.
        let end = stim.marker_b - (0.05 * f64::from(rate)) as usize;
        let ok = deconvolve(
            &stim.samples[..end],
            marker_a,
            1.0,
            SweepLength::Normal,
            rate,
        );
        assert!(ok.is_ok());
        let short = deconvolve(
            &stim.samples[..end - 1],
            marker_a,
            1.0,
            SweepLength::Normal,
            rate,
        );
        assert!(short.is_err());
        // Resampling path, on a recording from a clock 300 ppm fast.
        let d = 1.0003;
        let stretched = resample(&stim.samples, 1.0 / d);
        let last = (end - 1) as f64 * d;
        let len = last.ceil() as usize + 1;
        let ok = deconvolve(
            &stretched[..len],
            marker_a * d,
            d,
            SweepLength::Normal,
            rate,
        );
        assert!(ok.is_ok());
        let short = deconvolve(
            &stretched[..len - 1],
            marker_a * d,
            d,
            SweepLength::Normal,
            rate,
        );
        assert!(short.is_err());
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn resampler_flat_to_20k_at_44k1() {
        let rate = 44_100.0;
        let x: Vec<f32> = (0..88_200)
            .map(|i| (2.0 * PI * 20_000.0 * i as f64 / rate).sin() as f32 * 0.5)
            .collect();
        let input = rms(&x[1000..x.len() - 1000]);
        let y = resample(&x, 1.0003);
        let db = 20.0 * (rms(&y[1000..y.len() - 1000]) / input).log10();
        eprintln!("20 kHz @ 44.1 kHz resampled by 1.0003: {db:+.4} dB");
        assert!(db.abs() <= 0.1, "{db} dB");
        // Every fixed fractional delay, at the drift-correction cutoff.
        for frac in [0.125, 0.25, 0.375, 0.5, 0.75] {
            let y: Vec<f32> = (1000..x.len() - 1000)
                .map(|i| interpolate(&x, i as f64 + frac, 1.0 / 1.0003) as f32)
                .collect();
            let db = 20.0 * (rms(&y) / input).log10();
            eprintln!("  frac {frac}: {db:+.4} dB");
            assert!(db.abs() <= 0.1, "frac {frac}: {db} dB");
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn too_short_recording_errors() {
        let rate = 48_000;
        let stim = sweep_stimulus(SweepLength::Normal, rate);
        let rec = &stim.samples[..stim.marker_a + rate as usize];
        assert!(deconvolve(rec, stim.marker_a as f64, 1.0, SweepLength::Normal, rate).is_err());
        assert!(deconvolve(rec, stim.marker_a as f64, 1.0003, SweepLength::Normal, rate).is_err());
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn rates_below_44k1_are_refused() {
        let rate = 32_000;
        let stim = sweep_stimulus(SweepLength::Short, rate);
        let err = deconvolve(
            &stim.samples,
            stim.marker_a as f64,
            1.0,
            SweepLength::Short,
            rate,
        )
        .unwrap_err();
        assert!(err.to_string().contains("too slow"), "{err}");
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn harmonic_delay_matches_farina() {
        let want = 6.0 * 2f64.ln() / 2000f64.ln();
        let got = harmonic_delay(2, 6.0);
        assert!((got - want).abs() < 1e-12, "{got} s, want {want} s");
        assert_eq!(harmonic_delay(1, 6.0), 0.0);
    }

    // The FR is still computed from the gate of the full circular IR; the long IR the
    // measurement stores must gate to exactly the same samples, so a reloaded entry's
    // response matches.
    // Bugs caught: 0, False positives: 0
    #[test]
    fn long_ir_keeps_the_gated_response() {
        let rate = 48_000;
        let length = SweepLength::Short;
        let stim = sweep_stimulus(length, rate);
        let mut bq = biquad_peak(1000.0, 6.0, 1.0, rate);
        let rec: Vec<f32> = stim.samples.iter().map(|&v| bq(v)).collect();
        let raw = deconvolve(&rec, stim.marker_a as f64, 1.0, length, rate).unwrap();
        let long = long_ir(&raw, length.seconds());
        let fs = f64::from(rate);
        let pre = ((harmonic_delay(9, length.seconds()) + 0.05) * fs).round() as usize;
        assert_eq!(long.peak, pre);
        assert_eq!(long.samples.len(), pre + (0.3 * fs).round() as usize + 1);
        assert_eq!(long.samples[long.peak], raw.samples[raw.peak]);
        assert_eq!(gate(&long), gate(&raw));
        assert_eq!(
            response_from_ir(&gate(&long)),
            response_from_ir(&gate(&raw))
        );
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn gate_window_and_peak() {
        let rate = 48_000;
        let mut samples = vec![1.0f32; 20_000];
        samples[10_000] = 5.0;
        let g = gate(&ImpulseResponse {
            rate,
            samples,
            peak: 10_000,
        });
        assert_eq!(g.peak, 240);
        assert_eq!(g.samples.len(), 240 + 4800 + 1);
        assert_eq!(g.samples[g.peak], 5.0);
        assert!(g.samples[0].abs() < 1e-6);
        assert!(g.samples[g.samples.len() - 1].abs() < 1e-6);
        assert!(
            (g.samples[240 - 10] - 1.0).abs() < 1e-6,
            "flat after the 1 ms taper"
        );
        assert!(
            (g.samples[240 + 3800] - 1.0).abs() < 1e-6,
            "flat before the 20 ms taper"
        );
        assert!(g.samples[240 + 4300] > 0.1 && g.samples[240 + 4300] < 0.9);
    }
}
