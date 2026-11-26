//! The app: its state, messages, update and subscriptions.
//!
//! The audio engine and the analysis session report on their own threads.
//! Both push their events into one unbounded channel as [`Message`]s
//! ([`forward`]), and [`App::subscription`] hands the receiving end to iced
//! once, so every event reaches [`App::update`] on the UI thread.
use crate::browser::Pane;
use crate::devices::Opened;
use crate::settings::Inputs;
pub use crate::status::{Status, StatusLevel};
use crate::store_queue::Worker;
use crate::{browser, devices, export_png, graph, list, rta_mode, settings, tabs, theme, toolbar};
use fastgraph_core::EventSink;
use fastgraph_core::audio::{AudioCommand, AudioEvent, Engine, Node};
use fastgraph_core::deconv::ImpulseResponse;
use fastgraph_core::prefs::Prefs;
use fastgraph_core::response::{Response, Smoothing};
use fastgraph_core::session::{Mode, Session, SessionCommand, SessionEvent};
use fastgraph_core::set::{self, Entry, SetHeader};
use fastgraph_core::signal::{SweepLength, sweep_file_name, sweep_stimulus, write_wav};
use fastgraph_core::store::{Kind, Measurement, Meta, Side, Store};
use iced::futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use iced::futures::{Stream, StreamExt, stream};
use iced::widget::{Space, column, container, responsive, row, rule, stack};
use iced::{Element, Length, Subscription, Task};
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Room between the toolbar and the graph.
const GRAPH_MARGIN: f32 = 10.0;
/// The rate the sweep is rendered and played at.
pub(crate) const PLAY_RATE: u32 = 48_000;
/// The sweep file's peak, which the level slider's dBFS refer to.
pub(crate) const SWEEP_PEAK: f64 = 0.5;
/// How long after playback ends a Measure waits for the detection.
const MEASURE_TIMEOUT: Duration = Duration::from_secs(3);
/// How much longer than its own length, the engine's start timeout and
/// [`MEASURE_TIMEOUT`] a sweep's playback may take to finish (buffering)
/// before Measure gives up.
const PLAYBACK_SLACK: Duration = Duration::from_secs(2);
/// How long the meter stays red after a clipped block.
const CLIP_HOLD: Duration = Duration::from_secs(1);
/// The [`Message::Tick`] period while measuring.
const TICK: Duration = Duration::from_millis(100);
/// The meter's reading before any level arrives.
const FLOOR_DBFS: f32 = -120.0;
/// The graph's tab strip's width: three tabs of 110 px.
const GRAPH_TABS_WIDTH: f32 = 330.0;
/// How many earlier sessions' recovery files are kept regardless of age.
const KEEP_RECOVERY: usize = 5;
/// The boot note after the old store was packed into a recovery set.
pub(crate) const MIGRATED: &str = "Earlier measurements are in Sets › Unsaved sessions";

pub struct App {
    pub(crate) prefs: Prefs,
    /// False when the preferences file could not be read at start: it is
    /// then never overwritten.
    pub(crate) prefs_writable: bool,
    /// Every audio source and sink.
    pub(crate) nodes: Vec<Node>,
    pub(crate) engine: Option<Engine>,
    pub(crate) session: Option<Session>,
    /// Runs file work (recovery writes, the boot migration, saves, exports
    /// and folder listing) in order, off the UI thread.
    pub(crate) worker: Worker,
    /// This run's recovery file, rewritten after every change to the
    /// session; `None` when there is nowhere to keep it.
    pub(crate) recovery: Option<PathBuf>,
    /// Why there is no recovery file.
    pub(crate) recovery_error: Option<String>,
    /// The newest queued recovery write: an older one skips its write.
    recovery_generation: Arc<AtomicU64>,
    /// The last recovery write failed and no save has succeeded since: the
    /// session may exist only in memory.
    recovery_failed: Arc<AtomicBool>,
    /// The session's measurements.
    pub(crate) items: Vec<Item>,
    /// The open set's name; empty for a new session.
    pub(crate) set_name: String,
    /// The left pane's tab.
    pub(crate) pane: Pane,
    /// The Settings tab's text boxes, as typed (invalid text stays).
    pub(crate) settings: Inputs,
    /// The theme, rebuilt only when the colours change.
    pub(crate) theme: iced::Theme,
    /// The sets in the save folder, newest first, as last listed.
    pub(crate) sets: Vec<(PathBuf, SetHeader)>,
    /// The recovery files of earlier sessions, newest first.
    pub(crate) unsaved: Vec<(PathBuf, SetHeader)>,
    /// The save folder as the last listing resolved it (`Some(None)`: there
    /// is none); `None` before the first listing.
    pub(crate) sets_folder: Option<Option<PathBuf>>,
    /// When this run's recovery file was named (it rotates a second later
    /// at the soonest, so no two share a name).
    recovery_started: SystemTime,
    /// The open set's file.
    pub(crate) saved_as: Option<PathBuf>,
    /// The session changed since it was opened (or saved).
    pub(crate) dirty: bool,
    pub(crate) status: Status,
    pub(crate) state: Activity,
    pub(crate) side: Side,
    /// The input peak in dBFS, and whether the meter shows a clip.
    pub(crate) level: (f32, bool),
    /// When the input last clipped.
    pub(crate) clip_at: Option<Instant>,
    /// The loudest marker the detector has seen, as a multiple of its noise.
    pub(crate) best_ratio: f32,
    /// The open input's rate, once its stream has started.
    pub(crate) capture_rate: Option<u32>,
    /// The microphone compensation curve.
    pub(crate) comp: Option<(PathBuf, Response)>,
    /// The start-up compensation reload is pending: its failure joins the
    /// boot notes.
    pub(crate) comp_boot_reload: bool,
    /// The graph's tab.
    pub(crate) graph_view: graph::View,
    /// The tab pills' positions: the left pane's and the graph's.
    pub(crate) pane_slide: iced::Animation<f32>,
    pub(crate) graph_slide: iced::Animation<f32>,
    /// The time the tab pills are drawn at: the last frame's, or the last
    /// tab click's.
    pub(crate) now: Instant,
    /// The pointer over the graph: frequency and level.
    pub(crate) hover: Option<(f64, f64)>,
    /// The item whose row was clicked: the Distortion view shows it.
    pub(crate) selected: Option<String>,
    /// The distortion of the items that have it, by id.
    pub(crate) distortion:
        std::collections::HashMap<String, fastgraph_core::distortion::Distortion>,
    /// The Distortion view's units (not kept in the preferences).
    pub(crate) units: graph::Units,
    /// The item being renamed and its new name.
    pub(crate) renaming: Option<(String, String)>,
    /// The offset box text of items being edited, by id.
    pub(crate) offset_texts: std::collections::HashMap<String, String>,
    /// The recovery generation when the running save was queued: a change
    /// since then leaves the session unsaved.
    saving_generation: u64,
    /// A Save, Open or Import of a set runs, from its click to its end:
    /// another one waits.
    pub(crate) busy: bool,
    /// The set's name when Save was clicked.
    save_name: Option<String>,
    /// How many times Clear emptied the session: a curve import clicked
    /// before the last one is dropped.
    pub(crate) clears: u64,
    /// The PipeWire connection failed: the device pane offers Reconnect.
    pub(crate) offline: bool,
    /// The Mode segment: Sweep (true) or RTA.
    pub(crate) sweep_mode: bool,
    /// The input stream asked for, so a device-list update reopens it only
    /// when it changed.
    pub(crate) opened: Option<Opened>,
    /// The bridge's sending end, for the session and engines
    /// started later (Reconnect).
    bridge: UnboundedSender<Message>,
    /// The bridge's receiving end, until the subscription takes it.
    events: Events,
    /// Capture blocks from the engine to the session.
    samples: Option<mpsc::Sender<Vec<f32>>>,
    /// The last sweep rendered for playback.
    stimulus: Option<(SweepLength, Arc<Vec<f32>>)>,
    /// The sink the running sweep was sent to and the side it played;
    /// `None` for Listen.
    pub(crate) sweep_output: Option<(String, Side)>,
    /// While measuring: when the sweep's playback should have finished
    /// (sent + the engine's start timeout + its length + [`MEASURE_TIMEOUT`]
    /// + [`PLAYBACK_SLACK`]); `None` until the sweep is sent, while it renders.
    pub(crate) play_deadline: Option<Instant>,
    pub(crate) rta: rta_mode::RtaState,
    /// The commands sent to the engine and the session, for tests.
    #[cfg(test)]
    pub(crate) sent: std::cell::RefCell<Vec<rta_mode::Sent>>,
    pub(crate) graph: graph::Graph,
    /// The Export PNG panel, while it is open.
    pub(crate) png: Option<export_png::Options>,
}

pub struct Item {
    pub m: Measurement,
    pub shown: bool,
    pub color: usize,
    /// A sweep's impulse response, kept for the set it is saved in.
    pub ir: Option<Arc<ImpulseResponse>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Activity {
    Idle,
    /// The sweep plays; `played` is when its playback finished.
    Measuring {
        played: Option<Instant>,
    },
    /// Waiting for external sweeps.
    Listening,
    /// RTA: the noise plays here (`playing`) or elsewhere.
    Rta {
        playing: bool,
    },
}

#[derive(Debug, Clone)]
pub enum Message {
    Audio(AudioEvent),
    Session(SessionEvent),
    Tick,
    /// An input's [`Input::key`].
    InputPicked(String),
    /// A channel label of the input.
    InputChannelPicked(String),
    SidePicked(Side),
    /// A sink's `node.name`.
    OutputPicked(String),
    /// The playback level: the sweep's peak in dBFS.
    Level(f64),
    /// The level slider was let go: save the level.
    LevelReleased,
    LengthPicked(SweepLength),
    ModeSweep(bool),
    Measure,
    /// Abort a Measure.
    Stop,
    Listen(bool),
    /// Start (true) or stop the noise played here.
    RtaNoise(bool),
    /// Listen for noise played elsewhere (true), or stop.
    RtaListen(bool),
    RtaCapture,
    RtaReset,
    /// The noise rendered off the UI thread.
    NoiseRendered(Rendered),
    ExportStimulus,
    StimulusSaved(Result<PathBuf, String>),
    /// A sweep rendered off the UI thread for playback.
    StimulusRendered(SweepLength, Rendered),
    /// An item's id, and whether the graph shows it.
    Shown(String, bool),
    /// The pointer over the graph, the wheel over its axes, or Reset view.
    Graph(graph::Event),
    /// A tab over the graph.
    GraphViewSelected(graph::View),
    /// The Distortion view's units.
    UnitsPicked(graph::Units),
    /// A row's name was clicked: select that item.
    Select(String),
    /// An item's distortion, analysed on the worker.
    /// The id, the impulse response analysed (a late result for another one is dropped),
    /// and the analysis.
    DistortionReady(
        String,
        std::sync::Weak<ImpulseResponse>,
        Box<fastgraph_core::distortion::Distortion>,
    ),
    /// A worker job panicked, with the panic's text.
    WorkerPanicked(String),
    /// Start a new audio engine after the connection failed.
    Reconnect,
    SmoothingPicked(Smoothing),
    /// Open the compensation curve picker.
    LoadCompensation,
    CompensationLoaded(Result<(PathBuf, Response), String>),
    CompensationEnabled(bool),
    CompensationInvert(bool),
    /// Align the traces at the chosen frequency.
    Align(bool),
    /// The Align frequency box was typed in.
    AlignHzInput(String),
    /// Enter in the Align frequency box.
    AlignHzSubmit,
    /// An Align band width, in octaves, was picked.
    AlignWidthPicked(f64),
    Import,
    /// The curves read for an Import clicked after `n` Clears.
    Imported(u64, Result<Vec<(PathBuf, Response)>, String>),
    /// Export the item with this id.
    Export(String),
    Exported(Result<PathBuf, String>),
    /// The Export PNG button and panel.
    Png(export_png::Event),
    AverageShown,
    /// Start renaming the item with this id.
    RenameStart(String),
    RenameInput(String),
    RenameCommit,
    RenameCancel,
    Delete(String),
    /// The Clear button under the list: back to the first-opened state.
    Clear,
    /// Shift the trace with this id by this many dB (a button press).
    OffsetStep(String, f64),
    /// The offset box of this id was typed in.
    OffsetInput(String, String),
    /// Enter in the offset box of this id.
    OffsetSubmit(String),
    /// Replace the session with the set in this file.
    OpenSet(PathBuf),
    /// Add the measurements of the set in this file to the session.
    ImportSet(PathBuf),
    /// A set was read for [`Message::OpenSet`] (true: replace) or
    /// [`Message::ImportSet`] (false), or not; with the problems of the
    /// measurements that could not be read.
    SetOpened(
        Result<(PathBuf, SetHeader, Vec<Entry>, Vec<String>), String>,
        bool,
    ),
    /// A tab of the left pane.
    PaneSelected(Pane),
    /// The folder listings for the Sets tab: the save folder's sets, and
    /// the recovery folder's other sessions.
    /// The third is the save folder, `None` if there is none.
    SetsListed(
        Vec<(PathBuf, SetHeader)>,
        Vec<(PathBuf, SetHeader)>,
        Option<PathBuf>,
    ),
    /// The Set name box.
    SetNameInput(String),
    /// The Settings tab's folder boxes: typing, Enter, Browse… and the
    /// picker's answer, and the "ask" box.
    SetsDirInput(String),
    SetsDirSubmit,
    BrowseSetsDir,
    SetsDirPicked(Option<PathBuf>),
    AskSave(bool),
    ExportsDirInput(String),
    ExportsDirSubmit,
    BrowseExportsDir,
    ExportsDirPicked(Option<PathBuf>),
    AskExport(bool),
    /// The Appearance segments.
    ThemePicked(theme::ThemeMode),
    /// The accent box, and back to the default.
    AccentInput(String),
    AccentSubmit,
    AccentReset,
    /// Trace colour box `index`, and all of them back to the default.
    TraceColorInput(usize, String),
    TraceColorSubmit(usize),
    TraceColorsReset,
    /// The Save button.
    SaveSet,
    /// The save dialog's answer: the file to write, or `None` if cancelled.
    SaveSetTo(Option<PathBuf>),
    /// Whether the file Save would write exists (answered off the UI thread).
    SaveChecked(PathBuf, bool),
    /// A save finished: the file written, or why not.
    SetSaved(Result<PathBuf, String>),
    /// The start-up file work's notes, which join the boot notes.
    BootNotes(Vec<(String, StatusLevel)>),
    /// The recovery file could not be written.
    RecoveryFailed(String),
    /// The window's close button: finish the worker's writes, then quit.
    CloseRequested,
    /// The first frame was drawn; quit if `FASTGRAPH_EXIT_AFTER_FIRST_FRAME` is set.
    FirstFrame,
    /// A frame while a tab pill slides, at its time.
    Frame(Instant),
}

/// Rendered sweep samples; their debug form is just their length.
#[derive(Clone)]
pub struct Rendered(pub Arc<Vec<f32>>);

impl std::fmt::Debug for Rendered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Rendered({} samples)", self.0.len())
    }
}

/// Render the sweep for `length` on the executor, not the UI thread.
fn render(length: SweepLength) -> Task<Message> {
    Task::perform(
        async move { Rendered(Arc::new(sweep_stimulus(length, PLAY_RATE).samples)) },
        move |samples| Message::StimulusRendered(length, samples),
    )
}

/// The bridge's receiving end, which the subscription takes once.
#[derive(Clone, Default)]
struct Events(Arc<Mutex<Option<UnboundedReceiver<Message>>>>);

impl Hash for Events {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}

/// An event sink that wraps each event in a [`Message`] and sends it over
/// the bridge.
fn forward<E: Send + 'static>(
    bridge: &UnboundedSender<Message>,
    wrap: fn(E) -> Message,
) -> EventSink<E> {
    let bridge = bridge.clone();
    Box::new(move |event| {
        let _ = bridge.unbounded_send(wrap(event));
    })
}

/// [`Message::Tick`] every [`TICK`] from a thread that ends when the
/// subscription is dropped.
fn ticks() -> impl Stream<Item = Message> {
    let (tx, rx) = unbounded();
    let _ = std::thread::Builder::new()
        .name("fastgraph-tick".into())
        .spawn(move || {
            loop {
                std::thread::sleep(TICK);
                if tx.unbounded_send(Message::Tick).is_err() {
                    return;
                }
            }
        });
    rx
}

/// Whether the file Save would write exists, off the UI thread (the folder
/// may be slow), for [`Message::SaveChecked`].
fn check_save(path: PathBuf) -> Task<Message> {
    Task::future(async move {
        let exists = path.exists();
        Message::SaveChecked(path, exists)
    })
}

/// Read the set in `path` off the UI thread, for [`Message::SetOpened`].
fn read_set(path: PathBuf, replace: bool) -> Task<Message> {
    Task::perform(
        async move {
            set::read_set(&path)
                .map(|(header, entries, problems)| (path, header, entries, problems))
                .map_err(|e| format!("{e:#}"))
        },
        move |opened| Message::SetOpened(opened, replace),
    )
}

/// The start-up file work, on the worker: pack the old per-file store into a
/// recovery set, then drop old recovery files. Its notes
/// join the boot notes.
fn tidy_files(old: &Path, recovery: &Path) -> Option<Message> {
    let mut notes = Vec::new();
    match set::migrate_old_store(old, recovery) {
        Ok(None) => {}
        Ok(Some((_, problems))) => {
            notes.push((MIGRATED.to_string(), StatusLevel::Info));
            if !problems.is_empty() {
                notes.push((
                    format!(
                        "{} earlier measurements unreadable: {}",
                        problems.len(),
                        problems.join("; ")
                    ),
                    StatusLevel::Warn,
                ));
            }
        }
        Err(e) => notes.push((
            format!("Earlier measurements not moved: {e:#}"),
            StatusLevel::Warn,
        )),
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    if let Err(e) = set::prune_recovery(recovery, KEEP_RECOVERY, now) {
        notes.push((
            format!("Old recovery files not deleted: {e:#}"),
            StatusLevel::Warn,
        ));
    }
    (!notes.is_empty()).then_some(Message::BootNotes(notes))
}

/// The items of a session as [`App::snapshot`] takes them.
type Snapshot = Vec<(Measurement, Option<Arc<ImpulseResponse>>)>;

fn into_entries(snapshot: Snapshot) -> Vec<Entry> {
    snapshot
        .into_iter()
        .map(|(m, ir)| Entry {
            meta: m.meta,
            response: m.response,
            ir: ir.map(Arc::unwrap_or_clone),
        })
        .collect()
}

/// `path` with `.fgset` added unless it already ends in it; "v1.2" is
/// "v1.2.fgset", not "v1.fgset".
fn with_extension(path: PathBuf) -> PathBuf {
    if path.extension().is_none_or(|x| x != set::EXTENSION) {
        let mut name = path.into_os_string();
        name.push(format!(".{}", set::EXTENSION));
        return PathBuf::from(name);
    }
    path
}

/// `name` without a trailing ".fgset" (any case) and surrounding blanks.
fn strip_extension(name: &str) -> &str {
    let name = name.trim();
    match name.rsplit_once('.') {
        Some((stem, ext)) if ext.eq_ignore_ascii_case(set::EXTENSION) => stem.trim_end(),
        _ => name,
    }
}

/// Write the session to its recovery file, or remove the file when the
/// session is empty.
fn write_recovery(path: &Path, name: &str, entries: &[Entry]) -> Result<(), String> {
    if entries.is_empty() {
        return match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(format!("deleting {}: {e}", path.display()))
            }
            _ => Ok(()),
        };
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    set::write_set(path, name, entries).map_err(|e| format!("{e:#}"))
}

pub fn side_label(side: Side) -> &'static str {
    match side {
        Side::L => "L",
        Side::R => "R",
    }
}

impl App {
    /// Read the preferences, start the worker with its file work, the
    /// session and the PipeWire connection. Nothing is loaded: the session
    /// starts empty.
    pub fn boot() -> (App, Task<Message>) {
        let mut notes = Vec::new();
        let (prefs, prefs_writable) = match Prefs::path().and_then(|path| Prefs::load(&path)) {
            Ok(prefs) => (prefs, true),
            Err(e) => {
                notes.push((
                    format!("Preferences unreadable, changes won't be saved: {e:#}"),
                    StatusLevel::Warn,
                ));
                (Prefs::default(), false)
            }
        };
        let dirs = set::recovery_dir().and_then(|recovery| Ok((recovery, Store::default_dir()?)));
        let (dirs, recovery_error) = match dirs {
            Ok(dirs) => (Some(dirs), None),
            Err(e) => {
                notes.push((format!("Crash recovery is off: {e:#}"), StatusLevel::Warn));
                (None, Some(format!("{e:#}")))
            }
        };
        let mut app = App::new(prefs, dirs);
        app.recovery_error = recovery_error;
        app.prefs_writable = prefs_writable;
        let (samples, capture) = mpsc::channel();
        app.session = Some(Session::start(
            capture,
            forward(&app.bridge, Message::Session),
        ));
        app.samples = Some(samples);
        if let Err(e) = app.connect() {
            notes.push((e, StatusLevel::Error));
        }
        if let Some(status) = Status::joined(notes) {
            app.status = status;
        }
        let length = app.prefs.length;
        let reload = app.reload_compensation();
        (app, Task::batch([render(length), reload]))
    }

    /// An app with no audio; `dirs` are the recovery directory and the old
    /// store, `None` for no recovery file.
    fn new(prefs: Prefs, dirs: Option<(PathBuf, PathBuf)>) -> App {
        let (bridge, events) = unbounded();
        let worker = {
            let bridge = bridge.clone();
            Worker::start(move |message| {
                let _ = bridge.unbounded_send(message);
            })
        };
        let started = SystemTime::now();
        let recovery = dirs.map(|(recovery, old)| {
            let path = set::recovery_path(&recovery, started, std::process::id());
            worker.submit(Box::new(move || tidy_files(&old, &recovery)));
            path
        });
        let settings = Inputs::new(&prefs);
        let theme = theme::from_prefs(&prefs);
        App {
            prefs,
            prefs_writable: false,
            nodes: Vec::new(),
            engine: None,
            session: None,
            worker,
            recovery,
            recovery_error: None,
            recovery_generation: Arc::default(),
            recovery_failed: Arc::default(),
            items: Vec::new(),
            set_name: String::new(),
            pane: Pane::Measure,
            settings,
            theme,
            sets: Vec::new(),
            unsaved: Vec::new(),
            sets_folder: None,
            recovery_started: started,
            saved_as: None,
            dirty: false,
            status: Status {
                text: String::new(),
                level: StatusLevel::Info,
            },
            state: Activity::Idle,
            side: Side::L,
            level: (FLOOR_DBFS, false),
            clip_at: None,
            best_ratio: 0.0,
            capture_rate: None,
            comp: None,
            comp_boot_reload: false,
            graph_view: graph::View::default(),
            pane_slide: tabs::slide(browser::index(Pane::Measure)),
            graph_slide: tabs::slide(graph::index(graph::View::default())),
            now: Instant::now(),
            selected: None,
            distortion: std::collections::HashMap::new(),
            units: graph::Units::default(),
            hover: None,
            renaming: None,
            offset_texts: std::collections::HashMap::new(),
            saving_generation: 0,
            busy: false,
            save_name: None,
            clears: 0,
            offline: false,
            sweep_mode: true,
            opened: None,
            bridge,
            events: Events(Arc::new(Mutex::new(Some(events)))),
            samples: None,
            stimulus: None,
            sweep_output: None,
            play_deadline: None,
            rta: rta_mode::RtaState::default(),
            #[cfg(test)]
            sent: Default::default(),
            graph: graph::Graph::default(),
            png: None,
        }
    }

    /// An app without audio, that never writes the preferences. `data`
    /// stands for `$XDG_DATA_HOME/fastgraph`: the recovery files go to
    /// `data/recovery` and the old store is `data/measurements`; `None`
    /// keeps no recovery file.
    #[cfg(test)]
    pub fn new_for_test(prefs: Prefs, data: Option<PathBuf>) -> App {
        App::new(
            prefs,
            data.map(|data| (data.join("recovery"), data.join("measurements"))),
        )
    }

    /// Start an engine on the bridge; a failure leaves the app offline and
    /// says why.
    fn connect(&mut self) -> Result<(), String> {
        let Some(samples) = &self.samples else {
            return Ok(());
        };
        match Engine::start(forward(&self.bridge, Message::Audio), samples.clone()) {
            Ok(engine) => {
                self.engine = Some(engine);
                self.offline = false;
                Ok(())
            }
            Err(e) => {
                self.engine = None;
                self.offline = true;
                Err(format!("{e:#}"))
            }
        }
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Audio(event) => self.audio(event),
            Message::Session(event) => return self.analysis(event),
            Message::Tick => {
                match self.state {
                    Activity::Measuring { played: Some(at) } if at.elapsed() >= MEASURE_TIMEOUT => {
                        self.stop();
                        self.error(format!(
                            "Sweep not detected (loudest marker {:.1}×, need 12×)",
                            self.best_ratio
                        ));
                    }
                    // A playback that started and then stalled never finishes.
                    Activity::Measuring { played: None }
                        if self.play_deadline.is_some_and(|at| Instant::now() >= at) =>
                    {
                        self.stop();
                        self.error("Playback did not finish".into());
                    }
                    _ => {}
                }
            }
            Message::InputPicked(key) => {
                let channels = self
                    .inputs()
                    .into_iter()
                    .find(|input| input.key() == key)
                    .map(|input| input.node().channels.clone())
                    .unwrap_or_default();
                let keep = self
                    .prefs
                    .input_channel
                    .as_ref()
                    .is_some_and(|label| channels.contains(label));
                if !keep {
                    self.prefs.input_channel = channels.first().cloned();
                }
                self.prefs.input = Some(key);
                self.save_prefs();
                self.open_input();
                self.rta_input_changed();
            }
            Message::InputChannelPicked(label) => {
                self.prefs.input_channel = Some(label);
                self.save_prefs();
                self.open_input();
                self.rta_input_changed();
            }
            Message::SidePicked(side) => {
                self.side = side;
                self.apply_side_channels();
            }
            Message::OutputPicked(name) => {
                self.prefs.output = Some(name);
                // Derived here, not by `apply_side_channels`, so the noise
                // restarts once.
                self.prefs.output_channels = self
                    .output()
                    .map(|sink| devices::side_channels(&sink.channels, self.side))
                    .unwrap_or_default();
                self.save_prefs();
                self.rta_output_changed(true);
            }
            Message::Level(db) => self.prefs.level_db = db.round().clamp(-40.0, 0.0),
            Message::LevelReleased => {
                self.save_prefs();
                self.rta_output_changed(false);
            }
            Message::LengthPicked(length) => {
                // The sweep being measured keeps its length.
                if matches!(self.state, Activity::Measuring { .. }) {
                    return Task::none();
                }
                self.prefs.length = length;
                self.save_prefs();
                if self.state == Activity::Listening {
                    self.command(SessionCommand::SetMode(Mode::Sweep(length)));
                }
                if self
                    .stimulus
                    .as_ref()
                    .is_none_or(|(rendered, _)| *rendered != length)
                {
                    return render(length);
                }
            }
            Message::ModeSweep(sweep) => self.mode_picked(sweep),
            Message::Measure => return self.measure(),
            Message::Stop => {
                if matches!(self.state, Activity::Measuring { .. }) {
                    self.stop();
                    self.info("Measurement stopped".into());
                }
            }
            Message::StimulusRendered(length, Rendered(samples)) => {
                self.stimulus = Some((length, Arc::clone(&samples)));
                // A Measure waiting for this sweep plays it now.
                if self.state == (Activity::Measuring { played: None })
                    && self.play_deadline.is_none()
                    && length == self.prefs.length
                {
                    self.play(samples);
                }
            }
            Message::Listen(true) => match self.ready_to_start() {
                Ok(()) => {
                    self.arm();
                    self.state = Activity::Listening;
                    self.info("Listening for a sweep…".into());
                }
                Err(why) => self.error(why),
            },
            Message::Listen(false) => {
                if self.state == Activity::Listening {
                    self.stop();
                    self.info("Stopped listening".into());
                }
            }
            Message::RtaNoise(on) => return self.rta_noise(on),
            Message::RtaListen(on) => self.rta_listen(on),
            Message::RtaCapture => return self.rta_capture(),
            Message::RtaReset => self.rta_reset(),
            Message::NoiseRendered(Rendered(noise)) => self.noise_rendered(noise),
            Message::ExportStimulus => {
                return if self.sweep_mode {
                    self.export_stimulus()
                } else {
                    self.export_noise()
                };
            }
            Message::StimulusSaved(Ok(path)) => self.info(format!("Saved {}", path.display())),
            Message::StimulusSaved(Err(e)) => self.error(format!("WAV not saved: {e}")),
            Message::Shown(id, shown) => return self.set_shown(&id, shown),
            Message::Graph(graph::Event::Hover(graph::Hover::At { freq, db })) => {
                self.hover = Some((freq, db));
            }
            Message::Graph(graph::Event::Hover(graph::Hover::Left)) => self.hover = None,
            Message::Graph(graph::Event::Wheel(wheel)) => self.graph.wheel(wheel),
            Message::Graph(graph::Event::Reset) => self.graph.reset_view(),
            Message::Graph(graph::Event::Pan(pan)) => self.graph.pan(pan),
            Message::FirstFrame => return iced::exit(),
            Message::Frame(at) => self.now = at,
            Message::Reconnect => match self.connect() {
                Ok(()) => self.info("Reconnected to PipeWire".into()),
                Err(e) => self.error(e),
            },
            Message::SmoothingPicked(smoothing) => self.smoothing_picked(smoothing),
            Message::LoadCompensation => return self.load_compensation(),
            Message::CompensationLoaded(loaded) => self.compensation_loaded(loaded),
            Message::CompensationEnabled(on) => self.compensation_enabled(on),
            Message::CompensationInvert(on) => self.compensation_invert(on),
            Message::Align(on) => self.align(on),
            Message::AlignHzInput(text) => self.settings.align_hz = text,
            Message::AlignHzSubmit => self.align_hz_submit(),
            Message::AlignWidthPicked(octaves) => self.align_width_picked(octaves),
            Message::Import => return self.import(),
            // Curves picked for the session that Clear ended stay out.
            Message::Imported(clears, _) if clears != self.clears => {}
            Message::Imported(_, curves) => return self.imported(curves),
            Message::Export(id) => return self.export(&id),
            Message::Exported(Ok(path)) => self.info(format!("Exported {}", path.display())),
            Message::Exported(Err(e)) => self.error(format!("Export failed: {e}")),
            Message::Png(event) => return self.png_event(event),
            Message::AverageShown => return self.average_shown(),
            Message::RenameStart(id) => return self.rename_start(&id),
            Message::RenameInput(name) => self.rename_input(name),
            Message::RenameCommit => return self.rename_commit(),
            Message::RenameCancel => self.renaming = None,
            Message::OffsetStep(id, delta) => self.offset_step(&id, delta),
            Message::OffsetInput(id, text) => {
                self.offset_texts.insert(id, text);
            }
            Message::OffsetSubmit(id) => self.offset_submit(&id),
            Message::Delete(id) => return self.delete(&id),
            Message::Clear => self.clear(),
            Message::OpenSet(_) | Message::ImportSet(_) if self.busy => self.wait(),
            Message::OpenSet(path) => {
                self.busy = true;
                return read_set(path, true);
            }
            Message::ImportSet(path) => {
                self.busy = true;
                return read_set(path, false);
            }
            Message::SetOpened(opened, replace) => {
                self.busy = false;
                self.set_opened(opened, replace);
            }
            Message::GraphViewSelected(view) => {
                self.graph_view = view;
                self.now = Instant::now();
                tabs::go(&mut self.graph_slide, graph::index(view), self.now);
                self.graph.set_view(view);
                self.hover = None;
                self.redraw();
                self.png_refill();
            }
            Message::UnitsPicked(units) => {
                // The other units are another axis.
                if units != self.units {
                    self.graph.reset_level(graph::View::Distortion);
                }
                self.units = units;
                self.redraw();
                self.png_refill();
            }
            Message::Select(id) => {
                // Clicking the selected row again deselects it.
                self.selected = (self.selected.as_ref() != Some(&id)).then_some(id);
                self.redraw();
            }
            Message::WorkerPanicked(why) => {
                // Whatever the job was, its completion never came: unlock the app, and do not
                // trust that the session is backed up.
                self.busy = false;
                self.recovery_failed.store(true, Ordering::SeqCst);
                self.error(format!("Background task failed: {why}"));
            }
            Message::DistortionReady(id, source, d) => self.distortion_ready(id, &source, *d),
            Message::PaneSelected(pane) => {
                self.pane = pane;
                self.now = Instant::now();
                tabs::go(&mut self.pane_slide, browser::index(pane), self.now);
                if pane == Pane::Sets {
                    self.refresh_sets();
                }
            }
            Message::SetsListed(sets, unsaved, folder) => {
                self.sets_folder = Some(folder);
                self.sets = sets;
                self.unsaved = unsaved;
            }
            message @ (Message::SetsDirInput(_)
            | Message::SetsDirSubmit
            | Message::BrowseSetsDir
            | Message::SetsDirPicked(_)
            | Message::AskSave(_)
            | Message::ExportsDirInput(_)
            | Message::ExportsDirSubmit
            | Message::BrowseExportsDir
            | Message::ExportsDirPicked(_)
            | Message::AskExport(_)
            | Message::ThemePicked(_)
            | Message::AccentInput(_)
            | Message::AccentSubmit
            | Message::AccentReset
            | Message::TraceColorInput(..)
            | Message::TraceColorSubmit(_)
            | Message::TraceColorsReset) => return self.settings_message(message),
            Message::SetNameInput(name) => {
                self.set_name = name;
                self.session_changed();
            }
            Message::SaveSet => return self.save_set(),
            Message::SaveSetTo(Some(path)) => {
                let target = with_extension(path.clone());
                if self.in_recovery_dir(&target) {
                    self.busy = false;
                    self.error("That folder is for crash recovery; pick another".into());
                } else if target == path {
                    // The portal asked before replacing it.
                    self.start_save(target, true);
                } else {
                    // The portal never saw this name: it may be another set.
                    return check_save(target);
                }
            }
            Message::SaveChecked(path, exists) => self.save_checked(path, exists),
            Message::SaveSetTo(None) => self.busy = false,
            Message::SetSaved(saved) => self.set_saved(saved),
            Message::BootNotes(notes) => {
                // They join whatever the boot said, rather than replace it.
                let mut all = Vec::new();
                if !self.status.text.is_empty() {
                    all.push((self.status.text.clone(), self.status.level));
                }
                all.extend(notes);
                if let Some(status) = Status::joined(all) {
                    self.status = status;
                }
            }
            Message::RecoveryFailed(e) => self.warn(format!("Recovery file not written: {e}")),
            Message::CloseRequested => {
                self.worker.finish();
                return iced::exit();
            }
        }
        Task::none()
    }

    fn audio(&mut self, event: AudioEvent) {
        match event {
            AudioEvent::Nodes(nodes) => {
                self.nodes = nodes;
                self.apply_side_channels();
                self.open_input();
            }
            AudioEvent::InputStarted { rate } => {
                self.capture_rate = Some(rate);
                self.command(SessionCommand::Rate(rate));
                // A new stream: nothing heard on the last one may join it.
                self.command(SessionCommand::Reset);
            }
            AudioEvent::InputLost(why) => {
                self.input_closed();
                self.stop();
                self.error(format!("Input lost: {why}"));
            }
            AudioEvent::PlayStarted => self.rta_play_started(),
            AudioEvent::PlayFinished => {
                // Before the sweep was sent it is a stale one.
                if self.state == (Activity::Measuring { played: None })
                    && self.play_deadline.is_some()
                {
                    self.state = Activity::Measuring {
                        played: Some(Instant::now()),
                    };
                }
            }
            AudioEvent::PlayFailed(why) => {
                // Stale ones from an earlier playback are ignored.
                if matches!(
                    self.state,
                    Activity::Measuring { .. } | Activity::Rta { playing: true }
                ) {
                    self.stop();
                    self.error(format!("Playback failed: {why}"));
                }
            }
            AudioEvent::Error(why) => {
                self.input_closed();
                self.stop();
                // The engine's thread has ended; dropping it joins it.
                self.engine = None;
                self.offline = true;
                self.error(format!("Audio connection lost: {why}"));
            }
        }
    }

    fn analysis(&mut self, event: SessionEvent) -> Task<Message> {
        match event {
            SessionEvent::Level { peak_dbfs, clipped } => {
                if clipped {
                    self.clip_at = Some(Instant::now());
                    self.rta_clipped();
                }
                let held = self.clip_at.is_some_and(|at| at.elapsed() < CLIP_HOLD);
                self.level = (peak_dbfs, held);
            }
            SessionEvent::Measured {
                response,
                ir,
                sweep_seconds,
                drift_ppm,
                clipped,
            } => {
                match self.state {
                    Activity::Measuring { .. } => self.stop(),
                    Activity::Listening => {}
                    // A late result after Stop is stale.
                    _ => return Task::none(),
                }
                return self.add_measurement(response, ir, sweep_seconds, drift_ppm, clipped);
            }
            SessionEvent::MissingEnd => {
                if matches!(self.state, Activity::Measuring { .. }) {
                    self.stop();
                }
                self.warn(
                    "Sweep end marker not found — check that Length matches the file played, or reduce noise"
                        .into(),
                );
            }
            SessionEvent::Failed(e) => {
                if matches!(self.state, Activity::Measuring { .. }) {
                    self.stop();
                }
                self.error(format!("Measurement failed: {e}"));
            }
            SessionEvent::BestRatio(ratio) => self.best_ratio = ratio,
            SessionEvent::Rta { response, frames } => self.rta_data(response, frames),
            SessionEvent::RtaReset => self.rta_cleared(),
        }
        Task::none()
    }

    /// Arm the detector and play the sweep, once it is rendered.
    fn measure(&mut self) -> Task<Message> {
        if let Err(why) = self.ready_to_measure() {
            self.error(why);
            return Task::none();
        }
        let length = self.prefs.length;
        self.arm();
        self.state = Activity::Measuring { played: None };
        self.play_deadline = None;
        match &self.stimulus {
            Some((rendered, samples)) if *rendered == length => {
                let samples = Arc::clone(samples);
                self.play(samples);
                Task::none()
            }
            _ => {
                self.info("Preparing the sweep…".into());
                render(length)
            }
        }
    }

    /// Send the sweep to the output and start the playback deadline.
    fn play(&mut self, samples: Arc<Vec<f32>>) {
        let (sink, channels) = match self.output() {
            Some(sink) => (sink.clone(), self.output_channels(sink)),
            None => {
                self.stop();
                return self.error("Output not connected".into());
            }
        };
        let length = Duration::from_secs_f64(samples.len() as f64 / f64::from(PLAY_RATE));
        let gain = (10f64.powf(self.prefs.level_db / 20.0) / SWEEP_PEAK) as f32;
        self.sweep_output = Some((sink.name.clone(), self.side));
        self.send(AudioCommand::Play {
            sink,
            channels,
            samples,
            rate: PLAY_RATE,
            gain,
            looped: false,
        });
        self.play_deadline = Some(
            Instant::now()
                + fastgraph_core::audio::START_TIMEOUT
                + length
                + MEASURE_TIMEOUT
                + PLAYBACK_SLACK,
        );
        self.info("Measuring: playing the sweep…".into());
    }

    /// Put the session in sweep mode with a fresh detector.
    fn arm(&mut self) {
        self.command(SessionCommand::SetMode(Mode::Sweep(self.prefs.length)));
        self.command(SessionCommand::Reset);
        self.best_ratio = 0.0;
        self.sweep_output = None;
    }

    /// End the running activity: stop playback and analysis.
    pub(crate) fn stop(&mut self) {
        if matches!(
            self.state,
            Activity::Measuring { .. } | Activity::Rta { playing: true }
        ) {
            self.send(AudioCommand::StopPlay);
        }
        if self.state == (Activity::Rta { playing: true }) {
            self.rta.stopped_side = Some(self.side);
        }
        self.rta.started = false;
        self.rta.averaging = false;
        if self.state != Activity::Idle {
            self.command(SessionCommand::SetMode(Mode::Idle));
        }
        self.state = Activity::Idle;
        self.play_deadline = None;
    }

    /// The input stream is gone; a device-list update may reopen it.
    fn input_closed(&mut self) {
        self.opened = None;
        self.capture_rate = None;
        self.level = (FLOOR_DBFS, false);
    }

    /// Why Measure or Listen can't start, if it can't.
    pub(crate) fn ready_to_start(&self) -> Result<(), String> {
        if self.state != Activity::Idle {
            return Err("Stop the current measurement first".into());
        }
        if !self.sweep_mode {
            return Err("Switch the mode to Sweep first".into());
        }
        self.input_ready()
    }

    /// The sink and channel indices to play the sweep into.
    pub(crate) fn ready_to_measure(&self) -> Result<(Node, Vec<usize>), String> {
        self.ready_to_start()?;
        let sink = self.output().ok_or("Choose an output first")?;
        let channels = self.output_channels(sink);
        if channels.is_empty() {
            return Err(devices::NO_CHANNELS.into());
        }
        Ok((sink.clone(), channels))
    }

    /// Add a captured measurement and queue the recovery write.
    fn add_measurement(
        &mut self,
        response: Response,
        ir: ImpulseResponse,
        sweep_seconds: f64,
        drift_ppm: f64,
        clipped: bool,
    ) -> Task<Message> {
        // Listen takes the Side shown; a sweep played here, the one it played.
        let side = self
            .sweep_output
            .as_ref()
            .map_or(self.side, |(_, side)| *side);
        let seat = self.next_number(Kind::Sweep, &format!("{} seat ", side_label(side)));
        let created = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let m = Measurement {
            meta: Meta {
                id: Store::new_id(),
                name: format!("{} seat {seat}", side_label(side)),
                kind: Kind::Sweep,
                side: Some(side),
                seating: Some(seat),
                input: self.prefs.input.clone(),
                output: self.sweep_output.as_ref().map(|(sink, _)| sink.clone()),
                drift_ppm: Some(drift_ppm),
                clipped,
                created,
                source: None,
                sources: Vec::new(),
                compensable: false,
                baked_compensation: None,
                shown: true,
                offset_db: 0.0,
                ir_peak: Some(ir.peak),
                sweep_seconds: Some(sweep_seconds),
            },
            response,
        };
        self.add_item(m, Some(ir));
        self.redraw();
        let text = format!("Measured · drift {drift_ppm:+.0} ppm");
        if clipped {
            self.warn(format!("{text} · input clipped"));
        } else {
            self.info(text);
        }
        Task::none()
    }

    /// One more than the highest `n` of the `kind` items still named
    /// `{prefix}{n}` (renamed ones don't count), so a deleted number isn't
    /// reused while a later one exists.
    pub(crate) fn next_number(&self, kind: Kind, prefix: &str) -> u32 {
        self.items
            .iter()
            .filter(|i| i.m.meta.kind == kind)
            .filter_map(|i| i.m.meta.name.strip_prefix(prefix)?.parse::<u32>().ok())
            .max()
            .map_or(1, |n| n.saturating_add(1))
    }

    /// Add `m` (and its impulse response) to the session.
    pub(crate) fn add_item(&mut self, m: Measurement, ir: Option<ImpulseResponse>) {
        self.push_item(m, ir.map(Arc::new));
        self.session_changed();
    }

    /// Add `m` to the list, shown as its metadata says, in the least used
    /// colour.
    pub(crate) fn push_item(&mut self, m: Measurement, ir: Option<Arc<ImpulseResponse>>) {
        let color = self.next_color();
        self.items.push(Item {
            shown: m.meta.shown,
            m,
            color,
            ir,
        });
        if let Some(item) = self.items.last() {
            self.queue_distortion(item);
        }
    }

    /// The session changed: mark it unsaved and rewrite its recovery file,
    /// on the worker, from a snapshot of the items. Of the writes queued
    /// meanwhile only the newest runs; an empty session removes the file.
    pub(crate) fn session_changed(&mut self) {
        self.dirty = true;
        let generation = self.recovery_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let Some(path) = self.recovery.clone() else {
            return;
        };
        let latest = Arc::clone(&self.recovery_generation);
        let failed = Arc::clone(&self.recovery_failed);
        let name = self.set_name.clone();
        let snapshot = self.snapshot();
        self.worker.submit(Box::new(move || {
            if latest.load(Ordering::SeqCst) != generation {
                return None;
            }
            let entries = into_entries(snapshot);
            let written = write_recovery(&path, &name, &entries);
            failed.store(written.is_err(), Ordering::SeqCst);
            written.err().map(Message::RecoveryFailed)
        }));
    }

    /// The items as they are now. The impulse responses are shared, and
    /// copied on the worker by [`into_entries`].
    fn snapshot(&self) -> Snapshot {
        self.items
            .iter()
            .map(|i| (i.m.clone(), i.ir.clone()))
            .collect()
    }

    /// A Save, Open or Import clicked while another one runs.
    fn wait(&mut self) {
        self.info("Wait for the current save or open to finish".into());
    }

    /// The Save button: write `<sets folder>/<name>.fgset`, after the
    /// dialog when asked.
    fn save_set(&mut self) -> Task<Message> {
        if self.busy {
            self.wait();
            return Task::none();
        }
        let name = strip_extension(&self.set_name);
        if name.is_empty() {
            self.error("Give the set a name before saving".into());
            return Task::none();
        }
        let name = name.to_owned();
        let file = format!("{}.{}", set::safe_file_name(&name), set::EXTENSION);
        let dir = match self.prefs.sets_dir() {
            Ok(dir) => dir,
            Err(e) => {
                self.error(format!("No folder to save to: {e:#}"));
                return Task::none();
            }
        };
        self.busy = true;
        self.save_name = Some(name);
        if self.prefs.ask_save {
            return Task::future(async move {
                rfd::AsyncFileDialog::new()
                    .set_title("Save set")
                    .set_directory(dir)
                    .set_file_name(file)
                    .add_filter("Measurement set", &[set::EXTENSION])
                    .save_file()
                    .await
                    .map(|f| f.path().to_path_buf())
            })
            .map(Message::SaveSetTo);
        }
        check_save(dir.join(file))
    }

    /// The answer to whether Save's file exists: write it, replacing the
    /// file that is there.
    fn save_checked(&mut self, path: PathBuf, exists: bool) {
        self.busy = false;
        self.start_save(path, exists);
    }

    /// Write the session to `path` on the worker (replacing a file there
    /// only if `replace`), and then drop this run's recovery file unless the
    /// session changed meanwhile.
    fn start_save(&mut self, path: PathBuf, replace: bool) {
        self.busy = true;
        // The recovery writes queued before this still run first.
        let generation = self.recovery_generation.load(Ordering::SeqCst);
        self.saving_generation = generation;
        let latest = Arc::clone(&self.recovery_generation);
        let failed = Arc::clone(&self.recovery_failed);
        let recovery = self.recovery.clone();
        let name = self
            .save_name
            .take()
            .unwrap_or_else(|| strip_extension(&self.set_name).to_owned());
        let snapshot = self.snapshot();
        self.worker.submit(Box::new(move || {
            let entries = into_entries(snapshot);
            let written = path
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .map_or(Ok(()), |d| {
                    std::fs::create_dir_all(d).map_err(|e| format!("creating {}: {e}", d.display()))
                })
                .and_then(|()| {
                    set::write_set_to(&path, &name, &entries, replace).map_err(|e| {
                        let io = e.root_cause().downcast_ref::<std::io::Error>();
                        if io.is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists) {
                            let file = path.file_name().unwrap_or_default().to_string_lossy();
                            format!("{file} now exists — Save again to replace")
                        } else {
                            format!("{e:#}")
                        }
                    })
                });
            if let Err(e) = written {
                return Some(Message::SetSaved(Err(e)));
            }
            failed.store(false, Ordering::SeqCst);
            if let Some(recovery) = recovery
                && latest.load(Ordering::SeqCst) == generation
            {
                // Only a failure to delete matters, and only a little.
                let _ = std::fs::remove_file(recovery);
            }
            Some(Message::SetSaved(Ok(path)))
        }));
    }

    fn set_saved(&mut self, saved: Result<PathBuf, String>) {
        self.busy = false;
        match saved {
            Ok(path) => {
                let label = path
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                self.saved_as = Some(path);
                if self.recovery_generation.load(Ordering::SeqCst) == self.saving_generation {
                    self.dirty = false;
                }
                self.info(format!("Saved {label}"));
                self.refresh_sets();
            }
            Err(e) => self.error(format!("Set not saved: {e}")),
        }
    }

    /// List the save folder and the recovery folder on the worker, for the
    /// Sets tab. This run's own recovery file is the live session, not an
    /// unsaved one.
    pub(crate) fn refresh_sets(&mut self) {
        let prefs = self.prefs.clone();
        let recovery = self.recovery.clone();
        self.worker.submit(Box::new(move || {
            let folder = prefs.sets_dir().ok();
            let sets = folder.as_deref().map_or_else(Vec::new, set::list_sets);
            let unsaved = recovery
                .as_deref()
                .and_then(Path::parent)
                .map_or_else(Vec::new, set::list_sets)
                .into_iter()
                .filter(|(path, _)| Some(path) != recovery.as_ref())
                .collect();
            Some(Message::SetsListed(sets, unsaved, folder))
        }));
    }

    /// Give this run a new recovery file, so the old one, holding the
    /// session about to be replaced, stays on disk as an unsaved session.
    /// Writes already queued still go to the old file.
    fn rotate_recovery(&mut self) {
        let Some(old) = self.recovery.as_deref() else {
            return;
        };
        let Some(dir) = old.parent() else {
            return;
        };
        // File names have whole seconds: the next is a second after the last.
        let started = SystemTime::now().max(self.recovery_started + Duration::from_secs(1));
        let path = set::recovery_path(dir, started, std::process::id());
        self.recovery = Some(path);
        self.recovery_started = started;
        // A fresh counter: the queued writes of the old file keep theirs,
        // and the next change does not supersede them.
        self.recovery_generation = Arc::default();
        self.saving_generation = u64::MAX;
    }

    /// Whether Clear has nothing to do: the session is as first opened, with
    /// nothing running and the first tabs and mode showing.
    pub(crate) fn is_pristine(&self) -> bool {
        self.items.is_empty()
            && self.set_name.is_empty()
            && self.state == Activity::Idle
            && self.rta.live.is_none()
            && self.sweep_mode
            && self.pane == Pane::Measure
            && self.graph_view == graph::View::Response
            && !self.graph.any_zoomed()
            && self.png.is_none()
    }

    /// The Clear button: end any activity and return the session to its
    /// first-opened state. Unsaved work is kept as an unsaved session, as a
    /// replacing Open keeps it.
    fn clear(&mut self) {
        if self.busy {
            return self.wait();
        }
        if self.is_pristine() {
            return;
        }
        if self.dirty
            && !self.items.is_empty()
            && (self.recovery.is_none() || self.recovery_failed.load(Ordering::SeqCst))
        {
            return self.error("Save first — unsaved measurements could not be backed up".into());
        }
        if !self.items.is_empty() {
            self.rotate_recovery();
        }
        self.clears += 1;
        self.stop();
        self.rta.live = None;
        self.rta.clipped = false;
        // The app opens in Sweep mode; the mode is not a preference.
        self.sweep_mode = true;
        self.items.clear();
        self.selected = None;
        self.distortion.clear();
        self.renaming = None;
        self.png = None;
        self.offset_texts.clear();
        self.set_name.clear();
        self.saved_as = None;
        self.dirty = false;
        self.hover = None;
        self.status = Status {
            text: String::new(),
            level: StatusLevel::Info,
        };
        self.now = Instant::now();
        self.pane = Pane::Measure;
        tabs::go(
            &mut self.pane_slide,
            browser::index(Pane::Measure),
            self.now,
        );
        self.graph_view = graph::View::Response;
        tabs::go(
            &mut self.graph_slide,
            graph::index(graph::View::Response),
            self.now,
        );
        self.graph.set_view(graph::View::Response);
        self.graph.reset_zoom();
        self.redraw();
    }

    /// Put a set's measurements in the session: in place of its items
    /// (`replace`, the session is then that set), or after them with new ids
    /// for those that clash.
    fn set_opened(
        &mut self,
        opened: Result<(PathBuf, SetHeader, Vec<Entry>, Vec<String>), String>,
        replace: bool,
    ) {
        let (path, header, mut entries, problems) = match opened {
            Ok(opened) => opened,
            Err(e) => {
                let verb = if replace { "opened" } else { "imported" };
                return self.error(format!("Set not {verb}: {e}"));
            }
        };
        let label = header.title(&path);
        let n = entries.len();
        // `read_set` adds one problem, not an entry's, when the count is off.
        let unreadable = problems.len() - usize::from(n != header.count);
        if replace {
            let unsafe_to_drop =
                self.recovery.is_none() || self.recovery_failed.load(Ordering::SeqCst);
            if self.dirty && !self.items.is_empty() && unsafe_to_drop {
                return self
                    .error("Save first — unsaved measurements could not be backed up".into());
            }
            if !self.items.is_empty() {
                self.rotate_recovery();
            }
            self.items.clear();
            self.selected = None;
            self.distortion.clear();
            self.renaming = None;
            self.offset_texts.clear();
            // A recovery file is never "the open set": Save must not write
            // into the recovery folder. Nor is a set only partly read: saving
            // over it would drop what could not be read, so it is not named
            // either, and a plain Save asks for a name.
            self.set_name = if problems.is_empty() {
                header.name
            } else {
                String::new()
            };
            self.saved_as = (problems.is_empty() && !self.in_recovery_dir(&path)).then_some(path);
            self.dirty = false;
        } else {
            let taken: HashSet<String> = self.items.iter().map(|i| i.m.meta.id.clone()).collect();
            set::reissue_clashing(&mut entries, &taken);
        }
        for e in entries {
            let m = Measurement {
                meta: e.meta,
                response: e.response,
            };
            self.push_item(m, e.ir.map(Arc::new));
        }
        if !replace {
            self.session_changed();
        } else if self.pane == Pane::Sets {
            self.refresh_sets();
        }
        self.redraw();
        let text = if replace {
            format!("Opened {label}")
        } else {
            format!("Imported {n} from {label}")
        };
        if problems.is_empty() {
            self.info(text);
        } else if unreadable == 0 {
            self.warn(format!("{text} · {}", problems.join("; ")));
        } else {
            self.warn(format!(
                "{text} · {unreadable} unreadable: {}",
                problems.join("; ")
            ));
        }
    }

    /// Whether `path` is a file of the recovery folder.
    fn in_recovery_dir(&self, path: &Path) -> bool {
        let dir = match self.recovery.as_deref().and_then(Path::parent) {
            Some(dir) => Some(dir.to_path_buf()),
            None => set::recovery_dir().ok(),
        };
        dir.is_some_and(|dir| path.starts_with(dir))
    }

    /// The least used trace colour, the first of those on a tie.
    fn next_color(&self) -> usize {
        (0..theme::TRACE_COLOURS)
            .min_by_key(|&c| self.items.iter().filter(|i| i.color == c).count())
            .unwrap_or(0)
    }

    /// Recompute the shown traces and redraw the graph.
    pub(crate) fn redraw(&mut self) {
        let shown = self.items.iter().filter(|i| i.shown);
        match self.graph_view {
            graph::View::Response => {
                let traces = shown
                    .map(|i| graph::Trace {
                        name: i.m.meta.name.clone(),
                        response: self.display(i),
                        color: i.color,
                    })
                    .collect();
                self.graph.set(traces);
                self.graph.set_phase(Vec::new(), 0);
            }
            graph::View::Phase => {
                // Phase is not compensated, aligned or offset: those change
                // the level only.
                let mut missing = 0;
                let mut traces = Vec::new();
                for i in shown {
                    let r = &i.m.response;
                    match r.phase_smoothed(self.prefs.smoothing) {
                        Some(deg) => traces.push(graph::PhaseTrace {
                            freq: r.freq.clone(),
                            deg,
                            color: i.color,
                        }),
                        None => missing += 1,
                    }
                }
                self.graph.set(Vec::new());
                self.graph.set_phase(traces, missing);
            }
            graph::View::Distortion => {
                self.graph.set(Vec::new());
                self.graph.set_phase(Vec::new(), 0);
            }
        }
        let plot = (self.graph_view == graph::View::Distortion)
            .then(|| self.distortion_plot())
            .flatten();
        if plot.is_none() {
            let note = self.distortion_note();
            self.graph.set_distortion_note(note);
        }
        self.graph.set_distortion(plot);
        self.redraw_live();
    }

    fn export_stimulus(&self) -> Task<Message> {
        let length = self.prefs.length;
        let rate = self.prefs.checked_export_rate();
        let name = sweep_file_name(length, rate);
        Task::future(async move {
            let file = rfd::AsyncFileDialog::new()
                .set_title("Export sweep WAV")
                .set_file_name(name)
                .add_filter("WAV audio", &["wav"])
                .save_file()
                .await?;
            let path = file.path().to_path_buf();
            let stimulus = sweep_stimulus(length, rate);
            Some(
                write_wav(&path, &stimulus.samples, rate, 2)
                    .map(|()| path)
                    .map_err(|e| format!("{e:#}")),
            )
        })
        .and_then(|saved| Task::done(Message::StimulusSaved(saved)))
    }

    pub(crate) fn send(&self, command: AudioCommand) {
        #[cfg(test)]
        self.sent.borrow_mut().push(rta_mode::Sent::of(&command));
        if let Some(engine) = &self.engine {
            engine.send(command);
        }
    }

    pub(crate) fn command(&self, command: SessionCommand) {
        #[cfg(test)]
        self.sent
            .borrow_mut()
            .push(rta_mode::Sent::Session(command));
        if let Some(session) = &self.session {
            session.send(command);
        }
    }

    pub(crate) fn save_prefs(&mut self) {
        if !self.prefs_writable {
            return;
        }
        if let Err(e) = Prefs::path().and_then(|path| self.prefs.save(&path)) {
            self.warn(format!("Preferences not saved: {e:#}"));
        }
    }

    pub(crate) fn info(&mut self, text: String) {
        self.status = Status {
            text,
            level: StatusLevel::Info,
        };
    }

    pub(crate) fn warn(&mut self, text: String) {
        self.status = Status {
            text,
            level: StatusLevel::Warn,
        };
    }

    pub(crate) fn error(&mut self, text: String) {
        self.status = Status {
            text,
            level: StatusLevel::Error,
        };
    }

    // --- view ----------------------------------------------------------------

    pub fn view(&self) -> Element<'_, Message> {
        let graph = self
            .graph
            .view(self.hover, !self.items.is_empty())
            .map(Message::Graph);
        let graph = match &self.png {
            Some(options) => stack![graph, export_png::panel(options)].into(),
            None => graph,
        };
        // The header knows its width, to place Export PNG….
        let header = responsive(|size| self.header(size.width)).height(Length::Shrink);
        let main = column![
            header,
            Space::new().height(GRAPH_MARGIN),
            graph,
            rule::horizontal(1).style(theme::rule),
            list::view(self),
        ];
        let pane = match self.pane {
            Pane::Measure => devices::view(self),
            Pane::Sets => column![browser::view(self), devices::status(self)]
                .spacing(8)
                .into(),
            Pane::Settings => column![settings::view(self), devices::status(self)]
                .spacing(8)
                .into(),
        };
        row![
            container(
                column![browser::tabs(self), container(pane).height(Length::Fill)].spacing(10)
            )
            .width(Length::Fixed(devices::WIDTH))
            .height(Length::Fill)
            .padding(14),
            rule::vertical(1).style(theme::rule),
            container(main).padding([8, 8]),
        ]
        .into()
    }

    /// The lines over the graph, `width` wide. Line 1: the view tabs,
    /// shrinking below their width only when the buttons at the right need
    /// the room; the same in every view. Export PNG… joins them only where
    /// it leaves the tabs their width. Line 2: the controls, which wrap by
    /// whole groups.
    fn header(&self, width: f32) -> Element<'_, Message> {
        let on_tabs_line = toolbar::png_on_tabs_line(width, GRAPH_TABS_WIDTH);
        let (controls, actions) = toolbar::view(self, on_tabs_line);
        let tabs = container(tabs::pill_tabs(
            &graph::VIEWS,
            tabs::position(&self.graph_slide, self.now),
            Length::Fill,
            Message::GraphViewSelected,
        ))
        .width(Length::Fill)
        .max_width(GRAPH_TABS_WIDTH);
        column![
            row![container(tabs).width(Length::Fill), actions]
                .spacing(10)
                .align_y(iced::Alignment::Center),
            Space::new().height(GRAPH_MARGIN),
            controls,
        ]
        .into()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let events = Subscription::run_with(self.events.clone(), |events| {
            let receiver = events
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            stream::iter(receiver).flatten()
        });
        // Ticks time out a measurement.
        let tick =
            (matches!(self.state, Activity::Measuring { .. })).then(|| Subscription::run(ticks));
        // Escape cancels a rename, once the Export PNG panel over it is closed.
        let escape = (self.renaming.is_some() && self.png.is_none()).then(|| {
            iced::event::listen_with(|event, _, _| match event {
                iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
                    key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
                    ..
                }) => Some(Message::RenameCancel),
                _ => None,
            })
        });
        // Escape closes the Export PNG panel.
        let png_escape = self.png.is_some().then(|| {
            iced::event::listen_with(|event, _, _| match event {
                iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
                    key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
                    ..
                }) => Some(Message::Png(export_png::Event::Cancel)),
                _ => None,
            })
        });
        let close = iced::window::close_requests().map(|_| Message::CloseRequested);
        // Startup timing: the first redraw request comes after the first `view`.
        let first_frame = crate::exit_after_first_frame()
            .then(|| iced::window::frames().map(|_| Message::FirstFrame));
        // Frames only while a tab pill slides.
        let slide = (self.pane_slide.is_animating(self.now)
            || self.graph_slide.is_animating(self.now))
        .then(|| iced::window::frames().map(Message::Frame));
        Subscription::batch(
            [events, close]
                .into_iter()
                .chain(tick)
                .chain(escape)
                .chain(png_escape)
                .chain(first_frame)
                .chain(slide),
        )
    }

    pub fn theme(&self) -> iced::Theme {
        self.theme.clone()
    }
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
