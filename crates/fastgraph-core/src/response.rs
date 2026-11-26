//! Frequency-response data type: grid, interpolation, smoothing, compensation, averaging, alignment.

use serde::{Deserialize, Serialize};

/// A frequency response. `db` may contain `NaN` meaning "no data at this point".
#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    pub freq: Vec<f64>,
    pub db: Vec<f64>,
    /// Phase in degrees, when known.
    pub phase: Option<Vec<f64>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Smoothing {
    None,
    Oct48,
    #[default]
    Oct24,
    Oct12,
    Oct6,
    Oct3,
}

impl Smoothing {
    /// Octave fraction (the `n` of 1/n octave), `None` for no smoothing.
    pub fn fraction(self) -> Option<f64> {
        match self {
            Smoothing::None => None,
            Smoothing::Oct48 => Some(1.0 / 48.0),
            Smoothing::Oct24 => Some(1.0 / 24.0),
            Smoothing::Oct12 => Some(1.0 / 12.0),
            Smoothing::Oct6 => Some(1.0 / 6.0),
            Smoothing::Oct3 => Some(1.0 / 3.0),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Smoothing::None => "None",
            Smoothing::Oct48 => "1/48",
            Smoothing::Oct24 => "1/24",
            Smoothing::Oct12 => "1/12",
            Smoothing::Oct6 => "1/6",
            Smoothing::Oct3 => "1/3",
        }
    }
}

const GRID_START_HZ: f64 = 10.0;
pub const GRID_POINTS_PER_OCTAVE: f64 = 96.0;

/// 96 points per octave from 10 Hz to min(24 kHz, rate/2).
pub fn log_grid(rate: u32) -> Vec<f64> {
    let top = 24_000.0_f64.min(0.5 * f64::from(rate));
    (0..)
        .map(|k| GRID_START_HZ * (f64::from(k) / GRID_POINTS_PER_OCTAVE).exp2())
        .take_while(|f| *f <= top)
        .collect()
}

/// `deg` in (-180, 180]; non-finite input gives NaN.
fn wrap_degrees(deg: f64) -> f64 {
    if deg > -180.0 && deg <= 180.0 || deg.is_nan() {
        return deg;
    }
    let wrapped = (deg + 180.0).rem_euclid(360.0) - 180.0;
    if wrapped == -180.0 { 180.0 } else { wrapped }
}

/// Cell boundaries in log2 f for points at `x` (at least two): the midpoints between
/// neighbours, extended by half a step at each end. `x.len() + 1` values.
fn cell_edges(x: &[f64]) -> Vec<f64> {
    let n = x.len();
    let mut edges = Vec::with_capacity(n + 1);
    edges.push(x[0] - (x[1] - x[0]) / 2.0);
    edges.extend(x.windows(2).map(|w| (w[0] + w[1]) / 2.0));
    edges.push(x[n - 1] + (x[n - 1] - x[n - 2]) / 2.0);
    edges
}

impl Response {
    /// The level at a point's own frequency, else linear interpolation in log f; `NaN`
    /// outside the covered range or between a point and a `NaN` one.
    pub fn interp_db(&self, f: f64) -> f64 {
        let (Some(&first), Some(&last)) = (self.freq.first(), self.freq.last()) else {
            return f64::NAN;
        };
        if !(first..=last).contains(&f) {
            return f64::NAN;
        }
        // In range, so `hi` is a point, and one after the first unless `f` is the first.
        let hi = self.freq.partition_point(|x| *x < f);
        if self.freq[hi] == f {
            return self.db[hi];
        }
        let lo = hi - 1;
        let t = (f / self.freq[lo]).ln() / (self.freq[hi] / self.freq[lo]).ln();
        self.db[lo] + t * (self.db[hi] - self.db[lo])
    }

    /// Resample onto `grid` by interpolation; phase is dropped.
    pub fn resampled(&self, grid: &[f64]) -> Response {
        Response {
            freq: grid.to_vec(),
            db: grid.iter().map(|f| self.interp_db(*f)).collect(),
            phase: None,
        }
    }

    /// Power-domain average over `f * 2^(+-fraction/2)`, rectangular in log f, skipping `NaN`.
    pub fn smoothed(&self, s: Smoothing) -> Response {
        let Some(fraction) = s.fraction() else {
            return self.clone();
        };
        let n = self.freq.len();
        if n < 2 {
            return self.clone();
        }
        // Each point stands for the cell between the midpoints (in log f) to its neighbours.
        // The window takes the power-weighted share of every cell it overlaps, so an edge
        // landing on a point moves the result continuously instead of adding or dropping a
        // whole point, and uneven spacing (imported files) weighs each point by its width.
        let x: Vec<f64> = self.freq.iter().map(|f| f.log2()).collect();
        let edges = cell_edges(&x);
        let power: Vec<f64> = self.db.iter().map(|d| 10f64.powf(d / 10.0)).collect();
        // Running integrals of power and of covered width over the valid cells.
        let mut sum_p = vec![0.0];
        let mut sum_w = vec![0.0];
        for i in 0..n {
            let (p, w) = if power[i].is_nan() {
                (0.0, 0.0)
            } else {
                let w = edges[i + 1] - edges[i];
                (power[i] * w, w)
            };
            sum_p.push(sum_p[i] + p);
            sum_w.push(sum_w[i] + w);
        }
        // The integrals from the first edge to `at`.
        let upto = |at: f64| -> (f64, f64) {
            let at = at.clamp(edges[0], edges[n]);
            let i = edges.partition_point(|e| *e <= at).clamp(1, n) - 1;
            let part = at - edges[i];
            if power[i].is_nan() {
                (sum_p[i], sum_w[i])
            } else {
                (sum_p[i] + power[i] * part, sum_w[i] + part)
            }
        };
        let half = fraction / 2.0;
        let db = x
            .iter()
            .map(|c| {
                let (p0, w0) = upto(c - half);
                let (p1, w1) = upto(c + half);
                let w = w1 - w0;
                if w <= 1e-12 {
                    f64::NAN
                } else {
                    10.0 * ((p1 - p0) / w).log10()
                }
            })
            .collect();
        Response {
            freq: self.freq.clone(),
            db,
            phase: self.phase.clone(),
        }
    }

    /// The phase smoothed like [`Response::smoothed`] (same window and cell
    /// weights): the mean of unit vectors at the phase angle, then its angle,
    /// so a wrap at +-180 degrees is no jump. `None` without phase, or when it
    /// does not match the grid; `NaN` phase counts as no data.
    pub fn phase_smoothed(&self, s: Smoothing) -> Option<Vec<f64>> {
        let phase = self.phase.as_ref().filter(|p| p.len() == self.freq.len())?;
        let (Some(fraction), true) = (s.fraction(), self.freq.len() >= 2) else {
            return Some(phase.iter().map(|d| wrap_degrees(*d)).collect());
        };
        let n = self.freq.len();
        let x: Vec<f64> = self.freq.iter().map(|f| f.log2()).collect();
        let edges = cell_edges(&x);
        let unit: Vec<(f64, f64)> = phase.iter().map(|d| d.to_radians().sin_cos()).collect();
        // Running integrals of the vector sum over the valid cells.
        let mut sum_s = vec![0.0];
        let mut sum_c = vec![0.0];
        for i in 0..n {
            let (sn, cs) = if unit[i].0.is_nan() {
                (0.0, 0.0)
            } else {
                let w = edges[i + 1] - edges[i];
                (unit[i].0 * w, unit[i].1 * w)
            };
            sum_s.push(sum_s[i] + sn);
            sum_c.push(sum_c[i] + cs);
        }
        let upto = |at: f64| -> (f64, f64) {
            let at = at.clamp(edges[0], edges[n]);
            let i = edges.partition_point(|e| *e <= at).clamp(1, n) - 1;
            let part = at - edges[i];
            if unit[i].0.is_nan() {
                (sum_s[i], sum_c[i])
            } else {
                (sum_s[i] + unit[i].0 * part, sum_c[i] + unit[i].1 * part)
            }
        };
        let half = fraction / 2.0;
        Some(
            x.iter()
                .map(|c| {
                    let (s0, c0) = upto(c - half);
                    let (s1, c1) = upto(c + half);
                    let (sn, cs) = (s1 - s0, c1 - c0);
                    if sn.hypot(cs) <= 1e-12 {
                        f64::NAN
                    } else {
                        wrap_degrees(sn.atan2(cs).to_degrees())
                    }
                })
                .collect(),
        )
    }

    /// Subtract `curve` (interpolated in log f, edge values held); `invert` adds instead.
    pub fn compensated(&self, curve: &Response, invert: bool) -> Response {
        let (Some(&lo), Some(&hi)) = (curve.freq.first(), curve.freq.last()) else {
            return self.clone();
        };
        let sign = if invert { 1.0 } else { -1.0 };
        let db = self
            .freq
            .iter()
            .zip(&self.db)
            .map(|(f, d)| d + sign * curve.interp_db(f.clamp(lo, hi)))
            .collect();
        Response {
            freq: self.freq.clone(),
            db,
            phase: self.phase.clone(),
        }
    }

    pub fn level_at(&self, f: f64) -> f64 {
        self.interp_db(f)
    }

    /// Shift so the level at `f` is 0 dB. Unchanged if there is no level at `f`.
    pub fn aligned(&self, f: f64) -> Response {
        let level = self.level_at(f);
        if level.is_nan() {
            return self.clone();
        }
        Response {
            freq: self.freq.clone(),
            db: self.db.iter().map(|d| d - level).collect(),
            phase: self.phase.clone(),
        }
    }

    /// Shift so the mean level over `f * 2^(+-octaves/2)` is 0 dB. The mean is of dB,
    /// weighted by each point's cell overlap with the band, skipping `NaN`. `octaves` of 0
    /// is [`Response::aligned`]; unchanged if the band holds no data.
    pub fn aligned_band(&self, f: f64, octaves: f64) -> Response {
        if octaves <= 0.0 {
            return self.aligned(f);
        }
        let n = self.freq.len();
        if n < 2 {
            return self.clone();
        }
        let x: Vec<f64> = self.freq.iter().map(|f| f.log2()).collect();
        let edges = cell_edges(&x);
        let (lo, hi) = (f.log2() - octaves / 2.0, f.log2() + octaves / 2.0);
        let (mut sum, mut width) = (0.0, 0.0);
        for (i, d) in self.db.iter().enumerate() {
            let w = edges[i + 1].min(hi) - edges[i].max(lo);
            if !d.is_nan() && w > 0.0 {
                sum += d * w;
                width += w;
            }
        }
        if width <= 1e-12 {
            return self.clone();
        }
        let mean = sum / width;
        self.offset(-mean)
    }

    /// Add `db` to every point; `NaN` stays `NaN`.
    pub fn offset(&self, db: f64) -> Response {
        Response {
            freq: self.freq.clone(),
            db: self.db.iter().map(|d| d + db).collect(),
            phase: self.phase.clone(),
        }
    }
}

/// Per-point dB mean on `log_grid(48_000)`, ignoring `NaN` (`NaN` if all are). `None` for no input.
pub fn average(items: &[&Response]) -> Option<Response> {
    if items.is_empty() {
        return None;
    }
    let grid = log_grid(48_000);
    let resampled: Vec<Response> = items.iter().map(|r| r.resampled(&grid)).collect();
    let db = (0..grid.len())
        .map(|i| {
            let (sum, n) = resampled
                .iter()
                .map(|r| r.db[i])
                .filter(|d| !d.is_nan())
                .fold((0.0, 0usize), |(s, n), d| (s + d, n + 1));
            if n == 0 { f64::NAN } else { sum / n as f64 }
        })
        .collect();
    Some(Response {
        freq: grid,
        db,
        phase: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(db: f64) -> Response {
        let freq = log_grid(48_000);
        Response {
            db: vec![db; freq.len()],
            freq,
            phase: None,
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn grid_is_96_ppo() {
        let g = log_grid(48_000);
        assert_eq!(g[0], 10.0);
        for w in g.windows(2) {
            assert!((w[1] / w[0] - 2f64.powf(1.0 / 96.0)).abs() < 1e-12);
        }
        assert!(*g.last().unwrap() <= 24_000.0);
        assert!(*g.last().unwrap() > 24_000.0 / 2f64.powf(1.0 / 96.0) - 1.0);
        assert!(*log_grid(44_100).last().unwrap() <= 22_050.0);
    }

    // Bugs caught: 0, False positives: 1
    #[test]
    fn smoothing_flattens_ripple() {
        let mut r = flat(0.0);
        for (i, v) in r.db.iter_mut().enumerate() {
            *v = if i % 2 == 0 { 3.0 } else { -3.0 };
        }
        let s = r.smoothed(Smoothing::Oct3);
        // Power-domain mean of +-3 dB is 10*log10((10^0.3 + 10^-0.3)/2) = 0.964 dB, not 0 dB.
        let expected = 10.0 * ((10f64.powf(0.3) + 10f64.powf(-0.3)) / 2.0).log10();
        for (f, d) in s.freq.iter().zip(&s.db) {
            if (100.0..=10_000.0).contains(f) {
                assert!((d - expected).abs() < 0.2, "{f} Hz: {d}");
            }
        }
        assert_eq!(r.smoothed(Smoothing::None), r);
    }

    // On the 96 PPO grid a 1/48 octave window spans exactly two cells; its edges land on
    // grid points, so a point-counting window takes two or three points by rounding and
    // turns even ripple into a jagged trace.
    // Bugs caught: 1, False positives: 0
    #[test]
    fn smoothing_is_even_on_the_grid() {
        let mut r = flat(0.0);
        for (i, v) in r.db.iter_mut().enumerate() {
            *v = if i % 2 == 0 { 3.0 } else { -3.0 };
        }
        let expected = 10.0 * ((10f64.powf(0.3) + 10f64.powf(-0.3)) / 2.0).log10();
        for s in [Smoothing::Oct48, Smoothing::Oct24, Smoothing::Oct12] {
            let out = r.smoothed(s);
            for (f, d) in out.freq.iter().zip(&out.db) {
                if (100.0..=10_000.0).contains(f) {
                    assert!((d - expected).abs() < 0.025, "{s:?} at {f} Hz: {d} dB");
                }
            }
        }
    }

    // Imported REW files are not on the 96 PPO grid; smoothing must work on any spacing.
    // Bugs caught: 0, False positives: 0
    #[test]
    fn smoothing_handles_uneven_points() {
        // Points at -1/4, -1/16, 0 and +1/4 octave around 1024 Hz. The 10 dB
        // point's cell is 1/8 octave wide (midpoints at -5/32 and -1/32), all
        // inside the 1/3 octave window around 1024 Hz; counting points would
        // weigh it 1 in 3 (wrongly), its width weighs it 3/8.
        let offsets = [-0.25_f64, -1.0 / 16.0, 0.0, 0.25];
        let r = Response {
            freq: offsets.map(|x| 1024.0 * x.exp2()).to_vec(),
            db: vec![0.0, 10.0, 0.0, 0.0],
            phase: None,
        };
        let s = r.smoothed(Smoothing::Oct3);
        let expected = 10.0 * (1.0_f64 + 9.0 * 3.0 / 8.0).log10();
        assert!((s.db[2] - expected).abs() < 1e-9, "{} dB", s.db[2]);
    }

    // A missing point adds neither power nor width; a window with no data
    // at all has no level.
    // Bugs caught: 0, False positives: 0
    #[test]
    fn smoothing_skips_missing_points() {
        let freq: Vec<f64> = (0..8)
            .map(|k| 1000.0 * (f64::from(k) / 8.0).exp2())
            .collect();
        let mut db = vec![0.0; 8];
        db[3] = f64::NAN;
        db[4] = 6.0;
        let s = Response {
            freq: freq.clone(),
            db,
            phase: None,
        }
        .smoothed(Smoothing::Oct6);
        assert!(s.db.iter().all(|d| d.is_finite()), "{:?}", s.db);
        let none = Response {
            freq,
            db: vec![f64::NAN; 8],
            phase: None,
        }
        .smoothed(Smoothing::Oct6);
        assert!(none.db.iter().all(|d| d.is_nan()));
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn compensation_subtracts_and_inverts() {
        let r = flat(0.0);
        let curve = Response {
            freq: vec![100.0, 1000.0],
            db: vec![2.0, 2.0],
            phase: None,
        };
        let c = r.compensated(&curve, false);
        assert!(c.db.iter().all(|d| (d + 2.0).abs() < 1e-12)); // includes f outside 100..1000
        let c = r.compensated(&curve, true);
        assert!(c.db.iter().all(|d| (d - 2.0).abs() < 1e-12));
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn compensation_interpolates_in_log_f() {
        let r = flat(0.0);
        let curve = Response {
            freq: vec![100.0, 10_000.0],
            db: vec![0.0, 4.0],
            phase: None,
        };
        let c = r.compensated(&curve, false);
        assert!((c.level_at(1000.0) + 2.0).abs() < 0.05);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn average_ignores_nan() {
        let a = flat(0.0);
        let mut b = flat(6.0);
        for (f, d) in b.freq.iter().zip(b.db.iter_mut()) {
            if *f < 100.0 {
                *d = f64::NAN;
            }
        }
        let avg = average(&[&a, &b]).unwrap();
        for (f, d) in avg.freq.iter().zip(&avg.db) {
            if *f > 101.0 {
                assert!((d - 3.0).abs() < 1e-9);
            } else if *f < 99.0 {
                assert!(d.abs() < 1e-9);
            }
        }
        assert!(average(&[]).is_none());
        assert!(avg.phase.is_none());
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn align_at_1k() {
        let freq = log_grid(48_000);
        let db = freq
            .iter()
            .map(|f| if *f < 1000.0 { 3.0 } else { 5.0 })
            .collect();
        let r = Response {
            freq,
            db,
            phase: None,
        };
        let a = r.aligned(1000.0);
        assert!(a.level_at(1000.0).abs() < 1e-9);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn interp_is_nan_outside_range() {
        let r = flat(1.0);
        assert!(r.interp_db(5.0).is_nan());
        assert!(r.interp_db(30_000.0).is_nan());
        assert_eq!(r.interp_db(1234.5), 1.0);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn interp_gives_a_point_its_own_level_next_to_a_gap() {
        let r = Response {
            freq: vec![100.0, 200.0, 400.0, 800.0],
            db: vec![f64::NAN, 3.0, 5.0, f64::NAN],
            phase: None,
        };
        assert_eq!(r.interp_db(200.0), 3.0);
        assert_eq!(r.interp_db(400.0), 5.0);
        assert!(r.interp_db(150.0).is_nan());
        assert!((r.interp_db(282.842_712_474_619) - 4.0).abs() < 1e-9);
        let first = Response {
            freq: vec![100.0, 200.0],
            db: vec![1.0, f64::NAN],
            phase: None,
        };
        assert_eq!(first.interp_db(100.0), 1.0);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn band_align_zeroes_the_band_mean() {
        let freq = log_grid(48_000);
        let db = freq.iter().map(|f| 6.0 * (f / 1000.0).log2()).collect();
        let r = Response {
            freq,
            db,
            phase: None,
        };
        let a = r.aligned_band(1000.0, 1.0);
        let band: Vec<f64> = a
            .freq
            .iter()
            .zip(&a.db)
            .filter(|(f, _)| (707.0..=1414.0).contains(*f))
            .map(|(_, d)| *d)
            .collect();
        let mean = band.iter().sum::<f64>() / band.len() as f64;
        assert!(mean.abs() < 0.05, "{mean}");
        assert_eq!(r.aligned_band(1000.0, 0.0), r.aligned(1000.0));
        // No data in the band: unchanged.
        assert_eq!(r.aligned_band(1.0, 1.0), r);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn band_align_uses_the_cell_weighted_mean_of_the_exact_band() {
        // Points at 1 kHz * 2^x for x = -2, -0.5, 0, 1, 3 (uneven), so the
        // cell edges in x are -2.75, -1.25, -0.25, 0.5, 2, 4. A 1 octave
        // band at 1 kHz is x in [-0.5, 0.5]: it takes 0.25 of the cell of
        // x=-0.5 (10 dB) and 0.75 of the cell of x=0 (2 dB). The
        // weighted mean is (10*0.25 + 2*0.75) / 1 = 4 dB.
        // An unweighted mean of those two would be 6 dB; a band twice as
        // wide, x in [-1, 1], would give
        // (10*0.75 + 2*0.75 + 50*0.5) / 2 = 17 dB.
        let x = [-2.0_f64, -0.5, 0.0, 1.0, 3.0];
        let db = [100.0, 10.0, 2.0, 50.0, 100.0];
        let r = Response {
            freq: x.iter().map(|x| 1000.0 * x.exp2()).collect(),
            db: db.to_vec(),
            phase: None,
        };
        let a = r.aligned_band(1000.0, 1.0);
        for (got, want) in a.db.iter().zip(db.iter().map(|d| d - 4.0)) {
            assert!((got - want).abs() < 1e-9, "{got} vs {want}");
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn band_align_skips_nan() {
        let mut r = flat(4.0);
        for (f, d) in r.freq.iter().zip(r.db.iter_mut()) {
            if *f > 1000.0 {
                *d = f64::NAN;
            }
        }
        let a = r.aligned_band(1000.0, 1.0);
        assert!(a.db.iter().zip(&r.db).all(|(x, y)| if y.is_nan() {
            x.is_nan()
        } else {
            x.abs() < 1e-9
        }));
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn offset_shifts_every_point() {
        let r = Response {
            freq: vec![100.0, 200.0, 400.0],
            db: vec![1.0, f64::NAN, -3.0],
            phase: Some(vec![10.0, 20.0, 30.0]),
        };
        let o = r.offset(2.5);
        assert_eq!(o.freq, r.freq);
        assert_eq!(o.db[0], 3.5);
        assert!(o.db[1].is_nan());
        assert_eq!(o.db[2], -0.5);
        assert_eq!(o.phase, r.phase);
    }

    // A phase that rises steadily stays that line under smoothing, and one
    // that wraps at +-180 degrees is no jump to the average.
    // Bugs caught: 0, False positives: 0
    #[test]
    fn phase_smoothing_keeps_a_linear_phase_linear() {
        let freq = log_grid(48_000);
        let ramp: Vec<f64> = freq.iter().map(|f| 20.0 * (f / 1000.0).log2()).collect();
        let r = Response {
            db: vec![0.0; freq.len()],
            phase: Some(ramp.clone()),
            freq: freq.clone(),
        };
        let s = r.phase_smoothed(Smoothing::Oct3).unwrap();
        for ((f, got), want) in freq.iter().zip(&s).zip(&ramp) {
            if (100.0..=10_000.0).contains(f) {
                assert!((got - want).abs() < 0.1, "{f} Hz: {got} vs {want}");
            }
        }
        assert_eq!(r.phase_smoothed(Smoothing::None).unwrap(), ramp);
        let unwrapped = Response {
            phase: Some(vec![190.0, -190.0, 180.0, -180.0, 540.0]),
            freq: vec![1.0; 5],
            db: vec![0.0; 5],
        };
        assert_eq!(
            unwrapped.phase_smoothed(Smoothing::None).unwrap(),
            [-170.0, 170.0, 180.0, 180.0, 180.0]
        );
        // Either side of the wrap averages to the wrap, not to 0.
        let wrapped = Response {
            freq: vec![100.0, 101.0, 102.0],
            db: vec![0.0; 3],
            phase: Some(vec![170.0, -170.0, 170.0]),
        };
        let m = wrapped.phase_smoothed(Smoothing::Oct3).unwrap()[1];
        assert!(m.abs() > 170.0, "{m}");
        let none = Response {
            phase: None,
            ..wrapped
        };
        assert!(none.phase_smoothed(Smoothing::Oct3).is_none());
    }

    // atan2 of a vanishing sine may return -180: the smoothed angle is still in (-180, 180].
    // Bugs caught: 1, False positives: 0
    #[test]
    fn smoothed_phase_never_returns_minus_180() {
        let r = Response {
            freq: vec![100.0, 10_000.0],
            db: vec![0.0; 2],
            phase: Some(vec![180.0, -180.0]),
        };
        for p in r.phase_smoothed(Smoothing::Oct3).unwrap() {
            assert!(p > -180.0 && p <= 180.0, "{p}");
        }
    }
}
