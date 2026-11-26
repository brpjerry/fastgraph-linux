//! Print the audio devices the engine reports: `cargo run -p fastgraph-core --example list_devices`.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use fastgraph_core::audio::{AudioEvent, Engine};

fn main() -> anyhow::Result<()> {
    let (ev_tx, ev_rx) = mpsc::channel();
    let (s_tx, _s_rx) = mpsc::channel();
    let _engine = Engine::start(Box::new(move |e| drop(ev_tx.send(e))), s_tx)?;
    // The list settles once node params have arrived; print the last one within a second.
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut nodes = None;
    while let Ok(e) = ev_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        if let AudioEvent::Nodes(n) = e {
            nodes = Some(n);
        }
    }
    let nodes = nodes.ok_or_else(|| anyhow::anyhow!("no device list received"))?;
    for n in &nodes {
        println!(
            "{:<6} {:<60} serial={:<6} channels={:<12} {}",
            if n.is_sink { "sink" } else { "source" },
            n.label(),
            n.serial,
            n.channels.join(","),
            n.name
        );
    }
    Ok(())
}
