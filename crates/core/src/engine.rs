//! The session engine: one thread that owns the UDP socket, finds peers,
//! sets up and tears down a link, and feeds received audio to the jitter
//! buffer. Front ends talk to it through [`Engine`] and hear back through
//! [`Event`]s.
//!
//! A link is symmetric. Each end decides for itself what it captures and
//! sends ([`Config::send`]) and whether it plays what arrives
//! ([`Config::play`]); nothing a peer says can make this end start
//! recording. "Phone mic to PC" is the phone sending its mic and the PC
//! playing; "PC audio to phone" is the reverse; both at once is both.

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
use crate::protocol::{self, AudioHeader, BYE_BUSY, BYE_NORMAL, CAP_PLAYS, CAP_SENDS, Packet, Report};
use crate::sender::{Packetizer, TxShared};
use crate::synthetic;

const HELLO_INTERVAL: Duration = Duration::from_millis(250);
const PING_INTERVAL: Duration = Duration::from_millis(500);
const STATS_INTERVAL: Duration = Duration::from_millis(250);
const DISCOVER_INTERVAL: Duration = Duration::from_secs(2);
/// A peer that has not answered a probe for this long is dropped from the list.
const PEER_TTL: Duration = Duration::from_secs(7);
/// Pings arrive twice a second, so this much silence means the peer is gone.
const SESSION_TIMEOUT: Duration = Duration::from_secs(3);
const RETRY_DEVICE_AFTER: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq)]
pub struct SourceConfig {
    pub kind: SourceKind,
    /// Backend device id, or `None` for the default.
    pub device: Option<String>,
    /// Channels to send. `None` picks mono for a mic, stereo for system audio.
    pub channels: Option<u16>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SinkConfig {
    pub kind: SinkKind,
    pub device: Option<String>,
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
    pub send: Option<SourceConfig>,
    pub play: Option<SinkConfig>,
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
            send: None,
            play: Some(SinkConfig { kind: SinkKind::Speaker, device: None }),
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
    pub format: StreamFormat,
    pub packet_ms: f32,
    pub packets: u64,
    /// Peak input level since the previous report, 0..=1.
    pub level: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RxStats {
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

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stats {
    pub rtt_ms: Option<f32>,
    pub tx: Option<TxStats>,
    pub rx: Option<RxStats>,
    /// The peer's view of the stream we are sending it.
    pub remote: Option<Report>,
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
    shared: Arc<TxShared>,
    format: StreamFormat,
    packet_ms: f32,
}

struct Rx {
    stream: Box<dyn StreamHandle>,
    stream_id: u32,
    format: StreamFormat,
    device_format: StreamFormat,
    writer: PlayoutWriter,
    shared: Arc<playout::Shared>,
}

struct Session {
    peer: SocketAddr,
    name: String,
    /// We dialled; if the link drops, we are the one to redial.
    initiator: bool,
    last_heard: Instant,
    last_ping: Instant,
    last_stats: Instant,
    tx: Option<Tx>,
    rx: Option<Rx>,
    /// Don't hammer a device that just failed to open.
    rx_retry_at: Option<Instant>,
    /// When to try the capture device again after it failed or died.
    tx_retry_at: Option<Instant>,
    /// The last device error shown, so a retry loop reports it only once.
    last_error: Option<String>,
    rtt_us: Option<f32>,
    remote: Option<Report>,
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
        (if self.config.send.is_some() { CAP_SENDS } else { 0 })
            | (if self.config.play.is_some() { CAP_PLAYS } else { 0 })
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
                let tx_changed = old.send != self.config.send
                    || old.packet_ms != self.config.packet_ms
                    || old.period_frames != self.config.period_frames
                    || old.sample_rate != self.config.sample_rate;
                let rx_changed = old.play != self.config.play
                    || old.buffer != self.config.buffer
                    || old.period_frames != self.config.period_frames;
                if let Phase::Connected(session) = &mut self.phase {
                    if rx_changed {
                        // Reopened by the next audio packet.
                        session.rx = None;
                        session.rx_retry_at = None;
                        session.last_error = None;
                    }
                    if tx_changed {
                        // Closed now; `tend_streams` opens the new source.
                        session.tx = None;
                        session.tx_retry_at = Some(Instant::now());
                        session.last_error = None;
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
                    let report = session.rx.as_ref().map(|rx| {
                        let w = rx.writer.stats();
                        Report {
                            playing: rx.shared.playing(),
                            buffer_us: rx.shared.buffer_us(),
                            jitter_us: w.jitter_us,
                            lost: w.lost,
                            underruns: rx.shared.underruns(),
                        }
                    });
                    let peer = session.peer;
                    let t_us = (now - self.epoch).as_micros() as u64;
                    send(&self.socket, Packet::Ping { t_us, report }, peer);
                } else if now - session.last_stats >= STATS_INTERVAL {
                    session.last_stats = now;
                    self.tend_streams(now);
                    let Phase::Connected(session) = &mut self.phase else { return };
                    let stats = Stats {
                        rtt_ms: session.rtt_us.map(|us| us / 1000.0),
                        tx: session.tx.as_ref().map(|tx| TxStats {
                            format: tx.format,
                            packet_ms: tx.packet_ms,
                            packets: tx.shared.packets(),
                            level: tx.shared.take_peak(),
                        }),
                        rx: session.rx.as_ref().map(|rx| {
                            let w = rx.writer.stats();
                            RxStats {
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
                        }),
                        remote: session.remote,
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
            Packet::Ping { t_us, report } => {
                if let Some(session) = self.session_from(from) {
                    session.last_heard = now;
                    session.remote = report;
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
        let (tx, last_error) = match self.open_capture(peer) {
            Ok(tx) => (tx, None),
            Err(e) => {
                self.emit(Event::Error(e.clone()));
                (None, Some(e))
            }
        };
        let retry_at = last_error.as_ref().map(|_| now + RETRY_DEVICE_AFTER);
        self.phase = Phase::Connected(Box::new(Session {
            peer,
            name: name.clone(),
            initiator,
            last_heard: now,
            last_ping: now,
            last_stats: now,
            tx,
            rx: None,
            rx_retry_at: None,
            tx_retry_at: retry_at,
            last_error,
            rtt_us: None,
            remote: None,
        }));
        self.emit(Event::State(State::Connected { addr: peer, name }));
    }

    /// Opens the configured source, if there is one, and starts sending.
    fn open_capture(&self, peer: SocketAddr) -> Result<Option<Tx>, String> {
        let Some(source) = self.config.send.clone() else { return Ok(None) };
        let channels = source.channels.unwrap_or(match source.kind {
            SourceKind::System => 2,
            SourceKind::Mic | SourceKind::Tone => 1,
        });
        let packet_us = (self.config.packet_ms.clamp(0.5, 20.0) * 1000.0) as u32;
        let (socket, stream_id) = (self.socket.clone(), random_u32());
        // The packetizer can only be built once the backend knows the device
        // format; this carries its stats handle back out of the callback.
        let (info_tx, info_rx) = mpsc::channel();
        let make: MakeCapture = Box::new(move |device| {
            let (mut packetizer, shared, wire) =
                Packetizer::new(socket, peer, stream_id, device, channels, packet_us);
            let packet_ms = packetizer.packet_frames() as f32 * 1000.0 / wire.sample_rate as f32;
            let _ = info_tx.send((shared, wire, packet_ms));
            Box::new(move |samples| packetizer.push(samples))
        });
        let request = CaptureRequest {
            kind: source.kind,
            device: source.device,
            sample_rate: self.config.sample_rate,
            channels,
            period_frames: self.config.period_frames,
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
        let (shared, format, packet_ms) =
            info_rx.try_recv().map_err(|_| format!("Could not capture {what}: the stream never started"))?;
        Ok(Some(Tx { stream, shared, format, packet_ms }))
    }

    /// Shows a device error unless it is the one already on screen.
    fn report(&mut self, error: String) {
        if let Phase::Connected(session) = &mut self.phase {
            if session.last_error.as_ref() == Some(&error) {
                return;
            }
            session.last_error = Some(error.clone());
        }
        self.emit(Event::Error(error));
    }

    /// Replaces streams whose device went away, and retries a source that
    /// could not be opened.
    fn tend_streams(&mut self, now: Instant) {
        let Phase::Connected(session) = &mut self.phase else { return };
        if session.rx.as_ref().is_some_and(|rx| !rx.stream.is_alive()) {
            // The next audio packet reopens the output.
            session.rx = None;
        }
        if session.tx.as_ref().is_some_and(|tx| !tx.stream.is_alive()) {
            session.tx = None;
            session.tx_retry_at = Some(now);
        }
        if session.tx.is_some() || session.tx_retry_at.is_none_or(|at| now < at) {
            return;
        }
        let peer = session.peer;
        let opened = self.open_capture(peer);
        let Phase::Connected(session) = &mut self.phase else { return };
        match opened {
            Ok(tx) => {
                session.tx = tx;
                session.tx_retry_at = None;
            }
            Err(e) => {
                session.tx_retry_at = Some(now + RETRY_DEVICE_AFTER);
                self.report(e);
            }
        }
    }

    fn audio(&mut self, header: AudioHeader, payload: &[u8], from: SocketAddr, now: Instant) {
        let Some(sink) = self.config.play.clone() else { return };
        let now_us = (now - self.epoch).as_micros() as i64;
        let format = StreamFormat { sample_rate: header.sample_rate, channels: header.channels as u16 };
        let Phase::Connected(session) = &mut self.phase else { return };
        if session.peer != from {
            return;
        }
        session.last_heard = now;
        match &mut session.rx {
            Some(rx) if rx.format == format => {
                if rx.stream_id != header.stream_id {
                    // The sender restarted capture: same device, new timeline.
                    rx.stream_id = header.stream_id;
                    rx.writer.restart();
                }
            }
            _ => {
                if session.rx_retry_at.is_some_and(|at| now < at) {
                    return;
                }
                // Close the old output before opening its replacement.
                session.rx = None;
                let opened = open_playback(&*self.backend, &self.config, &sink, format, header.stream_id);
                let Phase::Connected(session) = &mut self.phase else { return };
                // Opening a device can take long enough to look like silence
                // from the peer, whose packets were queueing all the while.
                session.last_heard = Instant::now();
                match opened {
                    Ok(rx) => session.rx = Some(rx),
                    Err(e) => {
                        session.rx_retry_at = Some(now + RETRY_DEVICE_AFTER);
                        self.report(format!("Could not open audio output: {e}"));
                        return;
                    }
                }
            }
        }
        let Phase::Connected(session) = &mut self.phase else { return };
        if let Some(rx) = &mut session.rx {
            rx.writer.push(header.timestamp, payload, now_us);
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
