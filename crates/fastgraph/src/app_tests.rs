//! Tests of [`App::update`] and the main view.
use super::*;
use crate::browser::Pane;
use crate::rta_mode::Sent;
use crate::theme::{Palette, ThemeMode};
use fastgraph_core::deconv::ImpulseResponse;
use fastgraph_core::response::{Response, Smoothing, average, log_grid};
use fastgraph_core::store::Kind;
use iced::futures::StreamExt;
use std::collections::VecDeque;
use std::time::Duration;

/// Feed `message` to `app`, and every message the tasks it returns produce
/// after it (rendering), and the worker's results, as the runtime would.
fn run(app: &mut App, message: Message) {
    let mut queue = VecDeque::from([message]);
    while let Some(message) = queue.pop_front() {
        queue.extend(outputs(app.update(message)));
        queue.extend(worker_results(app));
    }
}

/// The messages `task` produces, run to its end.
fn outputs(task: Task<Message>) -> Vec<Message> {
    let Some(stream) = iced_runtime::task::into_stream(task) else {
        return Vec::new();
    };
    iced::futures::executor::block_on(stream.collect::<Vec<_>>())
        .into_iter()
        .filter_map(|action| match action {
            iced_runtime::Action::Output(message) => Some(message),
            _ => None,
        })
        .collect()
}

/// Wait for the worker and take the messages it sent.
fn worker_results(app: &App) -> Vec<Message> {
    app.worker.flush();
    let mut events = app.events.0.lock().unwrap();
    let receiver = events.as_mut().unwrap();
    std::iter::from_fn(|| receiver.try_recv().ok()).collect()
}

fn node(id: u32, name: &str, description: &str, is_sink: bool) -> Node {
    Node {
        id,
        serial: id.to_string(),
        name: name.into(),
        description: description.into(),
        is_sink,
        raw: false,
        channels: vec!["FL".into(), "FR".into()],
    }
}

fn mic() -> Node {
    node(31, "alsa_input.usb-mic", "USB Mic", false)
}

fn phones() -> Node {
    node(52, "bluez_output.phones", "Headphones", true)
}

/// A test app with a mic and headphones chosen and in the device list, the
/// mic's stream running; `data` is its `$XDG_DATA_HOME/fastgraph`.
fn ready(data: Option<PathBuf>) -> App {
    let prefs = Prefs {
        input: Some(mic().name),
        input_channel: Some("FL".into()),
        output: Some(phones().name),
        output_channels: vec!["FL".into(), "FR".into()],
        ..Prefs::default()
    };
    let mut app = App::new_for_test(prefs, data);
    run(
        &mut app,
        Message::Audio(AudioEvent::Nodes(vec![mic(), phones()])),
    );
    run(&mut app, started());
    app
}

fn started() -> Message {
    Message::Audio(AudioEvent::InputStarted { rate: 48_000 })
}

fn measured(db: f64, clipped: bool) -> Message {
    let freq = log_grid(48_000);
    Message::Session(SessionEvent::Measured {
        response: Response {
            db: vec![db; freq.len()],
            freq,
            phase: None,
        },
        ir: ImpulseResponse {
            rate: 48_000,
            samples: vec![0.0, 1.0, 0.5, 0.0],
            peak: 1,
        },
        sweep_seconds: 6.0,
        drift_ppm: 41.0,
        clipped,
    })
}

/// Deliver a sweep result as Listen gets it, and leave the app idle again
/// if it was.
fn heard(app: &mut App, db: f64, clipped: bool) {
    let idle = app.state == Activity::Idle;
    if idle {
        app.state = Activity::Listening;
    }
    run(app, measured(db, clipped));
    if idle {
        app.state = Activity::Idle;
    }
}

fn names(app: &App) -> Vec<&str> {
    app.items.iter().map(|i| i.m.meta.name.as_str()).collect()
}

// Bugs caught: 0, False positives: 1
#[test]
fn measured_adds_named_item() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = ready(data_in(&dir));
    heard(&mut app, 0.0, false);
    heard(&mut app, 1.0, false);
    assert_eq!(names(&app), ["L seat 1", "L seat 2"]);
    run(&mut app, Message::SidePicked(Side::R));
    heard(&mut app, 2.0, false);
    assert_eq!(names(&app), ["L seat 1", "L seat 2", "R seat 1"]);
    let last = &app.items[2].m.meta;
    assert_eq!(
        (last.kind, last.side, last.seating),
        (Kind::Sweep, Some(Side::R), Some(1))
    );
    assert_eq!(app.status.level, StatusLevel::Info);
    assert!(
        app.status.text.contains("drift +41 ppm"),
        "{}",
        app.status.text
    );
    // Every one is in the recovery file, with its impulse response, and each
    // has its own colour.
    let stored = recovered(&app);
    assert_eq!(stored.len(), 3);
    assert!(stored.iter().all(|e| e.ir.is_some()));
    let colours: Vec<_> = app.items.iter().map(|i| i.color).collect();
    assert_eq!(colours, [0, 1, 2]);
    assert!(app.items.iter().all(|i| i.shown));
}

// Bugs caught: 0, False positives: 1
#[test]
fn clipped_capture_shows_warning() {
    let mut app = ready(None);
    heard(&mut app, 0.0, true);
    assert_eq!(app.items.len(), 1, "a clipped capture is still kept");
    assert!(app.items[0].m.meta.clipped);
    assert_eq!(app.status.level, StatusLevel::Warn);
    assert!(
        app.status.text.contains("input clipped"),
        "{}",
        app.status.text
    );
}

// Bugs caught: 0, False positives: 0
#[test]
fn measure_timeout_reports_best_ratio() {
    let mut app = ready(None);
    run(&mut app, Message::Measure);
    assert_eq!(app.state, Activity::Measuring { played: None });
    assert!(app.play_deadline.is_some(), "the rendered sweep was sent");
    run(&mut app, Message::Audio(AudioEvent::PlayFinished));
    let Activity::Measuring { played: Some(_) } = app.state else {
        panic!("playback end not stamped: {:?}", app.state)
    };
    run(&mut app, Message::Session(SessionEvent::BestRatio(4.2)));
    // Not yet: the sweep may still be on its way through the device.
    run(&mut app, Message::Tick);
    assert!(matches!(app.state, Activity::Measuring { .. }));
    app.state = Activity::Measuring {
        played: Some(Instant::now() - Duration::from_secs_f64(3.1)),
    };
    run(&mut app, Message::Tick);
    assert_eq!(app.state, Activity::Idle);
    assert_eq!(app.status.level, StatusLevel::Error);
    assert!(app.status.text.contains("4.2×"), "{}", app.status.text);
    assert!(app.status.text.contains("12×"), "{}", app.status.text);
    assert!(app.items.is_empty());
}

// Bugs caught: 0, False positives: 0
#[test]
fn input_lost_stops_listening() {
    let mut app = ready(None);
    run(&mut app, Message::Listen(true));
    assert_eq!(app.state, Activity::Listening);
    run(
        &mut app,
        Message::Audio(AudioEvent::InputLost("gone".into())),
    );
    assert_eq!(app.state, Activity::Idle);
    assert_eq!(app.status.level, StatusLevel::Error);
    assert!(app.status.text.contains("gone"), "{}", app.status.text);
    // Not until the input runs again; then listening can start again.
    run(&mut app, Message::Listen(true));
    assert_eq!(app.state, Activity::Idle);
    assert!(
        app.status.text.contains("Input not running"),
        "{}",
        app.status.text
    );
    run(&mut app, started());
    run(&mut app, Message::Listen(true));
    assert_eq!(app.state, Activity::Listening);
}

// Bugs caught: 0, False positives: 0
#[test]
fn measured_while_measuring_ends_the_measurement_but_listening_goes_on() {
    let mut app = ready(None);
    run(&mut app, Message::Measure);
    heard(&mut app, 0.0, false);
    assert_eq!(app.state, Activity::Idle);
    run(&mut app, Message::Listen(true));
    heard(&mut app, 0.0, false);
    heard(&mut app, 0.0, false);
    assert_eq!(app.state, Activity::Listening);
    assert_eq!(app.items.len(), 3);
    // A missing end marker warns and keeps listening.
    run(&mut app, Message::Session(SessionEvent::MissingEnd));
    assert_eq!(app.state, Activity::Listening);
    assert_eq!(app.status.level, StatusLevel::Warn);
}

// Bugs caught: 0, False positives: 0
#[test]
fn measure_needs_a_running_input_and_an_output_channel() {
    let mut app = App::new_for_test(Prefs::default(), None);
    run(&mut app, Message::Audio(AudioEvent::Nodes(vec![phones()])));
    run(&mut app, Message::Measure);
    assert_eq!(app.state, Activity::Idle);
    assert_eq!(app.status.text, "Choose an input first");
    // A saved input that isn't in the device list.
    run(&mut app, Message::InputPicked(mic().name));
    run(&mut app, Message::Measure);
    assert_eq!(app.status.text, "Input not connected");
    run(
        &mut app,
        Message::Audio(AudioEvent::Nodes(vec![mic(), phones()])),
    );
    run(&mut app, Message::Measure);
    assert_eq!(app.status.text, "Input not running");
    run(&mut app, started());
    run(&mut app, Message::OutputPicked(phones().name));
    // Picking an output plays the Side's channel.
    assert_eq!(app.prefs.output_channels, ["FL"]);
    // An output with no channels cannot play.
    app.prefs.output_channels.clear();
    run(&mut app, Message::Measure);
    assert_eq!(app.state, Activity::Idle);
    assert_eq!(app.status.level, StatusLevel::Error);
    run(&mut app, Message::SidePicked(Side::R));
    run(&mut app, Message::Measure);
    assert_eq!(app.state, Activity::Measuring { played: None });
}

// Bugs caught: 0, False positives: 0
#[test]
fn measure_can_be_stopped() {
    let mut app = ready(None);
    run(&mut app, Message::Measure);
    // The Measure tab is named like the button: look at the tab's content.
    assert!(crate::testing::shows(app.view(), "Stop"));
    assert!(!crate::testing::shows(
        crate::devices::view(&app),
        "Measure"
    ));
    run(&mut app, Message::Stop);
    assert_eq!(app.state, Activity::Idle);
    assert_eq!(app.play_deadline, None);
    assert_eq!(app.status.text, "Measurement stopped");
    assert!(crate::testing::shows(crate::devices::view(&app), "Measure"));
    // A playback end arriving after the stop changes nothing.
    run(&mut app, Message::Audio(AudioEvent::PlayFinished));
    run(&mut app, Message::Tick);
    assert_eq!(app.state, Activity::Idle);
}

// Bugs caught: 0, False positives: 2
#[test]
fn a_stalled_playback_gives_up_at_its_deadline() {
    let mut app = ready(None);
    run(&mut app, Message::Measure);
    // The engine's start timeout, the stimulus (9 s for a 6 s sweep), the
    // detection wait and slack, from when it was sent.
    let deadline = app.play_deadline.expect("sent");
    let wait = deadline - Instant::now();
    assert!(
        wait > Duration::from_secs(23) && wait <= Duration::from_secs(24),
        "{wait:?}"
    );
    // No PlayFinished ever comes (a stream paused after it started).
    run(&mut app, Message::Tick);
    assert_eq!(app.state, Activity::Measuring { played: None });
    app.play_deadline = Some(Instant::now() - Duration::from_millis(1));
    run(&mut app, Message::Tick);
    assert_eq!(app.state, Activity::Idle);
    assert_eq!(app.status.level, StatusLevel::Error);
    assert_eq!(app.status.text, "Playback did not finish");
}

// Bugs caught: 0, False positives: 0
#[test]
fn a_playback_end_before_the_sweep_is_sent_is_stale() {
    let mut app = ready(None);
    // Measure without running its render task: the sweep isn't sent yet.
    let _render = app.update(Message::Measure);
    assert_eq!(app.status.text, "Preparing the sweep…");
    run(&mut app, Message::Audio(AudioEvent::PlayFinished));
    assert_eq!(app.state, Activity::Measuring { played: None });
    // The length can't change under a measurement.
    run(&mut app, Message::LengthPicked(SweepLength::Long));
    assert_eq!(app.prefs.length, SweepLength::Normal);
}

// Bugs caught: 0, False positives: 1
#[test]
fn a_failed_recovery_write_warns_but_keeps_the_item() {
    let dir = tempfile::tempdir().unwrap();
    // A file where the recovery directory goes: every write fails.
    std::fs::write(dir.path().join("recovery"), "").unwrap();
    let mut app = ready(data_in(&dir));
    heard(&mut app, 0.0, false);
    assert_eq!(app.items.len(), 1);
    assert_eq!(app.status.level, StatusLevel::Warn);
    assert!(
        app.status.text.starts_with("Recovery file not written"),
        "{}",
        app.status.text
    );
}

// Bugs caught: 0, False positives: 0
#[test]
fn boot_notes_are_all_kept() {
    assert_eq!(Status::joined(Vec::new()), None);
    let status = Status::joined(vec![
        ("Preferences unreadable".into(), StatusLevel::Warn),
        ("PipeWire unavailable".into(), StatusLevel::Error),
    ])
    .unwrap();
    assert_eq!(status.text, "Preferences unreadable · PipeWire unavailable");
    assert_eq!(status.level, StatusLevel::Error);
}

// Bugs caught: 0, False positives: 0
#[test]
fn clip_is_held_for_a_second() {
    let mut app = ready(None);
    let level = |peak_dbfs, clipped| Message::Session(SessionEvent::Level { peak_dbfs, clipped });
    run(&mut app, level(-18.0, false));
    assert_eq!(app.level, (-18.0, false));
    run(&mut app, level(0.0, true));
    run(&mut app, level(-20.0, false));
    assert_eq!(app.level, (-20.0, true), "held");
    app.clip_at = Some(Instant::now() - Duration::from_millis(1100));
    run(&mut app, level(-20.0, false));
    assert_eq!(app.level, (-20.0, false), "released");
}

// Bugs caught: 0, False positives: 0
#[test]
fn engine_error_offers_reconnect() {
    let mut app = ready(None);
    run(&mut app, Message::Listen(true));
    run(
        &mut app,
        Message::Audio(AudioEvent::Error("connection reset".into())),
    );
    assert_eq!(app.state, Activity::Idle);
    assert_eq!(app.status.level, StatusLevel::Error);
    assert!(app.status.text.contains("connection reset"));
    assert!(crate::testing::shows(app.view(), "Reconnect"));
}

/// `dir` as the app's `$XDG_DATA_HOME/fastgraph`.
fn data_in(dir: &tempfile::TempDir) -> Option<PathBuf> {
    Some(dir.path().to_path_buf())
}

/// A flat response of `db` on the 48 kHz grid.
fn flat(db: f64) -> Response {
    let freq = log_grid(48_000);
    Response {
        db: vec![db; freq.len()],
        freq,
        phase: None,
    }
}

fn id(app: &App, n: usize) -> String {
    app.items[n].m.meta.id.clone()
}

/// Click an item's Delete.
fn delete(app: &mut App, id: &str) {
    run(app, Message::Delete(id.into()));
}

/// An app with no smoothing and no align, so display levels are exact.
fn plain(data: Option<PathBuf>) -> App {
    let mut app = ready(data);
    run(&mut app, Message::SmoothingPicked(Smoothing::None));
    run(&mut app, Message::Align(false));
    app
}

// Bugs caught: 0, False positives: 1
#[test]
fn average_of_shown() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = plain(data_in(&dir));
    for db in [0.0, 6.0, 20.0] {
        heard(&mut app, db, false);
    }
    let hidden = id(&app, 2);
    run(&mut app, Message::Shown(hidden, false));
    run(&mut app, Message::AverageShown);
    assert_eq!(app.items.len(), 4);
    let avg = &app.items[3].m;
    assert_eq!(avg.meta.name, "Average of 2");
    assert_eq!(avg.meta.kind, Kind::Average);
    assert_eq!(avg.meta.sources, [id(&app, 0), id(&app, 1)]);
    assert!(avg.response.db.iter().all(|d| (d - 3.0).abs() < 1e-9));
    // It is in the recovery file.
    assert_eq!(recovered(&app).len(), 4);
}

// Bugs caught: 0, False positives: 1
#[test]
fn average_aligns_first_when_align_is_on() {
    let mut app = plain(None);
    run(&mut app, Message::Align(true));
    for db in [10.0, 30.0] {
        heard(&mut app, db, false);
    }
    run(&mut app, Message::AverageShown);
    // Both are 0 dB at 1 kHz first, so their mean is too.
    assert!(app.items[2].m.response.db.iter().all(|d| d.abs() < 1e-9));
}

// Bugs caught: 0, False positives: 1
#[test]
fn average_needs_two_shown() {
    let mut app = plain(None);
    heard(&mut app, 0.0, false);
    run(&mut app, Message::AverageShown);
    assert_eq!(app.items.len(), 1);
}

/// A constant +2 dB compensation curve.
fn comp_curve() -> (PathBuf, Response) {
    (
        PathBuf::from("/curves/coupler.txt"),
        Response {
            freq: vec![10.0, 24_000.0],
            db: vec![2.0, 2.0],
            phase: None,
        },
    )
}

// Bugs caught: 0, False positives: 1
#[test]
fn compensation_applies_to_measurements_only() {
    let mut app = plain(None);
    heard(&mut app, 0.0, false);
    run(
        &mut app,
        Message::Imported(0, Ok(vec![(PathBuf::from("/in/other.txt"), flat(0.0))])),
    );
    run(&mut app, Message::CompensationLoaded(Ok(comp_curve())));
    run(&mut app, Message::CompensationEnabled(true));
    assert!((app.display(&app.items[0]).db[0] + 2.0).abs() < 1e-9);
    assert_eq!(app.display(&app.items[1]).db[0], 0.0);
    // Inverted, it adds; disabled, it does nothing.
    run(&mut app, Message::CompensationInvert(true));
    assert!((app.display(&app.items[0]).db[0] - 2.0).abs() < 1e-9);
    run(&mut app, Message::CompensationEnabled(false));
    assert_eq!(app.display(&app.items[0]).db[0], 0.0);
    // An average of compensated measurements is not compensated again.
    run(&mut app, Message::CompensationEnabled(true));
    run(&mut app, Message::CompensationInvert(false));
    run(&mut app, Message::AverageShown);
    assert!((app.items[2].m.response.db[0] + 1.0).abs() < 1e-9);
    assert!((app.display(&app.items[2]).db[0] + 1.0).abs() < 1e-9);
}

// Bugs caught: 0, False positives: 1
#[test]
fn an_average_of_measurements_follows_the_compensation() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = plain(data_in(&dir));
    heard(&mut app, 0.0, false);
    heard(&mut app, 4.0, false);
    run(&mut app, Message::CompensationLoaded(Ok(comp_curve())));
    run(&mut app, Message::CompensationEnabled(true));
    run(&mut app, Message::AverageShown);
    let avg = &app.items[2];
    assert!(avg.m.meta.compensable);
    assert_eq!(avg.m.meta.baked_compensation, None);
    // Stored without the compensation, shown with it.
    assert!(avg.m.response.db.iter().all(|d| (d - 2.0).abs() < 1e-9));
    assert!(app.display(avg).db.iter().all(|d| d.abs() < 1e-9));
    let text = app.export_text(avg);
    assert!(text.contains("* Compensation: coupler.txt\n"), "{text}");
    // Apply off: the average follows, like its sources.
    run(&mut app, Message::CompensationEnabled(false));
    let avg = &app.items[2];
    assert!(app.display(avg).db.iter().all(|d| (d - 2.0).abs() < 1e-9));
    assert!(app.export_text(avg).contains("* Compensation: none\n"));
    // Inverted, it adds.
    run(&mut app, Message::CompensationEnabled(true));
    run(&mut app, Message::CompensationInvert(true));
    let avg = &app.items[2];
    assert!(app.display(avg).db.iter().all(|d| (d - 4.0).abs() < 1e-9));
    assert!(
        app.export_text(avg)
            .contains("* Compensation: coupler.txt (inverted)\n")
    );
    // An average of it and a measurement is compensable too, and all of it
    // comes back so from the recovery file.
    let second = id(&app, 1);
    run(&mut app, Message::Shown(second, false));
    run(&mut app, Message::AverageShown);
    assert!(app.items[3].m.meta.compensable);
    let again = reopened(&app);
    assert!(again.items[2].m.meta.compensable);
    assert!(again.items[3].m.meta.compensable);
}

// Bugs caught: 0, False positives: 1
#[test]
fn a_mixed_average_names_its_built_in_compensation() {
    let mut app = plain(None);
    heard(&mut app, 0.0, false);
    run(
        &mut app,
        Message::Imported(0, Ok(vec![(PathBuf::from("/in/other.txt"), flat(0.0))])),
    );
    run(&mut app, Message::CompensationLoaded(Ok(comp_curve())));
    run(&mut app, Message::CompensationEnabled(true));
    run(&mut app, Message::CompensationInvert(true));
    run(&mut app, Message::AverageShown);
    let avg = &app.items[2];
    assert!(!avg.m.meta.compensable);
    assert_eq!(
        avg.m.meta.baked_compensation.as_deref(),
        Some("coupler.txt (inverted)")
    );
    // Turning the compensation off leaves it as built.
    run(&mut app, Message::CompensationEnabled(false));
    let avg = &app.items[2];
    assert!(app.display(avg).db.iter().all(|d| (d - 1.0).abs() < 1e-9));
    let text = app.export_text(avg);
    assert!(
        text.contains("* Compensation: coupler.txt (inverted)\n"),
        "{text}"
    );
    assert_eq!(
        crate::list::caption(avg),
        "Average · comp coupler.txt (inverted)"
    );
    assert!(crate::testing::shows(
        app.view(),
        "Average · comp coupler.txt (inverted)"
    ));
    // Imported curves alone: nothing was applied, nothing is named.
    run(
        &mut app,
        Message::Imported(0, Ok(vec![(PathBuf::from("/in/third.txt"), flat(3.0))])),
    );
    for n in 0..3 {
        let item = id(&app, n);
        run(&mut app, Message::Shown(item, n == 1));
    }
    run(&mut app, Message::CompensationEnabled(true));
    run(&mut app, Message::AverageShown);
    let avg = &app.items[4].m.meta;
    assert!(!avg.compensable);
    assert_eq!(avg.baked_compensation, None);
}

// Bugs caught: 0, False positives: 0
#[test]
fn a_bad_compensation_keeps_the_old_one() {
    let mut app = plain(None);
    run(&mut app, Message::CompensationLoaded(Ok(comp_curve())));
    run(
        &mut app,
        Message::CompensationLoaded(Err("x.txt: no data".into())),
    );
    assert_eq!(app.status.level, StatusLevel::Warn);
    assert_eq!(app.comp.as_ref().unwrap().0, comp_curve().0);
    assert_eq!(app.prefs.compensation, Some(comp_curve().0));
}

// Bugs caught: 0, False positives: 1
#[test]
fn display_aligns_at_one_kilohertz() {
    let mut app = plain(None);
    heard(&mut app, 7.0, false);
    assert_eq!(app.display(&app.items[0]).db[0], 7.0);
    run(&mut app, Message::Align(true));
    assert!(app.display(&app.items[0]).db.iter().all(|d| d.abs() < 1e-9));
    assert!(app.prefs.align);
}

// Bugs caught: 0, False positives: 1
#[test]
fn bad_import_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = plain(data_in(&dir));
    heard(&mut app, 0.0, false);
    run(
        &mut app,
        Message::Imported(0, Err("bad.txt: line 3: non-finite value".into())),
    );
    assert_eq!(app.items.len(), 1);
    assert_eq!(app.status.level, StatusLevel::Error);
    assert!(app.status.text.contains("bad.txt"), "{}", app.status.text);
    assert_eq!(recovered(&app).len(), 1);
}

// Bugs caught: 0, False positives: 0
#[test]
fn import_adds_named_items_and_keeps_them_for_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = plain(data_in(&dir));
    run(
        &mut app,
        Message::Imported(
            0,
            Ok(vec![
                (PathBuf::from("/in/Origin.txt"), flat(1.0)),
                (PathBuf::from("/in/Target.csv"), flat(2.0)),
            ]),
        ),
    );
    assert_eq!(names(&app), ["Origin", "Target"]);
    let m = &app.items[0].m.meta;
    assert_eq!(m.kind, Kind::Imported);
    assert_eq!(m.source, Some(PathBuf::from("/in/Origin.txt")));
    assert_eq!(recovered(&app).len(), 2);
}

// Bugs caught: 0, False positives: 2
#[test]
fn delete_removes_from_the_recovery_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = plain(data_in(&dir));
    heard(&mut app, 0.0, false);
    heard(&mut app, 1.0, false);
    let gone = id(&app, 0);
    delete(&mut app, &gone);
    assert_eq!(app.items.len(), 1);
    let stored = recovered(&app);
    assert_eq!(stored.len(), 1);
    assert!(stored.iter().all(|m| m.meta.id != gone));
}

// Bugs caught: 1, False positives: 1
#[test]
fn delete_acts_on_the_first_click() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = plain(data_in(&dir));
    heard(&mut app, 0.0, false);
    heard(&mut app, 1.0, false);
    let first = id(&app, 0);
    assert!(!crate::testing::shows(app.view(), "Confirm"));
    run(&mut app, Message::Delete(first));
    assert_eq!(names(&app), ["L seat 2"]);
    assert_eq!(recovered(&app).len(), 1);
}

// Bugs caught: 0, False positives: 1
#[test]
fn hidden_items_stay_hidden_in_the_recovered_set() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = plain(data_in(&dir));
    for db in [0.0, 1.0, 2.0] {
        heard(&mut app, db, false);
    }
    let (second, third) = (id(&app, 1), id(&app, 2));
    run(&mut app, Message::Shown(second, false));
    run(&mut app, Message::Shown(third.clone(), false));
    run(&mut app, Message::Shown(third, true));
    assert!(!app.items[1].m.meta.shown);
    let again = reopened(&app);
    let shown: Vec<bool> = again.items.iter().map(|i| i.shown).collect();
    assert_eq!(shown, [true, false, true]);
    // The rest of the metadata is untouched.
    assert_eq!(again.items[1].m.meta, app.items[1].m.meta);
}

// Bugs caught: 0, False positives: 0
#[test]
fn unreadable_earlier_measurements_warn_on_boot() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("measurements");
    old_measurement(&old, "1-0", "Good");
    std::fs::write(old.join("2-0.toml"), "not toml [").unwrap();
    let mut app = App::new_for_test(Prefs::default(), data_in(&dir));
    for message in worker_results(&app) {
        run(&mut app, message);
    }
    assert!(app.items.is_empty());
    assert_eq!(app.status.level, StatusLevel::Warn);
    assert!(
        app.status.text.starts_with(&format!(
            "{MIGRATED} · 1 earlier measurements unreadable: 2-0"
        )),
        "{}",
        app.status.text
    );
    // A second start finds them moved and says nothing.
    let mut again = App::new_for_test(Prefs::default(), data_in(&dir));
    for message in worker_results(&again) {
        run(&mut again, message);
    }
    assert_eq!(again.status.text, "");
}

// Bugs caught: 0, False positives: 1
#[test]
fn rename_commits_on_enter_and_rejects_empty() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = plain(data_in(&dir));
    heard(&mut app, 0.0, false);
    let item = id(&app, 0);
    run(&mut app, Message::RenameStart(item.clone()));
    run(&mut app, Message::RenameInput("Left cup".into()));
    run(&mut app, Message::RenameCommit);
    assert_eq!(names(&app), ["Left cup"]);
    assert!(app.renaming.is_none());
    assert_eq!(recovered(&app)[0].meta.name, "Left cup");
    // Empty (or blank) names keep the old one.
    run(&mut app, Message::RenameStart(item));
    run(&mut app, Message::RenameInput("  ".into()));
    run(&mut app, Message::RenameCommit);
    assert_eq!(names(&app), ["Left cup"]);
    assert!(app.renaming.is_none());
    assert_eq!(app.status.level, StatusLevel::Warn);
}

// Bugs caught: 0, False positives: 1
#[test]
fn rename_cancels_on_escape() {
    let mut app = plain(None);
    heard(&mut app, 0.0, false);
    let item = id(&app, 0);
    run(&mut app, Message::RenameStart(item));
    run(&mut app, Message::RenameInput("Other".into()));
    run(&mut app, Message::RenameCancel);
    assert_eq!(names(&app), ["L seat 1"]);
    assert!(app.renaming.is_none());
}

// Bugs caught: 0, False positives: 1
#[test]
fn export_then_import_gives_the_same_trace() {
    let mut app = plain(None);
    app.items.clear();
    heard(&mut app, 0.0, false);
    app.items[0].m.response = headphone(0.0, 0.5);
    run(&mut app, Message::CompensationLoaded(Ok(comp_curve())));
    run(&mut app, Message::CompensationEnabled(true));
    run(&mut app, Message::SmoothingPicked(Smoothing::Oct12));
    let item = &app.items[0];
    let text = app.export_text(item);
    assert!(text.contains("Measurement: L seat 1"), "{text}");
    assert!(text.contains("fastgraph"), "{text}");
    assert!(text.contains("Compensation: coupler.txt"), "{text}");
    let expected = app.processed(item).smoothed(Smoothing::Oct12);
    let back = fastgraph_core::curve_io::read_curve(&text).unwrap();
    assert_eq!(back.freq.len(), expected.freq.len());
    for ((f, d), (bf, bd)) in expected
        .freq
        .iter()
        .zip(&expected.db)
        .zip(back.freq.iter().zip(&back.db))
    {
        assert!((f - bf).abs() < 1e-5 * f, "{f} vs {bf}");
        assert!((d - bd).abs() < 1e-4, "{d} vs {bd}");
    }
    // Align does not change what is written.
    run(&mut app, Message::Align(true));
    assert_eq!(app.export_text(&app.items[0]), text);
}

// Bugs caught: 0, False positives: 1
#[test]
fn export_writes_phase_only_without_smoothing() {
    let mut app = plain(None);
    heard(&mut app, 0.0, false);
    app.items[0].m.response.phase = Some(vec![10.0; app.items[0].m.response.freq.len()]);
    assert!(app.export_text(&app.items[0]).contains("Phase"));
    run(&mut app, Message::SmoothingPicked(Smoothing::Oct3));
    assert!(!app.export_text(&app.items[0]).contains("Phase"));
}

// Bugs caught: 0, False positives: 1
#[test]
fn toolbar_choices_redraw_the_graph() {
    let mut app = plain(None);
    heard(&mut app, 5.0, false);
    assert_eq!(app.graph.top_db(10.0), crate::plot::top_db(Some(5.0)));
    run(&mut app, Message::Align(true));
    assert_eq!(app.graph.top_db(10.0), crate::plot::top_db(Some(0.0)));
}

// Bugs caught: 0, False positives: 1
#[test]
fn toolbar_and_list_render() {
    let mut app = plain(None);
    heard(&mut app, 0.0, false);
    heard(&mut app, 1.0, false);
    for label in [
        "Import…",
        "Average shown",
        "Comp: none",
        "Align",
        "Export…",
        "Delete",
        "Rename",
    ] {
        assert!(crate::testing::shows(app.view(), label), "{label}");
    }
    run(&mut app, Message::CompensationLoaded(Ok(comp_curve())));
    assert!(crate::testing::shows(app.view(), "Comp: coupler.txt"));
}

// Bugs caught: 0, False positives: 1
#[test]
fn average_is_built_from_unsmoothed_responses() {
    let mut app = plain(None);
    heard(&mut app, 0.0, false);
    heard(&mut app, 0.0, false);
    app.items[0].m.response = headphone(0.0, 3.0);
    app.items[1].m.response = headphone(4.0, -3.0);
    run(&mut app, Message::SmoothingPicked(Smoothing::Oct3));
    run(&mut app, Message::AverageShown);
    let raw =
        fastgraph_core::response::average(&[&app.items[0].m.response, &app.items[1].m.response])
            .unwrap();
    assert_eq!(app.items[2].m.response, raw);
    // Smoothing would have changed it.
    assert_ne!(raw.smoothed(Smoothing::Oct3), raw);
}

// Bugs caught: 0, False positives: 0
#[test]
fn one_bad_file_fails_the_whole_import() {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, text: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        path
    };
    let good1 = write("a.txt", "100 1\n200 2\n");
    let bad = write("bad.txt", "100 1\n100 2\n");
    let good2 = write("c.txt", "100 1\n300 3\n");
    let ok = crate::toolbar::read_files(&[good1.clone(), good2.clone()]).unwrap();
    assert_eq!(ok.len(), 2);
    let err = crate::toolbar::read_files(&[good1, bad, good2]).unwrap_err();
    assert!(err.contains("bad.txt"), "{err}");
    let mut app = plain(None);
    run(&mut app, Message::Imported(0, Err(err)));
    assert!(app.items.is_empty());
    assert_eq!(app.status.level, StatusLevel::Error);
}

// Bugs caught: 0, False positives: 0
#[test]
fn comp_label_says_when_the_curve_is_not_loaded() {
    let path = PathBuf::from("/c/coupler.txt");
    assert_eq!(crate::toolbar::comp_label(None, false), "Comp: none");
    assert_eq!(
        crate::toolbar::comp_label(Some(&path), true),
        "Comp: coupler.txt"
    );
    assert_eq!(
        crate::toolbar::comp_label(Some(&path), false),
        "Comp: coupler.txt (not loaded)"
    );
}

// Bugs caught: 0, False positives: 0
#[test]
fn a_boot_compensation_warning_keeps_a_boot_error() {
    let prefs = Prefs {
        compensation: Some(PathBuf::from("/gone/coupler.txt")),
        ..Prefs::default()
    };
    let mut app = App::new_for_test(prefs, None);
    app.status = Status {
        text: "Measurements can't be stored".into(),
        level: StatusLevel::Error,
    };
    let _ = app.reload_compensation();
    run(
        &mut app,
        Message::CompensationLoaded(Err("coupler.txt: gone".into())),
    );
    assert_eq!(app.status.level, StatusLevel::Error);
    assert!(app.status.text.contains("can't be stored"));
    assert!(app.status.text.contains("Compensation not loaded"));
    // The prefs keep the path, and a later failure replaces plainly.
    assert!(app.prefs.compensation.is_some());
    run(&mut app, Message::CompensationLoaded(Err("again".into())));
    assert_eq!(app.status.level, StatusLevel::Warn);
    assert!(!app.status.text.contains("can't be stored"));
}

// Bugs caught: 0, False positives: 0
#[test]
fn export_is_written_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.txt");
    fastgraph_core::fsutil::write_atomic(&path, b"* x\n").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"* x\n");
    let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
    assert_eq!(names.len(), 1, "no .tmp left behind");
}

fn rta_event(db: f64, frames: usize) -> Message {
    let freq = log_grid(48_000);
    Message::Session(SessionEvent::Rta {
        response: Response {
            db: vec![db; freq.len()],
            freq,
            phase: None,
        },
        frames,
    })
}

/// A ready app in RTA mode.
fn rta_ready(data: Option<PathBuf>) -> App {
    let mut app = ready(data);
    run(&mut app, Message::ModeSweep(false));
    app
}

// Bugs caught: 0, False positives: 0
#[test]
fn rta_capture_adds_item() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = rta_ready(data_in(&dir));
    run(&mut app, Message::RtaListen(true));
    run(&mut app, rta_event(-30.0, 12));
    run(&mut app, Message::RtaCapture);
    run(&mut app, Message::RtaCapture);
    run(&mut app, Message::SidePicked(Side::R));
    run(&mut app, Message::RtaCapture);
    assert_eq!(names(&app), ["L RTA 1", "L RTA 2", "R RTA 1"]);
    let m = &app.items[0].m;
    assert_eq!(m.meta.kind, Kind::Rta);
    assert_eq!(m.response.db[0], -30.0, "the raw response is stored");
    assert!(app.rta.live.is_some(), "the live trace is kept");
    assert_eq!(app.rta.live.as_ref().unwrap().1, 12);
    assert_eq!(recovered(&app).len(), 3);
}

// Bugs caught: 0, False positives: 0
#[test]
fn capture_without_data_is_noop() {
    let mut app = rta_ready(None);
    run(&mut app, Message::RtaCapture);
    assert!(app.items.is_empty());
    assert_eq!(app.status.level, StatusLevel::Warn);
    assert_eq!(app.status.text, "No RTA data yet");
}

// Bugs caught: 0, False positives: 0
#[test]
fn the_live_trace_is_not_stored_compensated() {
    let mut app = rta_ready(None);
    run(&mut app, Message::CompensationLoaded(Ok(comp_curve())));
    run(&mut app, Message::CompensationEnabled(true));
    run(&mut app, Message::RtaListen(true));
    run(&mut app, rta_event(-30.0, 5));
    run(&mut app, Message::RtaCapture);
    assert_eq!(app.items[0].m.response.db[0], -30.0);
    // The drawn trace is what the item would be drawn as.
    let shown = app.display(&app.items[0]);
    assert_eq!(app.live_display().unwrap(), shown);
}

// Bugs caught: 0, False positives: 0
#[test]
fn switching_mode_stops_the_noise_and_clears_the_live_trace() {
    let mut app = rta_ready(None);
    run(&mut app, Message::RtaNoise(true));
    assert_eq!(app.state, Activity::Rta { playing: true });
    run(&mut app, rta_event(-30.0, 3));
    assert!(app.rta.live.is_some());
    app.sent.borrow_mut().clear();
    run(&mut app, Message::ModeSweep(true));
    assert_eq!(
        *app.sent.borrow(),
        [
            Sent::StopPlay,
            Sent::Session(SessionCommand::SetMode(Mode::Idle))
        ]
    );
    assert_eq!(app.state, Activity::Idle);
    assert!(app.sweep_mode);
    assert!(app.rta.live.is_none());
    // Listening ends the same way.
    run(&mut app, Message::ModeSweep(false));
    run(&mut app, Message::RtaListen(true));
    assert_eq!(app.state, Activity::Rta { playing: false });
    run(&mut app, rta_event(-30.0, 3));
    run(&mut app, Message::ModeSweep(true));
    assert_eq!(app.state, Activity::Idle);
    assert!(app.rta.live.is_none());
    // Late RTA events in Sweep mode are ignored.
    run(&mut app, rta_event(-30.0, 9));
    assert!(app.rta.live.is_none());
}

// Bugs caught: 0, False positives: 0
#[test]
fn changing_the_input_mid_rta_resets_the_average() {
    let mut app = rta_ready(None);
    run(&mut app, Message::RtaListen(true));
    run(&mut app, rta_event(-30.0, 8));
    app.sent.borrow_mut().clear();
    run(&mut app, Message::InputChannelPicked("FR".into()));
    assert!(
        app.sent
            .borrow()
            .contains(&Sent::Session(SessionCommand::ResetRta))
    );
    assert!(app.rta.live.is_none());
    assert_eq!(app.state, Activity::Rta { playing: false });
    run(&mut app, rta_event(-30.0, 2));
    run(&mut app, Message::RtaReset);
    assert!(app.rta.live.is_none());
    run(&mut app, rta_event(-30.0, 2));
    run(&mut app, Message::Session(SessionEvent::RtaReset));
    assert!(app.rta.live.is_none());
}

// Bugs caught: 1, False positives: 0
#[test]
fn stale_rta_events_after_stop_are_ignored() {
    let mut app = rta_ready(None);
    run(&mut app, Message::RtaListen(true));
    run(&mut app, rta_event(-30.0, 4));
    run(&mut app, Message::RtaListen(false));
    // Queued before the stop, delivered after it.
    run(&mut app, rta_event(-30.0, 5));
    assert!(app.rta.live.is_some(), "the average shown at Stop stays");
    run(&mut app, Message::RtaListen(true));
    assert!(app.rta.live.is_none(), "a restart begins empty");
    run(&mut app, Message::RtaListen(false));
    run(&mut app, rta_event(-30.0, 6));
    assert!(app.rta.live.is_none());
    run(&mut app, Message::RtaCapture);
    assert!(app.items.is_empty());
    assert_eq!(app.status.text, "No RTA data yet");
    assert_eq!(app.status.level, StatusLevel::Warn);
}

fn last_play(app: &App) -> Option<Sent> {
    app.sent
        .borrow()
        .iter()
        .rev()
        .find(|s| matches!(s, Sent::Play { .. }))
        .cloned()
}

// Bugs caught: 2, False positives: 0
#[test]
fn level_and_output_changes_restart_the_noise() {
    let mut app = rta_ready(None);
    run(&mut app, Message::RtaNoise(true));
    let play = |sink: &str, channels: Vec<usize>, gain: f32| Sent::Play {
        sink: sink.into(),
        channels,
        gain,
        looped: true,
    };
    assert_eq!(
        last_play(&app),
        Some(play(&phones().name, vec![0], 0.5023773))
    );
    run(&mut app, rta_event(-30.0, 4));
    // The level is applied when the slider is let go, and keeps the average.
    run(&mut app, Message::Level(-20.0));
    assert_eq!(
        last_play(&app),
        Some(play(&phones().name, vec![0], 0.5023773))
    );
    app.sent.borrow_mut().clear();
    run(&mut app, Message::LevelReleased);
    assert_eq!(last_play(&app), Some(play(&phones().name, vec![0], 0.2)));
    assert!(
        !app.sent
            .borrow()
            .contains(&Sent::Session(SessionCommand::ResetRta))
    );
    assert!(app.rta.live.is_some());
    // A side change restarts it and starts the average over.
    app.sent.borrow_mut().clear();
    run(&mut app, Message::SidePicked(Side::R));
    assert_eq!(last_play(&app), Some(play(&phones().name, vec![1], 0.2)));
    assert!(
        app.sent
            .borrow()
            .contains(&Sent::Session(SessionCommand::ResetRta))
    );
    assert!(app.rta.live.is_none());
    // Another output.
    let speakers = node(60, "alsa_output.speakers", "Speakers", true);
    run(
        &mut app,
        Message::Audio(AudioEvent::Nodes(vec![mic(), phones(), speakers])),
    );
    app.sent.borrow_mut().clear();
    run(
        &mut app,
        Message::OutputPicked("alsa_output.speakers".into()),
    );
    assert_eq!(
        last_play(&app),
        Some(play("alsa_output.speakers", vec![1], 0.2))
    );
    // One restart, not one per changed setting.
    let count = |wanted: fn(&Sent) -> bool| app.sent.borrow().iter().filter(|s| wanted(s)).count();
    assert_eq!(count(|s| matches!(s, Sent::Play { .. })), 1);
    assert_eq!(count(|s| *s == Sent::Session(SessionCommand::ResetRta)), 1);
}

// Bugs caught: 0, False positives: 0
#[test]
fn level_changes_while_idle_play_nothing() {
    let mut app = rta_ready(None);
    run(&mut app, Message::LevelReleased);
    run(&mut app, Message::SidePicked(Side::R));
    assert_eq!(last_play(&app), None);
}

// Bugs caught: 0, False positives: 0
#[test]
fn an_rta_capture_is_flagged_when_the_input_clipped() {
    let mut app = rta_ready(None);
    let clip = Message::Session(SessionEvent::Level {
        peak_dbfs: -0.1,
        clipped: true,
    });
    // Clipping outside an average doesn't count.
    run(&mut app, clip.clone());
    run(&mut app, Message::RtaListen(true));
    run(&mut app, rta_event(-30.0, 4));
    run(&mut app, Message::RtaCapture);
    assert!(!app.items[0].m.meta.clipped);
    run(&mut app, clip);
    run(&mut app, Message::RtaCapture);
    assert!(app.items[1].m.meta.clipped);
    assert_eq!(app.status.level, StatusLevel::Warn);
    assert!(
        app.status.text.contains("input clipped"),
        "{}",
        app.status.text
    );
    // A reset forgets it.
    run(&mut app, Message::RtaReset);
    run(&mut app, rta_event(-30.0, 1));
    run(&mut app, Message::RtaCapture);
    assert!(!app.items[2].m.meta.clipped);
}

// Bugs caught: 0, False positives: 0
#[test]
fn noise_needs_an_output_and_listen_needs_an_input() {
    let mut app = App::new_for_test(Prefs::default(), None);
    run(&mut app, Message::ModeSweep(false));
    run(&mut app, Message::Audio(AudioEvent::Nodes(vec![mic()])));
    run(&mut app, Message::InputPicked(mic().name));
    run(&mut app, started());
    run(&mut app, Message::RtaNoise(true));
    assert_eq!(app.state, Activity::Idle);
    assert_eq!(app.status.level, StatusLevel::Error);
    assert_eq!(app.status.text, "Choose an output first");
    // Listening needs no output.
    run(&mut app, Message::RtaListen(true));
    assert_eq!(app.state, Activity::Rta { playing: false });
    run(&mut app, Message::RtaListen(false));
    assert_eq!(app.state, Activity::Idle);
    run(
        &mut app,
        Message::Audio(AudioEvent::InputLost("gone".into())),
    );
    run(&mut app, Message::RtaListen(true));
    assert_eq!(app.state, Activity::Idle);
    assert_eq!(app.status.text, "Input not running");
}

// Bugs caught: 0, False positives: 0
#[test]
fn a_failed_noise_playback_stops_with_an_error() {
    let mut app = rta_ready(None);
    run(&mut app, Message::RtaNoise(true));
    assert!(app.rta.noise.is_some(), "the noise was rendered");
    assert!(app.rta.started);
    run(
        &mut app,
        Message::Audio(AudioEvent::PlayFailed("boom".into())),
    );
    assert_eq!(app.state, Activity::Idle);
    assert_eq!(app.status.level, StatusLevel::Error);
    assert!(app.status.text.contains("boom"), "{}", app.status.text);
    // A stale failure while idle changes nothing.
    run(
        &mut app,
        Message::Audio(AudioEvent::PlayFailed("again".into())),
    );
    assert!(app.status.text.contains("boom"));
}

// Bugs caught: 0, False positives: 0
#[test]
fn stopping_before_the_noise_is_rendered_cancels_its_start() {
    let mut app = rta_ready(None);
    let _ = app.update(Message::RtaNoise(true));
    assert!(!app.rta.started);
    run(&mut app, Message::RtaNoise(false));
    assert_eq!(app.state, Activity::Idle);
    run(
        &mut app,
        Message::NoiseRendered(Rendered(Arc::new(vec![0.0; 4]))),
    );
    assert!(!app.rta.started, "a late render starts nothing");
    assert!(app.rta.noise.is_some());
}

// Bugs caught: 0, False positives: 0
#[test]
fn rta_mode_view_shows_its_controls() {
    let mut app = rta_ready(None);
    for label in [
        "Start noise",
        "Listen",
        "Capture",
        "Reset",
        "0 averages",
        "Export noise WAV…",
    ] {
        assert!(crate::testing::shows(app.view(), label), "{label}");
    }
    run(&mut app, Message::RtaListen(true));
    run(&mut app, rta_event(-30.0, 12));
    assert!(crate::testing::shows(app.view(), "12 averages"));
    run(&mut app, Message::RtaListen(false));
    run(&mut app, Message::RtaNoise(true));
    assert!(crate::testing::shows(app.view(), "Stop noise"));
}

// Bugs caught: 0, False positives: 0
#[test]
fn window_renders() {
    let app = ready(None);
    assert!(crate::testing::shows(crate::devices::view(&app), "Measure"));
    assert!(crate::testing::shows(app.view(), "Export sweep WAV…"));
    assert!(crate::testing::shows(
        app.view(),
        "No measurements yet — Measure, Listen, or Import"
    ));
}

/// A headphone-like response: a bass shelf, the ear-gain peak, treble
/// roll-off, and a little seat-to-seat wobble.
fn headphone(offset: f64, wobble: f64) -> Response {
    let freq = log_grid(48_000);
    let db = freq
        .iter()
        .map(|&f: &f64| {
            let x = f.log10();
            let bass = 6.0 / (1.0 + (f / 120.0).powi(2));
            let ear = 9.0 * (-((x - 3.45) / 0.18).powi(2)).exp();
            let dip = -6.0 * (-((x - 3.9) / 0.06).powi(2)).exp();
            let top = -12.0 / (1.0 + (12_000.0 / f).powi(4));
            80.0 + offset + bass + ear + dip + top + wobble * (x * 9.0).sin()
        })
        .collect();
    Response {
        freq,
        db,
        phase: None,
    }
}

/// Where headless snapshots go: `$FASTGRAPH_SNAPSHOTS`, else
/// `target/snapshots/fastgraph`. A render is always written afresh.
fn snapshot(app: &App, name: &str) {
    snapshot_sized(app, name, crate::testing::SIZE);
}

fn snapshot_sized(app: &App, name: &str, size: (f32, f32)) {
    let dir = std::env::var_os("FASTGRAPH_SNAPSHOTS").map_or_else(
        || {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/snapshots/fastgraph")
        },
        PathBuf::from,
    );
    for renderer in ["wgpu", "tiny-skia"] {
        let _ = std::fs::remove_file(dir.join(format!("{name}-{renderer}.png")));
    }
    let shot = crate::testing::ui(size, app.view())
        .snapshot(&app.theme())
        .expect("headless render");
    assert!(shot.matches_image(dir.join(format!("{name}.png"))).unwrap());
}

// Not a check: an artifact generator. It renders the main view in several
// states to PNGs for a person to look at (`cargo test -p fastgraph --
// --ignored render_snapshots`, with `FASTGRAPH_SNAPSHOTS=<dir>`), and asserts
// only that each render succeeds. `window_renders` is the smoke test.
// Bugs caught: 0, False positives: 0
#[test]
#[ignore = "writes PNGs for inspection"]
fn render_snapshots() {
    let mut app = ready(None);
    app.capture_rate = Some(48_000);
    snapshot(&app, "empty");

    run(&mut app, Message::Measure);
    snapshot(&app, "measuring");
    run(&mut app, Message::Stop);

    for (side, offset, wobble) in [
        (Side::L, 0.0, 0.0),
        (Side::L, -1.5, 0.6),
        (Side::R, 1.0, -0.8),
    ] {
        run(&mut app, Message::SidePicked(side));
        run(
            &mut app,
            Message::Session(SessionEvent::Measured {
                response: headphone(offset, wobble),
                ir: ImpulseResponse {
                    rate: 48_000,
                    samples: vec![0.0, 1.0],
                    peak: 1,
                },
                sweep_seconds: 6.0,
                drift_ppm: 38.0,
                clipped: side == Side::R,
            }),
        );
    }
    run(
        &mut app,
        Message::Session(SessionEvent::Level {
            peak_dbfs: -14.0,
            clipped: false,
        }),
    );
    heard(&mut app, 0.0, false);
    let last = app.items.last().unwrap().m.meta.id.clone();
    run(&mut app, Message::Shown(last, false));
    run(&mut app, Message::Listen(true));
    run(
        &mut app,
        Message::Graph(graph::Event::Hover(graph::Hover::At {
            freq: 2900.0,
            db: 88.0,
        })),
    );
    snapshot(&app, "measurements");

    // The Phase view: a short delay unwraps, a long one wraps.
    for n in 0..app.items.len() {
        let shown = id(&app, n);
        run(&mut app, Message::Shown(shown, true));
    }
    for (delay, name) in [(50e-6, "phase-unwrapped"), (400e-6, "phase-wrapped")] {
        for (n, item) in app.items.iter_mut().enumerate() {
            let r = &mut item.m.response;
            let skew = n as f64 * 10.0;
            r.phase = Some(
                r.freq
                    .iter()
                    .map(|f| 60.0 / (1.0 + (f / 120.0).powi(2)) - 360.0 * f * delay - skew)
                    .collect(),
            );
        }
        run(&mut app, Message::GraphViewSelected(graph::View::Phase));
        snapshot(&app, name);
    }
    // Zoomed: Reset view above the plot, clear of a readout near its right
    // edge.
    let plot = crate::plot::PlotBox::inside(1000.0, 600.0, graph::INSETS);
    let wheel = |axis, at, lines| {
        Message::Graph(graph::Event::Wheel(graph::Wheel {
            axis,
            plot,
            at,
            lines,
        }))
    };
    let hover = |freq, db| Message::Graph(graph::Event::Hover(graph::Hover::At { freq, db }));
    run(
        &mut app,
        wheel(graph::Axis::Frequency, plot.right() - 100.0, 4.0),
    );
    run(&mut app, wheel(graph::Axis::Level, plot.top + 100.0, 2.0));
    run(&mut app, hover(12_000.0, 150.0));
    snapshot(&app, "phase-zoomed");
    // Distortion in %: at its default fit (every label whole), zoomed in
    // with a readout near the right edge, and zoomed fully out in a short
    // window.
    let first = id(&app, 0);
    app.distortion.insert(first.clone(), distortion_at(-60.0));
    if app.selected.as_deref() != Some(first.as_str()) {
        run(&mut app, Message::Select(first));
    }
    run(
        &mut app,
        Message::GraphViewSelected(graph::View::Distortion),
    );
    run(&mut app, hover(1000.0, -1.0));
    snapshot(&app, "distortion-pct");
    run(
        &mut app,
        wheel(graph::Axis::Frequency, plot.right() - 50.0, 3.0),
    );
    run(&mut app, wheel(graph::Axis::Level, plot.top + 300.0, 3.0));
    run(&mut app, hover(15_000.0, 0.5));
    snapshot(&app, "distortion-pct-zoomed");
    run(&mut app, wheel(graph::Axis::Level, plot.top + 300.0, -60.0));
    run(
        &mut app,
        Message::Graph(graph::Event::Hover(graph::Hover::Left)),
    );
    snapshot(&app, "distortion-pct-zoomed-out");
    snapshot_sized(&app, "distortion-pct-zoomed-out-short", (900.0, 400.0));
    run(&mut app, Message::Graph(graph::Event::Reset));
    run(&mut app, Message::GraphViewSelected(graph::View::Phase));
    run(&mut app, Message::Graph(graph::Event::Reset));
    run(&mut app, Message::GraphViewSelected(graph::View::Response));

    run(&mut app, Message::Listen(false));
    run(
        &mut app,
        Message::Session(SessionEvent::Level {
            peak_dbfs: -0.1,
            clipped: true,
        }),
    );
    run(
        &mut app,
        Message::Audio(AudioEvent::Error("connection reset".into())),
    );
    snapshot(&app, "offline");

    // RTA mode: a wobbly live average over the captured items.
    app.offline = false;
    run(&mut app, started());
    run(&mut app, Message::ModeSweep(false));
    run(&mut app, Message::SmoothingPicked(Smoothing::Oct6));
    run(&mut app, Message::RtaListen(true));
    let rta = |response, frames| Message::Session(SessionEvent::Rta { response, frames });
    run(&mut app, rta(headphone(-8.0, 1.2), 37));
    run(&mut app, Message::RtaCapture);
    run(&mut app, rta(headphone(-3.0, -2.0), 52));
    snapshot(&app, "rta");

    // The header (tabs, controls) at several widths, in two views: the
    // layout must not change between the views.
    run(&mut app, Message::ModeSweep(true));
    for (view, name) in [
        (graph::View::Response, "response"),
        (graph::View::Distortion, "distortion"),
    ] {
        run(&mut app, Message::GraphViewSelected(view));
        // No animation frames arrive headless: let the pill finish.
        app.now += Duration::from_secs(1);
        for width in [1900.0, 1280.0, 1000.0, 800.0] {
            snapshot_sized(&app, &format!("header-{name}-{width}"), (width, 800.0));
        }
    }

    // Every tab and graph view in both appearances.
    let saves = tempfile::tempdir().unwrap();
    for name in ["Velour pads", "Leather pads"] {
        write_pads(saves.path(), name, "Left");
    }
    run(
        &mut app,
        Message::SetsDirPicked(Some(saves.path().to_path_buf())),
    );
    for n in 0..app.items.len() {
        let item = id(&app, n);
        app.distortion
            .insert(item, distortion_at(-60.0 + 10.0 * n as f64));
    }
    // A selected row, and a hover card on each plot.
    let first = id(&app, 0);
    run(&mut app, Message::Select(first));
    for (mode, tag) in [(ThemeMode::Dark, "dark"), (ThemeMode::Light, "light")] {
        run(&mut app, Message::ThemePicked(mode));
        run(&mut app, Message::PaneSelected(Pane::Measure));
        for (view, name, hover) in [
            (graph::View::Response, "response", 4.0),
            (graph::View::Phase, "phase", -90.0),
            (graph::View::Distortion, "distortion", -1.0),
        ] {
            run(&mut app, Message::GraphViewSelected(view));
            run(
                &mut app,
                Message::Graph(graph::Event::Hover(graph::Hover::At {
                    freq: 2900.0,
                    db: hover,
                })),
            );
            app.now += Duration::from_secs(1);
            snapshot(&app, &format!("{tag}-measure-{name}"));
        }
        // Zoomed in %: Reset view, the finer grid and its labels in this
        // appearance's colours.
        run(
            &mut app,
            wheel(graph::Axis::Frequency, plot.right() - 50.0, 3.0),
        );
        run(&mut app, wheel(graph::Axis::Level, plot.top + 300.0, 3.0));
        run(&mut app, hover(15_000.0, 0.5));
        snapshot(&app, &format!("{tag}-measure-distortion-zoomed"));
        run(&mut app, Message::Graph(graph::Event::Reset));
        run(&mut app, Message::GraphViewSelected(graph::View::Response));
        for (pane, name) in [(Pane::Sets, "sets"), (Pane::Settings, "settings")] {
            run(&mut app, Message::PaneSelected(pane));
            app.now += Duration::from_secs(1);
            snapshot(&app, &format!("{tag}-{name}"));
        }
    }
}

// Bugs caught: 0, False positives: 1
#[test]
fn the_toolbar_has_the_same_groups_in_every_view() {
    for view in [
        graph::View::Response,
        graph::View::Phase,
        graph::View::Distortion,
    ] {
        let mut app = plain(None);
        run(&mut app, Message::GraphViewSelected(view));
        let (controls, actions) = crate::toolbar::view(&app);
        // One headless window at a time: each holds the GPU lock until dropped.
        {
            let mut ui = crate::testing::ui(crate::testing::SIZE, controls);
            for label in [
                "Smoothing",
                "Apply",
                "Invert",
                "Align",
                "Hz",
                "Units",
                "%",
                "dB",
            ] {
                assert!(ui.find(label).is_ok(), "{label} missing in {view:?}");
            }
        }
        let mut ui = crate::testing::ui(crate::testing::SIZE, actions);
        for label in ["Import…", "Average shown"] {
            assert!(ui.find(label).is_ok(), "{label} missing in {view:?}");
        }
    }
}

// Bugs caught: 0, False positives: 0
#[test]
fn a_long_comp_label_is_shortened_keeping_its_extension() {
    use crate::toolbar::shorten;
    assert_eq!(shorten("Comp: none", 28), "Comp: none");
    let long = shorten("Comp: Curve-CouplerCompensation.txt", 28);
    assert_eq!(long.chars().count(), 28);
    assert!(long.ends_with("….txt"), "{long}");
    assert_eq!(shorten("ééééééé", 4), "ééé…");
}

// An audio interface input that its split profile hides exists only as a
// channel (here AUX1) of the raw hardware node.
// Bugs caught: 0, False positives: 0
#[test]
fn a_raw_interface_channel_can_be_chosen() {
    let interface = Node {
        id: 81,
        serial: "14666".into(),
        name: "alsa_input.hw_interface_0".into(),
        description: "USB Interface".into(),
        is_sink: false,
        raw: true,
        channels: vec!["AUX0".into(), "AUX1".into(), "AUX2".into(), "AUX3".into()],
    };
    let mut app = App::new_for_test(Prefs::default(), None);
    run(
        &mut app,
        Message::Audio(AudioEvent::Nodes(vec![mic(), interface.clone(), phones()])),
    );
    let labels: Vec<String> = app.inputs().iter().map(|i| i.label()).collect();
    assert!(
        labels.contains(&"USB Interface (all channels)".to_string()),
        "{labels:?}"
    );
    run(&mut app, Message::InputPicked(interface.name.clone()));
    run(&mut app, Message::InputChannelPicked("AUX1".into()));
    let (input, channel) = app.selected_input().expect("raw input selected");
    assert_eq!(input.node().name, "alsa_input.hw_interface_0");
    assert_eq!(channel, 1);
    assert_eq!(app.prefs.input_channel.as_deref(), Some("AUX1"));
}

// Bugs caught: 0, False positives: 0
#[test]
fn closing_the_window_writes_the_pending_recovery_first() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = ready(data_in(&dir));
    // Hold the worker so that everything below is still pending.
    let (open, gate) = std::sync::mpsc::channel::<()>();
    app.worker.submit(Box::new(move || {
        let _ = gate.recv();
        None
    }));
    app.state = Activity::Listening;
    let _ = app.update(measured(0.0, false));
    let _ = app.update(measured(1.0, false));
    app.state = Activity::Idle;
    let first = app.items[0].m.meta.id.clone();
    let second = app.items[1].m.meta.id.clone();
    let _ = app.update(Message::Shown(first.clone(), false));
    let _ = app.update(Message::RenameStart(first.clone()));
    let _ = app.update(Message::RenameInput("Left cup".into()));
    let _ = app.update(Message::RenameCommit);
    let _ = app.update(Message::Delete(second.clone()));
    let _ = app.update(Message::Delete(second.clone()));
    assert_eq!(app.items.len(), 1);
    let path = app.recovery.clone().unwrap();
    assert!(!path.exists(), "all still pending");
    open.send(()).unwrap();
    // Closing the window finishes the worker first.
    let _ = app.update(Message::CloseRequested);
    let (_, stored, problems) = set::read_set(&path).unwrap();
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(stored.len(), 1);
    let meta = &stored[0].meta;
    assert_eq!(meta.id, first);
    assert_eq!(meta.name, "Left cup");
    assert!(!meta.shown);
    let results = worker_results(&app);
    assert!(results.is_empty(), "{results:?}");
}

// Bugs caught: 0, False positives: 0
#[test]
fn a_result_after_stop_is_dropped_and_each_run_starts_afresh() {
    let mut app = ready(None);
    let reset = Sent::Session(SessionCommand::Reset);
    app.sent.borrow_mut().clear();
    run(&mut app, Message::Measure);
    assert!(app.sent.borrow().contains(&reset));
    run(&mut app, Message::Stop);
    // Deconvolved before the stop, delivered after it.
    run(&mut app, measured(0.0, false));
    assert!(app.items.is_empty());
    assert_eq!(app.status.text, "Measurement stopped");
    app.sent.borrow_mut().clear();
    run(&mut app, Message::Listen(true));
    assert!(app.sent.borrow().contains(&reset));
    // A new input stream, even at the same rate, starts the analysis over.
    app.sent.borrow_mut().clear();
    run(&mut app, started());
    assert_eq!(
        *app.sent.borrow(),
        [Sent::Session(SessionCommand::Rate(48_000)), reset]
    );
}

// Bugs caught: 0, False positives: 0
#[test]
fn measurements_keep_the_devices_they_were_taken_with() {
    let other = node(60, "alsa_output.speakers", "Speakers", true);
    let mut app = ready(None);
    run(
        &mut app,
        Message::Audio(AudioEvent::Nodes(vec![mic(), phones(), other.clone()])),
    );
    // The sweep played on the headphones, whatever is picked meanwhile.
    run(&mut app, Message::Measure);
    run(&mut app, Message::OutputPicked(other.name.clone()));
    run(&mut app, measured(0.0, false));
    assert_eq!(app.items[0].m.meta.output, Some(phones().name));
    assert_eq!(app.items[0].m.meta.input, Some(mic().name));
    // Listen records no output: the sweep played elsewhere.
    run(&mut app, Message::Listen(true));
    run(&mut app, measured(0.0, false));
    assert_eq!(app.items[1].m.meta.output, None);
    run(&mut app, Message::Listen(false));
    // RTA keeps the devices of the average it shows, through later picks.
    run(&mut app, Message::OutputPicked(phones().name));
    run(&mut app, Message::ModeSweep(false));
    run(&mut app, Message::RtaNoise(true));
    run(&mut app, rta_event(-30.0, 8));
    run(&mut app, Message::RtaNoise(false));
    run(&mut app, Message::OutputPicked(other.name.clone()));
    run(
        &mut app,
        Message::InputPicked("alsa_input.elsewhere".into()),
    );
    run(&mut app, Message::RtaCapture);
    let meta = &app.items[2].m.meta;
    assert_eq!(meta.kind, Kind::Rta);
    assert_eq!(meta.output, Some(phones().name));
    assert_eq!(meta.input, Some(mic().name));
    // Noise played elsewhere: no output.
    run(&mut app, Message::InputPicked(mic().name));
    run(&mut app, started());
    run(&mut app, Message::RtaListen(true));
    run(&mut app, rta_event(-30.0, 8));
    run(&mut app, Message::RtaCapture);
    assert_eq!(app.items[3].m.meta.output, None);
    assert_eq!(app.items[3].m.meta.input, Some(mic().name));
}

// Bugs caught: 0, False positives: 0
#[test]
fn local_noise_is_averaged_from_when_it_streams() {
    let mut app = rta_ready(None);
    let session = |app: &App| -> Vec<Sent> {
        let sent: Vec<Sent> = app
            .sent
            .borrow()
            .iter()
            .filter(|s| matches!(s, Sent::Session(_)))
            .cloned()
            .collect();
        app.sent.borrow_mut().clear();
        sent
    };
    let averaging = [
        Sent::Session(SessionCommand::SetMode(Mode::Rta)),
        Sent::Session(SessionCommand::Reset),
    ];
    let play_started = || Message::Audio(AudioEvent::PlayStarted);
    // A start with no noise waiting is stale.
    run(&mut app, play_started());
    session(&app);
    run(&mut app, Message::RtaNoise(true));
    assert!(app.rta.started, "the noise was sent");
    assert_eq!(session(&app), [], "nothing is averaged before it plays");
    run(&mut app, play_started());
    assert_eq!(session(&app), averaging);
    run(&mut app, play_started());
    assert_eq!(session(&app), [], "already averaging");
    // A new output path starts the average over once the noise plays there.
    run(&mut app, Message::SidePicked(Side::R));
    session(&app);
    run(&mut app, play_started());
    assert_eq!(session(&app), averaging);
    // Noise played elsewhere is averaged at once.
    run(&mut app, Message::RtaNoise(false));
    session(&app);
    run(&mut app, Message::RtaListen(true));
    assert_eq!(
        session(&app),
        [Sent::Session(SessionCommand::SetMode(Mode::Rta))]
    );
    run(&mut app, play_started());
    assert_eq!(session(&app), []);
}

// Bugs caught: 0, False positives: 0
#[test]
fn without_recovery_the_device_pane_says_so() {
    let warning = "Unsaved work can't be recovered after a crash: no permission";
    let mut app = ready(None);
    app.recovery_error = Some("no permission".into());
    assert!(crate::testing::shows(app.view(), warning));
    // It stays after other news.
    heard(&mut app, 0.0, false);
    assert!(crate::testing::shows(app.view(), warning));
    let dir = tempfile::tempdir().unwrap();
    let mut app = ready(data_in(&dir));
    app.recovery_error = Some("no permission".into());
    assert!(!crate::testing::shows(app.view(), warning));
}

// Bugs caught: 0, False positives: 0
#[test]
fn a_deleted_seat_number_is_not_reused() {
    let mut app = ready(None);
    for db in [0.0, 1.0, 2.0] {
        heard(&mut app, db, false);
    }
    let second = app.items[1].m.meta.id.clone();
    delete(&mut app, &second);
    heard(&mut app, 3.0, false);
    assert_eq!(names(&app), ["L seat 1", "L seat 3", "L seat 4"]);
    assert_eq!(app.items[2].m.meta.seating, Some(4));
    // A renamed one no longer holds its number.
    let last = app.items[2].m.meta.id.clone();
    run(&mut app, Message::RenameStart(last));
    run(&mut app, Message::RenameInput("Left cup".into()));
    run(&mut app, Message::RenameCommit);
    heard(&mut app, 4.0, false);
    assert_eq!(app.items[3].m.meta.name, "L seat 4");
    // RTA captures count their own names.
    run(&mut app, Message::ModeSweep(false));
    run(&mut app, Message::RtaListen(true));
    run(&mut app, rta_event(-30.0, 8));
    for _ in 0..3 {
        run(&mut app, Message::RtaCapture);
    }
    let rta2 = app.items[5].m.meta.id.clone();
    delete(&mut app, &rta2);
    run(&mut app, Message::RtaCapture);
    assert_eq!(
        names(&app)[4..],
        ["L RTA 1", "L RTA 3", "L RTA 4"],
        "{:?}",
        names(&app)
    );
}

// Bugs caught: 0, False positives: 0
#[test]
fn a_finished_boot_reload_leaves_later_failures_alone() {
    let prefs = Prefs {
        compensation: Some("/x/coupler.txt".into()),
        ..Prefs::default()
    };
    let mut app = App::new_for_test(prefs, None);
    app.status = Status {
        text: "Measurements can't be stored".into(),
        level: StatusLevel::Error,
    };
    let _ = app.reload_compensation();
    run(&mut app, Message::CompensationLoaded(Ok(comp_curve())));
    assert!(!app.comp_boot_reload);
    run(
        &mut app,
        Message::CompensationLoaded(Err("other.txt: gone".into())),
    );
    assert_eq!(app.status.level, StatusLevel::Warn);
    assert!(
        !app.status.text.contains("can't be stored"),
        "{}",
        app.status.text
    );
}

// Bugs caught: 0, False positives: 0
#[test]
fn an_all_hidden_graph_says_how_to_show_one() {
    let mut app = ready(None);
    heard(&mut app, 0.0, false);
    let id = app.items[0].m.meta.id.clone();
    run(&mut app, Message::Shown(id, false));
    assert!(crate::testing::shows(app.view(), graph::ALL_HIDDEN));
    assert!(!crate::testing::shows(app.view(), graph::EMPTY));
}

// The Phase tab draws the shown traces that have phase and says how many it
// left out.
// Bugs caught: 0, False positives: 0
#[test]
fn phase_tab_draws_only_traces_with_phase() {
    let mut app = ready(None);
    let freq = log_grid(48_000);
    app.state = Activity::Listening;
    run(
        &mut app,
        Message::Session(SessionEvent::Measured {
            response: Response {
                db: vec![0.0; freq.len()],
                phase: Some(vec![30.0; freq.len()]),
                freq,
            },
            ir: ImpulseResponse {
                rate: 48_000,
                samples: vec![0.0, 1.0, 0.5, 0.0],
                peak: 1,
            },
            sweep_seconds: 6.0,
            drift_ppm: 0.0,
            clipped: false,
        }),
    );
    app.state = Activity::Idle;
    run(
        &mut app,
        Message::Imported(0, Ok(vec![(PathBuf::from("/in/Flat.txt"), flat(1.0))])),
    );
    assert_eq!(app.items.len(), 2);
    assert_eq!(app.graph.phase_count(), 0);
    run(&mut app, Message::GraphViewSelected(graph::View::Phase));
    assert_eq!(app.graph.phase_count(), 1);
    assert!(crate::testing::shows(
        app.view(),
        "1 shown trace has no phase"
    ));
    run(&mut app, Message::GraphViewSelected(graph::View::Response));
    assert_eq!(app.graph.phase_count(), 0);
    assert!(!crate::testing::shows(
        app.view(),
        "1 shown trace has no phase"
    ));
}

// --- the session, its recovery file and sets ----------------------------------

/// A measurement's metadata for a set written by a test.
fn meta(id: &str, name: &str) -> fastgraph_core::store::Meta {
    fastgraph_core::store::Meta {
        id: id.into(),
        name: name.into(),
        kind: Kind::Imported,
        side: None,
        seating: None,
        input: None,
        output: None,
        drift_ppm: None,
        clipped: false,
        created: 1,
        source: None,
        sources: Vec::new(),
        compensable: false,
        baked_compensation: None,
        shown: true,
        offset_db: 0.0,
        ir_peak: None,
        sweep_seconds: None,
    }
}

/// What the app's recovery file holds once the worker is idle.
fn recovered(app: &App) -> Vec<set::Entry> {
    app.worker.flush();
    let path = app.recovery.as_ref().expect("a recovery file");
    let (_, entries, problems) = set::read_set(path).unwrap();
    assert!(problems.is_empty(), "{problems:?}");
    entries
}

/// A new app with `app`'s recovery file opened.
fn reopened(app: &App) -> App {
    app.worker.flush();
    let mut again = App::new_for_test(Prefs::default(), None);
    run(&mut again, Message::OpenSet(app.recovery.clone().unwrap()));
    again
}

/// Write an old per-file store measurement, as earlier versions did.
fn old_measurement(dir: &std::path::Path, id: &str, name: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let header = fastgraph_core::curve_io::ExportHeader {
        name: name.into(),
        smoothing: Smoothing::None,
        compensation: None,
        created: std::time::UNIX_EPOCH,
        offset_db: 0.0,
    };
    let text = fastgraph_core::curve_io::write_rew(&flat(0.0), &header);
    std::fs::write(dir.join(format!("{id}.txt")), text).unwrap();
    let meta = format!(
        "id = \"{id}\"\nname = \"{name}\"\nkind = \"Sweep\"\nclipped = false\ncreated = 1\nsources = []\n"
    );
    std::fs::write(dir.join(format!("{id}.toml")), meta).unwrap();
}

// Bugs caught: 0, False positives: 0
#[test]
fn nothing_loads_on_start() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    let recovery = data.join("recovery");
    std::fs::create_dir_all(&recovery).unwrap();
    let earlier = recovery.join("2026-01-01 000000.fgset");
    set::write_set(
        &earlier,
        "Last time",
        &[set::Entry {
            meta: meta("1-0", "Kept"),
            response: flat(0.0),
            ir: None,
        }],
    )
    .unwrap();
    old_measurement(&data.join("measurements"), "2-0", "Old one");
    let app = ready(Some(data.clone()));
    assert!(app.items.is_empty(), "{:?}", names(&app));
    assert!(app.set_name.is_empty());
    assert!(!app.dirty);
    // The old store was packed away, and the last session's file is kept.
    assert!(recovery.join("Earlier measurements.fgset").exists());
    assert!(data.join("measurements.migrated").is_dir());
    assert!(earlier.exists());
    assert_eq!(app.status.level, StatusLevel::Info);
    assert!(
        app.status
            .text
            .contains("Earlier measurements are in Sets › Unsaved sessions"),
        "{}",
        app.status.text
    );
}

// Bugs caught: 0, False positives: 0
#[test]
fn recovery_holds_the_latest_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = plain(Some(dir.path().to_path_buf()));
    heard(&mut app, 0.0, false);
    heard(&mut app, 1.0, false);
    let (first, second) = (id(&app, 0), id(&app, 1));
    run(&mut app, Message::RenameStart(first.clone()));
    run(&mut app, Message::RenameInput("Left cup".into()));
    run(&mut app, Message::RenameCommit);
    delete(&mut app, &second);
    assert!(app.dirty);
    let entries = recovered(&app);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].meta.id, first);
    assert_eq!(entries[0].meta.name, "Left cup");
    assert!(
        entries[0].ir.is_some(),
        "a sweep keeps its impulse response"
    );
}

// Bugs caught: 0, False positives: 0
#[test]
fn an_emptied_session_removes_its_recovery_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = plain(Some(dir.path().to_path_buf()));
    heard(&mut app, 0.0, false);
    assert_eq!(recovered(&app).len(), 1);
    let path = app.recovery.clone().unwrap();
    assert!(path.exists());
    let only = id(&app, 0);
    delete(&mut app, &only);
    app.worker.flush();
    assert!(!path.exists());
}

// Bugs caught: 0, False positives: 1
#[test]
fn open_replaces_and_import_adds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Pads.fgset");
    let entries = [
        set::Entry {
            meta: meta("a-0", "Velour"),
            response: flat(1.0),
            ir: None,
        },
        set::Entry {
            meta: meta("b-0", "Leather"),
            response: flat(2.0),
            ir: None,
        },
    ];
    set::write_set(&path, "Pads", &entries).unwrap();
    // A recovery file keeps the session Open replaces.
    let data = tempfile::tempdir().unwrap();
    let mut app = plain(data_in(&data));
    heard(&mut app, 0.0, false);
    run(&mut app, Message::OpenSet(path.clone()));
    assert_eq!(names(&app), ["Velour", "Leather"]);
    assert_eq!(app.set_name, "Pads");
    assert_eq!(app.saved_as.as_deref(), Some(path.as_path()));
    assert!(!app.dirty);
    let colours: Vec<_> = app.items.iter().map(|i| i.color).collect();
    assert_eq!(colours, [0, 1]);
    run(&mut app, Message::ImportSet(path.clone()));
    assert_eq!(names(&app), ["Velour", "Leather", "Velour", "Leather"]);
    let ids: std::collections::HashSet<_> = app.items.iter().map(|i| &i.m.meta.id).collect();
    assert_eq!(ids.len(), 4, "{ids:?}");
    assert_eq!(app.set_name, "Pads");
    assert!(app.dirty);
    // A set that can't be read changes nothing.
    run(&mut app, Message::OpenSet(dir.path().join("gone.fgset")));
    assert_eq!(app.items.len(), 4);
    assert_eq!(app.status.level, StatusLevel::Error);
}

// --- saving a set ------------------------------------------------------------

/// A plain app whose sets folder is `saves`, with one measurement.
fn saver(data: &tempfile::TempDir, saves: &std::path::Path) -> App {
    let mut app = plain(data_in(data));
    app.prefs.sets_dir = Some(saves.to_path_buf());
    heard(&mut app, 3.0, false);
    app
}

// Bugs caught: 0, False positives: 0
#[test]
fn save_writes_without_a_dialog_when_ask_is_off() {
    let data = tempfile::tempdir().unwrap();
    let saves = tempfile::tempdir().unwrap();
    // The folder is created when it is missing.
    let folder = saves.path().join("Sets");
    let mut app = saver(&data, &folder);
    app.prefs.ask_save = false;
    run(&mut app, Message::SetNameInput("HD600".into()));
    let recovery = app.recovery.clone().unwrap();
    app.worker.flush();
    assert!(recovery.exists());
    run(&mut app, Message::SaveSet);
    let path = folder.join("HD600.fgset");
    let (header, entries, problems) = set::read_set(&path).unwrap();
    assert!(problems.is_empty());
    assert_eq!(header.name, "HD600");
    assert_eq!(entries.len(), 1);
    assert!(entries[0].ir.is_some());
    assert_eq!(app.saved_as.as_deref(), Some(path.as_path()));
    assert!(!app.dirty);
    assert!(!recovery.exists(), "the recovery file goes after a save");
    // The next change brings it back.
    heard(&mut app, 4.0, false);
    assert!(app.dirty);
    assert_eq!(recovered(&app).len(), 2);
}

// Bugs caught: 1, False positives: 0
#[test]
fn replacing_another_set_acts_on_the_first_click() {
    let data = tempfile::tempdir().unwrap();
    let saves = tempfile::tempdir().unwrap();
    let other = saves.path().join("Other.fgset");
    set::write_set(&other, "Other", &[]).unwrap();
    let mut app = saver(&data, saves.path());
    run(&mut app, Message::SetNameInput("Other".into()));
    run(&mut app, Message::SaveSet);
    assert_eq!(set::read_set(&other).unwrap().1.len(), 1);
    assert!(!crate::testing::shows(app.view(), "Replace Other?"));
    assert_eq!(app.saved_as.as_deref(), Some(other.as_path()));
    // Saving again over the open set is silent.
    heard(&mut app, 4.0, false);
    run(&mut app, Message::SaveSet);
    assert_eq!(set::read_set(&other).unwrap().1.len(), 2);
}

// Bugs caught: 0, False positives: 0
#[test]
fn an_empty_name_is_refused() {
    let data = tempfile::tempdir().unwrap();
    let saves = tempfile::tempdir().unwrap();
    let mut app = saver(&data, saves.path());
    run(&mut app, Message::SetNameInput("  ".into()));
    run(&mut app, Message::SaveSet);
    assert_eq!(app.status.level, StatusLevel::Error);
    assert_eq!(std::fs::read_dir(saves.path()).unwrap().count(), 0);
    assert!(app.saved_as.is_none());
}

// Bugs caught: 0, False positives: 0
#[test]
fn a_recovery_file_is_never_the_open_set() {
    let data = tempfile::tempdir().unwrap();
    let saves = tempfile::tempdir().unwrap();
    let mut first = saver(&data, saves.path());
    run(&mut first, Message::SetNameInput("Crashed".into()));
    first.worker.flush();
    let crashed = first.recovery.clone().unwrap();
    // Another run of the app, which opens the previous run's file.
    let mut app = plain(data_in(&data));
    app.prefs.sets_dir = Some(saves.path().to_path_buf());
    run(&mut app, Message::OpenSet(crashed.clone()));
    assert_eq!(app.set_name, "Crashed");
    assert!(app.saved_as.is_none());
    // Saving writes to the sets folder, not into the recovery folder.
    run(&mut app, Message::SaveSet);
    assert!(saves.path().join("Crashed.fgset").exists());
    let recovery_dir = crashed.parent().unwrap();
    assert!(!recovery_dir.join("Crashed.fgset").exists());
}

// Bugs caught: 0, False positives: 0
#[test]
fn a_dialog_path_always_ends_in_fgset() {
    let data = tempfile::tempdir().unwrap();
    let saves = tempfile::tempdir().unwrap();
    let mut app = saver(&data, saves.path());
    run(
        &mut app,
        Message::SaveSetTo(Some(saves.path().join("HD600.v2"))),
    );
    assert!(saves.path().join("HD600.v2.fgset").exists());
    run(
        &mut app,
        Message::SaveSetTo(Some(saves.path().join("Plain.fgset"))),
    );
    assert!(saves.path().join("Plain.fgset").exists());
    assert!(!saves.path().join("Plain.fgset.fgset").exists());
}

// Bugs caught: 0, False positives: 0
#[test]
fn a_dialog_path_in_the_recovery_folder_is_refused() {
    let data = tempfile::tempdir().unwrap();
    let saves = tempfile::tempdir().unwrap();
    let mut app = saver(&data, saves.path());
    let inside = app.recovery.clone().unwrap().parent().unwrap().join("X");
    run(&mut app, Message::SaveSetTo(Some(inside.clone())));
    assert_eq!(app.status.level, StatusLevel::Error);
    assert!(!inside.with_extension("fgset").exists());
    assert!(app.saved_as.is_none());
}

// Bugs caught: 0, False positives: 0
#[test]
fn a_typed_extension_is_not_doubled() {
    let data = tempfile::tempdir().unwrap();
    let saves = tempfile::tempdir().unwrap();
    let mut app = saver(&data, saves.path());
    run(&mut app, Message::SetNameInput("a.FGSET".into()));
    run(&mut app, Message::SaveSet);
    assert!(saves.path().join("a.fgset").exists());
    assert_eq!(std::fs::read_dir(saves.path()).unwrap().count(), 1);
}

// Bugs caught: 0, False positives: 0
#[test]
fn a_second_save_click_while_one_runs_does_nothing() {
    let data = tempfile::tempdir().unwrap();
    let saves = tempfile::tempdir().unwrap();
    let mut app = saver(&data, saves.path());
    run(&mut app, Message::SetNameInput("Twice".into()));
    // Both clicks arrive before the worker answers.
    let path = saves.path().join("Twice.fgset");
    drop(app.update(Message::SaveChecked(path, false)));
    assert!(app.busy);
    // The click starts no check of the file.
    assert_eq!(app.update(Message::SaveSet).units(), 0);
    run(&mut app, Message::Tick);
    assert!(!app.busy);
}

// --- the Sets browser --------------------------------------------------------

/// A set of one measurement named `entry` written to `dir/<name>.fgset`.
fn write_pads(dir: &std::path::Path, name: &str, entry: &str) -> PathBuf {
    let path = dir.join(format!("{name}.fgset"));
    let entries = [set::Entry {
        meta: meta("a-0", entry),
        response: flat(1.0),
        ir: None,
    }];
    set::write_set(&path, name, &entries).unwrap();
    path
}

/// An app whose sets folder is `saves`, showing the Sets tab.
fn browsing(data: Option<PathBuf>, saves: &std::path::Path) -> App {
    let mut app = plain(data);
    app.prefs.sets_dir = Some(saves.to_path_buf());
    run(&mut app, Message::PaneSelected(Pane::Sets));
    app
}

/// The messages a click on the text `label` in `view` sends.
fn click(view: Element<'_, Message>, label: &str) -> Vec<Message> {
    let mut ui = crate::testing::ui(crate::testing::SIZE, view);
    ui.click(label).unwrap();
    ui.messages()
}

// Bugs caught: 0, False positives: 0
#[test]
fn sets_tab_lists_newest_first() {
    let saves = tempfile::tempdir().unwrap();
    write_pads(saves.path(), "Aaa", "x");
    write_pads(saves.path(), "Bbb", "y");
    let mut app = browsing(None, saves.path());
    // Selecting the tab listed the folder on the worker.
    assert_eq!(app.sets.len(), 2);
    let mut sets = app.sets.clone();
    sets.reverse();
    // The rows keep the order of the list.
    run(
        &mut app,
        Message::SetsListed(sets.clone(), Vec::new(), None),
    );
    let y = |app: &App, label: &str| {
        let mut ui = crate::testing::ui(crate::testing::SIZE, app.view());
        ui.find(label).unwrap().bounds().y
    };
    let first = sets[0].1.name.clone();
    let second = sets[1].1.name.clone();
    assert!(y(&app, &first) < y(&app, &second));
    sets.reverse();
    run(&mut app, Message::SetsListed(sets, Vec::new(), None));
    assert!(y(&app, &second) < y(&app, &first));
}

// Bugs caught: 0, False positives: 0
#[test]
fn an_empty_sets_folder_says_so() {
    let saves = tempfile::tempdir().unwrap();
    let app = browsing(None, saves.path());
    let said = format!("No sets in {} yet", saves.path().display());
    assert!(crate::testing::shows(app.view(), &said));
}

// Bugs caught: 0, False positives: 1
#[test]
fn clicking_a_set_opens_it() {
    let saves = tempfile::tempdir().unwrap();
    let path = write_pads(saves.path(), "Pads", "Velour");
    let data = tempfile::tempdir().unwrap();
    let mut app = browsing(data_in(&data), saves.path());
    heard(&mut app, 0.0, false);
    let sent = click(app.view(), "Pads");
    assert!(
        matches!(sent.as_slice(), [Message::OpenSet(p)] if *p == path),
        "{sent:?}"
    );
    for message in sent {
        run(&mut app, message);
    }
    assert_eq!(names(&app), ["Velour"]);
    assert_eq!(app.set_name, "Pads");
}

// Bugs caught: 0, False positives: 0
#[test]
fn import_button_adds() {
    let saves = tempfile::tempdir().unwrap();
    let path = write_pads(saves.path(), "Pads", "Velour");
    let mut app = browsing(None, saves.path());
    heard(&mut app, 0.0, false);
    let sent = click(crate::browser::view(&app), "Import");
    assert!(
        matches!(sent.as_slice(), [Message::ImportSet(p)] if *p == path),
        "{sent:?}"
    );
    for message in sent {
        run(&mut app, message);
    }
    assert_eq!(app.items.len(), 2);
    assert!(names(&app).contains(&"Velour"));
}

// Bugs caught: 0, False positives: 0
#[test]
fn unsaved_sessions_are_listed() {
    let data = tempfile::tempdir().unwrap();
    let saves = tempfile::tempdir().unwrap();
    let recovery = data.path().join("recovery");
    std::fs::create_dir_all(&recovery).unwrap();
    // An earlier run's file, which has no name.
    let old = recovery.join("2000-01-01 000000.fgset");
    set::write_set(
        &old,
        "",
        &[set::Entry {
            meta: meta("b-0", "Old"),
            response: flat(1.0),
            ir: None,
        }],
    )
    .unwrap();
    let mut app = browsing(data_in(&data), saves.path());
    // This run has a recovery file too, which is the live session.
    heard(&mut app, 0.0, false);
    app.worker.flush();
    assert!(app.recovery.as_ref().unwrap().exists());
    run(&mut app, Message::PaneSelected(Pane::Sets));
    assert_eq!(app.unsaved.len(), 1, "{:?}", app.unsaved);
    assert_eq!(app.unsaved[0].0, old);
    let label = format!(
        "Unsaved · {}",
        crate::browser::saved_label(app.unsaved[0].1.saved)
    );
    assert!(crate::testing::shows(app.view(), "Unsaved sessions"));
    let sent = click(app.view(), &label);
    assert!(
        matches!(sent.as_slice(), [Message::OpenSet(p)] if *p == old),
        "{sent:?}"
    );
}

// Bugs caught: 0, False positives: 0
#[test]
fn opening_over_a_session_keeps_its_recovery_file() {
    let data = tempfile::tempdir().unwrap();
    let saves = tempfile::tempdir().unwrap();
    let path = write_pads(saves.path(), "Pads", "Velour");
    let mut app = browsing(data_in(&data), saves.path());
    heard(&mut app, 0.0, false);
    let old = app.recovery.clone().unwrap();
    assert_eq!(recovered(&app).len(), 1);
    run(&mut app, Message::OpenSet(path));
    let new = app.recovery.clone().unwrap();
    assert_ne!(new, old);
    // The next change writes the new file only.
    heard(&mut app, 2.0, false);
    assert_eq!(recovered(&app).len(), 2);
    let (_, kept, _) = set::read_set(&old).unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].meta.kind, Kind::Sweep);
    // The old file is listed as unsaved, the live one is not.
    assert_eq!(app.unsaved.len(), 1, "{:?}", app.unsaved);
    assert_eq!(app.unsaved[0].0, old);
    // An empty session leaves nothing to keep: no rotation.
    let mut empty = browsing(data_in(&data), saves.path());
    let before = empty.recovery.clone();
    run(
        &mut empty,
        Message::OpenSet(saves.path().join("Pads.fgset")),
    );
    assert_eq!(empty.recovery, before);
}

// Bugs caught: 0, False positives: 0
#[test]
fn opening_while_a_save_runs_is_ignored() {
    let data = tempfile::tempdir().unwrap();
    let saves = tempfile::tempdir().unwrap();
    let other = write_pads(saves.path(), "Pads", "Velour");
    let mut app = saver(&data, saves.path());
    run(&mut app, Message::SetNameInput("Mine".into()));
    drop(app.update(Message::SaveChecked(saves.path().join("Mine.fgset"), false)));
    assert!(app.busy);
    let before = names(&app)
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>();
    assert_eq!(app.update(Message::OpenSet(other)).units(), 0);
    assert_eq!(
        app.update(Message::ImportSet(saves.path().join("x.fgset")))
            .units(),
        0
    );
    assert_eq!(names(&app), before);
    assert_eq!(app.set_name, "Mine");
    assert!(app.status.text.contains("Wait for the current save"));
}

// Bugs caught: 0, False positives: 0
#[test]
fn accent_hex_changes_the_theme() {
    let mut app = plain(None);
    let before = theme::Palette::of(&app.theme()).accent;
    run(&mut app, Message::AccentInput("#30a0f0".into()));
    // Typing alone applies nothing.
    assert_eq!(app.prefs.accent, None);
    run(&mut app, Message::AccentSubmit);
    let accent = theme::Palette::of(&app.theme()).accent;
    assert_eq!(Some(accent), theme::parse_hex("#30a0f0"));
    assert_eq!(app.prefs.accent.as_deref(), Some("#30a0f0"));
    // Bad text stays in the box, is not applied, and the theme is unchanged.
    run(&mut app, Message::AccentInput("#zz".into()));
    run(&mut app, Message::AccentSubmit);
    assert_eq!(app.settings.accent, "#zz");
    assert_eq!(theme::Palette::of(&app.theme()).accent, accent);
    run(&mut app, Message::AccentReset);
    assert_eq!(theme::Palette::of(&app.theme()).accent, before);
    assert_eq!(app.prefs.accent, None);
}

// Bugs caught: 0, False positives: 0
#[test]
fn trace_colours_apply() {
    let mut app = plain(None);
    run(&mut app, Message::TraceColorInput(0, "00ff00".into()));
    run(&mut app, Message::TraceColorSubmit(0));
    let green = theme::parse_hex("#00ff00").unwrap();
    assert_eq!(theme::Palette::of(&app.theme()).trace(0), green);
    // The other seven keep their colours, and the accent is untouched.
    let default = theme::Palette::default();
    assert_eq!(theme::Palette::of(&app.theme()).trace(3), default.trace(3));
    assert_eq!(theme::Palette::of(&app.theme()).accent, default.accent);
    run(&mut app, Message::TraceColorsReset);
    assert_eq!(theme::Palette::of(&app.theme()).trace(0), default.accent);
    assert_eq!(app.prefs.trace_colors, None);
}

// Bugs caught: 0, False positives: 0
#[test]
fn export_without_ask_names_set_and_trace() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("new").join("exports");
    let mut app = plain(None);
    app.prefs.exports_dir = Some(out.clone());
    app.prefs.ask_export = false;
    heard(&mut app, 0.0, false);
    app.set_name = "HD600".into();
    let id = app.items[0].m.meta.id.clone();
    run(&mut app, Message::Export(id.clone()));
    let file = out.join("HD600 - L seat 1.txt");
    assert!(file.is_file(), "{:?}", app.status.text);
    assert!(app.status.text.contains("Exported"), "{}", app.status.text);
    assert!(app.status.text.contains("HD600 - L seat 1.txt"));
    // Without a set name the file is named after the trace; one that exists
    // is replaced.
    app.set_name = "  ".into();
    run(&mut app, Message::Export(id));
    assert!(out.join("L seat 1.txt").is_file());
}

// Bugs caught: 0, False positives: 0
#[test]
fn parse_hex_round_trips() {
    for text in ["#e9a2a1", "#000000", "#ffffff", "#30a0f0"] {
        assert_eq!(theme::to_hex(theme::parse_hex(text).unwrap()), text);
    }
    assert_eq!(theme::parse_hex("E9A2A1"), theme::parse_hex("#e9a2a1"));
    for bad in ["", "#", "#fff", "#gggggg", "e9a2a", "#e9a2a1f", " #e9a2a1"] {
        assert_eq!(theme::parse_hex(bad), None, "{bad:?}");
    }
}

// Bugs caught: 0, False positives: 0
#[test]
fn changing_the_sets_folder_relists() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    write_pads(second.path(), "Elsewhere", "x");
    let mut app = browsing(None, first.path());
    assert!(app.sets.is_empty());
    run(
        &mut app,
        Message::SetsDirInput(second.path().display().to_string()),
    );
    // Typing alone applies nothing.
    assert_eq!(app.prefs.sets_dir.as_deref(), Some(first.path()));
    run(&mut app, Message::SetsDirSubmit);
    assert_eq!(app.prefs.sets_dir.as_deref(), Some(second.path()));
    assert_eq!(app.sets.len(), 1);
    assert_eq!(app.sets_folder, Some(Some(second.path().to_path_buf())));
    // An empty box is the default.
    run(&mut app, Message::SetsDirInput(String::new()));
    run(&mut app, Message::SetsDirSubmit);
    assert_eq!(app.prefs.sets_dir, None);
}

// Bugs caught: 0, False positives: 0
#[test]
fn folder_boxes_expand_home_and_refuse_relative_paths() {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return;
    };
    let first = tempfile::tempdir().unwrap();
    let mut app = browsing(None, first.path());
    run(&mut app, Message::ExportsDirInput("~/fg-exports".into()));
    run(&mut app, Message::ExportsDirSubmit);
    assert_eq!(app.prefs.exports_dir, Some(home.join("fg-exports")));
    // A relative path is invalid: not applied.
    run(&mut app, Message::ExportsDirInput("exports".into()));
    run(&mut app, Message::ExportsDirSubmit);
    assert_eq!(app.prefs.exports_dir, Some(home.join("fg-exports")));
    run(&mut app, Message::SetsDirInput("sets/here".into()));
    run(&mut app, Message::SetsDirSubmit);
    assert_eq!(app.prefs.sets_dir.as_deref(), Some(first.path()));
}

// Bugs caught: 0, False positives: 0
#[test]
fn set_name_label_says_unsaved_when_dirty() {
    let mut app = plain(None);
    assert!(crate::testing::shows(app.view(), "Set name"));
    assert!(!crate::testing::shows(app.view(), "Set name · unsaved"));
    heard(&mut app, 0.0, false);
    assert!(app.dirty);
    assert!(crate::testing::shows(app.view(), "Set name · unsaved"));
}

// Bugs caught: 0, False positives: 0
#[test]
fn settings_tab_renders_its_sections() {
    let mut app = plain(None);
    run(&mut app, Message::PaneSelected(Pane::Settings));
    run(&mut app, Message::AccentInput("#zz".into()));
    run(&mut app, Message::AccentSubmit);
    for label in [
        "Sets folder",
        "Exports folder",
        "Appearance",
        "Dark",
        "Light",
        "Accent colour",
        "Trace colours",
    ] {
        assert!(crate::testing::shows(app.view(), label), "{label}");
    }
}

// Bugs caught: 0, False positives: 0
#[test]
fn the_appearance_segments_pick_the_mode() {
    let mut app = plain(None);
    run(&mut app, Message::PaneSelected(Pane::Settings));
    let sent = click(app.view(), "Light");
    assert!(
        matches!(sent[..], [Message::ThemePicked(ThemeMode::Light)]),
        "{sent:?}"
    );
}

// The mode applies at once: a new theme, the light palette, the mode's
// default colours in the boxes, and the graph's cached strokes redrawn.
// Bugs caught: 0, False positives: 0
#[test]
fn switching_the_appearance_rebuilds_the_theme_and_redraws_the_graph() {
    let mut app = plain(None);
    heard(&mut app, 0.0, false);
    let draws = |app: &App| {
        crate::testing::ui(crate::testing::SIZE, app.view())
            .snapshot(&app.theme())
            .expect("headless render");
        app.graph.drawn.get()
    };
    let dark = Palette::of(&app.theme());
    assert_eq!(dark, Palette::default());
    let before = draws(&app);

    run(&mut app, Message::ThemePicked(ThemeMode::Light));
    assert_eq!(app.prefs.theme, ThemeMode::Light);
    let light = Palette::of(&app.theme());
    assert_eq!(light, Palette::from_prefs(&app.prefs));
    assert_ne!(light.bg, dark.bg);
    assert_eq!(light.accent, theme::ACCENT_LIGHT);
    assert_eq!(app.settings.accent, theme::to_hex(theme::ACCENT_LIGHT));
    assert_eq!(app.settings.traces, light.traces.map(theme::to_hex));
    let after = draws(&app);
    assert_eq!(after, before + 1, "redrawn in the light colours");
    // Picking the shown mode again changes nothing.
    run(&mut app, Message::ThemePicked(ThemeMode::Light));
    assert_eq!(draws(&app), after);

    // A custom accent stays through a switch back.
    run(&mut app, Message::AccentInput("#30a0f0".into()));
    run(&mut app, Message::AccentSubmit);
    let custom = draws(&app);
    run(&mut app, Message::ThemePicked(ThemeMode::Dark));
    let p = Palette::of(&app.theme());
    assert_eq!(p.bg, dark.bg);
    assert_eq!(Some(p.accent), theme::parse_hex("#30a0f0"));
    assert_eq!(app.settings.accent, "#30a0f0");
    assert_eq!(draws(&app), custom + 1);
}

// A colour typed but not yet entered survives a switch; a box that shows the
// old appearance's colour takes the new one, also where a saved colour is
// invalid and the default is drawn in its place.
// Bugs caught: 0, False positives: 0
#[test]
fn switching_the_appearance_keeps_typed_colours_and_updates_shown_ones() {
    let mut app = plain(None);
    run(&mut app, Message::AccentInput("#30a0".into()));
    run(&mut app, Message::TraceColorInput(2, "#00ff00".into()));
    run(&mut app, Message::ThemePicked(ThemeMode::Light));
    let light = Palette::of(&app.theme());
    assert_eq!(
        light,
        Palette::new(ThemeMode::Light, theme::ACCENT_LIGHT, None)
    );
    assert_eq!(app.settings.accent, "#30a0");
    for (i, shown) in app.settings.traces.iter().enumerate() {
        let want = if i == 2 {
            "#00ff00".to_string()
        } else {
            theme::to_hex(light.traces[i])
        };
        assert_eq!(*shown, want, "trace {i}");
    }

    let prefs = Prefs {
        accent: Some("#zz".into()),
        trace_colors: Some(vec!["#123456".into(); 3]),
        ..Prefs::default()
    };
    let mut app = App::new_for_test(prefs, None);
    assert_eq!(app.settings.accent, theme::to_hex(theme::ACCENT));
    run(&mut app, Message::ThemePicked(ThemeMode::Light));
    let light = Palette::of(&app.theme());
    assert_eq!(
        light,
        Palette::new(ThemeMode::Light, theme::ACCENT_LIGHT, None)
    );
    assert_eq!(app.settings.accent, theme::to_hex(light.accent));
    assert_eq!(app.settings.traces, light.traces.map(theme::to_hex));
}

// The RTA average stays drawn after Stop, dashed in the foreground colour:
// a new appearance redraws it at the same window size.
// Bugs caught: 1, False positives: 0
#[test]
fn switching_the_appearance_redraws_a_stopped_rta_average() {
    let mut app = rta_ready(None);
    run(&mut app, Message::RtaListen(true));
    run(&mut app, rta_event(-30.0, 12));
    run(&mut app, Message::RtaListen(false));
    assert!(app.rta.live.is_some(), "the average stays after Stop");
    let draws = |app: &App| {
        crate::testing::ui(crate::testing::SIZE, app.view())
            .snapshot(&app.theme())
            .expect("headless render");
        app.graph.live_drawn.get()
    };
    let before = draws(&app);
    assert_eq!(draws(&app), before, "cached");
    run(&mut app, Message::ThemePicked(ThemeMode::Light));
    assert_eq!(draws(&app), before + 1, "redrawn in the light foreground");
}

/// A response tilting 6 dB per octave through 0 dB at 1 kHz.
fn tilted() -> Message {
    let freq = log_grid(48_000);
    let db = freq.iter().map(|f| 6.0 * (f / 1000.0).log2()).collect();
    Message::Session(SessionEvent::Measured {
        response: Response {
            freq,
            db,
            phase: None,
        },
        ir: ImpulseResponse {
            rate: 48_000,
            samples: vec![0.0, 1.0, 0.5, 0.0],
            peak: 1,
        },
        sweep_seconds: 6.0,
        drift_ppm: 41.0,
        clipped: false,
    })
}

// Bugs caught: 0, False positives: 0
#[test]
fn align_frequency_and_width_apply() {
    let mut app = plain(None);
    app.state = Activity::Listening;
    run(&mut app, tilted());
    app.state = Activity::Idle;
    // Curved, so aligning the band's mean differs from aligning the point.
    let raw = &mut app.items[0].m.response;
    for (db, f) in raw.db.iter_mut().zip(&raw.freq) {
        *db = 12.0 * (f / 2000.0).log2().powi(2);
    }
    let expected = raw.aligned_band(2000.0, 1.0);
    assert!(
        expected.interp_db(2000.0) < -0.5,
        "the fixture tells them apart"
    );
    run(&mut app, Message::Align(true));
    run(&mut app, Message::AlignHzInput("2k".into()));
    run(&mut app, Message::AlignHzSubmit);
    run(&mut app, Message::AlignWidthPicked(1.0));
    assert_eq!(app.prefs.align_hz, 2000.0);
    assert_eq!(app.prefs.align_octaves, 1.0);
    let shown = app.display(&app.items[0]);
    assert_eq!(shown.freq, expected.freq);
    for (s, e) in shown.db.iter().zip(&expected.db) {
        assert!((s - e).abs() < 1e-9, "{s} vs {e}");
    }
}

// Bugs caught: 0, False positives: 0
#[test]
fn invalid_align_text_is_not_applied() {
    let mut app = plain(None);
    run(&mut app, Message::AlignHzInput("abc".into()));
    run(&mut app, Message::AlignHzSubmit);
    assert_eq!(app.prefs.align_hz, 1000.0);
    // The text stays as typed.
    assert_eq!(app.settings.align_hz, "abc");
    run(&mut app, Message::AlignHzInput("2.5k".into()));
    run(&mut app, Message::AlignHzSubmit);
    assert_eq!(app.prefs.align_hz, 2500.0);
    assert_eq!(app.settings.align_hz, "2.5k");
}

// Bugs caught: 0, False positives: 0
#[test]
fn offset_buttons_move_the_trace() {
    let mut app = plain(None);
    app.state = Activity::Listening;
    run(&mut app, tilted());
    app.state = Activity::Idle;
    let id = app.items[0].m.meta.id.clone();
    let before = app.processed(&app.items[0]);
    run(&mut app, Message::OffsetStep(id.clone(), 0.5));
    run(&mut app, Message::OffsetStep(id, 0.5));
    assert_eq!(app.items[0].m.meta.offset_db, 1.0);
    let shown = app.display(&app.items[0]);
    for ((a, b), f) in shown.db.iter().zip(&before.db).zip(&shown.freq) {
        assert!((a - b - 1.0).abs() < 1e-9, "{f}: {a} vs {b}");
    }
    assert!(app.dirty);
}

// Bugs caught: 0, False positives: 0
#[test]
fn typed_offset_applies_on_enter() {
    let mut app = plain(None);
    app.state = Activity::Listening;
    run(&mut app, tilted());
    app.state = Activity::Idle;
    let id = app.items[0].m.meta.id.clone();
    for (text, want) in [
        ("1.5", 1.5),
        ("+2", 2.0),
        ("-2", -2.0),
        ("1,5", 1.5),
        ("99", 60.0),
    ] {
        run(&mut app, Message::OffsetInput(id.clone(), text.into()));
        run(&mut app, Message::OffsetSubmit(id.clone()));
        assert_eq!(app.items[0].m.meta.offset_db, want, "{text}");
    }
    // Invalid text is kept as typed and not applied.
    run(&mut app, Message::OffsetInput(id.clone(), "1.5".into()));
    run(&mut app, Message::OffsetSubmit(id.clone()));
    run(&mut app, Message::OffsetInput(id.clone(), "abc".into()));
    run(&mut app, Message::OffsetSubmit(id.clone()));
    assert_eq!(app.items[0].m.meta.offset_db, 1.5);
    assert_eq!(app.offset_texts.get(&id).map(String::as_str), Some("abc"));
    // Align does not cancel it: the offset comes last.
    run(&mut app, Message::Align(true));
    assert!((app.display(&app.items[0]).interp_db(1000.0) - 1.5).abs() < 0.1);
    // Deleting clears the text.
    delete(&mut app, &id);
    assert!(!app.offset_texts.contains_key(&id));
}

// Bugs caught: 0, False positives: 0
#[test]
fn offset_is_saved_in_sets() {
    let data = tempfile::tempdir().unwrap();
    let saves = tempfile::tempdir().unwrap();
    let mut app = saver(&data, saves.path());
    app.prefs.ask_save = false;
    let id = app.items[0].m.meta.id.clone();
    run(&mut app, Message::SetNameInput("Offs".into()));
    run(&mut app, Message::OffsetStep(id, -0.5));
    run(&mut app, Message::SaveSet);
    let (_, entries, problems) = set::read_set(&saves.path().join("Offs.fgset")).unwrap();
    assert!(problems.is_empty());
    assert_eq!(entries[0].meta.offset_db, -0.5);
}

// Bugs caught: 0, False positives: 0
#[test]
fn export_includes_the_offset() {
    let mut app = plain(None);
    app.state = Activity::Listening;
    run(&mut app, tilted());
    app.state = Activity::Idle;
    let id = app.items[0].m.meta.id.clone();
    let plain_text = app.export_text(&app.items[0]);
    run(&mut app, Message::OffsetInput(id.clone(), "2".into()));
    run(&mut app, Message::OffsetSubmit(id));
    let text = app.export_text(&app.items[0]);
    assert!(text.contains("* Offset: +2.0 dB"), "{text}");
    assert!(!plain_text.contains("Offset"));
    let before = fastgraph_core::curve_io::read_curve(&plain_text).unwrap();
    let after = fastgraph_core::curve_io::read_curve(&text).unwrap();
    for (a, b) in after.db.iter().zip(&before.db) {
        assert!((a - b - 2.0).abs() < 1e-3);
    }
}

// Bugs caught: 0, False positives: 0
#[test]
fn average_uses_the_offset_traces() {
    let mut app = plain(None);
    heard(&mut app, 0.0, false);
    heard(&mut app, 0.0, false);
    let id = app.items[0].m.meta.id.clone();
    run(&mut app, Message::OffsetInput(id.clone(), "4".into()));
    run(&mut app, Message::OffsetSubmit(id));
    run(&mut app, Message::AverageShown);
    let avg = app.items.last().unwrap();
    assert_eq!(avg.m.meta.offset_db, 2.0, "the mean of the sources offsets");
    assert!((app.display(avg).interp_db(1000.0) - 2.0).abs() < 1e-6);
}

// Bugs caught: 1, False positives: 0
#[test]
fn average_keeps_the_mean_offset_with_align_on() {
    let mut app = plain(None);
    app.state = Activity::Listening;
    run(&mut app, tilted());
    app.state = Activity::Idle;
    heard(&mut app, 5.0, false);
    run(&mut app, Message::Align(true));
    let ids: Vec<String> = app.items.iter().map(|i| i.m.meta.id.clone()).collect();
    for (id, db) in ids.iter().zip(["2", "4"]) {
        run(&mut app, Message::OffsetInput(id.clone(), db.into()));
        run(&mut app, Message::OffsetSubmit(id.clone()));
    }
    let aligned: Vec<Response> = app
        .items
        .iter()
        .map(|i| {
            app.processed(i)
                .aligned_band(app.prefs.align_hz, app.prefs.align_octaves)
        })
        .collect();
    let want = average(&aligned.iter().collect::<Vec<_>>()).unwrap();
    run(&mut app, Message::AverageShown);
    let avg = app.items.last().unwrap();
    assert_eq!(avg.m.meta.offset_db, 3.0);
    let drawn = app.display(avg);
    for (d, w) in drawn.db.iter().zip(&want.db) {
        assert!((d - w - 3.0).abs() < 1e-6, "{d} vs {w}");
    }
}

// Bugs caught: 0, False positives: 0
#[test]
fn offsets_round_to_a_tenth_and_unchanged_values_do_not_dirty() {
    let mut app = plain(None);
    heard(&mut app, 0.0, false);
    let id = app.items[0].m.meta.id.clone();
    run(&mut app, Message::OffsetInput(id.clone(), "1.26".into()));
    run(&mut app, Message::OffsetSubmit(id.clone()));
    assert_eq!(app.items[0].m.meta.offset_db, 1.3);
    run(&mut app, Message::OffsetInput(id.clone(), "60".into()));
    run(&mut app, Message::OffsetSubmit(id.clone()));
    app.dirty = false;
    run(&mut app, Message::OffsetStep(id.clone(), 0.5));
    run(&mut app, Message::OffsetInput(id.clone(), "60".into()));
    run(&mut app, Message::OffsetSubmit(id));
    assert!(!app.dirty);
}

// --- the Distortion view -------------------------------------------------------

/// A distortion analysis on the 48 kHz grid: the fundamental flat at 0 dB,
/// H2 and H3 at `h2` and `h2 - 10` dB, and the noise floor at -100 dB.
fn distortion_at(h2: f64) -> fastgraph_core::distortion::Distortion {
    let level = |db: f64| Response {
        freq: log_grid(48_000),
        db: vec![db; log_grid(48_000).len()],
        phase: None,
    };
    fastgraph_core::distortion::Distortion {
        fundamental: level(0.0),
        harmonics: vec![(2, level(h2)), (3, level(h2 - 10.0))],
        noise: Some(level(-100.0)),
    }
}

/// An app on the Distortion tab with three imported items, the first without
/// distortion data and the others given some.
fn distorted() -> App {
    let mut app = plain(None);
    run(
        &mut app,
        Message::Imported(
            0,
            Ok(vec![
                (PathBuf::from("/in/Plain.txt"), flat(0.0)),
                (PathBuf::from("/in/Left.txt"), flat(0.0)),
                (PathBuf::from("/in/Right.txt"), flat(0.0)),
            ]),
        ),
    );
    let ids = [id(&app, 1), id(&app, 2)];
    app.distortion.insert(ids[0].clone(), distortion_at(-60.0));
    app.distortion.insert(ids[1].clone(), distortion_at(-50.0));
    run(
        &mut app,
        Message::GraphViewSelected(graph::View::Distortion),
    );
    app
}

// The view shows the selected measurement, else the first shown one with
// data, and names it.
// Bugs caught: 0, False positives: 0
#[test]
fn distortion_shows_the_selected_measurement() {
    let mut app = distorted();
    assert!(crate::testing::shows(app.view(), "Distortion: Left"));
    let right = id(&app, 2);
    run(&mut app, Message::Select(right));
    assert!(crate::testing::shows(app.view(), "Distortion: Right"));
    // A selection without data says so, and clicking it again deselects it;
    // with nothing selected the first with data shows.
    let plain_id = id(&app, 0);
    run(&mut app, Message::Select(plain_id.clone()));
    assert!(crate::testing::shows(
        app.view(),
        "Plain has no distortion data"
    ));
    run(&mut app, Message::Select(plain_id));
    assert_eq!(app.selected, None);
    assert!(crate::testing::shows(app.view(), "Distortion: Left"));
    // Hidden ones are not the fallback.
    let left = id(&app, 1);
    run(&mut app, Message::Shown(left, false));
    assert!(crate::testing::shows(app.view(), "Distortion: Right"));
    // The % view leaves the fundamental out; dB keeps it.
    assert_eq!(app.units, graph::Units::Percent);
    assert!(
        !app.graph
            .curve_labels()
            .contains(&"Fundamental".to_string())
    );
    run(&mut app, Message::UnitsPicked(graph::Units::Db));
    assert!(
        app.graph
            .curve_labels()
            .contains(&"Fundamental".to_string())
    );
    assert_eq!(app.graph.curve_labels()[1..], ["H2", "H3", "THD", "Noise"]);
}

// Without data for any shown item the view says so.
// Bugs caught: 0, False positives: 0
#[test]
fn no_data_note_when_nothing_has_harmonics() {
    let mut app = plain(None);
    run(
        &mut app,
        Message::Imported(0, Ok(vec![(PathBuf::from("/in/Flat.txt"), flat(0.0))])),
    );
    run(
        &mut app,
        Message::GraphViewSelected(graph::View::Distortion),
    );
    assert!(crate::testing::shows(
        app.view(),
        "Distortion needs a sweep measured with this version"
    ));
}

// The note says what is actually wrong.
// Bugs caught: 0, False positives: 0
#[test]
fn distortion_notes_are_honest() {
    let mut app = plain(None);
    run(
        &mut app,
        Message::GraphViewSelected(graph::View::Distortion),
    );
    assert!(crate::testing::shows(app.view(), "No measurements"));
    let mut app = distorted();
    for i in 0..3 {
        let item = id(&app, i);
        run(&mut app, Message::Shown(item, false));
    }
    assert!(crate::testing::shows(
        app.view(),
        "Show a sweep measured with this version"
    ));
}

// A click on a row's name selects it, and the selected row has the fill.
// Bugs caught: 0, False positives: 0
#[test]
fn selecting_a_row_highlights_it() {
    let mut app = distorted();
    let sent = click(app.view(), "Right");
    let want = id(&app, 2);
    assert!(
        matches!(&sent[..], [Message::Select(i)] if *i == want),
        "{sent:?}"
    );
    run(&mut app, sent.into_iter().next().unwrap());
    assert_eq!(app.selected, Some(want.clone()));
    let p = theme::Palette::default();
    let theme = theme::with_colors(ThemeMode::Dark, theme::ACCENT, None);
    assert_eq!(
        theme::row_fill(true)(&theme).background,
        Some(p.selected.into())
    );
    assert_eq!(theme::row_fill(false)(&theme).background, None);
    // Deleting it clears the selection and its analysis.
    delete(&mut app, &want);
    assert_eq!(app.selected, None);
    assert!(!app.distortion.contains_key(&want));
}

// A sweep with a long impulse response is analysed on the worker as it
// arrives; one with a short one (an old measurement) has no distortion.
// Bugs caught: 0, False positives: 0
#[test]
fn distortion_is_analysed_on_arrival() {
    let mut app = ready(None);
    let freq = log_grid(48_000);
    let mut samples = vec![0.0f32; 48_000 * 12];
    let peak = 48_000 * 10;
    samples[peak] = 1.0;
    for (i, s) in samples.iter_mut().enumerate() {
        *s += 1e-5 * ((i as f32 * 12.9898).sin() * 43758.547).fract();
    }
    app.state = Activity::Listening;
    run(
        &mut app,
        Message::Session(SessionEvent::Measured {
            response: Response {
                db: vec![0.0; freq.len()],
                phase: None,
                freq,
            },
            ir: ImpulseResponse {
                rate: 48_000,
                samples,
                peak,
            },
            sweep_seconds: 6.0,
            drift_ppm: 0.0,
            clipped: false,
        }),
    );
    let id = id(&app, 0);
    let d = app.distortion.get(&id).expect("analysed");
    assert!(!d.harmonics.is_empty());
}

// An analysis that arrives after its item was deleted, or the session
// replaced by an open, is dropped.
// Bugs caught: 0, False positives: 1
#[test]
fn stale_distortion_results_are_ignored() {
    let mut app = distorted();
    let gone = id(&app, 1);
    delete(&mut app, &gone);
    run(
        &mut app,
        Message::DistortionReady(
            gone.clone(),
            std::sync::Weak::new(),
            Box::new(distortion_at(-60.0)),
        ),
    );
    assert!(!app.distortion.contains_key(&gone));

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Empty.fgset");
    set::write_set(&path, "Empty", &[]).unwrap();
    let old = id(&app, 0);
    assert!(!app.distortion.is_empty());
    // This app has no recovery folder, so an open over unsaved work would be
    // refused; count the session as saved.
    app.dirty = false;
    run(&mut app, Message::OpenSet(path));
    assert!(app.items.is_empty());
    run(
        &mut app,
        Message::DistortionReady(
            old.clone(),
            std::sync::Weak::new(),
            Box::new(distortion_at(-60.0)),
        ),
    );
    assert!(app.distortion.is_empty());
}

// A late analysis of an impulse response that is no longer the item's (the id reused by a
// replacement) is dropped; the item's own is kept.
// Bugs caught: 1, False positives: 0
#[test]
fn a_late_distortion_result_for_another_ir_is_dropped() {
    let mut app = distorted();
    let target = id(&app, 1);
    let ir = |peak| {
        Arc::new(fastgraph_core::deconv::ImpulseResponse {
            rate: 48_000,
            samples: vec![0.0; 100],
            peak,
        })
    };
    let old = ir(1);
    app.items[1].ir = Some(Arc::clone(&old));
    app.distortion.clear();
    // The id is reused by an item with a new impulse response.
    let new = ir(2);
    app.items[1].ir = Some(Arc::clone(&new));
    let ready = |source: &Arc<fastgraph_core::deconv::ImpulseResponse>| {
        Message::DistortionReady(
            target.clone(),
            Arc::downgrade(source),
            Box::new(distortion_at(-60.0)),
        )
    };
    run(&mut app, ready(&old));
    assert!(app.distortion.is_empty());
    run(&mut app, ready(&new));
    assert!(app.distortion.contains_key(&target));
}

// A job that panics never sends its completion; the app must not stay locked, and must not
// count the session as backed up.
// Bugs caught: 1, False positives: 0
#[test]
fn a_worker_panic_unlocks_save_and_distrusts_the_backup() {
    let data = tempfile::tempdir().unwrap();
    let saves = tempfile::tempdir().unwrap();
    let mut app = saver(&data, saves.path());
    run(&mut app, Message::SetNameInput("Panic".into()));
    app.busy = true;
    run(&mut app, Message::WorkerPanicked("boom".into()));
    assert!(!app.busy);
    assert!(app.recovery_failed.load(Ordering::SeqCst));
    run(&mut app, Message::SaveSet);
    assert!(saves.path().join("Panic.fgset").exists());
}

// Smoothing averages across a gap's edge; the plotted curve keeps the gap.
// Bugs caught: 1, False positives: 0
#[test]
fn smoothed_distortion_keeps_a_harmonic_gap() {
    let mut app = distorted();
    let shown = id(&app, 1);
    let mut d = distortion_at(-60.0);
    let h2 = &mut d.harmonics[0].1;
    for (f, db) in h2.freq.iter().zip(h2.db.iter_mut()) {
        if *f > 10_000.0 {
            *db = f64::NAN;
        }
    }
    app.distortion.insert(shown, d);
    run(&mut app, Message::SmoothingPicked(Smoothing::Oct3));
    let plot = app.distortion_plot().expect("a plot");
    let h2 = plot.curves.iter().find(|c| c.label == "H2").unwrap();
    let above: Vec<_> = h2.plotted().filter(|(f, _)| *f > 10_000.0).collect();
    assert!(!above.is_empty());
    assert!(above.iter().all(|(_, y)| y.is_nan()), "{above:?}");
}

// H2..H8 take the seven colours other than the fundamental's, once each; H9 is dim.
// Bugs caught: 1, False positives: 0
#[test]
fn harmonics_do_not_repeat_a_solid_colour() {
    let mut app = distorted();
    let shown = id(&app, 1);
    let mut d = distortion_at(-60.0);
    let h = d.harmonics[0].1.clone();
    d.harmonics = (2..=9).map(|k| (k, h.clone())).collect();
    app.distortion.insert(shown, d);
    run(&mut app, Message::UnitsPicked(graph::Units::Db));
    let plot = app.distortion_plot().expect("a plot");
    let inks: Vec<_> = plot
        .curves
        .iter()
        .filter(|c| c.label.starts_with('H'))
        .map(|c| c.ink)
        .collect();
    assert_eq!(inks.len(), 8);
    assert_eq!(inks[7], graph::Ink::Dim);
    let solid = &inks[..7];
    for (i, a) in solid.iter().enumerate() {
        assert!(!solid[i + 1..].contains(a), "{inks:?}");
    }
}

// --- names, dialogs and saves ----------------------------------------------

// Bugs caught: 1, False positives: 0
#[test]
fn a_unicode_name_saves() {
    let data = tempfile::tempdir().unwrap();
    let saves = tempfile::tempdir().unwrap();
    let mut app = saver(&data, saves.path());
    for name in ["école", "x.FGSET"] {
        run(&mut app, Message::SetNameInput(name.into()));
        run(&mut app, Message::SaveSet);
    }
    assert!(saves.path().join("école.fgset").exists());
    assert!(saves.path().join("x.fgset").exists());
    assert_eq!(std::fs::read_dir(saves.path()).unwrap().count(), 2);
}

// The portal approved "Pads", not the "Pads.fgset" the extension makes of it.
// Bugs caught: 1, False positives: 0
#[test]
fn a_dialog_name_that_becomes_an_existing_set_is_replaced() {
    let data = tempfile::tempdir().unwrap();
    let saves = tempfile::tempdir().unwrap();
    let pads = saves.path().join("Pads.fgset");
    set::write_set(&pads, "Pads", &[]).unwrap();
    let mut app = saver(&data, saves.path());
    run(&mut app, Message::SetNameInput("Mine".into()));
    run(
        &mut app,
        Message::SaveSetTo(Some(saves.path().join("Pads"))),
    );
    assert_eq!(set::read_set(&pads).unwrap().1.len(), 1);
    assert_eq!(app.saved_as.as_deref(), Some(pads.as_path()));
}

// Another program creates the file after Save found it free.
// Bugs caught: 1, False positives: 0
#[test]
fn a_file_that_appears_before_the_write_is_kept() {
    let data = tempfile::tempdir().unwrap();
    let saves = tempfile::tempdir().unwrap();
    let mut app = saver(&data, saves.path());
    run(&mut app, Message::SetNameInput("Late".into()));
    let path = saves.path().join("Late.fgset");
    std::fs::write(&path, "theirs").unwrap();
    run(&mut app, Message::SaveChecked(path.clone(), false));
    assert_eq!(std::fs::read(&path).unwrap(), b"theirs");
    assert_eq!(app.status.level, StatusLevel::Error);
    assert!(
        app.status.text.contains("Late.fgset now exists"),
        "{}",
        app.status.text
    );
    assert!(app.saved_as.is_none());
    // Save again replaces it.
    run(&mut app, Message::SaveSet);
    assert_eq!(set::read_set(&path).unwrap().1.len(), 1);
}

// Bugs caught: 1, False positives: 0
#[test]
fn a_save_keeps_the_name_it_was_clicked_with() {
    let data = tempfile::tempdir().unwrap();
    let saves = tempfile::tempdir().unwrap();
    let mut app = saver(&data, saves.path());
    run(&mut app, Message::SetNameInput("A".into()));
    let check = app.update(Message::SaveSet);
    // The name changes while the folder is checked.
    run(&mut app, Message::SetNameInput("B".into()));
    for message in outputs(check) {
        run(&mut app, message);
    }
    let (header, _, _) = set::read_set(&saves.path().join("A.fgset")).unwrap();
    assert_eq!(header.name, "A");
}

// Bugs caught: 1, False positives: 0
#[test]
fn file_operations_wait_for_the_one_running() {
    let data = tempfile::tempdir().unwrap();
    let saves = tempfile::tempdir().unwrap();
    let other = write_pads(saves.path(), "Pads", "Velour");
    let mut app = saver(&data, saves.path());
    run(&mut app, Message::SetNameInput("Mine".into()));
    // The Save click's check of the folder has not answered yet.
    let check = app.update(Message::SaveSet);
    let before = app.items.len();
    assert_eq!(app.update(Message::OpenSet(other.clone())).units(), 0);
    assert_eq!(app.update(Message::ImportSet(other.clone())).units(), 0);
    // A second click while the check runs is ignored.
    assert_eq!(app.update(Message::SaveSet).units(), 0);
    assert_eq!(app.items.len(), before);
    assert_eq!(app.status.level, StatusLevel::Info);
    assert!(app.status.text.contains("Wait"), "{}", app.status.text);
    for message in outputs(check) {
        run(&mut app, message);
    }
    assert!(saves.path().join("Mine.fgset").exists());
    // An Open waits the same way, and its end frees the next one.
    let read = app.update(Message::OpenSet(other.clone()));
    assert_eq!(app.update(Message::SaveSet).units(), 0);
    for message in outputs(read) {
        run(&mut app, message);
    }
    assert_eq!(names(&app), ["Velour"]);
    assert_ne!(app.update(Message::SaveSet).units(), 0);
}

/// A set of two measurements, `a-0` and `b-0`, whose `b-0` does not load.
fn half_broken(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("Pads.fgset");
    let entries = ["a-0", "b-0"].map(|id| set::Entry {
        meta: meta(id, id),
        response: flat(1.0),
        ir: None,
    });
    set::write_set(&path, "Pads", &entries).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let (from, to) = (b"id = \"b-0\"", b"id = \"b-9\"");
    let at = bytes.windows(from.len()).position(|w| w == from).unwrap();
    let mut bytes = bytes;
    bytes[at..at + to.len()].copy_from_slice(to);
    std::fs::write(&path, bytes).unwrap();
    path
}

// Bugs caught: 2, False positives: 0
#[test]
fn a_partly_read_set_is_not_the_open_set() {
    let data = tempfile::tempdir().unwrap();
    let saves = tempfile::tempdir().unwrap();
    let path = half_broken(saves.path());
    let mut app = plain(data_in(&data));
    app.prefs.sets_dir = Some(saves.path().to_path_buf());
    run(&mut app, Message::OpenSet(path.clone()));
    assert_eq!(names(&app), ["a-0"]);
    assert!(app.saved_as.is_none());
    assert_eq!(app.status.level, StatusLevel::Warn);
    assert!(
        app.status.text.contains("1 unreadable"),
        "{}",
        app.status.text
    );
    // A plain Save has no name to write over it with.
    let before = std::fs::read(&path).unwrap();
    run(&mut app, Message::SaveSet);
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

// Bugs caught: 1, False positives: 0
#[test]
fn an_average_with_too_few_points_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = plain(data_in(&dir));
    // Both lie above the averaging grid.
    let high = |db: f64| Response {
        freq: vec![30_000.0, 40_000.0],
        db: vec![db, db],
        phase: None,
    };
    run(
        &mut app,
        Message::Imported(
            0,
            Ok(vec![
                (PathBuf::from("/in/a.txt"), high(1.0)),
                (PathBuf::from("/in/b.txt"), high(2.0)),
            ]),
        ),
    );
    run(&mut app, Message::AverageShown);
    assert_eq!(app.items.len(), 2);
    assert_eq!(app.status.level, StatusLevel::Warn);
    assert_eq!(recovered(&app).len(), 2);
}

// Bugs caught: 1, False positives: 0
#[test]
fn a_non_utf8_import_path_is_still_recovered() {
    use std::os::unix::ffi::OsStrExt;
    let dir = tempfile::tempdir().unwrap();
    let mut app = plain(data_in(&dir));
    let path = PathBuf::from(std::ffi::OsStr::from_bytes(b"/in/caf\xe9.txt"));
    run(&mut app, Message::Imported(0, Ok(vec![(path, flat(1.0))])));
    let stored = recovered(&app);
    assert_eq!(stored.len(), 1);
    assert_eq!(
        stored[0].meta.source,
        Some(PathBuf::from("/in/caf\u{fffd}.txt"))
    );
}

// A full-band trace at +10 dB and a shorter one at 0 dB: +5 dB where both
// have data, +10 dB where only the first does.
// Bugs caught: 1, False positives: 0
#[test]
fn average_offsets_follow_the_traces_present_at_each_frequency() {
    let mut app = plain(None);
    let short = Response {
        freq: vec![100.0, 1000.0],
        db: vec![0.0, 0.0],
        phase: None,
    };
    run(
        &mut app,
        Message::Imported(
            0,
            Ok(vec![
                (PathBuf::from("/in/full.txt"), flat(0.0)),
                (PathBuf::from("/in/short.txt"), short),
            ]),
        ),
    );
    let full = id(&app, 0);
    run(&mut app, Message::OffsetInput(full.clone(), "10".into()));
    run(&mut app, Message::OffsetSubmit(full));
    run(&mut app, Message::AverageShown);
    let avg = app.items.last().unwrap();
    assert_eq!(avg.m.meta.offset_db, 5.0);
    let drawn = app.display(avg);
    assert!((drawn.interp_db(500.0) - 5.0).abs() < 1e-6, "overlap");
    assert!((drawn.interp_db(5000.0) - 10.0).abs() < 1e-6, "one source");
}

// A failed save must not cancel the recovery write queued before it.
// Bugs caught: 1, False positives: 0
#[test]
fn a_failed_save_then_close_keeps_the_latest_recovery() {
    let data = tempfile::tempdir().unwrap();
    let saves = tempfile::tempdir().unwrap();
    let mut app = saver(&data, saves.path());
    let (open, gate) = std::sync::mpsc::channel::<()>();
    app.worker.submit(Box::new(move || {
        let _ = gate.recv();
        None
    }));
    let _ = app.update(Message::SetNameInput("Latest".into()));
    // A file where the folder should be: the save fails.
    std::fs::write(saves.path().join("file"), "").unwrap();
    let bad = saves.path().join("file").join("Latest.fgset");
    let _ = app.update(Message::SaveChecked(bad, false));
    open.send(()).unwrap();
    let path = app.recovery.clone().unwrap();
    let _ = app.update(Message::CloseRequested);
    let (header, entries, _) = set::read_set(&path).unwrap();
    assert_eq!(header.name, "Latest");
    assert_eq!(entries.len(), 1);
}

// Bugs caught: 1, False positives: 0
#[test]
fn open_keeps_a_session_that_could_not_be_backed_up() {
    let saves = tempfile::tempdir().unwrap();
    let pads = write_pads(saves.path(), "Pads", "Velour");
    // Recovery writes fail: a file where the recovery folder goes.
    let data = tempfile::tempdir().unwrap();
    std::fs::write(data.path().join("recovery"), "").unwrap();
    let mut failing = plain(data_in(&data));
    // No recovery file at all.
    let mut none = plain(None);
    for app in [&mut failing, &mut none] {
        heard(app, 0.0, false);
        run(app, Message::OpenSet(pads.clone()));
        assert_eq!(app.items.len(), 1);
        assert_eq!(app.items[0].m.meta.kind, Kind::Sweep);
        assert!(
            app.status.text.contains("Save first"),
            "{}",
            app.status.text
        );
        // Importing loses nothing and is allowed.
        run(app, Message::ImportSet(pads.clone()));
        assert_eq!(app.items.len(), 2);
    }
    // A clean session may be replaced.
    none.dirty = false;
    run(&mut none, Message::OpenSet(pads.clone()));
    assert_eq!(names(&none), ["Velour"]);
}

// The traces' strokes are cached with their colours: a new colour redraws
// them, at the same window size and with the same measurements.
// Bugs caught: 1, False positives: 0
#[test]
fn colour_changes_redraw_the_traces() {
    let mut app = plain(None);
    heard(&mut app, 0.0, false);
    let draws = |app: &App| {
        crate::testing::ui(crate::testing::SIZE, app.view())
            .snapshot(&app.theme())
            .expect("headless render");
        app.graph.drawn.get()
    };
    let mut before = draws(&app);
    assert_eq!(draws(&app), before, "cached");
    for change in [
        vec![
            Message::TraceColorInput(0, "#00ff00".into()),
            Message::TraceColorSubmit(0),
        ],
        vec![Message::TraceColorsReset],
        vec![
            Message::AccentInput("#30a0f0".into()),
            Message::AccentSubmit,
        ],
        vec![Message::AccentReset],
    ] {
        for message in change {
            run(&mut app, message);
        }
        let after = draws(&app);
        assert_eq!(after, before + 1);
        before = after;
    }
}

// --- Clear -------------------------------------------------------------------

// Bugs caught: 0, False positives: 0
#[test]
fn clear_returns_to_the_first_opened_state() {
    let mut app = plain(None);
    heard(&mut app, 0.0, false);
    heard(&mut app, 1.0, false);
    let first = id(&app, 0);
    run(&mut app, Message::SetNameInput("Mine".into()));
    run(&mut app, Message::GraphViewSelected(graph::View::Phase));
    run(&mut app, Message::PaneSelected(Pane::Settings));
    run(&mut app, Message::Select(first.clone()));
    run(&mut app, Message::OffsetInput(first.clone(), "2.5".into()));
    app.hover = Some((1000.0, -3.0));
    app.dirty = false;
    app.saved_as = Some(PathBuf::from("/x/Mine.fgset"));
    app.sweep_mode = false;
    app.renaming = Some((first, "x".into()));
    run(&mut app, Message::Clear);
    assert!(app.items.is_empty());
    assert!(app.selected.is_none());
    assert!(app.distortion.is_empty());
    assert!(app.offset_texts.is_empty());
    assert!(app.renaming.is_none());
    assert!(app.set_name.is_empty());
    assert!(app.saved_as.is_none());
    assert!(!app.dirty);
    assert_eq!(app.state, Activity::Idle);
    assert!(app.rta.live.is_none());
    assert_eq!(app.graph_view, graph::View::Response);
    assert_eq!(app.pane, Pane::Measure);
    assert!(app.hover.is_none());
    assert!(app.status.text.is_empty());
    assert_eq!(app.graph.trace_count(), 0);
    assert_eq!(app.graph.phase_count(), 0);
    assert!(app.sweep_mode, "Clear returns to Sweep mode");
}

// Bugs caught: 0, False positives: 0
#[test]
fn clearing_unsaved_work_keeps_it_as_an_unsaved_session() {
    let data = tempfile::tempdir().unwrap();
    let saves = tempfile::tempdir().unwrap();
    let mut app = browsing(data_in(&data), saves.path());
    heard(&mut app, 0.0, false);
    assert!(app.dirty);
    let old = app.recovery.clone().unwrap();
    run(&mut app, Message::Clear);
    assert!(app.items.is_empty());
    assert_ne!(app.recovery.as_ref().unwrap(), &old);
    app.worker.flush();
    let (_, kept, _) = set::read_set(&old).unwrap();
    assert_eq!(kept.len(), 1);
    run(&mut app, Message::PaneSelected(Pane::Sets));
    assert_eq!(app.unsaved.len(), 1, "{:?}", app.unsaved);
    assert_eq!(app.unsaved[0].0, old);
}

// Bugs caught: 0, False positives: 0
#[test]
fn clear_stops_listening() {
    let data = tempfile::tempdir().unwrap();
    let mut app = plain(data_in(&data));
    heard(&mut app, 0.0, false);
    run(&mut app, Message::Listen(true));
    assert_eq!(app.state, Activity::Listening);
    run(&mut app, Message::Clear);
    assert_eq!(app.state, Activity::Idle);
    assert!(app.items.is_empty());
}

// Bugs caught: 0, False positives: 0
#[test]
fn clear_is_refused_without_a_backup() {
    let mut app = plain(None);
    heard(&mut app, 0.0, false);
    assert!(app.dirty);
    run(&mut app, Message::Clear);
    assert_eq!(app.items.len(), 1);
    assert_eq!(app.status.level, StatusLevel::Error);
    assert!(
        app.status.text.starts_with("Save first"),
        "{}",
        app.status.text
    );
}

// Bugs caught: 0, False positives: 0
#[test]
fn the_clear_button_is_below_the_list() {
    let mut app = plain(None);
    // Nothing to clear: the button is there but disabled.
    assert!(crate::testing::shows(app.view(), "Clear"));
    heard(&mut app, 0.0, false);
    assert!(matches!(
        click(app.view(), "Clear").as_slice(),
        [Message::Clear]
    ));
}

// Bugs caught: 0, False positives: 0
#[test]
fn side_r_plays_on_the_right_channel() {
    let mut app = ready(None);
    assert_eq!(app.prefs.output_channels, ["FL"]);
    run(&mut app, Message::SidePicked(Side::R));
    assert_eq!(app.prefs.output_channels, ["FR"]);
}

// Bugs caught: 0, False positives: 0
#[test]
fn changing_the_output_rederives_the_channels() {
    let mut app = ready(None);
    run(&mut app, Message::SidePicked(Side::R));
    let mut aux = node(60, "alsa_output.aux", "Aux", true);
    aux.channels = (0..4).map(|i| format!("AUX{i}")).collect();
    run(
        &mut app,
        Message::Audio(AudioEvent::Nodes(vec![mic(), phones(), aux])),
    );
    run(&mut app, Message::OutputPicked("alsa_output.aux".into()));
    assert_eq!(app.prefs.output_channels, ["AUX1"]);
    run(&mut app, Message::OutputPicked(phones().name));
    assert_eq!(app.prefs.output_channels, ["FR"]);
}

// Bugs caught: 0, False positives: 0
#[test]
fn side_channels_follow_the_positions() {
    use crate::devices::side_channels;
    let v = |p: &[&str]| p.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(side_channels(&v(&["FL", "FR"]), Side::L), ["FL"]);
    assert_eq!(side_channels(&v(&["FL", "FR"]), Side::R), ["FR"]);
    assert_eq!(side_channels(&v(&["MONO"]), Side::L), ["MONO"]);
    assert_eq!(side_channels(&v(&["MONO"]), Side::R), ["MONO"]);
    let aux = v(&["AUX0", "AUX1", "AUX2", "AUX3"]);
    assert_eq!(side_channels(&aux, Side::L), ["AUX0"]);
    assert_eq!(side_channels(&aux, Side::R), ["AUX1"]);
    assert_eq!(side_channels(&v(&["AUX0"]), Side::R), ["AUX0"]);
    assert!(side_channels(&[], Side::L).is_empty());
    assert!(side_channels(&[], Side::R).is_empty());
}

// Bugs caught: 0, False positives: 0
#[test]
fn the_output_caption_names_the_side_and_channel() {
    let mut app = ready(None);
    assert!(crate::testing::shows(
        crate::devices::view(&app),
        "Plays on Left (FL)"
    ));
    run(&mut app, Message::SidePicked(Side::R));
    assert!(crate::testing::shows(
        crate::devices::view(&app),
        "Plays on Right (FR)"
    ));
}

// A local capture is named for the side that played, whatever Side shows
// when it is kept.
// Bugs caught: 1, False positives: 0
#[test]
fn a_local_capture_keeps_the_side_it_played_on() {
    let mut app = ready(None);
    // A left sweep, with Right picked while it plays.
    run(&mut app, Message::Measure);
    run(&mut app, Message::SidePicked(Side::R));
    run(&mut app, measured(0.0, false));
    let meta = &app.items[0].m.meta;
    assert_eq!((meta.name.as_str(), meta.side), ("L seat 1", Some(Side::L)));
    // Left noise played here and stopped, then Right picked.
    run(&mut app, Message::SidePicked(Side::L));
    run(&mut app, Message::ModeSweep(false));
    run(&mut app, Message::RtaNoise(true));
    run(&mut app, rta_event(-30.0, 8));
    run(&mut app, Message::RtaNoise(false));
    run(&mut app, Message::SidePicked(Side::R));
    run(&mut app, Message::RtaCapture);
    let meta = &app.items[1].m.meta;
    assert_eq!((meta.name.as_str(), meta.side), ("L RTA 1", Some(Side::L)));
    // Noise played elsewhere takes the Side shown at Capture.
    run(&mut app, Message::RtaListen(true));
    run(&mut app, rta_event(-30.0, 8));
    run(&mut app, Message::RtaCapture);
    assert_eq!(names(&app)[2], "R RTA 1");
}

// Bugs caught: 1, False positives: 0
#[test]
fn clear_is_offered_whenever_there_is_something_to_reset() {
    let mut app = ready(None);
    assert!(app.is_pristine());
    // Listening before the first measurement.
    run(&mut app, Message::Listen(true));
    assert!(!app.is_pristine());
    run(&mut app, Message::Clear);
    assert_eq!(app.state, Activity::Idle);
    assert!(app.is_pristine());
    // Another graph view or pane.
    run(&mut app, Message::GraphViewSelected(graph::View::Phase));
    assert!(!app.is_pristine());
    run(&mut app, Message::Clear);
    assert_eq!(app.graph_view, graph::View::Response);
    run(&mut app, Message::PaneSelected(Pane::Settings));
    assert!(!app.is_pristine());
    run(&mut app, Message::Clear);
    assert_eq!(app.pane, Pane::Measure);
    // Noise playing in RTA mode.
    run(&mut app, Message::ModeSweep(false));
    run(&mut app, Message::RtaNoise(true));
    assert!(!app.is_pristine());
    run(&mut app, Message::Clear);
    assert_eq!(app.state, Activity::Idle);
    assert!(app.sweep_mode);
    assert!(app.is_pristine());
}

// Bugs caught: 2, False positives: 0
#[test]
fn the_graph_tabs_keep_their_width_while_there_is_room() {
    let app = plain(None);
    let mut ui = crate::testing::ui((900.0, 800.0), app.view());
    let first = ui.find("Response").unwrap().bounds();
    let last = ui.find("Distortion").unwrap().bounds();
    // Three equal tabs across the whole strip, inside its 2 px inset.
    let pitch = (last.center_x() - first.center_x()) / 2.0;
    let full = (GRAPH_TABS_WIDTH - 4.0) / 3.0;
    assert!((pitch - full).abs() < 1.0, "{pitch} vs {full}");
}

// Bugs caught: 1, False positives: 0
#[test]
fn an_output_without_channels_says_so() {
    let mut app = ready(None);
    let mut silent = phones();
    silent.channels.clear();
    run(
        &mut app,
        Message::Audio(AudioEvent::Nodes(vec![mic(), silent])),
    );
    assert!(crate::testing::shows(
        crate::devices::view(&app),
        "No channels reported by this output"
    ));
    run(&mut app, Message::Measure);
    assert_eq!(app.status.text, "No channels reported by this output");
    run(&mut app, Message::ModeSweep(false));
    run(&mut app, Message::RtaNoise(true));
    assert_eq!(app.status.text, "No channels reported by this output");
}

// Bugs caught: 1, False positives: 0
#[test]
fn an_import_clicked_before_clear_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = plain(data_in(&dir));
    heard(&mut app, 0.0, false);
    let clicked = app.clears;
    run(&mut app, Message::Clear);
    let late = || Ok(vec![(PathBuf::from("/in/late.txt"), flat(0.0))]);
    run(&mut app, Message::Imported(clicked, late()));
    assert!(app.items.is_empty());
    assert!(!app.dirty);
    // An empty pick changes nothing; an import clicked since is added.
    let since = app.clears;
    run(&mut app, Message::Imported(since, Ok(Vec::new())));
    assert!(!app.dirty);
    assert!(app.status.text.is_empty(), "{}", app.status.text);
    run(&mut app, Message::Imported(since, late()));
    assert_eq!(names(&app), ["late"]);
}

/// The messages a click on Reset view gives; `None` when the window does
/// not show it.
fn reset_view_clicked(app: &App) -> Option<Vec<Message>> {
    let mut ui = crate::testing::ui(crate::testing::SIZE, app.view());
    ui.click("Reset view").ok()?;
    Some(ui.messages())
}

/// The messages a wheel scroll of `lines` notches at `at` over the graph
/// alone, in a window of `size`, gives.
fn graph_scrolled(app: &App, size: (f32, f32), at: iced::Point, lines: f32) -> Vec<Message> {
    let view = app.graph.view(None, false).map(Message::Graph);
    let mut ui = crate::testing::ui(size, view);
    ui.point_at(at);
    ui.simulate([iced::Event::Mouse(iced::mouse::Event::WheelScrolled {
        delta: iced::mouse::ScrollDelta::Lines { x: 0.0, y: lines },
    })]);
    ui.messages()
}

// Bugs caught: 0, False positives: 0
#[test]
fn reset_view_is_offered_after_a_zoom_and_restores_the_default() {
    let mut app = plain(None);
    let curve = Ok(vec![(PathBuf::from("/in/a.txt"), flat(80.0))]);
    let clears = app.clears;
    run(&mut app, Message::Imported(clears, curve));
    assert_eq!(app.graph.trace_count(), 1);
    let size = (1000.0, 600.0);
    let plot = crate::plot::PlotBox::inside(size.0, size.1, graph::INSETS);
    let shown = |app: &App| app.graph.axis(plot);
    let default = shown(&app);
    // Not offered while nothing is zoomed.
    assert!(reset_view_clicked(&app).is_none());
    // The wheel over the plot itself does nothing.
    let middle = iced::Point::new(plot.left + plot.width / 2.0, plot.top + plot.height / 2.0);
    let wheel = |m: &Message| matches!(m, Message::Graph(graph::Event::Wheel(_)));
    assert!(!graph_scrolled(&app, size, middle, 2.0).iter().any(wheel));
    // Over the frequency labels, it zooms frequency.
    let under = iced::Point::new(middle.x, plot.bottom() + 12.0);
    let scrolled = graph_scrolled(&app, size, under, 2.0);
    assert!(scrolled.iter().any(wheel), "{scrolled:?}");
    for message in scrolled {
        run(&mut app, message);
    }
    let zoomed = shown(&app);
    assert!(zoomed.0.f_high / zoomed.0.f_low < default.0.f_high / default.0.f_low);
    assert_eq!((zoomed.0.span, zoomed.1), (default.0.span, default.1));
    // Over the level labels, it zooms the level axis too.
    let beside = iced::Point::new(plot.left - 12.0, middle.y);
    for message in graph_scrolled(&app, size, beside, 1.0) {
        run(&mut app, message);
    }
    assert!(shown(&app).0.span < default.0.span);
    // Reset view is offered in this view only, and restores the default.
    run(&mut app, Message::GraphViewSelected(graph::View::Phase));
    assert!(reset_view_clicked(&app).is_none());
    run(&mut app, Message::GraphViewSelected(graph::View::Response));
    let clicked = reset_view_clicked(&app).expect("Reset view offered once zoomed");
    let reset = |m: &Message| matches!(m, Message::Graph(graph::Event::Reset));
    assert!(clicked.iter().any(reset), "{clicked:?}");
    for message in clicked {
        run(&mut app, message);
    }
    assert_eq!(shown(&app), default);
    assert!(reset_view_clicked(&app).is_none());
}

// Bugs caught: 0, False positives: 0
#[test]
fn clear_resets_every_views_zoom() {
    let mut app = plain(None);
    let plot = crate::plot::PlotBox::inside(1000.0, 600.0, graph::INSETS);
    let wheel = graph::Wheel {
        axis: graph::Axis::Frequency,
        plot,
        at: plot.left + 10.0,
        lines: 3.0,
    };
    // A zoom alone is something for Clear to reset.
    run(&mut app, Message::Graph(graph::Event::Wheel(wheel)));
    assert!(!app.is_pristine());
    run(
        &mut app,
        Message::GraphViewSelected(graph::View::Distortion),
    );
    run(&mut app, Message::Graph(graph::Event::Wheel(wheel)));
    run(&mut app, Message::Clear);
    assert!(!app.graph.any_zoomed());
    assert!(app.is_pristine());
}

// Reset view sits in the margin above the plot, at the plot's right end, so
// it covers neither the plot nor a readout card in it. Its row is the UI
// font's line height at the small size, the same for the fallback face.
// Bugs caught: 1, False positives: 0
#[test]
fn reset_view_sits_in_the_margin_above_the_plot() {
    let mut app = plain(None);
    let curve = Ok(vec![(PathBuf::from("/in/a.txt"), flat(80.0))]);
    let clears = app.clears;
    run(&mut app, Message::Imported(clears, curve));
    let size = (1000.0, 600.0);
    let plot = crate::plot::PlotBox::inside(size.0, size.1, graph::INSETS);
    let wheel = graph::Wheel {
        axis: graph::Axis::Level,
        plot,
        at: plot.top + 20.0,
        lines: 2.0,
    };
    run(&mut app, Message::Graph(graph::Event::Wheel(wheel)));
    let view = app.graph.view(None, false).map(Message::Graph);
    let mut ui = crate::testing::ui(size, view);
    let label = ui.find("Reset view").expect("Reset view offered").bounds();
    assert!(label.y >= 0.0, "{label:?}");
    assert!(label.y + label.height <= plot.top, "{label:?}");
    // Its button's right edge, past 8 px of padding, is the plot's.
    assert!(
        (label.x + label.width + 8.0 - plot.right()).abs() < 1.0,
        "{label:?}"
    );
}
