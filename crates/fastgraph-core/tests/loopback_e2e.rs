//! End-to-end checks through real PipeWire, driving `audio::Engine` and `session::Session` the
//! way the GUI does. Ignored by default; run with
//! `cargo test -p fastgraph-core --test loopback_e2e -- --ignored --nocapture --test-threads 1`.
//!
//! Everything plays into a `fastgraph_test` null sink and is captured from its monitor; nothing
//! reaches a real device. External players are `pw-play` subprocesses started after a random
//! 2–6 s delay while the session keeps listening, as a phone or another computer would.

mod common;

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::{NullSink, SERIAL, SINK, start, wait_for, wait_for_test_sink};
use fastgraph_core::audio::{AudioCommand, AudioEvent, Engine, Input, Node};
use fastgraph_core::distortion;
use fastgraph_core::response::{Response, Smoothing};
use fastgraph_core::rta::noise_stimulus;
use fastgraph_core::session::{Mode, Session, SessionCommand, SessionEvent};
use fastgraph_core::signal::{SweepLength, pink_noise, sweep_stimulus, write_wav};

/// The GUI's playback rate and the rate the sweep is exported at.
const RATE: u32 = 48_000;
/// Peak of the sweep in the stimulus file.
const SWEEP_PEAK: f64 = 0.5;
/// Default local playback level (sweep peak in dBFS).
const LEVEL_DB: f64 = -12.0;
const ALIGN_HZ: f64 = 1000.0;
/// Local Measure gives up this long after playback finished.
const MEASURE_TIMEOUT: Duration = Duration::from_secs(3);
/// How long to keep listening after a measurement, to catch duplicates.
const SETTLE: Duration = Duration::from_secs(1);

/// The capture side of the GUI: the engine capturing the null sink's monitor, and a session
/// analysing it. Fields drop in order: session, engine, then the sink is unloaded.
struct Rig {
    _session: Session,
    session_events: mpsc::Receiver<SessionEvent>,
    engine: Engine,
    audio_events: mpsc::Receiver<AudioEvent>,
    node: Node,
    /// A `PlayFinished` was seen.
    play_finished: Cell<bool>,
    opened: Instant,
    _sink: NullSink,
}

impl Rig {
    fn open(mode: Mode) -> Rig {
        let sink = NullSink::load();
        let (engine, audio_events, blocks) = start();
        let node = wait_for_test_sink(&audio_events);
        let (tx, session_events) = mpsc::channel();
        let session = Session::start(blocks, Box::new(move |e| drop(tx.send(e))));
        engine.send(AudioCommand::OpenInput {
            input: Input::MonitorOf(node.clone()),
            channel: 0,
        });
        let rate = wait_for(&audio_events, Duration::from_secs(10), |e| match e {
            AudioEvent::InputStarted { rate } => Some(rate),
            AudioEvent::InputLost(m) => panic!("input lost: {m}"),
            _ => None,
        });
        println!("capturing Monitor of {SINK} at {rate} Hz");
        session.send(SessionCommand::Rate(rate));
        session.send(SessionCommand::SetMode(mode));
        Rig {
            _session: session,
            session_events,
            engine,
            audio_events,
            node,
            play_finished: Cell::new(false),
            opened: Instant::now(),
            _sink: sink,
        }
    }

    fn elapsed(&self) -> Duration {
        self.opened.elapsed()
    }

    /// Handles engine events: notes `PlayFinished`, panics on a lost input or a failure.
    fn audio(&self) {
        for e in self.audio_events.try_iter() {
            match e {
                AudioEvent::PlayFinished => self.play_finished.set(true),
                AudioEvent::InputLost(m) => panic!("input lost: {m}"),
                AudioEvent::PlayFailed(m) => panic!("play failed: {m}"),
                AudioEvent::Error(m) => panic!("engine error: {m}"),
                _ => {}
            }
        }
    }

    /// Session outcomes (everything but meter and near-miss updates) arriving within `wait`.
    fn outcomes(&self, wait: Duration, out: &mut Vec<SessionEvent>) {
        let deadline = Instant::now() + wait;
        loop {
            self.audio();
            let left = deadline.saturating_duration_since(Instant::now());
            match self
                .session_events
                .recv_timeout(left.min(Duration::from_millis(50)))
            {
                Ok(SessionEvent::Level { .. } | SessionEvent::BestRatio(_)) => {}
                Ok(e) => out.push(e),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(e) => panic!("session gone: {e}"),
            }
            if left.is_zero() {
                return;
            }
        }
    }

    /// Listen until a sweep outcome arrives or `wait` passes, then `SETTLE` more.
    fn sweep_outcomes(&self, wait: Duration, out: &mut Vec<SessionEvent>) {
        let deadline = Instant::now() + wait;
        let before = out.len();
        while out.len() == before && Instant::now() < deadline {
            self.outcomes(Duration::from_millis(100), out);
        }
        self.outcomes(SETTLE, out);
    }
}

/// A uniform value in [0, 1) from the clock; enough for a playback delay.
fn random_unit() -> f64 {
    let mut x = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
        | 1;
    for _ in 0..4 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
    }
    (x >> 11) as f64 / (1u64 << 53) as f64
}

fn random_delay() -> Duration {
    Duration::from_secs_f64(2.0 + 4.0 * random_unit())
}

/// `pw-play` into the null sink only: never falls back to another device.
fn pw_play(path: &Path) -> Child {
    Command::new("pw-play")
        .args([
            "--target",
            SINK,
            "-P",
            "{ node.dont-fallback = true node.dont-reconnect = true }",
        ])
        .arg(path)
        .spawn()
        .expect("starting pw-play")
}

/// Waits a random 2–6 s while listening, then plays `path` with `pw-play` and listens until a
/// sweep outcome arrives (or 5 s after the player exited). Returns all outcomes seen.
fn play_externally(rig: &Rig, path: &Path) -> Vec<SessionEvent> {
    let mut out = Vec::new();
    let delay = random_delay();
    println!("{}: playing after {delay:.2?}", path.display());
    rig.outcomes(delay, &mut out);
    assert!(out.is_empty(), "outcomes before playback: {out:?}");
    let mut player = pw_play(path);
    let started = Instant::now();
    let status = loop {
        if let Some(status) = player.try_wait().expect("waiting for pw-play") {
            break status;
        }
        rig.outcomes(Duration::from_millis(50), &mut out);
    };
    assert!(status.success(), "pw-play: {status}");
    println!("  pw-play exited after {:.2?}", started.elapsed());
    rig.sweep_outcomes(Duration::from_secs(5), &mut out);
    out
}

/// Largest |dB| of `r` within `lo..=hi` Hz and where it is. Panics when the band has no
/// points.
fn max_error(r: &Response, lo: f64, hi: f64) -> (f64, f64) {
    r.freq
        .iter()
        .zip(&r.db)
        .filter(|(f, _)| (lo..=hi).contains(*f))
        .map(|(&f, &db)| {
            assert!(db.is_finite(), "no data at {f:.1} Hz");
            (db.abs(), f)
        })
        .reduce(|a, b| if b.0 > a.0 { b } else { a })
        .unwrap_or_else(|| panic!("no response points in {lo}..={hi} Hz"))
}

/// The single `Measured` among `out`: (aligned response, drift ppm). Panics on anything else.
fn single_measurement(label: &str, out: Vec<SessionEvent>) -> (Response, f64) {
    let mut measured = Vec::new();
    for e in out {
        match e {
            SessionEvent::Measured {
                response,
                drift_ppm,
                clipped,
                ..
            } => {
                assert!(!clipped, "{label}: clipped");
                measured.push((response.aligned(ALIGN_HZ), drift_ppm));
            }
            other => panic!("{label}: unexpected {other:?}"),
        }
    }
    assert_eq!(
        measured.len(),
        1,
        "{label}: expected exactly one measurement"
    );
    measured.pop().unwrap()
}

/// Drift the detector should report for a 48 kHz file declared at `declared` Hz:
/// the markers land `48000 / declared` times as far apart as expected.
fn expected_drift_ppm(declared: u32) -> f64 {
    (f64::from(RATE) / f64::from(declared) - 1.0) * 1e6
}

// Bugs caught: 0, False positives: 0
#[test]
#[ignore = "needs PipeWire; loads a null sink"]
fn external_sweeps_with_drift() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let stimulus = sweep_stimulus(SweepLength::Normal, RATE);
    // Drift copies: the same samples, declared at another rate, as a player whose clock runs
    // off by that much would play them. And a 44.1 kHz export, resampled by PipeWire.
    let mut files: Vec<(String, PathBuf, f64)> = Vec::new();
    for declared in [48_000, 48_014, 47_986] {
        let path = dir.path().join(format!("sweep-{declared}.wav"));
        write_wav(&path, &stimulus.samples, declared, 2).unwrap();
        files.push((
            format!("48k file declared {declared} Hz"),
            path,
            expected_drift_ppm(declared),
        ));
    }
    let path = dir.path().join("sweep-44k1.wav");
    let s44 = sweep_stimulus(SweepLength::Normal, 44_100);
    write_wav(&path, &s44.samples, 44_100, 2).unwrap();
    files.push(("44.1 kHz export".into(), path, 0.0));

    let rig = Rig::open(Mode::Sweep(SweepLength::Normal));
    let mut results = Vec::new();
    for (label, path, want) in &files {
        let (response, drift) = single_measurement(label, play_externally(&rig, path));
        let (err, at) = max_error(&response, 30.0, 16_000.0);
        println!(
            "  {label}: drift {drift:+.2} ppm (expected {want:+.2}), \
             max error {err:.3} dB at {at:.1} Hz"
        );
        results.push((label, drift, *want, err, at));
    }
    for (label, drift, want, err, at) in results {
        assert!(
            (drift - want).abs() <= 10.0,
            "{label}: drift {drift:+.2} ppm, want {want:+.2}"
        );
        assert!(err <= 0.5, "{label}: {err:.3} dB at {at:.1} Hz");
    }
}

// Bugs caught: 0, False positives: 0
#[test]
#[ignore = "needs PipeWire; loads a null sink"]
fn external_sweep_over_other_audio() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let sweep = dir.path().join("sweep.wav");
    write_wav(
        &sweep,
        &sweep_stimulus(SweepLength::Normal, RATE).samples,
        RATE,
        2,
    )
    .unwrap();
    // Pink noise at −30 dBFS RMS, long enough to cover the delay and the whole sweep.
    let noise_gain = 10f32.powf(-30.0 / 20.0);
    let noise: Vec<f32> = pink_noise(RATE as usize * 25, RATE, 7)
        .into_iter()
        .map(|v| v * noise_gain)
        .collect();
    let noise_path = dir.path().join("noise.wav");
    write_wav(&noise_path, &noise, RATE, 2).unwrap();

    let rig = Rig::open(Mode::Sweep(SweepLength::Normal));
    let mut background = pw_play(&noise_path);
    let out = play_externally(&rig, &sweep);
    let still_playing = background.try_wait().unwrap().is_none();
    let _ = background.kill();
    let _ = background.wait();
    assert!(
        still_playing,
        "the noise ended before the sweep was measured"
    );

    let (response, drift) = single_measurement("sweep over noise", out);
    let (err, at) = max_error(&response, 30.0, 16_000.0);
    println!(
        "  sweep over −30 dB pink noise: drift {drift:+.2} ppm, max error {err:.3} dB at {at:.1} Hz"
    );
    assert!(drift.abs() <= 10.0, "drift {drift:+.2} ppm");
    assert!(err <= 1.0, "{err:.3} dB at {at:.1} Hz");
}

// Bugs caught: 0, False positives: 0
#[test]
#[ignore = "needs PipeWire; loads a null sink"]
fn local_measure() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let rig = Rig::open(Mode::Sweep(SweepLength::Normal));
    let samples = Arc::new(sweep_stimulus(SweepLength::Normal, RATE).samples);
    let length = Duration::from_secs_f64(samples.len() as f64 / f64::from(RATE));
    let gain = (10f64.powf(LEVEL_DB / 20.0) / SWEEP_PEAK) as f32;
    let started = Instant::now();
    rig.engine.send(AudioCommand::Play {
        sink: rig.node.clone(),
        channels: vec![0, 1],
        samples,
        rate: RATE,
        gain,
        looped: false,
    });
    let mut out = Vec::new();
    let deadline = started + length + Duration::from_secs(10);
    let finished = loop {
        assert!(Instant::now() < deadline, "PlayFinished not received");
        if rig.play_finished.get() {
            break started.elapsed();
        }
        rig.outcomes(Duration::from_millis(50), &mut out);
    };
    println!("PlayFinished after {finished:.2?}");
    rig.sweep_outcomes(MEASURE_TIMEOUT, &mut out);

    let distortion = out.iter().find_map(|e| match e {
        SessionEvent::Measured {
            ir, sweep_seconds, ..
        } => distortion::analyse(ir, *sweep_seconds),
        _ => None,
    });
    let (response, drift) = single_measurement("local measure", out);
    let (err, at) = max_error(&response, 30.0, 16_000.0);
    println!(
        "  local Measure at {LEVEL_DB} dB: drift {drift:+.2} ppm, max error {err:.3} dB at {at:.1} Hz"
    );
    // A clean digital loop has no distortion: every harmonic sits at the noise floor. Both are
    // medians of noisy estimates, hence the 6 dB margin.
    let d = distortion.expect("the sweep's distortion");
    assert!(
        d.harmonics
            .iter()
            .map(|(k, _)| *k)
            .eq(2..=fastgraph_core::distortion::MAX_HARMONIC),
        "a Normal sweep must retain H2 through H9"
    );
    let median_over = |r: &Response| {
        let mut v: Vec<f64> = r
            .freq
            .iter()
            .zip(&r.db)
            .filter(|(f, db)| (200.0..=5000.0).contains(*f) && !db.is_nan())
            .map(|(_, db)| *db)
            .collect();
        assert!(!v.is_empty(), "no points in 200 Hz..5 kHz");
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let noise = d.noise.as_ref().map(median_over);
    for (k, h) in &d.harmonics {
        let level = median_over(h);
        println!(
            "  H{k}: {level:+.1} dB, noise {} dB (absolute)",
            noise.map_or("n/a".into(), |n| format!("{n:+.1}"))
        );
        assert!(
            noise.is_some_and(|n| level < n + 6.0),
            "H{k} {level:+.1} dB is above the noise {noise:?} + 6 dB"
        );
    }
    assert!(drift.abs() <= 10.0, "drift {drift:+.2} ppm");
    assert!(err <= 0.5, "{err:.3} dB at {at:.1} Hz");
}

// Bugs caught: 0, False positives: 0
#[test]
#[ignore = "needs PipeWire; loads a null sink"]
fn rta_noise_is_flat() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let rig = Rig::open(Mode::Rta);
    let gain = (10f64.powf(LEVEL_DB / 20.0) / SWEEP_PEAK) as f32;
    rig.engine.send(AudioCommand::Play {
        sink: rig.node.clone(),
        channels: vec![0, 1],
        samples: Arc::new(noise_stimulus(RATE)),
        rate: RATE,
        gain,
        looped: true,
    });
    // Statistics, not the chain, limit flatness: with 1/12 octave at 50 Hz a smoothing cell
    // holds about 4 FFT bins (0.73 Hz apart), so after 20 s (28 Welch frames) the per-cell
    // spread is about 0.5 dB and the worst cell of 50–200 Hz often passes 1 dB. The same noise
    // fed straight into `Rta` (no PipeWire) gives 0.9–1.2 dB after 20 s and 0.5–0.7 dB after
    // 45 s. So the 20 s figure is reported and ±1 dB is required after 45 s.
    let mut report = Vec::new();
    for secs in [20, 45] {
        let mut out = Vec::new();
        rig.outcomes(
            Duration::from_secs(secs).saturating_sub(rig.elapsed()),
            &mut out,
        );
        let (response, frames) = latest_rta(out).expect("no RTA update");
        let mut errs = Vec::new();
        for smoothing in [Smoothing::Oct12, Smoothing::Oct6] {
            let shown = response.smoothed(smoothing).aligned(ALIGN_HZ);
            let (err, at) = max_error(&shown, 50.0, 16_000.0);
            println!(
                "  RTA after {secs} s ({frames} frames), {}: max error {err:.3} dB at {at:.1} Hz",
                smoothing.label()
            );
            errs.push((err, at));
        }
        report.push(errs[0]);
    }
    rig.engine.send(AudioCommand::StopPlay);
    let (err, at) = report[1];
    assert!(err <= 1.0, "after 45 s: {err:.3} dB at {at:.1} Hz");
}

/// The latest `Rta` update in `out`; panics on any other outcome.
fn latest_rta(out: Vec<SessionEvent>) -> Option<(Response, usize)> {
    let mut latest = None;
    for e in out {
        match e {
            SessionEvent::Rta { response, frames } => latest = Some((response, frames)),
            other => panic!("unexpected {other:?}"),
        }
    }
    latest
}
