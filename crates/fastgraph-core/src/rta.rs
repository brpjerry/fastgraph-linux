//! RTA fallback: pink-noise stimulus and a Welch-averaged magnitude analyser.

use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};
use std::f64::consts::PI;
use std::sync::Arc;

use crate::response::{GRID_POINTS_PER_OCTAVE, Response, log_grid};
use crate::signal::pink_noise;

/// Samples in one period of the noise file.
pub const NOISE_PERIOD: usize = 1 << 22;
const NOISE_SEED: u64 = 0xFA57;
const NOISE_PEAK: f32 = 0.5;
const FRAME: usize = 65_536;
const HOP: usize = FRAME / 2;
const MIN_FRAMES: usize = 4;
const BAND_LO: f64 = 20.0;
const BAND_HI: f64 = 20_000.0;

/// One period of band-limited pink noise, peak 0.5.
pub fn noise_stimulus(rate: u32) -> Vec<f32> {
    let mut x = pink_noise(NOISE_PERIOD, rate, NOISE_SEED);
    let peak = x.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
    if peak > 0.0 {
        let g = NOISE_PEAK / peak;
        x.iter_mut().for_each(|v| *v *= g);
    }
    x
}

/// e.g. `fastgraph-pink-48k.wav`, `fastgraph-pink-44k1.wav`.
pub fn noise_file_name(rate: u32) -> String {
    let tag = if rate.is_multiple_of(1000) {
        format!("{}k", rate / 1000)
    } else if rate.is_multiple_of(100) {
        format!("{}k{}", rate / 1000, rate % 1000 / 100)
    } else {
        rate.to_string()
    };
    format!("fastgraph-pink-{tag}.wav")
}

/// Bins `k_lo..k_end` whose frequencies lie in the grid cell `[f*2^-1/192, f*2^+1/192)` around `f`.
/// Adjacent 96 ppo grid points have touching cells, so every bin belongs to one point.
fn cell(f: f64, df: f64) -> (usize, usize) {
    let half_step = (0.5 / GRID_POINTS_PER_OCTAVE).exp2();
    (
        (f / half_step / df).ceil() as usize,
        (f * half_step / df).ceil() as usize,
    )
}

/// Streaming Welch estimator (Hann, 65 536 / 50 % hop) of the recorded noise spectrum.
pub struct Rta {
    rate: u32,
    fft: Arc<dyn RealToComplex<f64>>,
    window: Vec<f64>,
    pending: Vec<f32>,
    power: Vec<f64>,
    frames: usize,
    input: Vec<f64>,
    spectrum: Vec<Complex<f64>>,
    scratch: Vec<Complex<f64>>,
}

impl Rta {
    pub fn new(rate: u32) -> Self {
        let window = (0..FRAME)
            .map(|i| 0.5 - 0.5 * (2.0 * PI * i as f64 / FRAME as f64).cos())
            .collect();
        let fft = RealFftPlanner::<f64>::new().plan_fft_forward(FRAME);
        Self {
            rate,
            input: fft.make_input_vec(),
            spectrum: fft.make_output_vec(),
            scratch: fft.make_scratch_vec(),
            fft,
            window,
            pending: Vec::new(),
            power: vec![0.0; FRAME / 2 + 1],
            frames: 0,
        }
    }

    pub fn push(&mut self, block: &[f32]) {
        // A non-finite sample would poison the summed power for good.
        self.pending
            .extend(block.iter().map(|&x| if x.is_finite() { x } else { 0.0 }));
        let mut start = 0;
        while self.pending.len() - start >= FRAME {
            for (i, o) in self.input.iter_mut().enumerate() {
                *o = f64::from(self.pending[start + i]) * self.window[i];
            }
            self.fft
                .process_with_scratch(&mut self.input, &mut self.spectrum, &mut self.scratch)
                .expect("buffer sizes match the plan");
            for (p, c) in self.power.iter_mut().zip(&self.spectrum) {
                *p += c.norm_sqr();
            }
            self.frames += 1;
            start += HOP;
        }
        self.pending.drain(..start);
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    pub fn reset(&mut self) {
        self.pending.clear();
        self.power.iter_mut().for_each(|p| *p = 0.0);
        self.frames = 0;
    }

    /// Mean power divided by the analytic pink PSD (proportional to 1/f), in dB, on the
    /// log grid. `NaN` outside 20 Hz to 20 kHz. `None` before 4 frames, or with fewer than
    /// two finite levels (a silent input).
    pub fn response(&self) -> Option<Response> {
        if self.frames < MIN_FRAMES {
            return None;
        }
        let df = f64::from(self.rate) / FRAME as f64;
        let n = self.frames as f64;
        // Pink reference: multiplying power by f flattens it. Bins below 10 Hz are unused.
        let flat = |k: usize| self.power[k] / n * (k as f64 * df);
        let freq = log_grid(self.rate);
        let last = self.power.len() - 1;
        let db: Vec<f64> = freq
            .iter()
            .map(|&f| {
                if !(BAND_LO..=BAND_HI).contains(&f) {
                    return f64::NAN;
                }
                // Average every bin inside the grid cell; interpolate where the grid is denser.
                let (k_lo, k_end) = cell(f, df);
                let (k_lo, k_end) = (k_lo.max(1), k_end.min(last + 1));
                let p = if k_end > k_lo + 1 {
                    (k_lo..k_end).map(flat).sum::<f64>() / (k_end - k_lo) as f64
                } else {
                    let pos = f / df;
                    let lo = pos.floor() as usize;
                    if lo < 1 || lo >= last {
                        return f64::NAN;
                    }
                    let t = pos - lo as f64;
                    flat(lo) * (1.0 - t) + flat(lo + 1) * t
                };
                if p > 0.0 { 10.0 * p.log10() } else { f64::NAN }
            })
            .collect();
        // At least two finite levels.
        db.iter().filter(|d| d.is_finite()).nth(1)?;
        Some(Response {
            freq,
            db,
            phase: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deconv::resample;
    use crate::deconv::testutil::*;
    use crate::response::Smoothing;

    const RATE: u32 = 48_000;

    fn check(r: &Response, align: f64, want: impl Fn(f64) -> f64) -> f64 {
        check_tol(r, 1.0, align, want)
    }

    fn check_tol(r: &Response, tol: f64, align: f64, want: impl Fn(f64) -> f64) -> f64 {
        let s = r.smoothed(Smoothing::Oct12).aligned(align);
        let (mut worst, mut at) = (0.0f64, 0.0f64);
        for (&f, &d) in s.freq.iter().zip(&s.db) {
            if (50.0..=16_000.0).contains(&f) {
                assert!(!d.is_nan(), "NaN at {f}");
                let e = (d - want(f)).abs();
                if e > worst {
                    worst = e;
                    at = f;
                }
            }
        }
        eprintln!("max error {worst:.3} dB at {at:.0} Hz");
        assert!(worst <= tol, "max error {worst} dB");
        worst
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn flat_system_is_flat() {
        let x = noise_stimulus(RATE);
        let mut rta = Rta::new(RATE);
        assert!(rta.response().is_none());
        for b in x[..40 * RATE as usize].chunks(4096) {
            rta.push(b);
        }
        let r = rta.response().unwrap();
        assert!(r.phase.is_none());
        check(&r, 1000.0, |_| 0.0);
        // nothing reported outside the stimulus band
        for (&f, &d) in r.freq.iter().zip(&r.db) {
            if !(20.0..=20_000.0).contains(&f) {
                assert!(d.is_nan(), "{f} Hz has data");
            }
        }
    }

    // Bugs caught: 0, False positives: 1
    #[test]
    fn recovers_known_filter() {
        let x = noise_stimulus(RATE);
        let mut filt = biquad_peak(1000.0, 6.0, 1.0, RATE);
        let y: Vec<f32> = x[..40 * RATE as usize].iter().map(|&v| filt(v)).collect();
        let mut rta = Rta::new(RATE);
        for b in y.chunks(3000) {
            rta.push(b);
        }
        let at200 = biquad_peak_db(200.0, 1000.0, 6.0, 1.0, RATE);
        check(&rta.response().unwrap(), 200.0, |f| {
            biquad_peak_db(f, 1000.0, 6.0, 1.0, RATE) - at200
        });
    }

    // Bugs caught: 0, False positives: 1
    #[test]
    fn starts_mid_period() {
        let x = noise_stimulus(RATE);
        let start = 1_234_567;
        let y = resample(&x[start..start + 40 * RATE as usize + 100], 1.0003);
        let mut rta = Rta::new(RATE);
        for b in y.chunks(5000) {
            rta.push(b);
        }
        check(&rta.response().unwrap(), 1000.0, |_| 0.0);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn one_period_is_tight() {
        let x = noise_stimulus(RATE);
        let mut rta = Rta::new(RATE);
        rta.push(&x);
        rta.push(&x[..FRAME - HOP]);
        assert_eq!(rta.frames(), NOISE_PERIOD / HOP);
        check_tol(&rta.response().unwrap(), 0.2, 1000.0, |_| 0.0);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn cells_tile_the_grid() {
        let df = f64::from(RATE) / FRAME as f64;
        let grid = log_grid(RATE);
        let mut used = std::collections::HashMap::new();
        let mut checked = 0;
        for w in grid.windows(2) {
            if w[0] < 200.0 || w[1] > 19_000.0 {
                continue;
            }
            // contiguous: the next cell starts where this one ends
            assert_eq!(
                cell(w[0], df).1,
                cell(w[1], df).0,
                "gap/overlap at {}",
                w[0]
            );
            let (lo, end) = cell(w[0], df);
            for k in lo..end {
                assert!(used.insert(k, w[0]).is_none(), "bin {k} used twice");
            }
            checked += 1;
        }
        assert!(checked > 500);
        // every bin between the first and last used cell is covered
        let (min, max) = (*used.keys().min().unwrap(), *used.keys().max().unwrap());
        assert_eq!(used.len(), max - min + 1);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn chunking_invariant() {
        let x = noise_stimulus(RATE);
        let x = &x[..10 * RATE as usize];
        let mut a = Rta::new(RATE);
        a.push(x);
        let mut b = Rta::new(RATE);
        for c in x.chunks(997) {
            b.push(c);
        }
        assert_eq!(a.frames(), b.frames());
        let (ra, rb) = (a.response().unwrap(), b.response().unwrap());
        // NaN points compare unequal, so compare bit patterns.
        let bits = |r: &Response| r.db.iter().map(|d| d.to_bits()).collect::<Vec<_>>();
        assert_eq!(bits(&ra), bits(&rb));
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn silence_has_no_response_and_non_finite_samples_are_dropped() {
        let mut rta = Rta::new(RATE);
        rta.push(&vec![0.0; 8 * FRAME]);
        assert!(rta.frames() >= MIN_FRAMES);
        assert!(rta.response().is_none(), "silence gives no average");
        let x = noise_stimulus(RATE);
        let mut rta = Rta::new(RATE);
        let mut block = x[..10 * RATE as usize].to_vec();
        block[1000] = f32::NAN;
        block[2000] = f32::INFINITY;
        rta.push(&block);
        let r = rta.response().unwrap();
        let finite = r.db.iter().filter(|d| d.is_finite()).count();
        assert!(finite > r.db.len() / 2, "only {finite} finite levels");
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn reset_and_names() {
        let mut rta = Rta::new(RATE);
        rta.push(&white_noise(FRAME * 5, 1));
        assert!(rta.frames() >= 4 && rta.response().is_some());
        rta.reset();
        assert_eq!(rta.frames(), 0);
        assert!(rta.response().is_none());
        assert_eq!(noise_file_name(48_000), "fastgraph-pink-48k.wav");
        assert_eq!(noise_file_name(44_100), "fastgraph-pink-44k1.wav");
    }
}
