//! The device pane on the left: input and its channel, the side measured,
//! the input meter, output and its channels, playback level, mode, sweep
//! length, Measure / Listen, the sweep export and the status line.
use crate::app::{Activity, App, Message, StatusLevel};
use crate::rta_mode;
use crate::theme::{self, Palette};
use fastgraph_core::audio::{AudioCommand, Input, Node, channel_display};
use fastgraph_core::signal::SweepLength;
use fastgraph_core::store::Side;
use iced::widget::{button, column, pick_list, progress_bar, row, slider, space, text, text_input};
use iced::{Alignment, Color, Element, Length};
use std::fmt;

/// The pane's width, padding included.
pub const WIDTH: f32 = 300.0;
/// The meter's range in dBFS.
const METER_FLOOR: f32 = -60.0;

/// A drop-down entry: what the app keeps, and what the person reads.
#[derive(Debug, Clone, PartialEq)]
struct Choice {
    key: String,
    label: String,
}

impl fmt::Display for Choice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label)
    }
}

/// A sweep length as the Length drop-down shows it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct LengthChoice(SweepLength);

impl fmt::Display for LengthChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} · {} s", self.0.label(), self.0.seconds())
    }
}

const LENGTHS: [LengthChoice; 3] = [
    LengthChoice(SweepLength::Short),
    LengthChoice(SweepLength::Normal),
    LengthChoice(SweepLength::Long),
];

pub(crate) fn heading<'a>(
    label: impl iced::widget::text::IntoFragment<'a>,
) -> Element<'a, Message> {
    text(label)
        .size(theme::TEXT_SMALL)
        .style(theme::text(|p| p.fg_dim))
        .into()
}

pub(crate) fn caption<'a>(content: String) -> iced::widget::Text<'a> {
    text(content)
        .size(theme::TEXT_SMALL)
        .font(theme::mono())
        .style(theme::text(|p| p.fg_dim))
}

pub(crate) fn segment(
    label: &str,
    selected: bool,
    on_press: Option<Message>,
) -> Element<'_, Message> {
    button(text(label).align_x(Alignment::Center).width(Length::Fill))
        .on_press_maybe(on_press)
        .padding([4, 10])
        .width(Length::Fill)
        .style(theme::segment(selected))
        .into()
}

pub fn view(app: &App) -> Element<'_, Message> {
    let idle = matches!(app.state, Activity::Idle | Activity::Rta { .. });
    column![
        heading("Input"),
        input(app),
        row![
            text("Channel"),
            input_channel(app),
            space::horizontal(),
            text("Side"),
            row![
                segment("L", app.side == Side::L, Some(Message::SidePicked(Side::L))),
                segment("R", app.side == Side::R, Some(Message::SidePicked(Side::R))),
            ]
            .spacing(4)
            .width(Length::Fixed(72.0)),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
        meter(app),
        space().height(8),
        heading("Output"),
        output(app),
        output_caption(app),
        row![
            text("Level").width(Length::Fixed(44.0)),
            slider(-40.0..=0.0, app.prefs.level_db, Message::Level)
                .step(1.0)
                .on_release(Message::LevelReleased)
                .style(theme::slider),
            caption(format!("{:+.0} dB", app.prefs.level_db)).width(Length::Fixed(48.0)),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
        space().height(8),
        heading("Measurement"),
        row![
            text("Mode").width(Length::Fixed(44.0)),
            segment(
                "Sweep",
                app.sweep_mode,
                idle.then_some(Message::ModeSweep(true))
            ),
            segment(
                "RTA",
                !app.sweep_mode,
                idle.then_some(Message::ModeSweep(false))
            ),
        ]
        .spacing(4)
        .align_y(Alignment::Center),
        mode_controls(app),
        button(
            text(if app.sweep_mode {
                "Export sweep WAV…"
            } else {
                "Export noise WAV…"
            })
            .align_x(Alignment::Center)
            .width(Length::Fill)
        )
        .on_press(Message::ExportStimulus)
        .padding([5, 10])
        .width(Length::Fill)
        .style(theme::button),
        space().height(8),
        heading(if app.dirty {
            "Set name · unsaved"
        } else {
            "Set name"
        }),
        save_row(app),
        space().height(Length::Fill),
        status(app),
    ]
    .spacing(8)
    .into()
}

/// The Set name box and the Save button.
fn save_row(app: &App) -> Element<'_, Message> {
    row![
        text_input("Name this set", &app.set_name)
            .on_input(Message::SetNameInput)
            .on_submit(Message::SaveSet)
            .size(theme::TEXT)
            .padding([4, 6])
            .width(Length::Fill)
            .style(theme::input),
        button(text("Save").align_x(Alignment::Center))
            .on_press_maybe((!app.busy).then_some(Message::SaveSet))
            .padding([5, 10])
            .style(theme::button),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

fn input(app: &App) -> Element<'_, Message> {
    let choices: Vec<Choice> = app
        .inputs()
        .iter()
        .map(|i| Choice {
            key: i.key(),
            label: i.label(),
        })
        .collect();
    let selected = app
        .prefs
        .input
        .as_ref()
        .and_then(|key| choices.iter().find(|c| &c.key == key).cloned());
    let placeholder = match &app.prefs.input {
        Some(key) if selected.is_none() => format!("{key} (not connected)"),
        _ => "Choose an input".into(),
    };
    pick_list(choices, selected, |c: Choice| Message::InputPicked(c.key))
        .placeholder(placeholder)
        .width(Length::Fill)
        .style(theme::picker)
        .menu_style(theme::menu)
        .into()
}

fn input_channel(app: &App) -> Element<'_, Message> {
    let channels: Vec<Choice> = app
        .selected_input()
        .map(|(input, _)| {
            input
                .node()
                .channels
                .iter()
                .map(|c| channel_choice(c))
                .collect()
        })
        .unwrap_or_default();
    let selected = app
        .selected_input()
        .and_then(|(input, i)| input.node().channels.get(i).map(|c| channel_choice(c)));
    pick_list(channels, selected, |c: Choice| {
        Message::InputChannelPicked(c.key)
    })
    .placeholder("—")
    .width(Length::Fixed(118.0))
    .style(theme::picker)
    .menu_style(theme::menu)
    .into()
}

/// A channel position as a pick-list entry: stored by position, shown by `channel_display`.
fn channel_choice(position: &str) -> Choice {
    Choice {
        key: position.to_owned(),
        label: channel_display(position),
    }
}

/// The input peak, −60..0 dBFS, red while a clip is held.
fn meter(app: &App) -> Element<'_, Message> {
    let (peak, clipped) = app.level;
    let live = app.capture_rate.is_some();
    let shown = if live {
        peak.clamp(METER_FLOOR, 0.0)
    } else {
        METER_FLOOR
    };
    let readout = if !live {
        "—".to_string()
    } else if peak <= METER_FLOOR {
        format!("<{METER_FLOOR:.0} dBFS")
    } else {
        format!("{peak:.0} dBFS")
    };
    let rate = app
        .capture_rate
        .map(|r| format!("{} kHz", f64::from(r) / 1000.0))
        .unwrap_or_default();
    column![
        progress_bar(METER_FLOOR..=0.0, shown)
            .girth(8)
            .length(Length::Fill)
            .style(theme::meter(clipped)),
        row![
            caption(readout).style(move |theme: &iced::Theme| {
                let p = Palette::of(theme);
                text::Style {
                    color: Some(if clipped { p.err } else { p.fg_dim }),
                }
            }),
            space::horizontal(),
            caption(rate),
        ],
    ]
    .spacing(3)
    .into()
}

fn output(app: &App) -> Element<'_, Message> {
    let choices: Vec<Choice> = app
        .sinks()
        .map(|n| Choice {
            key: n.name.clone(),
            label: n.label(),
        })
        .collect();
    let selected = app
        .prefs
        .output
        .as_ref()
        .and_then(|key| choices.iter().find(|c| &c.key == key).cloned());
    let placeholder = match &app.prefs.output {
        Some(key) if selected.is_none() => format!("{key} (not connected)"),
        _ => "Choose an output".into(),
    };
    pick_list(choices, selected, |c: Choice| Message::OutputPicked(c.key))
        .placeholder(placeholder)
        .width(Length::Fill)
        .style(theme::picker)
        .menu_style(theme::menu)
        .into()
}

/// Why an output can't play: it reported no channels.
pub(crate) const NO_CHANNELS: &str = "No channels reported by this output";

/// Which channel the Side plays on.
fn output_caption(app: &App) -> Element<'_, Message> {
    let Some(sink) = app.output() else {
        return space().height(0).into();
    };
    let side = match app.side {
        Side::L => "Left",
        Side::R => "Right",
    };
    match side_channels(&sink.channels, app.side).first() {
        Some(channel) => caption(format!("Plays on {side} ({})", channel_display(channel))).into(),
        None => caption(NO_CHANNELS.into()).into(),
    }
}

/// The output channels a side plays on: its own front channel, else the
/// mono one, else the first (left) or second (right) channel.
pub(crate) fn side_channels(positions: &[String], side: Side) -> Vec<String> {
    let (own, fallback) = match side {
        Side::L => ("FL", 0),
        Side::R => ("FR", 1),
    };
    let find = |wanted: &[&str]| positions.iter().find(|p| wanted.contains(&p.as_str()));
    find(&[own])
        .or_else(|| find(&["MONO", "MO"]))
        .or_else(|| positions.get(fallback).or(positions.first()))
        .cloned()
        .into_iter()
        .collect()
}

/// The Length drop-down and Measure / Listen, or in RTA mode its controls.
fn mode_controls(app: &App) -> Element<'_, Message> {
    if !app.sweep_mode {
        return rta_mode::actions(app);
    }
    column![
        row![
            text("Length").width(Length::Fixed(44.0)),
            pick_list(
                LENGTHS,
                Some(LengthChoice(app.prefs.length)),
                |l: LengthChoice| { Message::LengthPicked(l.0) }
            )
            .width(Length::Fill)
            .style(theme::picker)
            .menu_style(theme::menu),
        ]
        .spacing(4)
        .align_y(Alignment::Center),
        actions(app),
    ]
    .spacing(8)
    .into()
}

/// Measure and Listen, each usable only when nothing else runs and the
/// devices it needs are chosen.
fn actions(app: &App) -> Element<'_, Message> {
    let online = !app.offline;
    let measuring = matches!(app.state, Activity::Measuring { .. });
    let listening = app.state == Activity::Listening;
    let measure = button(
        text(if measuring { "Stop" } else { "Measure" })
            .align_x(Alignment::Center)
            .width(Length::Fill),
    )
    .on_press_maybe(if measuring {
        Some(Message::Stop)
    } else {
        (online && app.ready_to_measure().is_ok()).then_some(Message::Measure)
    })
    .padding([6, 10])
    .width(Length::Fill)
    .style(theme::primary);
    let listen_message = if listening {
        Some(Message::Listen(false))
    } else {
        (online && app.ready_to_start().is_ok()).then_some(Message::Listen(true))
    };
    let listen = button(
        text(if listening {
            "Stop listening"
        } else {
            "Listen"
        })
        .align_x(Alignment::Center)
        .width(Length::Fill),
    )
    .on_press_maybe(listen_message)
    .padding([6, 10])
    .width(Length::Fill)
    .style(theme::segment(listening));
    row![measure, listen].spacing(8).into()
}

/// The status line, in its level's colour; offline, the Reconnect button.
pub(crate) fn status(app: &App) -> Element<'_, Message> {
    let level = app.status.level;
    let content = if app.status.text.is_empty() {
        hint(app)
    } else {
        app.status.text.clone()
    };
    let line = text(content).style(move |theme: &iced::Theme| {
        let p = Palette::of(theme);
        let color: Color = match level {
            StatusLevel::Info => p.fg_dim,
            StatusLevel::Warn => p.warn,
            StatusLevel::Error => p.err,
        };
        text::Style { color: Some(color) }
    });
    let mut status = column![line].spacing(8);
    if app.recovery.is_none() {
        let why = app.recovery_error.as_deref().unwrap_or("no data folder");
        status = status.push(
            text(format!(
                "Unsaved work can't be recovered after a crash: {why}"
            ))
            .style(theme::text(|p| p.warn)),
        );
    }
    if app.offline {
        status = status.push(
            button(
                text("Reconnect")
                    .align_x(Alignment::Center)
                    .width(Length::Fill),
            )
            .on_press(Message::Reconnect)
            .padding([5, 10])
            .width(Length::Fill)
            .style(theme::button),
        );
    }
    status.into()
}

/// What to do next, before anything has happened.
fn hint(app: &App) -> String {
    let ready = if app.sweep_mode {
        app.ready_to_measure().map(|_| ())
    } else {
        app.ready_to_rta()
    };
    ready.map_or_else(|why| why, |()| "Ready".into())
}

/// The input stream the app opened.
#[derive(Debug, PartialEq)]
pub(crate) struct Opened {
    pub(crate) key: String,
    pub(crate) node: u32,
    pub(crate) channel: usize,
}

impl App {
    pub(crate) fn sinks(&self) -> impl Iterator<Item = &Node> {
        self.nodes.iter().filter(|n| n.is_sink)
    }

    /// Every source, then the monitor of every sink.
    pub(crate) fn inputs(&self) -> Vec<Input> {
        let sources = self
            .nodes
            .iter()
            .filter(|n| !n.is_sink)
            .cloned()
            .map(Input::Source);
        let monitors = self.sinks().cloned().map(Input::MonitorOf);
        sources.chain(monitors).collect()
    }

    /// The chosen input, if it is connected, and its channel index.
    pub(crate) fn selected_input(&self) -> Option<(Input, usize)> {
        let key = self.prefs.input.as_deref()?;
        let input = self.inputs().into_iter().find(|i| i.key() == key)?;
        let channel = self
            .prefs
            .input_channel
            .as_ref()
            .and_then(|label| input.node().channels.iter().position(|c| c == label))
            .unwrap_or(0);
        Some((input, channel))
    }

    /// The chosen output, if it is connected.
    pub(crate) fn output(&self) -> Option<&Node> {
        let name = self.prefs.output.as_deref()?;
        self.sinks().find(|n| n.name == name)
    }

    /// The indices of `sink`'s chosen channels.
    pub(crate) fn output_channels(&self, sink: &Node) -> Vec<usize> {
        (0..sink.channels.len())
            .filter(|&i| self.prefs.output_channels.contains(&sink.channels[i]))
            .collect()
    }

    /// Set the output channels from the side. A change mid-noise restarts it.
    pub(crate) fn apply_side_channels(&mut self) {
        let Some(sink) = self.output() else {
            return;
        };
        let channels = side_channels(&sink.channels, self.side);
        if channels != self.prefs.output_channels {
            self.prefs.output_channels = channels;
            self.save_prefs();
            self.rta_output_changed(true);
        }
    }

    /// Open the chosen input unless it is open already.
    pub(crate) fn open_input(&mut self) {
        let Some((input, channel)) = self.selected_input() else {
            return;
        };
        let want = Opened {
            key: input.key(),
            node: input.node().id,
            channel,
        };
        if self.opened.as_ref() == Some(&want) {
            return;
        }
        if let Some(engine) = &self.engine {
            engine.send(AudioCommand::OpenInput { input, channel });
            self.opened = Some(want);
            self.capture_rate = None;
        }
    }
}
