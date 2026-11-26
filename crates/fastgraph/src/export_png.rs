//! Export PNG: the options panel over the graph, prefilled from the view,
//! and the image of the graph drawn off-screen, off the UI thread, by the
//! same canvas code as the window (with the tiny-skia renderer, which needs
//! no GPU), then encoded as PNG.
use crate::app::{App, Message};
use crate::graph::{self, Export, Graph, Ink, LegendEntry, Units, View};
use crate::plot::{F_MAX, F_MIN};
use crate::theme;
use crate::zoom::Range;
use fastgraph_core::prefs::{Prefs, format_hz, parse_hz};
use fastgraph_core::set::safe_file_name;
use iced::advanced::renderer::{self, Headless};
use iced::futures::FutureExt as _;
use iced::widget::{
    Space, button, center, checkbox, column, container, opaque, row, scrollable, text, text_input,
};
use iced::{Alignment, Color, Element, Length, Size, Task, Theme};
use iced_runtime::user_interface::{self, UserInterface};
use std::path::{Path, PathBuf};

/// The size presets, in pixels, and the default among them.
pub const SIZES: [(u32, u32); 3] = [(1280, 720), (1920, 1080), (3840, 2160)];
pub const DEFAULT_SIZE: (u32, u32) = (1920, 1080);
/// The least and the most pixels on a side.
pub const MIN_SIDE: u32 = 320;
pub const MAX_SIDE: u32 = 8192;
/// The size beyond which the image is drawn scaled up, so text and lines
/// keep their weight against the plot; a smaller image is drawn at scale 1.
const BASE: Size = Size::new(1920.0, 1080.0);
/// How far from zero the level fields reach, in dB and in degrees, and the
/// percent range: further out the grid would have no lines worth drawing.
const MAX_DB: f64 = 1000.0;
const MAX_DEG: f64 = 100_000.0;
const PCT_RANGE: (f64, f64) = (1e-9, 1e9);
/// The live trace's name in the legend.
pub const LIVE_NAME: &str = "RTA live";

/// What the panel's controls send.
#[derive(Debug, Clone)]
pub enum Event {
    /// The Export PNG… button.
    Open,
    Input(Field, String),
    /// A size preset.
    Preset(u32, u32),
    Legend(bool),
    Export,
    Cancel,
}

/// A text field of the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    FreqMin,
    FreqMax,
    LevelMin,
    LevelMax,
    Width,
    Height,
}

/// The level axis's unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Db,
    Degrees,
    Percent,
}

impl Unit {
    fn of(view: View, units: Units) -> Self {
        match view {
            View::Response => Unit::Db,
            View::Phase => Unit::Degrees,
            View::Distortion if units == Units::Percent => Unit::Percent,
            View::Distortion => Unit::Db,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Unit::Db => "dB",
            Unit::Degrees => "°",
            Unit::Percent => "%",
        }
    }
}

/// The panel's state: its fields as typed.
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub view: View,
    pub unit: Unit,
    pub freq_min: String,
    pub freq_max: String,
    pub level_min: String,
    pub level_max: String,
    pub width: String,
    pub height: String,
    pub legend: bool,
    /// The level fields as prefilled from an automatic fit, if it was one:
    /// left so, traces beyond them are pinned to the edge as on screen.
    fit: Option<(String, String)>,
}

/// Valid options, as the export draws them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Checked {
    /// In decades (log10 Hz).
    pub freq: Range,
    /// In dB, degrees, or the decades of a percentage.
    pub level: Range,
    pub size: (u32, u32),
    pub legend: bool,
    pub pins: bool,
}

/// `x` to `digits` significant digits, as short as it reads back.
fn significant(x: f64, digits: i32) -> f64 {
    if x == 0.0 || !x.is_finite() {
        return x;
    }
    let decimals = (digits - 1 - x.abs().log10().floor() as i32).max(0) as usize;
    format!("{x:.decimals$}").parse().unwrap_or(x)
}

/// A level for its field: tenths of a dB or degree, or a percentage to
/// four significant digits.
fn level_text(value: f64, unit: Unit) -> String {
    let rounded = match unit {
        Unit::Percent => significant(10f64.powf(value), 4),
        Unit::Db | Unit::Degrees => (value * 10.0).round() / 10.0,
    };
    // No "-0".
    (rounded + 0.0).to_string()
}

/// A frequency for its field, to four significant digits, inside the
/// plotted decades.
fn hz_text(decades: f64) -> String {
    format_hz(significant(10f64.powf(decades), 4).clamp(F_MIN, F_MAX))
}

impl Options {
    /// The options for the graph's current view, its axes as shown, at
    /// `size` with or without the legend.
    pub fn of(graph: &Graph, view: View, units: Units, size: (u32, u32), legend: bool) -> Self {
        let (freq, level, fit) = graph.shown_axes();
        let unit = Unit::of(view, units);
        let (level_min, level_max) = (level_text(level.low, unit), level_text(level.high, unit));
        Self {
            view,
            unit,
            freq_min: hz_text(freq.low),
            freq_max: hz_text(freq.high),
            fit: fit.then(|| (level_min.clone(), level_max.clone())),
            level_min,
            level_max,
            width: size.0.to_string(),
            height: size.1.to_string(),
            legend,
        }
    }

    fn field(&mut self, field: Field) -> &mut String {
        match field {
            Field::FreqMin => &mut self.freq_min,
            Field::FreqMax => &mut self.freq_max,
            Field::LevelMin => &mut self.level_min,
            Field::LevelMax => &mut self.level_max,
            Field::Width => &mut self.width,
            Field::Height => &mut self.height,
        }
    }

    /// The size as typed, if both sides are valid.
    fn size(&self) -> Option<(u32, u32)> {
        Some((side(&self.width)?, side(&self.height)?))
    }

    /// A level field's value on the axis, if valid alone.
    fn level(&self, text: &str) -> Option<f64> {
        let value: f64 = text.trim().parse().ok().filter(|v: &f64| v.is_finite())?;
        match self.unit {
            Unit::Db => (value.abs() <= MAX_DB).then_some(value),
            Unit::Degrees => (value.abs() <= MAX_DEG).then_some(value),
            Unit::Percent => (PCT_RANGE.0..=PCT_RANGE.1)
                .contains(&value)
                .then(|| value.log10()),
        }
    }

    /// The fields that do not hold a valid value: one out of its range, or
    /// both of a pair whose minimum is not below its maximum.
    pub fn invalid(&self) -> Vec<Field> {
        let mut bad = Vec::new();
        // Compared in decades, as drawn: frequencies equal there are no range.
        let hz = |t: &str| {
            parse_hz(t)
                .filter(|f| (F_MIN..=F_MAX).contains(f))
                .map(f64::log10)
        };
        let mut pair = |low: Option<f64>, high: Option<f64>, fields: [Field; 2]| match (low, high) {
            (Some(l), Some(h)) if l < h => {}
            (Some(_), Some(_)) => bad.extend(fields),
            (l, h) => {
                if l.is_none() {
                    bad.push(fields[0]);
                }
                if h.is_none() {
                    bad.push(fields[1]);
                }
            }
        };
        pair(
            hz(&self.freq_min),
            hz(&self.freq_max),
            [Field::FreqMin, Field::FreqMax],
        );
        pair(
            self.level(&self.level_min),
            self.level(&self.level_max),
            [Field::LevelMin, Field::LevelMax],
        );
        if side(&self.width).is_none() {
            bad.push(Field::Width);
        }
        if side(&self.height).is_none() {
            bad.push(Field::Height);
        }
        bad
    }

    /// The options as the export draws them, if every field is valid.
    pub fn checked(&self) -> Option<Checked> {
        if !self.invalid().is_empty() {
            return None;
        }
        let low = parse_hz(&self.freq_min)?.log10();
        let high = parse_hz(&self.freq_max)?.log10();
        let level = Range::new(self.level(&self.level_min)?, self.level(&self.level_max)?);
        // The fit's numbers, however they are spelled.
        let untouched = |(min, max): &(String, String)| {
            self.level(min) == Some(level.low) && self.level(max) == Some(level.high)
        };
        Some(Checked {
            freq: Range::new(low, high),
            level,
            size: self.size()?,
            legend: self.legend,
            pins: self.fit.as_ref().is_some_and(untouched),
        })
    }
}

/// A side's pixels, if valid.
fn side(text: &str) -> Option<u32> {
    text.trim()
        .parse()
        .ok()
        .filter(|n| (MIN_SIDE..=MAX_SIDE).contains(n))
}

/// `<set> - <view>.png`, or `<view>.png` for a session with no set name.
pub fn file_name(set_name: &str, view: View) -> String {
    let view = graph::VIEWS
        .iter()
        .find(|(v, _)| *v == view)
        .map_or("Graph", |(_, label)| label);
    if set_name.trim().is_empty() {
        format!("{view}.png")
    } else {
        format!("{} - {view}.png", safe_file_name(set_name))
    }
}

/// The scale `size` is drawn at: 1 up to [`BASE`], more to fill a larger
/// image at the same proportions.
fn scale(size: (u32, u32)) -> f32 {
    let (w, h) = (size.0 as f32, size.1 as f32);
    (w / BASE.width).min(h / BASE.height).max(1.0)
}

/// `graph` as shown in the window (without the hover, the scroll bars or
/// Reset view) drawn
/// at `size` pixels in the theme `prefs` describe, as RGBA rows; `rows`:
/// the list has measurements (for the empty graph's note).
pub fn render(graph: &Graph, prefs: &Prefs, size: (u32, u32), rows: bool) -> Vec<u8> {
    // Built on this thread, so the palette has the custom trace colours.
    let theme: Theme = theme::from_prefs(prefs);
    let scale = scale(size);
    let logical = Size::new(size.0 as f32 / scale, size.1 as f32 / scale);
    // Ready at once (the wgpu half declines "tiny-skia" before any await),
    // so it is polled here rather than run on an executor: this runs on
    // the worker thread.
    let mut renderer =
        <iced::Renderer as Headless>::new(theme::ui(), theme::TEXT.into(), Some("tiny-skia"))
            .now_or_never()
            .flatten()
            .expect("the tiny-skia renderer is made at once");
    let mut ui = UserInterface::build(
        graph.view(None, rows),
        logical,
        user_interface::Cache::default(),
        &mut renderer,
    );
    let base = iced::theme::Base::base(&theme);
    ui.draw(
        &mut renderer,
        &theme,
        &renderer::Style {
            text_color: base.text_color,
        },
        iced::mouse::Cursor::Unavailable,
    );
    renderer.screenshot(Size::new(size.0, size.1), scale, base.background_color)
}

/// RGBA rows of `size` as a PNG file.
pub fn encode(rgba: &[u8], size: (u32, u32)) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, size.0, size.1);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(rgba).map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())?;
    Ok(out)
}

/// The image of `graph` at `size` as PNG bytes.
pub fn png(graph: &Graph, prefs: &Prefs, size: (u32, u32), rows: bool) -> Result<Vec<u8>, String> {
    encode(&render(graph, prefs, size, rows), size)
}

/// The file name of a path, for messages.
fn file_label(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into(),
    )
}

/// Render `graph` and write it to `path`, creating the folder `dir` first
/// when given.
fn write(
    graph: &Graph,
    prefs: &Prefs,
    checked: Checked,
    rows: bool,
    path: &Path,
    dir: Option<&Path>,
) -> Result<PathBuf, String> {
    let bytes = png(graph, prefs, checked.size, rows)?;
    if let Some(dir) = dir {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    fastgraph_core::fsutil::write_atomic(path, &bytes)
        .map(|()| path.to_path_buf())
        .map_err(|e| format!("{}: {e:#}", file_label(path)))
}

impl App {
    pub(crate) fn png_event(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Open => self.png_open(),
            Event::Input(field, value) => {
                if let Some(o) = &mut self.png {
                    *o.field(field) = value;
                }
            }
            Event::Preset(w, h) => {
                if let Some(o) = &mut self.png {
                    o.width = w.to_string();
                    o.height = h.to_string();
                }
            }
            Event::Legend(on) => {
                if let Some(o) = &mut self.png {
                    o.legend = on;
                }
            }
            Event::Cancel => self.png = None,
            Event::Export => return self.png_export(),
        }
        Task::none()
    }

    /// Open the panel prefilled from the current view.
    fn png_open(&mut self) {
        self.png = Some(Options::of(
            &self.graph,
            self.graph_view,
            self.units,
            DEFAULT_SIZE,
            true,
        ));
    }

    /// The view or its level's unit changed under an open panel: its axes
    /// follow, its size as typed and its legend stay. Otherwise it is left
    /// as typed.
    pub(crate) fn png_refill(&mut self) {
        let unit = Unit::of(self.graph_view, self.units);
        let Some(o) = self
            .png
            .as_mut()
            .filter(|o| o.view != self.graph_view || o.unit != unit)
        else {
            return;
        };
        // The size as typed, valid or not.
        let (width, height) = (std::mem::take(&mut o.width), std::mem::take(&mut o.height));
        *o = Options {
            width,
            height,
            ..Options::of(
                &self.graph,
                self.graph_view,
                self.units,
                DEFAULT_SIZE,
                o.legend,
            )
        };
    }

    /// The legend's rows for the current view: the shown measurements it
    /// draws (and the RTA's live trace), or the distortion curves.
    pub(crate) fn legend_rows(&self) -> Vec<LegendEntry> {
        let shown = self.items.iter().filter(|i| i.shown);
        let trace = |i: &crate::app::Item| LegendEntry {
            name: i.m.meta.name.clone(),
            ink: Ink::Trace(i.color),
            dashed: false,
            thick: false,
        };
        match self.graph_view {
            View::Response => shown
                .map(trace)
                .chain(self.rta.live.as_ref().map(|_| LegendEntry {
                    name: LIVE_NAME.into(),
                    ink: Ink::Fg,
                    dashed: true,
                    thick: false,
                }))
                .collect(),
            View::Phase => shown
                .filter(|i| {
                    let r = &i.m.response;
                    r.phase.as_ref().is_some_and(|p| p.len() == r.freq.len())
                })
                .map(trace)
                .collect(),
            View::Distortion => self.graph.distortion_legend(),
        }
    }

    /// Export: draw the view with the panel's options on the worker and
    /// write it to the exports folder, or where the save dialog says when
    /// "Ask where to export" is on.
    fn png_export(&mut self) -> Task<Message> {
        let Some(checked) = self.png.as_ref().and_then(Options::checked) else {
            return Task::none();
        };
        self.png = None;
        let export = Export {
            pins: checked.pins,
            legend: checked.legend.then(|| self.legend_rows()),
        };
        let graph = self.graph.export_copy(checked.freq, checked.level, export);
        let prefs = self.prefs.clone();
        let rows = !self.items.is_empty();
        let name = file_name(&self.set_name, self.graph_view);
        let dir = match self.prefs.exports_dir() {
            Ok(dir) => dir,
            Err(e) => return Task::done(Message::Exported(Err(format!("{e:#}")))),
        };
        // On the worker, so exports finish in the order asked for and the
        // close waits for them.
        let job = move |path: PathBuf, dir: Option<PathBuf>| -> crate::store_queue::Job {
            Box::new(move || {
                let saved = write(&graph, &prefs, checked, rows, &path, dir.as_deref());
                Some(Message::Exported(saved))
            })
        };
        if !self.prefs.ask_export {
            self.info(format!("Exporting {name}…"));
            self.worker.submit(job(dir.join(&name), Some(dir)));
            return Task::none();
        }
        // The dialog is not on the worker; what it picks is queued there,
        // unless the window has closed meanwhile.
        let worker = self.worker.sender();
        Task::future(async move {
            let file = rfd::AsyncFileDialog::new()
                .set_title("Export PNG")
                .set_directory(&dir)
                .set_file_name(name)
                .add_filter("PNG image", &["png"])
                .save_file()
                .await;
            if let (Some(file), Some(worker)) = (file, worker.upgrade()) {
                let _ = worker.send(job(file.path().to_path_buf(), None));
            }
        })
        .discard()
    }
}

/// The Export PNG… button.
pub fn button_view<'a>() -> Element<'a, Message> {
    button(text(crate::toolbar::PNG_LABEL))
        .on_press(Message::Png(Event::Open))
        .padding([4, 10])
        .style(theme::button)
        .into()
}

/// A field's label, in the left column.
const LABEL_WIDTH: f32 = 74.0;
const FIELD_WIDTH: f32 = 76.0;

fn label<'a>(content: &'a str) -> Element<'a, Message> {
    text(content)
        .size(theme::TEXT_SMALL)
        .style(theme::text(|p| p.fg_dim))
        .width(LABEL_WIDTH)
        .into()
}

/// A row of the panel.
fn line<'a>(items: Vec<Element<'a, Message>>) -> Element<'a, Message> {
    row(items).spacing(8).align_y(Alignment::Center).into()
}

fn dim<'a>(content: &'a str) -> Element<'a, Message> {
    text(content).style(theme::text(|p| p.fg_dim)).into()
}

/// The panel over the graph: the options and Export and Cancel, on a scrim
/// that keeps the pointer off the graph.
pub fn panel(o: &Options) -> Element<'_, Message> {
    let bad = o.invalid();
    let field = |placeholder: &'static str, value: &str, f: Field| -> Element<'_, Message> {
        text_input(placeholder, value)
            .on_input(move |t| Message::Png(Event::Input(f, t)))
            .on_submit_maybe(bad.is_empty().then_some(Message::Png(Event::Export)))
            .size(theme::TEXT)
            .padding([4, 6])
            .width(FIELD_WIDTH)
            .style(theme::input_checked(!bad.contains(&f)))
            .into()
    };
    let view = graph::VIEWS
        .iter()
        .find(|(v, _)| *v == o.view)
        .map_or("", |(_, label)| label);
    let presets = row(SIZES.map(|(w, h)| {
        let current = o.width.trim() == w.to_string() && o.height.trim() == h.to_string();
        button(text(format!("{w} × {h}")))
            .on_press(Message::Png(Event::Preset(w, h)))
            .padding([4, 10])
            .style(theme::segment(current))
            .into()
    }))
    .spacing(2);
    let actions = row![
        Space::new().width(Length::Fill),
        button(text("Cancel"))
            .on_press(Message::Png(Event::Cancel))
            .padding([4, 10])
            .style(theme::button),
        button(text("Export"))
            .on_press_maybe(bad.is_empty().then_some(Message::Png(Event::Export)))
            .padding([4, 10])
            .style(theme::primary),
    ]
    .spacing(10)
    .align_y(Alignment::Center);
    let card = column![
        row![
            text("Export PNG"),
            text(view).style(theme::text(|p| p.fg_dim))
        ]
        .spacing(8),
        line(vec![
            label("Frequency"),
            field("20", &o.freq_min, Field::FreqMin),
            dim("–"),
            field("20k", &o.freq_max, Field::FreqMax),
            dim("Hz"),
        ]),
        line(vec![
            label("Level"),
            field("min", &o.level_min, Field::LevelMin),
            dim("–"),
            field("max", &o.level_max, Field::LevelMax),
            dim(o.unit.label()),
        ]),
        line(vec![label("Size"), presets.into()]),
        line(vec![
            Space::new().width(LABEL_WIDTH).into(),
            field("1920", &o.width, Field::Width),
            dim("×"),
            field("1080", &o.height, Field::Height),
            dim("px"),
        ]),
        line(vec![
            Space::new().width(LABEL_WIDTH).into(),
            checkbox(o.legend)
                .label("Legend")
                .on_toggle(|on| Message::Png(Event::Legend(on)))
                .size(14)
                .style(theme::check)
                .into(),
        ]),
        actions,
    ]
    .spacing(12)
    .width(Length::Fixed(420.0));
    let card = container(card).padding(16).style(theme::card);
    // Where the graph is smaller than the card, the card scrolls.
    let card = scrollable(card)
        .direction(scrollable::Direction::Both {
            vertical: scrollable::Scrollbar::default(),
            horizontal: scrollable::Scrollbar::default(),
        })
        .width(Length::Shrink)
        .height(Length::Shrink)
        .style(theme::scrollbar);
    opaque(center(card).style(scrim))
}

/// The graph dimmed behind the panel.
fn scrim(theme: &Theme) -> container::Style {
    let bg = theme::Palette::of(theme).bg;
    container::Style::default().background(Color { a: 0.7, ..bg })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Axis, INSETS, Trace, Wheel};
    use crate::plot::PlotBox;
    use crate::theme::{Palette, ThemeMode};
    use fastgraph_core::response::{Response, log_grid};

    fn options() -> Options {
        Options::of(
            &Graph::default(),
            View::Response,
            Units::Percent,
            DEFAULT_SIZE,
            true,
        )
    }

    fn with(mut o: Options, field: Field, value: &str) -> Options {
        *o.field(field) = value.into();
        o
    }

    fn graph() -> Graph {
        let freq = log_grid(48_000);
        // A slope, so the traces cross the plot.
        let db: Vec<f64> = freq.iter().map(|f| -10.0 * (f / 1000.0).log10()).collect();
        let mut graph = Graph::default();
        graph.set(vec![
            Trace {
                name: "Left".into(),
                response: Response {
                    freq: freq.clone(),
                    db: db.clone(),
                    phase: None,
                },
                color: 0,
            },
            Trace {
                name: "Right".into(),
                response: Response {
                    db: db.iter().map(|d| d - 6.0).collect(),
                    freq,
                    phase: None,
                },
                color: 1,
            },
        ]);
        graph
    }

    fn legend() -> Vec<LegendEntry> {
        ["Left", "Right"]
            .iter()
            .enumerate()
            .map(|(i, name)| LegendEntry {
                name: (*name).into(),
                ink: Ink::Trace(i),
                dashed: false,
                thick: false,
            })
            .collect()
    }

    /// `graph` drawn over its current view's axes, with or without the legend.
    fn copy(graph: &Graph, legend: bool) -> Graph {
        let checked = Options::of(graph, View::Response, Units::Percent, DEFAULT_SIZE, legend)
            .checked()
            .unwrap();
        let export = Export {
            pins: checked.pins,
            legend: legend.then(legend_rows),
        };
        graph.export_copy(checked.freq, checked.level, export)
    }

    fn legend_rows() -> Vec<LegendEntry> {
        legend()
    }

    fn decode(bytes: &[u8]) -> ((u32, u32), Vec<u8>) {
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        buf.truncate(info.buffer_size());
        ((info.width, info.height), buf)
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn the_options_check_ranges_and_sizes() {
        let o = options();
        assert_eq!(o.invalid(), []);
        assert!(o.checked().is_some());
        // Min not below max: both fields of the pair.
        let o2 = with(with(options(), Field::FreqMin, "2k"), Field::FreqMax, "1k");
        assert_eq!(o2.invalid(), [Field::FreqMin, Field::FreqMax]);
        assert!(o2.checked().is_none());
        let level = with(with(options(), Field::LevelMin, "5"), Field::LevelMax, "5");
        assert_eq!(level.invalid(), [Field::LevelMin, Field::LevelMax]);
        // Unreadable or out of range: that field alone.
        assert_eq!(
            with(options(), Field::FreqMin, "abc").invalid(),
            [Field::FreqMin]
        );
        assert_eq!(
            with(options(), Field::FreqMax, "30k").invalid(),
            [Field::FreqMax]
        );
        assert_eq!(
            with(options(), Field::LevelMax, "inf").invalid(),
            [Field::LevelMax]
        );
        for (bad, good) in [
            ("319", "320"),
            ("8193", "8192"),
            ("0", "1080"),
            ("1.5", " 640 "),
        ] {
            assert_eq!(
                with(options(), Field::Width, bad).invalid(),
                [Field::Width],
                "{bad}"
            );
            assert_eq!(
                with(options(), Field::Height, bad).invalid(),
                [Field::Height]
            );
            assert_eq!(with(options(), Field::Width, good).invalid(), [], "{good}");
        }
        // Percentages are positive.
        let mut pct = options();
        pct.unit = Unit::Percent;
        let pct = with(with(pct, Field::LevelMin, "0"), Field::LevelMax, "100");
        assert_eq!(pct.invalid(), [Field::LevelMin]);
        let pct = with(pct, Field::LevelMin, "0.01");
        let checked = pct.checked().unwrap();
        assert!((checked.level.low + 2.0).abs() < 1e-12);
        assert!((checked.level.high - 2.0).abs() < 1e-12);
    }

    // The panel opens on the ranges the zoomed view shows, and the export
    // draws exactly those; untouched, an unzoomed level axis stays pinned.
    // Bugs caught: 0, False positives: 0
    #[test]
    fn the_options_default_to_the_zoomed_view() {
        let mut graph = graph();
        let fit = Options::of(&graph, View::Response, Units::Percent, DEFAULT_SIZE, true);
        assert_eq!(
            (fit.freq_min.as_str(), fit.freq_max.as_str()),
            ("20", "20k")
        );
        assert!(fit.checked().unwrap().pins);
        assert!(!with(fit, Field::LevelMax, "30").checked().unwrap().pins);
        let plot = PlotBox::inside(1920.0, 1080.0, INSETS);
        for (axis, at) in [(Axis::Frequency, 900.0), (Axis::Level, 500.0)] {
            graph.wheel(Wheel {
                axis,
                plot,
                at,
                lines: 3.0,
            });
        }
        let (shown, top) = graph.axis(plot);
        let o = Options::of(&graph, View::Response, Units::Percent, DEFAULT_SIZE, true);
        assert_eq!(o.unit, Unit::Db);
        let checked = o.checked().unwrap();
        assert!(!checked.pins);
        // Within the fields' rounding: 4 digits of Hz, tenths of a dB.
        assert!(
            (checked.freq.low - shown.f_low.log10()).abs() < 1e-3,
            "{o:?}"
        );
        assert!(
            (checked.freq.high - shown.f_high.log10()).abs() < 1e-3,
            "{o:?}"
        );
        assert!(checked.freq.low > 20f64.log10() + 0.01, "{o:?}");
        assert!((checked.level.high - top).abs() <= 0.05, "{o:?}");
        assert!(
            (checked.level.low - (top - shown.span)).abs() <= 0.05,
            "{o:?}"
        );
        let export = graph.export_copy(checked.freq, checked.level, Export::default());
        // At any size.
        let (drawn, drawn_top) = export.axis(PlotBox::inside(480.0, 320.0, INSETS));
        assert!((drawn.f_low.log10() - checked.freq.low).abs() < 1e-9);
        assert!((drawn_top - checked.level.high).abs() < 1e-9);
        assert!((drawn.span - checked.level.span()).abs() < 1e-9);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_small_render_has_the_size_asked_for() {
        let graph = copy(&graph(), true);
        let bytes = png(&graph, &Prefs::default(), (480, 320), true).unwrap();
        let ((w, h), rgba) = decode(&bytes);
        assert_eq!((w, h), (480, 320));
        assert_eq!(rgba.len(), 480 * 320 * 4);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn the_legend_toggle_changes_the_image() {
        let graph = graph();
        let size = (480, 320);
        let with_legend = render(&copy(&graph, true), &Prefs::default(), size, true);
        let without = render(&copy(&graph, false), &Prefs::default(), size, true);
        assert_eq!(with_legend.len(), without.len());
        let differ = with_legend
            .chunks(4)
            .zip(without.chunks(4))
            .filter(|(a, b)| a != b)
            .count();
        // A box with two rows of text, not a stray pixel.
        assert!(differ > 500, "{differ} pixels differ");
    }

    // Bugs caught: 0, False positives: 1
    #[test]
    fn a_light_mode_export_has_the_light_background() {
        let graph = graph();
        let light = Prefs {
            theme: ThemeMode::Light,
            ..Prefs::default()
        };
        let pixel = |prefs: &Prefs| {
            let export = copy(&graph, true);
            let ((w, _), rgba) = decode(&png(&export, prefs, (480, 320), true).unwrap());
            // Inside the frame's corner, clear of the labels.
            let at = ((6 * w + 6) * 4) as usize;
            rgba[at..at + 4].to_vec()
        };
        let expect = |mode| {
            let [r, g, b, _] = Palette::new(mode, theme::default_accent(mode), None)
                .bg
                .into_rgba8();
            vec![r, g, b, 255]
        };
        assert_eq!(pixel(&light), expect(ThemeMode::Light));
        assert_eq!(pixel(&Prefs::default()), expect(ThemeMode::Dark));
    }

    // A long name in a narrow image is shortened to fit the legend: nothing
    // of it reaches past the plot's right edge.
    // Bugs caught: 1, False positives: 0
    #[test]
    fn a_long_legend_name_stays_inside_a_narrow_image() {
        let graph = graph();
        let size = (320, 320);
        let rows = vec![LegendEntry {
            name: "SENNHEISER HD 600 LEFT CUP, SEAL ADJUSTED, TAKE 12".into(),
            ink: Ink::Trace(0),
            dashed: false,
            thick: false,
        }];
        let checked = Options::of(&graph, View::Response, Units::Percent, size, true)
            .checked()
            .unwrap();
        let draw = |legend: Option<Vec<LegendEntry>>| {
            let export = Export {
                pins: checked.pins,
                legend,
            };
            let copy = graph.export_copy(checked.freq, checked.level, export);
            render(&copy, &Prefs::default(), size, true)
        };
        let (with_legend, without) = (draw(Some(rows)), draw(None));
        let plot = PlotBox::inside(size.0 as f32, size.1 as f32, INSETS);
        let from = plot.right().ceil() as usize + 2;
        let w = size.0 as usize;
        let differ = (0..size.1 as usize)
            .flat_map(|y| (from..w).map(move |x| (y * w + x) * 4))
            .filter(|&at| with_legend[at..at + 4] != without[at..at + 4])
            .count();
        assert_eq!(differ, 0);
        assert_ne!(with_legend, without);
    }

    // A level typed as the same number, "10.0" for "10", keeps the fit
    // pinned; another number does not.
    // Bugs caught: 1, False positives: 0
    #[test]
    fn an_equal_level_keeps_the_fit_pinned() {
        let fit = Options::of(&graph(), View::Response, Units::Percent, DEFAULT_SIZE, true);
        assert!(fit.checked().unwrap().pins);
        for (field, text) in [
            (Field::LevelMin, &fit.level_min),
            (Field::LevelMax, &fit.level_max),
        ] {
            let same = if text.contains('.') {
                format!(" {text}0")
            } else {
                format!("{text}.0")
            };
            let o = with(fit.clone(), field, &same);
            assert!(o.checked().unwrap().pins, "{same}");
        }
        let other: f64 = fit.level_max.parse().unwrap();
        let o = with(fit, Field::LevelMax, &(other + 1.0).to_string());
        assert!(!o.checked().unwrap().pins);
    }

    // Two frequencies that differ in Hz but not in decades, as drawn, are no
    // range.
    // Bugs caught: 1, False positives: 0
    #[test]
    fn frequencies_equal_in_decades_are_no_range() {
        let o = with(options(), Field::FreqMin, "1000");
        let o = with(o, Field::FreqMax, "1000.0000000000001");
        assert_eq!(o.invalid(), [Field::FreqMin, Field::FreqMax]);
        assert!(o.checked().is_none());
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn the_file_is_named_after_the_set_and_the_view() {
        assert_eq!(file_name("HD600", View::Response), "HD600 - Response.png");
        assert_eq!(file_name("a/b", View::Phase), "a_b - Phase.png");
        assert_eq!(file_name("  ", View::Distortion), "Distortion.png");
        assert_eq!(file_name("", View::Response), "Response.png");
    }
}
