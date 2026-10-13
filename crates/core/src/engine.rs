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
//! Either list can be any length. Every source is its own stream, numbered
//! by its place in the list (its slot). An arriving stream is played on the
//! sinks that name its slot, or failing those on each sink that takes its
//! kind.

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
    self, AudioHeader, BYE_BUSY, BYE_NORMAL, CAP_PLAYS, CAP_SENDS, MAX_STREAMS, Packet, Report,
};
use crate::sender::{Packetizer, StreamSpec, TxShared};
use crate::{denoise, synthetic};

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

#[derive(Clone, Debug, PartialEq)]
pub struct SourceConfig {
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
    pub fn new(kind: SourceKind) -> Self {
        SourceConfig {
            kind,
            device: None,
            channels: None,
            denoise: denoise::Mode::Off,
            auto_gain: false,
            gain_db: 0.0,
        }
    }
}

/// Which of the peer's streams a sink plays.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Streams {
    #[default]
    All,
    /// Those of one kind.
    Kind(SourceKind),
    /// The one the peer sends in this slot, whatever it carries. A stream
    /// that any sink names this way is played on such sinks only.
    Slot(u8),
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

    /// Whether the stream in `slot` is played here; `named` says that some
    /// sink asks for that slot.
    fn takes(&self, slot: u8, kind: SourceKind, named: bool) -> bool {
        self.kind != SinkKind::Nothing
            && match self.from {
                Streams::Slot(wanted) => wanted == slot,
                Streams::Kind(wanted) => !named && wanted == kind,
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
    /// What to capture and send, one stream each. At most
    /// [`MAX_STREAMS`](protocol::MAX_STREAMS) are used.
    pub send: Vec<SourceConfig>,
    /// Where received streams go. A stream is played on every sink that
    /// names its slot, or with none that does on every sink that takes its
    /// kind, and dropped if there is none of either.
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
    /// The peer's slot for this stream.
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

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
    pub rtt_ms: Option<f32>,
    /// The streams being sent. A configured source that is missing here
    /// could not be opened.
    pub tx: Vec<TxStats>,
    /// The streams being played, once per sink each goes to.
    pub rx: Vec<RxStats>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    State(State),
    Stats(Stats),
    /// The current set of discovered peers.
    Peers(Vec<Peer>),
    /// Something worth telling the user that is not a failure.
    Info(String),
    /// Something failed; the engine keeps running.
    Error(String),
}

enum Command {
    Connect(SocketAddr),
    Disconnect,
    Configure(Box<Config>),
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
        let worker = Worker {
            socket: Arc::new(socket),
            backend,
            on_event: Box::new(on_event),
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

/// One configured source; its index in `Session::tx` is its slot.
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
        (if self.config.send.is_empty() { 0 } else { CAP_SENDS }) | (if plays { CAP_PLAYS } else { 0 })
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
                    // Sources that are the same as before keep running.
                    let now = Instant::now();
                    session.tx.resize_with(self.config.send.len().min(MAX_STREAMS), TxSlot::default);
                    for (slot, tx) in session.tx.iter_mut().enumerate() {
                        if all_tx_changed || old.send.get(slot) != self.config.send.get(slot) {
                            // Closed now; `tend_streams` opens the new source.
                            *tx = TxSlot::pending(now);
                        }
                    }
                }
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
                } else if now - session.last_stats >= STATS_INTERVAL {
                    session.last_stats = now;
                    self.tend_streams(now);
                    let Phase::Connected(session) = &mut self.phase else { return };
                    let play = &self.config.play;
                    let stats = Stats {
                        rtt_ms: session.rtt_us.map(|us| us / 1000.0),
                        tx: (session.tx.iter().enumerate())
                            .filter_map(|(slot, tx)| Some((slot as u8, tx.open.as_ref()?)))
                            .map(|(slot, tx)| TxStats {
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
                    };
                    self.emit(Event::Stats(stats));
                }
            }
        }
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
            Packet::Hello { name, .. } => {
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
                    Phase::Connecting { .. } => self.establish(from, name.to_string(), true, now),
                    Phase::Idle => self.establish(from, name.to_string(), false, now),
                }
            }
            Packet::HelloAck { name, .. } => {
                if matches!(&self.phase, Phase::Connecting { addr, .. } if *addr == from) {
                    self.establish(from, name.to_string(), true, now);
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
        }
    }

    fn session_from(&mut self, from: SocketAddr) -> Option<&mut Session> {
        match &mut self.phase {
            Phase::Connected(session) if session.peer == from => Some(session),
            _ => None,
        }
    }

    fn establish(&mut self, peer: SocketAddr, name: String, initiator: bool, now: Instant) {
        let name = if name.is_empty() { peer.ip().to_string() } else { name };
        let sources = self.config.send.len().min(MAX_STREAMS);
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
        }));
        self.emit(Event::State(State::Connected { addr: peer, name }));
        self.tend_streams(now);
    }

    /// Opens the source configured for `slot` and starts sending.
    fn open_capture(&self, peer: SocketAddr, slot: usize) -> Result<Tx, String> {
        let source = self.config.send[slot].clone();
        let channels = source.channels.unwrap_or(match source.kind {
            SourceKind::System => 2,
            SourceKind::Mic | SourceKind::Tone => 1,
        });
        let spec = StreamSpec {
            slot: slot as u8,
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
        for slot in 0..session.tx.len() {
            let Phase::Connected(session) = &mut self.phase else { return };
            let tx = &mut session.tx[slot];
            if tx.open.as_ref().is_some_and(|tx| !tx.stream.is_alive()) {
                tx.open = None;
                tx.retry_at = Some(now);
            }
            if tx.open.is_some() || tx.retry_at.is_none_or(|at| now < at) {
                continue;
            }
            let opened = self.open_capture(peer, slot);
            let Phase::Connected(session) = &mut self.phase else { return };
            let tx = &mut session.tx[slot];
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
        let named = self.config.play.iter().any(|sink| sink.from == Streams::Slot(header.slot));
        for (sink, config) in self.config.play.iter().enumerate() {
            if !config.takes(header.slot, header.kind, named) {
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
