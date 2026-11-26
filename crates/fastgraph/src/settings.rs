//! The Settings tab: the sets and exports folders, the "ask" boxes, the
//! appearance, the accent colour and the trace colours. Text boxes keep what
//! was typed in [`Inputs`]; only a valid entry reaches the preferences.
use crate::app::{App, Message};
use crate::theme::{self, Palette, TRACE_COLOURS, ThemeMode};
use crate::{devices, theme::parse_hex, theme::to_hex};
use fastgraph_core::prefs::Prefs;
use fastgraph_core::prefs::format_hz;
use iced::widget::{button, checkbox, column, container, row, scrollable, space, text, text_input};
use iced::{Alignment, Element, Length, Task};
use std::path::PathBuf;

/// The text boxes as typed.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    pub sets_dir: String,
    pub exports_dir: String,
    pub accent: String,
    /// The toolbar's Align frequency box.
    pub align_hz: String,
    pub traces: [String; TRACE_COLOURS],
}

fn path_text(dir: &Option<PathBuf>) -> String {
    dir.as_ref()
        .map_or_else(String::new, |d| d.display().to_string())
}

/// The palette the preferences describe.
fn palette(prefs: &Prefs) -> Palette {
    Palette::from_prefs(prefs)
}

impl Inputs {
    pub fn new(prefs: &Prefs) -> Self {
        let p = palette(prefs);
        Inputs {
            sets_dir: path_text(&prefs.sets_dir),
            exports_dir: path_text(&prefs.exports_dir),
            accent: to_hex(p.accent),
            align_hz: format_hz(prefs.align_hz),
            traces: p.traces.map(to_hex),
        }
    }
}

/// The folder a box means: `Some(None)` for empty (the default), `Some(Some)`
/// for an absolute path (a leading `~/` is the home folder), `None` for
/// anything else.
fn folder(text: &str) -> Option<Option<PathBuf>> {
    let text = text.trim();
    if text.is_empty() {
        return Some(None);
    }
    let path = match text.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var_os("HOME")?).join(rest),
        None => PathBuf::from(text),
    };
    path.is_absolute().then_some(Some(path))
}

/// The portal's folder picker, starting in `start`.
fn browse(
    start: Option<PathBuf>,
    title: &'static str,
    done: fn(Option<PathBuf>) -> Message,
) -> Task<Message> {
    Task::perform(
        async move {
            let mut dialog = rfd::AsyncFileDialog::new().set_title(title);
            if let Some(dir) = start {
                dialog = dialog.set_directory(dir);
            }
            dialog.pick_folder().await.map(|f| f.path().to_path_buf())
        },
        done,
    )
}

impl App {
    pub(crate) fn settings_message(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SetsDirInput(text) => self.settings.sets_dir = text,
            Message::SetsDirSubmit => {
                if let Some(dir) = folder(&self.settings.sets_dir) {
                    self.prefs.sets_dir = dir;
                    self.save_prefs();
                    self.refresh_sets();
                }
            }
            Message::BrowseSetsDir => {
                let start = folder(&self.settings.sets_dir)
                    .flatten()
                    .or_else(|| self.prefs.sets_dir().ok());
                return browse(start, "Sets folder", Message::SetsDirPicked);
            }
            Message::SetsDirPicked(Some(dir)) => {
                self.settings.sets_dir = dir.display().to_string();
                return self.settings_message(Message::SetsDirSubmit);
            }
            Message::AskSave(on) => {
                self.prefs.ask_save = on;
                self.save_prefs();
            }
            Message::ExportsDirInput(text) => self.settings.exports_dir = text,
            Message::ExportsDirSubmit => {
                if let Some(dir) = folder(&self.settings.exports_dir) {
                    self.prefs.exports_dir = dir;
                    self.save_prefs();
                }
            }
            Message::BrowseExportsDir => {
                let start = folder(&self.settings.exports_dir)
                    .flatten()
                    .or_else(|| self.prefs.exports_dir().ok());
                return browse(start, "Exports folder", Message::ExportsDirPicked);
            }
            Message::ExportsDirPicked(Some(dir)) => {
                self.settings.exports_dir = dir.display().to_string();
                return self.settings_message(Message::ExportsDirSubmit);
            }
            Message::AskExport(on) => {
                self.prefs.ask_export = on;
                self.save_prefs();
            }
            Message::ThemePicked(mode) => {
                if self.prefs.theme != mode {
                    self.prefs.theme = mode;
                    self.save_prefs();
                    // The default accent and traces are the mode's own.
                    if self.prefs.accent.is_none() {
                        self.settings.accent = to_hex(palette(&self.prefs).accent);
                    }
                    self.sync_default_traces();
                    self.apply_colors();
                }
            }
            Message::AccentInput(text) => self.settings.accent = text,
            Message::AccentSubmit => {
                if let Some(color) = parse_hex(&self.settings.accent) {
                    self.prefs.accent = Some(to_hex(color));
                    self.save_prefs();
                    self.sync_default_traces();
                    self.apply_colors();
                }
            }
            Message::AccentReset => {
                self.prefs.accent = None;
                self.save_prefs();
                self.settings.accent = to_hex(palette(&self.prefs).accent);
                self.sync_default_traces();
                self.apply_colors();
            }
            Message::TraceColorInput(index, text) => {
                if let Some(slot) = self.settings.traces.get_mut(index) {
                    *slot = text;
                }
            }
            Message::TraceColorSubmit(index) => {
                let color = self.settings.traces.get(index).and_then(|t| parse_hex(t));
                if let Some(color) = color {
                    let mut colors = palette(&self.prefs).traces.map(to_hex);
                    colors[index] = to_hex(color);
                    self.prefs.trace_colors = Some(colors.to_vec());
                    self.save_prefs();
                    self.apply_colors();
                }
            }
            Message::TraceColorsReset => {
                self.prefs.trace_colors = None;
                self.save_prefs();
                self.settings.traces = palette(&self.prefs).traces.map(to_hex);
                self.apply_colors();
            }
            // A cancelled picker changes nothing.
            _ => {}
        }
        Task::none()
    }

    /// Install the mode and colours the preferences name: the graph's cached
    /// strokes have the old ones.
    fn apply_colors(&mut self) {
        self.theme = theme::from_prefs(&self.prefs);
        self.graph.colors_changed();
    }

    /// While the trace colours are the theme's, their boxes follow the accent.
    fn sync_default_traces(&mut self) {
        if self.prefs.trace_colors.is_none() {
            self.settings.traces = palette(&self.prefs).traces.map(to_hex);
        }
    }
}

/// A text box that draws its text in the error colour while `valid` is false.
fn hex_box<'a>(
    value: &'a str,
    on_input: impl Fn(String) -> Message + 'a,
    on_submit: Message,
) -> Element<'a, Message> {
    let valid = parse_hex(value).is_some();
    text_input("#rrggbb", value)
        .on_input(on_input)
        .on_submit(on_submit)
        .size(theme::TEXT)
        .padding([4, 6])
        .width(Length::Fill)
        .style(theme::input_checked(valid))
        .into()
}

fn swatch<'a>(style: impl Fn(&iced::Theme) -> container::Style + 'a) -> Element<'a, Message> {
    container(space()).width(14).height(14).style(style).into()
}

/// What a folder section shows and sends.
struct Folder<'a> {
    label: &'a str,
    value: &'a str,
    ask_label: &'a str,
    ask: bool,
    input: fn(String) -> Message,
    submit: Message,
    browse: Message,
    toggle: fn(bool) -> Message,
}

fn folder_section(f: Folder<'_>) -> Element<'_, Message> {
    column![
        devices::heading(f.label),
        text_input("Default folder", f.value)
            .on_input(f.input)
            .on_submit(f.submit)
            .size(theme::TEXT)
            .padding([4, 6])
            .width(Length::Fill)
            .style(theme::input_checked(folder(f.value).is_some())),
        button(text("Browse…"))
            .on_press(f.browse)
            .padding([4, 10])
            .style(theme::button),
        checkbox(f.ask)
            .label(f.ask_label)
            .on_toggle(f.toggle)
            .size(14)
            .style(theme::check),
    ]
    .spacing(6)
    .into()
}

fn reset(on_press: Message) -> Element<'static, Message> {
    button(text("Reset"))
        .on_press(on_press)
        .padding([4, 10])
        .style(theme::button)
        .into()
}

pub fn view(app: &App) -> Element<'_, Message> {
    let prefs = &app.prefs;
    let inputs = &app.settings;
    let mut grid = column![].spacing(6);
    for pair in 0..TRACE_COLOURS / 2 {
        grid = grid.push(
            row((pair * 2..pair * 2 + 2).map(|i| {
                row![
                    swatch(theme::swatch(i)),
                    hex_box(
                        &inputs.traces[i],
                        move |t| Message::TraceColorInput(i, t),
                        Message::TraceColorSubmit(i)
                    ),
                ]
                .spacing(6)
                .align_y(Alignment::Center)
                .width(Length::Fill)
                .into()
            }))
            .spacing(8),
        );
    }
    let appearance = column![
        devices::heading("Appearance"),
        row(
            [("Dark", ThemeMode::Dark), ("Light", ThemeMode::Light)].map(|(label, mode)| {
                devices::segment(label, prefs.theme == mode, Some(Message::ThemePicked(mode)))
            })
        )
        .spacing(4)
        .width(Length::Fixed(160.0)),
    ]
    .spacing(6);
    let content = column![
        folder_section(Folder {
            label: "Sets folder",
            value: &inputs.sets_dir,
            ask_label: "Ask where to save",
            ask: prefs.ask_save,
            input: Message::SetsDirInput,
            submit: Message::SetsDirSubmit,
            browse: Message::BrowseSetsDir,
            toggle: Message::AskSave,
        }),
        folder_section(Folder {
            label: "Exports folder",
            value: &inputs.exports_dir,
            ask_label: "Ask where to export",
            ask: prefs.ask_export,
            input: Message::ExportsDirInput,
            submit: Message::ExportsDirSubmit,
            browse: Message::BrowseExportsDir,
            toggle: Message::AskExport,
        }),
        appearance,
        column![
            devices::heading("Accent colour"),
            row![
                swatch(theme::accent_swatch),
                hex_box(&inputs.accent, Message::AccentInput, Message::AccentSubmit),
                reset(Message::AccentReset),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        ]
        .spacing(6),
        column![
            row![
                devices::heading("Trace colours"),
                space::horizontal(),
                reset(Message::TraceColorsReset)
            ]
            .align_y(Alignment::Center),
            grid,
        ]
        .spacing(6),
    ]
    .spacing(18)
    .padding([0, 8]);
    scrollable(content)
        .style(theme::scrollbar)
        .height(Length::Fill)
        .into()
}
