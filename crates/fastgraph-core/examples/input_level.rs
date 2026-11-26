//! Meter one input channel through the engine, as the app's meter does:
//! `cargo run -p fastgraph-core --example input_level -- <node.name> <channel label> [seconds]`.
//! Prefix the name with `monitor:` for a sink's monitor. Prints the peak each 0.5 s.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use fastgraph_core::audio::{AudioCommand, AudioEvent, Engine, Input, channel_display};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let key = args
        .next()
        .context("usage: input_level <node.name> <channel> [seconds]")?;
    let label = args
        .next()
        .context("missing channel label, e.g. AUX1 or MONO")?;
    let seconds: f64 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(3.0);

    let (ev_tx, ev_rx) = mpsc::channel();
    let (s_tx, s_rx) = mpsc::channel::<Vec<f32>>();
    let engine = Engine::start(Box::new(move |e| drop(ev_tx.send(e))), s_tx)?;

    let deadline = Instant::now() + Duration::from_secs(2);
    let input = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match ev_rx.recv_timeout(left) {
            Ok(AudioEvent::Nodes(nodes)) => {
                let found = nodes.into_iter().find_map(|n| {
                    let input = if n.is_sink {
                        Input::MonitorOf(n)
                    } else {
                        Input::Source(n)
                    };
                    (input.key() == key).then_some(input)
                });
                if let Some(i) = found {
                    break i;
                }
            }
            Ok(_) => {}
            Err(_) => bail!("no input with key {key}"),
        }
    };
    let channel = input
        .node()
        .channels
        .iter()
        .position(|c| *c == label)
        .with_context(|| format!("{} has no channel {label}", input.label()))?;
    println!("{} · {}", input.label(), channel_display(&label));
    engine.send(AudioCommand::OpenInput { input, channel });

    let end = Instant::now() + Duration::from_secs_f64(seconds);
    let mut window = Instant::now();
    let mut peak = 0f32;
    while Instant::now() < end {
        while let Ok(e) = ev_rx.try_recv() {
            match e {
                AudioEvent::InputStarted { rate } => println!("started at {rate} Hz"),
                AudioEvent::InputLost(why) | AudioEvent::Error(why) => bail!("input lost: {why}"),
                _ => {}
            }
        }
        if let Ok(block) = s_rx.recv_timeout(Duration::from_millis(50)) {
            peak = block.iter().fold(peak, |p, x| p.max(x.abs()));
        }
        if window.elapsed() >= Duration::from_millis(500) {
            println!("peak {:7.1} dBFS", 20.0 * peak.max(1e-12).log10());
            peak = 0.0;
            window = Instant::now();
        }
    }
    Ok(())
}
