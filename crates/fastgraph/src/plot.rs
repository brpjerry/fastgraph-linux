//! Pixel <-> (frequency, dB) mapping for the response graph: a log frequency
//! axis over 20 Hz–20 kHz and a dB axis whose top follows the loudest trace
//! shown and whose span follows the plot's shape, so a curve keeps its look
//! at any window size. Free of any UI type so it can be unit-tested.
//!
//! Adapted from fastpeq-gui's `plot.rs` (WTFPL); the
//! preamp-centred dB axis became a top-anchored one.

pub const F_MIN: f64 = 20.0;
pub const F_MAX: f64 = 20_000.0;
/// The dB span of a plot as tall as it is wide: 100 dB across the three
/// decades, so a plot about twice as wide as tall (a 16:9 window) shows 50 dB.
pub const DB_PER_WIDTH: f64 = 100.0;
/// Room kept above the loudest level, and the step the top moves in.
pub const HEADROOM_DB: f64 = 5.0;
/// The top of the dB axis when nothing is shown.
pub const EMPTY_TOP_DB: f64 = 10.0;

/// Room around the plot area for the axis labels, in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Insets {
    pub left: f32,
    pub right: f32,
    pub top: f32,
    pub bottom: f32,
}

/// The plot area inside the canvas, after the axis-label room.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlotBox {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
    /// The dB between the top and bottom edges.
    pub span: f64,
    /// The frequencies on the left and right edges, in Hz.
    pub f_low: f64,
    pub f_high: f64,
}

impl PlotBox {
    pub fn inside(width: f32, height: f32, insets: Insets) -> Self {
        let width = (width - insets.left - insets.right).max(1.0);
        let height = (height - insets.top - insets.bottom).max(1.0);
        Self {
            left: insets.left,
            top: insets.top,
            width,
            height,
            span: DB_PER_WIDTH * f64::from(height / width),
            f_low: F_MIN,
            f_high: F_MAX,
        }
    }

    /// The plot showing the decades `low..high` (log10 Hz) across it. An
    /// edge on 20 Hz or 20 kHz is exactly that, through the rounding of
    /// the powers.
    pub fn with_decades(self, low: f64, high: f64) -> Self {
        let hz = |decades: f64| {
            if decades <= F_MIN.log10() {
                F_MIN
            } else if decades >= F_MAX.log10() {
                F_MAX
            } else {
                10f64.powf(decades).clamp(F_MIN, F_MAX)
            }
        };
        Self {
            f_low: hz(low),
            f_high: hz(high),
            ..self
        }
    }

    pub fn right(&self) -> f32 {
        self.left + self.width
    }

    pub fn bottom(&self) -> f32 {
        self.top + self.height
    }

    /// Whether `(x, y)` is inside the plot area.
    pub fn contains(&self, x: f32, y: f32) -> bool {
        (self.left..=self.right()).contains(&x) && (self.top..=self.bottom()).contains(&y)
    }

    pub fn x_of(&self, freq: f64) -> f32 {
        let t = (freq.max(1e-9) / self.f_low).log10() / (self.f_high / self.f_low).log10();
        self.left + (t as f32) * self.width
    }

    /// Clamped to the frequencies in view; on an edge, exactly its frequency.
    pub fn freq_at(&self, x: f32) -> f64 {
        let t = (f64::from((x - self.left) / self.width)).clamp(0.0, 1.0);
        if t == 1.0 {
            return self.f_high;
        }
        self.f_low * (self.f_high / self.f_low).powf(t)
    }

    /// `top_db` sits on the top edge, `top_db - span` on the bottom one.
    pub fn y_of(&self, db: f64, top_db: f64) -> f32 {
        let t = ((top_db - db) / self.span) as f32;
        self.top + self.height * t
    }

    /// The level at `y` on an axis topped at `top_db`.
    pub fn db_at(&self, y: f32, top_db: f64) -> f64 {
        top_db - f64::from((y - self.top) / self.height) * self.span
    }
}

/// The top of the dB axis for the loudest shown level: rounded up to 5 dB,
/// plus 5 dB of headroom. Nothing shown gives [`EMPTY_TOP_DB`].
pub fn top_db(max: Option<f64>) -> f64 {
    match max {
        Some(max) if max.is_finite() => (max / HEADROOM_DB).ceil() * HEADROOM_DB + HEADROOM_DB,
        _ => EMPTY_TOP_DB,
    }
}

/// How prominent a frequency grid line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tick {
    /// A fractional multiple such as 1.1, 1.2 or 9.9 times a power of ten,
    /// drawn on a zoomed axis with room for them.
    Fine,
    /// A 3, 4, 6, 7, 8 or 9 multiple.
    Minor,
    /// A 2 or 5 multiple.
    Major,
    /// 100 Hz, 1 kHz, 10 kHz.
    Decade,
}

/// A frequency grid line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FreqLine {
    pub freq: f64,
    pub tick: Tick,
    /// Its tenths of the power of ten below it: 10 for 100 Hz, 25 for 250 Hz.
    pub tenths: u32,
}

/// The least room between the fine lines of a stretch, in pixels: they
/// show only on an axis zoomed well in.
const FINE_GAP: f64 = 24.0;

/// The lines of a frequency axis from `low` to `high` Hz, `width` pixels
/// wide: every 1..9 multiple of a power of ten in view, and between two
/// multiples m and m + 1 their tenths, where those stand [`FINE_GAP`] apart.
/// A stretch with none of those gets the tenth nearest its logarithmic
/// middle, which a third of an octave keeps in view.
pub fn freq_grid(low: f64, high: f64, width: f32) -> Vec<FreqLine> {
    let px_per_decade = f64::from(width) / (high / low).log10();
    let mut out = Vec::new();
    for exp in low.log10().floor() as i32..=high.log10().floor() as i32 {
        let tenth = 10f64.powi(exp) / 10.0;
        for m in 1..=9u32 {
            let fine = f64::from(10 * m + 10) / f64::from(10 * m + 9);
            let fine = fine.log10() * px_per_decade >= FINE_GAP;
            for tenths in 10 * m..10 * m + if fine { 10 } else { 1 } {
                let freq = f64::from(tenths) * tenth;
                // A line on an edge stays, through the rounding of a zoom.
                if !(low * (1.0 - 1e-9)..=high * (1.0 + 1e-9)).contains(&freq) {
                    continue;
                }
                let tick = tick_of(tenths);
                out.push(FreqLine { freq, tick, tenths });
            }
        }
    }
    if out.is_empty() {
        let middle = (low * high).sqrt();
        let tenth = 10f64.powi(middle.log10().floor() as i32) / 10.0;
        let tenths = (middle / tenth).round() as u32;
        out.push(FreqLine {
            freq: f64::from(tenths) * tenth,
            tick: tick_of(tenths),
            tenths,
        });
    }
    out
}

/// How prominent the line at `tenths` tenths of a power of ten is.
fn tick_of(tenths: u32) -> Tick {
    match (tenths % 10, tenths / 10) {
        (0, 1) => Tick::Decade,
        (0, 2 | 5) => Tick::Major,
        (0, _) => Tick::Minor,
        _ => Tick::Fine,
    }
}

/// The order frequency lines are labelled in: 1, 2 and 5 times a power
/// of ten, then 3, 7, 4, 6, 8 and 9, then the halves between (1.5, 2.5,
/// …), then the other tenths. Each leaves more room than the next.
fn label_tier(tenths: u32) -> usize {
    match tenths {
        10 | 20 | 50 => 0,
        30 => 1,
        70 => 2,
        40 => 3,
        60 => 4,
        80 => 5,
        90 => 6,
        t if t % 10 == 5 => 7,
        _ => 8,
    }
}

/// The advance of the mono face, as a share of its size.
const MONO_ADVANCE: f32 = 0.62;
/// The least room between two labels side by side, in pixels.
const LABEL_PAD: f32 = 6.0;

/// Which of `lines`, on an axis from `low` to `high` Hz `width` pixels
/// wide, carry a label `size` pixels tall: the tiers of [`label_tier`] in
/// turn while every label keeps room from its neighbours. 1, 2 and 5 times
/// a power of ten are always labelled.
pub fn freq_labelled(lines: &[FreqLine], low: f64, high: f64, width: f32, size: f32) -> Vec<bool> {
    let tiers = (1..=8)
        .take_while(|&n| labels_fit(lines, n + 1, low, high, width, size))
        .last()
        .map_or(1, |n| n + 1);
    lines.iter().map(|l| label_tier(l.tenths) < tiers).collect()
}

/// Whether the labels of the `lines` in the first `tiers` label tiers
/// stand apart.
fn labels_fit(
    lines: &[FreqLine],
    tiers: usize,
    low: f64,
    high: f64,
    width: f32,
    size: f32,
) -> bool {
    let decades = (high / low).log10();
    let placed: Vec<(f32, f32)> = lines
        .iter()
        .filter(|l| label_tier(l.tenths) < tiers)
        .map(|l| {
            let x = ((l.freq / low).log10() / decades) as f32 * width;
            let w = freq_tick(l.freq).chars().count() as f32 * MONO_ADVANCE * size;
            (x, w)
        })
        .collect();
    placed
        .windows(2)
        .all(|pair| pair[1].0 - pair[0].0 >= (pair[0].1 + pair[1].1) / 2.0 + LABEL_PAD)
}

/// Grid lines at absolute dB multiples of `step` on an axis from `top_db`
/// down `span` dB.
pub fn db_grid(top_db: f64, span: f64, step: f64) -> Vec<f64> {
    let low = ((top_db - span) / step).ceil() as i64;
    let high = (top_db / step).floor() as i64;
    (low..=high).map(|k| k as f64 * step).collect()
}

/// A frequency axis label: "50", "200", "1k", "20k".
pub fn freq_tick(f: f64) -> String {
    if f >= 1000.0 {
        format!("{}k", f / 1000.0)
    } else {
        format!("{f}")
    }
}

/// A dB axis label: "+12", "0", "-6".
pub fn db_tick(db: f64) -> String {
    if db > 0.0 {
        format!("+{db}")
    } else {
        format!("{db}")
    }
}

/// A phase in whole degrees, as an axis label and in the readout: "45°",
/// "0°", "−45°".
pub fn phase_label(deg: f64) -> String {
    let whole = deg.round() as i64;
    if whole < 0 {
        format!("−{}°", -whole)
    } else {
        format!("{whole}°")
    }
}

/// A frequency read out in full: "220 Hz", "1.80 kHz", "12.5 kHz".
pub fn freq_label(f: f64) -> String {
    if f >= 10_000.0 {
        format!("{:.1} kHz", f / 1000.0)
    } else if f >= 1000.0 {
        format!("{:.2} kHz", f / 1000.0)
    } else {
        format!("{} Hz", f.round())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INSETS: Insets = Insets {
        left: 44.0,
        right: 16.0,
        top: 16.0,
        bottom: 26.0,
    };

    fn plot() -> PlotBox {
        PlotBox::inside(1000.0, 600.0, INSETS)
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn frequency_mapping_round_trips_and_spans_the_plot() {
        let p = plot();
        assert!((p.x_of(F_MIN) - p.left).abs() < 1e-3);
        assert!((p.x_of(F_MAX) - p.right()).abs() < 1e-3);
        for f in [20.0, 63.0, 1000.0, 4321.0, 20_000.0] {
            assert!((p.freq_at(p.x_of(f)) / f - 1.0).abs() < 1e-4, "{f}");
        }
        assert_eq!(p.freq_at(p.left - 50.0), F_MIN);
        assert_eq!(p.freq_at(p.right() + 50.0), F_MAX);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn level_mapping_spans_the_plot_below_the_top() {
        let p = plot();
        for top in [10.0, 95.0, -15.0] {
            assert!((p.y_of(top, top) - p.top).abs() < 1e-3);
            assert!((p.y_of(top - p.span, top) - p.bottom()).abs() < 1e-3);
            let y = p.y_of(top - 12.5, top);
            assert!((p.db_at(y, top) - (top - 12.5)).abs() < 1e-3);
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn top_rounds_the_loudest_level_up_to_5_db_plus_5() {
        assert_eq!(top_db(Some(87.3)), 95.0);
        assert_eq!(top_db(Some(85.0)), 90.0);
        assert_eq!(top_db(Some(-3.0)), 5.0);
        assert_eq!(top_db(None), EMPTY_TOP_DB);
        assert_eq!(top_db(Some(f64::NAN)), EMPTY_TOP_DB);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn db_grid_stays_in_view_and_on_step_multiples() {
        assert_eq!(
            db_grid(10.0, 50.0, 10.0),
            [-40.0, -30.0, -20.0, -10.0, 0.0, 10.0]
        );
        let grid = db_grid(97.0, 50.0, 5.0);
        assert!(grid.iter().all(|db| (47.0..=97.0).contains(db)));
        assert_eq!(grid.first(), Some(&50.0));
        assert_eq!(grid.last(), Some(&95.0));
        assert_eq!(db_tick(12.0), "+12");
        assert_eq!(db_tick(-24.0), "-24");
    }

    // The same curve keeps its shape at any window size: the pixels per dB
    // stay a fixed share of the pixels per octave.
    // Bugs caught: 0, False positives: 0
    #[test]
    fn db_span_follows_the_plot_shape() {
        let none = Insets {
            left: 0.0,
            right: 0.0,
            top: 0.0,
            bottom: 0.0,
        };
        assert!((PlotBox::inside(1000.0, 500.0, none).span - 50.0).abs() < 1e-9);
        for (w, h) in [(1000.0, 500.0), (2400.0, 400.0), (500.0, 900.0)] {
            let p = PlotBox::inside(w, h, none);
            let px_per_db = (p.y_of(-10.0, 0.0) - p.y_of(0.0, 0.0)) / 10.0;
            let px_per_decade = p.x_of(200.0) - p.x_of(20.0);
            assert!((px_per_decade / px_per_db - 33.333).abs() < 0.01, "{w}x{h}");
        }
    }

    /// The labelled lines' multiples (in tenths) on an axis from `low` to
    /// `high` Hz, `width` pixels wide, in 11 px labels.
    fn labelled(low: f64, high: f64, width: f32) -> Vec<u32> {
        let lines = freq_grid(low, high, width);
        let labelled = freq_labelled(&lines, low, high, width, 11.0);
        let mut tenths: Vec<u32> = lines
            .iter()
            .zip(labelled)
            .filter(|(_, l)| *l)
            .map(|(l, _)| l.tenths)
            .collect();
        tenths.sort_unstable();
        tenths.dedup();
        tenths
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_wide_axis_labels_the_minor_lines_too() {
        assert_eq!(
            labelled(F_MIN, F_MAX, 3000.0),
            [10, 20, 30, 40, 50, 60, 70, 80, 90]
        );
        // A typical plot labels what fits: 6 would crowd 7.
        assert_eq!(labelled(F_MIN, F_MAX, 1000.0), [10, 20, 30, 40, 50, 70]);
        // A small plot keeps 1, 2 and 5 only.
        assert_eq!(labelled(F_MIN, F_MAX, 300.0), [10, 20, 50]);
        for width in [300.0, 1000.0, 3000.0] {
            let lines = freq_grid(F_MIN, F_MAX, width);
            let tiers = lines
                .iter()
                .zip(freq_labelled(&lines, F_MIN, F_MAX, width, 11.0))
                .filter(|(_, l)| *l)
                .map(|(l, _)| label_tier(l.tenths))
                .max()
                .unwrap();
            assert!(labels_fit(&lines, tiers + 1, F_MIN, F_MAX, width, 11.0));
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn frequency_grid_marks_decades_and_one_two_five_lines() {
        let grid = freq_grid(F_MIN, F_MAX, 1000.0);
        let at = |tick| {
            grid.iter()
                .filter(|l| l.tick >= tick)
                .map(|l| l.freq)
                .collect::<Vec<_>>()
        };
        assert_eq!(at(Tick::Decade), [100.0, 1000.0, 10_000.0]);
        assert_eq!(
            at(Tick::Major),
            [
                20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10_000.0, 20_000.0
            ]
        );
        // No fine lines at the full range, even on a wide plot.
        assert_eq!(grid.len(), 28);
        assert_eq!(freq_grid(F_MIN, F_MAX, 2400.0).len(), 28);
        assert_eq!(freq_tick(10_000.0), "10k");
        assert_eq!(freq_label(1234.0), "1.23 kHz");
    }

    // A zoomed axis has lines and labels in view only, the tenths between
    // multiples where there is room, and labels that stand apart.
    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_zoomed_frequency_axis_has_lines_in_view_only() {
        // A third of an octave from 1 kHz: tenths of the kHz.
        let (low, high) = (1000.0, 1000.0 * 2f64.powf(1.0 / 3.0));
        let grid = freq_grid(low, high, 900.0);
        let freqs: Vec<f64> = grid.iter().map(|l| l.freq).collect();
        assert_eq!(freqs, [1000.0, 1100.0, 1200.0]);
        assert_eq!(grid[1].tick, Tick::Fine);
        assert_eq!(labelled(low, high, 900.0), [10, 11, 12]);
        assert_eq!(freq_tick(1100.0), "1.1k");
        // Two octaves at 100 Hz: tenths only where they stand apart, which
        // is the low multiples first.
        let grid = freq_grid(100.0, 400.0, 900.0);
        assert!(grid.iter().all(|l| (100.0..=400.0).contains(&l.freq)));
        assert!(grid.iter().any(|l| l.freq == 110.0));
        assert!(!grid.iter().any(|l| l.freq == 390.0));
        let lines = freq_grid(100.0, 400.0, 900.0);
        let labelled = freq_labelled(&lines, 100.0, 400.0, 900.0, 11.0);
        let shown: Vec<FreqLine> = lines
            .iter()
            .zip(labelled)
            .filter(|(_, l)| *l)
            .map(|(l, _)| *l)
            .collect();
        assert!(shown.len() >= 4, "{shown:?}");
        // A line on the edge survives the rounding of a zoom.
        let edge = 10f64.powf(F_MIN.log10());
        assert_eq!(freq_grid(edge, 30.0, 900.0)[0].freq, 20.0);
    }
    // A narrow stretch between two multiples, too tight for its tenths,
    // still gets one line: the tenth nearest its middle.
    // Bugs caught: 1, False positives: 0
    #[test]
    fn a_narrow_zoom_keeps_one_frequency_line() {
        let (low, high) = (1100.0, 1850.0);
        let grid = freq_grid(low, high, 200.0);
        assert_eq!(grid.len(), 1, "{grid:?}");
        assert_eq!((grid[0].freq, grid[0].tenths), (1400.0, 14));
        assert_eq!(labelled(low, high, 200.0), [14]);
        // At every place a third of an octave can be, the line is in view.
        let third = 2f64.powf(1.0 / 3.0);
        for k in 0..300 {
            let low = F_MIN * (F_MAX / F_MIN / third).powf(f64::from(k) / 299.0);
            let grid = freq_grid(low, low * third, 60.0);
            assert!(!grid.is_empty(), "{low}");
            let inside = low * (1.0 - 1e-9)..=low * third * (1.0 + 1e-9);
            assert!(grid.iter().all(|l| inside.contains(&l.freq)), "{low}");
        }
    }

    // The edges of the full range read exactly 20 Hz and 20 kHz however
    // the axis was zoomed, so a trace ending at 20 kHz is read there.
    // Bugs caught: 1, False positives: 0
    #[test]
    fn the_global_edges_stay_exact_on_a_zoomed_axis() {
        let p = plot();
        let (lo, hi) = (F_MIN.log10(), F_MAX.log10());
        for low in [lo, 2.0, 3.5, 4.2] {
            let z = p.with_decades(low, hi);
            assert_eq!(z.f_high, F_MAX, "{low}");
            assert_eq!(z.freq_at(z.right()), F_MAX, "{low}");
        }
        for high in [hi, 3.0, 1.6] {
            let z = p.with_decades(lo, high);
            assert_eq!(z.f_low, F_MIN, "{high}");
            assert_eq!(z.freq_at(z.left), F_MIN, "{high}");
        }
        // Zoomed in at the right edge, in steps.
        let limits = crate::zoom::Limits::frequency();
        let mut range = limits.default;
        for lines in [1.0, 2.5, 0.7, 4.0] {
            range = crate::zoom::zoom(range, hi, crate::zoom::factor(lines), &limits).unwrap();
            let z = p.with_decades(range.low, range.high);
            assert!(z.f_high <= F_MAX, "{range:?}");
            let edge = z.freq_at(z.right());
            assert!(edge <= F_MAX && edge / F_MAX > 1.0 - 1e-12, "{edge}");
        }
    }
}
