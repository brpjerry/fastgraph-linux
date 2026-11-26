//! Harmonic distortion and noise floor from a deconvolved sweep (Farina's method).

use crate::deconv::{
    ImpulseResponse, SWEEP_F2, cell_at, gate_lengths, harmonic_delay, ir_spectrum,
    response_from_ir, taper_weight,
};
use crate::response::{Response, log_grid};

/// The longest sweep read; corrupt metadata beyond it is refused.
const MAX_SWEEP_SECONDS: f64 = 60.0;
/// The IR rates analysed; a stored WAV's header beyond them is refused, bounding the FFT size.
const ANALYSIS_RATES: std::ops::RangeInclusive<u32> = 8_000..=384_000;
/// The highest harmonic extracted.
pub const MAX_HARMONIC: u32 = 9;
/// A harmonic's window ends this fraction of the gap to harmonic k+1, which arrives earlier.
const HARMONIC_SPACING_FRACTION: f64 = 0.9;
/// The noise window starts this long after the peak: past the gate's end (+100 ms) and the
/// linear IR's decay, and (with the gate's 105 ms length) before the long IR's end (+300 ms).
const NOISE_START_SECONDS: f64 = 0.15;

/// The distortion analysis of one sweep measurement, on the response grid.
#[derive(Clone, Debug, PartialEq)]
pub struct Distortion {
    /// The gated linear response, as the FR shows it, without phase.
    pub fundamental: Response,
    /// Harmonics k = 2..=9 whose windows fit in the IR, plotted against the fundamental
    /// frequency.
    pub harmonics: Vec<(u32, Response)>,
    /// The noise floor, `None` when the IR has no quiet stretch long enough after the peak.
    pub noise: Option<Response>,
}

/// Harmonics, fundamental and noise floor from a long IR (see [`crate::deconv::long_ir`])
/// of a sweep of `sweep_seconds`. `None` if the IR lacks room before the peak for H2.
///
/// The IR is read as linear here (not circularly): windows must lie inside it. Harmonic k's
/// window runs from 5 ms before its arrival, `harmonic_delay(k)` before the peak, to
/// `min(100 ms, 0.9·(delay(k+1) − delay(k)))` after it, with the gate's half-Hann tapers.
/// That length is the spacing to harmonic k+1, which arrives *before* k; the gap after k (to
/// harmonic k−1, or the linear IR for k = 2) is always wider, so the window ends before the
/// next arrival. Harmonics whose window starts before the IR's first sample are dropped.
pub fn analyse(ir: &ImpulseResponse, sweep_seconds: f64) -> Option<Distortion> {
    let len = ir.samples.len();
    if ir.peak >= len
        || !ANALYSIS_RATES.contains(&ir.rate)
        || !(sweep_seconds > 0.0 && sweep_seconds <= MAX_SWEEP_SECONDS)
    {
        return None;
    }
    let fs = f64::from(ir.rate);
    let (pre, post, left, right) = gate_lengths(ir.rate);
    let top = SWEEP_F2.min(fs / 2.0);
    let mut harmonics = Vec::new();
    for k in 2..=MAX_HARMONIC {
        let delay = harmonic_delay(k, sweep_seconds);
        let lead = (delay * fs).round() as usize + pre;
        // Later harmonics arrive earlier still, so none of them fits either.
        let Some(start) = ir.peak.checked_sub(lead) else {
            break;
        };
        let spacing = harmonic_delay(k + 1, sweep_seconds) - delay;
        let k_post = ((HARMONIC_SPACING_FRACTION * spacing * fs).round() as usize).min(post);
        let window = cut(ir, start, pre, k_post, left, right.min(k_post));
        harmonics.push((k, harmonic_response(&window, k, top)));
    }
    if harmonics.is_empty() {
        return None;
    }
    // H2 fits, so there is room for the gate's 5 ms before the peak.
    let fundamental = without_phase(response_from_ir(&cut(
        ir,
        ir.peak - pre,
        pre,
        post,
        left,
        right,
    )));
    let noise_start = ir.peak + (NOISE_START_SECONDS * fs).round() as usize;
    let noise = (noise_start + pre + post < len).then(|| {
        without_phase(response_from_ir(&cut(
            ir,
            noise_start,
            pre,
            post,
            left,
            right,
        )))
    });
    Some(Distortion {
        fundamental,
        harmonics,
        noise,
    })
}

/// `pre + post + 1` samples of `ir` from `start` (zero past its end), with half-Hann tapers
/// over the first `left` and last `right` samples; `peak` is `pre`.
fn cut(
    ir: &ImpulseResponse,
    start: usize,
    pre: usize,
    post: usize,
    left: usize,
    right: usize,
) -> ImpulseResponse {
    let total = pre + post + 1;
    let samples = (0..total)
        .map(|i| {
            let v = ir.samples.get(start + i).copied().unwrap_or(0.0);
            (f64::from(v) * taper_weight(i, total, left, right)) as f32
        })
        .collect();
    ImpulseResponse {
        rate: ir.rate,
        samples,
        peak: pre,
    }
}

/// Harmonic k's level on `log_grid`: the 1/96 octave cell around `k·f` for each grid `f`,
/// `NaN` where `k·f` passes `top`.
///
/// No level correction is applied: the k-th harmonic of an exponential sweep is the same sweep
/// shifted in time (its frequency `k·f(t)` equals `f(t + delay(k))`), so at `k·f` its spectral
/// density matches the reference sweep's and the division leaves the harmonic's amplitude
/// relative to the sweep's.
fn harmonic_response(window: &ImpulseResponse, k: u32, top: f64) -> Response {
    let (bins, bin_per_hz) = ir_spectrum(window);
    let grid = log_grid(window.rate);
    let db = grid
        .iter()
        .map(|&f| {
            let at = f64::from(k) * f;
            if at > top {
                f64::NAN
            } else {
                10.0 * cell_at(&bins, bin_per_hz, at).0.max(1e-30).log10()
            }
        })
        .collect();
    Response {
        freq: grid,
        db,
        phase: None,
    }
}

fn without_phase(r: Response) -> Response {
    Response { phase: None, ..r }
}

impl Distortion {
    /// The power sum of the harmonics in dB; `NaN` where none has data.
    pub fn thd(&self) -> Response {
        let db = (0..self.fundamental.freq.len())
            .map(|i| {
                let powers: Vec<f64> = self
                    .harmonics
                    .iter()
                    .filter_map(|(_, h)| h.db.get(i).copied())
                    .filter(|d| !d.is_nan())
                    .map(|d| 10f64.powf(d / 10.0))
                    .collect();
                if powers.is_empty() {
                    f64::NAN
                } else {
                    10.0 * powers.iter().sum::<f64>().log10()
                }
            })
            .collect();
        Response {
            freq: self.fundamental.freq.clone(),
            db,
            phase: None,
        }
    }
}

/// `level` as a percentage of `fundamental` on the same grid: `100·10^((L−F)/20)` per point,
/// `NaN` where either is.
pub fn percent_of(level: &Response, fundamental: &Response) -> Vec<f64> {
    level
        .db
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let f = fundamental.db.get(i).copied().unwrap_or(f64::NAN);
            // NaN in either propagates.
            100.0 * 10f64.powf((l - f) / 20.0)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deconv::testutil::{biquad_peak, white_noise};
    use crate::deconv::{deconvolve, gate, long_ir, response_from_ir};
    use crate::signal::{SweepLength, sweep_stimulus};

    const RATE: u32 = 48_000;

    /// Deconvolves `rec`, a recording laid out like the stimulus (marker A where the
    /// stimulus has it, no drift).
    fn raw_ir(rec: &[f32], length: SweepLength) -> ImpulseResponse {
        let stim = sweep_stimulus(length, RATE);
        deconvolve(rec, stim.marker_a as f64, 1.0, length, RATE).unwrap()
    }

    /// The dB values of `r` at grid points in `lo..=hi`, none of them `NaN`.
    fn values_in(r: &Response, lo: f64, hi: f64) -> Vec<f64> {
        let v: Vec<f64> = r
            .freq
            .iter()
            .zip(&r.db)
            .filter(|(f, _)| (lo..=hi).contains(*f))
            .map(|(_, d)| *d)
            .collect();
        assert!(!v.is_empty(), "no points in {lo}..={hi} Hz");
        assert!(v.iter().all(|d| !d.is_nan()), "NaN in {lo}..={hi} Hz");
        v
    }

    fn median_in(r: &Response, lo: f64, hi: f64) -> f64 {
        let mut v = values_in(r, lo, hi);
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    }

    fn harmonic(d: &Distortion, k: u32) -> &Response {
        &d.harmonics
            .iter()
            .find(|(h, _)| *h == k)
            .expect("harmonic")
            .1
    }

    /// Mean of harmonic `k` relative to the fundamental over 300 Hz–2 kHz, in dB.
    fn relative_db(d: &Distortion, k: u32) -> f64 {
        let h = values_in(harmonic(d, k), 300.0, 2000.0);
        let f = values_in(&d.fundamental, 300.0, 2000.0);
        h.iter().zip(&f).map(|(h, f)| h - f).sum::<f64>() / h.len() as f64
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn harmonics_of_a_known_nonlinearity() {
        let length = SweepLength::Normal;
        let stim = sweep_stimulus(length, RATE);
        let amp = stim.samples[stim.sweep_start..stim.sweep_start + stim.sweep_len]
            .iter()
            .fold(0.0f64, |m, &v| m.max(f64::from(v).abs()));
        let (a, b) = (0.1, 0.03);
        let rec: Vec<f32> = stim
            .samples
            .iter()
            .map(|&v| {
                let x = f64::from(v);
                (x + a * x * x + b * x * x * x) as f32
            })
            .collect();
        let raw = raw_ir(&rec, length);
        let d = analyse(&long_ir(&raw, length.seconds()), length.seconds()).expect("distortion");
        let ks: Vec<u32> = d.harmonics.iter().map(|(k, _)| *k).collect();
        assert_eq!(ks, (2..=MAX_HARMONIC).collect::<Vec<_>>());

        // A sine of amplitude A through a·x² gives a second harmonic of a·A²/2, and through
        // b·x³ a third of b·A³/4.
        // The fundamental itself gains the cubic's 3bA²/4, and the ratios are against it.
        let fundamental_gain = 1.0 + 0.75 * b * amp * amp;
        let want2 = 20.0 * (a * amp / (2.0 * fundamental_gain)).log10();
        let want3 = 20.0 * (b * amp * amp / (4.0 * fundamental_gain)).log10();
        let (h2, h3) = (relative_db(&d, 2), relative_db(&d, 3));
        eprintln!(
            "A = {amp:.4}: H2 {h2:+.3} dB (want {want2:+.3}), H3 {h3:+.3} dB (want {want3:+.3})"
        );
        assert!((h2 - want2).abs() <= 0.1, "H2 {h2} dB, want {want2}");
        assert!((h3 - want3).abs() <= 0.1, "H3 {h3} dB, want {want3}");
        for k in 4..=MAX_HARMONIC {
            let hk = relative_db(&d, k);
            eprintln!("  H{k} {hk:+.1} dB");
            assert!(
                hk <= h3 - 30.0,
                "H{k} {hk} dB is not 30 dB below H3 ({h3} dB)"
            );
        }
    }

    // H_k is plotted against the fundamental: a filter after the nonlinearity that peaks at
    // 2 kHz boosts H2 where 2f = 2 kHz, i.e. at f = 1 kHz.
    // Bugs caught: 0, False positives: 0
    #[test]
    fn harmonic_k_is_read_at_k_times_f() {
        let length = SweepLength::Normal;
        let stim = sweep_stimulus(length, RATE);
        let mut bq = biquad_peak(2000.0, 12.0, 2.0, RATE);
        let rec: Vec<f32> = stim
            .samples
            .iter()
            .map(|&v| {
                let x = f64::from(v);
                bq((x + 0.1 * x * x) as f32)
            })
            .collect();
        let raw = raw_ir(&rec, length);
        let d = analyse(&long_ir(&raw, length.seconds()), length.seconds()).expect("distortion");
        let h2 = harmonic(&d, 2);
        let median = median_in(h2, 200.0, 8000.0);
        let at = |f: f64| h2.level_at(f) - median;
        eprintln!(
            "H2 vs median: 1 kHz {:+.1}, 2 kHz {:+.1}",
            at(1000.0),
            at(2000.0)
        );
        assert!(at(1000.0) > 6.0, "bump at 1 kHz: {}", at(1000.0));
        assert!(at(2000.0) < 3.0, "no bump at 2 kHz: {}", at(2000.0));

        for k in 2..=MAX_HARMONIC {
            let h = harmonic(&d, k);
            let limit = 20_000.0 / f64::from(k);
            for (f, db) in h.freq.iter().zip(&h.db) {
                if *f > limit * 1.001 {
                    assert!(db.is_nan(), "H{k} at {f} Hz is {db}");
                } else if *f < limit * 0.99 {
                    assert!(!db.is_nan(), "H{k} at {f} Hz is NaN");
                }
            }
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn noise_floor_tracks_added_noise() {
        let length = SweepLength::Normal;
        let stim = sweep_stimulus(length, RATE);
        let gain = 10f64.powf(-60.0 / 20.0);
        let noise: Vec<f32> = white_noise(stim.samples.len(), 11)
            .iter()
            .map(|&v| (f64::from(v) * gain) as f32)
            .collect();
        let mut bq = biquad_peak(1000.0, 6.0, 1.0, RATE);
        let rec: Vec<f32> = stim
            .samples
            .iter()
            .zip(&noise)
            .map(|(&x, &n)| bq(x) + n)
            .collect();
        let raw = raw_ir(&rec, length);
        let d = analyse(&long_ir(&raw, length.seconds()), length.seconds()).expect("distortion");
        let floor = d.noise.as_ref().expect("noise floor");

        // The level the same noise gives in the FR's own window.
        let mut alone = raw_ir(&noise, length);
        alone.peak = raw.peak;
        let reference = response_from_ir(&gate(&alone));

        let got = median_in(floor, 200.0, 10_000.0);
        let want = median_in(&reference, 200.0, 10_000.0);
        let fundamental = median_in(&d.fundamental, 200.0, 10_000.0);
        eprintln!(
            "noise floor {got:+.2} dB, reference {want:+.2} dB, fundamental {fundamental:+.2} dB"
        );
        assert!((got - want).abs() <= 2.0, "noise {got} dB, want {want} dB");
    }

    // Bugs caught: 1, False positives: 0
    #[test]
    fn short_ir_drops_harmonics_that_do_not_fit() {
        let fs = f64::from(RATE);
        let seconds = SweepLength::Long.seconds();
        let ir_with = |pre_seconds: f64| {
            let pre = (pre_seconds * fs).round() as usize;
            let mut samples = vec![0.0f32; pre + (0.3 * fs) as usize + 1];
            samples[pre] = 1.0;
            ImpulseResponse {
                rate: RATE,
                samples,
                peak: pre,
            }
        };
        // H2 arrives 1.0 s early for an 11 s sweep.
        assert!(analyse(&ir_with(0.3), seconds).is_none());

        // 2 s holds H2 (1.0 s) and H3 (1.59 s) but not H4 (2.006 s).
        let d = analyse(&ir_with(2.0), seconds).expect("H2 fits");
        let ks: Vec<u32> = d.harmonics.iter().map(|(k, _)| *k).collect();
        assert_eq!(ks, vec![2, 3]);
        assert!(d.noise.is_some());
        assert!(d.fundamental.level_at(1000.0).abs() < 0.01);

        // Too little after the peak for the noise window.
        let mut cut = ir_with(2.0);
        cut.samples.truncate(cut.peak + (0.2 * fs) as usize);
        let d = analyse(&cut, seconds).expect("H2 fits");
        assert!(d.noise.is_none());

        // Degenerate input never panics.
        let empty = ImpulseResponse {
            rate: RATE,
            samples: Vec::new(),
            peak: 0,
        };
        assert!(analyse(&empty, seconds).is_none());
        let past_end = ImpulseResponse {
            rate: RATE,
            samples: vec![0.0; 10],
            peak: 50,
        };
        assert!(analyse(&past_end, seconds).is_none());
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY, 1e300, 61.0] {
            assert!(analyse(&ir_with(2.0), bad).is_none(), "{bad}");
        }
        // A rate from a corrupt WAV header would size the FFT without bound.
        for rate in [0, 7_999, 384_001, u32::MAX] {
            let ir = ImpulseResponse {
                rate,
                samples: vec![0.0; 1000],
                peak: 500,
            };
            assert!(analyse(&ir, seconds).is_none(), "{rate}");
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn thd_sums_power_and_percent_is_relative() {
        let freq = vec![100.0, 200.0, 300.0];
        let resp = |db: Vec<f64>| Response {
            freq: freq.clone(),
            db,
            phase: None,
        };
        let d = Distortion {
            fundamental: resp(vec![0.0, 0.0, f64::NAN]),
            harmonics: vec![
                (2, resp(vec![-20.0, -20.0, f64::NAN])),
                (3, resp(vec![-20.0, f64::NAN, f64::NAN])),
            ],
            noise: None,
        };
        let thd = d.thd();
        assert_eq!(thd.freq, freq);
        assert!((thd.db[0] - (-20.0 + 10.0 * 2f64.log10())).abs() < 1e-9);
        assert!((thd.db[1] - -20.0).abs() < 1e-9);
        assert!(thd.db[2].is_nan());
        let pct = percent_of(&d.harmonics[0].1, &d.fundamental);
        assert!((pct[0] - 10.0).abs() < 1e-9);
        assert!(pct[2].is_nan());
    }
}
