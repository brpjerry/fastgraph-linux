//! The graph drawn off-screen for a PNG export: a copy of the graph with
//! the export's axis ranges and, if asked for, a legend in a corner of the
//! plot.
use super::{Graph, INSETS, Ink, LABEL_SIZE, THD_WIDTH, TRACE_WIDTH, View, cut, plotted};
use crate::plot::PlotBox;
use crate::theme::{self, Palette};
use crate::zoom::{Range, ViewZoom};
use iced::alignment::Vertical;
use iced::widget::canvas::{self, Cache, Frame, Path, Stroke, Text};
use iced::widget::text::Alignment;
use iced::{Point, Size};
use std::cell::Cell;

/// What an export copy of the graph draws differently.
#[derive(Debug, Clone, Default)]
pub(crate) struct Export {
    /// Traces beyond the level axis are pinned to its edge, as on the
    /// automatic fit, rather than cut.
    pub pins: bool,
    /// The legend's rows, top to bottom; `None` draws no legend.
    pub legend: Option<Vec<LegendEntry>>,
}

/// A legend row: a line sample in a curve's ink and style, and its name.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LegendEntry {
    pub name: String,
    pub ink: Ink,
    pub dashed: bool,
    pub thick: bool,
}

/// The legend's text size, row height, inner padding, line sample length
/// and the gap after it, and its distance from the plot's edges.
const TEXT_SIZE: f32 = 12.0;
const ROW: f32 = 18.0;
const PAD: f32 = 8.0;
const SAMPLE: f32 = 20.0;
const GAP: f32 = 6.0;
const MARGIN: f32 = 8.0;
/// The most characters of a name in the legend.
const NAME_CHARS: usize = 40;

impl Graph {
    /// The current view's axes as last drawn: the frequencies in decades
    /// (log10 Hz), the level range (dB, degrees or the decades of a
    /// percentage), and whether the level is the automatic fit.
    pub(crate) fn shown_axes(&self) -> (Range, Range, bool) {
        let span = self.drawn_span.get();
        // Before the first draw: the default export's shape.
        let plot = PlotBox::inside(1920.0, 1080.0, INSETS);
        let plot = if span > 0.0 {
            PlotBox { span, ..plot }
        } else {
            plot
        };
        let (axis, top) = self.axis(plot);
        let freq = Range::new(axis.f_low.log10(), axis.f_high.log10());
        (freq, Range::new(top - axis.span, top), self.pins())
    }

    /// A copy of what the graph shows, for drawing off-screen: the current
    /// view over `freq` (decades) and `level`, with `export`'s legend.
    pub(crate) fn export_copy(&self, freq: Range, level: Range, export: Export) -> Graph {
        let mut zooms = [ViewZoom::default(); super::VIEWS.len()];
        zooms[super::index(self.view)] = ViewZoom {
            freq: Some(freq),
            level: Some(level),
        };
        Graph {
            view: self.view,
            traces: self.traces.clone(),
            phase: self.phase.clone(),
            no_phase: self.no_phase,
            unwrapped: self.unwrapped.clone(),
            phase_axis: self.phase_axis,
            distortion: self.distortion.clone(),
            live: self.live.clone(),
            extent: self.extent,
            drawn_span: Cell::new(self.drawn_span.get()),
            distortion_note: self.distortion_note.clone(),
            zooms,
            // A copy is never panned: no drag on it outlives the copy.
            generation: 0,
            export: Some(export),
            cache: Cache::new(),
            live_cache: Cache::new(),
            #[cfg(test)]
            drawn: Cell::new(0),
            #[cfg(test)]
            live_drawn: Cell::new(0),
        }
    }

    /// The legend's rows for the Distortion view: its curves.
    pub(crate) fn distortion_legend(&self) -> Vec<LegendEntry> {
        self.distortion
            .iter()
            .flat_map(|d| &d.curves)
            .map(|c| LegendEntry {
                name: c.label.clone(),
                ink: c.ink,
                dashed: c.dashed,
                thick: c.thick,
            })
            .collect()
    }
}

/// The points of every line the current view draws, in the canvas.
fn line_points(plot: &PlotBox, graph: &Graph, top: f64) -> Vec<Point> {
    let at = |(f, y): (f64, f64)| Point::new(plot.x_of(f), plot.y_of(y, top));
    match graph.view {
        View::Response => graph
            .traces
            .iter()
            .map(|t| &t.response)
            .chain(&graph.live)
            .flat_map(|r| plotted(r).map(at).collect::<Vec<_>>())
            .collect(),
        View::Phase => {
            let (traces, wrapped) = graph.phase_shown();
            traces
                .iter()
                .flat_map(|t| {
                    if wrapped {
                        t.plotted()
                    } else {
                        t.freq.iter().copied().zip(t.deg.iter().copied()).collect()
                    }
                })
                .map(at)
                .collect()
        }
        View::Distortion => graph
            .distortion
            .iter()
            .flat_map(|d| &d.curves)
            .flat_map(|c| c.plotted().map(at).collect::<Vec<_>>())
            .collect(),
    }
}

/// The width of `content` in `font` at `size`.
pub(super) fn text_width(content: &str, font: iced::Font, size: f32) -> f32 {
    use iced::advanced::text::Paragraph as _;
    let text = iced::advanced::Text {
        content,
        bounds: Size::new(f32::INFINITY, f32::INFINITY),
        size: size.into(),
        line_height: Default::default(),
        font,
        align_x: Alignment::Left,
        align_y: Vertical::Center,
        shaping: Text::default().shaping,
        wrapping: iced::widget::text::Wrapping::None,
    };
    iced::advanced::graphics::text::Paragraph::with_text(text)
        .min_bounds()
        .width
}

/// The corner of `plot` where a box of `size` covers the fewest of
/// `points`; on a tie the first of top right, top left, bottom right and
/// bottom left. The bottom left holds a caption in the Phase and
/// Distortion views, so it comes last there.
fn corner(plot: &PlotBox, size: Size, points: &[Point], caption: bool) -> Point {
    let (left, right) = (plot.left + MARGIN, plot.right() - MARGIN - size.width);
    let (top, bottom) = (plot.top + MARGIN, plot.bottom() - MARGIN - size.height);
    let corners = [
        Point::new(right, top),
        Point::new(left, top),
        Point::new(right, bottom),
        Point::new(left, bottom),
    ];
    let covered = |at: &Point| {
        points
            .iter()
            .filter(|p| {
                (at.x..=at.x + size.width).contains(&p.x)
                    && (at.y..=at.y + size.height).contains(&p.y)
            })
            .count()
    };
    let usable = if caption { 3 } else { 4 };
    corners[..usable]
        .iter()
        .copied()
        .min_by_key(covered)
        .unwrap_or(corners[0])
}

/// The legend: each row's line sample and name in a box in the corner of
/// the plot its lines cross least, with "+N more" when the rows outgrow
/// the plot's height.
pub(super) fn draw_legend(
    frame: &mut Frame,
    plot: &PlotBox,
    graph: &Graph,
    top: f64,
    rows: &[LegendEntry],
    p: &Palette,
) {
    if rows.is_empty() {
        return;
    }
    let room = ((plot.height - 2.0 * (MARGIN + PAD)) / ROW)
        .floor()
        .max(0.0) as usize;
    if room == 0 {
        return;
    }
    let (shown, more) = if rows.len() > room {
        (room - 1, rows.len() - (room - 1))
    } else {
        (rows.len(), 0)
    };
    let font = theme::ui();
    // Within the legend's widest box, shortened further where they need it.
    let name_room = plot.width - 2.0 * (MARGIN + PAD) - SAMPLE - GAP;
    let names: Vec<String> = rows[..shown]
        .iter()
        .map(|r| {
            let name = r.name.split_whitespace().collect::<Vec<_>>().join(" ");
            let mut name = cut(&name, NAME_CHARS);
            let mut chars = name.chars().count();
            while chars > 1 && text_width(&name, font, TEXT_SIZE) > name_room {
                chars -= 1;
                name = cut(&name, chars);
            }
            name
        })
        .collect();
    let more = (more > 0).then(|| format!("+{more} more"));
    let widest = names
        .iter()
        .chain(&more)
        .map(|n| text_width(n, font, TEXT_SIZE))
        .fold(0.0, f32::max);
    let lines = shown + usize::from(more.is_some());
    let size = Size::new(
        (2.0 * PAD + SAMPLE + GAP + widest.ceil()).min(plot.width - 2.0 * MARGIN),
        2.0 * PAD + ROW * lines as f32,
    );
    let caption = graph.view != View::Response;
    let at = corner(plot, size, &line_points(plot, graph, top), caption);
    let card = Path::rounded_rectangle(at, size, theme::RADIUS.into());
    frame.fill(&card, p.bg);
    frame.stroke(
        &card,
        Stroke::default()
            .with_color(p.border)
            .with_width(theme::HAIRLINE),
    );
    let ink = |ink: Ink| match ink {
        Ink::Trace(i) => p.trace(i),
        Ink::Fg => p.fg,
        Ink::Dim => p.fg_dim,
    };
    for (i, (row, name)) in rows.iter().zip(&names).enumerate() {
        let y = at.y + PAD + ROW * (i as f32 + 0.5);
        let x = at.x + PAD;
        let stroke = Stroke {
            line_dash: if row.dashed {
                canvas::LineDash {
                    segments: &[6.0, 4.0],
                    offset: 0,
                }
            } else {
                canvas::LineDash::default()
            },
            ..Stroke::default()
                .with_color(ink(row.ink))
                .with_width(if row.thick { THD_WIDTH } else { TRACE_WIDTH })
        };
        frame.stroke(
            &Path::line(Point::new(x, y), Point::new(x + SAMPLE, y)),
            stroke,
        );
        frame.fill_text(Text {
            content: name.clone(),
            position: Point::new(x + SAMPLE + GAP, y),
            color: p.fg,
            size: TEXT_SIZE.into(),
            font,
            align_x: Alignment::Left,
            align_y: Vertical::Center,
            ..Text::default()
        });
    }
    if let Some(more) = more {
        let y = at.y + PAD + ROW * (shown as f32 + 0.5);
        frame.fill_text(Text {
            content: more,
            position: Point::new(at.x + PAD + SAMPLE + GAP, y),
            color: p.fg_dim,
            size: LABEL_SIZE.into(),
            font,
            align_x: Alignment::Left,
            align_y: Vertical::Center,
            ..Text::default()
        });
    }
}
