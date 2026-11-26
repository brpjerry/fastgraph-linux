//! RTA mode: the fallback to the sweep. Pink noise plays from here (or from
//! another device) and the session averages what the input hears; the live
//! trace shows the running average and Capture keeps it as a measurement.
//!
//! The noise takes a while to synthesise, so it is rendered once in a task
//! and cached, as the sweep is. The live trace is drawn the way a captured
//! item would be (compensated, smoothed, aligned) but its stored form is the
//! raw response.
use crate::app::{Activity, App, Message, PLAY_RATE, Rendered, SWEEP_PEAK, side_label};
use crate::devices::{NO_CHANNELS, caption};
use crate::theme;
use fastgraph_core::audio::AudioCommand;
use fastgraph_core::response::Response;
use fastgraph_core::rta::{noise_file_name, noise_stimulus};
use fastgraph_core::session::{Mode, SessionCommand};
use fastgraph_core::signal::write_wav;
use fastgraph_core::store::{Kind, Measurement, Meta, Side, Store};
use iced::widget::{button, column, row, text};
use iced::{Alignment, Element, Length, Task};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// What RTA mode keeps.
#[derive(Default)]
pub struct RtaState {
    /// The noise rendered for playback.
    pub(crate) noise: Option<Arc<Vec<f32>>>,
    /// The noise was sent to the output for the running start.
    pub(crate) started: bool,
    /// The session averages: at once for noise played elsewhere, once it
    /// streams for noise played here.
    pub(crate) averaging: bool,
    /// The input and the sink (`None` for noise played elsewhere) the
    /// average was taken with.
    pub(crate) input: Option<String>,
    pub(crate) output: Option<String>,
    /// The side the noise played here was on when it stopped: a kept
    /// average is that ear's.
    pub(crate) stopped_side: Option<Side>,
    /// The session's latest average (raw) and how many frames it has.
    pub(crate) live: Option<(Response, usize)>,
    /// The input clipped since the average began or was reset.
    pub(crate) clipped: bool,
}

/// A command sent to the engine or session, as tests see it.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub enum Sent {
    StopPlay,
    Play {
        sink: String,
        channels: Vec<usize>,
        gain: f32,
        looped: bool,
    },
    OtherAudio,
    Session(SessionCommand),
}

#[cfg(test)]
impl Sent {
    pub fn of(command: &AudioCommand) -> Sent {
        match command {
            AudioCommand::StopPlay => Sent::StopPlay,
            AudioCommand::Play {
                sink,
                channels,
                gain,
                looped,
                ..
            } => Sent::Play {
                sink: sink.name.clone(),
                channels: channels.clone(),
                gain: *gain,
                looped: *looped,
            },
            _ => Sent::OtherAudio,
        }
    }
}

/// Render the noise on the executor, not the UI thread.
fn render_noise() -> Task<Message> {
    Task::perform(
        async { Rendered(Arc::new(noise_stimulus(PLAY_RATE))) },
        Message::NoiseRendered,
    )
}

impl App {
    /// Why Measure, Listen or the noise can't start for lack of an input.
    pub(crate) fn input_ready(&self) -> Result<(), String> {
        if self.selected_input().is_none() {
            return Err(match self.prefs.input {
                Some(_) => "Input not connected".into(),
                None => "Choose an input first".into(),
            });
        }
        if self.capture_rate.is_none() {
            return Err("Input not running".into());
        }
        Ok(())
    }

    /// Why the RTA can't start listening, if it can't.
    pub(crate) fn ready_to_rta(&self) -> Result<(), String> {
        if self.state != Activity::Idle {
            return Err("Stop the current measurement first".into());
        }
        if self.sweep_mode {
            return Err("Switch the mode to RTA first".into());
        }
        self.input_ready()
    }

    /// Why the noise can't start, if it can't: it also needs an output.
    pub(crate) fn ready_to_noise(&self) -> Result<(), String> {
        self.ready_to_rta()?;
        let sink = self.output().ok_or("Choose an output first")?;
        if self.output_channels(sink).is_empty() {
            return Err(NO_CHANNELS.into());
        }
        Ok(())
    }

    /// Switch between Sweep and RTA; the noise or listening ends with it.
    pub(crate) fn mode_picked(&mut self, sweep: bool) {
        match self.state {
            Activity::Rta { .. } => self.stop(),
            Activity::Idle => {}
            _ => return,
        }
        if self.sweep_mode != sweep {
            self.sweep_mode = sweep;
            self.rta.live = None;
            self.redraw_live();
        }
    }

    /// Fresh averaging in RTA mode; noise played here is averaged once it
    /// streams ([`App::rta_play_started`]).
    fn rta_begin(&mut self, playing: bool) {
        if !playing {
            self.command(SessionCommand::SetMode(Mode::Rta));
        }
        self.state = Activity::Rta { playing };
        self.rta.started = false;
        self.rta.averaging = !playing;
        self.rta.input = self.prefs.input.clone();
        self.rta.output = None;
        self.rta.stopped_side = None;
        self.rta.live = None;
        self.rta.clipped = false;
        self.redraw_live();
    }

    pub(crate) fn rta_noise(&mut self, on: bool) -> Task<Message> {
        if !on {
            if self.state == (Activity::Rta { playing: true }) {
                self.stop();
                self.info("Noise stopped".into());
            }
            return Task::none();
        }
        if let Err(why) = self.ready_to_noise() {
            self.error(why);
            return Task::none();
        }
        self.rta_begin(true);
        match self.rta.noise.clone() {
            Some(noise) => {
                self.play_noise(noise);
                Task::none()
            }
            None => {
                self.info("Preparing the noise…".into());
                render_noise()
            }
        }
    }

    pub(crate) fn rta_listen(&mut self, on: bool) {
        if !on {
            if self.state == (Activity::Rta { playing: false }) {
                self.stop();
                self.info("Stopped listening".into());
            }
            return;
        }
        match self.ready_to_rta() {
            Ok(()) => {
                self.rta_begin(false);
                self.info("Listening to noise…".into());
            }
            Err(why) => self.error(why),
        }
    }

    pub(crate) fn noise_rendered(&mut self, noise: Arc<Vec<f32>>) {
        self.rta.noise = Some(Arc::clone(&noise));
        if self.state == (Activity::Rta { playing: true }) && !self.rta.started {
            self.play_noise(noise);
        }
    }

    /// Loop the noise into the output at the chosen level.
    fn play_noise(&mut self, samples: Arc<Vec<f32>>) {
        let Some(sink) = self.output().cloned() else {
            self.stop();
            return self.error("Output not connected".into());
        };
        let channels = self.output_channels(&sink);
        if channels.is_empty() {
            self.stop();
            return self.error(NO_CHANNELS.into());
        }
        let gain = (10f64.powf(self.prefs.level_db / 20.0) / SWEEP_PEAK) as f32;
        self.rta.output = Some(sink.name.clone());
        self.send(AudioCommand::Play {
            sink,
            channels,
            samples,
            rate: PLAY_RATE,
            gain,
            looped: true,
        });
        self.rta.started = true;
        self.info("Playing noise…".into());
    }

    /// The noise played here started streaming: average from now on. A
    /// stale event (no noise waiting to start) is ignored.
    pub(crate) fn rta_play_started(&mut self) {
        if self.state == (Activity::Rta { playing: true })
            && self.rta.started
            && !self.rta.averaging
        {
            self.rta.averaging = true;
            self.command(SessionCommand::SetMode(Mode::Rta));
            self.command(SessionCommand::Reset);
            self.rta_cleared();
        }
    }

    /// The input changed: what was averaged came from another one.
    pub(crate) fn rta_input_changed(&mut self) {
        if matches!(self.state, Activity::Rta { .. }) {
            self.rta.input = self.prefs.input.clone();
            self.rta_reset();
        }
    }

    pub(crate) fn rta_reset(&mut self) {
        self.command(SessionCommand::ResetRta);
        self.rta_cleared();
    }

    /// The session's new average.
    pub(crate) fn rta_data(&mut self, response: Response, frames: usize) {
        // A late event after Stop or a restart is stale.
        if matches!(self.state, Activity::Rta { .. }) {
            self.rta.live = Some((response, frames));
            self.redraw_live();
        }
    }

    /// The input clipped: an average running now carries it.
    pub(crate) fn rta_clipped(&mut self) {
        if matches!(self.state, Activity::Rta { .. }) {
            self.rta.clipped = true;
        }
    }

    /// The level or the output changed: the noise restarts with them
    /// (`Play` replaces the running playback). A different output or
    /// channels changed the acoustic path, so the average starts over.
    pub(crate) fn rta_output_changed(&mut self, path_changed: bool) {
        if self.state != (Activity::Rta { playing: true }) || !self.rta.started {
            return;
        }
        if path_changed {
            // The average starts over once the noise streams on the new path.
            self.rta.averaging = false;
            self.rta_reset();
        }
        if let Some(noise) = self.rta.noise.clone() {
            self.play_noise(noise);
        }
    }

    pub(crate) fn rta_cleared(&mut self) {
        self.rta.clipped = false;
        self.rta.live = None;
        self.redraw_live();
    }

    /// The live trace as the graph draws it.
    pub(crate) fn live_display(&self) -> Option<Response> {
        let (response, _) = self.rta.live.as_ref()?;
        Some(self.shaped(self.compensated(response, true)))
    }

    pub(crate) fn redraw_live(&mut self) {
        // The live trace shows only on the Response view, which redraws it when chosen.
        if self.graph_view != crate::graph::View::Response {
            return;
        }
        let live = self.live_display();
        self.graph.set_live(live);
    }

    /// Keep the live average as a measurement; the live trace stays.
    pub(crate) fn rta_capture(&mut self) -> Task<Message> {
        let Some((response, frames)) = self.rta.live.clone() else {
            self.warn("No RTA data yet".into());
            return Task::none();
        };
        let clipped = self.rta.clipped;
        let side = self.rta.stopped_side.unwrap_or(self.side);
        let prefix = format!("{} RTA ", side_label(side));
        let name = format!("{prefix}{}", self.next_number(Kind::Rta, &prefix));
        let m = Measurement {
            meta: Meta {
                id: Store::new_id(),
                name: name.clone(),
                kind: Kind::Rta,
                side: Some(side),
                seating: None,
                input: self.rta.input.clone(),
                output: self.rta.output.clone(),
                drift_ppm: None,
                clipped,
                created: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs()),
                source: None,
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
        self.add_item(m, None);
        self.redraw();
        let text = format!("Captured {name} · {frames} averages");
        if clipped {
            self.warn(format!("{text} · input clipped"));
        } else {
            self.info(text);
        }
        Task::none()
    }

    /// Save the noise as a stereo WAV at the export rate.
    pub(crate) fn export_noise(&self) -> Task<Message> {
        let rate = self.prefs.checked_export_rate();
        let name = noise_file_name(rate);
        Task::future(async move {
            let file = rfd::AsyncFileDialog::new()
                .set_title("Export noise WAV")
                .set_file_name(name)
                .add_filter("WAV audio", &["wav"])
                .save_file()
                .await?;
            let path = file.path().to_path_buf();
            let noise = noise_stimulus(rate);
            Some(
                write_wav(&path, &noise, rate, 2)
                    .map(|()| path)
                    .map_err(|e| format!("{e:#}")),
            )
        })
        .and_then(|saved| Task::done(Message::StimulusSaved(saved)))
    }
}

/// Start/Stop noise and Listen, then Capture and Reset with the average
/// count.
pub fn actions(app: &App) -> Element<'_, Message> {
    let online = !app.offline;
    let playing = app.state == (Activity::Rta { playing: true });
    let listening = app.state == (Activity::Rta { playing: false });
    let noise = button(
        text(if playing { "Stop noise" } else { "Start noise" })
            .align_x(Alignment::Center)
            .width(Length::Fill),
    )
    .on_press_maybe(if playing {
        Some(Message::RtaNoise(false))
    } else {
        (online && app.ready_to_noise().is_ok()).then_some(Message::RtaNoise(true))
    })
    .padding([6, 10])
    .width(Length::Fill)
    .style(theme::primary);
    let listen = button(
        text(if listening {
            "Stop listening"
        } else {
            "Listen"
        })
        .align_x(Alignment::Center)
        .width(Length::Fill),
    )
    .on_press_maybe(if listening {
        Some(Message::RtaListen(false))
    } else {
        (online && app.ready_to_rta().is_ok()).then_some(Message::RtaListen(true))
    })
    .padding([6, 10])
    .width(Length::Fill)
    .style(theme::segment(listening));
    let small = |label: &'static str, message: Option<Message>| {
        button(text(label).align_x(Alignment::Center).width(Length::Fill))
            .on_press_maybe(message)
            .padding([5, 10])
            .width(Length::Fill)
            .style(theme::button)
    };
    let frames = app.rta.live.as_ref().map_or(0, |(_, frames)| *frames);
    column![
        row![noise, listen].spacing(8),
        row![
            small(
                "Capture",
                app.rta.live.is_some().then_some(Message::RtaCapture)
            ),
            small("Reset", Some(Message::RtaReset)),
        ]
        .spacing(8),
        caption(format!("{frames} averages")),
    ]
    .spacing(8)
    .into()
}
