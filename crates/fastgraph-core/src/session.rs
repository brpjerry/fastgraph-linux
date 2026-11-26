//! Analysis session: a worker thread between the capture stream and the GUI.
//!
//! It meters the input, runs the sweep detector and deconvolution or the RTA
//! analyser depending on the mode, and reports through an [`EventSink`].

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::EventSink;
use crate::deconv::{ImpulseResponse, deconvolve, gate, long_ir, response_from_ir};
use crate::detect::{DetectEvent, Detector};
use crate::response::Response;
use crate::rta::Rta;
use crate::signal::{MIN_SWEEP_RATE, SweepLength};

const POLL: Duration = Duration::from_millis(50);
const LEVEL_EVERY: Duration = Duration::from_millis(50);
const BEST_EVERY: Duration = Duration::from_millis(500);
const RTA_EVERY: Duration = Duration::from_millis(200);
const CLIP_LEVEL: f32 = 0.999;
const FLOOR_DBFS: f32 = -120.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Idle,
    Sweep(SweepLength),
    Rta,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionCommand {
    Rate(u32),
    SetMode(Mode),
    ResetRta,
    /// Start the detector or the RTA afresh, even when the rate and mode are
    /// unchanged (a new run, or a new input stream).
    Reset,
    Shutdown,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SessionEvent {
    Level {
        peak_dbfs: f32,
        clipped: bool,
    },
    Measured {
        /// The gated response.
        response: Response,
        /// The long IR (see [`crate::deconv::long_ir`]), with the harmonics before the peak.
        ir: ImpulseResponse,
        /// The sweep's length, for [`crate::distortion::analyse`].
        sweep_seconds: f64,
        drift_ppm: f64,
        clipped: bool,
    },
    MissingEnd,
    Failed(String),
    BestRatio(f32),
    Rta {
        response: Response,
        frames: usize,
    },
    /// The RTA average was cleared by `ResetRta`; no `Rta` event follows until it has data again.
    RtaReset,
}

/// One ordered stream of commands and capture blocks, so a command sent before a block
/// is always applied before that block.
enum Msg {
    Cmd(SessionCommand),
    Samples(Vec<f32>),
}

pub struct Session {
    tx: Sender<Msg>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Session {
    pub fn start(samples: Receiver<Vec<f32>>, events: EventSink<SessionEvent>) -> Session {
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));

        // Forwarder: merges capture blocks into the ordered message stream.
        let fwd_tx = tx.clone();
        let fwd_stop = stop.clone();
        std::thread::Builder::new()
            .name("fastgraph-session-fwd".into())
            .spawn(move || {
                while !fwd_stop.load(Ordering::Relaxed) {
                    match samples.recv_timeout(POLL) {
                        Ok(block) => {
                            if fwd_tx.send(Msg::Samples(block)).is_err() {
                                return;
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            })
            .expect("spawn session forwarder");

        let worker_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("fastgraph-session".into())
            .spawn(move || Worker::new(events).run(rx, worker_stop))
            .expect("spawn session thread");
        Session {
            tx,
            stop,
            thread: Some(thread),
        }
    }

    pub fn send(&self, cmd: SessionCommand) {
        let _ = self.tx.send(Msg::Cmd(cmd));
    }

    /// Stop the worker and wait for it to finish (it may first finish a capture in progress).
    pub fn shutdown(mut self) {
        self.signal();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    fn signal(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.tx.send(Msg::Cmd(SessionCommand::Shutdown));
    }
}

impl Drop for Session {
    /// Signals the worker and detaches; never blocks the caller (use [`Session::shutdown`] to join).
    fn drop(&mut self) {
        self.signal();
    }
}

struct Worker {
    events: EventSink<SessionEvent>,
    rate: Option<u32>,
    mode: Mode,
    detector: Option<Detector>,
    rta: Option<Rta>,
    peak: f32,
    clipped: bool,
    last_level: Instant,
    last_best: Instant,
    last_rta: Option<Instant>,
    last_frames: usize,
}

impl Worker {
    fn new(events: EventSink<SessionEvent>) -> Self {
        let now = Instant::now();
        Worker {
            events,
            rate: None,
            mode: Mode::Idle,
            detector: None,
            rta: None,
            peak: 0.0,
            clipped: false,
            last_level: now,
            last_best: now,
            last_rta: None,
            last_frames: 0,
        }
    }

    fn run(mut self, rx: Receiver<Msg>, stop: Arc<AtomicBool>) {
        while !stop.load(Ordering::Relaxed) {
            match rx.recv_timeout(POLL) {
                Ok(Msg::Cmd(SessionCommand::Shutdown)) => return,
                Ok(Msg::Cmd(cmd)) => self.command(cmd),
                Ok(Msg::Samples(block)) => self.block(&block),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            self.tick();
        }
    }

    fn command(&mut self, cmd: SessionCommand) {
        match cmd {
            SessionCommand::Rate(r) => {
                if self.rate != Some(r) {
                    self.rate = Some(r);
                    self.rebuild();
                }
            }
            SessionCommand::SetMode(m) => {
                if self.mode != m {
                    self.mode = m;
                    self.rebuild();
                }
            }
            SessionCommand::ResetRta => {
                if let Some(rta) = &mut self.rta {
                    rta.reset();
                    self.last_frames = 0;
                    self.last_rta = None;
                    (self.events)(SessionEvent::RtaReset);
                }
            }
            SessionCommand::Reset => {
                self.rebuild();
                if self.rta.is_some() {
                    (self.events)(SessionEvent::RtaReset);
                }
            }
            SessionCommand::Shutdown => {}
        }
    }

    fn rebuild(&mut self) {
        self.detector = None;
        self.rta = None;
        self.last_rta = None;
        self.last_frames = 0;
        self.last_best = Instant::now();
        let Some(rate) = self.rate else { return };
        match self.mode {
            Mode::Idle => {}
            Mode::Sweep(_) if rate < MIN_SWEEP_RATE => (self.events)(SessionEvent::Failed(
                "Sweeps need a capture rate of at least 44.1 kHz; use RTA".into(),
            )),
            Mode::Sweep(length) => self.detector = Some(Detector::new(length, rate)),
            Mode::Rta => self.rta = Some(Rta::new(rate)),
        }
    }

    fn block(&mut self, block: &[f32]) {
        for &x in block {
            let a = x.abs();
            if a > self.peak {
                self.peak = a;
            }
            if a >= CLIP_LEVEL {
                self.clipped = true;
            }
        }
        // A panic in the analysis must not end the worker: report it and
        // start the detector or the RTA afresh.
        if let Some(detector) = &mut self.detector {
            match catch_unwind(AssertUnwindSafe(|| {
                fault_hook(block);
                detector.push(block)
            })) {
                Ok(events) => {
                    for event in events {
                        let out = match event {
                            DetectEvent::MissingEnd => SessionEvent::MissingEnd,
                            DetectEvent::Captured(c) => measure(&c),
                        };
                        (self.events)(out);
                    }
                }
                Err(p) => {
                    (self.events)(SessionEvent::Failed(internal_error(p)));
                    self.rebuild();
                    return;
                }
            }
        }
        if let Some(rta) = &mut self.rta
            && let Err(p) = catch_unwind(AssertUnwindSafe(|| {
                fault_hook(block);
                rta.push(block)
            }))
        {
            (self.events)(SessionEvent::Failed(internal_error(p)));
            self.rebuild();
            (self.events)(SessionEvent::RtaReset);
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_level) >= LEVEL_EVERY {
            self.last_level = now;
            let peak_dbfs = if self.peak > 0.0 {
                (20.0 * self.peak.log10()).max(FLOOR_DBFS)
            } else {
                FLOOR_DBFS
            };
            let clipped = self.clipped;
            self.peak = 0.0;
            self.clipped = false;
            (self.events)(SessionEvent::Level { peak_dbfs, clipped });
        }
        if let Some(detector) = &self.detector
            && now.duration_since(self.last_best) >= BEST_EVERY
        {
            self.last_best = now;
            (self.events)(SessionEvent::BestRatio(detector.best_ratio()));
        }
        if let Some(rta) = &self.rta
            && self
                .last_rta
                .is_none_or(|t| now.duration_since(t) >= RTA_EVERY)
            && rta.frames() != self.last_frames
        {
            let out = match catch_unwind(AssertUnwindSafe(|| rta.response())) {
                Ok(Some(response)) => Some(SessionEvent::Rta {
                    response,
                    frames: rta.frames(),
                }),
                Ok(None) => None,
                Err(p) => Some(SessionEvent::Failed(internal_error(p))),
            };
            // A silent input has no average: don't recompute it until new frames come.
            self.last_frames = rta.frames();
            if let Some(out) = out {
                self.last_rta = Some(now);
                (self.events)(out);
            }
        }
    }
}

/// The first sample of a block that makes the analysis panic, in tests.
#[cfg(test)]
const FAULT_SAMPLE: f32 = f32::MIN;

/// Panics on a block that starts with [`FAULT_SAMPLE`], to test recovery.
#[cfg(test)]
fn fault_hook(block: &[f32]) {
    if block.first() == Some(&FAULT_SAMPLE) {
        panic!("injected fault");
    }
}

#[cfg(not(test))]
fn fault_hook(_: &[f32]) {}

fn internal_error(p: Box<dyn std::any::Any + Send>) -> String {
    let msg = p
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panic".into());
    format!("internal error: {msg}")
}

fn measure(c: &crate::detect::Capture) -> SessionEvent {
    let run = || {
        deconvolve(&c.recording, c.marker_a, c.drift_ratio, c.length, c.rate).map(|raw| {
            let response = response_from_ir(&gate(&raw));
            (response, long_ir(&raw, c.length.seconds()))
        })
    };
    match catch_unwind(AssertUnwindSafe(run)) {
        Ok(Ok((response, ir))) => SessionEvent::Measured {
            response,
            ir,
            sweep_seconds: c.length.seconds(),
            drift_ppm: c.drift_ppm,
            clipped: c.clipped,
        },
        Ok(Err(e)) => SessionEvent::Failed(format!("{e:#}")),
        Err(p) => SessionEvent::Failed(internal_error(p)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::{pink_noise, sweep_stimulus};
    use std::sync::Mutex;

    type Store = Arc<Mutex<Vec<SessionEvent>>>;

    fn collector() -> (EventSink<SessionEvent>, Store) {
        let store: Store = Arc::new(Mutex::new(Vec::new()));
        let s = store.clone();
        (Box::new(move |e| s.lock().unwrap().push(e)), store)
    }

    fn snapshot(store: &Store) -> Vec<SessionEvent> {
        store.lock().unwrap().clone()
    }

    fn wait_for(store: &Store, secs: u64, f: impl Fn(&SessionEvent) -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if snapshot(store).iter().any(&f) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn is_level(e: &SessionEvent) -> bool {
        matches!(e, SessionEvent::Level { .. })
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn measures_from_stream() {
        let rate = 48_000;
        let (sink, store) = collector();
        let (tx, rx) = mpsc::channel();
        let session = Session::start(rx, sink);
        session.send(SessionCommand::Rate(rate));
        session.send(SessionCommand::SetMode(Mode::Sweep(SweepLength::Short)));
        assert!(wait_for(&store, 5, is_level), "worker not running");

        let noise = pink_noise(rate as usize * 2, rate, 1);
        let tail = pink_noise(rate as usize, rate, 2);
        let mut stream: Vec<f32> = noise.iter().map(|x| x * 0.01).collect();
        stream.extend(sweep_stimulus(SweepLength::Short, rate).samples);
        stream.extend(tail.iter().map(|x| x * 0.01));
        for block in stream.chunks(1024) {
            tx.send(block.to_vec()).unwrap();
        }
        assert!(
            wait_for(&store, 10, |e| matches!(e, SessionEvent::Measured { .. })),
            "no Measured event"
        );
        let events = snapshot(&store);
        assert!(events.iter().any(is_level));
        let Some(SessionEvent::Measured { response, .. }) = events
            .iter()
            .find(|e| matches!(e, SessionEvent::Measured { .. }))
        else {
            unreachable!()
        };
        let aligned = response.aligned(1000.0);
        for (f, db) in aligned.freq.iter().zip(&aligned.db) {
            if (30.0..=16_000.0).contains(f) {
                assert!(db.abs() <= 0.5, "{f} Hz: {db} dB");
            }
        }
        session.shutdown();
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn rta_updates_are_throttled() {
        let rate = 48_000;
        let (sink, store) = collector();
        let (tx, rx) = mpsc::channel();
        let session = Session::start(rx, sink);
        session.send(SessionCommand::Rate(rate));
        session.send(SessionCommand::SetMode(Mode::Rta));
        assert!(wait_for(&store, 5, is_level), "worker not running");

        let noise = pink_noise(rate as usize * 10, rate, 3);
        for block in noise.chunks(1024) {
            tx.send(block.iter().map(|x| x * 0.1).collect()).unwrap();
        }
        // Wait until the worker has consumed everything: the channel stays quiet.
        let mut seen = (0, Instant::now());
        let stamps = Arc::new(Mutex::new(Vec::<Instant>::new()));
        let end = Instant::now() + Duration::from_secs(60);
        loop {
            std::thread::sleep(Duration::from_millis(100));
            let n = snapshot(&store)
                .iter()
                .filter(|e| matches!(e, SessionEvent::Rta { .. }))
                .count();
            if n != seen.0 {
                seen = (n, Instant::now());
                stamps.lock().unwrap().push(seen.1);
            } else if n > 0 && seen.1.elapsed() > Duration::from_millis(600) {
                break;
            }
            assert!(Instant::now() < end, "worker did not drain");
        }
        let rta: Vec<usize> = snapshot(&store)
            .iter()
            .filter_map(|e| {
                if let SessionEvent::Rta { frames, .. } = e {
                    Some(*frames)
                } else {
                    None
                }
            })
            .collect();
        assert!(rta.len() >= 2, "only {} Rta events", rta.len());
        assert!(rta.windows(2).all(|w| w[0] <= w[1]), "frames decreased");
        // Last event reflects (nearly) all frames: 10 s of noise at 65 536 / 32 768 hop.
        let expected = (rate as usize * 10 - 65_536) / 32_768 + 1;
        assert!(
            *rta.last().unwrap() + 1 >= expected,
            "last frames {} < {expected}",
            rta.last().unwrap()
        );
        // Rate bound over the span between first and last observed events (polling granularity 100 ms).
        let st = stamps.lock().unwrap();
        let span = st.last().unwrap().duration_since(st[0]).as_secs_f64() + 0.2;
        assert!(
            rta.len() as f64 <= 5.0 * span + 2.0,
            "{} events in {span} s",
            rta.len()
        );
        session.shutdown();
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn drop_does_not_hang() {
        let counter = Arc::new(());
        let held = counter.clone();
        let store: Store = Arc::new(Mutex::new(Vec::new()));
        let s = store.clone();
        let sink: EventSink<SessionEvent> = Box::new(move |e| {
            let _keep = &held;
            s.lock().unwrap().push(e);
        });
        let (tx, rx) = mpsc::channel::<Vec<f32>>();
        let session = Session::start(rx, sink);
        session.send(SessionCommand::Rate(48_000));
        session.send(SessionCommand::SetMode(Mode::Rta));
        assert!(wait_for(&store, 5, is_level), "worker not running");
        for _ in 0..2000 {
            tx.send(vec![0.1; 1024]).unwrap();
        }
        let t0 = Instant::now();
        drop(session);
        assert!(
            t0.elapsed() < Duration::from_millis(50),
            "drop blocked {:?}",
            t0.elapsed()
        );
        // The worker (which owns the sink) must exit promptly although `tx` is alive.
        let end = Instant::now() + Duration::from_secs(1);
        while Arc::strong_count(&counter) > 1 && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(Arc::strong_count(&counter), 1, "worker still running");
        drop(tx);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn shutdown_joins() {
        for drop_sender_first in [false, true] {
            let counter = Arc::new(());
            let held = counter.clone();
            let sink: EventSink<SessionEvent> = Box::new(move |_| {
                let _keep = &held;
            });
            let (tx, rx) = mpsc::channel::<Vec<f32>>();
            let session = Session::start(rx, sink);
            session.send(SessionCommand::Rate(48_000));
            tx.send(vec![0.0; 1024]).unwrap();
            if drop_sender_first {
                drop(tx);
                std::thread::sleep(Duration::from_millis(120));
                session.shutdown();
            } else {
                session.shutdown();
                drop(tx);
            }
            // Joined: the worker's sink is already gone.
            assert_eq!(Arc::strong_count(&counter), 1);
        }
    }

    fn is_internal_failure(e: &SessionEvent) -> bool {
        matches!(e, SessionEvent::Failed(why) if why.starts_with("internal error: injected fault"))
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn detector_panic_is_reported_and_measuring_goes_on() {
        let rate = 48_000;
        let (sink, store) = collector();
        let (tx, rx) = mpsc::channel();
        let session = Session::start(rx, sink);
        session.send(SessionCommand::Rate(rate));
        session.send(SessionCommand::SetMode(Mode::Sweep(SweepLength::Short)));
        assert!(wait_for(&store, 5, is_level), "worker not running");
        tx.send(vec![FAULT_SAMPLE; 64]).unwrap();
        assert!(
            wait_for(&store, 5, is_internal_failure),
            "no Failed event: {:?}",
            snapshot(&store)
        );
        // The worker lives on with a fresh detector.
        let mut stream: Vec<f32> = pink_noise(rate as usize, rate, 5)
            .iter()
            .map(|x| x * 0.01)
            .collect();
        stream.extend(sweep_stimulus(SweepLength::Short, rate).samples);
        stream.extend(vec![0.0; rate as usize]);
        for block in stream.chunks(1024) {
            tx.send(block.to_vec()).unwrap();
        }
        assert!(
            wait_for(&store, 10, |e| matches!(e, SessionEvent::Measured { .. })),
            "no Measured event after the panic"
        );
        session.shutdown();
    }

    // Bugs caught: 0, False positives: 1
    #[test]
    fn rta_panic_is_reported_and_the_average_restarts() {
        let rate = 48_000;
        let (sink, store) = collector();
        let (tx, rx) = mpsc::channel();
        let session = Session::start(rx, sink);
        session.send(SessionCommand::Rate(rate));
        session.send(SessionCommand::SetMode(Mode::Rta));
        assert!(wait_for(&store, 5, is_level), "worker not running");
        tx.send(vec![FAULT_SAMPLE; 64]).unwrap();
        assert!(wait_for(&store, 5, is_internal_failure), "no Failed event");
        assert!(wait_for(&store, 5, |e| matches!(e, SessionEvent::RtaReset)));
        for block in pink_noise(rate as usize * 5, rate, 6).chunks(1024) {
            tx.send(block.to_vec()).unwrap();
        }
        assert!(
            wait_for(&store, 10, |e| matches!(e, SessionEvent::Rta { .. })),
            "no Rta event after the panic"
        );
        session.shutdown();
    }

    // Bugs caught: 0, False positives: 1
    #[test]
    fn reset_rta_notifies_and_same_mode_keeps_state() {
        let rate = 48_000;
        let (sink, store) = collector();
        let (tx, rx) = mpsc::channel();
        let session = Session::start(rx, sink);
        session.send(SessionCommand::Rate(rate));
        session.send(SessionCommand::SetMode(Mode::Rta));
        for block in pink_noise(rate as usize * 5, rate, 4).chunks(1024) {
            tx.send(block.to_vec()).unwrap();
        }
        assert!(wait_for(&store, 10, |e| matches!(
            e,
            SessionEvent::Rta { .. }
        )));
        // Re-sending identical settings must not clear the average.
        session.send(SessionCommand::Rate(rate));
        session.send(SessionCommand::SetMode(Mode::Rta));
        session.send(SessionCommand::ResetRta);
        assert!(wait_for(&store, 5, |e| matches!(e, SessionEvent::RtaReset)));
        session.shutdown();
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn reset_starts_afresh_with_the_same_settings() {
        let rate = 48_000;
        let (sink, store) = collector();
        let (tx, rx) = mpsc::channel();
        let session = Session::start(rx, sink);
        session.send(SessionCommand::Rate(rate));
        session.send(SessionCommand::SetMode(Mode::Sweep(SweepLength::Short)));
        // Marker A and the start of a sweep from one stream...
        let stim = sweep_stimulus(SweepLength::Short, rate);
        let mut first: Vec<f32> = vec![0.0; rate as usize];
        first.extend(&stim.samples[..stim.sweep_start + stim.sweep_len / 2]);
        for block in first.chunks(1024) {
            tx.send(block.to_vec()).unwrap();
        }
        // ...then a new stream at the same rate: only its own audio counts.
        session.send(SessionCommand::Rate(rate));
        session.send(SessionCommand::Reset);
        let mut second: Vec<f32> = stim.samples[stim.sweep_start + stim.sweep_len / 2..].to_vec();
        second.extend(vec![0.0; rate as usize * 3]);
        for block in second.chunks(1024) {
            tx.send(block.to_vec()).unwrap();
        }
        assert!(wait_for(&store, 5, is_level));
        std::thread::sleep(Duration::from_millis(500));
        let events = snapshot(&store);
        assert!(
            !events.iter().any(|e| matches!(
                e,
                SessionEvent::Measured { .. } | SessionEvent::MissingEnd | SessionEvent::Failed(_)
            )),
            "a result joined the two streams"
        );
        // In RTA mode a Reset clears the average and says so.
        session.send(SessionCommand::SetMode(Mode::Rta));
        session.send(SessionCommand::Reset);
        assert!(wait_for(&store, 5, |e| matches!(e, SessionEvent::RtaReset)));
        session.shutdown();
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn sweeps_are_refused_below_44k1_but_rta_runs() {
        let rate = 32_000;
        let (sink, store) = collector();
        let (tx, rx) = mpsc::channel();
        let session = Session::start(rx, sink);
        session.send(SessionCommand::Rate(rate));
        session.send(SessionCommand::SetMode(Mode::Sweep(SweepLength::Short)));
        assert!(wait_for(&store, 5, |e| matches!(
            e,
            SessionEvent::Failed(why) if why.contains("at least 44.1 kHz")
        )));
        session.send(SessionCommand::SetMode(Mode::Rta));
        for block in pink_noise(rate as usize * 10, rate, 7).chunks(1024) {
            tx.send(block.to_vec()).unwrap();
        }
        assert!(wait_for(&store, 10, |e| matches!(
            e,
            SessionEvent::Rta { .. }
        )));
        session.shutdown();
    }
}
