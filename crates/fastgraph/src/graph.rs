//! The response graph: the shown measurements on a 20 Hz–20 kHz log grid
//! with a dB axis topped by the loudest trace and scaled to the plot's shape.
//! Its Phase view plots their phase instead, unwrapped when it all fits 720° and wrapped to
//! ±180° otherwise, and its Distortion view
//! the harmonics, THD and noise floor of one measurement in % or on a dB axis that holds them
//! all.
//!
//! The wheel over an axis's labels zooms that axis, each view on its own
//! (see [`crate::zoom`]).
//!
//! As in fastpeq-gui's graph, the grid and the traces are cached and redrawn
//! only when what they show ([`Graph::set`]) or the canvas size changes; the
//! hover crosshair and readout are drawn every frame on their own layer.
use crate::plot::{self, Insets, PlotBox, Tick};
use crate::theme::{self, Palette};
use crate::zoom::{self, Range, ViewZoom};
use fastgraph_core::response::Response;
use iced::advanced::mouse::{self, Cursor};
use iced::alignment::Vertical;
use iced::widget::canvas::{self, Cache, Frame, Geometry, Path, Stroke, Text};
use iced::widget::text::Alignment;
use iced::widget::{Canvas, Stack, button, container, text};
use iced::{Element, Length, Point, Rectangle, Renderer, Size, Theme};
use std::borrow::Cow;
use std::cell::Cell;

/// Shown on an empty graph.
pub const EMPTY: &str = "No measurements yet — Measure, Listen, or Import";
/// Shown on an empty graph while the list has measurements, all hidden.
pub const ALL_HIDDEN: &str = "Show a measurement, or use Measure, Listen, or Import";

/// The most dB the plot shape alone requests for the Distortion view.
const MAX_DB_SPAN: f64 = 160.0;

/// Room for the axis labels.
pub(crate) const INSETS: Insets = Insets {
    left: 44.0,
    right: 16.0,
    top: 16.0,
    bottom: 26.0,
};
/// Degrees between horizontal phase lines, and between the brighter ones,
/// labelled unless the plot is too short for that.
const PHASE_LINES: f64 = 45.0;
const PHASE_MAJORS: f64 = 90.0;
/// The wrapped phase axis runs from +180° at the top to −180° at the bottom.
const PHASE_TOP: f64 = 180.0;
const PHASE_SPAN: f64 = 360.0;
/// The widest range of unwrapped phase drawn unwrapped; wider, the view
/// wraps it.
const MAX_UNWRAPPED_SPAN: f64 = 720.0;
/// The unwrapped axis's ends are on multiples of this.
const PHASE_AXIS_STEP: f64 = 90.0;
/// The frequency an unwrapped trace is anchored at: its phase there is in
/// (−180°, 180°].
const PHASE_ANCHOR_HZ: f64 = 1000.0;
/// dB between horizontal grid lines, and between labelled ones.
const DB_LINES: f64 = 5.0;
const DB_LABELS: f64 = 10.0;
const LABEL_SIZE: f32 = 11.0;
/// The least room between two stacked axis labels' lines.
const LABEL_ROOM: f32 = 2.2 * LABEL_SIZE;
const TRACE_WIDTH: f32 = 1.5;

/// What the graph plots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    #[default]
    Response,
    Phase,
    Distortion,
}

/// The graph's tabs.
pub const VIEWS: [(View, &str); 3] = [
    (View::Response, "Response"),
    (View::Phase, "Phase"),
    (View::Distortion, "Distortion"),
];

/// The tab index of `view`.
pub fn index(view: View) -> usize {
    VIEWS.iter().position(|&(v, _)| v == view).unwrap_or(0)
}

/// Shown in the Distortion view when no shown measurement has distortion data.
pub const NO_DISTORTION: &str = "Distortion needs a sweep measured with this version";

/// The Distortion view's units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Units {
    /// Percent of the fundamental, on a log axis.
    #[default]
    Percent,
    /// Absolute levels on the dB axis.
    Db,
}

/// The units toggle's segments.
pub const UNITS: [(Units, &str); 2] = [(Units::Percent, "%"), (Units::Db, "dB")];

/// The percent axis runs from 100 % (2 decades) at the top, 4 decades down
/// to 0.01 % at the bottom.
const PCT_TOP: f64 = 2.0;
const PCT_SPAN: f64 = 4.0;
/// The line width of the THD curve, and the room between curve labels.
const THD_WIDTH: f32 = 2.5;
const LABEL_GAP: f32 = LABEL_SIZE + 2.0;

/// The y of `percent` on the percent axis `plot` (its span in decades)
/// topped at the decade `top`, values beyond pinned.
fn percent_y(plot: &PlotBox, top: f64, percent: f64) -> f32 {
    plot.y_of(percent_axis(percent), top)
        .clamp(plot.top, plot.bottom())
}

/// A percentage as its place on the percent axis: its decade. `NaN` (a
/// gap) stays `NaN`; zero is clamped to a tiny value.
fn percent_axis(percent: f64) -> f64 {
    if percent.is_nan() {
        return percent;
    }
    percent.max(1e-12).log10()
}

/// What colour a distortion curve takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ink {
    /// A measurement trace colour.
    Trace(usize),
    /// The foreground colour.
    Fg,
    /// The dim foreground.
    Dim,
}

/// One curve of the Distortion view over the response grid.
#[derive(Debug, Clone)]
pub struct Curve {
    pub label: String,
    pub ink: Ink,
    pub dashed: bool,
    pub thick: bool,
    /// Whether its name is written at its right end.
    pub labelled: bool,
    pub freq: Vec<f64>,
    /// Its place on the axis of the view's units: dB, or the decade of the
    /// percentage. `NaN` where it has no data.
    pub y: Vec<f64>,
}

impl Curve {
    /// A curve of `level` re `fundamental` (on the same grid) in `units`.
    pub fn new(
        label: &str,
        ink: Ink,
        level: &Response,
        fundamental: &Response,
        units: Units,
    ) -> Curve {
        let y = match units {
            Units::Db => level.db.clone(),
            Units::Percent => fastgraph_core::distortion::percent_of(level, fundamental)
                .into_iter()
                .map(percent_axis)
                .collect(),
        };
        Curve {
            label: label.into(),
            ink,
            dashed: false,
            thick: false,
            labelled: false,
            freq: level.freq.clone(),
            y,
        }
    }

    /// The place on the axis at `f`, interpolated in log f; `NaN` outside
    /// the data or beside a gap.
    fn at(&self, f: f64) -> f64 {
        let (Some(&first), Some(&last)) = (self.freq.first(), self.freq.last()) else {
            return f64::NAN;
        };
        if !(first..=last).contains(&f) {
            return f64::NAN;
        }
        let hi = self.freq.partition_point(|x| *x < f);
        if self.freq[hi] == f || hi == 0 {
            return self.y[hi];
        }
        let (a, b) = (self.y[hi - 1], self.y[hi]);
        let t = (f / self.freq[hi - 1]).ln() / (self.freq[hi] / self.freq[hi - 1]).ln();
        a + t * (b - a)
    }

    /// The points inside the plotted decades.
    pub(crate) fn plotted(&self) -> impl Iterator<Item = (f64, f64)> + '_ {
        self.freq
            .iter()
            .zip(&self.y)
            .filter(|(f, _)| (plot::F_MIN..=plot::F_MAX).contains(*f))
            .map(|(f, y)| (*f, *y))
    }
}

/// What the Distortion view shows: the curves of one measurement.
#[derive(Debug, Clone)]
pub struct DistortionPlot {
    /// The measurement's name.
    pub name: String,
    pub units: Units,
    pub curves: Vec<Curve>,
}

impl DistortionPlot {
    /// A level on the axis, as the readout writes it: "0.042 %", "-52.1 dB".
    fn reading(&self, y: f64) -> String {
        match self.units {
            Units::Db => format!("{y:+.1} dB"),
            Units::Percent => {
                let pct = 10f64.powf(y);
                match pct {
                    p if p >= 10.0 => format!("{p:.1} %"),
                    p if p >= 1.0 => format!("{p:.2} %"),
                    p if p >= 0.001 => format!("{p:.3} %"),
                    p => format!("{p:.1e} %"),
                }
            }
        }
    }

    /// The curve closest in level to `y` at `freq`, with its place there.
    fn nearest(&self, freq: f64, y: f64) -> Option<(&Curve, f64)> {
        self.curves
            .iter()
            .filter_map(|c| {
                let at = c.at(freq);
                at.is_finite().then_some((c, at))
            })
            .min_by(|a, b| (a.1 - y).abs().total_cmp(&(b.1 - y).abs()))
    }
}

/// The note for shown traces the Phase view cannot draw.
pub fn no_phase_note(n: usize) -> String {
    match n {
        1 => "1 shown trace has no phase".into(),
        n => format!("{n} shown traces have no phase"),
    }
}

/// Where the pointer is over the plot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Hover {
    /// The frequency and level under the pointer.
    At {
        freq: f64,
        db: f64,
    },
    Left,
}

/// What the canvas reports.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event {
    Hover(Hover),
    /// The wheel turned over an axis's labels.
    Wheel(Wheel),
    /// Reset view was pressed: the view back to its automatic axes.
    Reset,
}

/// A graph axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Frequency,
    /// The vertical one: dB, degrees or percent.
    Level,
}

/// A wheel scroll over an axis: it zooms that axis about the pointer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Wheel {
    pub axis: Axis,
    /// The plot it turned over, before any zoom: the default axes follow
    /// its shape.
    pub plot: PlotBox,
    /// The pointer's place along the axis: x for frequency, y for level.
    pub at: f32,
    /// Notches, positive (up) zooming in.
    pub lines: f64,
}

/// The least span a level axis zooms in to: 5 dB, 10° or a decade of
/// percent.
const MIN_DB_SPAN: f64 = 5.0;
const MIN_PHASE_SPAN: f64 = 10.0;
const MIN_PCT_SPAN: f64 = 1.0;

/// One shown measurement: its display response and colour index.
#[derive(Debug, Clone)]
pub struct Trace {
    /// The measurement's name; `Graph::set` turns it into the hover readout's
    /// display name (one line, cut short), so it is prepared once per rebuild.
    pub name: String,
    pub response: Response,
    pub color: usize,
}

/// One shown measurement's phase in degrees (wrapped to ±180°) over `freq`.
#[derive(Debug, Clone)]
pub struct PhaseTrace {
    pub freq: Vec<f64>,
    pub deg: Vec<f64>,
    pub color: usize,
}

impl PhaseTrace {
    /// The phase at `f`: a point's own, else interpolated in log f along the
    /// short arc the line is drawn on, wrapped back to (-180, 180]. `NaN`
    /// outside the data or beside a gap.
    fn at(&self, f: f64) -> f64 {
        let (Some(&first), Some(&last)) = (self.freq.first(), self.freq.last()) else {
            return f64::NAN;
        };
        if !(first..=last).contains(&f) {
            return f64::NAN;
        }
        let hi = self.freq.partition_point(|x| *x < f);
        if self.freq[hi] == f {
            return self.deg[hi];
        }
        let (a, b) = (self.deg[hi - 1], self.deg[hi]);
        let t = (f / self.freq[hi - 1]).ln() / (self.freq[hi] / self.freq[hi - 1]).ln();
        let delta = b - a;
        if delta.abs() <= 180.0 {
            return a + t * delta;
        }
        let phase = a + t * (delta - 360.0 * delta.signum());
        if phase.abs() > 180.0 || phase == -180.0 {
            phase - 360.0 * phase.signum()
        } else {
            phase
        }
    }

    /// The points inside the plotted decades, with the values at the two
    /// ends.
    fn plotted(&self) -> Vec<(f64, f64)> {
        let inside = self
            .freq
            .iter()
            .zip(&self.deg)
            .filter(|(f, _)| plot::F_MIN < **f && **f < plot::F_MAX)
            .map(|(f, d)| (*f, *d));
        std::iter::once((plot::F_MIN, self.at(plot::F_MIN)))
            .chain(inside)
            .chain(std::iter::once((plot::F_MAX, self.at(plot::F_MAX))))
            .collect()
    }
}

/// `points` cut into the runs that can be joined by a line: a break at
/// every gap (`NaN`) and wherever the phase jumps by more than 180°, which
/// is a wrap, not a swing. At a wrap the run ending runs on to the ±180°
/// edge it crosses and the next starts on the opposite edge, at the
/// crossing's frequency (interpolated in log f on the unwrapped line), so
/// the trace has no gap there and no point is left alone.
fn phase_segments(points: &[(f64, f64)]) -> Vec<Vec<(f64, f64)>> {
    let mut runs: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut last: Option<(f64, f64)> = None;
    for &(f, deg) in points {
        if !deg.is_finite() {
            last = None;
            continue;
        }
        match (last, runs.last_mut()) {
            (Some((_, before)), Some(run)) if (deg - before).abs() <= 180.0 => run.push((f, deg)),
            (Some((f0, before)), Some(run)) => {
                // Up through +180°, or down through −180°.
                let (edge, unwrapped) = if before > 0.0 {
                    (180.0, deg + 360.0)
                } else {
                    (-180.0, deg - 360.0)
                };
                let t = (edge - before) / (unwrapped - before);
                let crossing = f0 * (f / f0).powf(t);
                run.push((crossing, edge));
                runs.push(vec![(crossing, -edge), (f, deg)]);
            }
            _ => runs.push(vec![(f, deg)]),
        }
        last = Some((f, deg));
    }
    runs
}

/// The phase trace closest in degrees to `deg` at `freq`, with its phase
/// there; around the wrap when the traces are `wrapped`.
fn nearest_phase(
    traces: &[PhaseTrace],
    freq: f64,
    deg: f64,
    wrapped: bool,
) -> Option<(&PhaseTrace, f64)> {
    let gap = |at: f64| {
        if wrapped {
            circular_gap(at, deg)
        } else {
            (at - deg).abs()
        }
    };
    traces
        .iter()
        .filter_map(|t| {
            let at = t.at(freq);
            at.is_finite().then_some((t, at))
        })
        .min_by(|a, b| gap(a.1).total_cmp(&gap(b.1)))
}

/// The angle between two phases, wrap included: 179° and −179° are 2° apart.
fn circular_gap(a: f64, b: f64) -> f64 {
    (((a - b + 540.0) % 360.0) - 180.0).abs()
}

/// `deg` (wrapped, over `freq`) made continuous: a point more than 180° from
/// the finite point before it, across gaps (`NaN`, kept) too, moves by whole
/// turns to lie within 180° of it; an exact half-turn stays put. The whole curve then moves by whole turns so that its
/// point nearest 1 kHz lies in (−180°, 180°].
pub fn unwrap_phase(freq: &[f64], deg: &[f64]) -> Vec<f64> {
    let mut out = Vec::with_capacity(deg.len());
    let mut last: Option<f64> = None;
    for &d in deg {
        if !d.is_finite() {
            out.push(f64::NAN);
            continue;
        }
        let at = match last {
            Some(before) if (d - before).abs() > 180.0 => {
                d - 360.0 * ((d - before) / 360.0).round()
            }
            _ => d,
        };
        out.push(at);
        last = Some(at);
    }
    let anchor = freq
        .iter()
        .zip(&out)
        .filter(|(_, d)| d.is_finite())
        .min_by(|a, b| {
            let off = |f: f64| (f / PHASE_ANCHOR_HZ).ln().abs();
            off(*a.0).total_cmp(&off(*b.0))
        })
        .map(|(_, d)| *d);
    if let Some(anchor) = anchor {
        // The turns that bring the anchor into (−180, 180].
        let turns = -((anchor - 180.0) / 360.0).ceil();
        for d in &mut out {
            *d += 360.0 * turns;
        }
    }
    out
}

/// How the Phase view draws phase.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PhaseAxis {
    /// Wrapped to ±180°, broken at the wraps.
    Wrapped,
    /// Unwrapped, on an axis from `top` down `span` degrees.
    Unwrapped { top: f64, span: f64 },
}

impl PhaseAxis {
    /// The axis for these unwrapped traces: unwrapped when they all fit
    /// [`MAX_UNWRAPPED_SPAN`], on 90° multiples at least 360° apart.
    fn of(unwrapped: &[PhaseTrace]) -> Self {
        let Some((low, high)) = unwrapped
            .iter()
            .flat_map(|t| t.deg.iter().copied())
            .filter(|d| d.is_finite())
            .map(|d| (d, d))
            .reduce(|a, b| (a.0.min(b.0), a.1.max(b.1)))
        else {
            return Self::Wrapped;
        };
        if high - low > MAX_UNWRAPPED_SPAN {
            return Self::Wrapped;
        }
        let bottom = (low / PHASE_AXIS_STEP).floor() * PHASE_AXIS_STEP;
        let top = (high / PHASE_AXIS_STEP).ceil() * PHASE_AXIS_STEP;
        // Short ranges grow to a turn about their middle, in whole 90° steps.
        let span = (top - bottom).max(PHASE_SPAN);
        let steps = ((span - (top - bottom)) / PHASE_AXIS_STEP / 2.0).floor();
        Self::Unwrapped {
            top: top + steps * PHASE_AXIS_STEP,
            span,
        }
    }

    /// The degrees at the axis's top, and its span.
    fn range(self) -> (f64, f64) {
        match self {
            Self::Wrapped => (PHASE_TOP, PHASE_SPAN),
            Self::Unwrapped { top, span } => (top, span),
        }
    }

    /// The caption naming the mode.
    pub fn caption(self) -> &'static str {
        match self {
            Self::Wrapped => "Phase · wrapped",
            Self::Unwrapped { .. } => "Phase · unwrapped",
        }
    }
}

/// `trace` over the plotted decades, unwrapped.
fn unwrapped(trace: &PhaseTrace) -> PhaseTrace {
    let (freq, deg): (Vec<f64>, Vec<f64>) = trace.plotted().into_iter().unzip();
    let deg = unwrap_phase(&freq, &deg);
    PhaseTrace {
        freq,
        deg,
        color: trace.color,
    }
}

/// What the graph shows, and its cached layer.
pub struct Graph {
    view: View,
    traces: Vec<Trace>,
    /// The Phase view's traces, and how many shown measurements it cannot
    /// draw for lack of phase.
    phase: Vec<PhaseTrace>,
    no_phase: usize,
    /// The phase traces unwrapped over the plotted decades, and how the
    /// view draws them.
    unwrapped: Vec<PhaseTrace>,
    phase_axis: PhaseAxis,
    /// The Distortion view's curves, if a shown measurement has any.
    distortion: Option<DistortionPlot>,
    /// The RTA's running average, dashed in the foreground colour.
    live: Option<Response>,
    /// The quietest and loudest levels shown, within the plotted decades.
    extent: Option<(f64, f64)>,
    /// The dB span of the last draw, so an update can tell whether it moves
    /// the axis as drawn.
    drawn_span: Cell<f64>,
    /// What the Distortion view says when it has no curves.
    distortion_note: String,
    /// Each view's zoom, by tab index.
    zooms: [ViewZoom; VIEWS.len()],
    cache: Cache,
    /// The live trace's own layer, cleared on every RTA update.
    live_cache: Cache,
    /// How many times the cached layer was drawn.
    #[cfg(test)]
    pub(crate) drawn: Cell<usize>,
}

impl Default for Graph {
    fn default() -> Self {
        Self {
            view: View::Response,
            traces: Vec::new(),
            phase: Vec::new(),
            no_phase: 0,
            unwrapped: Vec::new(),
            phase_axis: PhaseAxis::Wrapped,
            distortion: None,
            live: None,
            extent: None,
            drawn_span: Cell::new(0.0),
            distortion_note: NO_DISTORTION.to_string(),
            zooms: Default::default(),
            cache: Cache::new(),
            live_cache: Cache::new(),
            #[cfg(test)]
            drawn: Cell::new(0),
        }
    }
}

impl Graph {
    /// Switch the view, redrawing the cached layer.
    pub fn set_view(&mut self, view: View) {
        if self.view != view {
            self.view = view;
            self.cache.clear();
            self.live_cache.clear();
        }
    }

    /// Show these phase traces, with the number of shown measurements left
    /// out for lack of phase.
    pub fn set_phase(&mut self, traces: Vec<PhaseTrace>, no_phase: usize) {
        self.unwrapped = traces.iter().map(unwrapped).collect();
        self.phase_axis = PhaseAxis::of(&self.unwrapped);
        self.phase = traces;
        self.no_phase = no_phase;
        self.cache.clear();
    }

    /// Show these distortion curves (or none), rescaling the dB axis to them.
    pub fn set_distortion(&mut self, plot: Option<DistortionPlot>) {
        self.distortion = plot;
        self.rescale();
        self.cache.clear();
    }

    /// Set what the Distortion view says when it has no curves.
    pub fn set_distortion_note(&mut self, note: String) {
        self.distortion_note = note;
    }

    /// The labels of the distortion curves.
    #[cfg(test)]
    pub fn curve_labels(&self) -> Vec<String> {
        self.distortion
            .iter()
            .flat_map(|d| d.curves.iter().map(|c| c.label.clone()))
            .collect()
    }

    /// The phase traces as drawn, and whether they are wrapped.
    fn phase_shown(&self) -> (&[PhaseTrace], bool) {
        match self.phase_axis {
            PhaseAxis::Wrapped => (&self.phase, true),
            PhaseAxis::Unwrapped { .. } => (&self.unwrapped, false),
        }
    }

    /// How many response traces are set.
    #[cfg(test)]
    pub fn trace_count(&self) -> usize {
        self.traces.len()
    }

    /// How many phase traces are set.
    #[cfg(test)]
    pub fn phase_count(&self) -> usize {
        self.phase.len()
    }

    /// The plot with its axes as this view draws them, zoomed or not, and
    /// the value at its top: dB, degrees, or the decade of a percentage.
    pub(crate) fn axis(&self, plot: PlotBox) -> (PlotBox, f64) {
        let zoom = self.zoom();
        let plot = match zoom.freq {
            Some(r) => plot.with_decades(r.low, r.high),
            None => plot,
        };
        let (plot, top) = self.default_axis(plot);
        match zoom.level {
            // Within this size's limits, about the saved centre, so a range
            // saved at another size is drawn as the wheel would leave it.
            Some(r) => {
                let span = r
                    .span()
                    .max(self.least_level_span(plot.span))
                    .min(zoom::LEVEL_OUT * plot.span);
                let top = r.high + (span - r.span()) / 2.0;
                (PlotBox { span, ..plot }, top)
            }
            None => (plot, top),
        }
    }

    /// Whether traces beyond the level axis are pinned to its edge, as on
    /// the automatic fit, rather than cut, as on a zoomed axis.
    fn pins(&self) -> bool {
        self.zoom().level.is_none()
    }

    /// The current view's zoom.
    fn zoom(&self) -> ViewZoom {
        self.zooms[index(self.view)]
    }

    /// Whether the current view is zoomed: Reset view has something to do.
    pub fn is_zoomed(&self) -> bool {
        self.zoom().is_zoomed()
    }

    /// Whether any view is zoomed.
    pub fn any_zoomed(&self) -> bool {
        self.zooms.iter().any(ViewZoom::is_zoomed)
    }

    /// Reset view: the current view's axes back to their automatic fit.
    pub fn reset_view(&mut self) {
        self.set_zoom(self.view, ViewZoom::default());
    }

    /// Every view's axes back to their automatic fit.
    pub fn reset_zoom(&mut self) {
        for view in VIEWS.map(|(v, _)| v) {
            self.set_zoom(view, ViewZoom::default());
        }
    }

    /// `view`'s level axis back to its fit, as when its units change.
    pub fn reset_level(&mut self, view: View) {
        let zoom = ViewZoom {
            level: None,
            ..self.zooms[index(view)]
        };
        self.set_zoom(view, zoom);
    }

    fn set_zoom(&mut self, view: View, zoom: ViewZoom) {
        let slot = &mut self.zooms[index(view)];
        if *slot != zoom {
            *slot = zoom;
            if view == self.view {
                self.cache.clear();
                self.live_cache.clear();
            }
        }
    }

    /// Zoom the axis the wheel turned over, about the value under the
    /// pointer, from the range drawn. An empty Distortion view has no level
    /// axis to zoom.
    pub fn wheel(&mut self, wheel: Wheel) {
        if wheel.axis == Axis::Level && self.view == View::Distortion && self.distortion.is_none() {
            return;
        }
        let (shown, top) = self.axis(wheel.plot);
        let factor = zoom::factor(wheel.lines);
        let mut zoomed = self.zoom();
        match wheel.axis {
            Axis::Frequency => {
                let limits = zoom::Limits::frequency();
                let range = zoomed.freq.unwrap_or(limits.default);
                let at = shown.freq_at(wheel.at).log10();
                zoomed.freq = zoom::zoom(range, at, factor, &limits);
            }
            Axis::Level => {
                let limits = self.level_limits(wheel.plot);
                let range = Range::new(top - shown.span, top);
                let at = shown.db_at(wheel.at, top);
                zoomed.level = zoom::zoom(range, at, factor, &limits);
            }
        }
        self.set_zoom(self.view, zoomed);
    }

    /// How far the current view's level axis zooms on `plot`.
    fn level_limits(&self, plot: PlotBox) -> zoom::Limits {
        let (fit, top) = self.default_axis(plot);
        let least = self.least_level_span(fit.span);
        zoom::Limits::level(Range::new(top - fit.span, top), least)
    }

    /// The least span the current view's level axis zooms in to, for an
    /// automatic fit of `fit_span`: a tighter fit is itself the least.
    fn least_level_span(&self, fit_span: f64) -> f64 {
        let least = match self.view {
            View::Phase => MIN_PHASE_SPAN,
            View::Distortion if self.units() == Units::Percent => MIN_PCT_SPAN,
            View::Response | View::Distortion => MIN_DB_SPAN,
        };
        least.min(fit_span)
    }

    /// The plot with its vertical axis fitted as this view draws it
    /// unzoomed, and the value at its top: dB, or degrees (wrapped ±180° or
    /// the unwrapped fit), or the decade of a percentage.
    fn default_axis(&self, plot: PlotBox) -> (PlotBox, f64) {
        match self.view {
            View::Phase => {
                let (top, span) = self.phase_axis.range();
                (PlotBox { span, ..plot }, top)
            }
            View::Distortion if self.units() == Units::Percent => (
                PlotBox {
                    span: PCT_SPAN,
                    ..plot
                },
                PCT_TOP,
            ),
            View::Distortion => {
                let (span, top) = self.distortion_db_axis(plot.span);
                (PlotBox { span, ..plot }, top)
            }
            View::Response => (plot, self.top_db(plot.span)),
        }
    }

    /// The Distortion view's dB span and top for a plot whose shape gives
    /// `shape_span`: the top is the loudest level rounded up to 10 dB plus
    /// 5 dB of headroom, and the span grows to hold the quietest curve,
    /// since the fundamental and the noise floor lie far apart.
    fn distortion_db_axis(&self, shape_span: f64) -> (f64, f64) {
        let Some((quiet, loud)) = self.extent else {
            return (shape_span, self.top_db(shape_span));
        };
        let top = (loud / 10.0).ceil() * 10.0 + 5.0;
        let span = shape_span.min(MAX_DB_SPAN).max(top - quiet + 10.0);
        (span, top)
    }

    /// The Distortion view's units.
    fn units(&self) -> Units {
        self.distortion.as_ref().map_or(Units::Percent, |d| d.units)
    }

    /// Show `traces`, rescaling the dB axis to them.
    pub fn set(&mut self, mut traces: Vec<Trace>) {
        for t in &mut traces {
            t.name = readout_name(&t.name);
        }
        self.traces = traces;
        self.rescale();
        self.cache.clear();
    }

    /// The colours changed: redraw the traces in the new ones.
    pub fn colors_changed(&self) {
        self.cache.clear();
    }

    /// Show the RTA's average (or none). Only the live layer is redrawn,
    /// unless the dB axis moves with it.
    pub fn set_live(&mut self, live: Option<Response>) {
        self.live = live;
        self.live_cache.clear();
        // A zoomed level axis stays put.
        let held = self.view == View::Response && self.zoom().level.is_some();
        if self.rescale() && !held {
            self.cache.clear();
        }
    }

    /// Fit the dB axis to what is shown; whether its top moved at the span
    /// last drawn (a new size redraws anyway).
    fn rescale(&mut self) -> bool {
        let span = self.drawn_span.get();
        let before = self.top_db(span);
        // The distortion curves set the dB axis only in dB.
        let curves = self
            .distortion
            .iter()
            .filter(|d| d.units == Units::Db)
            .flat_map(|d| &d.curves)
            .filter_map(|c| {
                c.plotted()
                    .map(|(_, y)| y)
                    .filter(|y| y.is_finite())
                    .map(|y| (y, y))
                    .reduce(|a, b| (a.0.min(b.0), a.1.max(b.1)))
            });
        self.extent = self
            .traces
            .iter()
            .map(|t| &t.response)
            // The live trace is drawn in the Response view only.
            .chain(self.live.as_ref().filter(|_| self.view == View::Response))
            .filter_map(extent_of)
            .chain(curves)
            .reduce(|a, b| (a.0.min(b.0), a.1.max(b.1)));
        self.top_db(span) != before
    }

    /// The top of a `span` dB axis: what is shown sits centred (to the
    /// nearest step) when it fits with room to spare, else the loudest level
    /// sits just under the top, or mid-plot when the plot is too short for
    /// the headroom.
    pub fn top_db(&self, span: f64) -> f64 {
        let Some((quiet, loud)) = self.extent else {
            return plot::top_db(None);
        };
        let pinned = plot::top_db(Some(loud));
        if pinned - span <= quiet - plot::HEADROOM_DB {
            ((quiet + loud + span) / 2.0 / plot::HEADROOM_DB).round() * plot::HEADROOM_DB
        } else {
            pinned.min(loud + span / 2.0)
        }
    }

    /// The canvas, with a note over it when there is something to say and
    /// Reset view while the view is zoomed; `rows`: the list has
    /// measurements.
    pub fn view(&self, hover: Option<(f64, f64)>, rows: bool) -> Element<'_, Event> {
        let layers = self.layers(hover, rows);
        let reset = self.is_zoomed().then(reset_button);
        Stack::with_children(layers).extend(reset).into()
    }

    /// The canvas and the notes over it.
    fn layers(&self, hover: Option<(f64, f64)>, rows: bool) -> Vec<Element<'_, Event>> {
        let canvas = Canvas::new(Plot { graph: self, hover })
            .width(Length::Fill)
            .height(Length::Fill);
        if let (View::Distortion, Some(d)) = (self.view, &self.distortion) {
            // Which measurement it is.
            return vec![
                canvas.into(),
                corner_caption(format!("Distortion: {}", d.name)),
            ];
        }
        // Whether the phase is drawn wrapped or unwrapped.
        let caption = (self.view == View::Phase && !self.phase.is_empty())
            .then(|| corner_caption(self.phase_axis.caption().to_string()));
        let empty = if rows { ALL_HIDDEN } else { EMPTY };
        // Whether the note stands alone on an empty plot.
        let (note, alone) = match self.view {
            View::Response => (
                (self.traces.is_empty() && self.live.is_none()).then(|| empty.to_string()),
                true,
            ),
            View::Phase if self.no_phase > 0 => {
                (Some(no_phase_note(self.no_phase)), self.phase.is_empty())
            }
            View::Phase => (self.phase.is_empty().then(|| empty.to_string()), true),
            View::Distortion => (Some(self.distortion_note.clone()), true),
        };
        let Some(note) = note else {
            return std::iter::once(canvas.into()).chain(caption).collect();
        };
        let note = container(text(note).style(theme::text(|p| p.fg_dim)))
            .padding([4, 10])
            .style(theme::backdrop);
        let placed = if alone {
            container(note).center(Length::Fill)
        } else {
            // Beside traces: out of their way, under the plot.
            container(note)
                .align_x(iced::alignment::Horizontal::Center)
                .align_y(Vertical::Bottom)
                .padding(iced::Padding::ZERO.bottom(34.0))
                .width(Length::Fill)
                .height(Length::Fill)
        };
        [canvas.into(), placed.into()]
            .into_iter()
            .chain(caption)
            .collect()
    }
}

/// Reset view, in the margin above the plot at its right end: clear of the
/// plot and the readout cards in it. Its row is the small text's line
/// height (13 px in any face), inside the 16 px margin.
fn reset_button<'a>() -> Element<'a, Event> {
    let reset = button(text("Reset view").size(theme::TEXT_SMALL))
        .on_press(Event::Reset)
        .padding([0, 8])
        .style(theme::button);
    container(container(reset).style(theme::backdrop))
        .align_x(iced::alignment::Horizontal::Right)
        .align_y(Vertical::Top)
        .padding(iced::Padding {
            top: 1.0,
            right: INSETS.right,
            ..iced::Padding::ZERO
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// A caption in the plot's bottom-left corner.
fn corner_caption<'a>(content: String) -> Element<'a, Event> {
    let caption = container(
        text(content)
            .size(LABEL_SIZE)
            .style(theme::text(|p| p.fg_dim)),
    )
    .padding([2, 8])
    .style(theme::backdrop);
    container(caption)
        .align_x(iced::alignment::Horizontal::Left)
        .align_y(Vertical::Bottom)
        .padding(iced::Padding {
            left: INSETS.left + 6.0,
            bottom: INSETS.bottom + 6.0,
            ..iced::Padding::ZERO
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// The quietest and loudest levels of the points drawn, the line's ends included.
fn extent_of(response: &Response) -> Option<(f64, f64)> {
    plotted(response)
        .map(|(_, db)| db)
        .filter(|db| db.is_finite())
        .map(|db| (db, db))
        .reduce(|a, b| (a.0.min(b.0), a.1.max(b.1)))
}

/// Most lines the hover readout shows, the frequency's and "+N more" included.
const READOUT_LINES: usize = 12;
/// Longest trace name in the readout, in characters.
const READOUT_NAME: usize = 18;

/// Every trace with a level at `freq` (the live RTA trace as "Live"), loudest
/// first. The names are the traces' display names, prepared by `Graph::set`.
fn readout_lines<'a>(
    traces: &'a [Trace],
    live: Option<&Response>,
    freq: f64,
) -> Vec<(&'a str, Ink, f64)> {
    let mut lines: Vec<_> = traces
        .iter()
        .map(|t| (t.name.as_str(), Ink::Trace(t.color), &t.response))
        .chain(live.map(|r| ("Live", Ink::Fg, r)))
        .filter_map(|(name, ink, r)| {
            let level = r.interp_db(freq);
            level.is_finite().then_some((name, ink, level))
        })
        .collect();
    lines.sort_by(|a, b| b.2.total_cmp(&a.2));
    lines
}

/// `name` as one readout row: each whitespace run a single space, cut to
/// `READOUT_NAME` characters.
pub(crate) fn readout_name(name: &str) -> String {
    let one_line = name.split_whitespace().collect::<Vec<_>>().join(" ");
    cut(&one_line, READOUT_NAME)
}

/// `name` cut to `limit` characters, ending in an ellipsis when cut.
fn cut(name: &str, limit: usize) -> String {
    let limit = limit.max(1);
    if name.chars().count() <= limit {
        return name.to_string();
    }
    let mut out: String = name.chars().take(limit - 1).collect();
    out.push('…');
    out
}

/// Padding inside the readout card, top and bottom together.
const CARD_PAD: f32 = 6.0;

/// How many trace lines and how many "+N more" to show for `total` traces on a plot
/// `height` high with rows `row` high; `None` when not even the frequency's row fits.
fn card_rows(height: f32, row: f32, total: usize) -> Option<(usize, usize)> {
    let rows = ((height - CARD_PAD) / row) as usize;
    match rows {
        0 => None,
        1 => Some((0, 0)),
        _ => Some(readout_fit(total, rows - 1)),
    }
}

/// Where the card of `size` goes for a pointer at (`x`, `y`): beside it on the
/// side with room (right, else left; below, else above), then clamped inside the plot.
fn card_origin(plot: &PlotBox, x: f32, y: f32, size: Size) -> Point {
    let left = if x + 10.0 + size.width <= plot.right() {
        x + 10.0
    } else {
        x - 10.0 - size.width
    }
    .clamp(plot.left, (plot.right() - size.width).max(plot.left));
    let top = if y + 10.0 + size.height <= plot.bottom() {
        y + 10.0
    } else {
        y - 10.0 - size.height
    }
    .clamp(plot.top, (plot.bottom() - size.height).max(plot.top));
    Point::new(left, top)
}

/// How many of `total` trace lines to show in `room` lines (the frequency's line not
/// counted), and how many are left out. A "+N more" line takes the last of the room.
fn readout_fit(total: usize, room: usize) -> (usize, usize) {
    let room = room.min(READOUT_LINES - 1);
    if total <= room {
        (total, 0)
    } else {
        let shown = room.saturating_sub(1);
        (shown, total - shown)
    }
}

struct Plot<'a> {
    graph: &'a Graph,
    hover: Option<(f64, f64)>,
}

/// The wheel over an axis's labels of `plot` at `at` (in the canvas): the
/// band under the plot is the frequency axis's, the one left of it the
/// level axis's. `None` over the plot itself or elsewhere.
fn wheel_axis(plot: &PlotBox, at: Point) -> Option<(Axis, f32)> {
    let across = (plot.left..=plot.right()).contains(&at.x);
    let along = (plot.top..=plot.bottom()).contains(&at.y);
    if across && at.y > plot.bottom() {
        Some((Axis::Frequency, at.x))
    } else if along && at.x < plot.left {
        Some((Axis::Level, at.y))
    } else {
        None
    }
}

/// A wheel delta in notches, up positive.
fn wheel_lines(delta: mouse::ScrollDelta) -> f64 {
    match delta {
        mouse::ScrollDelta::Lines { y, .. } => f64::from(y),
        mouse::ScrollDelta::Pixels { y, .. } => f64::from(y / zoom::PIXELS_PER_LINE),
    }
}

impl canvas::Program<Event> for Plot<'_> {
    /// The hover last published.
    type State = Option<Hover>;

    fn update(
        &self,
        last: &mut Option<Hover>,
        event: &canvas::Event,
        bounds: Rectangle,
        cursor: Cursor,
    ) -> Option<canvas::Action<Event>> {
        let plot = PlotBox::inside(bounds.width, bounds.height, INSETS);
        if let canvas::Event::Mouse(mouse::Event::WheelScrolled { delta }) = event {
            let (axis, at) = wheel_axis(&plot, cursor.position_in(bounds)?)?;
            let lines = wheel_lines(*delta);
            if lines == 0.0 {
                return None;
            }
            let wheel = Wheel {
                axis,
                plot,
                at,
                lines,
            };
            return Some(canvas::Action::publish(Event::Wheel(wheel)).and_capture());
        }
        let canvas::Event::Mouse(mouse::Event::CursorMoved { .. } | mouse::Event::CursorLeft) =
            event
        else {
            return None;
        };
        let hover = match cursor.position_in(bounds) {
            Some(at) if plot.contains(at.x, at.y) => {
                let (axis, top) = self.graph.axis(plot);
                Hover::At {
                    freq: axis.freq_at(at.x),
                    db: axis.db_at(at.y, top),
                }
            }
            _ => Hover::Left,
        };
        if *last == Some(hover) {
            return None;
        }
        *last = Some(hover);
        Some(canvas::Action::publish(Event::Hover(hover)))
    }

    fn draw(
        &self,
        _last: &Option<Hover>,
        renderer: &Renderer,
        theme: &Theme,
        bounds: Rectangle,
        cursor: Cursor,
    ) -> Vec<Geometry> {
        let p = Palette::of(theme);
        let size = bounds.size();
        let plot = PlotBox::inside(size.width, size.height, INSETS);
        let graph = self.graph;
        // The dB axis follows the plot's shape in any view, so its span is
        // tracked even while another view is up.
        graph.drawn_span.set(plot.span);
        let (plot, top_db) = graph.axis(plot);
        let base = graph.cache.draw(renderer, size, |frame| {
            #[cfg(test)]
            graph.drawn.set(graph.drawn.get() + 1);
            draw_frame(frame, &plot, &p);
            match graph.view {
                View::Response => {
                    draw_db_grid(frame, &plot, top_db, &p);
                    draw_traces(frame, &plot, graph, top_db, &p);
                }
                View::Phase => {
                    draw_phase_grid(frame, &plot, top_db, &p);
                    draw_phase_traces(frame, &plot, graph, top_db, &p);
                }
                View::Distortion => {
                    draw_distortion(frame, &plot, graph, top_db, &p);
                }
            }
        });
        let live = graph.live_cache.draw(renderer, size, |frame| {
            if let (View::Response, Some(response)) = (graph.view, &graph.live) {
                let pin = graph.pins();
                draw_live(frame, &plot, response, top_db, pin, &p);
            }
        });
        let mut overlay = Frame::new(renderer, size);
        // Read the pointer against the axis as drawn now: the published
        // hover may predate an axis move. Headless snapshots have no cursor.
        let hover = match cursor.position() {
            Some(_) => cursor
                .position_in(bounds)
                .filter(|at| plot.contains(at.x, at.y))
                .map(|at| (plot.freq_at(at.x), plot.db_at(at.y, top_db))),
            None => self.hover,
        };
        if let Some((freq, db)) = hover {
            match graph.view {
                View::Response => draw_hover(&mut overlay, &plot, graph, top_db, freq, db, &p),
                View::Phase => {
                    draw_phase_hover(&mut overlay, &plot, graph, top_db, freq, db, &p);
                }
                View::Distortion => {
                    draw_distortion_hover(&mut overlay, &plot, graph, top_db, freq, db, &p);
                }
            }
        }
        vec![base, live, overlay.into_geometry()]
    }
}

fn label(
    content: String,
    position: Point,
    align_x: Alignment,
    align_y: Vertical,
    p: &Palette,
) -> Text {
    Text {
        content,
        position,
        color: p.fg_dim,
        size: LABEL_SIZE.into(),
        font: theme::mono(),
        align_x,
        align_y,
        ..Text::default()
    }
}

fn line(frame: &mut Frame, from: Point, to: Point, color: iced::Color) {
    frame.stroke(
        &Path::line(from, to),
        Stroke::default()
            .with_color(color)
            .with_width(theme::HAIRLINE),
    );
}

/// The background and the log frequency lines and their labels: hairlines
/// on black, mono numbers.
fn draw_frame(frame: &mut Frame, plot: &PlotBox, p: &Palette) {
    let size = frame.size();
    let background = Path::rounded_rectangle(
        Point::new(0.5, 0.5),
        Size::new(size.width - 1.0, size.height - 1.0),
        theme::RADIUS.into(),
    );
    frame.fill(&background, p.bg);
    frame.stroke(
        &background,
        Stroke::default()
            .with_color(p.border)
            .with_width(theme::HAIRLINE),
    );
    let (low, high) = (plot.f_low, plot.f_high);
    let lines = plot::freq_grid(low, high, plot.width);
    let labelled = plot::freq_labelled(&lines, low, high, plot.width, LABEL_SIZE);
    for (grid, labelled) in lines.iter().zip(labelled) {
        let f = grid.freq;
        let x = plot.x_of(f);
        let color = match grid.tick {
            Tick::Fine | Tick::Minor => p.bg_overlay,
            Tick::Major | Tick::Decade => p.border,
        };
        line(
            frame,
            Point::new(x, plot.top),
            Point::new(x, plot.bottom()),
            color,
        );
        if labelled {
            frame.fill_text(label(
                plot::freq_tick(f),
                Point::new(x, plot.bottom() + 6.0),
                Alignment::Center,
                Vertical::Top,
                p,
            ));
        }
    }
}

/// The dB lines and their labels.
fn draw_db_grid(frame: &mut Frame, plot: &PlotBox, top_db: f64, p: &Palette) {
    let (lines, majors) = db_steps(plot);
    let labels = label_step(plot, lines, majors);
    for db in plot::db_grid(top_db, plot.span, lines) {
        let y = plot.y_of(db, top_db);
        let labelled = on_step(db, labels);
        let color = if on_step(db, majors) {
            p.border
        } else {
            p.bg_overlay
        };
        line(
            frame,
            Point::new(plot.left, y),
            Point::new(plot.right(), y),
            color,
        );
        if labelled {
            frame.fill_text(label(
                plot::db_tick(db),
                Point::new(plot.left - 6.0, y),
                Alignment::Right,
                Vertical::Center,
                p,
            ));
        }
    }
}

/// Whether `value` is a whole multiple of `step`, rounding aside.
fn on_step(value: f64, step: f64) -> bool {
    let k = value / step;
    (k - k.round()).abs() < 1e-6
}

/// The phase lines (every 45°, finer on a zoomed axis: see
/// [`phase_steps`]) and their labels (spaced to fit the plot's height: see
/// [`phase_label_step`]).
fn draw_phase_grid(frame: &mut Frame, plot: &PlotBox, top: f64, p: &Palette) {
    let (lines, majors) = phase_steps(plot);
    let labels = phase_label_step(plot);
    for deg in plot::db_grid(top, plot.span, lines) {
        let y = plot.y_of(deg, top);
        let labelled = on_step(deg, labels);
        let color = if on_step(deg, majors) {
            p.border
        } else {
            p.bg_overlay
        };
        line(
            frame,
            Point::new(plot.left, y),
            Point::new(plot.right(), y),
            color,
        );
        if labelled {
            frame.fill_text(label(
                plot::phase_label(deg),
                Point::new(plot.left - 6.0, y),
                Alignment::Right,
                Vertical::Center,
                p,
            ));
        }
    }
}

/// Each phase trace in its colour, broken at gaps, and at wraps when the
/// view wraps phase.
fn draw_phase_traces(frame: &mut Frame, plot: &PlotBox, graph: &Graph, top: f64, p: &Palette) {
    let (traces, wrapped) = graph.phase_shown();
    for trace in traces {
        let points: Vec<(f64, f64)> = if wrapped {
            trace.plotted()
        } else {
            trace
                .freq
                .iter()
                .copied()
                .zip(trace.deg.iter().copied())
                .collect()
        };
        let runs = if wrapped {
            phase_segments(&points)
        } else {
            points
                .split(|(_, deg)| !deg.is_finite())
                .filter(|run| !run.is_empty())
                .map(<[_]>::to_vec)
                .collect()
        };
        let runs = runs.iter().flat_map(|run| {
            cut_runs(
                run.iter()
                    .map(|(f, deg)| Point::new(plot.x_of(*f), plot.y_of(*deg, top))),
                plot,
                graph.pins(),
            )
        });
        let path = Path::new(|b| {
            for run in runs {
                let [first, rest @ ..] = run.as_slice() else {
                    continue;
                };
                if rest.is_empty() {
                    // Alone between gaps: a dot.
                    b.circle(*first, 0.75);
                    continue;
                }
                b.move_to(*first);
                for &at in rest {
                    b.line_to(at);
                }
            }
        });
        frame.stroke(
            &path,
            Stroke::default()
                .with_color(p.trace(trace.color))
                .with_width(TRACE_WIDTH)
                .with_line_join(canvas::LineJoin::Round),
        );
    }
}

/// The fewest labelled lines a short (zoomed) dB axis aims for.
const MIN_DB_LABELS: f64 = 3.0;

/// The dB between grid lines and between labels: 5 and 10, scaled up by 2,
/// 5, 10, … when a narrow plot's labels would overlap, which also bounds the
/// number of lines by the plot's height; scaled down by 2, 5 and 10 on an axis
/// too short for [`MIN_DB_LABELS`] of them, down to 0.5 and 1.
fn db_steps(plot: &PlotBox) -> (f64, f64) {
    let label_px = f64::from(plot.height) * DB_LABELS / plot.span;
    let need = f64::from(LABEL_SIZE + 4.0) / label_px;
    let shortest = if plot.span >= DB_LABELS * MIN_DB_LABELS {
        1.0
    } else {
        [0.5, 0.2, 0.1]
            .into_iter()
            .find(|&s| DB_LABELS * MIN_DB_LABELS * s <= plot.span)
            .unwrap_or(0.1)
    };
    if let Some(stride) = [0.1, 0.2, 0.5]
        .into_iter()
        .find(|&s| s >= shortest && s >= need)
    {
        return (DB_LINES * stride, DB_LABELS * stride);
    }
    let mut stride = 1.0;
    'search: loop {
        for m in [1.0, 2.0, 5.0] {
            if stride * m >= need {
                stride *= m;
                break 'search;
            }
        }
        stride *= 10.0;
    }
    (DB_LINES * stride, DB_LABELS * stride)
}

/// The step between labelled lines on a vertical axis with lines every
/// `lines` and brighter ones every `majors`: every line when the lines stand
/// [`LABEL_ROOM`] apart on `plot`, else the brighter ones.
fn label_step(plot: &PlotBox, lines: f64, majors: f64) -> f64 {
    let px = f64::from(plot.height) * lines / plot.span;
    if px >= f64::from(LABEL_ROOM) {
        lines
    } else {
        majors
    }
}

/// The degrees between phase lines and between the brighter ones: 45° and
/// 90°, finer on an axis too short for two of the brighter ones.
fn phase_steps(plot: &PlotBox) -> (f64, f64) {
    [(PHASE_LINES, PHASE_MAJORS), (15.0, 45.0), (5.0, 15.0)]
        .into_iter()
        .find(|(_, majors)| plot.span >= 2.0 * majors)
        .unwrap_or((1.0, 5.0))
}

/// The degrees between phase labels: every line or every brighter one as
/// [`label_step`] says, thinned further (doubled) where that is closer than
/// a label's height and a little room.
fn phase_label_step(plot: &PlotBox) -> f64 {
    let (lines, majors) = phase_steps(plot);
    let mut step = label_step(plot, lines, majors);
    let px = |step: f64| f64::from(plot.height) * step / plot.span;
    while step < plot.span && px(step) < f64::from(LABEL_SIZE + 4.0) {
        step *= 2.0;
    }
    step
}

/// Whether the percent axis `plot` (its span in decades) has room to label
/// the 2× and 5× lines of each decade: their closest neighbours are a third
/// of a decade apart.
fn percent_minor_labels(plot: &PlotBox) -> bool {
    f64::from(plot.height) / plot.span * 2f64.log10() >= f64::from(LABEL_ROOM)
}

/// Whether a zoomed percent axis has room to label every line: 9× and 10×
/// are the closest.
fn percent_all_labels(plot: &PlotBox) -> bool {
    f64::from(plot.height) / plot.span * (10f64 / 9.0).log10() >= f64::from(LABEL_SIZE + 4.0)
}

/// A percent axis label, compact to fit left of the plot: "0.01%", "0.5%",
/// "20%", and beyond 0.01–900 % (a zoomed-out axis) "1e-4%", "5e3%".
fn percent_tick(decade: i32, m: u32) -> String {
    if !(-2..=2).contains(&decade) {
        return format!("{m}e{decade}%");
    }
    let decimals = (-decade).max(0) as usize;
    format!("{:.*}%", decimals, 10f64.powi(decade) * f64::from(m))
}

/// Each trace in its colour, broken where it has no data and held to the
/// plot: cut at the frequencies in view, levels beyond the axis pinned to
/// its edge (cut there too on a zoomed level axis: see [`Graph::pins`]).
///
/// Clipped by hand: iced_wgpu 0.14 drops what a `Frame::with_clip` draft
/// draws in this cached layer (the traces vanished from wgpu snapshots
/// while tiny-skia drew them).
fn draw_traces(frame: &mut Frame, plot: &PlotBox, graph: &Graph, top_db: f64, p: &Palette) {
    for trace in &graph.traces {
        frame.stroke(
            &trace_path(plot, &trace.response, top_db, graph.pins()),
            Stroke::default()
                .with_color(p.trace(trace.color))
                .with_width(TRACE_WIDTH)
                .with_line_join(canvas::LineJoin::Round),
        );
    }
}

/// The RTA's live trace: the foreground colour, dashed, so it never
/// shares a colour with a measurement.
fn draw_live(
    frame: &mut Frame,
    plot: &PlotBox,
    response: &Response,
    top_db: f64,
    pin: bool,
    p: &Palette,
) {
    frame.stroke(
        &trace_path(plot, response, top_db, pin),
        Stroke {
            line_dash: canvas::LineDash {
                segments: &[6.0, 4.0],
                offset: 0,
            },
            ..Stroke::default()
                .with_color(p.fg)
                .with_width(TRACE_WIDTH)
                .with_line_join(canvas::LineJoin::Round)
        },
    );
}

/// `response` as a line, broken where it has no data.
fn trace_path(plot: &PlotBox, response: &Response, top_db: f64, pin: bool) -> Path {
    let points = plotted(response).map(|(f, db)| Point::new(plot.x_of(f), plot.y_of(db, top_db)));
    runs_path(&cut_runs(points, plot, pin))
}

/// A line through the runs [`cut_runs`] gives.
fn runs_path(runs: &[Vec<Point>]) -> Path {
    Path::new(|b| {
        for run in runs {
            let mut run = run.iter();
            let Some(&first) = run.next() else {
                continue;
            };
            b.move_to(first);
            for &at in run {
                b.line_to(at);
            }
        }
    })
}

/// `points` (in pixels) as the runs a line joins: broken at gaps (a `NaN`),
/// cut where they cross the plot's left and right edges (the frequencies in
/// view), and pinned to its top and bottom edges, or cut there too unless
/// `pin`. A point alone between gaps is a run of one.
fn cut_runs(points: impl IntoIterator<Item = Point>, plot: &PlotBox, pin: bool) -> Vec<Vec<Point>> {
    let inside = |at: Point| {
        (plot.left..=plot.right()).contains(&at.x)
            && (pin || (plot.top..=plot.bottom()).contains(&at.y))
    };
    let pinned = |at: Point| Point::new(at.x, at.y.clamp(plot.top, plot.bottom()));
    let mut runs: Vec<Vec<Point>> = Vec::new();
    let mut run: Vec<Point> = Vec::new();
    let mut last: Option<Point> = None;
    for at in points {
        if !(at.x.is_finite() && at.y.is_finite()) {
            runs.extend((!run.is_empty()).then(|| std::mem::take(&mut run)));
            last = None;
            continue;
        }
        match last {
            None if inside(at) => run.push(pinned(at)),
            None => {}
            Some(from) => match cut_segment(from, at, plot, !pin) {
                None => runs.extend((!run.is_empty()).then(|| std::mem::take(&mut run))),
                Some((start, end, cut)) => {
                    if cut || run.is_empty() {
                        runs.extend((!run.is_empty()).then(|| std::mem::take(&mut run)));
                        run.push(pinned(start));
                    }
                    run.push(pinned(end));
                }
            },
        }
        last = Some(at);
    }
    runs.extend((!run.is_empty()).then_some(run));
    runs
}

/// The part of the segment `a`–`b` between the plot's left and right
/// edges, and its top and bottom ones too when `vertical`, with whether its
/// start was cut; `None` when none of it is.
fn cut_segment(a: Point, b: Point, plot: &PlotBox, vertical: bool) -> Option<(Point, Point, bool)> {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let edges = [
        (-dx, a.x - plot.left),
        (dx, plot.right() - a.x),
        (-dy, a.y - plot.top),
        (dy, plot.bottom() - a.y),
    ];
    // Liang–Barsky: narrow the share of the segment inside, edge by edge.
    let (mut from, mut to) = (0f32, 1f32);
    for (p, q) in edges.into_iter().take(if vertical { 4 } else { 2 }) {
        if p == 0.0 {
            if q < 0.0 {
                return None;
            }
            continue;
        }
        let t = q / p;
        if p < 0.0 {
            from = from.max(t);
        } else {
            to = to.min(t);
        }
    }
    if from > to {
        return None;
    }
    let at = |t: f32| {
        if t == 0.0 {
            a
        } else if t == 1.0 {
            b
        } else {
            Point::new(a.x + t * dx, a.y + t * dy)
        }
    };
    Some((at(from), at(to), from > 0.0))
}

/// `response`'s points inside the plotted decades, with its levels at the
/// two ends (`NaN` where it has none).
fn plotted(response: &Response) -> impl Iterator<Item = (f64, f64)> + '_ {
    let inside = response
        .freq
        .iter()
        .zip(&response.db)
        .filter(|(f, _)| plot::F_MIN < **f && **f < plot::F_MAX)
        .map(|(f, db)| (*f, *db));
    std::iter::once((plot::F_MIN, response.interp_db(plot::F_MIN)))
        .chain(inside)
        .chain(std::iter::once((
            plot::F_MAX,
            response.interp_db(plot::F_MAX),
        )))
}

/// The Distortion view's grid, curves and curve names.
fn draw_distortion(frame: &mut Frame, plot: &PlotBox, graph: &Graph, top: f64, p: &Palette) {
    let Some(d) = &graph.distortion else {
        return;
    };
    match d.units {
        Units::Percent => draw_percent_grid(frame, plot, top, p),
        Units::Db => draw_db_grid(frame, plot, top, p),
    }
    let ink = |ink: Ink| match ink {
        Ink::Trace(i) => p.trace(i),
        Ink::Fg => p.fg,
        Ink::Dim => p.fg_dim,
    };
    let mut ends = Vec::new();
    for curve in &d.curves {
        let runs = curve_runs(curve, plot, top, graph.pins());
        let width = if curve.thick { THD_WIDTH } else { TRACE_WIDTH };
        let stroke = Stroke {
            line_dash: if curve.dashed {
                canvas::LineDash {
                    segments: &[6.0, 4.0],
                    offset: 0,
                }
            } else {
                canvas::LineDash::default()
            },
            ..Stroke::default()
                .with_color(ink(curve.ink))
                .with_width(width)
                .with_line_join(canvas::LineJoin::Round)
        };
        frame.stroke(&runs_path(&runs), stroke);
        if let (true, Some(end)) = (curve.labelled, line_end(&runs)) {
            ends.push((end.y, curve));
        }
    }
    // Names at the right end of their curves, spread so they stay apart and
    // inside the plot.
    ends.sort_by(|a, b| a.0.total_cmp(&b.0));
    let wanted: Vec<f32> = ends.iter().map(|(y, _)| *y).collect();
    let Some(ys) = label_rows(&wanted, plot) else {
        return;
    };
    let low = plot.top + LABEL_SIZE / 2.0;
    for ((_, curve), y) in ends.iter().zip(ys) {
        frame.fill_text(Text {
            color: ink(curve.ink),
            ..label(
                curve.label.clone(),
                Point::new(plot.right() - 4.0, y.max(low)),
                Alignment::Right,
                Vertical::Center,
                p,
            )
        });
    }
}

/// `curve` as the runs of its line on `plot`, topped at `top`.
fn curve_runs(curve: &Curve, plot: &PlotBox, top: f64, pin: bool) -> Vec<Vec<Point>> {
    let points = curve
        .plotted()
        .map(|(f, y)| Point::new(plot.x_of(f), plot.y_of(y, top)));
    cut_runs(points, plot, pin)
}

/// Where the line of `runs` ends on the right: the last point drawn, an
/// edge crossing included. `None` when no line is drawn.
fn line_end(runs: &[Vec<Point>]) -> Option<Point> {
    runs.iter()
        .rev()
        .filter(|r| r.len() > 1)
        .find_map(|r| r.last().copied())
}

/// Rows for names wanting the sorted `wanted` heights: spread so they stay apart and inside
/// the plot. `None` when the plot is too short for them; the curves' names are on hover.
fn label_rows(wanted: &[f32], plot: &PlotBox) -> Option<Vec<f32>> {
    let room = plot.height - LABEL_SIZE;
    if room < LABEL_GAP * wanted.len().saturating_sub(1) as f32 {
        return None;
    }
    let (low, high) = (
        plot.top + LABEL_SIZE / 2.0,
        plot.bottom() - LABEL_SIZE / 2.0,
    );
    let mut ys: Vec<f32> = wanted.iter().map(|y| y.clamp(low, high)).collect();
    for i in 1..ys.len() {
        ys[i] = ys[i].max(ys[i - 1] + LABEL_GAP);
    }
    for i in (0..ys.len()).rev() {
        let ceiling = ys.get(i + 1).map_or(high, |next| next - LABEL_GAP);
        ys[i] = ys[i].min(ceiling);
    }
    Some(ys)
}

/// A line of the percent axis: its height, whether it is a decade's, and
/// its label, if it has one.
type PercentLine = (f32, bool, Option<String>);

/// The percent axis's lines: one per decade, and fainter ones at 2..9 times
/// each. The decades are labelled, every few of them where a decade is
/// shorter than a label's row; the 2× and 5× lines too when there is room.
fn percent_lines(plot: &PlotBox, top: f64) -> Vec<PercentLine> {
    let minors = percent_minor_labels(plot);
    let all = percent_all_labels(plot);
    let bottom = top - plot.span;
    let stride = (f64::from(LABEL_SIZE + 4.0) * plot.span / f64::from(plot.height))
        .ceil()
        .max(1.0) as i32;
    let mut out = Vec::new();
    for decade in bottom.floor() as i32..=top.floor() as i32 {
        for m in 1..=9 {
            let pct = 10f64.powi(decade) * f64::from(m);
            let at = pct.log10();
            if at < bottom - 1e-9 || at > top + 1e-9 {
                continue;
            }
            let major = m == 1;
            let labelled =
                (major && decade.rem_euclid(stride) == 0) || all || (minors && (m == 2 || m == 5));
            out.push((
                percent_y(plot, top, pct),
                major,
                labelled.then(|| percent_tick(decade, m)),
            ));
        }
    }
    out
}

/// The percent axis's lines and labels: see [`percent_lines`].
fn draw_percent_grid(frame: &mut Frame, plot: &PlotBox, top: f64, p: &Palette) {
    for (y, major, tick) in percent_lines(plot, top) {
        line(
            frame,
            Point::new(plot.left, y),
            Point::new(plot.right(), y),
            if major { p.border } else { p.bg_overlay },
        );
        if let Some(tick) = tick {
            frame.fill_text(label(
                tick,
                Point::new(plot.left - 6.0, y),
                Alignment::Right,
                Vertical::Center,
                p,
            ));
        }
    }
}

/// The Distortion view's crosshair and the readout of the curve nearest
/// `y`: "1.23 kHz  H3 0.042 %".
fn draw_distortion_hover(
    frame: &mut Frame,
    plot: &PlotBox,
    graph: &Graph,
    top: f64,
    freq: f64,
    y: f64,
    p: &Palette,
) {
    let x = plot.x_of(freq);
    line(
        frame,
        Point::new(x, plot.top),
        Point::new(x, plot.bottom()),
        p.fg_faint,
    );
    let readout = match graph
        .distortion
        .as_ref()
        .and_then(|d| Some((d, d.nearest(freq, y)?)))
    {
        Some((d, (curve, at))) => {
            let py = plot.y_of(at, top);
            if (plot.top..=plot.bottom()).contains(&py) {
                line(
                    frame,
                    Point::new(plot.left, py),
                    Point::new(plot.right(), py),
                    p.fg_faint,
                );
                let color = match curve.ink {
                    Ink::Trace(i) => p.trace(i),
                    Ink::Fg => p.fg,
                    Ink::Dim => p.fg_dim,
                };
                frame.fill(&Path::circle(Point::new(x, py), 3.5), color);
            }
            format!(
                "{}  {} {}",
                plot::freq_label(freq),
                curve.label,
                d.reading(at)
            )
        }
        None => plot::freq_label(freq),
    };
    draw_readout(frame, plot, x, readout, p);
}

/// The crosshair at `freq`, a dot on every trace there and a readout of all their levels.
fn draw_hover(
    frame: &mut Frame,
    plot: &PlotBox,
    graph: &Graph,
    top_db: f64,
    freq: f64,
    db: f64,
    p: &Palette,
) {
    let x = plot.x_of(freq);
    line(
        frame,
        Point::new(x, plot.top),
        Point::new(x, plot.bottom()),
        p.fg_faint,
    );
    let lines = readout_lines(&graph.traces, graph.live.as_ref(), freq);
    for (_, ink, level) in &lines {
        let y = plot.y_of(*level, top_db);
        // Off a zoomed axis, no dot; off the fitted one, on its edge.
        if !graph.pins() && !(plot.top..=plot.bottom()).contains(&y) {
            continue;
        }
        let y = y.clamp(plot.top, plot.bottom());
        let color = match ink {
            Ink::Trace(i) => p.trace(*i),
            Ink::Fg => p.fg,
            Ink::Dim => p.fg_dim,
        };
        frame.fill(&Path::circle(Point::new(x, y), 3.5), color);
    }
    let row = LABEL_SIZE + 4.0;
    let Some((shown, more)) = card_rows(plot.height, row, lines.len()) else {
        return;
    };
    // The whole card is drawn in the mono font, so a character's advance is a
    // fixed 0.6 em and the widths below match the drawn text exactly.
    let width_of = |content: &str| content.chars().count() as f32 * LABEL_SIZE * 0.6;
    let freq_text = plot::freq_label(freq);
    let more_text = format!("+{more} more");
    let levels: Vec<String> = lines[..shown]
        .iter()
        .map(|l| format!("{:+.1} dB", l.2))
        .collect();
    // Swatch and gaps around the name and the level.
    let level_width = levels.iter().map(|l| width_of(l)).fold(0.0, f32::max);
    let fixed = 28.0 + 12.0 + level_width;
    // A card wider than the plot gets shorter names, not a smaller scale.
    let name_room = (((plot.width - fixed) / (LABEL_SIZE * 0.6)) as usize).max(1);
    let names: Vec<Cow<str>> = lines[..shown]
        .iter()
        .map(|l| {
            if l.0.chars().count() > name_room {
                Cow::Owned(cut(l.0, name_room))
            } else {
                Cow::Borrowed(l.0)
            }
        })
        .collect();
    let rows = 1 + shown + usize::from(more > 0);
    let mut width = width_of(&freq_text) + 14.0;
    if more > 0 {
        width = width.max(width_of(&more_text) + 14.0);
    }
    for (name, level) in names.iter().zip(&levels) {
        width = width.max(28.0 + width_of(name) + 12.0 + width_of(level));
    }
    let height = row * rows as f32 + CARD_PAD;
    let pointer = plot.y_of(db, top_db).clamp(plot.top, plot.bottom());
    let at = card_origin(plot, x, pointer, Size::new(width, height));
    let card = Path::rounded_rectangle(at, Size::new(width, height), theme::RADIUS.into());
    frame.fill(&card, p.bg_raised);
    frame.stroke(
        &card,
        Stroke::default()
            .with_color(p.border)
            .with_width(theme::HAIRLINE),
    );
    let text = |content: String, x: f32, row_y: f32, align: Alignment| Text {
        color: p.fg,
        font: theme::mono(),
        ..label(content, Point::new(x, row_y), align, Vertical::Center, p)
    };
    let row_y = |i: usize| at.y + CARD_PAD / 2.0 + row * (i as f32 + 0.5);
    frame.fill_text(text(freq_text, at.x + 7.0, row_y(0), Alignment::Left));
    for (i, ((_, ink, _), (name, level))) in lines[..shown]
        .iter()
        .zip(names.iter().zip(levels))
        .enumerate()
    {
        let y = row_y(i + 1);
        let color = match ink {
            Ink::Trace(i) => p.trace(*i),
            Ink::Fg => p.fg,
            Ink::Dim => p.fg_dim,
        };
        let swatch = Path::rectangle(Point::new(at.x + 7.0, y - 3.0), Size::new(8.0, 6.0));
        frame.fill(&swatch, color);
        frame.fill_text(text(name.to_string(), at.x + 21.0, y, Alignment::Left));
        frame.fill_text(text(level, at.x + width - 7.0, y, Alignment::Right));
    }
    if more > 0 {
        frame.fill_text(text(
            more_text,
            at.x + 7.0,
            row_y(rows - 1),
            Alignment::Left,
        ));
    }
}

/// The Phase view's crosshair and the readout of the trace nearest `deg`.
fn draw_phase_hover(
    frame: &mut Frame,
    plot: &PlotBox,
    graph: &Graph,
    top: f64,
    freq: f64,
    deg: f64,
    p: &Palette,
) {
    let x = plot.x_of(freq);
    line(
        frame,
        Point::new(x, plot.top),
        Point::new(x, plot.bottom()),
        p.fg_faint,
    );
    let (traces, wrapped) = graph.phase_shown();
    let readout = match nearest_phase(traces, freq, deg, wrapped) {
        Some((trace, at)) => {
            let y = plot.y_of(at, top);
            // Off a zoomed axis, the readout alone.
            if (plot.top..=plot.bottom()).contains(&y) {
                line(
                    frame,
                    Point::new(plot.left, y),
                    Point::new(plot.right(), y),
                    p.fg_faint,
                );
                frame.fill(&Path::circle(Point::new(x, y), 3.5), p.trace(trace.color));
            }
            format!("{}  {}", plot::freq_label(freq), plot::phase_label(at))
        }
        None => plot::freq_label(freq),
    };
    draw_readout(frame, plot, x, readout, p);
}

/// The readout card beside the crosshair at `x`.
fn draw_readout(frame: &mut Frame, plot: &PlotBox, x: f32, readout: String, p: &Palette) {
    // The mono face's advance is about 0.6 em.
    let width = readout.chars().count() as f32 * LABEL_SIZE * 0.6 + 14.0;
    let height = LABEL_SIZE + 10.0;
    let left = if x + 10.0 + width <= plot.right() {
        x + 10.0
    } else {
        x - 10.0 - width
    };
    let at = Point::new(left, plot.top + 8.0);
    let card = Path::rounded_rectangle(at, Size::new(width, height), theme::RADIUS.into());
    frame.fill(&card, p.bg_raised);
    frame.stroke(
        &card,
        Stroke::default()
            .with_color(p.border)
            .with_width(theme::HAIRLINE),
    );
    frame.fill_text(Text {
        color: p.fg,
        ..label(
            readout,
            Point::new(at.x + 7.0, at.y + height / 2.0),
            Alignment::Left,
            Vertical::Center,
            p,
        )
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(db: f64) -> Trace {
        let freq = vec![10.0, 20.0, 1000.0, 20_000.0, 24_000.0];
        let mut levels = vec![db; freq.len()];
        // Out of the plotted decades: never sets the scale.
        levels[0] = db + 40.0;
        Trace {
            name: String::new(),
            response: Response {
                freq,
                db: levels,
                phase: None,
            },
            color: 0,
        }
    }

    /// A plot `w` by `h` pixels inside the axis-label room.
    fn sized(w: f32, h: f32) -> PlotBox {
        PlotBox::inside(
            w + INSETS.left + INSETS.right,
            h + INSETS.top + INSETS.bottom,
            INSETS,
        )
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_big_plot_labels_every_db_line() {
        // The dB span follows the shape: 5 dB is a twentieth of the width.
        let big = sized(1400.0, 700.0);
        let (lines, majors) = db_steps(&big);
        assert_eq!((lines, majors), (DB_LINES, DB_LABELS));
        assert_eq!(label_step(&big, lines, majors), DB_LINES);
        let small = sized(400.0, 300.0);
        let (lines, majors) = db_steps(&small);
        assert_eq!(label_step(&small, lines, majors), majors);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_tall_plot_labels_every_45_degrees() {
        let phase = |h| PlotBox {
            span: PHASE_SPAN,
            ..sized(1000.0, h)
        };
        assert_eq!(label_step(&phase(600.0), PHASE_LINES, PHASE_MAJORS), 45.0);
        assert_eq!(label_step(&phase(150.0), PHASE_LINES, PHASE_MAJORS), 90.0);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_tall_plot_labels_2_and_5_percent_lines() {
        let percent = |h| PlotBox {
            span: PCT_SPAN,
            ..sized(1000.0, h)
        };
        assert!(percent_minor_labels(&percent(600.0)));
        assert!(!percent_minor_labels(&percent(200.0)));
        assert_eq!(percent_tick(-2, 1), "0.01%");
        assert_eq!(percent_tick(-2, 5), "0.05%");
        assert_eq!(percent_tick(-1, 2), "0.2%");
        assert_eq!(percent_tick(0, 5), "5%");
        assert_eq!(percent_tick(1, 2), "20%");
        assert_eq!(percent_tick(2, 1), "100%");
        // The decades a zoomed-out axis adds, compact.
        assert_eq!(percent_tick(-4, 1), "1e-4%");
        assert_eq!(percent_tick(3, 5), "5e3%");
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn the_axis_follows_the_loudest_shown_trace() {
        let mut graph = Graph::default();
        assert_eq!(graph.top_db(10.0), plot::EMPTY_TOP_DB);
        graph.set(vec![flat(71.0), flat(83.2)]);
        assert_eq!(graph.top_db(20.0), 90.0);
        graph.set(Vec::new());
        assert_eq!(graph.top_db(10.0), plot::EMPTY_TOP_DB);
    }

    // A tall plot (a tall or narrow window) has more dB than the curves
    // need: they sit in the middle, not crowded under the top edge.
    // Bugs caught: 0, False positives: 0
    #[test]
    fn spare_room_centres_the_curves() {
        let mut graph = Graph::default();
        graph.set(vec![flat(-10.0), flat(10.0)]);
        // -10..+10 dB on a 100 dB axis: 40 dB spare above and below.
        let top = graph.top_db(100.0);
        assert_eq!(top, 50.0);
        // Too little room to centre with margins: pinned to the loudest.
        assert_eq!(graph.top_db(25.0), plot::top_db(Some(10.0)));
    }

    // A short, wide plot spans only a few dB: pinning the loudest level 5 dB
    // under the top must not push every trace off the bottom edge.
    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_short_plot_keeps_the_curves_in_view() {
        let mut graph = Graph::default();
        graph.set(vec![flat(1.0)]);
        let top = graph.top_db(5.0);
        assert!(top > 1.0 && top - 5.0 < 1.0, "top {top}");
    }

    // Just past the span where centring starts, the top must not jump a
    // whole step and leave the quietest level on the bottom edge.
    // Bugs caught: 0, False positives: 0
    #[test]
    fn centring_keeps_room_below() {
        let mut graph = Graph::default();
        graph.set(vec![flat(-10.0), flat(0.0)]);
        for span in [20.0, 20.01, 23.0, 37.5] {
            let top = graph.top_db(span);
            assert!(top - span <= -12.0 && top >= 2.0, "span {span}: top {top}");
        }
    }

    // A live RTA update that moves the quietest level without moving the
    // axis must not redraw the grid and every stored trace.
    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_live_update_that_keeps_the_axis_keeps_the_cache() {
        let mut graph = Graph::default();
        graph.drawn_span.set(100.0);
        graph.set(vec![flat(0.0), flat(-20.0)]);
        let live = |db: f64| flat(db).response;
        graph.set_live(Some(live(-24.0)));
        // The quietest level moves, the centred top (40 dB) does not.
        graph.live = Some(live(-24.2));
        assert!(!graph.rescale());
        graph.live = Some(live(-40.0));
        assert!(graph.rescale());
    }

    // Narrow plots stretch the dB span; labels and lines thin out so they
    // never overlap and the line count stays bounded by the plot's height.
    // Bugs caught: 0, False positives: 0
    #[test]
    fn db_steps_never_crowd() {
        for (w, h) in [(900.0, 450.0), (80.0, 600.0), (1.0, 600.0)] {
            let plot = PlotBox::inside(w + INSETS.left + INSETS.right, h, INSETS);
            let (lines, labels) = db_steps(&plot);
            let px = |db: f64| f64::from(plot.height) * db / plot.span;
            assert!(
                px(labels) >= f64::from(LABEL_SIZE) + 4.0,
                "{w}x{h}: labels {labels}"
            );
            assert!(
                plot.span / lines <= f64::from(plot.height),
                "{w}x{h}: lines {lines}"
            );
            assert_eq!((labels / lines).fract(), 0.0);
        }
        let wide = PlotBox::inside(900.0 + INSETS.left + INSETS.right, 450.0, INSETS);
        assert_eq!(db_steps(&wide), (DB_LINES, DB_LABELS));
    }

    // Bugs caught: 1, False positives: 0
    #[test]
    fn the_distortion_axis_holds_a_wide_extent() {
        let mut graph = Graph {
            view: View::Distortion,
            extent: Some((-175.0, 5.0)),
            ..Graph::default()
        };
        let (span, top) = graph.distortion_db_axis(50.0);
        assert!(top >= 5.0 && top - span <= -175.0, "span {span} top {top}");
        // The cap is on the shape's request alone.
        assert_eq!(graph.distortion_db_axis(500.0).0, 200.0);
        // A live trace does not count outside the Response view.
        graph.live = Some(flat(60.0).response);
        graph.rescale();
        assert_eq!(graph.extent, None);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn the_axis_fits_a_curve_known_only_beyond_the_plot() {
        let mut graph = Graph::default();
        graph.set(vec![Trace {
            name: String::new(),
            response: Response {
                freq: vec![10.0, 24_000.0],
                db: vec![75.0, 75.0],
                phase: None,
            },
            color: 0,
        }]);
        let top = graph.top_db(5.0);
        assert!(top - 5.0 < 75.0 && 75.0 < top, "top {top}");
    }

    fn named(name: &str, db: f64) -> Trace {
        Trace {
            name: name.into(),
            ..flat(db)
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn the_readout_lists_every_trace_loudest_first() {
        let traces = [named("quiet", 70.0), named("loud", 80.0)];
        let lines = readout_lines(&traces, None, 1000.0);
        let names: Vec<_> = lines.iter().map(|l| l.0).collect();
        assert_eq!(names, ["loud", "quiet"]);
        assert_eq!((lines[0].2, lines[1].2), (80.0, 70.0));
        // The live trace joins in, named "Live".
        let live = flat(75.0).response;
        let lines = readout_lines(&traces, Some(&live), 1000.0);
        assert_eq!(lines[1].0, "Live");
        assert!(matches!(lines[1].1, Ink::Fg));
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn the_readout_leaves_out_traces_without_data_there() {
        let mut short = named("short", 90.0);
        short.response = Response {
            freq: vec![20.0, 500.0],
            db: vec![90.0, 90.0],
            phase: None,
        };
        let traces = [named("full", 70.0), short];
        let lines = readout_lines(&traces, None, 1000.0);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].0, "full");
        assert!(readout_lines(&traces, None, 5.0).is_empty());
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn the_readout_counts_what_it_leaves_out() {
        assert_eq!(readout_fit(3, 11), (3, 0));
        assert_eq!(readout_fit(11, 11), (11, 0));
        // Twelve lines: the frequency, 10 traces and "+N more".
        assert_eq!(readout_fit(15, 40), (10, 5));
        // A short plot shows what fits, the last line of room being "+N more".
        assert_eq!(readout_fit(5, 3), (2, 3));
        assert_eq!(readout_fit(5, 0), (0, 5));
        let name = readout_name("a very long measurement name");
        assert_eq!(name.chars().count(), READOUT_NAME);
        assert!(name.ends_with('…'));
    }

    // Bugs caught: 1, False positives: 0
    #[test]
    fn a_name_is_one_row() {
        assert_eq!(readout_name("Left\nrear\t\t EQ"), "Left rear EQ");
        // Graph::set prepares every trace's name once.
        let mut graph = Graph::default();
        graph.set(vec![named("a\r\nb", 70.0)]);
        assert_eq!(graph.traces[0].name, "a b");
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn the_card_stays_inside_a_narrow_plot() {
        let plot = sized(300.0, 300.0);
        // 18-character name: swatch, name, gap and "+100.0 dB" in mono 11 px.
        let size = Size::new(28.0 + 18.0 * 6.6 + 12.0 + 9.0 * 6.6, 40.0);
        let at = card_origin(&plot, plot.left + 150.0, plot.top + 100.0, size);
        assert!(
            at.x >= plot.left && at.x + size.width <= plot.right(),
            "{at:?}"
        );
        assert!(
            at.y >= plot.top && at.y + size.height <= plot.bottom(),
            "{at:?}"
        );
        // Wider than the plot: pinned to the left edge.
        let wide = card_origin(
            &plot,
            plot.left + 150.0,
            plot.top + 100.0,
            Size::new(400.0, 40.0),
        );
        assert_eq!(wide.x, plot.left);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_short_plot_shows_the_level_that_fits() {
        let row = LABEL_SIZE + 4.0;
        // The frequency and one level need two rows and the padding.
        assert_eq!(card_rows(2.0 * row + CARD_PAD, row, 1), Some((1, 0)));
        assert_eq!(card_rows(50.0, row, 1), Some((1, 0)));
        // Only the frequency fits, then nothing.
        assert_eq!(card_rows(row + CARD_PAD, row, 3), Some((0, 0)));
        assert_eq!(card_rows(row + CARD_PAD - 1.0, row, 3), None);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn traces_are_cut_at_the_plotted_decades() {
        let response = Response {
            freq: vec![10.0, 100.0, 1000.0, 30_000.0],
            db: vec![0.0, 10.0, f64::NAN, 0.0],
            phase: None,
        };
        let points: Vec<_> = plotted(&response).collect();
        assert_eq!(points.len(), 4);
        assert_eq!(points[0].0, plot::F_MIN);
        assert!((points[0].1 - 3.0103).abs() < 1e-3, "{}", points[0].1);
        assert_eq!(points[1], (100.0, 10.0));
        assert!(points[2].1.is_nan(), "a gap stays a gap");
        assert_eq!(points[3].0, plot::F_MAX);
        assert!(points[3].1.is_nan(), "no level next to the gap");
    }

    // Phase is wrapped: a line that jumps across the wrap breaks, and each
    // side runs on to the edge it crossed, at the crossing's frequency.
    // Bugs caught: 1, False positives: 0
    #[test]
    fn phase_breaks_at_wrap() {
        let wrapped = phase_segments(&[(100.0, -170.0), (200.0, 170.0), (400.0, 160.0)]);
        assert_eq!(wrapped.len(), 2);
        // -170 -> 170 is -170 -> -190: the crossing of -180 is halfway in log f.
        let mid = (100.0_f64 * 200.0).sqrt();
        assert_eq!(wrapped[0].len(), 2);
        assert_eq!(wrapped[0][0], (100.0, -170.0));
        assert!((wrapped[0][1].0 - mid).abs() < 1e-9);
        assert_eq!(wrapped[0][1].1, -180.0);
        assert!((wrapped[1][0].0 - mid).abs() < 1e-9);
        assert_eq!(wrapped[1][0].1, 180.0);
        assert_eq!(wrapped[1][1..], [(200.0, 170.0), (400.0, 160.0)]);
        let up = phase_segments(&[(100.0, 170.0), (200.0, -170.0)]);
        assert_eq!((up[0][1].1, up[1][0].1), (180.0, -180.0));
        let smooth = phase_segments(&[(100.0, -10.0), (200.0, 90.0), (400.0, 179.0)]);
        assert_eq!(smooth.len(), 1);
        let gap = phase_segments(&[(100.0, 0.0), (200.0, f64::NAN), (400.0, 5.0)]);
        assert_eq!(gap.len(), 2);
    }

    // Phase falling 200 degrees a point wraps at nearly every point: each
    // must still be drawn, in a run of at least two points.
    // Bugs caught: 1, False positives: 0
    #[test]
    fn dense_wraps_leave_no_point_alone() {
        let points: Vec<(f64, f64)> = (0..8)
            .map(|k| {
                let deg = -200.0 * f64::from(k) + 160.0;
                (
                    100.0 * 1.1_f64.powi(k),
                    (deg + 180.0).rem_euclid(360.0) - 180.0,
                )
            })
            .collect();
        let runs = phase_segments(&points);
        assert!(runs.iter().all(|r| r.len() >= 2), "{runs:?}");
        for point in &points {
            assert!(runs.iter().any(|r| r.contains(point)), "{point:?}");
        }
    }

    /// A trace over 20 Hz–20 kHz whose phase falls linearly in log f from
    /// `from` to `to` degrees, wrapped to (−180, 180].
    fn ramp(from: f64, to: f64) -> (PhaseTrace, Vec<f64>) {
        let n = 200;
        let freq: Vec<f64> = (0..n)
            .map(|i| plot::F_MIN * 1000f64.powf(i as f64 / (n - 1) as f64))
            .collect();
        let line: Vec<f64> = (0..n)
            .map(|i| from + (to - from) * i as f64 / (n - 1) as f64)
            .collect();
        let wrap = |d: f64| 180.0 - (180.0 - d).rem_euclid(360.0);
        let trace = PhaseTrace {
            freq,
            deg: line.iter().map(|d| wrap(*d)).collect(),
            color: 0,
        };
        (trace, line)
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_wrapped_ramp_unwraps_to_a_straight_line() {
        let (trace, line) = ramp(-100.0, -500.0);
        assert!(trace.deg.iter().all(|d| *d > -180.0 && *d <= 180.0));
        let out = unwrap_phase(&trace.freq, &trace.deg);
        // At 1 kHz the line is near −326°: anchored a turn up, near +34°.
        for (got, want) in out.iter().zip(&line) {
            assert!((got - (want + 360.0)).abs() < 1e-9, "{got} vs {want}");
        }
        // A gap keeps its place, and the rest stays on the line.
        let mut gappy = trace.deg.clone();
        for d in &mut gappy[90..100] {
            *d = f64::NAN;
        }
        let out = unwrap_phase(&trace.freq, &gappy);
        assert!(out[90..100].iter().all(|d| d.is_nan()));
        for (i, (got, want)) in out.iter().zip(&line).enumerate() {
            if !(90..100).contains(&i) {
                assert!((got - (want + 360.0)).abs() < 1e-9, "{i}: {got}");
            }
        }
    }

    // Bugs caught: 0, False positives: 1
    #[test]
    fn phase_wraps_when_it_spans_more_than_720_degrees() {
        let mut graph = Graph::default();
        graph.set_phase(vec![ramp(-100.0, -500.0).0], 0);
        // 260° down to −140°: 270° to −180°, then 45° lines from the top.
        assert_eq!(
            graph.phase_axis,
            PhaseAxis::Unwrapped {
                top: 270.0,
                span: 450.0
            }
        );
        assert_eq!(graph.phase_axis.caption(), "Phase · unwrapped");
        let (shown, wrapped) = graph.phase_shown();
        assert!(!wrapped);
        assert!(shown[0].deg.windows(2).all(|w| (w[1] - w[0]).abs() < 180.0));
        // A short range is drawn on at least a full turn.
        graph.set_phase(vec![ramp(10.0, -20.0).0], 0);
        assert_eq!(
            graph.phase_axis,
            PhaseAxis::Unwrapped {
                top: 180.0,
                span: 360.0
            }
        );
        // Two traces that fit alone but span 820° together wrap: anchored,
        // the first runs 360°..−340°, the second −460°..140°.
        graph.set_phase(vec![ramp(0.0, -700.0).0], 0);
        assert!(matches!(graph.phase_axis, PhaseAxis::Unwrapped { .. }));
        graph.set_phase(vec![ramp(0.0, -700.0).0, ramp(-100.0, 500.0).0], 0);
        assert_eq!(graph.phase_axis, PhaseAxis::Wrapped);
        assert_eq!(graph.phase_axis.caption(), "Phase · wrapped");
        assert!(graph.phase_shown().1);
        graph.set_phase(vec![ramp(0.0, -900.0).0], 0);
        assert_eq!(graph.phase_axis, PhaseAxis::Wrapped);
    }

    // Bugs caught: 1, False positives: 1
    #[test]
    fn exact_half_turns_add_no_turns() {
        let freq: Vec<f64> = (0..12).map(|i| 100.0 * 1.5f64.powi(i)).collect();
        let deg: Vec<f64> = (0..12)
            .map(|i| if i % 2 == 0 { 0.0 } else { 180.0 })
            .collect();
        let out = unwrap_phase(&freq, &deg);
        let (low, high) = out
            .iter()
            .fold((f64::MAX, f64::MIN), |(l, h), d| (l.min(*d), h.max(*d)));
        assert_eq!(high - low, 180.0, "{out:?}");
        // A step past a half-turn is still a wrap (then anchored at 400 Hz).
        let out = unwrap_phase(&[100.0, 200.0, 400.0], &[170.0, -170.0, -150.0]);
        assert_eq!(out, [-190.0, -170.0, -150.0]);
    }

    // Bugs caught: 1, False positives: 0
    #[test]
    fn a_short_phase_range_pads_in_whole_90_degree_steps() {
        // 10°..20° sits in one 90° step; the axis grows to a turn on 90° lines.
        let trace = PhaseTrace {
            freq: vec![100.0, 1000.0, 10_000.0],
            deg: vec![10.0, 15.0, 20.0],
            color: 0,
        };
        assert_eq!(
            PhaseAxis::of(&[trace]),
            PhaseAxis::Unwrapped {
                top: 180.0,
                span: 360.0
            }
        );
    }

    // Bugs caught: 1, False positives: 0
    #[test]
    fn phase_labels_thin_out_on_a_short_plot() {
        let phase = |h, span| PlotBox {
            span,
            ..sized(1000.0, h)
        };
        assert_eq!(phase_label_step(&phase(600.0, 360.0)), 45.0);
        assert_eq!(phase_label_step(&phase(60.0, 360.0)), 90.0);
        // 720° in 60 px: a label every 90° would be 7.5 px apart.
        assert_eq!(phase_label_step(&phase(60.0, 720.0)), 180.0);
        // No room at all still ends.
        assert!(phase_label_step(&phase(0.0, 720.0)).is_finite());
    }

    // Bugs caught: 1, False positives: 0
    #[test]
    fn nearest_phase_wraps_around() {
        let at = |deg: f64| PhaseTrace {
            freq: vec![100.0, 200.0],
            deg: vec![deg, deg],
            color: 0,
        };
        let traces = [at(0.0), at(-179.0)];
        assert_eq!(
            nearest_phase(&traces, 150.0, 179.0, true).unwrap().1,
            -179.0
        );
        // Unwrapped, the gap is plain.
        assert_eq!(nearest_phase(&traces, 150.0, 179.0, false).unwrap().1, 0.0);
        assert_eq!(circular_gap(179.0, -179.0), 2.0);
    }

    // Bugs caught: 1, False positives: 0
    #[test]
    fn phase_readout_follows_the_short_arc() {
        let trace = |a: f64, b: f64| PhaseTrace {
            freq: vec![100.0, 400.0],
            deg: vec![a, b],
            color: 0,
        };
        // Halfway in log f between +170 and -170 is the wrap, not 0.
        let mid = trace(170.0, -170.0).at(200.0);
        assert_eq!(mid.abs(), 180.0, "{mid}");
        // +150 to -100 is 110 degrees the short way round, so halfway is -155.
        let q = trace(150.0, -100.0).at(200.0);
        assert!((q + 155.0).abs() < 1e-9, "{q}");
        // No wrap: plain interpolation.
        assert!((trace(10.0, 30.0).at(200.0) - 20.0).abs() < 1e-9);
    }

    // Bugs caught: 1, False positives: 0
    #[test]
    fn labels_are_skipped_on_a_plot_too_short_for_them() {
        let short = PlotBox {
            height: LABEL_SIZE - 1.0,
            ..PlotBox::inside(1000.0, 600.0, INSETS)
        };
        assert!(label_rows(&[1.0, 2.0, 3.0], &short).is_none());
        assert!(label_rows(&[1.0], &short).is_none());
        let tall = PlotBox::inside(1000.0, 600.0, INSETS);
        let ys = label_rows(&[tall.top, tall.top], &tall).unwrap();
        assert!(ys[1] - ys[0] >= LABEL_GAP);
    }

    // Bugs caught: 1, False positives: 0
    #[test]
    fn percent_readout_uses_scientific_below_a_thousandth() {
        let d = DistortionPlot {
            name: String::new(),
            units: Units::Percent,
            curves: Vec::new(),
        };
        assert_eq!(d.reading(-2.0), "0.010 %");
        assert_eq!(d.reading((3.2e-5f64).log10()), "3.2e-5 %");
    }

    // 0.01 % sits on the bottom edge and 100 % on the top one, a decade
    // apart in between; less than 0.01 % clamps to the bottom.
    // Bugs caught: 0, False positives: 0
    #[test]
    fn percent_axis_maps_decades() {
        let graph = Graph {
            view: View::Distortion,
            ..Graph::default()
        };
        let (plot, top) = graph.axis(PlotBox::inside(1000.0, 600.0, INSETS));
        assert_eq!(percent_y(&plot, top, 0.01), plot.bottom());
        assert_eq!(percent_y(&plot, top, 100.0), plot.top);
        let step = (plot.bottom() - plot.top) / 4.0;
        assert!((percent_y(&plot, top, 1.0) - (plot.top + 2.0 * step)).abs() < 1e-3);
        assert!((percent_y(&plot, top, 10.0) - (plot.top + step)).abs() < 1e-3);
        assert_eq!(percent_y(&plot, top, 0.0001), plot.bottom());
        assert_eq!(percent_y(&plot, top, 0.0), plot.bottom());
    }

    // A gap in a curve stays a gap on the percent axis: it is not read as
    // 0 %, which would draw a flat line on the floor and win the hover.
    // Bugs caught: 1, False positives: 0
    #[test]
    fn percent_gaps_stay_gaps() {
        assert!(percent_axis(f64::NAN).is_nan());
        assert_eq!(percent_axis(0.0), -12.0);
        let freq = vec![100.0, 1000.0, 10_000.0, 20_000.0];
        let fundamental = Response {
            freq: freq.clone(),
            db: vec![0.0; 4],
            phase: None,
        };
        let level = Response {
            freq,
            db: vec![-40.0, -40.0, f64::NAN, f64::NAN],
            phase: None,
        };
        let curve = Curve::new("H2", Ink::Fg, &level, &fundamental, Units::Percent);
        assert!(curve.at(1000.0).abs() < 1e-9);
        assert!(curve.at(15_000.0).is_nan());
        assert!(curve.at(10_000.0).is_nan());
        let last = curve.plotted().filter(|(_, y)| y.is_finite()).last();
        assert_eq!(last.map(|l| l.0), Some(1000.0));
        let plot = DistortionPlot {
            name: String::new(),
            units: Units::Percent,
            curves: vec![curve],
        };
        assert!(plot.nearest(15_000.0, 0.0).is_none());
    }

    /// A wheel scroll of `lines` notches over `axis` at `at` on a 1000×600
    /// canvas.
    fn wheel(axis: Axis, at: f32, lines: f64) -> Wheel {
        Wheel {
            axis,
            plot: PlotBox::inside(1000.0, 600.0, INSETS),
            at,
            lines,
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn the_wheel_zooms_only_over_an_axis() {
        let plot = PlotBox::inside(1000.0, 600.0, INSETS);
        let mid = Point::new(plot.left + plot.width / 2.0, plot.top + plot.height / 2.0);
        let under = Point::new(mid.x, plot.bottom() + 10.0);
        let beside = Point::new(plot.left - 10.0, mid.y);
        assert_eq!(wheel_axis(&plot, mid), None);
        assert_eq!(wheel_axis(&plot, under), Some((Axis::Frequency, mid.x)));
        assert_eq!(wheel_axis(&plot, beside), Some((Axis::Level, mid.y)));
        // The corner under the level labels, and the top and right margins.
        assert_eq!(
            wheel_axis(&plot, Point::new(10.0, plot.bottom() + 10.0)),
            None
        );
        assert_eq!(wheel_axis(&plot, Point::new(mid.x, 4.0)), None);
        assert_eq!(
            wheel_axis(&plot, Point::new(plot.right() + 4.0, mid.y)),
            None
        );
        assert_eq!(
            wheel_lines(mouse::ScrollDelta::Pixels { x: 0.0, y: -120.0 }),
            -2.0
        );
    }

    // Each view keeps its own zoom; Reset view and Clear's reset put the
    // automatic axes back.
    // Bugs caught: 0, False positives: 0
    #[test]
    fn each_view_keeps_its_own_zoom() {
        let mut graph = Graph::default();
        graph.set(vec![flat(80.0)]);
        let plot = PlotBox::inside(1000.0, 600.0, INSETS);
        let (fit, top) = graph.axis(plot);
        graph.wheel(wheel(Axis::Frequency, plot.left + 100.0, 2.0));
        assert!(graph.is_zoomed());
        graph.set_view(View::Phase);
        assert!(!graph.is_zoomed() && graph.any_zoomed());
        graph.wheel(wheel(Axis::Level, plot.top + 10.0, 1.0));
        graph.set_view(View::Response);
        let (zoomed, zoomed_top) = graph.axis(plot);
        assert!(zoomed.f_high / zoomed.f_low < plot::F_MAX / plot::F_MIN);
        assert_eq!((zoomed.span, zoomed_top), (fit.span, top));
        graph.reset_view();
        assert!(!graph.is_zoomed() && graph.any_zoomed());
        assert_eq!(graph.axis(plot), (fit, top));
        graph.reset_zoom();
        assert!(!graph.any_zoomed());
    }

    // The readout reads the axes as zoomed: the pointer's frequency stays
    // under it, and the plot's middle is no longer the full range's middle.
    // Bugs caught: 0, False positives: 0
    #[test]
    fn zoomed_axes_map_the_pointer() {
        let mut graph = Graph::default();
        graph.set(vec![flat(80.0)]);
        let plot = PlotBox::inside(1000.0, 600.0, INSETS);
        let x = plot.left + plot.width * 0.25;
        let y = plot.top + plot.height * 0.75;
        let (before, top) = graph.axis(plot);
        let (f, db) = (before.freq_at(x), before.db_at(y, top));
        graph.wheel(wheel(Axis::Frequency, x, 3.0));
        graph.wheel(wheel(Axis::Level, y, 3.0));
        let (after, top_after) = graph.axis(plot);
        assert!((after.freq_at(x) / f - 1.0).abs() < 1e-9);
        assert!((after.db_at(y, top_after) - db).abs() < 1e-9);
        assert!((after.x_of(f) - x).abs() < 1e-3);
        let mid = plot.left + plot.width / 2.0;
        assert!(after.freq_at(mid) < before.freq_at(mid));
    }

    // Bugs caught: 0, False positives: 1
    #[test]
    fn each_level_axis_zooms_in_to_its_least_span() {
        let plot = PlotBox::inside(1000.0, 600.0, INSETS);
        let cases = [
            (View::Response, None, MIN_DB_SPAN),
            (View::Phase, None, MIN_PHASE_SPAN),
            (View::Distortion, Some(Units::Percent), MIN_PCT_SPAN),
            (View::Distortion, Some(Units::Db), MIN_DB_SPAN),
        ];
        for (view, units, least) in cases {
            let mut graph = Graph::default();
            graph.set_view(view);
            if let Some(units) = units {
                graph.set_distortion(Some(DistortionPlot {
                    name: String::new(),
                    units,
                    curves: Vec::new(),
                }));
            }
            let fit = graph.axis(plot).0.span;
            graph.wheel(wheel(Axis::Level, plot.top + 50.0, 100.0));
            let (zoomed, _) = graph.axis(plot);
            assert!((zoomed.span - least).abs() < 1e-9, "{view:?} {units:?}");
            graph.wheel(wheel(Axis::Level, plot.top + 50.0, -200.0));
            let (out, _) = graph.axis(plot);
            assert!((out.span - 2.0 * fit).abs() < 1e-9, "{view:?} {units:?}");
            // A level zoom leaves the frequency axis alone.
            assert_eq!((out.f_low, out.f_high), (plot::F_MIN, plot::F_MAX));
        }
    }

    // A zoomed axis gets finer lines, and its labels still stand apart.
    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_zoomed_level_axis_has_finer_lines() {
        let plot = |span| PlotBox {
            span,
            ..sized(1000.0, 500.0)
        };
        assert_eq!(db_steps(&plot(50.0)), (DB_LINES, DB_LABELS));
        assert_eq!(db_steps(&plot(25.0)), (2.5, 5.0));
        assert_eq!(db_steps(&plot(5.0)), (0.5, 1.0));
        // The default rules for a 2× zoom out.
        assert_eq!(db_steps(&plot(100.0)), (DB_LINES, DB_LABELS));
        assert_eq!(phase_steps(&plot(360.0)), (PHASE_LINES, PHASE_MAJORS));
        assert_eq!(phase_steps(&plot(100.0)), (15.0, 45.0));
        assert_eq!(phase_steps(&plot(10.0)), (1.0, 5.0));
        assert!(on_step(2.5 * 3.0, 2.5) && !on_step(1.5, 1.0));
        assert!(percent_all_labels(&plot(1.0)));
        assert!(!percent_all_labels(&plot(PCT_SPAN)));
    }

    // Traces are cut where they leave the frequencies in view, and pinned
    // to the top and bottom edges.
    // Bugs caught: 0, False positives: 0
    #[test]
    fn runs_are_cut_at_the_plot_edges() {
        let plot = sized(100.0, 100.0);
        let (l, t) = (plot.left, plot.top);
        let at = |x: f32, y: f32| Point::new(l + x, t + y);
        let runs = cut_runs(
            [
                at(-50.0, 50.0),
                at(50.0, 50.0),
                at(150.0, -50.0),
                at(200.0, 0.0),
            ],
            &plot,
            true,
        );
        assert_eq!(runs, [vec![at(0.0, 50.0), at(50.0, 50.0), at(100.0, 0.0)]]);
        // A gap breaks a run; a point alone between gaps is a run of one.
        let nan = Point::new(f32::NAN, f32::NAN);
        let runs = cut_runs(
            [at(10.0, 10.0), at(20.0, 20.0), nan, at(30.0, 30.0), nan],
            &plot,
            true,
        );
        assert_eq!(
            runs,
            [vec![at(10.0, 10.0), at(20.0, 20.0)], vec![at(30.0, 30.0)]]
        );
        // Out and back in: two runs; beyond the bottom, pinned to it.
        let runs = cut_runs(
            [
                at(80.0, 10.0),
                at(120.0, 10.0),
                at(120.0, 30.0),
                at(80.0, 300.0),
            ],
            &plot,
            true,
        );
        assert_eq!(runs.len(), 2, "{runs:?}");
        assert_eq!(runs[0], [at(80.0, 10.0), at(100.0, 10.0)]);
        assert_eq!(runs[1].last(), Some(&at(80.0, 100.0)));
        // Wholly outside: nothing.
        assert!(cut_runs([at(-10.0, 0.0), at(-5.0, 0.0)], &plot, true).is_empty());
        // On a zoomed level axis, cut at the top and bottom too: up through
        // the top, along above it, and back down in.
        let runs = cut_runs(
            [
                at(0.0, 50.0),
                at(20.0, -50.0),
                at(60.0, -50.0),
                at(80.0, 50.0),
            ],
            &plot,
            false,
        );
        assert_eq!(
            runs,
            [
                vec![at(0.0, 50.0), at(10.0, 0.0)],
                vec![at(70.0, 0.0), at(80.0, 50.0)]
            ]
        );
        assert!(cut_runs([at(10.0, -5.0), at(50.0, -5.0)], &plot, false).is_empty());
    }
    /// A level wheel of `lines` notches at the middle of `plot`'s level axis.
    fn level_wheel(plot: PlotBox, lines: f64) -> Wheel {
        Wheel {
            axis: Axis::Level,
            plot,
            at: plot.top + plot.height / 2.0,
            lines,
        }
    }

    // After a resize, the next gesture starts from the range drawn at the
    // new size: zooming out never shrinks it, nor zooming in widens it.
    // Bugs caught: 1, False positives: 0
    #[test]
    fn a_wheel_gesture_after_a_resize_keeps_its_direction() {
        let mut graph = Graph::default();
        graph.set(vec![flat(80.0)]);
        let tall = PlotBox::inside(1000.0, 1000.0, INSETS);
        let short = PlotBox::inside(1000.0, 300.0, INSETS);
        graph.wheel(level_wheel(tall, -50.0));
        assert!((graph.axis(tall).0.span - 2.0 * tall.span).abs() < 1e-9);
        // Drawn within the short plot's own limits.
        let before = graph.axis(short).0.span;
        assert!(before <= 2.0 * short.span + 1e-9, "{before}");
        graph.wheel(level_wheel(short, -1.0));
        let out = graph.axis(short).0.span;
        assert!(out >= before - 1e-9, "{before} -> {out}");
        graph.wheel(level_wheel(short, 1.0));
        let back = graph.axis(short).0.span;
        assert!(back < out, "{out} -> {back}");
        // Zoomed in on the short plot, then made tall: drawn no narrower
        // than the tall plot allows, and the next notch in narrows it.
        graph.wheel(level_wheel(short, 50.0));
        let drawn = graph.axis(tall).0.span;
        assert!(drawn >= MIN_DB_SPAN - 1e-9, "{drawn}");
        graph.wheel(level_wheel(tall, 1.0));
        assert!(graph.axis(tall).0.span <= drawn + 1e-9);
    }

    // A short, wide plot whose automatic span is under the least zoomed
    // span: zooming in leaves it be, zooming out widens it.
    // Bugs caught: 1, False positives: 0
    #[test]
    fn a_fit_tighter_than_the_least_span_is_the_least() {
        let mut graph = Graph::default();
        graph.set(vec![flat(80.0)]);
        let thin = PlotBox::inside(2000.0, 100.0, INSETS);
        let fit = graph.axis(thin).0.span;
        assert!(fit < MIN_DB_SPAN, "{fit}");
        graph.wheel(level_wheel(thin, 1.0));
        assert!(graph.axis(thin).0.span <= fit + 1e-9);
        graph.wheel(level_wheel(thin, -1.0));
        assert!(graph.axis(thin).0.span > fit);
    }

    // With no distortion data there is no level axis to zoom, so no range
    // is saved to be read in other units later; the frequency axis still
    // zooms.
    // Bugs caught: 1, False positives: 0
    #[test]
    fn an_empty_distortion_view_ignores_the_level_wheel() {
        let mut graph = Graph::default();
        graph.set_view(View::Distortion);
        let plot = PlotBox::inside(1000.0, 600.0, INSETS);
        graph.wheel(level_wheel(plot, 2.0));
        assert!(!graph.is_zoomed());
        graph.wheel(wheel(Axis::Frequency, plot.left + 100.0, 2.0));
        assert!(graph.is_zoomed());
        assert_eq!(graph.zoom().level, None);
    }

    // Levels so large that the axis's ends round together: the wheel
    // saves nothing rather than a range without a span.
    // Bugs caught: 1, False positives: 0
    #[test]
    fn levels_too_large_for_a_span_are_not_zoomed() {
        let mut graph = Graph::default();
        graph.set(vec![flat(1e300)]);
        let plot = PlotBox::inside(1000.0, 600.0, INSETS);
        for lines in [1.0, -1.0, 30.0] {
            graph.wheel(level_wheel(plot, lines));
            assert!(!graph.is_zoomed(), "{lines}");
        }
    }

    /// The width of `content` in `font` at the axis labels' size.
    fn label_width(content: &str, font: iced::Font) -> f32 {
        use iced::advanced::text::Paragraph as _;
        let text = iced::advanced::Text {
            content,
            bounds: Size::new(f32::INFINITY, f32::INFINITY),
            size: LABEL_SIZE.into(),
            line_height: Default::default(),
            font,
            align_x: Alignment::Right,
            align_y: Vertical::Center,
            shaping: Text::default().shaping,
            wrapping: iced::widget::text::Wrapping::None,
        };
        iced::advanced::graphics::text::Paragraph::with_text(text)
            .min_bounds()
            .width
    }

    // Every percent label the axis can show, at any zoom, fits left of the
    // plot: they end 6 px from it, and keep 2 px from the canvas edge. In
    // the labels' face and in the bundled fallback.
    // Bugs caught: 1, False positives: 0
    #[test]
    fn every_percent_label_fits_the_left_inset() {
        let fit = Range::new(PCT_TOP - PCT_SPAN, PCT_TOP);
        let full = zoom::Limits::level(fit, MIN_PCT_SPAN).full;
        let room = INSETS.left - 6.0 - 2.0;
        for font in [theme::mono(), iced::Font::with_name(theme::FALLBACK_FAMILY)] {
            let mut widest = (0.0, String::new());
            for decade in full.low.floor() as i32..=full.high.floor() as i32 {
                for m in 1..=9 {
                    let tick = percent_tick(decade, m);
                    let width = label_width(&tick, font);
                    if width > widest.0 {
                        widest = (width, tick);
                    }
                }
            }
            assert!(widest.0 > 0.0);
            assert!(widest.0 <= room, "{font:?}: {widest:?} wider than {room}");
        }
    }

    // On a short plot zoomed fully out, the decade labels thin out to stand
    // a row apart; the lines stay.
    // Bugs caught: 0, False positives: 1
    #[test]
    fn percent_labels_thin_out_when_a_decade_is_short() {
        let plot = PlotBox {
            span: 2.0 * PCT_SPAN,
            ..sized(800.0, 64.0)
        };
        let lines = percent_lines(&plot, PCT_TOP + PCT_SPAN / 2.0);
        let ys: Vec<f32> = lines
            .iter()
            .filter(|l| l.2.is_some())
            .map(|l| l.0)
            .collect();
        assert!(ys.len() >= 2, "{lines:?}");
        assert!(
            ys.windows(2)
                .all(|w| (w[1] - w[0]).abs() >= LABEL_SIZE + 4.0),
            "{ys:?}"
        );
        assert_eq!(lines.iter().filter(|l| l.1).count(), 9);
        // The default fit on a tall plot labels every decade.
        let tall = PlotBox {
            span: PCT_SPAN,
            ..sized(1000.0, 600.0)
        };
        let labels: Vec<String> = percent_lines(&tall, PCT_TOP)
            .into_iter()
            .filter(|l| l.1)
            .filter_map(|l| l.2)
            .collect();
        assert_eq!(labels, ["0.01%", "0.1%", "1%", "10%", "100%"]);
    }

    // A curve's name sits where its drawn line ends: none for a harmonic
    // wholly outside the zoomed view, the edge crossing for one leaving it.
    // Bugs caught: 0, False positives: 0
    #[test]
    fn distortion_names_follow_the_visible_line() {
        let freq = vec![100.0, 400.0, 800.0, 3000.0, 6000.0];
        let fundamental = Response {
            freq: freq.clone(),
            db: vec![0.0; 5],
            phase: None,
        };
        let level = |db: Vec<f64>| Response {
            freq: freq.clone(),
            db,
            phase: None,
        };
        // Data up to 800 Hz only.
        let early = level(vec![-40.0, -40.0, -40.0, f64::NAN, f64::NAN]);
        let early = Curve::new("H3", Ink::Fg, &early, &fundamental, Units::Db);
        // Rising through 2 kHz, the right edge.
        let rising = level(vec![-60.0, -60.0, -60.0, -40.0, -40.0]);
        let rising = Curve::new("H2", Ink::Fg, &rising, &fundamental, Units::Db);
        let plot = sized(500.0, 400.0).with_decades(3.0, 2000f64.log10());
        let top = 0.0;
        assert_eq!(line_end(&curve_runs(&early, &plot, top, true)), None);
        let end = line_end(&curve_runs(&rising, &plot, top, true)).unwrap();
        assert!((end.x - plot.right()).abs() < 1e-3, "{end:?}");
        // Between the levels at 800 Hz and 3 kHz, not the one at 800 Hz.
        let at_800 = plot.y_of(-60.0, top);
        assert!(end.y < at_800 - 1.0, "{end:?}");
        // Wholly above a zoomed level axis: cut, so no name either.
        let low = PlotBox { span: 10.0, ..plot };
        assert_eq!(line_end(&curve_runs(&rising, &low, -70.0, false)), None);
    }
}
