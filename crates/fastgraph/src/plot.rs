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
        let t = (freq.max(1e-9) / F_MIN).log10() / (F_MAX / F_MIN).log10();
        self.left + (t as f32) * self.width
    }

    /// Clamped to the plotted decades.
    pub fn freq_at(&self, x: f32) -> f64 {
        let t = (f64::from((x - self.left) / self.width)).clamp(0.0, 1.0);
        F_MIN * (F_MAX / F_MIN).powf(t)
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
    /// A 3, 4, 6, 7, 8 or 9 multiple.
    Minor,
    /// A 2 or 5 multiple.
    Major,
    /// 100 Hz, 1 kHz, 10 kHz.
    Decade,
}

/// Every 1..9 multiple per decade in view, with its prominence.
pub fn freq_grid() -> Vec<(f64, Tick)> {
    let mut out = Vec::new();
    for decade in [10.0, 100.0, 1000.0, 10_000.0] {
        for m in 1..=9 {
            let f = decade * f64::from(m);
            if (F_MIN..=F_MAX).contains(&f) {
                let tick = match m {
                    1 => Tick::Decade,
                    2 | 5 => Tick::Major,
                    _ => Tick::Minor,
                };
                out.push((f, tick));
            }
        }
    }
    out
}

/// The 1..9 multiple of a power of ten that `f` is: 3 for 300 Hz.
pub fn multiple(f: f64) -> u32 {
    (f / 10f64.powf(f.log10().floor())).round() as u32
}

/// The advance of the mono face, as a share of its size.
const MONO_ADVANCE: f32 = 0.62;
/// The least room between two labels side by side, in pixels.
const LABEL_PAD: f32 = 6.0;
/// The multiples labelled beyond 1, 2 and 5, in the order they are added:
/// each leaves more room than the next.
const EXTRA_LABELS: [u32; 6] = [3, 7, 4, 6, 8, 9];

/// The multiples whose lines a frequency axis `width` pixels wide labels,
/// in labels `size` pixels tall: 1, 2 and 5, then 3, 7, 4, 6, 8 and 9 in
/// turn while every label keeps room from its neighbours.
pub fn freq_label_multiples(width: f32, size: f32) -> Vec<u32> {
    let mut labelled = vec![1, 2, 5];
    for extra in EXTRA_LABELS {
        labelled.push(extra);
        if !labels_fit(&labelled, width, size) {
            labelled.pop();
            break;
        }
    }
    labelled
}

/// Whether the labels of the `multiples` lines on an axis `width` pixels
/// wide stand apart.
fn labels_fit(multiples: &[u32], width: f32, size: f32) -> bool {
    let decades = (F_MAX / F_MIN).log10();
    let placed: Vec<(f32, f32)> = freq_grid()
        .into_iter()
        .filter(|(f, _)| multiples.contains(&multiple(*f)))
        .map(|(f, _)| {
            let x = ((f / F_MIN).log10() / decades) as f32 * width;
            let w = freq_tick(f).chars().count() as f32 * MONO_ADVANCE * size;
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

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_wide_axis_labels_the_minor_lines_too() {
        assert_eq!(multiple(300.0), 3);
        assert_eq!(multiple(20_000.0), 2);
        assert_eq!(multiple(7000.0), 7);
        let all = freq_label_multiples(3000.0, 11.0);
        assert_eq!(all, [1, 2, 5, 3, 7, 4, 6, 8, 9]);
        // A typical plot labels what fits: 6 would crowd 7.
        assert_eq!(freq_label_multiples(1000.0, 11.0), [1, 2, 5, 3, 7, 4]);
        // A small plot keeps 1, 2 and 5 only.
        assert_eq!(freq_label_multiples(300.0, 11.0), [1, 2, 5]);
        for width in [300.0, 1000.0, 3000.0] {
            assert!(labels_fit(&freq_label_multiples(width, 11.0), width, 11.0));
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn frequency_grid_marks_decades_and_one_two_five_lines() {
        let grid = freq_grid();
        let at = |tick| {
            grid.iter()
                .filter(|(_, t)| *t >= tick)
                .map(|(f, _)| *f)
                .collect::<Vec<_>>()
        };
        assert_eq!(at(Tick::Decade), [100.0, 1000.0, 10_000.0]);
        assert_eq!(
            at(Tick::Major),
            [
                20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10_000.0, 20_000.0
            ]
        );
        assert_eq!(grid.len(), 28);
        assert_eq!(freq_tick(10_000.0), "10k");
        assert_eq!(freq_label(1234.0), "1.23 kHz");
    }
}
