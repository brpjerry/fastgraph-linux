//! Shared helpers for the live PipeWire tests: a `fastgraph_test` null sink that is always
//! unloaded, and an audio engine with its event and sample channels.
// Each test binary compiles this module and uses a different subset of it.
#![allow(dead_code)]

use std::process::Command;
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

use fastgraph_core::audio::{AudioEvent, Engine, Node};

pub const SINK: &str = "fastgraph_test";

/// Tests share the sink name, so they run one at a time.
pub static SERIAL: Mutex<()> = Mutex::new(());

/// Run pactl; `None` on any failure. Used where panicking is not allowed (Drop).
pub fn try_pactl(args: &[&str]) -> Option<String> {
    let out = Command::new("pactl").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

pub fn pactl(args: &[&str]) -> String {
    let out = Command::new("pactl")
        .args(args)
        .output()
        .expect("running pactl");
    assert!(
        out.status.success(),
        "pactl {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A loaded `module-null-sink`, unloaded on drop (also when the test panics). Restores the
/// default sink if loading the null sink changed it.
pub struct NullSink {
    module: Option<String>,
    default_before: String,
}

impl NullSink {
    pub fn load() -> NullSink {
        let default_before = pactl(&["get-default-sink"]);
        let module = pactl(&[
            "load-module",
            "module-null-sink",
            &format!("sink_name={SINK}"),
        ]);
        let sink = NullSink {
            module: Some(module),
            default_before,
        };
        println!("loaded null sink module {:?}", sink.module);
        sink
    }

    pub fn unload(&mut self) {
        if let Some(id) = self.module.take() {
            match try_pactl(&["unload-module", &id]) {
                Some(_) => println!("unloaded null sink module {id}"),
                None => eprintln!("failed to unload null sink module {id}"),
            }
        }
    }
}

impl Drop for NullSink {
    fn drop(&mut self) {
        self.unload();
        // The default follows priorities once the null sink is gone; only intervene if not.
        let deadline = Instant::now() + Duration::from_secs(2);
        // Nothing here may panic: Drop also runs while a failed test unwinds.
        loop {
            let now = try_pactl(&["get-default-sink"]);
            if now.as_deref() == Some(self.default_before.as_str()) {
                break;
            }
            if Instant::now() > deadline {
                println!(
                    "default sink is {now:?}, restoring {:?}",
                    self.default_before
                );
                if try_pactl(&["set-default-sink", &self.default_before]).is_none() {
                    eprintln!("failed to restore the default sink");
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

pub fn start() -> (Engine, mpsc::Receiver<AudioEvent>, mpsc::Receiver<Vec<f32>>) {
    let (ev_tx, ev_rx) = mpsc::channel();
    let (s_tx, s_rx) = mpsc::channel();
    let engine = Engine::start(Box::new(move |e| drop(ev_tx.send(e))), s_tx).expect("engine");
    (engine, ev_rx, s_rx)
}

pub fn wait_for<T>(
    events: &mpsc::Receiver<AudioEvent>,
    within: Duration,
    mut pick: impl FnMut(AudioEvent) -> Option<T>,
) -> T {
    let deadline = Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(left) {
            Ok(e) => {
                if let Some(v) = pick(e) {
                    return v;
                }
            }
            Err(e) => panic!("timed out after {within:?} ({e})"),
        }
    }
}

pub fn print_nodes(nodes: &[Node]) {
    for n in nodes {
        println!(
            "  {:<6} {:<55} serial={:<6} id={:<4} {:?}  [{}]",
            if n.is_sink { "sink" } else { "source" },
            n.description,
            n.serial,
            n.id,
            n.channels,
            n.name
        );
    }
}

pub fn wait_for_test_sink(events: &mpsc::Receiver<AudioEvent>) -> Node {
    wait_for(events, Duration::from_secs(3), |e| match e {
        AudioEvent::Nodes(nodes) => {
            println!("Nodes:");
            print_nodes(&nodes);
            nodes.into_iter().find(|n| n.name == SINK)
        }
        _ => None,
    })
}
