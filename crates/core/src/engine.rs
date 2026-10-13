//! The session engine: one thread that owns the UDP socket, finds peers,
//! sets up and tears down a link, and feeds received audio to the jitter
//! buffer. Front ends talk to it through [`Engine`] and hear back through
//! [`Event`]s.
//!
//! A link is symmetric. Each end decides for itself what it captures and
//! sends ([`Config::send`]) and where what arrives is played
//! ([`Config::play`]); nothing a peer says can make this end start
//! recording. "Phone mic to PC" is the phone sending its mic and the PC
//! playing; "PC audio to phone" is the reverse; both at once is both.
//!
//! Either list can be any length. Every source is its own stream, known at
//! both ends by the id of that source ([`SourceConfig::id`]), which on a
//! desktop is the id of the device it is captured from. An arriving stream
//! is played on the sinks that name its source, or failing those on each
//! sink that names none.
//!
//! How a microphone is filtered and levelled is a setting of the end that
//! sends it, but it is the other end that hears the result. So each end
//! tells the other how its sources are set, and takes requests to set a
//! microphone differently ([`Engine::set_remote`], [`Event::SourceChanged`]).

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};

use crate::audio::{
    AudioBackend, CaptureRequest, MakeCapture, MakeRender, PlaybackRequest, SinkKind, SourceKind,
    StreamFormat, StreamHandle,
};
pub use crate::playout::BufferMode;
use crate::playout::{self, PlayoutWriter};
use crate::protocol::{
    self, AudioHeader, BYE_BUSY, BYE_NORMAL, CAP_IDENTIFIES, CAP_PLAYS, CAP_SENDS, MAX_STREAMS, Packet,
    Report, SourceSettings,
};
use crate::sender::{Packetizer, StreamSpec, TxShared};
use crate::{denoise, gain, synthetic};

const HELLO_INTERVAL: Duration = Duration::from_millis(250);
const PING_INTERVAL: Duration = Duration::from_millis(500);
const STATS_INTERVAL: Duration = Duration::from_millis(250);
const DISCOVER_INTERVAL: Duration = Duration::from_secs(2);
/// A peer that has not answered a probe for this long is dropped from the list.
const PEER_TTL: Duration = Duration::from_secs(7);
/// Pings arrive twice a second, so this much silence means the peer is gone.
const SESSION_TIMEOUT: Duration = Duration::from_secs(3);
const RETRY_DEVICE_AFTER: Duration = Duration::from_secs(2);
/// An incoming stream this quiet has been stopped by its sender, and its
/// output is closed.
const STREAM_IDLE: Duration = Duration::from_secs(2);
/// How long the peer is asked to change a source before giving up on it.
const ASK_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq)]
pub struct SourceConfig {
    /// What the source is known by for good, here and at the other end,
    /// which gives the stream its place by it: see [`SourceConfig::id_of`].
    /// Of two sources with the same id only the first is sent.
    pub id: String,
    /// What the peer is told to show the stream as; empty for nothing. It
    /// can change without the stream becoming another.
    pub name: String,
    pub kind: SourceKind,
    /// Backend device id, or `None` for the default.
    pub device: Option<String>,
    /// Channels to send. `None` picks mono for a mic, stereo for system audio.
    pub channels: Option<u16>,
    /// How much besides speech is removed: background noise, or keys,
    /// clicks and breathing as well (see [`crate::denoise`]). Anything but
    /// off makes the stream mono.
    pub denoise: denoise::Mode,
    /// Bring speech to a steady level, however loud the microphone is (see
    /// [`crate::gain`]).
    pub auto_gain: bool,
    /// Gain in dB: on top of the automatic one, or on its own. Anything but
    /// 0 also puts a limiter on the stream.
    pub gain_db: f32,
}

impl SourceConfig {
    /// The default device of this kind, under the id that has.
    pub fn new(kind: SourceKind) -> Self {
        SourceConfig {
            id: SourceConfig::id_of(kind, None),
            name: String::new(),
            kind,
            device: None,
            channels: None,
            denoise: denoise::Mode::Off,
            auto_gain: false,
            gain_db: 0.0,
        }
    }

    /// The id of a source that is captured from `device`: the id of that
    /// device, or for the default one what kind of source it is.
    pub fn id_of(kind: SourceKind, device: Option<&str>) -> String {
        device.unwrap_or(kind_word(kind)).to_string()
    }

    /// Whether capture runs the same for `other`, which a name does not change.
    fn captures_like(&self, other: &SourceConfig) -> bool {
        let unnamed = |source: &SourceConfig| SourceConfig { name: String::new(), ..source.clone() };
        unnamed(self) == unnamed(other)
    }

    /// What the peer is told about this source, and can ask to have changed.
    fn settings(&self, slot: u8) -> SourceSettings {
        SourceSettings {
            slot,
            kind: self.kind,
            denoise: self.denoise,
            auto_gain: self.auto_gain,
            gain_db: self.gain_db,
        }
    }
}

/// The id of a default device, and of a stream whose sender is too old to
/// say where it comes from.
fn kind_word(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Mic => "mic",
        SourceKind::System => "system",
        SourceKind::Tone => "tone",
    }
}

/// Gives each source a slot to be sent in: the one it had in `old` if it was
/// sent before, or else the next after `next` that is free, so that a slot
/// is not soon taken over by another source. A source whose id an earlier
/// one has, or for which no slot is left, gets none.
fn assign_slots(old: &[(String, Option<u8>)], send: &[SourceConfig], next: &mut u8) -> Vec<Option<u8>> {
    let mut slots: Vec<Option<u8>> = Vec::with_capacity(send.len());
    let had = |id: &str| old.iter().find(|(old, _)| old == id).and_then(|(_, slot)| *slot);
    // Kept slots first, so that no newcomer is given one of them.
    for (at, source) in send.iter().enumerate() {
        let first = send[..at].iter().all(|earlier| earlier.id != source.id);
        slots.push(if first { had(&source.id) } else { None });
    }
    for (at, source) in send.iter().enumerate() {
        let first = send[..at].iter().all(|earlier| earlier.id != source.id);
        if !first || slots[at].is_some() {
            continue;
        }
        let mut from_next = (0..MAX_STREAMS as u8).map(|step| (*next + step) % MAX_STREAMS as u8);
        if let Some(slot) = from_next.find(|slot| !slots.contains(&Some(*slot))) {
            slots[at] = Some(slot);
            *next = (slot + 1) % MAX_STREAMS as u8;
        }
    }
    slots
}

/// A peer's request, brought within what a source can be set to.
fn sanitized(settings: SourceSettings) -> SourceSettings {
    let gain_db = match settings.gain_db {
        db if db.is_finite() => db.clamp(*gain::OFFSET_RANGE_DB.start(), *gain::OFFSET_RANGE_DB.end()),
        _ => 0.0,
    };
    SourceSettings { denoise: settings.denoise.clamped(), gain_db, ..settings }
}

/// Which of the peer's streams a sink plays.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Streams {
    /// Those that no sink names.
    #[default]
    All,
    /// The one from the source the peer knows by this id (see
    /// [`RemoteStream::id`]). A stream that any sink names this way is
    /// played on such sinks only.
    Source(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct SinkConfig {
    pub kind: SinkKind,
    pub device: Option<String>,
    pub from: Streams,
}

impl SinkConfig {
    pub fn new(kind: SinkKind) -> Self {
        SinkConfig { kind, device: None, from: Streams::All }
    }

    /// Whether the stream from the source `id` is played here; `named` says
    /// that some sink asks for that source.
    fn takes(&self, id: &str, named: bool) -> bool {
        self.kind != SinkKind::Nothing
            && match &self.from {
                Streams::Source(wanted) => wanted == id,
                Streams::All => !named,
            }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// How this machine is shown to others.
    pub name: String,
    /// UDP port to bind; 0 picks any free one.
    pub port: u16,
    /// Fall back to any free port when `port` is taken, instead of failing.
    /// Such an instance can still connect out, but cannot be discovered.
    pub port_fallback: bool,
    /// Answer discovery probes and accept incoming connections.
    pub listen: bool,
    /// Probe the local network for listening peers while idle.
    pub discover: bool,
    /// What to capture and send, one stream each, told apart by
    /// [`SourceConfig::id`]. At most [`MAX_STREAMS`](protocol::MAX_STREAMS)
    /// are sent.
    pub send: Vec<SourceConfig>,
    /// Where received streams go. A stream is played on every sink that
    /// names its source, or with none that does on every sink that names
    /// none, and dropped if there is none of either.
    pub play: Vec<SinkConfig>,
    /// Audio per packet. Smaller is lower latency and more packets.
    pub packet_ms: f32,
    pub buffer: BufferMode,
    /// Callback size asked of audio devices, in frames.
    pub period_frames: u32,
    pub sample_rate: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            name: "SoundBridge".into(),
            port: protocol::DEFAULT_PORT,
            port_fallback: false,
            listen: true,
            discover: false,
            send: Vec::new(),
            play: vec![SinkConfig::new(SinkKind::Speaker)],
            packet_ms: 2.5,
            buffer: BufferMode::default(),
            period_frames: 128,
            sample_rate: 48_000,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    Connecting { addr: SocketAddr },
    Connected { addr: SocketAddr, name: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    pub addr: SocketAddr,
    pub name: String,
    /// Already linked to someone else.
    pub busy: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TxStats {
    /// Index of the source in [`Config::send`].
    pub source: usize,
    /// The slot it is sent in.
    pub slot: u8,
    pub kind: SourceKind,
    pub format: StreamFormat,
    pub packet_ms: f32,
    pub packets: u64,
    /// Peak level sent since the previous report, 0..=1.
    pub level: f32,
    /// The noise suppression running on this stream.
    pub denoise: denoise::Mode,
    /// The peer's view of this stream, if it is playing it.
    pub remote: Option<Report>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RxStats {
    /// The peer's slot for this stream; [`Stats::remote_stream`] says which
    /// of its sources that is.
    pub slot: u8,
    pub kind: SourceKind,
    /// Where it is being played.
    pub sink: SinkKind,
    pub format: StreamFormat,
    pub device_format: StreamFormat,
    pub playing: bool,
    /// Time audio currently spends in the jitter buffer.
    pub buffer_ms: f32,
    /// The jitter margin being aimed for.
    pub target_ms: f32,
    pub jitter_ms: f32,
    pub packets: u64,
    pub lost: u32,
    pub late: u32,
    pub underruns: u32,
    pub skips: u32,
    pub drift_ppm: i32,
    pub level: f32,
}

/// One of the streams the peer sends, as it identifies it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteStream {
    /// The slot it arrives in, which is what [`RxStats`] and
    /// [`SourceSettings`] know it by. Good for this session only.
    pub slot: u8,
    /// What the peer knows the source by for good: what
    /// [`Streams::Source`] gives the stream its place by. A peer too old to
    /// say is taken to send its default microphone or device audio.
    pub id: String,
    /// What the peer calls it, if anything. This can change.
    pub name: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
    pub rtt_ms: Option<f32>,
    /// The streams being sent. A configured source that is missing here
    /// could not be opened.
    pub tx: Vec<TxStats>,
    /// The streams being played, once per sink each goes to.
    pub rx: Vec<RxStats>,
    /// Everything the peer sends, played here or not, and how each is set
    /// there. A change asked for with [`Engine::set_remote`] shows at once,
    /// and is taken back if the peer does not go along with it. Empty for a
    /// peer too old to say.
    pub remote_sources: Vec<SourceSettings>,
    /// Which of its sources each of those streams comes from, and each of
    /// the ones being played.
    pub remote_streams: Vec<RemoteStream>,
}

impl Stats {
    /// The source of the stream the peer sends in `slot`, once it is known.
    pub fn remote_stream(&self, slot: u8) -> Option<&RemoteStream> {
        self.remote_streams.iter().find(|stream| stream.slot == slot)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    State(State),
    Stats(Stats),
    /// The current set of discovered peers.
    Peers(Vec<Peer>),
    /// The peer changed how one of this end's sources is sent, and the
    /// engine has done so. The front end should take the change into its own
    /// settings: the next [`Engine::configure`] is what counts again.
    /// `source` is the index of the source in [`Config::send`].
    SourceChanged { source: usize, settings: SourceSettings },
    /// Something worth telling the user that is not a failure.
    Info(String),
    /// Something failed; the engine keeps running.
    Error(String),
}

enum Command {
    Connect(SocketAddr),
    Disconnect,
    Configure(Box<Config>),
    SetRemote(SourceSettings),
    Shutdown,
}

/// Handle to a running engine. Dropping it ends any session and stops the thread.
pub struct Engine {
    tx: Sender<Command>,
    thread: Option<JoinHandle<()>>,
    local_port: u16,
}

impl Engine {
    /// Binds the socket and starts the engine thread. `on_event` is called
    /// from that thread.
    pub fn start(
        config: Config,
        backend: Arc<dyn AudioBackend>,
        on_event: impl Fn(Event) + Send + 'static,
    ) -> io::Result<Engine> {
        let socket = match bind(config.port) {
            Err(e) if e.kind() == io::ErrorKind::AddrInUse && config.port_fallback => bind(0)?,
            other => other?,
        };
        let local_port = socket.local_addr()?.port();
        let (tx, rx) = mpsc::channel();
        let mut next_slot = 0;
        let worker = Worker {
            socket: Arc::new(socket),
            backend,
            on_event: Box::new(on_event),
            slots: assign_slots(&[], &config.send, &mut next_slot),
            next_slot,
            config,
            commands: rx,
            instance: random_u32(),
            epoch: Instant::now(),
            phase: Phase::Idle,
            peers: Vec::new(),
            last_discover: None,
        };
        let thread = thread::Builder::new().name("sb-engine".into()).spawn(move || worker.run())?;
        Ok(Engine { tx, thread: Some(thread), local_port })
    }

    /// The port actually bound, which differs from the configured one after
    /// a fallback.
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    /// Starts connecting to a peer, replacing any current session. Keeps
    /// trying until it answers or [`disconnect`](Self::disconnect) is called.
    pub fn connect(&self, addr: SocketAddr) {
        let _ = self.tx.send(Command::Connect(addr));
    }

    pub fn disconnect(&self) {
        let _ = self.tx.send(Command::Disconnect);
    }

    /// Applies new settings. Changes to what is sent or played take effect on
    /// a live session; the port cannot be changed after start.
    pub fn configure(&self, config: Config) {
        let _ = self.tx.send(Command::Configure(Box::new(config)));
    }

    /// Asks the peer to filter and level one of the microphones it sends
    /// this way, as if that had been set there. `settings` names the stream
    /// by the slot and kind it has in [`Stats::remote_sources`]; a request
    /// for anything that is not listed there is dropped.
    pub fn set_remote(&self, settings: SourceSettings) {
        let _ = self.tx.send(Command::SetRemote(settings));
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn bind(port: u16) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_broadcast(true)?;
    // DSCP "expedited forwarding". Wi-Fi maps it to the voice access
    // category, which gets shorter contention windows than bulk traffic.
    let _ = socket.set_tos_v4(0xB8);
    let _ = socket.set_recv_buffer_size(1 << 20);
    socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())?;
    let socket: UdpSocket = socket.into();
    // Bounds how long a command or timer waits when no packets are arriving.
    socket.set_read_timeout(Some(Duration::from_millis(20)))?;
    Ok(socket)
}

/// Control packets only; audio goes out through the [`Packetizer`].
fn send(socket: &UdpSocket, packet: Packet, to: SocketAddr) {
    let _ = socket.send_to(&packet.to_vec(), to);
}

fn random_u32() -> u32 {
    RandomState::new().build_hasher().finish() as u32
}

/// Best effort: on Linux and Android a thread may raise its own priority if
/// the rlimit allows, which Android grants to apps and desktops usually don't.
fn raise_thread_priority() {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    // SAFETY: plain syscall on the calling thread; failure is ignored.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, -16);
    }
}

struct Tx {
    stream: Box<dyn StreamHandle>,
    kind: SourceKind,
    shared: Arc<TxShared>,
    format: StreamFormat,
    packet_ms: f32,
    denoise: denoise::Mode,
}

/// One configured source; its index in `Session::tx` is the source's in
/// `Config::send`.
#[derive(Default)]
struct TxSlot {
    open: Option<Tx>,
    /// When to (re)try opening the device; `None` while it is open.
    retry_at: Option<Instant>,
    /// The last device error shown, so a retry loop reports it only once.
    last_error: Option<String>,
}

impl TxSlot {
    /// A source `tend_streams` should open as soon as it runs.
    fn pending(now: Instant) -> Self {
        TxSlot { retry_at: Some(now), ..TxSlot::default() }
    }
}

struct Rx {
    stream: Box<dyn StreamHandle>,
    stream_id: u32,
    format: StreamFormat,
    device_format: StreamFormat,
    writer: PlayoutWriter,
    shared: Arc<playout::Shared>,
}

/// One incoming stream on one of the sinks it goes to.
struct RxSlot {
    slot: u8,
    /// Index into `Config::play`.
    sink: usize,
    kind: SourceKind,
    open: Option<Rx>,
    /// Don't hammer a device that just failed to open.
    retry_at: Option<Instant>,
    last_error: Option<String>,
    last_packet: Instant,
}

struct Session {
    peer: SocketAddr,
    name: String,
    /// We dialled; if the link drops, we are the one to redial.
    initiator: bool,
    last_heard: Instant,
    last_ping: Instant,
    last_stats: Instant,
    tx: Vec<TxSlot>,
    rx: Vec<RxSlot>,
    rtt_us: Option<f32>,
    /// The peer's latest reports on the streams we send it.
    remote: Vec<Report>,
    /// The peer says which source each of its streams comes from.
    identifies: bool,
    /// What the peer last said it sends.
    remote_sources: Vec<SourceSettings>,
    /// Which of its sources it said each of those comes from.
    remote_streams: Vec<RemoteStream>,
    /// Changes to those that it has been asked for and has not yet made,
    /// with when each was first asked.
    asks: Vec<(SourceSettings, Instant)>,
}

impl Session {
    /// The peer's sources as they will be once it has done what it was asked.
    fn remote_sources(&self) -> Vec<SourceSettings> {
        let asked = |source: &SourceSettings| {
            self.asks.iter().map(|(ask, _)| *ask).find(|ask| (ask.slot, ask.kind) == (source.slot, source.kind))
        };
        self.remote_sources.iter().map(|source| asked(source).unwrap_or(*source)).collect()
    }


    /// The peer's streams with their sources, as far as those are known.
    fn remote_streams(&self) -> Vec<RemoteStream> {
        let sent = self.remote_sources.iter().map(|source| (source.slot, source.kind));
        let mut streams = self.remote_streams.clone();
        for (slot, kind) in sent.chain(self.rx.iter().map(|rx| (rx.slot, rx.kind))) {
            if streams.iter().all(|stream| stream.slot != slot)
                && let Some(id) = source_of(&self.remote_streams, self.identifies, slot, kind)
            {
                streams.push(RemoteStream { slot, id: id.to_string(), name: String::new() });
            }
        }
        streams
    }
}

/// The source of the stream a peer sends in `slot`, which carries `kind`:
/// what it said (`known`), or for one that does not say (`identifies`), its
/// default source of that kind. `None` while a peer that does has yet to.
fn source_of(known: &[RemoteStream], identifies: bool, slot: u8, kind: SourceKind) -> Option<&str> {
    match known.iter().find(|stream| stream.slot == slot) {
        Some(stream) => Some(&stream.id),
        None if identifies => None,
        None => Some(kind_word(kind)),
    }
}

enum Phase {
    Idle,
    Connecting { addr: SocketAddr, last_hello: Option<Instant> },
    Connected(Box<Session>),
}

struct Worker {
    socket: Arc<UdpSocket>,
    backend: Arc<dyn AudioBackend>,
    on_event: Box<dyn Fn(Event) + Send>,
    config: Config,
    /// The slot each source in `config.send` is sent in; `None` for one
    /// that is not sent.
    slots: Vec<Option<u8>>,
    /// Where the search for a free slot starts.
    next_slot: u8,
    commands: Receiver<Command>,
    /// Lets us recognise our own discovery broadcasts.
    instance: u32,
    epoch: Instant,
    phase: Phase,
    peers: Vec<(Peer, Instant)>,
    last_discover: Option<Instant>,
}

impl Worker {
    fn run(mut self) {
        raise_thread_priority();
        self.report_unsent();
        let mut buf = [0u8; 2048];
        loop {
            loop {
                match self.commands.try_recv() {
                    Ok(Command::Shutdown) | Err(mpsc::TryRecvError::Disconnected) => {
                        self.hang_up();
                        return;
                    }
                    Ok(cmd) => self.command(cmd),
                    Err(mpsc::TryRecvError::Empty) => break,
                }
            }
            self.tick(Instant::now());
            match self.socket.recv_from(&mut buf) {
                Ok((n, from)) => {
                    if let Some(packet) = protocol::decode(&buf[..n]) {
                        self.packet(packet, from, Instant::now());
                    }
                }
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                // Windows reports a peer's ICMP "port unreachable" here; any
                // persistent error must not turn this loop into a spin.
                Err(_) => thread::sleep(Duration::from_millis(5)),
            }
        }
    }

    /// Callers holding a borrow of `self.phase` call `self.on_event` directly.
    fn emit(&self, event: Event) {
        (self.on_event)(event);
    }

    fn caps(&self) -> u8 {
        let plays = self.config.play.iter().any(|sink| sink.kind != SinkKind::Nothing);
        let sends = if self.config.send.is_empty() { 0 } else { CAP_SENDS };
        sends | (if plays { CAP_PLAYS } else { 0 }) | CAP_IDENTIFIES
    }

    /// Says which of the configured sources have no stream of their own.
    fn report_unsent(&self) {
        for (source, _) in self.config.send.iter().zip(&self.slots).filter(|(_, slot)| slot.is_none()) {
            self.emit(Event::Error(format!(
                "\"{}\" is sent only once: each stream needs a source of its own",
                if source.name.is_empty() { &source.id } else { &source.name }
            )));
        }
    }

    fn command(&mut self, cmd: Command) {
        match cmd {
            Command::Connect(addr) => {
                self.hang_up();
                self.phase = Phase::Connecting { addr, last_hello: None };
                self.emit(Event::State(State::Connecting { addr }));
            }
            Command::Disconnect => {
                if !matches!(self.phase, Phase::Idle) {
                    self.hang_up();
                    self.emit(Event::State(State::Idle));
                }
            }
            Command::Configure(new) => {
                let old = std::mem::replace(&mut self.config, *new);
                let had: Vec<_> = old.send.iter().map(|source| source.id.clone()).zip(self.slots.drain(..)).collect();
                self.slots = assign_slots(&had, &self.config.send, &mut self.next_slot);
                if old.send != self.config.send {
                    self.report_unsent();
                }
                let all_tx_changed = old.packet_ms != self.config.packet_ms
                    || old.period_frames != self.config.period_frames
                    || old.sample_rate != self.config.sample_rate;
                let rx_changed = old.play != self.config.play
                    || old.buffer != self.config.buffer
                    || old.period_frames != self.config.period_frames;
                if let Phase::Connected(session) = &mut self.phase {
                    if rx_changed {
                        // Reopened by the next audio packets.
                        session.rx.clear();
                    }
                    // Sources that are the same as before keep running,
                    // wherever in the list they are now. The rest are closed
                    // here, and `tend_streams` opens the new ones.
                    let now = Instant::now();
                    let mut running = std::mem::take(&mut session.tx);
                    session.tx = (self.config.send.iter().zip(&self.slots))
                        .map(|(source, slot)| {
                            let was = had.iter().position(|(id, had)| *id == source.id && had.is_some() && had == slot);
                            was.filter(|&was| !all_tx_changed && old.send[was].captures_like(source))
                                .and_then(|was| running.get_mut(was).map(std::mem::take))
                                .unwrap_or_else(|| TxSlot::pending(now))
                        })
                        .collect();
                    if old.send != self.config.send {
                        self.advertise();
                    }
                }
            }
            Command::SetRemote(settings) => {
                let settings = sanitized(settings);
                let Phase::Connected(session) = &mut self.phase else { return };
                let same = |s: &SourceSettings| (s.slot, s.kind) == (settings.slot, settings.kind);
                if !session.remote_sources.iter().any(same) {
                    return;
                }
                session.asks.retain(|(ask, _)| ask.slot != settings.slot);
                session.asks.push((settings, Instant::now()));
                send(&self.socket, Packet::Set { source: settings }, session.peer);
            }
            Command::Shutdown => unreachable!("handled by the run loop"),
        }
    }

    /// Ends whatever is in progress, telling the peer, and goes idle.
    fn hang_up(&mut self) {
        let to = match std::mem::replace(&mut self.phase, Phase::Idle) {
            Phase::Idle => return,
            Phase::Connecting { addr, .. } => addr,
            // Dropping the session closes its audio streams.
            Phase::Connected(session) => session.peer,
        };
        // Sent twice: it is the one packet whose loss leaves the peer
        // waiting out a timeout.
        for _ in 0..2 {
            send(&self.socket, Packet::Bye { reason: BYE_NORMAL }, to);
        }
    }

    fn tick(&mut self, now: Instant) {
        let due = |last: Option<Instant>, every: Duration| last.is_none_or(|t| now - t >= every);
        match &mut self.phase {
            Phase::Idle => {
                if self.config.discover && due(self.last_discover, DISCOVER_INTERVAL) {
                    self.last_discover = Some(now);
                    self.probe();
                    let before = self.peers.len();
                    self.peers.retain(|(_, seen)| now - *seen < PEER_TTL);
                    if self.peers.len() != before {
                        self.emit_peers();
                    }
                }
            }
            Phase::Connecting { addr, last_hello } => {
                if due(*last_hello, HELLO_INTERVAL) {
                    *last_hello = Some(now);
                    let addr = *addr;
                    let (name, caps) = (self.config.name.clone(), self.caps());
                    send(&self.socket, Packet::Hello { name: &name, caps }, addr);
                }
            }
            Phase::Connected(session) => {
                if now - session.last_heard > SESSION_TIMEOUT {
                    let (peer, name, redial) = (session.peer, session.name.clone(), session.initiator);
                    self.phase = Phase::Idle;
                    if redial {
                        self.emit(Event::Info(format!("Lost {name}, reconnecting")));
                        self.phase = Phase::Connecting { addr: peer, last_hello: None };
                        self.emit(Event::State(State::Connecting { addr: peer }));
                    } else {
                        self.emit(Event::Info(format!("Lost {name}")));
                        self.emit(Event::State(State::Idle));
                    }
                    return;
                }
                if now - session.last_ping >= PING_INTERVAL {
                    session.last_ping = now;
                    // A stream on several sinks is reported as the first of them.
                    let mut reports: Vec<Report> = Vec::new();
                    for slot in &session.rx {
                        let Some(rx) = &slot.open else { continue };
                        if reports.iter().any(|r| r.slot == slot.slot) {
                            continue;
                        }
                        let w = rx.writer.stats();
                        reports.push(Report {
                            slot: slot.slot,
                            playing: rx.shared.playing(),
                            buffer_us: rx.shared.buffer_us(),
                            jitter_us: w.jitter_us,
                            lost: w.lost,
                            underruns: rx.shared.underruns(),
                        });
                    }
                    let peer = session.peer;
                    let t_us = (now - self.epoch).as_micros() as u64;
                    send(&self.socket, Packet::Ping { t_us, reports }, peer);
                    // Repeated until the peer says it is so, or for long
                    // enough that it evidently will not.
                    session.asks.retain(|(_, since)| now - *since < ASK_TIMEOUT);
                    for (ask, _) in &session.asks {
                        send(&self.socket, Packet::Set { source: *ask }, peer);
                    }
                    self.advertise();
                } else if now - session.last_stats >= STATS_INTERVAL {
                    session.last_stats = now;
                    self.tend_streams(now);
                    let Phase::Connected(session) = &mut self.phase else { return };
                    let play = &self.config.play;
                    let stats = Stats {
                        rtt_ms: session.rtt_us.map(|us| us / 1000.0),
                        tx: (session.tx.iter().zip(&self.slots).enumerate())
                            .filter_map(|(source, (tx, slot))| Some((source, (*slot)?, tx.open.as_ref()?)))
                            .map(|(source, slot, tx)| TxStats {
                                source,
                                slot,
                                kind: tx.kind,
                                format: tx.format,
                                packet_ms: tx.packet_ms,
                                packets: tx.shared.packets(),
                                level: tx.shared.take_peak(),
                                denoise: tx.denoise,
                                remote: session.remote.iter().find(|r| r.slot == slot).copied(),
                            })
                            .collect(),
                        rx: (session.rx.iter())
                            .filter_map(|slot| Some((slot, slot.open.as_ref()?, play.get(slot.sink)?)))
                            .map(|(slot, rx, sink)| {
                                let w = rx.writer.stats();
                                RxStats {
                                    slot: slot.slot,
                                    kind: slot.kind,
                                    sink: sink.kind,
                                    format: rx.format,
                                    device_format: rx.device_format,
                                    playing: rx.shared.playing(),
                                    buffer_ms: rx.shared.buffer_us() as f32 / 1000.0,
                                    target_ms: w.target_us as f32 / 1000.0,
                                    jitter_ms: w.jitter_us as f32 / 1000.0,
                                    packets: w.packets,
                                    lost: w.lost,
                                    late: w.late,
                                    underruns: rx.shared.underruns(),
                                    skips: rx.shared.skips(),
                                    drift_ppm: rx.shared.drift_ppm(),
                                    level: rx.shared.take_peak(),
                                }
                            })
                            .collect(),
                        remote_sources: session.remote_sources(),
                        remote_streams: session.remote_streams(),
                    };
                    self.emit(Event::Stats(stats));
                }
            }
        }
    }

    /// The sources that are sent, each with its slot.
    fn sent(&self) -> impl Iterator<Item = (&SourceConfig, u8)> {
        self.config.send.iter().zip(&self.slots).filter_map(|(source, slot)| Some((source, (*slot)?)))
    }

    /// Tells the peer what this end sends: how each source is set, which
    /// it is and what it is called.
    fn advertise(&self) {
        let Phase::Connected(session) = &self.phase else { return };
        let sources = self.sent().map(|(source, slot)| source.settings(slot)).collect();
        send(&self.socket, Packet::Sources { sources }, session.peer);
        for (source, slot) in self.sent() {
            send(&self.socket, Packet::Stream { slot, id: &source.id, name: &source.name }, session.peer);
        }
    }

    /// The peer asks for one of our sources to be sent differently.
    fn peer_set(&mut self, wanted: SourceSettings, from: SocketAddr, now: Instant) {
        let wanted = sanitized(wanted);
        let Some(session) = self.session_from(from) else { return };
        session.last_heard = now;
        let Some(at) = self.slots.iter().position(|slot| *slot == Some(wanted.slot)) else { return };
        let source = &mut self.config.send[at];
        // Device audio is sent as it is, and a source of another kind is not
        // the one the peer was looking at.
        if source.kind == wanted.kind && source.kind != SourceKind::System && source.settings(wanted.slot) != wanted {
            source.denoise = wanted.denoise;
            source.auto_gain = wanted.auto_gain;
            source.gain_db = wanted.gain_db;
            // Closed now; `tend_streams` opens it the new way.
            if let (Phase::Connected(session), now) = (&mut self.phase, Instant::now())
                && let Some(tx) = session.tx.get_mut(at)
            {
                *tx = TxSlot::pending(now);
            }
            self.emit(Event::SourceChanged { source: at, settings: wanted });
        }
        // The answer, whether or not anything changed.
        self.advertise();
    }

    /// Broadcasts a discovery probe on every IPv4 interface.
    fn probe(&mut self) {
        let port = protocol::DEFAULT_PORT;
        let mut targets = vec![Ipv4Addr::BROADCAST];
        if let Ok(interfaces) = if_addrs::get_if_addrs() {
            for interface in interfaces {
                if let if_addrs::IfAddr::V4(v4) = interface.addr
                    && let Some(broadcast) = v4.broadcast.filter(|b| !targets.contains(b))
                {
                    targets.push(broadcast);
                }
            }
        }
        for target in targets {
            send(&self.socket, Packet::Discover { instance: self.instance }, (target, port).into());
        }
    }

    fn emit_peers(&self) {
        self.emit(Event::Peers(self.peers.iter().map(|(p, _)| p.clone()).collect()));
    }

    fn packet(&mut self, packet: Packet, from: SocketAddr, now: Instant) {
        match packet {
            Packet::Discover { instance } => {
                if instance != self.instance && self.config.listen {
                    let busy = !matches!(self.phase, Phase::Idle);
                    let name = self.config.name.clone();
                    send(&self.socket, Packet::Announce { instance: self.instance, busy, name: &name }, from);
                }
            }
            Packet::Announce { instance, busy, name } => {
                if instance == self.instance {
                    return;
                }
                let peer = Peer { addr: from, name: name.to_string(), busy };
                match self.peers.iter_mut().find(|(p, _)| p.addr == from) {
                    Some((known, seen)) => {
                        *seen = now;
                        if *known != peer {
                            *known = peer;
                            self.emit_peers();
                        }
                    }
                    None => {
                        self.peers.push((peer, now));
                        self.emit_peers();
                    }
                }
            }
            Packet::Hello { name, caps: theirs } => {
                let accept = match &self.phase {
                    // A repeat from the peer we already have: it missed our ack.
                    Phase::Connected(s) => s.peer == from,
                    // Both ends dialled each other at once.
                    Phase::Connecting { addr, .. } => *addr == from,
                    Phase::Idle => self.config.listen,
                };
                if !accept {
                    send(&self.socket, Packet::Bye { reason: BYE_BUSY }, from);
                    return;
                }
                let (own, caps) = (self.config.name.clone(), self.caps());
                send(&self.socket, Packet::HelloAck { name: &own, caps }, from);
                match &mut self.phase {
                    Phase::Connected(s) => s.last_heard = now,
                    Phase::Connecting { .. } => self.establish(from, name.to_string(), theirs, true, now),
                    Phase::Idle => self.establish(from, name.to_string(), theirs, false, now),
                }
            }
            Packet::HelloAck { name, caps } => {
                if matches!(&self.phase, Phase::Connecting { addr, .. } if *addr == from) {
                    self.establish(from, name.to_string(), caps, true, now);
                }
            }
            Packet::Bye { reason } => {
                let message = match &self.phase {
                    Phase::Connected(s) if s.peer == from => format!("{} disconnected", s.name),
                    Phase::Connecting { addr, .. } if *addr == from => match reason {
                        BYE_BUSY => format!("{addr} is busy or not accepting connections"),
                        _ => format!("{addr} refused the connection"),
                    },
                    _ => return,
                };
                self.phase = Phase::Idle;
                self.emit(Event::Info(message));
                self.emit(Event::State(State::Idle));
            }
            Packet::Ping { t_us, reports } => {
                if let Some(session) = self.session_from(from) {
                    session.last_heard = now;
                    session.remote = reports;
                    send(&self.socket, Packet::Pong { t_us }, from);
                }
            }
            Packet::Pong { t_us } => {
                let now_us = (now - self.epoch).as_micros() as u64;
                if let Some(session) = self.session_from(from) {
                    session.last_heard = now;
                    let sample = now_us.saturating_sub(t_us) as f32;
                    session.rtt_us = Some(match session.rtt_us {
                        Some(rtt) => rtt + (sample - rtt) * 0.2,
                        None => sample,
                    });
                }
            }
            Packet::Audio { header, payload } => self.audio(header, payload, from, now),
            Packet::Sources { sources } => {
                if let Some(session) = self.session_from(from) {
                    session.last_heard = now;
                    // Done, or about a source that is no longer there.
                    session.asks.retain(|(ask, _)| {
                        let source = sources.iter().find(|s| s.slot == ask.slot);
                        source.is_some_and(|source| source.kind == ask.kind && source != ask)
                    });
                    session.remote_streams.retain(|stream| sources.iter().any(|s| s.slot == stream.slot));
                    session.remote_sources = sources;
                }
            }
            Packet::Stream { slot, id, name } => {
                if let Some(session) = self.session_from(from) {
                    session.last_heard = now;
                    match session.remote_streams.iter_mut().find(|stream| stream.slot == slot) {
                        Some(stream) if stream.id == id => {
                            if stream.name != name {
                                stream.name = name.to_string();
                            }
                        }
                        known => {
                            let stream = RemoteStream { slot, id: id.to_string(), name: name.to_string() };
                            match known {
                                Some(known) => *known = stream,
                                None => session.remote_streams.push(stream),
                            }
                            // Another source than the slot was taken to
                            // carry: its stream is given its place afresh.
                            session.rx.retain(|rx| rx.slot != slot);
                        }
                    }
                }
            }
            Packet::Set { source } => self.peer_set(source, from, now),
        }
    }

    fn session_from(&mut self, from: SocketAddr) -> Option<&mut Session> {
        match &mut self.phase {
            Phase::Connected(session) if session.peer == from => Some(session),
            _ => None,
        }
    }

    fn establish(&mut self, peer: SocketAddr, name: String, caps: u8, initiator: bool, now: Instant) {
        let name = if name.is_empty() { peer.ip().to_string() } else { name };
        let sources = self.config.send.len();
        self.phase = Phase::Connected(Box::new(Session {
            peer,
            name: name.clone(),
            initiator,
            last_heard: now,
            last_ping: now,
            last_stats: now,
            tx: (0..sources).map(|_| TxSlot::pending(now)).collect(),
            rx: Vec::new(),
            rtt_us: None,
            remote: Vec::new(),
            identifies: caps & CAP_IDENTIFIES != 0,
            remote_sources: Vec::new(),
            remote_streams: Vec::new(),
            asks: Vec::new(),
        }));
        self.emit(Event::State(State::Connected { addr: peer, name }));
        self.advertise();
        self.tend_streams(now);
    }

    /// Opens the source at `index` in the configured list and starts
    /// sending it in `slot`.
    fn open_capture(&self, peer: SocketAddr, index: usize, slot: u8) -> Result<Tx, String> {
        let source = self.config.send[index].clone();
        let channels = source.channels.unwrap_or(match source.kind {
            SourceKind::System => 2,
            SourceKind::Mic | SourceKind::Tone => 1,
        });
        let spec = StreamSpec {
            slot,
            kind: source.kind,
            stream_id: random_u32(),
            channels,
            packet_us: (self.config.packet_ms.clamp(0.5, 20.0) * 1000.0) as u32,
            denoise: source.denoise,
            auto_gain: source.auto_gain,
            gain_db: source.gain_db,
        };
        let socket = self.socket.clone();
        // The packetizer can only be built once the backend knows the device
        // format; this carries its stats handle back out of the callback.
        let (info_tx, info_rx) = mpsc::channel();
        let make: MakeCapture = Box::new(move |device| {
            let (mut packetizer, shared, wire) = Packetizer::new(socket, peer, spec, device);
            let packet_ms = packetizer.packet_frames() as f32 * 1000.0 / wire.sample_rate as f32;
            let _ = info_tx.send((shared, wire, packet_ms, packetizer.filtered()));
            Box::new(move |samples| packetizer.push(samples))
        });
        let request = CaptureRequest {
            kind: source.kind,
            device: source.device,
            // The noise filter cannot work at any other rate.
            sample_rate: if source.denoise.is_on() { denoise::RATE } else { self.config.sample_rate },
            channels,
            period_frames: self.config.period_frames,
            denoise: source.denoise.is_on(),
        };
        let opened = match source.kind {
            SourceKind::Tone => {
                let format = StreamFormat { sample_rate: request.sample_rate, channels };
                Ok(synthetic::tone(format, request.period_frames, make))
            }
            _ => self.backend.open_capture(&request, make),
        };
        let what = match source.kind {
            SourceKind::Mic => "microphone",
            SourceKind::System => "system audio",
            SourceKind::Tone => "test tone",
        };
        let stream = opened.map_err(|e| format!("Could not capture {what}: {e}"))?;
        let (shared, format, packet_ms, denoise) =
            info_rx.try_recv().map_err(|_| format!("Could not capture {what}: the stream never started"))?;
        if source.denoise.is_on() && !denoise.is_on() {
            self.emit(Event::Info(format!(
                "Noise suppression needs a {} kHz {what}; this one runs at {}, so it is sent as it is",
                denoise::RATE / 1000,
                format
            )));
        }
        Ok(Tx { stream, kind: source.kind, shared, format, packet_ms, denoise })
    }

    /// Opens sources that are due, replaces streams whose device went away,
    /// and closes outputs whose stream has stopped arriving.
    fn tend_streams(&mut self, now: Instant) {
        let Phase::Connected(session) = &mut self.phase else { return };
        session.rx.retain(|rx| now.saturating_duration_since(rx.last_packet) < STREAM_IDLE);
        for rx in &mut session.rx {
            if rx.open.as_ref().is_some_and(|rx| !rx.stream.is_alive()) {
                // The next audio packet reopens the output.
                rx.open = None;
            }
        }
        let peer = session.peer;
        for index in 0..session.tx.len() {
            // A source without a slot is not sent.
            let Some(slot) = self.slots.get(index).copied().flatten() else { continue };
            let Phase::Connected(session) = &mut self.phase else { return };
            let tx = &mut session.tx[index];
            if tx.open.as_ref().is_some_and(|tx| !tx.stream.is_alive()) {
                tx.open = None;
                tx.retry_at = Some(now);
            }
            if tx.open.is_some() || tx.retry_at.is_none_or(|at| now < at) {
                continue;
            }
            let opened = self.open_capture(peer, index, slot);
            let Phase::Connected(session) = &mut self.phase else { return };
            let tx = &mut session.tx[index];
            match opened {
                Ok(open) => *tx = TxSlot { open: Some(open), ..TxSlot::default() },
                Err(e) => {
                    tx.retry_at = Some(now + RETRY_DEVICE_AFTER);
                    // Shown unless it is the one already on screen.
                    if tx.last_error.as_ref() != Some(&e) {
                        tx.last_error = Some(e.clone());
                        (self.on_event)(Event::Error(e));
                    }
                }
            }
        }
    }

    fn audio(&mut self, header: AudioHeader, payload: &[u8], from: SocketAddr, now: Instant) {
        let now_us = (now - self.epoch).as_micros() as i64;
        let format = StreamFormat { sample_rate: header.sample_rate, channels: header.channels as u16 };
        let Phase::Connected(session) = &mut self.phase else { return };
        let session = &mut **session;
        if session.peer != from {
            return;
        }
        session.last_heard = now;
        // The sender put a different kind of source in this slot: route it afresh.
        session.rx.retain(|rx| rx.slot != header.slot || rx.kind == header.kind);
        // Until the peer has said which source this is, it has no place.
        let Some(id) = source_of(&session.remote_streams, session.identifies, header.slot, header.kind) else {
            return;
        };
        let named = self.config.play.iter().any(|sink| matches!(&sink.from, Streams::Source(wanted) if wanted == id));
        for (sink, config) in self.config.play.iter().enumerate() {
            if !config.takes(id, named) {
                continue;
            }
            let found = session.rx.iter().position(|rx| rx.slot == header.slot && rx.sink == sink);
            let at = found.unwrap_or_else(|| {
                session.rx.push(RxSlot {
                    slot: header.slot,
                    sink,
                    kind: header.kind,
                    open: None,
                    retry_at: None,
                    last_error: None,
                    last_packet: now,
                });
                session.rx.len() - 1
            });
            let rx = &mut session.rx[at];
            rx.last_packet = now;
            match &mut rx.open {
                Some(open) if open.format == format => {
                    if open.stream_id != header.stream_id {
                        // The sender restarted capture: same device, new timeline.
                        open.stream_id = header.stream_id;
                        open.writer.restart();
                    }
                }
                _ => {
                    if rx.retry_at.is_some_and(|at| now < at) {
                        continue;
                    }
                    // Close the old output before opening its replacement.
                    rx.open = None;
                    let opened = open_playback(&*self.backend, &self.config, config, format, header.stream_id);
                    // Opening a device can take long enough to look like silence
                    // from the peer, whose packets were queueing all the while.
                    session.last_heard = Instant::now();
                    match opened {
                        Ok(open) => {
                            rx.open = Some(open);
                            rx.retry_at = None;
                            rx.last_error = None;
                        }
                        Err(e) => {
                            rx.retry_at = Some(now + RETRY_DEVICE_AFTER);
                            let e = format!("Could not open audio output: {e}");
                            if rx.last_error.as_ref() != Some(&e) {
                                rx.last_error = Some(e.clone());
                                (self.on_event)(Event::Error(e));
                            }
                            continue;
                        }
                    }
                }
            }
            if let Some(open) = &mut rx.open {
                open.writer.push(header.timestamp, payload, now_us);
            }
        }
    }
}
fn open_playback(
    backend: &dyn AudioBackend,
    config: &Config,
    sink: &SinkConfig,
    format: StreamFormat,
    stream_id: u32,
) -> Result<Rx, String> {
    let mode = config.buffer;
    let (info_tx, info_rx) = mpsc::channel();
    let make: MakeRender = Box::new(move |device| {
        let (writer, mut reader, shared) = playout::playout(format, device, mode);
        let _ = info_tx.send((writer, shared, device));
        let channels = device.channels as usize;
        Box::new(move |out| reader.render(out, channels))
    });
    let request = PlaybackRequest {
        kind: sink.kind,
        device: sink.device.clone(),
        sample_rate: format.sample_rate,
        channels: format.channels,
        period_frames: config.period_frames,
    };
    let stream = match sink.kind {
        SinkKind::Null => synthetic::null_sink(format, request.period_frames, make),
        _ => backend.open_playback(&request, make)?,
    };
    let (writer, shared, device_format) =
        info_rx.try_recv().map_err(|_| "the stream never started".to_string())?;
    Ok(Rx { stream, stream_id, format, device_format, writer, shared })
}
