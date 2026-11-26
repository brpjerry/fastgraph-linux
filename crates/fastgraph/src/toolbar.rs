//! The bar over the graph (smoothing, compensation, align, average) and what
//! the post-processing messages do: compensation, import, export, average.
//!
//! The graph draws [`App::display`]: an item's [`App::processed`] response,
//! smoothed, then aligned when Align is on. All file access runs in
//! tasks; their results come back as messages.
use crate::app::{App, Item, Message, Status, StatusLevel};
use crate::list::round_offset;
use crate::{graph, theme};
use fastgraph_core::curve_io::{ExportHeader, read_curve, write_rew};
use fastgraph_core::prefs::{ALIGN_WIDTHS, format_hz, parse_hz};
use fastgraph_core::response::{Response, Smoothing, average};
use fastgraph_core::set::safe_file_name;
use fastgraph_core::store::{Kind, Measurement, Meta, Store};
use iced::widget::{button, checkbox, pick_list, row, text, text_input, tooltip};
use iced::{Alignment, Element, Length, Task};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// An align band width as the pick list shows it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct WidthChoice(f64);

impl fmt::Display for WidthChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 <= 0.0 {
            f.write_str("Point")
        } else if (self.0 - 1.0 / 3.0).abs() < 1e-9 {
            f.write_str("1/3 oct")
        } else if (self.0 - 0.5).abs() < 1e-9 {
            f.write_str("1/2 oct")
        } else {
            write!(f, "{} oct", self.0)
        }
    }
}

/// A smoothing as the drop-down shows it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct SmoothingChoice(Smoothing);

impl fmt::Display for SmoothingChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Smoothing::None => f.write_str("None"),
            s => write!(f, "{} oct", s.label()),
        }
    }
}

const SMOOTHINGS: [SmoothingChoice; 6] = [
    SmoothingChoice(Smoothing::None),
    SmoothingChoice(Smoothing::Oct48),
    SmoothingChoice(Smoothing::Oct24),
    SmoothingChoice(Smoothing::Oct12),
    SmoothingChoice(Smoothing::Oct6),
    SmoothingChoice(Smoothing::Oct3),
];

/// The file name of a path, for labels and messages.
fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into(),
    )
}

/// `<set> - <trace>.txt`, or `<trace>.txt` for a session with no set name.
pub(crate) fn export_file_name(set_name: &str, trace: &str) -> String {
    let trace = safe_file_name(trace);
    if set_name.trim().is_empty() {
        format!("{trace}.txt")
    } else {
        format!("{} - {trace}.txt", safe_file_name(set_name))
    }
}

/// The compensation button's label.
pub fn comp_label(path: Option<&PathBuf>, loaded: bool) -> String {
    let name = match path {
        None => "none".into(),
        Some(p) if loaded => file_name(p),
        Some(p) => format!("{} (not loaded)", file_name(p)),
    };
    format!("Comp: {name}")
}

/// The tabs' line's buttons, Export PNG… last, and their padding.
const LINE_ONE: [&str; 3] = ["Import…", "Average shown", PNG_LABEL];
pub const PNG_LABEL: &str = "Export PNG…";
const BUTTON_PAD: f32 = 10.0;
/// Room between the buttons, and between the tabs and them.
const LINE_ONE_SPACING: f32 = 10.0;

/// The width of a button label at the body size.
fn label_width(content: &str) -> f32 {
    use iced::advanced::text::Paragraph as _;
    let text = iced::advanced::Text {
        content,
        bounds: iced::Size::new(f32::INFINITY, f32::INFINITY),
        size: theme::TEXT.into(),
        line_height: text::LineHeight::default(),
        font: theme::ui(),
        align_x: text::Alignment::Left,
        align_y: iced::alignment::Vertical::Center,
        shaping: text::Shaping::default(),
        wrapping: text::Wrapping::None,
    };
    iced::advanced::graphics::text::Paragraph::with_text(text)
        .min_bounds()
        .width
}

/// Whether Export PNG… fits on the tabs' line of `width` beside the tabs
/// at their full `tabs` width; else it ends the controls, so the tabs keep
/// their width.
pub fn png_on_tabs_line(width: f32, tabs: f32) -> bool {
    // The labels and the font are fixed: measured once.
    static BUTTONS: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    let buttons = BUTTONS.get_or_init(|| {
        LINE_ONE
            .iter()
            .map(|l| label_width(l).ceil() + 2.0 * BUTTON_PAD + 1.0)
            .sum()
    });
    tabs + LINE_ONE_SPACING * LINE_ONE.len() as f32 + buttons <= width
}

/// The controls (four fixed groups in a wrapping row) and the import,
/// average and Export PNG… buttons, which sit on the tabs' line; Export
/// PNG… ends the controls instead when it is not `on_tabs_line`.
pub fn view(app: &App, on_tabs_line: bool) -> (Element<'_, Message>, Element<'_, Message>) {
    let shown = app.items.iter().filter(|i| i.shown).count();
    // Compensation, align and offset change the level, not the phase, and
    // the Distortion view has its own curves: their controls rest there.
    let levels = app.graph_view == graph::View::Response;
    let loaded = app.comp.is_some() && levels;
    let comp_name = app
        .comp
        .as_ref()
        .map(|(path, _)| path)
        .or(app.prefs.compensation.as_ref());
    let label = |t: &'static str| {
        text(t)
            .size(theme::TEXT_SMALL)
            .style(theme::text(|p| p.fg_dim))
    };
    let smoothing = row![
        label("Smoothing"),
        pick_list(
            SMOOTHINGS,
            Some(SmoothingChoice(app.prefs.smoothing)),
            |c: SmoothingChoice| Message::SmoothingPicked(c.0)
        )
        .style(theme::picker)
        .menu_style(theme::menu),
    ]
    .spacing(10)
    .align_y(Alignment::Center);
    let full = comp_label(comp_name, app.comp.is_some());
    let comp_button = button(text(shorten(&full, COMP_CHARS)).wrapping(text::Wrapping::None))
        .on_press_maybe(levels.then_some(Message::LoadCompensation))
        .padding([4, 10])
        .width(COMP_WIDTH)
        .style(theme::button);
    let compensation = row![
        tooltip(comp_button, text(full), tooltip::Position::Bottom)
            .padding(4)
            .style(theme::card),
        checkbox(app.prefs.comp_enabled)
            .label("Apply")
            .on_toggle_maybe(loaded.then_some(Message::CompensationEnabled))
            .size(14)
            .style(theme::check),
        checkbox(app.prefs.comp_invert)
            .label("Invert")
            .on_toggle_maybe(loaded.then_some(Message::CompensationInvert))
            .size(14)
            .style(theme::check),
    ]
    .spacing(10)
    .align_y(Alignment::Center);
    let align = row![
        checkbox(app.prefs.align)
            .label("Align")
            .on_toggle_maybe(levels.then_some(Message::Align))
            .size(14)
            .style(theme::check),
        align_box(&app.settings.align_hz, levels),
        label("Hz"),
        width_picker(app.prefs.align_octaves, levels),
    ]
    .spacing(10)
    .align_y(Alignment::Center);
    let units = units_toggle(app.units, app.graph_view == graph::View::Distortion);
    let png = crate::export_png::button_view();
    let (png_controls, png_actions) = if on_tabs_line {
        (None, Some(png))
    } else {
        (Some(png), None)
    };
    let controls = row![smoothing, compensation, align, units]
        .extend(png_controls)
        .spacing(24)
        .align_y(Alignment::Center)
        .width(Length::Fill)
        .wrap()
        .vertical_spacing(8);
    let actions = row![
        button(text(LINE_ONE[0]))
            .on_press(Message::Import)
            .padding([4.0, BUTTON_PAD])
            .style(theme::button),
        button(text(LINE_ONE[1]))
            .on_press_maybe((shown >= 2).then_some(Message::AverageShown))
            .padding([4.0, BUTTON_PAD])
            .style(theme::button),
    ]
    .extend(png_actions)
    .spacing(LINE_ONE_SPACING)
    .align_y(Alignment::Center);
    (controls.into(), actions.into())
}

/// The Distortion view's units: `%` of the fundamental, or absolute deconvolved levels in dB.
/// The segments rest (the choice still shows) outside that view.
fn units_toggle<'a>(current: graph::Units, enabled: bool) -> Element<'a, Message> {
    row![
        text("Units")
            .size(theme::TEXT_SMALL)
            .style(theme::text(|p| p.fg_dim)),
        row(graph::UNITS.map(|(units, label)| {
            button(text(label))
                .on_press_maybe(enabled.then_some(Message::UnitsPicked(units)))
                .padding([4, 10])
                .style(theme::segment(units == current))
                .into()
        }))
        .spacing(2),
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .into()
}

/// The compensation button's width and the label characters that fit it.
const COMP_WIDTH: f32 = 220.0;
const COMP_CHARS: usize = 28;

/// `label` cut to `max` characters with a trailing `…` (before a short
/// extension, so `Curve-Coupl….txt`).
pub(crate) fn shorten(label: &str, max: usize) -> String {
    let count = label.chars().count();
    if count <= max {
        return label.into();
    }
    let ext = label
        .rfind('.')
        .map(|i| &label[i..])
        .filter(|e| e.chars().count() <= 5)
        .unwrap_or("");
    let keep = max.saturating_sub(1 + ext.chars().count());
    let stem: String = label.chars().take(keep).collect();
    format!("{stem}…{ext}")
}

/// The room the Align width picker takes, so the bar keeps its layout when
/// the picker rests.
const WIDTH_PICKER: f32 = 100.0;

/// The Align width drop-down, or a resting button of the same size.
fn width_picker(octaves: f64, enabled: bool) -> Element<'static, Message> {
    if enabled {
        pick_list(
            ALIGN_WIDTHS.map(WidthChoice),
            Some(WidthChoice(octaves)),
            |c: WidthChoice| Message::AlignWidthPicked(c.0),
        )
        .width(WIDTH_PICKER)
        .style(theme::picker)
        .menu_style(theme::menu)
        .into()
    } else {
        button(text(WidthChoice(octaves).to_string()))
            .padding([5, 10])
            .width(WIDTH_PICKER)
            .style(theme::button)
            .into()
    }
}

/// The Align frequency box; its text is in the error colour while it does
/// not parse.
fn align_box(value: &str, enabled: bool) -> Element<'_, Message> {
    let valid = parse_hz(value).is_some();
    text_input("1k", value)
        .on_input_maybe(enabled.then_some(Message::AlignHzInput))
        .on_submit_maybe(enabled.then_some(Message::AlignHzSubmit))
        .size(theme::TEXT)
        .padding([4, 6])
        .width(60)
        .style(theme::input_checked(valid))
        .into()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Read a curve file.
pub(crate) fn read_file(path: &Path) -> Result<Response, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", file_name(path)))?;
    read_curve(&text).map_err(|e| format!("{}: {e:#}", file_name(path)))
}

/// Read every file; one failure fails the lot, naming the file.
pub(crate) fn read_files(paths: &[PathBuf]) -> Result<Vec<(PathBuf, Response)>, String> {
    paths
        .iter()
        .map(|path| read_file(path).map(|curve| (path.clone(), curve)))
        .collect()
}

impl App {
    /// The response of `item` with the microphone compensation applied, for
    /// measurements and the averages built only from them
    /// ([`Meta::takes_compensation`]): imported curves and averages with the
    /// compensation built in are never compensated again.
    pub fn processed(&self, item: &Item) -> Response {
        self.compensated(&item.m.response, item.m.meta.takes_compensation())
    }

    /// `response`, compensated when it `takes` compensation.
    pub(crate) fn compensated(&self, response: &Response, takes: bool) -> Response {
        match self.applied_compensation(takes) {
            Some((_, curve)) => response.compensated(curve, self.prefs.comp_invert),
            None => response.clone(),
        }
    }

    /// The compensation applied to a response that `takes` it, if any.
    fn applied_compensation(&self, takes: bool) -> Option<&(PathBuf, Response)> {
        self.comp
            .as_ref()
            .filter(|_| takes && self.prefs.comp_enabled)
    }

    /// The applied compensation as export headers and captions name it.
    fn compensation_name(&self, path: &Path) -> String {
        let name = file_name(path);
        if self.prefs.comp_invert {
            format!("{name} (inverted)")
        } else {
            name
        }
    }

    /// The response the graph draws for `item`: shaped, then shifted by its
    /// offset (last, so Align does not cancel it).
    pub fn display(&self, item: &Item) -> Response {
        self.shaped(self.processed(item))
            .offset(item.m.meta.offset_db)
    }

    /// A processed response, smoothed, then aligned when Align is on.
    pub(crate) fn shaped(&self, processed: Response) -> Response {
        let smoothed = processed.smoothed(self.prefs.smoothing);
        if self.prefs.align {
            smoothed.aligned_band(self.prefs.align_hz, self.prefs.align_octaves)
        } else {
            smoothed
        }
    }

    /// What `Export` writes for `item`: processed and smoothed but not
    /// aligned, so levels stay comparable. Phase goes along only when the
    /// smoothing leaves it valid.
    pub fn export_text(&self, item: &Item) -> String {
        let mut response = self
            .processed(item)
            .smoothed(self.prefs.smoothing)
            .offset(item.m.meta.offset_db);
        if self.prefs.smoothing != Smoothing::None {
            response.phase = None;
        }
        let meta = &item.m.meta;
        let header = ExportHeader {
            name: meta.name.clone(),
            smoothing: self.prefs.smoothing,
            compensation: self
                .applied_compensation(meta.takes_compensation())
                .map(|(path, _)| self.compensation_name(path))
                .or_else(|| meta.baked_compensation.clone()),
            created: UNIX_EPOCH + std::time::Duration::from_secs(item.m.meta.created),
            offset_db: meta.offset_db,
        };
        write_rew(&response, &header)
    }

    pub(crate) fn smoothing_picked(&mut self, smoothing: Smoothing) {
        self.prefs.smoothing = smoothing;
        self.save_prefs();
        self.redraw();
    }

    pub(crate) fn load_compensation(&self) -> Task<Message> {
        Task::future(async {
            let file = rfd::AsyncFileDialog::new()
                .set_title("Compensation curve")
                .add_filter("Curve", &["txt", "csv", "frd"])
                .pick_file()
                .await?;
            let path = file.path().to_path_buf();
            Some(read_file(&path).map(|curve| (path, curve)))
        })
        .and_then(|loaded| {
            let ok = loaded.is_ok();
            let loaded = Task::done(Message::CompensationLoaded(loaded));
            // Picking a curve turns it on.
            if ok {
                loaded.chain(Task::done(Message::CompensationEnabled(true)))
            } else {
                loaded
            }
        })
    }

    /// Read the compensation curve the preferences name, at start.
    pub(crate) fn reload_compensation(&mut self) -> Task<Message> {
        let Some(path) = self.prefs.compensation.clone() else {
            return Task::none();
        };
        self.comp_boot_reload = true;
        Task::perform(
            async move { read_file(&path).map(|curve| (path, curve)) },
            Message::CompensationLoaded,
        )
    }

    pub(crate) fn compensation_loaded(&mut self, loaded: Result<(PathBuf, Response), String>) {
        // Whichever way the start-up reload ends, later results are the user's.
        let boot = std::mem::take(&mut self.comp_boot_reload);
        match loaded {
            Ok((path, curve)) => {
                self.prefs.compensation = Some(path.clone());
                self.comp = Some((path, curve));
                self.save_prefs();
                self.redraw();
            }
            // The preferences keep the old file.
            Err(e) => {
                let note = (format!("Compensation not loaded: {e}"), StatusLevel::Warn);
                // The start-up reload joins the boot notes instead of
                // replacing a worse one.
                let mut notes = Vec::new();
                if boot && !self.status.text.is_empty() {
                    notes.push((self.status.text.clone(), self.status.level));
                }
                notes.push(note);
                if let Some(status) = Status::joined(notes) {
                    self.status = status;
                }
            }
        }
    }

    pub(crate) fn compensation_enabled(&mut self, on: bool) {
        self.prefs.comp_enabled = on;
        self.save_prefs();
        self.redraw();
    }

    pub(crate) fn compensation_invert(&mut self, on: bool) {
        self.prefs.comp_invert = on;
        self.save_prefs();
        self.redraw();
    }

    pub(crate) fn align(&mut self, on: bool) {
        self.prefs.align = on;
        self.save_prefs();
        self.redraw();
    }

    /// Enter in the frequency box: apply valid text, keep invalid as typed.
    pub(crate) fn align_hz_submit(&mut self) {
        let Some(hz) = parse_hz(&self.settings.align_hz) else {
            return;
        };
        self.prefs.align_hz = hz;
        self.settings.align_hz = format_hz(hz);
        self.save_prefs();
        self.redraw();
    }

    pub(crate) fn align_width_picked(&mut self, octaves: f64) {
        self.prefs.align_octaves = octaves;
        self.save_prefs();
        self.redraw();
    }

    pub(crate) fn import(&self) -> Task<Message> {
        let clears = self.clears;
        Task::future(async {
            let files = rfd::AsyncFileDialog::new()
                .set_title("Import curves")
                .add_filter("Curves", &["txt", "csv", "frd"])
                .pick_files()
                .await?;
            let paths: Vec<PathBuf> = files.iter().map(|f| f.path().to_path_buf()).collect();
            Some(read_files(&paths))
        })
        .and_then(move |curves| Task::done(Message::Imported(clears, curves)))
    }

    /// Add imported curves, all or none.
    pub(crate) fn imported(
        &mut self,
        curves: Result<Vec<(PathBuf, Response)>, String>,
    ) -> Task<Message> {
        let curves = match curves {
            Ok(curves) if curves.is_empty() => return Task::none(),
            Ok(curves) => curves,
            Err(e) => {
                self.error(format!("Import failed: {e}"));
                return Task::none();
            }
        };
        let n = curves.len();
        for (path, response) in curves {
            let name = path
                .file_stem()
                .map_or_else(|| file_name(&path), |s| s.to_string_lossy().into());
            let m = Measurement {
                meta: Meta {
                    id: Store::new_id(),
                    name,
                    kind: Kind::Imported,
                    side: None,
                    seating: None,
                    input: None,
                    output: None,
                    drift_ppm: None,
                    clipped: false,
                    created: now(),
                    // The metadata is TOML text, which a non-UTF-8 path can't be.
                    source: Some(path.to_string_lossy().into_owned().into()),
                    sources: Vec::new(),
                    compensable: false,
                    baked_compensation: None,
                    shown: true,
                    offset_db: 0.0,
                    ir_peak: None,
                    sweep_seconds: None,
                },
                response,
            };
            self.push_item(m, None);
        }
        self.session_changed();
        self.redraw();
        self.info(format!(
            "Imported {n} curve{}",
            if n == 1 { "" } else { "s" }
        ));
        Task::none()
    }

    pub(crate) fn export(&self, id: &str) -> Task<Message> {
        let Some(item) = self.items.iter().find(|i| i.m.meta.id == id) else {
            return Task::none();
        };
        let text = self.export_text(item);
        let name = export_file_name(&self.set_name, &item.m.meta.name);
        let dir = match self.prefs.exports_dir() {
            Ok(dir) => dir,
            Err(e) => {
                return Task::done(Message::Exported(Err(format!("{e:#}"))));
            }
        };
        if !self.prefs.ask_export {
            // The folder may not exist yet: the worker creates it.
            self.worker.submit(Box::new(move || {
                let path = dir.join(name);
                let made =
                    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()));
                let written = made.and_then(|()| {
                    fastgraph_core::fsutil::write_atomic(&path, text.as_bytes())
                        .map_err(|e| format!("{}: {e:#}", file_name(&path)))
                });
                Some(Message::Exported(written.map(|()| path)))
            }));
            return Task::none();
        }
        Task::future(async move {
            let file = rfd::AsyncFileDialog::new()
                .set_title("Export curve")
                .set_directory(dir)
                .set_file_name(name)
                .add_filter("Text", &["txt"])
                .save_file()
                .await?;
            let path = file.path().to_path_buf();
            Some(
                fastgraph_core::fsutil::write_atomic(&path, text.as_bytes())
                    .map(|()| path.clone())
                    .map_err(|e| format!("{}: {e:#}", file_name(&path))),
            )
        })
        .and_then(|saved| Task::done(Message::Exported(saved)))
    }

    /// The mean of the shown items, aligned first when Align is on.
    ///
    /// Built only from measurements (or such averages), it is the mean of
    /// their uncompensated responses and takes the compensation like they
    /// do; alignment shifts each by a constant, so it commutes with the
    /// compensation. From mixed sources, it is the mean of what the graph
    /// shows unsmoothed, with the compensation built in and named.
    pub(crate) fn average_shown(&mut self) -> Task<Message> {
        let shown: Vec<&Item> = self.items.iter().filter(|i| i.shown).collect();
        if shown.len() < 2 {
            return Task::none();
        }
        let compensable = shown.iter().all(|i| i.m.meta.takes_compensation());
        let mut baked: Vec<String> = Vec::new();
        if !compensable {
            let applied = shown
                .iter()
                .filter_map(|i| self.applied_compensation(i.m.meta.takes_compensation()))
                .map(|(path, _)| self.compensation_name(path));
            let inherited = shown
                .iter()
                .filter_map(|i| i.m.meta.baked_compensation.clone());
            for name in applied.chain(inherited) {
                if !baked.contains(&name) {
                    baked.push(name);
                }
            }
        }
        // The sources' offsets live on as their mean: `display()` adds it
        // after aligning, so the average sits where its sources are drawn.
        let offset_db = round_offset(
            shown.iter().map(|i| i.m.meta.offset_db).sum::<f64>() / shown.len() as f64,
        );
        // Unsmoothed, since the display smooths it. Each source is shifted
        // by its offset's difference from the mean, so where only some have
        // data the level follows those.
        let responses: Vec<Response> = shown
            .iter()
            .map(|i| {
                let r = if compensable {
                    i.m.response.clone()
                } else {
                    self.processed(i)
                };
                let r = if self.prefs.align {
                    r.aligned_band(self.prefs.align_hz, self.prefs.align_octaves)
                } else {
                    r
                };
                r.offset(i.m.meta.offset_db - offset_db)
            })
            .collect();
        let refs: Vec<&Response> = responses.iter().collect();
        let Some(response) = average(&refs) else {
            return Task::none();
        };
        // A curve needs two points to be drawn or stored.
        if response
            .db
            .iter()
            .filter(|d| d.is_finite())
            .nth(1)
            .is_none()
        {
            self.warn("Too few measured frequencies to average".into());
            return Task::none();
        }
        let sources: Vec<String> = shown.iter().map(|i| i.m.meta.id.clone()).collect();
        let m = Measurement {
            meta: Meta {
                id: Store::new_id(),
                name: format!("Average of {}", sources.len()),
                kind: Kind::Average,
                side: None,
                seating: None,
                input: None,
                output: None,
                drift_ppm: None,
                clipped: false,
                created: now(),
                source: None,
                sources,
                compensable,
                baked_compensation: (!baked.is_empty()).then(|| baked.join(", ")),
                shown: true,
                offset_db,
                ir_peak: None,
                sweep_seconds: None,
            },
            response,
        };
        self.add_item(m, None);
        self.redraw();
        Task::none()
    }
}
