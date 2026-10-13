//! `soundbridge`: the command-line front end.

use std::io::{IsTerminal, Write};
use std::net::{SocketAddr, ToSocketAddrs};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};
use soundbridge_core::cpal_backend::CpalBackend;
use soundbridge_core::denoise::{Gate, Mode};
use soundbridge_core::gain::OFFSET_RANGE_DB;
use soundbridge_core::{
    AudioBackend, BufferMode, Config, DEFAULT_PORT, DeviceDirection, Engine, Event, SinkConfig,
    SinkKind, SourceConfig, SourceKind, State, Stats, StreamFormat,
};

/// A low-latency audio bridge between machines on the same network.
///
/// One end runs `listen`, the other `connect`s to it; each end chooses for
/// itself what it sends (--send) and where received audio goes (--play).
/// Both can be given several times, for several streams at once.
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

  The same, while also hearing what the phone is playing:
    soundbridge listen --play mic:virtual-mic --play system:speaker

  Hear this PC on the phone (the phone is listening):
    soundbridge connect 192.168.1.20 --send system --play none

  Talk both ways between two PCs:
    soundbridge listen --send mic
    soundbridge connect otherpc.local --send mic

  Send a headset mic, voice only, and what this PC is playing:
    soundbridge connect 192.168.1.20 --send mic=ID --send system --denoise

  Voice only without losing the start of a word after a pause, at more delay:
    soundbridge connect 192.168.1.20 --send mic --denoise=whole

  The same with a tuning of your own: the gate opens more readily and stays
  open longer:
    soundbridge connect 192.168.1.20 --send mic --denoise --gate-lookahead 80 \
        --gate-hold 400 --gate-threshold 0.4

  Send a quiet microphone at a normal level, a little below the usual:
    soundbridge connect 192.168.1.20 --send mic --auto-gain --gain -3";

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
    /// Update to the latest release, or turn automatic updates on or off
    ///
    /// Left alone, `listen` and `connect` look for a new release once a day
    /// and install it in the background, for the next start.
    #[cfg(feature = "self-update")]
    Update {
        /// Whether updates install themselves from now on, instead of updating now
        #[arg(long, value_name = "on|off", value_parser = parse_switch)]
        auto: Option<bool>,
    },
}

#[cfg(feature = "self-update")]
fn parse_switch(s: &str) -> Result<bool, String> {
    match s {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err("expected on or off".into()),
    }
}

/// One `--send`: a kind of source, and optionally which device.
#[derive(Clone)]
struct Source {
    kind: SourceKind,
    device: Option<String>,
}

/// One `--play`: where streams go, and optionally which kind and on which
/// device. No sink is `none`.
#[derive(Clone)]
struct Sink {
    from: Option<SourceKind>,
    kind: Option<SinkKind>,
    device: Option<String>,
}

fn parse_kind(s: &str) -> Result<SourceKind, String> {
    match s {
        "mic" => Ok(SourceKind::Mic),
        "system" => Ok(SourceKind::System),
        "tone" => Ok(SourceKind::Tone),
        _ => Err(format!("'{s}' is not a kind of source: expected mic, system or tone")),
    }
}

/// Splits a trailing `=DEVICE` off an argument. Device ids may themselves
/// contain `=`, so the first one is the separator.
fn split_device(s: &str) -> (&str, Option<String>) {
    match s.split_once('=') {
        Some((what, device)) => (what, Some(device.to_string()).filter(|d| !d.is_empty())),
        None => (s, None),
    }
}

fn parse_source(s: &str) -> Result<Source, String> {
    let (kind, device) = split_device(s);
    Ok(Source { kind: parse_kind(kind)?, device })
}

fn parse_sink(s: &str) -> Result<Sink, String> {
    let (what, device) = split_device(s);
    let (from, sink) = match what.split_once(':') {
        Some((from, sink)) => (Some(parse_kind(from)?), sink),
        None => (None, what),
    };
    let kind = match sink {
        "speaker" => Some(SinkKind::Speaker),
        "virtual-mic" => Some(SinkKind::VirtualMic),
        "null" => Some(SinkKind::Null),
        "none" => None,
        _ => return Err(format!("'{sink}' is not a sink: expected speaker, virtual-mic, null or none")),
    };
    Ok(Sink { from, kind, device })
}

#[derive(Args)]
struct LinkArgs {
    /// Capture and send audio from this machine: `mic` (a microphone or
    /// other input), `system` (whatever this machine is playing) or `tone`
    /// (a 440 Hz test tone). Add `=ID` to pick the device (see `soundbridge
    /// devices`); for `system` that is the output whose playback is
    /// captured. Repeat to send several streams
    #[arg(short, long, value_name = "SOURCE[=ID]", value_parser = parse_source)]
    send: Vec<Source>,
    /// Channels to send [default: 1 for mic, 2 for system]
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..=2))]
    channels: Option<u16>,
    /// Filter microphones. `reduce` takes background noise out from under
    /// the voice and cuts nothing off (about 10 ms of delay). `voice` also
    /// shuts the stream between phrases, which removes typing, mouse clicks
    /// and breathing (50 ms; can clip the start of a word after a pause).
    /// `whole` is `voice` listening further ahead, which keeps those starts
    /// (130 ms). The --gate options tune `voice` and `whole` further
    #[arg(long, value_name = "MODE", num_args = 0..=1, require_equals = true,
          default_missing_value = "voice")]
    denoise: Option<Mode>,
    /// How far the gate looks ahead of the voice that opens it, ms: more
    /// keeps more of a word's start, and delays the stream as much
    /// [default: 40, or 120 with --denoise=whole]
    #[arg(long, value_name = "MS", requires = "denoise",
          value_parser = clap::value_parser!(u32).range(0..=Gate::MAX_LOOKAHEAD_MS as i64))]
    gate_lookahead: Option<u32>,
    /// How long the gate stays open after the voice stops, ms: more bridges
    /// longer gaps, and lets more noise trail a phrase [default: 250]
    #[arg(long, value_name = "MS", requires = "denoise",
          value_parser = clap::value_parser!(u32).range(10..=Gate::MAX_HOLD_MS as i64))]
    gate_hold: Option<u32>,
    /// How sure the gate must be that it hears a voice, 0.05 to 0.95: lower
    /// opens for quieter speech, and for more that is not speech
    /// [default: 0.5]
    #[arg(long, value_name = "LEVEL", requires = "denoise", value_parser = parse_threshold)]
    gate_threshold: Option<f32>,
    /// Bring speech from microphones to a steady level, however quiet or
    /// loud the microphone is. A limiter keeps it from clipping
    #[arg(long)]
    auto_gain: bool,
    /// Gain for microphones, dB: added to the automatic one, or a fixed
    /// gain (with the limiter) without it
    #[arg(long, default_value_t = 0.0, value_name = "DB", allow_negative_numbers = true,
          value_parser = parse_gain)]
    gain: f32,

    /// What to do with audio received from the peer: `speaker` (an output
    /// device), `virtual-mic` (a microphone other applications can record
    /// from; PipeWire), `null` (receive and measure, play nothing) or `none`
    /// (ignore it). Add `=ID` to pick the device. Prefix `mic:` or `system:`
    /// to play only that kind of stream there, and repeat to send each kind
    /// somewhere else
    #[arg(short, long, value_name = "[KIND:]SINK[=ID]", default_value = "speaker", value_parser = parse_sink)]
    play: Vec<Sink>,

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

fn parse_gain(s: &str) -> Result<f32, String> {
    match s.trim_end_matches("dB").trim_end().parse::<f32>() {
        Ok(db) if OFFSET_RANGE_DB.contains(&db) => Ok(db),
        _ => Err(format!(
            "expected a gain in dB between {} and {}, e.g. `6` or `-6`",
            OFFSET_RANGE_DB.start(),
            OFFSET_RANGE_DB.end()
        )),
    }
}

fn parse_threshold(s: &str) -> Result<f32, String> {
    match s.parse::<f32>() {
        Ok(level) if Gate::THRESHOLDS.contains(&level) => Ok(level),
        _ => Err(format!(
            "expected a level between {} and {}, e.g. `0.4`",
            Gate::THRESHOLDS.start(),
            Gate::THRESHOLDS.end()
        )),
    }
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
    /// The filter for microphones: the chosen mode, with any --gate options
    /// laid over its gate. Those alone make a gate of a mode without one.
    fn denoise(&self) -> Mode {
        let mode = self.denoise.unwrap_or_default();
        if self.gate_lookahead.is_none() && self.gate_hold.is_none() && self.gate_threshold.is_none() {
            return mode;
        }
        let gate = match mode {
            Mode::Gate(gate) => gate,
            _ => Gate::VOICE,
        };
        Mode::Gate(Gate {
            lookahead_ms: self.gate_lookahead.unwrap_or(gate.lookahead_ms),
            hold_ms: self.gate_hold.unwrap_or(gate.hold_ms),
            threshold: self.gate_threshold.unwrap_or(gate.threshold),
        })
        .clamped()
    }

    fn config(&self, port: u16, listen: bool) -> Config {
        Config {
            name: self.name.clone().unwrap_or_else(soundbridge_core::default_name),
            port,
            port_fallback: false,
            listen,
            discover: false,
            send: (self.send.iter())
                .map(|source| SourceConfig {
                    kind: source.kind,
                    device: source.device.clone(),
                    channels: self.channels,
                    denoise: if source.kind == SourceKind::Mic { self.denoise() } else { Mode::Off },
                    auto_gain: self.auto_gain && source.kind == SourceKind::Mic,
                    gain_db: if source.kind == SourceKind::Mic { self.gain } else { 0.0 },
                })
                .collect(),
            play: (self.play.iter())
                .filter_map(|sink| {
                    Some(SinkConfig { kind: sink.kind?, device: sink.device.clone(), from: sink.from })
                })
                .collect(),
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
    let command = Cli::parse().command;
    #[cfg(feature = "self-update")]
    let updated = matches!(command, Command::Listen { .. } | Command::Connect { .. }).then(auto_update);
    let result = match command {
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
        #[cfg(feature = "self-update")]
        Command::Update { auto } => update(auto),
    };
    // Said on the way out, where it cannot land in the statistics line.
    #[cfg(feature = "self-update")]
    if let Some(Ok(version)) = updated.map(|updated| updated.try_recv()) {
        eprintln!("soundbridge: updated to {version}, which the next start runs");
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("soundbridge: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Starts the daily look for a new release; the version arrives if one was
/// installed while this run lasted.
#[cfg(feature = "self-update")]
fn auto_update() -> mpsc::Receiver<soundbridge_update::Version> {
    let (tx, rx) = mpsc::channel();
    soundbridge_update::auto(move |version| {
        let _ = tx.send(version);
    });
    rx
}

#[cfg(feature = "self-update")]
fn update(auto: Option<bool>) -> Result<(), String> {
    use soundbridge_update::Outcome;
    if let Some(auto) = auto {
        soundbridge_update::set_auto_enabled(auto);
    }
    match auto {
        Some(true) => println!("Updates install themselves, looked for once a day"),
        Some(false) => println!("Updates are installed only by `soundbridge update`"),
        None => match soundbridge_update::update()? {
            Outcome::Current(version) => println!("{version} is the latest release"),
            Outcome::Updated(version) => println!("Updated to {version}"),
        },
    }
    Ok(())
}

fn devices() {
    let backend = CpalBackend::new();
    let devices = backend.devices();
    println!("Audio system: {}", backend.host_name());
    for (direction, title) in [
        (DeviceDirection::Input, "Inputs (--send mic=ID)"),
        (DeviceDirection::Output, "Outputs (--play speaker=ID, or --send system=ID)"),
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
        send: Vec::new(),
        play: Vec::new(),
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
        // One stream each way gets the full story; more have to share the line.
        let brief = stats.tx.len() > 1 || stats.rx.len() > 1;
        let mut parts = Vec::new();
        for tx in &stats.tx {
            let filtered = match tx.denoise {
                Mode::Off => "",
                Mode::Reduce => " less noise",
                Mode::Gate(_) => " voice only",
            };
            let mut part = format!("send {}{filtered} {}", stream(tx.kind, tx.format, brief), meter(tx.level));
            if let Some(remote) = tx.remote.filter(|_| !brief) {
                part += &format!(" > peer buffer {:.1} ms", remote.buffer_us as f32 / 1000.0);
                if remote.lost > 0 || remote.underruns > 0 {
                    part += &format!(" (lost {}, underruns {})", remote.lost, remote.underruns);
                }
            }
            parts.push(part);
        }
        for rx in &stats.rx {
            let state = if rx.playing { "" } else { " (buffering)" };
            let mut part = format!(
                "recv {} {} buffer {:.1} ms{state}",
                stream(rx.kind, rx.format, brief),
                meter(rx.level),
                rx.buffer_ms
            );
            if !brief {
                part += &format!(
                    " jitter {:.1} lost {} late {} underruns {} {:+} ppm",
                    rx.jitter_ms, rx.lost, rx.late, rx.underruns, rx.drift_ppm,
                );
            } else if rx.lost > 0 || rx.underruns > 0 {
                part += &format!(" (lost {}, underruns {})", rx.lost, rx.underruns);
            }
            parts.push(part);
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

/// Names a stream by what it carries: at length, or just its kind.
fn stream(kind: SourceKind, format: StreamFormat, brief: bool) -> String {
    let kind = match kind {
        SourceKind::Mic => "mic",
        SourceKind::System => "system",
        SourceKind::Tone => "tone",
    };
    if brief { kind.to_string() } else { format!("{kind} {format}") }
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
    fn sources_and_sinks() {
        let source = parse_source("mic=pipewire:alsa_input.usb={a=b}").unwrap();
        assert_eq!((source.kind, source.device.as_deref()), (SourceKind::Mic, Some("pipewire:alsa_input.usb={a=b}")));
        assert!(parse_source("system").unwrap().device.is_none());
        assert!(parse_source("loud").is_err());

        let sink = parse_sink("mic:virtual-mic").unwrap();
        assert_eq!((sink.from, sink.kind), (Some(SourceKind::Mic), Some(SinkKind::VirtualMic)));
        let sink = parse_sink("speaker=wasapi:{0.0}").unwrap();
        assert_eq!((sink.from, sink.kind, sink.device.as_deref()), (None, Some(SinkKind::Speaker), Some("wasapi:{0.0}")));
        assert!(parse_sink("none").unwrap().kind.is_none());
        assert!(parse_sink("mic:loud").is_err());
        assert!(parse_sink("loud:speaker").is_err());
    }

    #[test]
    fn gain_is_for_microphones() {
        let cli = Cli::try_parse_from([
            "soundbridge", "connect", "host", "--send", "mic", "--send", "system", "--auto-gain", "--gain", "-6",
        ])
        .unwrap();
        let Command::Connect { link, .. } = cli.command else { panic!() };
        let sent: Vec<_> = link.config(0, false).send.iter().map(|s| (s.auto_gain, s.gain_db)).collect();
        assert_eq!(sent, [(true, -6.0), (false, 0.0)]);
        assert!(Cli::try_parse_from(["soundbridge", "listen", "--gain", "90"]).is_err());
    }

    #[test]
    fn several_streams_each_way() {
        let cli = Cli::try_parse_from([
            "soundbridge", "listen", "--denoise", "--send", "mic", "--send", "system", "--play", "mic:virtual-mic",
            "--play", "system:speaker", "--play", "none",
        ])
        .unwrap();
        let Command::Listen { link, .. } = cli.command else { panic!() };
        let config = link.config(0, true);
        let sent: Vec<_> = config.send.iter().map(|s| (s.kind, s.denoise)).collect();
        assert_eq!(sent, [(SourceKind::Mic, Mode::VOICE), (SourceKind::System, Mode::Off)]);
        let played: Vec<_> = config.play.iter().map(|s| (s.from, s.kind)).collect();
        assert_eq!(
            played,
            [(Some(SourceKind::Mic), SinkKind::VirtualMic), (Some(SourceKind::System), SinkKind::Speaker)]
        );

        // With nothing said, received audio goes to the speakers.
        let cli = Cli::try_parse_from(["soundbridge", "connect", "host"]).unwrap();
        let Command::Connect { link, .. } = cli.command else { panic!() };
        let config = link.config(0, false);
        assert!(config.send.is_empty());
        assert_eq!(config.play, [SinkConfig::new(SinkKind::Speaker)]);
    }

    #[test]
    fn denoise_presets_and_tuning() {
        let denoise = |args: &[&str]| {
            let args = ["soundbridge", "connect", "host", "--send", "mic"].iter().chain(args);
            let Command::Connect { link, .. } = Cli::try_parse_from(args)?.command else { panic!() };
            Ok::<_, clap::Error>(link.config(0, false).send[0].denoise)
        };
        assert_eq!(denoise(&[]).unwrap(), Mode::Off);
        assert_eq!(denoise(&["--denoise"]).unwrap(), Mode::VOICE);
        assert_eq!(denoise(&["--denoise=reduce"]).unwrap(), Mode::Reduce);
        assert_eq!(denoise(&["--denoise=whole"]).unwrap(), Mode::WHOLE_WORDS);
        assert_eq!(
            denoise(&["--denoise=whole", "--gate-hold", "400", "--gate-threshold", "0.4"]).unwrap(),
            Mode::Gate(Gate { hold_ms: 400, threshold: 0.4, ..Gate::WHOLE_WORDS })
        );
        assert_eq!(
            denoise(&["--denoise", "--gate-lookahead", "85"]).unwrap(),
            Mode::Gate(Gate { lookahead_ms: 80, ..Gate::VOICE })
        );
        assert!(denoise(&["--denoise=loud"]).is_err());
        assert!(denoise(&["--gate-hold", "400"]).is_err());
        assert!(denoise(&["--denoise", "--gate-threshold", "2"]).is_err());
    }

    #[test]
    fn meter_scale() {
        assert_eq!(meter(0.0), "[-----]");
        assert_eq!(meter(1.0), "[#####]");
        assert_eq!(meter(0.1), "[###--]");
    }
}
