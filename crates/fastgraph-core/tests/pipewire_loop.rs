//! Live tests against a PipeWire null sink (`fastgraph_test`). Ignored by default; run with
//! `cargo test -p fastgraph-core --test pipewire_loop -- --ignored --nocapture`.
//! Nothing is played on a real device: playback only ever targets the null sink.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{NullSink, SERIAL, SINK, start, wait_for, wait_for_test_sink};
use fastgraph_core::audio::{AudioCommand, AudioEvent, Input};
use fastgraph_core::detect::{DetectEvent, Detector};
use fastgraph_core::signal::{SweepLength, sweep_stimulus};

// Bugs caught: 1, False positives: 0
#[test]
#[ignore = "needs PipeWire; loads a null sink"]
fn detects_sweep_through_null_sink() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _sink = NullSink::load();
    let (engine, events, blocks) = start();
    let node = wait_for_test_sink(&events);
    assert!(node.is_sink);
    assert_eq!(node.channels, ["FL", "FR"]);

    engine.send(AudioCommand::OpenInput {
        input: Input::MonitorOf(node.clone()),
        channel: 0,
    });
    let rate = wait_for(&events, Duration::from_secs(3), |e| match e {
        AudioEvent::InputStarted { rate } => Some(rate),
        AudioEvent::InputLost(m) => panic!("input lost: {m}"),
        _ => None,
    });
    println!("capture rate {rate}");

    let stimulus = sweep_stimulus(SweepLength::Normal, 48_000);
    let play_started = Instant::now();
    engine.send(AudioCommand::Play {
        sink: node,
        channels: vec![0],
        samples: Arc::new(stimulus.samples),
        rate: 48_000,
        gain: 1.0,
        looped: false,
    });

    let mut detector = Detector::new(SweepLength::Normal, rate);
    let mut captures = Vec::new();
    let mut finished_at = None;
    let mut received = 0usize;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        // Keep listening one second after playback finished to catch duplicate detections.
        if finished_at.is_some_and(|t: Instant| t.elapsed() > Duration::from_secs(1)) {
            break;
        }
        for e in events.try_iter() {
            match e {
                AudioEvent::PlayFinished => {
                    println!("PlayFinished after {:.2?}", play_started.elapsed());
                    finished_at = Some(Instant::now());
                }
                AudioEvent::PlayFailed(m) => panic!("play failed: {m}"),
                AudioEvent::InputLost(m) => panic!("input lost: {m}"),
                AudioEvent::Error(m) => panic!("engine error: {m}"),
                _ => {}
            }
        }
        if let Ok(block) = blocks.recv_timeout(Duration::from_millis(50)) {
            received += block.len();
            for ev in detector.push(&block) {
                if let DetectEvent::Captured(c) = ev {
                    println!(
                        "Captured: drift {:.2} ppm, peak ratio {:.1}, clipped {}",
                        c.drift_ppm, c.peak_ratio, c.clipped
                    );
                    captures.push(c);
                }
            }
        }
    }
    println!(
        "received {received} samples ({:.2} s)",
        received as f64 / rate as f64
    );
    assert!(finished_at.is_some(), "PlayFinished not received");
    assert_eq!(captures.len(), 1, "expected exactly one capture");
    assert!(
        captures[0].drift_ppm.abs() < 5.0,
        "drift {}",
        captures[0].drift_ppm
    );
    drop(engine);
}

// Bugs caught: 2, False positives: 0
#[test]
#[ignore = "needs PipeWire; loads a null sink"]
fn input_lost_when_device_disappears() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut sink = NullSink::load();
    let (engine, events, _blocks) = start();
    let node = wait_for_test_sink(&events);
    engine.send(AudioCommand::OpenInput {
        input: Input::MonitorOf(node),
        channel: 1,
    });
    wait_for(&events, Duration::from_secs(3), |e| match e {
        AudioEvent::InputStarted { .. } => Some(()),
        AudioEvent::InputLost(m) => panic!("input lost early: {m}"),
        _ => None,
    });
    let unplugged = Instant::now();
    sink.unload();
    let message = wait_for(&events, Duration::from_secs(3), |e| match e {
        AudioEvent::InputLost(m) => Some(m),
        _ => None,
    });
    println!("InputLost after {:.2?}: {message}", unplugged.elapsed());
    // The engine keeps serving the device list after the loss.
    wait_for(&events, Duration::from_secs(3), |e| match e {
        AudioEvent::Nodes(n) => n.iter().all(|n| n.name != SINK).then_some(()),
        AudioEvent::Error(m) => panic!("engine error: {m}"),
        _ => None,
    });
}
