//! `soundbridge`: the command-line front end.

use std::io::{IsTerminal, Write};
use std::net::{SocketAddr, ToSocketAddrs};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand, ValueEnum};
use soundbridge_core::cpal_backend::CpalBackend;
use soundbridge_core::{
    AudioBackend, BufferMode, Config, DEFAULT_PORT, DeviceDirection, Engine, Event, SinkConfig,
    SinkKind, SourceConfig, SourceKind, State, Stats,
};

/// A low-latency audio bridge between machines on the same network.
///
/// One end runs `listen`, the other `connect`s to it; each end chooses for
/// itself what it sends (--send) and where received audio goes (--play).
#[derive(Parser)]
#[command(name = "soundbridge", version, after_help = EXAMPLES)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

const EXAMPLES: &str = "\
Examples:
  Use a phone as this PC's microphone (the phone connects and sends its mic):
    soundbridge listen --play virtual-mic

  Hear this PC on the phone (the phone is listening):
    soundbridge connect 192.168.1.20 --send system --play none

  Talk both ways between two PCs:
    soundbridge listen --send mic
    soundbridge connect otherpc.local --send mic";

#[derive(Subcommand)]
enum Command {
    /// Wait for a peer to connect
    Listen {
        /// UDP port to listen on
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
        /// Exit when the first session ends instead of waiting for another
        #[arg(long)]
        once: bool,
        #[command(flatten)]
        link: LinkArgs,
    },
    /// Connect to a listening peer
    Connect {
        /// Peer address: HOST or HOST:PORT
        peer: String,
        #[command(flatten)]
        link: LinkArgs,
    },
    /// Find listening peers on the local network
    Discover {
        /// Seconds to wait for answers
        #[arg(short, long, default_value_t = 2.0)]
        timeout: f32,
    },
    /// List audio devices and their ids
    Devices,
}

#[derive(Clone, Copy, ValueEnum)]
enum Send {
    /// A microphone or other input device
    Mic,
    /// Whatever this machine is playing
    System,
    /// A generated 440 Hz test tone
    Tone,
}

#[derive(Clone, Copy, PartialEq, ValueEnum)]
enum Play {
    /// An output device
    Speaker,
    /// A virtual microphone other applications can record from (PipeWire)
    VirtualMic,
    /// Receive and measure, but play nothing
    Null,
    /// Ignore incoming audio
    None,
}

#[derive(Args)]
struct LinkArgs {
    /// Capture and send audio from this machine
    #[arg(short, long, value_enum)]
    send: Option<Send>,
    /// Device to capture from, by id (see `soundbridge devices`). For
    /// `--send system` this is the output device whose playback is captured
    #[arg(long, value_name = "ID")]
    input_device: Option<String>,
    /// Channels to send [default: 1 for mic, 2 for system]
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..=2))]
    channels: Option<u16>,

    /// What to do with audio received from the peer
    #[arg(short, long, value_enum, default_value = "speaker")]
    play: Play,
    /// Device to play on, by id
    #[arg(long, value_name = "ID")]
    output_device: Option<String>,

    /// Jitter buffer: `auto` adapts to the network, or a fixed size in ms
    #[arg(short, long, default_value = "auto", value_parser = parse_buffer)]
    buffer: Buffer,
    /// Smallest jitter margin `auto` will use, ms
    #[arg(long, default_value_t = 2.0, value_name = "MS")]
    min_buffer: f32,
    /// Largest jitter margin `auto` will use, ms
    #[arg(long, default_value_t = 80.0, value_name = "MS")]
    max_buffer: f32,
    /// Audio per packet, ms. Smaller is lower latency and more packets
    #[arg(long, default_value_t = 2.5, value_name = "MS")]
    packet: f32,
    /// Audio device period to request, frames
    #[arg(long, default_value_t = 128, value_name = "FRAMES")]
    period: u32,
    /// Sample rate to request from capture devices
    #[arg(long, default_value_t = 48_000, value_name = "HZ")]
    rate: u32,

    /// Name shown to the peer [default: this machine's hostname]
    #[arg(short, long)]
    name: Option<String>,
    /// Print events only, no live statistics
    #[arg(short, long)]
    quiet: bool,
}

#[derive(Clone, Copy)]
enum Buffer {
    Auto,
    Fixed(f32),
}

fn parse_buffer(s: &str) -> Result<Buffer, String> {
    if s.eq_ignore_ascii_case("auto") {
        return Ok(Buffer::Auto);
    }
    match s.trim_end_matches("ms").parse::<f32>() {
        Ok(ms) if (0.0..=1000.0).contains(&ms) => Ok(Buffer::Fixed(ms)),
        _ => Err("expected `auto` or a size in milliseconds, e.g. `8`".into()),
    }
}

impl LinkArgs {
    fn config(&self, port: u16, listen: bool) -> Config {
        Config {
            name: self.name.clone().unwrap_or_else(soundbridge_core::default_name),
            port,
            port_fallback: false,
            listen,
            discover: false,
            send: self.send.map(|kind| SourceConfig {
                kind: match kind {
                    Send::Mic => SourceKind::Mic,
                    Send::System => SourceKind::System,
                    Send::Tone => SourceKind::Tone,
                },
                device: self.input_device.clone(),
                channels: self.channels,
            }),
            play: match self.play {
                Play::Speaker => Some(SinkKind::Speaker),
                Play::VirtualMic => Some(SinkKind::VirtualMic),
                Play::Null => Some(SinkKind::Null),
                Play::None => None,
            }
            .map(|kind| SinkConfig { kind, device: self.output_device.clone() }),
            packet_ms: self.packet,
            buffer: match self.buffer {
                Buffer::Auto => BufferMode::Auto { min_ms: self.min_buffer, max_ms: self.max_buffer },
                Buffer::Fixed(ms) => BufferMode::Fixed { ms },
            },
            period_frames: self.period,
            sample_rate: self.rate,
        }
    }
}

fn resolve(peer: &str) -> Result<SocketAddr, String> {
    let with_port = if peer.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) {
        peer.to_string()
    } else {
        format!("{peer}:{DEFAULT_PORT}")
    };
    with_port
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve '{peer}': {e}"))?
        .find(SocketAddr::is_ipv4)
        .ok_or_else(|| format!("'{peer}' has no IPv4 address"))
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Listen { port, once, link } => run_link(link.config(port, true), None, once, link.quiet),
        Command::Connect { peer, link } => match resolve(&peer) {
            // Any free local port: the dialer does not need to be findable.
            Ok(addr) => run_link(link.config(0, false), Some(addr), true, link.quiet),
            Err(e) => Err(e),
        },
        Command::Discover { timeout } => discover(timeout),
        Command::Devices => {
            devices();
            Ok(())
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("soundbridge: {e}");
            ExitCode::FAILURE
        }
    }
}

fn devices() {
    let backend = CpalBackend::new();
    let devices = backend.devices();
    println!("Audio system: {}", backend.host_name());
    for (direction, title) in [
        (DeviceDirection::Input, "Inputs (--send mic --input-device ID)"),
        (
            DeviceDirection::Output,
            "Outputs (--output-device ID, or --send system --input-device ID)",
        ),
    ] {
        println!("\n{title}:");
        let mut any = false;
        for d in devices.iter().filter(|d| d.direction == direction) {
            println!("  {:<60} {}", d.id, d.name);
            any = true;
        }
        if !any {
            println!("  (none)");
        }
    }
}

fn discover(timeout: f32) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    let config = Config {
        port: 0,
        listen: false,
        discover: true,
        send: None,
        play: None,
        ..Config::default()
    };
    let _engine = Engine::start(config, Arc::new(CpalBackend::new()), move |e| {
        let _ = tx.send(e);
    })
    .map_err(|e| format!("cannot open a socket: {e}"))?;

    let deadline = Instant::now() + Duration::from_secs_f32(timeout.clamp(0.2, 60.0));
    let mut peers = Vec::new();
    while let Ok(event) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        if let Event::Peers(found) = event {
            peers = found;
        }
    }
    if peers.is_empty() {
        return Err("no peers found (is one listening, on this network?)".into());
    }
    for peer in peers {
        let busy = if peer.busy { "  (busy)" } else { "" };
        println!("{:<22} {}{busy}", peer.addr, peer.name);
    }
    Ok(())
}

/// Runs a session until interrupted, or (with `until_idle`) until the first
/// session ends.
fn run_link(config: Config, dial: Option<SocketAddr>, until_idle: bool, quiet: bool) -> Result<(), String> {
    let port = config.port;
    let (tx, rx) = mpsc::channel();
    let events = tx.clone();
    let engine = Engine::start(config, Arc::new(CpalBackend::new()), move |e| {
        let _ = events.send(Some(e));
    })
    .map_err(|e| match e.kind() {
        std::io::ErrorKind::AddrInUse => {
            format!("port {port} is in use (is another SoundBridge running? try --port)")
        }
        _ => format!("cannot open a socket: {e}"),
    })?;
    // Leave through the normal path on Ctrl-C so the peer is told and audio
    // devices are closed, rather than waiting out its timeout.
    let _ = ctrlc::set_handler(move || {
        let _ = tx.send(None);
    });

    let mut status = Status::new(quiet, std::io::stderr().is_terminal());
    match dial {
        Some(addr) => engine.connect(addr),
        None => status.line(&format!("Listening on port {}", engine.local_port())),
    }

    let mut was_connected = false;
    let mut failed = false;
    while let Ok(Some(event)) = rx.recv() {
        match event {
            Event::State(State::Connecting { addr }) => status.line(&format!("Connecting to {addr}...")),
            Event::State(State::Connected { addr, name }) => {
                was_connected = true;
                status.line(&format!("Connected to {name} ({addr})"));
            }
            Event::State(State::Idle) => {
                if until_idle {
                    // A dialer that never got in has failed; one whose peer
                    // hung up has simply finished.
                    failed = dial.is_some() && !was_connected;
                    break;
                }
                status.line("Waiting for a peer");
            }
            Event::Stats(stats) => status.stats(&stats),
            Event::Info(message) => status.line(&message),
            Event::Error(message) => status.line(&format!("error: {message}")),
            Event::Peers(_) => {}
        }
    }
    status.clear();
    drop(engine);
    if failed { Err("could not connect".into()) } else { Ok(()) }
}

/// Event lines that scroll, above one statistics line that is redrawn in
/// place. When stderr is not a terminal (a log file, a service journal) the
/// statistics become an occasional ordinary line instead.
struct Status {
    quiet: bool,
    tty: bool,
    drawn: bool,
    last_logged: Option<Instant>,
}

const LOG_STATS_EVERY: Duration = Duration::from_secs(5);

impl Status {
    fn new(quiet: bool, tty: bool) -> Self {
        Status { quiet, tty, drawn: false, last_logged: None }
    }

    fn clear(&mut self) {
        if self.drawn {
            eprint!("\r\x1b[2K");
            self.drawn = false;
        }
    }

    fn line(&mut self, text: &str) {
        self.clear();
        eprintln!("{text}");
    }

    fn stats(&mut self, stats: &Stats) {
        if self.quiet {
            return;
        }
        if !self.tty {
            if self.last_logged.is_some_and(|t| t.elapsed() < LOG_STATS_EVERY) {
                return;
            }
            self.last_logged = Some(Instant::now());
        }
        let mut parts = Vec::new();
        if let Some(tx) = &stats.tx {
            let mut part = format!("send {} {}", tx.format, meter(tx.level));
            if let Some(remote) = &stats.remote {
                part += &format!(" > peer buffer {:.1} ms", remote.buffer_us as f32 / 1000.0);
                if remote.lost > 0 || remote.underruns > 0 {
                    part += &format!(" (lost {}, underruns {})", remote.lost, remote.underruns);
                }
            }
            parts.push(part);
        }
        if let Some(rx) = &stats.rx {
            let state = if rx.playing { "" } else { " (buffering)" };
            parts.push(format!(
                "recv {} {} buffer {:.1} ms{state} jitter {:.1} lost {} late {} underruns {} {:+} ppm",
                rx.format,
                meter(rx.level),
                rx.buffer_ms,
                rx.jitter_ms,
                rx.lost,
                rx.late,
                rx.underruns,
                rx.drift_ppm,
            ));
        }
        if let Some(rtt) = stats.rtt_ms {
            parts.push(format!("rtt {rtt:.1} ms"));
        }
        if parts.is_empty() {
            parts.push("connected, no audio in either direction".into());
        }
        if !self.tty {
            eprintln!("{}", parts.join(" | "));
            return;
        }
        eprint!("\r\x1b[2K{}", parts.join(" | "));
        let _ = std::io::stderr().flush();
        self.drawn = true;
    }
}

/// A five-step level meter covering roughly the top 40 dB.
fn meter(level: f32) -> String {
    let db = 20.0 * level.max(1e-6).log10();
    let lit = (((db + 40.0) / 8.0).ceil().clamp(0.0, 5.0)) as usize;
    format!("[{}{}]", "#".repeat(lit), "-".repeat(5 - lit))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_definition_is_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn peer_addresses_get_the_default_port() {
        assert_eq!(resolve("127.0.0.1").unwrap(), ([127, 0, 0, 1], DEFAULT_PORT).into());
        assert_eq!(resolve("127.0.0.1:9000").unwrap(), ([127, 0, 0, 1], 9000).into());
        assert!(resolve("not a host").is_err());
    }

    #[test]
    fn buffer_argument() {
        assert!(matches!(parse_buffer("auto"), Ok(Buffer::Auto)));
        assert!(matches!(parse_buffer("8"), Ok(Buffer::Fixed(ms)) if ms == 8.0));
        assert!(matches!(parse_buffer("2.5ms"), Ok(Buffer::Fixed(ms)) if ms == 2.5));
        assert!(parse_buffer("fast").is_err());
        assert!(parse_buffer("-1").is_err());
    }

    #[test]
    fn meter_scale() {
        assert_eq!(meter(0.0), "[-----]");
        assert_eq!(meter(1.0), "[#####]");
        assert_eq!(meter(0.1), "[###--]");
    }
}
