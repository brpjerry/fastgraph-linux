//! Streaming sweep detector: finds marker A … marker B in a continuous capture.
//!
//! The capture runs through a matched filter for the marker: FFT overlap-save producing 8192
//! new outputs per block (FFT length the next power of two ≥ 8192 + marker length − 1),
//! correlating with the marker, i.e. convolving with the time-reversed marker. The envelope
//! `|output|` is indexed by the position where a marker would *start*, so a peak at `s` means a
//! marker starting at sample `s`. Positions are refined by parabolic interpolation over the 3
//! envelope samples around the peak.
//!
//! A candidate is a local maximum of the envelope within ±50 ms whose detection ratio (see
//! `Detector::ratio`) is at least 12: the peak must reach 12× the median envelope of the 2 s
//! before it (the median is the exact median of every n-th envelope sample, about 4000 values,
//! an approximation of the median of all of them) and 12× a local median-equivalent level
//! within ±50 ms. After marker A the detector waits for the whole window where marker B may lie
//! (±2 % of the expected spacing) plus marker B and the 0.2 s tail, then takes the largest
//! envelope value in that window as B, with the same rule (median taken from before A). Further
//! A candidates while a capture is pending are ignored.

use std::collections::VecDeque;
use std::sync::Arc;

use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

use crate::signal::{SweepLength, expected_spacing, marker};

/// New envelope samples produced per matched-filter FFT.
const BLOCK: usize = 8192;
/// A marker peak must reach this multiple of the median envelope (and of the local level).
const THRESHOLD: f64 = 12.0;
/// Span of envelope before a candidate that the median is taken over.
const MEDIAN_SECONDS: f64 = 2.0;
/// The median is estimated from every n-th envelope sample, about this many per 2 s window.
const MEDIAN_POINTS: f64 = 4000.0;
/// A candidate needs at least this much envelope before it for a meaningful median.
const MIN_HISTORY_SECONDS: f64 = 0.25;
/// A peak must be the largest envelope value within ± this.
const LOCAL_MAX_SECONDS: f64 = 0.05;
/// Marker B is searched within ± this fraction of the expected spacing.
const SPACING_TOLERANCE: f64 = 0.02;
/// Recording kept before marker A and after the end of marker B.
const PAD_SECONDS: f64 = 0.2;
/// Audio and envelope history kept beyond the sweep length: at least `L + 3 s` is retained.
const HISTORY_EXTRA_SECONDS: f64 = 3.0;
/// Half-width of the matched-filter main lobe left out of the local level.
const MAIN_LOBE_SECONDS: f64 = 0.001;
/// Median of |x| over RMS of x for Gaussian x.
const GAUSSIAN_MEDIAN_PER_RMS: f64 = 0.6745;
/// Ratios are capped so digital silence does not report infinity.
const MAX_RATIO: f64 = 1e6;
const CLIP_LEVEL: f32 = 0.999;

/// A detected sweep, cut from the stream with its marker positions.
#[derive(Clone, Debug, PartialEq)]
pub struct Capture {
    pub rate: u32,
    pub length: SweepLength,
    /// Marker A − 0.2 s to the end of marker B + 0.2 s.
    pub recording: Vec<f32>,
    /// Sub-sample start of marker A in `recording`.
    ///
    /// Under clock drift both marker positions carry the same bias of about
    /// `(drift_ratio − 1) × 19.5 ms` (+0.28 samples at +300 ppm, 48 kHz): the correlation peak
    /// aligns the high-frequency-weighted part of the stretched chirp, not its start. The bias
    /// is common to both markers, so `drift_ratio` is unaffected.
    pub marker_a: f64,
    /// Sub-sample start of marker B in `recording`.
    pub marker_b: f64,
    /// Measured marker spacing / expected spacing.
    pub drift_ratio: f64,
    pub drift_ppm: f64,
    /// The weaker of the two markers' detection ratios (≥ 12).
    pub peak_ratio: f32,
    /// Any |sample| ≥ 0.999 in `recording`.
    pub clipped: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DetectEvent {
    Captured(Capture),
    /// Marker A was found but no marker B followed within ±2 % of the expected spacing.
    MissingEnd,
}

/// Samples of a stream from absolute position `start` on; trimmed to at least `keep` samples.
struct Ring {
    start: usize,
    data: Vec<f32>,
    keep: usize,
}

impl Ring {
    fn new(keep: usize) -> Self {
        Ring {
            start: 0,
            data: Vec::with_capacity(2 * keep),
            keep,
        }
    }

    fn end(&self) -> usize {
        self.start + self.data.len()
    }

    fn extend(&mut self, x: &[f32]) {
        self.data.extend_from_slice(x);
        // Trim in bulk so the memmove costs about one copy per sample.
        if self.data.len() >= 2 * self.keep {
            let drop = self.data.len() - self.keep;
            self.data.drain(..drop);
            self.start += drop;
        }
    }

    fn at(&self, i: usize) -> f32 {
        self.data[i - self.start]
    }

    fn slice(&self, from: usize, to: usize) -> &[f32] {
        &self.data[from - self.start..to - self.start]
    }
}

/// Marker A, waiting for the window where marker B may lie.
struct Pending {
    /// Sub-sample start of marker A (absolute).
    a: f64,
    a_peak: usize,
    ratio: f64,
    median: f64,
    /// Marker B search range (absolute, inclusive).
    lo: usize,
    hi: usize,
}

/// Streaming marker detector; see the module documentation.
pub struct Detector {
    rate: u32,
    length: SweepLength,
    marker_len: usize,
    /// Expected spacing in samples.
    spacing: f64,
    forward: Arc<dyn RealToComplex<f64>>,
    inverse: Arc<dyn ComplexToReal<f64>>,
    /// conj(FFT(marker)) / N.
    kernel: Vec<Complex<f64>>,
    frame: Vec<f64>,
    spectrum: Vec<Complex<f64>>,
    output: Vec<f64>,
    scratch_fwd: Vec<Complex<f64>>,
    scratch_inv: Vec<Complex<f64>>,
    audio: Ring,
    env: Ring,
    window: usize,
    main_lobe: usize,
    median_span: usize,
    median_step: usize,
    min_history: usize,
    pad: usize,
    median_buf: Vec<f32>,
    /// Envelope indices with decreasing values over the local-maximum window.
    maxima: VecDeque<usize>,
    /// Next envelope index to enter `maxima`.
    next: usize,
    /// Candidates at or before this index are not markers A (already used).
    used_until: Option<usize>,
    pending: Option<Pending>,
    best: f64,
}

impl Detector {
    pub fn new(length: SweepLength, rate: u32) -> Self {
        let fs = f64::from(rate);
        let m = marker(rate);
        let marker_len = m.len();
        let n = (BLOCK + marker_len - 1).next_power_of_two();
        let mut planner = RealFftPlanner::<f64>::new();
        let forward = planner.plan_fft_forward(n);
        let inverse = planner.plan_fft_inverse(n);
        let mut frame = forward.make_input_vec();
        frame
            .iter_mut()
            .zip(&m)
            .for_each(|(d, &s)| *d = f64::from(s));
        let mut kernel = forward.make_output_vec();
        forward
            .process(&mut frame, &mut kernel)
            .expect("buffer sizes match the plan");
        kernel.iter_mut().for_each(|c| *c = c.conj() / n as f64);
        let keep = ((length.seconds() + HISTORY_EXTRA_SECONDS) * fs).ceil() as usize;
        Detector {
            rate,
            length,
            marker_len,
            spacing: expected_spacing(length) * fs,
            spectrum: forward.make_output_vec(),
            output: inverse.make_output_vec(),
            scratch_fwd: forward.make_scratch_vec(),
            scratch_inv: inverse.make_scratch_vec(),
            forward,
            inverse,
            kernel,
            frame,
            audio: Ring::new(keep),
            env: Ring::new(keep),
            window: (LOCAL_MAX_SECONDS * fs).round() as usize,
            main_lobe: (MAIN_LOBE_SECONDS * fs).round() as usize,
            median_span: (MEDIAN_SECONDS * fs).round() as usize,
            median_step: ((MEDIAN_SECONDS * fs / MEDIAN_POINTS).round() as usize).max(1),
            min_history: (MIN_HISTORY_SECONDS * fs).round() as usize,
            pad: (PAD_SECONDS * fs).round() as usize,
            median_buf: Vec::new(),
            maxima: VecDeque::new(),
            next: 0,
            used_until: None,
            pending: None,
            best: 0.0,
        }
    }

    /// Feeds captured audio of any length; returns the sweeps completed by it.
    pub fn push(&mut self, block: &[f32]) -> Vec<DetectEvent> {
        let mut events = Vec::new();
        // Each piece is filtered and scanned before the next one is appended, so trimming the
        // history never drops audio the filter, the scan or a pending capture still needs.
        for piece in block.chunks(BLOCK) {
            if piece.iter().all(|v| v.is_finite()) {
                self.audio.extend(piece);
            } else {
                // A NaN would poison the matched filter's whole block and every ratio after it.
                let clean: Vec<f32> = piece
                    .iter()
                    .map(|&v| if v.is_finite() { v } else { 0.0 })
                    .collect();
                self.audio.extend(&clean);
            }
            self.filter();
            events.extend(self.scan());
        }
        events
    }

    /// Largest detection ratio of any envelope peak seen since the last
    /// [`Detector::reset_best`] (12 is needed to detect); not updated while a capture is pending.
    pub fn best_ratio(&self) -> f32 {
        self.best as f32
    }

    pub fn reset_best(&mut self) {
        self.best = 0.0;
    }

    /// Runs the matched filter over every complete block of audio.
    fn filter(&mut self) {
        let span = BLOCK + self.marker_len - 1;
        while self.audio.end() >= self.env.end() + span {
            let s0 = self.env.end();
            let input = self.audio.slice(s0, s0 + span);
            for (d, &s) in self.frame.iter_mut().zip(input) {
                *d = f64::from(s);
            }
            self.frame[span..].fill(0.0);
            self.forward
                .process_with_scratch(&mut self.frame, &mut self.spectrum, &mut self.scratch_fwd)
                .expect("buffer sizes match the plan");
            for (y, k) in self.spectrum.iter_mut().zip(&self.kernel) {
                *y *= k;
            }
            // realfft rejects a non-zero imaginary part in the DC and Nyquist bins.
            let last = self.spectrum.len() - 1;
            self.spectrum[0].im = 0.0;
            self.spectrum[last].im = 0.0;
            self.inverse
                .process_with_scratch(&mut self.spectrum, &mut self.output, &mut self.scratch_inv)
                .expect("buffer sizes match the plan");
            let env: Vec<f32> = self.output[..BLOCK]
                .iter()
                .map(|v| v.abs() as f32)
                .collect();
            self.env.extend(&env);
        }
    }

    /// Walks the new envelope: local maxima become marker A candidates, pending captures
    /// are resolved once their marker B window and tail have arrived.
    fn scan(&mut self) -> Vec<DetectEvent> {
        let mut events = Vec::new();
        let w = self.window;
        while self.next < self.env.end() {
            let t = self.next;
            // Centre whose ±window ends at t.
            let centre = t.checked_sub(w);
            if let (Some(c), Some(p)) = (centre, &self.pending)
                && c > p.hi
            {
                if self.audio.end() < p.hi + self.marker_len + self.pad + 2 {
                    break;
                }
                events.push(self.resolve());
            }
            let v = self.env.at(t);
            while self.maxima.back().is_some_and(|&i| self.env.at(i) <= v) {
                self.maxima.pop_back();
            }
            self.maxima.push_back(t);
            while self.maxima.front().is_some_and(|&i| i + 2 * w < t) {
                self.maxima.pop_front();
            }
            self.next += 1;
            if let Some(c) = centre
                && self.maxima.front() == Some(&c)
                && self.pending.is_none()
                && self.used_until.is_none_or(|u| c > u)
            {
                self.candidate(c);
            }
        }
        events
    }

    /// Median envelope over the span before `c`, from every `median_step`-th sample.
    fn median_before(&mut self, c: usize) -> Option<f64> {
        let from = c.saturating_sub(self.median_span).max(self.env.start);
        if c < from + self.min_history {
            return None;
        }
        let step = self.median_step;
        let first = from.div_ceil(step) * step;
        self.median_buf.clear();
        self.median_buf
            .extend((first..c).step_by(step).map(|i| self.env.at(i)));
        let mid = self.median_buf.len() / 2;
        let (_, m, _) = self.median_buf.select_nth_unstable_by(mid, f32::total_cmp);
        Some(f64::from(*m))
    }

    /// Detection ratio of the peak at `c`: the smaller of peak / `median` (the median rule) and
    /// peak / local level, where the local level is the RMS of the envelope within ±50 ms
    /// (without the ±1 ms main lobe) scaled to a median by [`GAUSSIAN_MEDIAN_PER_RMS`].
    ///
    /// The local term rejects energy that is loud against the quiet before it but not
    /// pulse-like: the stimulus' own wake noise (measured ratio 75 against −50 dB noise, local
    /// ratio 6) and the sweep (ratio 305, local ratio 2.2), which correlate with the marker
    /// without compressing to a peak. A real marker compresses (local ratio ≈ 170 without
    /// noise); in stationary Gaussian background both terms estimate the same median, so the
    /// local term does not change the sensitivity.
    fn ratio(&self, c: usize, median: f64) -> f64 {
        let peak = f64::from(self.env.at(c));
        let from = c.saturating_sub(self.window).max(self.env.start);
        let to = (c + self.window + 1).min(self.env.end());
        let (mut sum, mut n) = (0.0, 0usize);
        for (i, &v) in (from..to).zip(self.env.slice(from, to)) {
            if i.abs_diff(c) > self.main_lobe {
                sum += f64::from(v).powi(2);
                n += 1;
            }
        }
        let local = GAUSSIAN_MEDIAN_PER_RMS * (sum / n.max(1) as f64).sqrt();
        let over = |level: f64| {
            if level > 0.0 {
                peak / level
            } else if peak > 0.0 {
                f64::INFINITY
            } else {
                0.0
            }
        };
        over(median).min(over(local)).min(MAX_RATIO)
    }

    /// Whether `ring` still holds `from..to`.
    fn holds(ring: &Ring, from: usize, to: usize) -> bool {
        ring.start <= from && to <= ring.end()
    }

    fn is_local_max(&self, c: usize) -> bool {
        let v = self.env.at(c);
        let from = c.saturating_sub(self.window).max(self.env.start);
        let to = (c + self.window + 1).min(self.env.end());
        self.env.slice(from, to).iter().all(|&x| x <= v)
    }

    /// Sub-sample position of the peak at `c` by parabolic interpolation.
    fn refine(&self, c: usize) -> f64 {
        if c == self.env.start || c + 1 >= self.env.end() {
            return c as f64;
        }
        let (l, m, r) = (
            f64::from(self.env.at(c - 1)),
            f64::from(self.env.at(c)),
            f64::from(self.env.at(c + 1)),
        );
        let d = l - 2.0 * m + r;
        if d >= 0.0 {
            return c as f64;
        }
        c as f64 + (0.5 * (l - r) / d).clamp(-0.5, 0.5)
    }

    fn candidate(&mut self, c: usize) {
        let Some(median) = self.median_before(c) else {
            return;
        };
        let ratio = self.ratio(c, median);
        self.best = self.best.max(ratio);
        if ratio < THRESHOLD {
            return;
        }
        let a = self.refine(c);
        let tol = SPACING_TOLERANCE * self.spacing;
        self.pending = Some(Pending {
            a,
            a_peak: c,
            ratio,
            median,
            lo: (a + self.spacing - tol).ceil() as usize,
            hi: (a + self.spacing + tol).floor() as usize,
        });
    }

    /// Looks for marker B in the pending window; the envelope covers it and its ±window.
    fn resolve(&mut self) -> DetectEvent {
        let p = self
            .pending
            .take()
            .expect("resolve needs a pending capture");
        let w = self.window;
        if !Self::holds(&self.env, p.lo.saturating_sub(w), p.hi + w + 1) {
            debug_assert!(false, "marker B window no longer in the envelope history");
            self.used_until = Some(p.hi);
            return DetectEvent::MissingEnd;
        }
        let mut b = p.lo;
        for i in p.lo..=p.hi {
            if self.env.at(i) > self.env.at(b) {
                b = i;
            }
        }
        let ratio = self.ratio(b, p.median);
        self.best = self.best.max(ratio);
        if ratio < THRESHOLD || !self.is_local_max(b) {
            self.used_until = Some(p.hi);
            return DetectEvent::MissingEnd;
        }
        self.used_until = Some(p.hi.max(b + self.window));
        let b_pos = self.refine(b);
        let end = b + self.marker_len + self.pad;
        let Some(start) = p
            .a_peak
            .checked_sub(self.pad)
            .filter(|&start| Self::holds(&self.audio, start, end))
        else {
            // Never emit a capture whose recording would be cut short.
            debug_assert!(false, "capture span no longer in the audio history");
            return DetectEvent::MissingEnd;
        };
        let recording = self.audio.slice(start, end).to_vec();
        let clipped = recording.iter().any(|v| v.abs() >= CLIP_LEVEL);
        let drift_ratio = (b_pos - p.a) / self.spacing;
        DetectEvent::Captured(Capture {
            rate: self.rate,
            length: self.length,
            recording,
            marker_a: p.a - start as f64,
            marker_b: b_pos - start as f64,
            drift_ratio,
            drift_ppm: (drift_ratio - 1.0) * 1e6,
            peak_ratio: p.ratio.min(ratio) as f32,
            clipped,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deconv::testutil::{biquad_peak, biquad_peak_db, white_noise};
    use crate::deconv::{deconvolve, gate, resample, response_from_ir};
    use crate::signal::{Stimulus, sweep_stimulus};

    const RATE: u32 = 48_000;

    fn db(x: f64) -> f64 {
        10f64.powf(x / 20.0)
    }

    /// White noise at `level_db` dBFS RMS.
    fn noise(len: usize, level_db: f64, seed: u64) -> Vec<f32> {
        let g = db(level_db);
        white_noise(len, seed)
            .into_iter()
            .map(|v| (f64::from(v) * g) as f32)
            .collect()
    }

    /// Adds `signal` into `stream` starting at `offset`.
    fn insert(stream: &mut [f32], signal: &[f32], offset: usize) {
        for (d, s) in stream[offset..offset + signal.len()].iter_mut().zip(signal) {
            *d += s;
        }
    }

    /// Feeds `stream` in blocks of 997 samples (odd, to exercise block boundaries).
    fn feed(det: &mut Detector, stream: &[f32]) -> Vec<DetectEvent> {
        stream.chunks(997).flat_map(|b| det.push(b)).collect()
    }

    fn captures(events: &[DetectEvent]) -> Vec<&Capture> {
        events
            .iter()
            .filter_map(|e| match e {
                DetectEvent::Captured(c) => Some(c),
                DetectEvent::MissingEnd => None,
            })
            .collect()
    }

    fn missing(events: &[DetectEvent]) -> usize {
        events
            .iter()
            .filter(|e| matches!(e, DetectEvent::MissingEnd))
            .count()
    }

    /// Absolute stream position of `recording` (it is copied verbatim from the stream).
    fn locate(stream: &[f32], recording: &[f32]) -> usize {
        let head = &recording[..64];
        stream
            .windows(64)
            .position(|w| w == head)
            .expect("recording is a slice of the stream")
    }

    fn seeded_offset(seed: u64, lo: usize, hi: usize) -> usize {
        let r = white_noise(1, seed)[0];
        let u = (f64::from(r) / 3f64.sqrt() + 1.0) / 2.0;
        lo + (u * (hi - lo) as f64) as usize
    }

    fn stimulus() -> Stimulus {
        sweep_stimulus(SweepLength::Normal, RATE)
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn detects_at_random_offsets() {
        let stim = stimulus();
        for seed in [11u64, 12, 13] {
            let mut stream = noise(30 * RATE as usize, -50.0, seed);
            let offset = seeded_offset(seed, 3 * RATE as usize, 20 * RATE as usize);
            insert(&mut stream, &stim.samples, offset);
            let mut det = Detector::new(SweepLength::Normal, RATE);
            let events = feed(&mut det, &stream);
            assert_eq!(events.len(), 1, "seed {seed}: {} events", events.len());
            let caps = captures(&events);
            assert_eq!(caps.len(), 1, "seed {seed}: no capture");
            let c = caps[0];
            let start = locate(&stream, &c.recording);
            let truth_a = (offset + stim.marker_a) as f64;
            let truth_b = (offset + stim.marker_b) as f64;
            let err_a = start as f64 + c.marker_a - truth_a;
            let err_b = start as f64 + c.marker_b - truth_b;
            eprintln!(
                "seed {seed}: offset {offset}, A error {err_a:+.4}, B error {err_b:+.4}, \
                 drift {:+.3} ppm, peak ratio {:.0}",
                c.drift_ppm, c.peak_ratio
            );
            assert!(err_a.abs() < 0.5, "seed {seed}: A error {err_a}");
            assert!(err_b.abs() < 0.5, "seed {seed}: B error {err_b}");
            assert!(c.drift_ppm.abs() < 5.0, "drift {}", c.drift_ppm);
            assert_eq!(c.rate, RATE);
            assert_eq!(c.length, SweepLength::Normal);
            assert!(!c.clipped);
            assert!(c.peak_ratio >= 12.0);
            // Recording spans A − 0.2 s .. B end + 0.2 s.
            let pad = 0.2 * f64::from(RATE);
            let marker_len = 0.1 * f64::from(RATE);
            assert!((c.marker_a - pad).abs() <= 1.0, "marker_a {}", c.marker_a);
            let tail = c.recording.len() as f64 - (c.marker_b + marker_len);
            assert!((tail - pad).abs() <= 1.0, "tail {tail}");
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn measures_drift() {
        let stim = stimulus();
        for (ratio, want) in [(1.0 / 1.0003, 300.0), (1.0 / 0.99971, -290.0)] {
            let played = resample(&stim.samples, ratio);
            let stretch = 1.0 / ratio;
            let mut stream = noise(16 * RATE as usize, -50.0, 5);
            let offset = 4 * RATE as usize + 123;
            insert(&mut stream, &played, offset);
            let mut det = Detector::new(SweepLength::Normal, RATE);
            let events = feed(&mut det, &stream);
            let caps = captures(&events);
            assert_eq!(events.len(), 1);
            assert_eq!(caps.len(), 1);
            let c = caps[0];
            let start = locate(&stream, &c.recording);
            let truth_a = offset as f64 + stim.marker_a as f64 * stretch;
            let err_a = start as f64 + c.marker_a - truth_a;
            eprintln!(
                "stretch {stretch:.6}: drift {:+.3} ppm (want {want:+}), ratio {:.7}, A error {err_a:+.4}",
                c.drift_ppm, c.drift_ratio
            );
            assert!((c.drift_ppm - want).abs() <= 5.0, "drift {}", c.drift_ppm);
            assert!(((c.drift_ratio - 1.0) * 1e6 - c.drift_ppm).abs() < 1e-6);
            assert!(err_a.abs() < 0.5, "A error {err_a}");
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn missing_end_reported() {
        let stim = stimulus();
        let truncated = &stim.samples[..stim.sweep_start + stim.sweep_len];
        let mut stream = noise(36 * RATE as usize, -50.0, 21);
        insert(&mut stream, truncated, 3 * RATE as usize);
        let second = 20 * RATE as usize;
        insert(&mut stream, &stim.samples, second);
        let mut det = Detector::new(SweepLength::Normal, RATE);
        let events = feed(&mut det, &stream);
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0], DetectEvent::MissingEnd);
        assert_eq!(missing(&events), 1);
        let caps = captures(&events);
        assert_eq!(caps.len(), 1);
        let start = locate(&stream, &caps[0].recording);
        let err = start as f64 + caps[0].marker_a - (second + stim.marker_a) as f64;
        assert!(err.abs() < 0.5, "A error {err}");
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn detects_at_other_rates_and_lengths() {
        for (rate, length) in [(44_100, SweepLength::Long), (96_000, SweepLength::Short)] {
            let stim = sweep_stimulus(length, rate);
            let secs = stim.samples.len() / rate as usize + 7;
            let mut stream = noise(secs * rate as usize, -50.0, 81);
            let offset = 3 * rate as usize + 77;
            insert(&mut stream, &stim.samples, offset);
            let mut det = Detector::new(length, rate);
            let events = feed(&mut det, &stream);
            let caps = captures(&events);
            assert_eq!(events.len(), 1, "{rate} {length:?}: {events:?}");
            assert_eq!(caps.len(), 1);
            let c = caps[0];
            assert_eq!((c.rate, c.length), (rate, length));
            let start = locate(&stream, &c.recording);
            let err = start as f64 + c.marker_a - (offset + stim.marker_a) as f64;
            eprintln!(
                "{rate} {length:?}: A error {err:+.4}, drift {:+.3} ppm",
                c.drift_ppm
            );
            assert!(err.abs() < 0.5, "A error {err}");
            assert!(c.drift_ppm.abs() < 5.0, "drift {}", c.drift_ppm);
            deconvolve(&c.recording, c.marker_a, c.drift_ratio, length, rate).unwrap();
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn non_finite_input_is_ignored() {
        let stim = stimulus();
        let mut stream = noise(14 * RATE as usize, -50.0, 91);
        insert(&mut stream, &stim.samples, 3 * RATE as usize);
        stream[RATE as usize] = f32::NAN;
        stream[RATE as usize + 1] = f32::INFINITY;
        let mut det = Detector::new(SweepLength::Normal, RATE);
        let events = feed(&mut det, &stream);
        assert_eq!(captures(&events).len(), 1, "{events:?}");
        assert!(det.best_ratio().is_finite());
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn sweep_alone_does_not_trigger() {
        let stim = stimulus();
        let sweep = &stim.samples[stim.sweep_start..stim.sweep_start + stim.sweep_len];
        let mut stream = noise(14 * RATE as usize, -50.0, 31);
        insert(&mut stream, sweep, 3 * RATE as usize);
        let mut det = Detector::new(SweepLength::Normal, RATE);
        let events = feed(&mut det, &stream);
        eprintln!("sweep alone: best ratio {:.2}", det.best_ratio());
        assert!(events.is_empty(), "{events:?}");
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn capture_flags_clipping() {
        let stim = stimulus();
        for (gain, want) in [(2.5f32, true), (1.0, false)] {
            let played: Vec<f32> = stim
                .samples
                .iter()
                .map(|&v| (v * gain).clamp(-1.0, 1.0))
                .collect();
            let mut stream = noise(14 * RATE as usize, -50.0, 41);
            insert(&mut stream, &played, 3 * RATE as usize);
            let mut det = Detector::new(SweepLength::Normal, RATE);
            let events = feed(&mut det, &stream);
            let caps = captures(&events);
            assert_eq!(events.len(), 1, "gain {gain}: {events:?}");
            assert_eq!(caps.len(), 1);
            assert_eq!(caps[0].clipped, want, "gain {gain}");
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn best_ratio_reports_near_misses() {
        let stim = stimulus();
        let background = noise(14 * RATE as usize, -50.0, 51);
        let mut det = Detector::new(SweepLength::Normal, RATE);
        assert!(feed(&mut det, &background).is_empty());
        let noise_best = det.best_ratio();
        // −62 dB puts the marker clearly above the noise maxima yet below the 12× need.
        let g = db(-62.0) as f32;
        let quiet: Vec<f32> = stim.samples.iter().map(|&v| v * g).collect();
        let mut stream = background.clone();
        insert(&mut stream, &quiet, 3 * RATE as usize);
        let mut det = Detector::new(SweepLength::Normal, RATE);
        let events = feed(&mut det, &stream);
        let best = det.best_ratio();
        eprintln!("near miss: best ratio {best:.2} (noise alone {noise_best:.2})");
        assert!(events.is_empty(), "{events:?}");
        assert!(noise_best > 0.0, "noise best {noise_best}");
        assert!(
            noise_best + 1.0 < best && best < 12.0,
            "best ratio {best}, noise alone {noise_best}"
        );
        det.reset_best();
        assert_eq!(det.best_ratio(), 0.0);
    }

    // Bugs caught: 1, False positives: 0
    #[test]
    fn large_pushes_keep_marker_a_in_recording() {
        let rate = RATE;
        let stim = sweep_stimulus(SweepLength::Long, rate);
        // Longer than twice the history, so a single push spans more than the ring keeps.
        let offset = 17 * rate as usize + 555;
        let mut stream = noise(offset + stim.samples.len() + 6 * rate as usize, -50.0, 101);
        insert(&mut stream, &stim.samples, offset);
        let fs = rate as usize;
        for chunk in [stream.len(), 3 * fs, 2 * fs, 3 * fs / 2] {
            let mut det = Detector::new(SweepLength::Long, rate);
            let events: Vec<DetectEvent> = stream.chunks(chunk).flat_map(|b| det.push(b)).collect();
            let caps = captures(&events);
            assert_eq!(events.len(), 1, "chunk {chunk}: {events:?}");
            assert_eq!(caps.len(), 1, "chunk {chunk}");
            let c = caps[0];
            let pad = 0.2 * f64::from(rate);
            assert!(
                (c.marker_a - pad).abs() <= 1.0,
                "chunk {chunk}: marker_a {}",
                c.marker_a
            );
            let start = locate(&stream, &c.recording);
            let err = start as f64 + c.marker_a - (offset + stim.marker_a) as f64;
            assert!(err.abs() < 0.5, "chunk {chunk}: A error {err}");
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn capture_feeds_deconvolution() {
        let stim = stimulus();
        let mut bq = biquad_peak(1000.0, 6.0, 1.0, RATE);
        let filtered: Vec<f32> = stim.samples.iter().map(|&v| bq(v)).collect();
        let played = resample(&filtered, 1.0 / 1.0003);
        let sweep = &stim.samples[stim.sweep_start..stim.sweep_start + stim.sweep_len];
        let sweep_rms =
            (sweep.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / sweep.len() as f64).sqrt();
        // −60 dB re sweep RMS, as in the deconvolution tests.
        let level = 20.0 * sweep_rms.log10() - 60.0;
        let mut stream = noise(16 * RATE as usize, level, 61);
        insert(&mut stream, &played, 3 * RATE as usize + 4321);
        let mut det = Detector::new(SweepLength::Normal, RATE);
        let events = feed(&mut det, &stream);
        let caps = captures(&events);
        assert_eq!(caps.len(), 1, "{events:?}");
        let c = caps[0];
        let ir = deconvolve(&c.recording, c.marker_a, c.drift_ratio, c.length, c.rate).unwrap();
        let resp = response_from_ir(&gate(&ir)).aligned(200.0);
        let reference = biquad_peak_db(200.0, 1000.0, 6.0, 1.0, RATE);
        let mut worst = (0.0f64, 0.0f64);
        for (f, d) in resp.freq.iter().zip(&resp.db) {
            if (30.0..=16_000.0).contains(f) {
                let want = biquad_peak_db(*f, 1000.0, 6.0, 1.0, RATE) - reference;
                let err = (d - want).abs();
                assert!(!err.is_nan(), "no data at {f} Hz");
                if err > worst.0 {
                    worst = (err, *f);
                }
            }
        }
        eprintln!(
            "chain: drift {:+.3} ppm, max error {:.4} dB at {:.1} Hz",
            c.drift_ppm, worst.0, worst.1
        );
        assert!(worst.0 <= 0.3, "max error {} dB at {} Hz", worst.0, worst.1);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    #[ignore = "benchmark; run with --release --ignored"]
    fn throughput() {
        let stim = stimulus();
        let mut stream = noise(60 * RATE as usize, -50.0, 71);
        insert(&mut stream, &stim.samples, 20 * RATE as usize);
        let mut det = Detector::new(SweepLength::Normal, RATE);
        let t0 = std::time::Instant::now();
        let events: Vec<DetectEvent> = stream.chunks(1024).flat_map(|b| det.push(b)).collect();
        let elapsed = t0.elapsed();
        eprintln!("60 s at 48 kHz in {elapsed:?}");
        assert_eq!(captures(&events).len(), 1);
        assert!(elapsed.as_secs_f64() < 1.0, "{elapsed:?}");
    }
}
