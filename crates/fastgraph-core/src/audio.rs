//! PipeWire audio engine: device list, single-channel capture, stimulus playback.
//!
//! Everything PipeWire runs on one thread owned by [`Engine`]. Commands reach it through a
//! `pipewire::channel` attached to its main loop; events leave through the [`EventSink`] and
//! captured audio through a `std::sync::mpsc` sender. Stream callbacks only record what
//! happened; streams are created and torn down between loop iterations, never from inside
//! their own callbacks.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::rc::{Rc, Weak};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use pipewire::{self as pw, properties::properties, spa};
use spa::param::ParamType;
use spa::pod::{Pod, Value};

use crate::EventSink;

/// Quiet period before a changed device list is reported.
const DEBOUNCE: Duration = Duration::from_millis(100);
/// Upper bound on how long the loop sleeps between housekeeping passes.
const TICK: Duration = Duration::from_millis(20);
/// `res` of the errors PipeWire reports on the core for calls on objects it has already
/// destroyed (`unknown resource N op:M`), e.g. our stream after its target device vanished.
const ENOENT: i32 = 2;
/// How long a new stream may take to start (capture: format negotiated; playback: streaming).
/// Generous because Bluetooth devices can take several seconds to wake or switch profile.
pub const START_TIMEOUT: Duration = Duration::from_secs(10);

/// An audio source or sink known to PipeWire.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    /// Global id (changes when the device reappears).
    pub id: u32,
    /// `object.serial`, used as `target.object` for streams.
    pub serial: String,
    /// `node.name`.
    pub name: String,
    /// `node.description` (falls back to `node.nick`, then the name).
    pub description: String,
    pub is_sink: bool,
    /// The interface's raw hardware node (`Audio/*/Internal`), which a session manager's
    /// split profile hides behind per-port nodes. It carries every hardware channel, so an
    /// input the active profile doesn't expose is still reachable.
    pub raw: bool,
    /// Channel position labels (`FL`, `FR`, `MONO`, `AUX0`, ... or `CH1..CHn`).
    pub channels: Vec<String>,
}

/// Where capture comes from: a source, or the monitor of a sink.
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    Source(Node),
    MonitorOf(Node),
}

impl Node {
    /// The name shown for this node: its description, marked when it is a raw hardware node.
    pub fn label(&self) -> String {
        if self.raw {
            format!("{} (all channels)", self.description)
        } else {
            self.description.clone()
        }
    }
}

impl Input {
    pub fn node(&self) -> &Node {
        match self {
            Input::Source(n) | Input::MonitorOf(n) => n,
        }
    }

    pub fn label(&self) -> String {
        match self {
            Input::Source(n) => n.label(),
            Input::MonitorOf(n) => format!("Monitor of {}", n.label()),
        }
    }

    /// Stable identifier stored in preferences.
    pub fn key(&self) -> String {
        match self {
            Input::Source(n) => n.name.clone(),
            Input::MonitorOf(n) => format!("monitor:{}", n.name),
        }
    }
}

#[derive(Clone)]
pub enum AudioCommand {
    /// Start capturing `channel` (index into the node's channels); replaces any open input.
    OpenInput {
        input: Input,
        channel: usize,
    },
    CloseInput,
    /// Play `samples` (mono, at `rate`) into `channels` of `sink`; replaces any playback.
    Play {
        sink: Node,
        channels: Vec<usize>,
        samples: Arc<Vec<f32>>,
        rate: u32,
        gain: f32,
        looped: bool,
    },
    StopPlay,
    Shutdown,
}

/// Events from the audio thread.
///
/// Events carry no stream generation: just after a new `OpenInput` or `Play`, a stale
/// `InputLost`/`PlayFinished`/`PlayFailed` or capture blocks from the replaced stream may
/// still arrive. Receivers must tolerate that.
#[derive(Clone, Debug)]
pub enum AudioEvent {
    /// Every audio source and sink, sorted by description.
    Nodes(Vec<Node>),
    /// Capture format negotiated; blocks arrive at this rate.
    InputStarted { rate: u32 },
    /// The input stream failed, did not start within 10 s, or its device went away; capture
    /// has stopped.
    InputLost(String),
    /// The playback stream started streaming (once per `Play`).
    PlayStarted,
    /// All samples were queued (non-looped playback); the stream is stopped.
    PlayFinished,
    /// Playback failed or did not start within 10 s; the stream is stopped.
    PlayFailed(String),
    /// The PipeWire connection failed. The engine has stopped: it is preceded by
    /// `InputLost`/`PlayFailed` for streams that were open, no further events follow, and
    /// commands are ignored. Start a new [`Engine`] to reconnect.
    Error(String),
}

/// Handle to the audio thread. Dropping it shuts the thread down.
pub struct Engine {
    tx: pw::channel::Sender<AudioCommand>,
    thread: Option<JoinHandle<()>>,
}

impl Engine {
    /// Connect to PipeWire on a new thread. Fails if PipeWire is unreachable.
    pub fn start(
        events: EventSink<AudioEvent>,
        samples: mpsc::Sender<Vec<f32>>,
    ) -> anyhow::Result<Engine> {
        let (tx, rx) = pw::channel::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("fastgraph-audio".into())
            .spawn(move || run(events, samples, rx, ready_tx))
            .context("spawning the audio thread")?;
        let ready = ready_rx.recv();
        match ready {
            Ok(Ok(())) => Ok(Engine {
                tx,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(anyhow!("PipeWire unavailable: {e}"))
            }
            Err(_) => {
                let _ = thread.join();
                bail!("the audio thread exited during startup")
            }
        }
    }

    /// Queue a command; ignored if the audio thread has already stopped.
    pub fn send(&self, cmd: AudioCommand) {
        let _ = self.tx.send(cmd);
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.tx.send(AudioCommand::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Pure helpers

/// Which audio nodes are devices: `(is_sink, raw)` for sources and sinks, including the raw
/// hardware nodes (`…/Internal`) behind split profiles; `None` for streams and the rest.
pub fn classify(media_class: &str) -> Option<(bool, bool)> {
    match media_class {
        "Audio/Sink" | "Audio/Sink/Virtual" => Some((true, false)),
        "Audio/Source" | "Audio/Source/Virtual" => Some((false, false)),
        "Audio/Sink/Internal" => Some((true, true)),
        "Audio/Source/Internal" => Some((false, true)),
        _ => None,
    }
}

/// How a channel position is shown: `AUXn` (an interface's numbered hardware channel) as
/// `Ch n+1 (AUXn)`, so it matches the jack numbering; other positions unchanged.
pub fn channel_display(position: &str) -> String {
    match position
        .strip_prefix("AUX")
        .and_then(|n| n.parse::<u32>().ok())
    {
        Some(n) => format!("Ch {} ({position})", n + 1),
        None => position.to_owned(),
    }
}

/// Channel labels from the `audio.position` / `audio.channels` node properties.
pub fn positions(prop_position: Option<&str>, channels: Option<u32>) -> Vec<String> {
    let listed: Vec<String> = prop_position
        .unwrap_or("")
        .split(|c: char| c == ',' || c == '[' || c == ']' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    channel_labels(listed, channels)
}

/// The listed positions when they tell the channels apart, else `CH1..CHn`:
/// one per listed position when some repeat (several `UNK`), or `channels`
/// of them when none are listed.
fn channel_labels(listed: Vec<String>, channels: Option<u32>) -> Vec<String> {
    let distinct = listed
        .iter()
        .enumerate()
        .all(|(i, label)| !listed[..i].contains(label));
    if !listed.is_empty() && distinct {
        return listed;
    }
    let n = if listed.is_empty() {
        channels.unwrap_or(0)
    } else {
        listed.len() as u32
    };
    (1..=n).map(|i| format!("CH{i}")).collect()
}

/// Names of SPA channel ids 0..=BRC, in id order (`spa/param/audio/raw-types.h`).
const CHANNEL_NAMES: [&str; 38] = [
    "UNK", "NA", "MONO", "FL", "FR", "FC", "LFE", "SL", "SR", "FLC", "FRC", "RC", "RL", "RR", "TC",
    "TFL", "TFC", "TFR", "TRL", "TRC", "TRR", "RLC", "RRC", "FLW", "FRW", "LFE2", "FLH", "FCH",
    "FRH", "TFLC", "TFRC", "TSL", "TSR", "LLFE", "RLFE", "BC", "BLC", "BRC",
];
const AUX_FIRST: u32 = spa::sys::SPA_AUDIO_CHANNEL_START_Aux;
const AUX_LAST: u32 = spa::sys::SPA_AUDIO_CHANNEL_LAST_Aux;

/// Short label of a SPA channel id (`FL`, `MONO`, `AUX3`, ...).
fn channel_label(id: u32) -> String {
    match id {
        AUX_FIRST..=AUX_LAST => format!("AUX{}", id - AUX_FIRST),
        _ => CHANNEL_NAMES.get(id as usize).unwrap_or(&"UNK").to_string(),
    }
}

/// SPA channel id of a label produced by [`channel_label`]; `None` for `UNK`/`NA`/`CHn`.
fn channel_id(label: &str) -> Option<u32> {
    if let Some(n) = label.strip_prefix("AUX") {
        let n: u32 = n.parse().ok()?;
        return (n <= AUX_LAST - AUX_FIRST).then_some(AUX_FIRST + n);
    }
    let id = CHANNEL_NAMES.iter().position(|&n| n == label)? as u32;
    (id >= spa::sys::SPA_AUDIO_CHANNEL_MONO).then_some(id)
}

/// SPA position array for stream formats, if every label is a known position.
fn position_array(labels: &[String]) -> Option<[u32; spa::param::audio::MAX_CHANNELS]> {
    if labels.is_empty() || labels.len() > spa::param::audio::MAX_CHANNELS {
        return None;
    }
    let mut out = [0; spa::param::audio::MAX_CHANNELS];
    for (slot, label) in out.iter_mut().zip(labels) {
        *slot = channel_id(label)?;
    }
    Some(out)
}

/// One channel of interleaved little-endian f32 frames; a trailing partial frame is dropped.
fn deinterleave(bytes: &[u8], channels: usize, channel: usize) -> Vec<f32> {
    if channel >= channels {
        return Vec::new();
    }
    bytes
        .chunks_exact(4 * channels)
        .map(|frame| {
            let s = &frame[4 * channel..4 * channel + 4];
            f32::from_le_bytes([s[0], s[1], s[2], s[3]])
        })
        .collect()
}

/// Rate, channel count and positions read from a `Format`/`EnumFormat` pod. Values given as
/// choices (ranges, enums) are left `None`.
#[derive(Debug, Default, PartialEq)]
struct FormatInfo {
    rate: Option<u32>,
    channels: Option<u32>,
    positions: Option<Vec<String>>,
}

fn parse_format(pod: &Pod) -> FormatInfo {
    let Ok(obj) = pod.as_object() else {
        return FormatInfo::default();
    };
    let value = |key| fixed_value(obj.find_prop(spa::utils::Id(key))?.value());
    let int = |key| match value(key)? {
        (spa::sys::SPA_TYPE_Int, body) => {
            let v = i32::from_ne_bytes(body.get(..4)?.try_into().ok()?);
            (v > 0).then_some(v as u32)
        }
        _ => None,
    };
    FormatInfo {
        rate: int(spa::sys::SPA_FORMAT_AUDIO_rate),
        channels: int(spa::sys::SPA_FORMAT_AUDIO_channels),
        positions: value(spa::sys::SPA_FORMAT_AUDIO_position)
            .and_then(|(ty, body)| id_array(ty, body))
            .filter(|ids| !ids.is_empty())
            .map(|ids| ids.into_iter().map(channel_label).collect()),
    }
}

fn word(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// Type and body of a property value, looking through a `None` choice (PipeWire wraps the
/// fixed values of negotiated formats in one). Real choices (ranges, enums) give `None`.
/// Read by hand: the generic pod deserializer rejects a choice of arrays.
fn fixed_value(pod: &Pod) -> Option<(u32, &[u8])> {
    let b = pod.as_bytes();
    let ty = word(b, 4)?;
    if ty != spa::sys::SPA_TYPE_Choice {
        return Some((ty, &b[8..]));
    }
    // spa_pod_choice body: choice type, flags, child pod header (size, type), values.
    if word(b, 8)? != spa::sys::SPA_CHOICE_None {
        return None;
    }
    let size = word(b, 16)? as usize;
    Some((word(b, 20)?, b.get(24..24 + size)?))
}

/// Elements of an array-of-Id body (child header (size, type), then the ids).
fn id_array(ty: u32, body: &[u8]) -> Option<Vec<u32>> {
    if ty != spa::sys::SPA_TYPE_Array
        || word(body, 0)? != 4
        || word(body, 4)? != spa::sys::SPA_TYPE_Id
    {
        return None;
    }
    Some(
        body[8..]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|w| u32::from_ne_bytes(*w))
            .collect(),
    )
}

/// Fill interleaved f32 frames: `samples·gain` (clamped) into the `mask`ed channels,
/// zeros elsewhere. Returns true once the (non-looped) samples are exhausted.
fn render(
    out: &mut [u8],
    mask: &[bool],
    samples: &[f32],
    pos: &mut usize,
    gain: f32,
    looped: bool,
) -> bool {
    if mask.is_empty() {
        return !looped && *pos >= samples.len();
    }
    for frame in out.chunks_exact_mut(4 * mask.len()) {
        if looped && *pos >= samples.len() {
            *pos = 0;
        }
        let value = match samples.get(*pos) {
            Some(&s) => {
                *pos += 1;
                (s * gain).clamp(-1.0, 1.0)
            }
            None => 0.0,
        };
        for (slot, &on) in frame.as_chunks_mut::<4>().0.iter_mut().zip(mask) {
            *slot = if on { value } else { 0.0 }.to_le_bytes();
        }
    }
    !looped && *pos >= samples.len()
}

/// Whether an error reported through the core listener ends the connection. Everything on the
/// core is fatal except "unknown resource" errors about objects that are already gone.
fn is_fatal_core_error(id: u32, res: i32) -> bool {
    id == pw::core::PW_ID_CORE && res != -ENOENT
}

/// Whether a stream waiting to start since `since` has waited [`START_TIMEOUT`] at `now`.
fn start_timed_out(since: Option<Instant>, now: Instant) -> bool {
    since.is_some_and(|t| now.saturating_duration_since(t) >= START_TIMEOUT)
}

/// Events sent when the connection fails: open streams first, then `Error`.
fn fatal_events(message: &str, had_capture: bool, had_play: bool) -> Vec<AudioEvent> {
    let mut events = Vec::new();
    if had_capture {
        events.push(AudioEvent::InputLost(message.to_owned()));
    }
    if had_play {
        events.push(AudioEvent::PlayFailed(message.to_owned()));
    }
    events.push(AudioEvent::Error(message.to_owned()));
    events
}

/// Pod bytes in 8-byte aligned storage: `Pod::from_bytes` reads the header in place.
struct PodBuf(Vec<u64>);

impl PodBuf {
    fn new(bytes: &[u8]) -> PodBuf {
        let mut words = vec![0u64; bytes.len().div_ceil(8)];
        for (word, chunk) in words.iter_mut().zip(bytes.chunks(8)) {
            let mut b = [0u8; 8];
            b[..chunk.len()].copy_from_slice(chunk);
            *word = u64::from_ne_bytes(b);
        }
        PodBuf(words)
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: the slice covers exactly the Vec's initialized storage, and u8 has no
        // alignment or validity requirements.
        unsafe { std::slice::from_raw_parts(self.0.as_ptr().cast(), self.0.len() * 8) }
    }

    fn pod(&self) -> Option<&Pod> {
        Pod::from_bytes(self.bytes())
    }
}

/// Serialized `EnumFormat` pod: F32LE, optional rate, channels and positions.
fn format_pod(rate: Option<u32>, labels: &[String]) -> anyhow::Result<PodBuf> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    if let Some(rate) = rate {
        info.set_rate(rate);
    }
    if !labels.is_empty() {
        info.set_channels(labels.len() as u32);
        if let Some(pos) = position_array(labels) {
            info.set_position(pos);
        }
    }
    let bytes = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(spa::pod::Object {
            type_: spa::sys::SPA_TYPE_OBJECT_Format,
            id: ParamType::EnumFormat.as_raw(),
            properties: info.into(),
        }),
    )
    .map_err(|e| anyhow!("serializing the stream format: {e:?}"))?
    .0
    .into_inner();
    Ok(PodBuf::new(&bytes))
}

// ---------------------------------------------------------------------------------------------
// Audio thread

/// A node being tracked, with the proxy that delivers its info and params.
struct Tracked {
    is_sink: bool,
    raw: bool,
    serial: String,
    name: String,
    description: String,
    prop_position: Option<String>,
    prop_channels: Option<u32>,
    /// The channel count of the `Format` (preferred) or `EnumFormat` param.
    param_channels: Option<u32>,
    enum_positions: Option<Vec<String>>,
    format_positions: Option<Vec<String>>,
    // Field order matters: the listener is removed before the proxy is destroyed.
    _listener: pw::node::NodeListener,
    _proxy: pw::node::Node,
}

impl Tracked {
    fn node(&self, id: u32) -> Node {
        let count = self.param_channels.or(self.prop_channels);
        let channels = match self
            .format_positions
            .clone()
            .or_else(|| self.enum_positions.clone())
        {
            Some(listed) => channel_labels(listed, count),
            None => positions(self.prop_position.as_deref(), count),
        };
        Node {
            id,
            serial: self.serial.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            is_sink: self.is_sink,
            raw: self.raw,
            channels,
        }
    }

    fn update_props(&mut self, props: &spa::utils::dict::DictRef) {
        if let Some(s) = props.get(*pw::keys::OBJECT_SERIAL) {
            self.serial = s.to_owned();
        }
        if let Some(n) = props.get(*pw::keys::NODE_NAME) {
            self.name = n.to_owned();
        }
        if let Some(d) = props
            .get(*pw::keys::NODE_DESCRIPTION)
            .or_else(|| props.get(*pw::keys::NODE_NICK))
        {
            self.description = d.to_owned();
        }
        if let Some(p) = props.get("audio.position") {
            self.prop_position = Some(p.to_owned());
        }
        if let Some(c) = props
            .get(*pw::keys::AUDIO_CHANNELS)
            .and_then(|c| c.parse().ok())
        {
            self.prop_channels = Some(c);
        }
    }
}

/// A connected stream and its listener.
struct Active<D> {
    listener: pw::stream::StreamListener<D>,
    stream: pw::stream::StreamRc,
}

impl<D> Active<D> {
    /// Stop listening first so our own disconnect is not reported as a loss.
    fn close(self) {
        drop(self.listener);
        let _ = self.stream.disconnect();
    }
}

struct CaptureData {
    tx: mpsc::Sender<Vec<f32>>,
    channel: usize,
    channels: usize,
    shared: Weak<Shared>,
}

struct PlayData {
    samples: Arc<Vec<f32>>,
    pos: usize,
    mask: Vec<bool>,
    gain: f32,
    looped: bool,
    finished: bool,
    shared: Weak<Shared>,
}

enum PlayEnd {
    Finished,
    Failed(String),
}

/// State of the audio thread, shared by the loop callbacks (all on this one thread).
struct Shared {
    me: Weak<Shared>,
    samples: mpsc::Sender<Vec<f32>>,
    events: RefCell<EventSink<AudioEvent>>,
    commands: RefCell<VecDeque<AudioCommand>>,
    quit: Cell<bool>,
    nodes: RefCell<BTreeMap<u32, Tracked>>,
    changed_at: Cell<Option<Instant>>,
    last_nodes: RefCell<Option<Vec<Node>>>,
    capture: RefCell<Option<Active<CaptureData>>>,
    capture_lost: RefCell<Option<String>>,
    /// Since when the open capture has been waiting for its format (`None` once started).
    capture_waiting: Cell<Option<Instant>>,
    play: RefCell<Option<Active<PlayData>>>,
    play_end: RefCell<Option<PlayEnd>>,
    /// Since when the open playback has been waiting to stream (`None` once streaming).
    play_waiting: Cell<Option<Instant>>,
    /// Set by the core listener when the connection is lost.
    fatal: RefCell<Option<String>>,
}

impl Shared {
    fn emit(&self, event: AudioEvent) {
        (self.events.borrow_mut())(event);
    }

    fn touch(&self) {
        self.changed_at.set(Some(Instant::now()));
    }

    fn close_capture(&self) {
        let capture = self.capture.borrow_mut().take();
        if let Some(c) = capture {
            c.close();
        }
        self.capture_lost.borrow_mut().take();
        self.capture_waiting.set(None);
    }

    fn close_play(&self) {
        let play = self.play.borrow_mut().take();
        if let Some(p) = play {
            p.close();
        }
        self.play_end.borrow_mut().take();
        self.play_waiting.set(None);
    }

    /// Emit the device list if it has settled and differs from the last one sent.
    fn flush_nodes(&self) {
        match self.changed_at.get() {
            Some(t) if t.elapsed() >= DEBOUNCE => self.changed_at.set(None),
            _ => return,
        }
        let mut list: Vec<Node> = self
            .nodes
            .borrow()
            .iter()
            .map(|(&id, t)| t.node(id))
            .collect();
        list.sort_by(|a, b| a.description.cmp(&b.description).then(a.name.cmp(&b.name)));
        if self.last_nodes.borrow().as_ref() == Some(&list) {
            return;
        }
        *self.last_nodes.borrow_mut() = Some(list.clone());
        self.emit(AudioEvent::Nodes(list));
    }

    /// Act on what stream callbacks recorded, then on queued commands.
    fn housekeeping(&self, core: &pw::core::CoreRc) {
        let now = Instant::now();
        if start_timed_out(self.capture_waiting.get(), now) {
            self.capture_lost.borrow_mut().get_or_insert_with(|| {
                format!(
                    "the input did not start within {} s",
                    START_TIMEOUT.as_secs()
                )
            });
        }
        if start_timed_out(self.play_waiting.get(), now) {
            self.play_end.borrow_mut().get_or_insert_with(|| {
                PlayEnd::Failed(format!(
                    "playback did not start within {} s",
                    START_TIMEOUT.as_secs()
                ))
            });
        }
        let lost = self.capture_lost.borrow_mut().take();
        if let Some(msg) = lost {
            self.close_capture();
            self.emit(AudioEvent::InputLost(msg));
        }
        let end = self.play_end.borrow_mut().take();
        if let Some(end) = end {
            self.close_play();
            self.emit(match end {
                PlayEnd::Finished => AudioEvent::PlayFinished,
                PlayEnd::Failed(msg) => AudioEvent::PlayFailed(msg),
            });
        }
        loop {
            let cmd = self.commands.borrow_mut().pop_front();
            let Some(cmd) = cmd else { break };
            self.command(core, cmd);
        }
        self.flush_nodes();
    }

    fn command(&self, core: &pw::core::CoreRc, cmd: AudioCommand) {
        match cmd {
            AudioCommand::OpenInput { input, channel } => {
                self.close_capture();
                match self.open_capture(core, &input, channel) {
                    Ok(active) => {
                        *self.capture.borrow_mut() = Some(active);
                        self.capture_waiting.set(Some(Instant::now()));
                    }
                    Err(e) => self.emit(AudioEvent::InputLost(format!("{e:#}"))),
                }
            }
            AudioCommand::CloseInput => self.close_capture(),
            AudioCommand::Play {
                sink,
                channels,
                samples,
                rate,
                gain,
                looped,
            } => {
                self.close_play();
                let data = |shared| PlayData {
                    samples,
                    pos: 0,
                    mask: (0..sink.channels.len())
                        .map(|c| channels.contains(&c))
                        .collect(),
                    gain,
                    looped,
                    finished: false,
                    shared,
                };
                match self.open_play(core, &sink, &channels, rate, data) {
                    Ok(active) => {
                        *self.play.borrow_mut() = Some(active);
                        self.play_waiting.set(Some(Instant::now()));
                    }
                    Err(e) => self.emit(AudioEvent::PlayFailed(format!("{e:#}"))),
                }
            }
            AudioCommand::StopPlay => self.close_play(),
            AudioCommand::Shutdown => self.quit.set(true),
        }
    }

    fn weak(&self) -> Weak<Shared> {
        self.me.clone()
    }

    /// Registry `global`: track audio sources and sinks.
    fn add_node(
        &self,
        registry: &pw::registry::RegistryRc,
        global: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>,
    ) {
        if global.type_ != pw::types::ObjectType::Node {
            return;
        }
        let Some(props) = global.props else { return };
        let Some((is_sink, raw)) = props.get(*pw::keys::MEDIA_CLASS).and_then(classify) else {
            return;
        };
        let Ok(proxy) = registry.bind::<pw::node::Node, _>(global) else {
            return;
        };
        let id = global.id;
        let listener = proxy
            .add_listener_local()
            .info({
                let me = self.weak();
                move |info| {
                    if let (Some(s), Some(props)) = (me.upgrade(), info.props()) {
                        if let Some(t) = s.nodes.borrow_mut().get_mut(&id) {
                            t.update_props(props);
                        }
                        s.touch();
                    }
                }
            })
            .param({
                let me = self.weak();
                move |_, kind, _, _, pod| {
                    if let (Some(s), Some(pod)) = (me.upgrade(), pod) {
                        s.node_param(id, kind, pod);
                    }
                }
            })
            .register();
        proxy.subscribe_params(&[ParamType::EnumFormat, ParamType::Format]);
        let name = props
            .get(*pw::keys::NODE_NAME)
            .unwrap_or_default()
            .to_owned();
        let mut tracked = Tracked {
            is_sink,
            raw,
            serial: String::new(),
            description: name.clone(),
            name,
            prop_position: None,
            prop_channels: None,
            param_channels: None,
            enum_positions: None,
            format_positions: None,
            _listener: listener,
            _proxy: proxy,
        };
        tracked.update_props(props);
        self.nodes.borrow_mut().insert(id, tracked);
        self.touch();
    }

    /// Node `param` event: channel positions and count from `Format` (preferred) or
    /// `EnumFormat`.
    fn node_param(&self, id: u32, kind: ParamType, pod: &spa::pod::Pod) {
        let format = parse_format(pod);
        let format_kind = kind == ParamType::Format;
        if !format_kind && kind != ParamType::EnumFormat {
            return;
        }
        {
            let mut nodes = self.nodes.borrow_mut();
            let Some(t) = nodes.get_mut(&id) else { return };
            let mut changed = false;
            if let Some(count) = format.channels
                && (format_kind || t.param_channels.is_none())
                && t.param_channels != Some(count)
            {
                t.param_channels = Some(count);
                changed = true;
            }
            if let Some(found) = format.positions {
                if format_kind {
                    t.format_positions = Some(found);
                    changed = true;
                } else if t.enum_positions.is_none() {
                    t.enum_positions = Some(found);
                    changed = true;
                }
            }
            if !changed {
                return;
            }
        }
        self.touch();
    }

    fn open_capture(
        &self,
        core: &pw::core::CoreRc,
        input: &Input,
        channel: usize,
    ) -> anyhow::Result<Active<CaptureData>> {
        let node = input.node();
        if !node.channels.is_empty() && channel >= node.channels.len() {
            bail!("{} has no channel {}", input.label(), channel + 1);
        }
        let mut props = stream_props("Capture", "fastgraph_capture", &node.serial);
        if matches!(input, Input::MonitorOf(_)) {
            props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
        }
        let stream = pw::stream::StreamRc::new(core.clone(), "fastgraph capture", props)?;
        let data = CaptureData {
            tx: self.samples.clone(),
            channel,
            // What we asked for; replaced by the negotiated count in `param_changed`.
            channels: node.channels.len(),
            shared: self.weak(),
        };
        let listener = stream
            .add_local_listener_with_user_data(data)
            .state_changed(|_, data, old, new| {
                if let (Some(msg), Some(s)) = (lost_reason(&old, &new), data.shared.upgrade()) {
                    s.capture_lost.borrow_mut().get_or_insert(msg);
                }
            })
            .param_changed(|_, data, id, pod| {
                let Some(pod) = pod.filter(|_| id == ParamType::Format.as_raw()) else {
                    return;
                };
                let Some(shared) = data.shared.upgrade() else {
                    return;
                };
                let format = parse_format(pod);
                if let Some(c) = format.channels {
                    data.channels = c as usize;
                }
                match format.rate {
                    Some(rate) if data.channel < data.channels => {
                        shared.capture_waiting.set(None);
                        shared.emit(AudioEvent::InputStarted { rate })
                    }
                    Some(_) => {
                        shared.capture_lost.borrow_mut().get_or_insert(format!(
                            "the input has {} channels, channel {} is not available",
                            data.channels,
                            data.channel + 1
                        ));
                    }
                    None => {}
                }
            })
            .process(|stream, data| {
                // Take every buffer that is ready, so none wait for the next callback.
                while let Some(mut buffer) = stream.dequeue_buffer() {
                    let Some(d) = buffer.datas_mut().first_mut() else {
                        continue;
                    };
                    let (offset, size) = (d.chunk().offset() as usize, d.chunk().size() as usize);
                    let Some(bytes) = d.data() else { continue };
                    let end = offset.saturating_add(size).min(bytes.len());
                    let block =
                        deinterleave(&bytes[offset.min(end)..end], data.channels, data.channel);
                    if !block.is_empty() {
                        // The receiver may be gone; capture blocks are then dropped.
                        let _ = data.tx.send(block);
                    }
                }
            })
            .register()?;
        let pod = format_pod(None, &node.channels)?;
        stream.connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut [pod.pod().context("invalid stream format")?],
        )?;
        Ok(Active { listener, stream })
    }

    fn open_play(
        &self,
        core: &pw::core::CoreRc,
        sink: &Node,
        channels: &[usize],
        rate: u32,
        data: impl FnOnce(Weak<Shared>) -> PlayData,
    ) -> anyhow::Result<Active<PlayData>> {
        if sink.channels.is_empty() {
            bail!("{} reports no channels", sink.description);
        }
        if let Some(c) = channels.iter().find(|&&c| c >= sink.channels.len()) {
            bail!("{} has no channel {}", sink.description, c + 1);
        }
        if rate == 0 {
            bail!("invalid playback rate 0");
        }
        let props = stream_props("Playback", "fastgraph_playback", &sink.serial);
        let stream = pw::stream::StreamRc::new(core.clone(), "fastgraph playback", props)?;
        let listener = stream
            .add_local_listener_with_user_data(data(self.weak()))
            .state_changed(|_, data, old, new| {
                let Some(s) = data.shared.upgrade() else {
                    return;
                };
                if matches!(new, pw::stream::StreamState::Streaming)
                    && s.play_waiting.take().is_some()
                {
                    s.emit(AudioEvent::PlayStarted);
                }
                if let Some(msg) = lost_reason(&old, &new) {
                    s.play_end.borrow_mut().get_or_insert(PlayEnd::Failed(msg));
                }
            })
            .process(|stream, data| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let requested = buffer.requested() as usize;
                let Some(d) = buffer.datas_mut().first_mut() else {
                    return;
                };
                let stride = 4 * data.mask.len();
                let mut size = 0;
                if let Some(bytes) = d.data() {
                    let mut frames = bytes.len() / stride;
                    if requested > 0 {
                        frames = frames.min(requested);
                    }
                    size = frames * stride;
                    let done = render(
                        &mut bytes[..size],
                        &data.mask,
                        &data.samples,
                        &mut data.pos,
                        data.gain,
                        data.looped,
                    );
                    if done && !data.finished {
                        data.finished = true;
                        if let Some(s) = data.shared.upgrade() {
                            s.play_end.borrow_mut().get_or_insert(PlayEnd::Finished);
                        }
                    }
                }
                let chunk = d.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = stride as i32;
                *chunk.size_mut() = size as u32;
            })
            .register()?;
        let pod = format_pod(Some(rate), &sink.channels)?;
        stream.connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut [pod.pod().context("invalid stream format")?],
        )?;
        Ok(Active { listener, stream })
    }
}

/// Why a stream transition means the stream is gone. We never disconnect while listening,
/// so any return to `Unconnected` is a loss.
fn lost_reason(old: &pw::stream::StreamState, new: &pw::stream::StreamState) -> Option<String> {
    use pw::stream::StreamState as S;
    match (old, new) {
        (_, S::Error(e)) => Some(e.clone()),
        (S::Unconnected, S::Unconnected) => None,
        (_, S::Unconnected) => Some("the device disconnected".into()),
        _ => None,
    }
}

fn stream_props(category: &str, name: &str, serial: &str) -> pw::properties::PropertiesBox {
    properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => category,
        *pw::keys::MEDIA_ROLE => "Production",
        *pw::keys::APP_NAME => "fastgraph",
        *pw::keys::NODE_NAME => name,
        "node.dont-reconnect" => "true",
        "node.dont-fallback" => "true",
        *pw::keys::TARGET_OBJECT => serial,
    }
}

fn run(
    events: EventSink<AudioEvent>,
    samples: mpsc::Sender<Vec<f32>>,
    commands: pw::channel::Receiver<AudioCommand>,
    ready: mpsc::Sender<Result<(), String>>,
) {
    let setup = || -> anyhow::Result<_> {
        pw::init();
        let mainloop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&mainloop, None)?;
        let core = context.connect_rc(None)?;
        let registry = core.get_registry_rc()?;
        Ok((mainloop, context, core, registry))
    };
    let (mainloop, _context, core, registry) = match setup() {
        Ok(v) => v,
        Err(e) => {
            let _ = ready.send(Err(format!("{e:#}")));
            return;
        }
    };
    let shared = Rc::new_cyclic(|me| Shared {
        me: me.clone(),
        samples,
        events: RefCell::new(events),
        commands: RefCell::new(VecDeque::new()),
        quit: Cell::new(false),
        nodes: RefCell::new(BTreeMap::new()),
        changed_at: Cell::new(None),
        last_nodes: RefCell::new(None),
        capture: RefCell::new(None),
        capture_lost: RefCell::new(None),
        capture_waiting: Cell::new(None),
        play: RefCell::new(None),
        play_end: RefCell::new(None),
        play_waiting: Cell::new(None),
        fatal: RefCell::new(None),
    });
    let _core_listener = core
        .add_listener_local()
        .error({
            let me = shared.weak();
            move |id, _seq, res, message| {
                if is_fatal_core_error(id, res)
                    && let Some(s) = me.upgrade()
                {
                    s.fatal
                        .borrow_mut()
                        .get_or_insert_with(|| format!("PipeWire connection failed: {message}"));
                    s.quit.set(true);
                }
            }
        })
        .register();
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let me = shared.weak();
            let registry = registry.downgrade();
            move |global| {
                if let (Some(s), Some(r)) = (me.upgrade(), registry.upgrade()) {
                    s.add_node(&r, global);
                }
            }
        })
        .global_remove({
            let me = shared.weak();
            move |id| {
                if let Some(s) = me.upgrade() {
                    let removed = s.nodes.borrow_mut().remove(&id);
                    if removed.is_some() {
                        s.touch();
                    }
                }
            }
        })
        .register();
    let _commands = commands.attach(mainloop.loop_(), {
        let me = shared.weak();
        move |cmd| {
            if let Some(s) = me.upgrade() {
                s.commands.borrow_mut().push_back(cmd);
            }
        }
    });
    // Report the list once even when there are no audio nodes at all.
    shared.touch();
    let _ = ready.send(Ok(()));
    while !shared.quit.get() {
        mainloop.loop_().iterate(pw::loop_::Timeout::Finite(TICK));
        if shared.fatal.borrow().is_some() {
            break;
        }
        shared.housekeeping(&core);
    }
    let had_capture = shared.capture.borrow().is_some();
    let had_play = shared.play.borrow().is_some();
    shared.close_capture();
    shared.close_play();
    shared.nodes.borrow_mut().clear();
    let fatal = shared.fatal.borrow_mut().take();
    if let Some(message) = fatal {
        for event in fatal_events(&message, had_capture, had_play) {
            shared.emit(event);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use pipewire::spa;

    fn bytes(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    fn floats(b: &[u8]) -> Vec<f32> {
        b.as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect()
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn channel_labels_fallback() {
        assert_eq!(positions(Some("FL,FR"), None), ["FL", "FR"]);
        assert_eq!(positions(Some("[ FL, FR ]"), Some(2)), ["FL", "FR"]);
        assert_eq!(positions(None, Some(3)), ["CH1", "CH2", "CH3"]);
        assert_eq!(positions(None, None), Vec::<String>::new());
        assert_eq!(positions(Some(""), Some(1)), ["CH1"]);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn repeated_or_missing_positions_are_numbered() {
        let labels = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(channel_labels(labels(&["FL", "FR"]), Some(2)), ["FL", "FR"]);
        assert_eq!(
            channel_labels(labels(&["UNK", "UNK", "UNK"]), Some(3)),
            ["CH1", "CH2", "CH3"]
        );
        assert_eq!(
            channel_labels(labels(&["AUX0", "AUX0"]), None),
            ["CH1", "CH2"]
        );
        // Only a count from the format.
        assert_eq!(
            channel_labels(Vec::new(), Some(4)),
            ["CH1", "CH2", "CH3", "CH4"]
        );
        assert_eq!(positions(Some("UNK,UNK"), None), ["CH1", "CH2"]);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn hardware_nodes_behind_split_profiles_are_listed() {
        assert_eq!(classify("Audio/Sink"), Some((true, false)));
        assert_eq!(classify("Audio/Sink/Virtual"), Some((true, false)));
        assert_eq!(classify("Audio/Source"), Some((false, false)));
        assert_eq!(classify("Audio/Source/Virtual"), Some((false, false)));
        // The raw interface nodes a split profile hides.
        assert_eq!(classify("Audio/Source/Internal"), Some((false, true)));
        assert_eq!(classify("Audio/Sink/Internal"), Some((true, true)));
        // Streams, including the split loopbacks, are never devices.
        assert_eq!(classify("Stream/Input/Audio/Internal"), None);
        assert_eq!(classify("Stream/Output/Audio"), None);
        assert_eq!(classify("Video/Source"), None);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn raw_nodes_and_aux_channels_read_as_hardware() {
        let raw = Node {
            id: 81,
            serial: "14666".into(),
            name: "alsa_input.hw_interface_0".into(),
            description: "USB Interface".into(),
            is_sink: false,
            raw: true,
            channels: vec!["AUX0".into(), "AUX1".into(), "AUX2".into(), "AUX3".into()],
        };
        assert_eq!(raw.label(), "USB Interface (all channels)");
        assert_eq!(
            Input::Source(raw.clone()).label(),
            "USB Interface (all channels)"
        );
        assert_eq!(
            Input::MonitorOf(raw).label(),
            "Monitor of USB Interface (all channels)"
        );
        assert_eq!(channel_display("AUX1"), "Ch 2 (AUX1)");
        assert_eq!(channel_display("AUX0"), "Ch 1 (AUX0)");
        assert_eq!(channel_display("AUX15"), "Ch 16 (AUX15)");
        assert_eq!(channel_display("FL"), "FL");
        assert_eq!(channel_display("MONO"), "MONO");
        assert_eq!(channel_display("AUXx"), "AUXx");
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn input_label_and_key() {
        let node = Node {
            id: 7,
            serial: "123".into(),
            name: "alsa_output.usb-dac".into(),
            description: "USB DAC".into(),
            is_sink: true,
            raw: false,
            channels: vec!["FL".into(), "FR".into()],
        };
        assert_eq!(Input::Source(node.clone()).label(), "USB DAC");
        assert_eq!(Input::Source(node.clone()).key(), "alsa_output.usb-dac");
        assert_eq!(Input::MonitorOf(node.clone()).label(), "Monitor of USB DAC");
        assert_eq!(Input::MonitorOf(node).key(), "monitor:alsa_output.usb-dac");
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn stream_positions_need_known_labels() {
        let labels = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let pos = position_array(&labels(&["FL", "FR"])).unwrap();
        assert_eq!(
            &pos[..3],
            &[
                spa::sys::SPA_AUDIO_CHANNEL_FL,
                spa::sys::SPA_AUDIO_CHANNEL_FR,
                0
            ]
        );
        assert_eq!(position_array(&labels(&["CH1", "CH2"])), None);
        assert_eq!(position_array(&[]), None);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn spa_channel_ids_map_to_labels_and_back() {
        assert_eq!(channel_label(spa::sys::SPA_AUDIO_CHANNEL_MONO), "MONO");
        assert_eq!(channel_label(spa::sys::SPA_AUDIO_CHANNEL_FL), "FL");
        assert_eq!(channel_label(spa::sys::SPA_AUDIO_CHANNEL_FR), "FR");
        assert_eq!(channel_label(spa::sys::SPA_AUDIO_CHANNEL_BRC), "BRC");
        assert_eq!(channel_label(spa::sys::SPA_AUDIO_CHANNEL_AUX0), "AUX0");
        assert_eq!(
            channel_label(spa::sys::SPA_AUDIO_CHANNEL_AUX0 + 17),
            "AUX17"
        );
        assert_eq!(channel_label(9999), "UNK");
        for id in (2..=spa::sys::SPA_AUDIO_CHANNEL_BRC).chain(4096..4200) {
            assert_eq!(channel_id(&channel_label(id)), Some(id), "{id}");
        }
        assert_eq!(channel_id("CH1"), None);
        assert_eq!(channel_id("UNK"), None);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn deinterleave_selects_one_channel() {
        let b = bytes(&[1.0, -1.0, 2.0, -2.0, 3.0, -3.0, 9.0]);
        assert_eq!(deinterleave(&b, 2, 0), [1.0, 2.0, 3.0]);
        assert_eq!(deinterleave(&b, 2, 1), [-1.0, -2.0, -3.0]);
        assert_eq!(deinterleave(&b[..8], 1, 0), [1.0, -1.0]);
        assert!(deinterleave(&b, 2, 2).is_empty());
        assert!(deinterleave(&b, 0, 0).is_empty());
    }

    fn serialize(value: &spa::pod::Value) -> Vec<u8> {
        spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), value)
            .unwrap()
            .0
            .into_inner()
    }

    fn format_pod(id: spa::param::ParamType, properties: Vec<spa::pod::Property>) -> Vec<u8> {
        serialize(&spa::pod::Value::Object(spa::pod::Object {
            type_: spa::sys::SPA_TYPE_OBJECT_Format,
            id: id.as_raw(),
            properties,
        }))
    }

    fn parse(bytes: &[u8]) -> FormatInfo {
        parse_format(PodBuf::new(bytes).pod().unwrap())
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn reads_rate_channels_and_positions_from_format_pods() {
        let mut info = spa::param::audio::AudioInfoRaw::new();
        info.set_format(spa::param::audio::AudioFormat::F32LE);
        info.set_rate(44_100);
        info.set_channels(2);
        let mut pos = [0; spa::param::audio::MAX_CHANNELS];
        pos[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
        pos[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
        info.set_position(pos);
        let pod = format_pod(spa::param::ParamType::Format, info.into());
        assert_eq!(
            parse(&pod),
            FormatInfo {
                rate: Some(44_100),
                channels: Some(2),
                positions: Some(vec!["FL".into(), "FR".into()]),
            }
        );

        // EnumFormat with a rate range and no positions.
        let mut info = spa::param::audio::AudioInfoRaw::new();
        info.set_format(spa::param::audio::AudioFormat::F32LE);
        info.set_channels(1);
        let mut props: Vec<spa::pod::Property> = info.into();
        props.push(spa::pod::Property::new(
            spa::sys::SPA_FORMAT_AUDIO_rate,
            spa::pod::Value::Choice(spa::pod::ChoiceValue::Int(spa::utils::Choice(
                spa::utils::ChoiceFlags::empty(),
                spa::utils::ChoiceEnum::Range {
                    default: 48_000,
                    min: 1,
                    max: 384_000,
                },
            ))),
        ));
        let pod = format_pod(spa::param::ParamType::EnumFormat, props);
        assert_eq!(
            parse(&pod),
            FormatInfo {
                rate: None,
                channels: Some(1),
                positions: None,
            }
        );
        assert_eq!(
            parse(&serialize(&spa::pod::Value::Int(5))),
            FormatInfo::default()
        );
    }

    /// `Format` param of a capture stream as PipeWire 1.6.11 delivered it: every value is
    /// wrapped in a `None` choice, the positions in a choice of an Id array.
    const NEGOTIATED_FORMAT: [u8; 272] = [
        0x08, 0x01, 0, 0, 0x0f, 0, 0, 0, 0x03, 0, 0x04, 0, 0x03, 0, 0, 0, //
        0x01, 0, 0, 0, 0, 0, 0, 0, 0x14, 0, 0, 0, 0x13, 0, 0, 0, //
        0, 0, 0, 0, 0, 0, 0, 0, 0x04, 0, 0, 0, 0x03, 0, 0, 0, //
        0x01, 0, 0, 0, 0, 0, 0, 0, 0x02, 0, 0, 0, 0, 0, 0, 0, //
        0x14, 0, 0, 0, 0x13, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
        0x04, 0, 0, 0, 0x03, 0, 0, 0, 0x01, 0, 0, 0, 0, 0, 0, 0, //
        0x01, 0, 0x01, 0, 0, 0, 0, 0, 0x14, 0, 0, 0, 0x13, 0, 0, 0, //
        0, 0, 0, 0, 0, 0, 0, 0, 0x04, 0, 0, 0, 0x03, 0, 0, 0, //
        0x1b, 0x01, 0, 0, 0, 0, 0, 0, 0x04, 0, 0x01, 0, 0, 0, 0, 0, //
        0x14, 0, 0, 0, 0x13, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
        0x04, 0, 0, 0, 0x04, 0, 0, 0, 0x02, 0, 0, 0, 0, 0, 0, 0, //
        0x05, 0, 0x01, 0, 0, 0, 0, 0, 0x20, 0, 0, 0, 0x13, 0, 0, 0, //
        0, 0, 0, 0, 0, 0, 0, 0, 0x10, 0, 0, 0, 0x0d, 0, 0, 0, //
        0x04, 0, 0, 0, 0x03, 0, 0, 0, 0x03, 0, 0, 0, 0x04, 0, 0, 0, //
        0x03, 0, 0x01, 0, 0, 0, 0, 0, 0x1c, 0, 0, 0, 0x13, 0, 0, 0, //
        0, 0, 0, 0, 0, 0, 0, 0, 0x04, 0, 0, 0, 0x04, 0, 0, 0, //
        0x80, 0xbb, 0, 0, 0x01, 0, 0, 0, 0xff, 0xff, 0xff, 0x7f, 0, 0, 0, 0,
    ];

    // Bugs caught: 0, False positives: 0
    #[test]
    fn reads_negotiated_format_wrapped_in_none_choices() {
        assert_eq!(
            parse(&NEGOTIATED_FORMAT),
            FormatInfo {
                rate: Some(48_000),
                channels: Some(2),
                positions: Some(vec!["FL".into(), "FR".into()]),
            }
        );
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn pod_buffers_are_aligned_and_zero_padded() {
        let buf = PodBuf::new(&[1, 2, 3]);
        assert_eq!(buf.bytes().as_ptr() as usize % 8, 0);
        assert_eq!(buf.bytes(), [1, 2, 3, 0, 0, 0, 0, 0]);
        assert_eq!(PodBuf::new(&NEGOTIATED_FORMAT).bytes(), NEGOTIATED_FORMAT);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn only_stale_resource_errors_on_the_core_are_survivable() {
        let core = pw::core::PW_ID_CORE;
        assert!(is_fatal_core_error(core, -32)); // EPIPE: connection lost
        assert!(is_fatal_core_error(core, -71)); // EPROTO
        assert!(is_fatal_core_error(core, -5)); // EIO
        // Observed live on PipeWire 1.6.11 after a capture target vanished:
        // id=0 res=-2 "unknown resource 9 op:3".
        assert!(!is_fatal_core_error(core, -2));
        // Errors on our own proxies (e.g. "defined target not found" on the stream).
        assert!(!is_fatal_core_error(9, -2));
        assert!(!is_fatal_core_error(9, -32));
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn streams_time_out_ten_seconds_after_opening() {
        let t0 = Instant::now();
        assert!(!start_timed_out(None, t0 + Duration::from_secs(60)));
        assert!(!start_timed_out(Some(t0), t0));
        assert!(!start_timed_out(
            Some(t0),
            t0 + Duration::from_millis(9_999)
        ));
        assert!(start_timed_out(Some(t0), t0 + Duration::from_secs(10)));
        assert!(start_timed_out(Some(t0), t0 + Duration::from_secs(15)));
        // A clock reading before the start never counts as a timeout.
        assert!(!start_timed_out(Some(t0 + Duration::from_secs(1)), t0));
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn fatal_exit_reports_open_streams_before_the_error() {
        let names = |events: Vec<AudioEvent>| {
            events
                .into_iter()
                .map(|e| match e {
                    AudioEvent::InputLost(m) => format!("InputLost:{m}"),
                    AudioEvent::PlayFailed(m) => format!("PlayFailed:{m}"),
                    AudioEvent::Error(m) => format!("Error:{m}"),
                    other => panic!("unexpected {other:?}"),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(fatal_events("gone", true, true)),
            ["InputLost:gone", "PlayFailed:gone", "Error:gone"]
        );
        assert_eq!(
            names(fatal_events("gone", false, true)),
            ["PlayFailed:gone", "Error:gone"]
        );
        assert_eq!(names(fatal_events("gone", false, false)), ["Error:gone"]);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn render_writes_scaled_clamped_samples_into_selected_channels() {
        let samples = [0.5, -0.9, 0.2];
        let mask = [true, false, true];
        let mut out = vec![0xAAu8; 4 * 3 * 4];
        let mut pos = 0;
        let done = render(&mut out, &mask, &samples, &mut pos, 2.0, false);
        assert!(done);
        assert_eq!(
            floats(&out),
            [1.0, 0.0, 1.0, -1.0, 0.0, -1.0, 0.4, 0.0, 0.4, 0.0, 0.0, 0.0]
        );

        // Not finished while samples remain; continues where it stopped.
        let mut out = vec![0u8; 4 * 2];
        let mut pos = 0;
        assert!(!render(&mut out, &[true], &samples, &mut pos, 1.0, false));
        assert_eq!(floats(&out), [0.5, -0.9]);
        assert!(render(&mut out, &[true], &samples, &mut pos, 1.0, false));
        assert_eq!(floats(&out), [0.2, 0.0]);

        // Looped playback wraps around and never finishes.
        let mut out = vec![0u8; 4 * 7];
        let mut pos = 0;
        assert!(!render(&mut out, &[true], &samples, &mut pos, 1.0, true));
        assert_eq!(floats(&out), [0.5, -0.9, 0.2, 0.5, -0.9, 0.2, 0.5]);
        let mut out = vec![0u8; 4 * 2];
        assert!(!render(&mut out, &[true], &[], &mut 0, 1.0, true));
        assert_eq!(floats(&out), [0.0, 0.0]);
    }
}
