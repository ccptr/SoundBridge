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
    SinkKind, SourceConfig, SourceKind, SourceSettings, State, Stats, StreamFormat, Streams,
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

  The same, while also hearing what the phone is playing (a phone's
  microphone is the source `mic`, what it plays is `system`):
    soundbridge listen --play mic:virtual-mic --play speaker

  Send two microphones under names of their own, and at the other end play
  the second on a device of its own and the rest on the default one. A
  stream is known by the id of the device it is captured from:
    soundbridge connect 192.168.1.20 --send headset:mic=ID --send desk:mic=OTHER
    soundbridge listen --play OTHER:speaker=OUTPUT --play speaker

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
    soundbridge connect 192.168.1.20 --send mic --auto-gain --gain -3

  Use a phone as this PC's microphone, and set from here how the phone
  filters and levels it:
    soundbridge listen --play virtual-mic --remote denoise=whole --remote gain=6";

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
        /// Say whether there is a newer release, without installing it
        #[arg(long, conflicts_with = "auto")]
        check: bool,
        /// Whether updates install themselves from now on, instead of updating now
        #[arg(long, value_name = "on|off", value_parser = parse_switch)]
        auto: Option<bool>,
    },
}

fn parse_switch(s: &str) -> Result<bool, String> {
    match s {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err("expected on or off".into()),
    }
}

/// One `--send`: a kind of source, and optionally what the stream is called
/// and which device it is captured from.
#[derive(Clone)]
struct Source {
    name: Option<String>,
    kind: SourceKind,
    device: Option<String>,
}

/// One `--play`: where streams go, and optionally which of them and on
/// which device. No sink is `none`.
#[derive(Clone)]
struct Sink {
    from: Streams,
    kind: Option<SinkKind>,
    device: Option<String>,
}

/// One `--remote`: a setting for the microphones the peer sends, or for the
/// one whose source is named.
#[derive(Clone, Debug, PartialEq)]
struct Remote {
    source: Option<String>,
    change: Change,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Change {
    Denoise(Mode),
    AutoGain(bool),
    Gain(f32),
}

impl Remote {
    /// `source`, which the peer knows by `id`, as it is wanted, if this is
    /// about it.
    fn apply(&self, source: SourceSettings, id: &str) -> SourceSettings {
        if self.source.as_ref().is_some_and(|wanted| wanted != id) {
            return source;
        }
        match self.change {
            Change::Denoise(denoise) => SourceSettings { denoise, ..source },
            Change::AutoGain(auto_gain) => SourceSettings { auto_gain, ..source },
            Change::Gain(gain_db) => SourceSettings { gain_db, ..source },
        }
    }
}

/// Splits `[SOURCE:]REST`, where REST starts with one of `words`, alone or
/// with `=` after it. SOURCE is a device id, which may hold anything.
fn split_source<'a>(s: &'a str, words: &[&str]) -> (Option<&'a str>, &'a str) {
    let starts = |rest: &str| {
        words.iter().any(|word| rest.strip_prefix(word).is_some_and(|after| after.is_empty() || after.starts_with('=')))
    };
    if starts(s) {
        return (None, s);
    }
    let colon = s.match_indices(':').map(|(at, _)| at).find(|&at| starts(&s[at + 1..]));
    match colon {
        Some(at) => (Some(&s[..at]), &s[at + 1..]),
        None => (None, s),
    }
}

fn parse_remote(s: &str) -> Result<Remote, String> {
    let (source, setting) = split_source(s, &["denoise", "auto-gain", "gain"]);
    let source = source.map(str::to_string);
    let expected = "expected denoise=MODE, auto-gain=on|off or gain=DB";
    let (name, value) = setting.split_once('=').ok_or_else(|| format!("'{setting}' is not a setting: {expected}"))?;
    let change = match name {
        "denoise" => Change::Denoise(value.parse()?),
        "auto-gain" => Change::AutoGain(parse_switch(value)?),
        "gain" => Change::Gain(parse_gain(value)?),
        _ => return Err(format!("'{name}' is not a setting: {expected}")),
    };
    Ok(Remote { source, change })
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
    let (what, device) = split_device(s);
    // The name may itself hold a colon; the kind cannot.
    let (name, kind) = match what.rsplit_once(':') {
        Some((name, kind)) => (Some(name.to_string()).filter(|name| !name.is_empty()), kind),
        None => (None, what),
    };
    Ok(Source { name, kind: parse_kind(kind)?, device })
}

fn parse_sink(s: &str) -> Result<Sink, String> {
    let (source, sink) = split_source(s, &["speaker", "virtual-mic", "null", "none"]);
    let from = match source {
        Some("") => return Err("there is no source before the colon".into()),
        Some(source) => Streams::Source(source.to_string()),
        None => Streams::All,
    };
    let (sink, device) = split_device(sink);
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
    /// captured. Repeat to send several streams. The peer knows each by
    /// its source: the ID of the device, or without one `mic`, `system` or
    /// `tone`. Prefix `NAME:` to tell it what to show the stream as
    /// [default: the name of the device, if one is picked]
    #[arg(short, long, value_name = "[NAME:]SOURCE[=ID]", value_parser = parse_source)]
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
    /// (ignore it). Add `=ID` to pick the device. Prefix the peer's id for
    /// a source to play that one stream there (`mic:`, `system:`, or the id
    /// of the device it captures from, which is printed when the stream
    /// first arrives); that stream then plays nowhere else, and with `none`
    /// nowhere at all. Repeat to give each stream a place; without a source
    /// it is where the streams that have none go
    #[arg(short, long, value_name = "[SOURCE:]SINK[=ID]", default_value = "speaker", value_parser = parse_sink)]
    play: Vec<Sink>,

    /// Set how the peer filters and levels the microphones it sends, as if
    /// that had been set there: `denoise=MODE` (off, reduce, voice, whole,
    /// or gate:LOOKAHEAD:HOLD:THRESHOLD for a tuning of the gate),
    /// `auto-gain=on|off` or `gain=DB`. Prefix the peer's id for a source
    /// for that one only (`mic:gain=6`). Repeat for several settings. What
    /// is not mentioned stays as the peer has it
    #[arg(long, value_name = "[SOURCE:]SETTING=VALUE", value_parser = parse_remote)]
    remote: Vec<Remote>,

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
                    id: SourceConfig::id_of(source.kind, source.device.as_deref()),
                    name: source.name.clone().unwrap_or_default(),
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
                    let kind = match (sink.kind, &sink.from) {
                        (Some(kind), _) => kind,
                        // Said of one stream, `none` takes it off the other sinks.
                        (None, Streams::Source(_)) => SinkKind::Nothing,
                        (None, _) => return None,
                    };
                    Some(SinkConfig { kind, device: sink.device.clone(), from: sink.from.clone() })
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
        Command::Listen { port, once, link } => run_link(named(link.config(port, true)), None, once, &link),
        Command::Connect { peer, link } => match resolve(&peer) {
            // Any free local port: the dialer does not need to be findable.
            Ok(addr) => run_link(named(link.config(0, false)), Some(addr), true, &link),
            Err(e) => Err(e),
        },
        Command::Discover { timeout } => discover(timeout),
        Command::Devices => {
            devices();
            Ok(())
        }
        #[cfg(feature = "self-update")]
        Command::Update { check, auto } => update(check, auto),
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
fn update(check: bool, auto: Option<bool>) -> Result<(), String> {
    use soundbridge_update::Outcome;
    if check {
        match soundbridge_update::check()? {
            Some(version) => println!("{version} has been released; `soundbridge update` installs it"),
            None => println!("{} is the latest release", soundbridge_update::Version::current()),
        }
        return Ok(());
    }
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

/// Calls each source that was given a device and no name what that device
/// is called.
fn named(mut config: Config) -> Config {
    let unnamed = |source: &SourceConfig| source.name.is_empty() && source.device.is_some();
    if !config.send.iter().any(unnamed) {
        return config;
    }
    let devices = CpalBackend::new().devices();
    for source in config.send.iter_mut().filter(|source| unnamed(source)) {
        let direction = match source.kind {
            SourceKind::System => DeviceDirection::Output,
            _ => DeviceDirection::Input,
        };
        let device = devices.iter().find(|d| Some(&d.id) == source.device.as_ref() && d.direction == direction);
        if let Some(device) = device {
            source.name = device.name.clone();
        }
    }
    config
}

/// How a source is filtered and levelled, in the words of `--remote`.
fn settings_text(source: &SourceSettings) -> String {
    let auto_gain = if source.auto_gain { "on" } else { "off" };
    format!("denoise={} auto-gain={auto_gain} gain={}", source.denoise, source.gain_db)
}

/// Runs a session until interrupted, or (with `until_idle`) until the first
/// session ends.
fn run_link(config: Config, dial: Option<SocketAddr>, until_idle: bool, link: &LinkArgs) -> Result<(), String> {
    let port = config.port;
    let sent = (config.send.iter())
        .map(|source| if source.name.is_empty() { source.id.clone() } else { source.name.clone() })
        .collect();
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

    let mut status = Status::new(link.quiet, std::io::stderr().is_terminal(), sent);
    match dial {
        Some(addr) => engine.connect(addr),
        None => status.line(&format!("Listening on port {}", engine.local_port())),
    }

    let mut was_connected = false;
    let mut failed = false;
    // The peer's streams that --remote has been applied to. Each is asked
    // once: after that, what is set at the other end stands.
    let mut asked: Vec<u8> = Vec::new();
    // The sources the peer has been seen to send.
    let mut seen: Vec<String> = Vec::new();
    while let Ok(Some(event)) = rx.recv() {
        match event {
            Event::State(State::Connecting { addr }) => status.line(&format!("Connecting to {addr}...")),
            Event::State(State::Connected { addr, name }) => {
                was_connected = true;
                asked.clear();
                seen.clear();
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
            Event::Stats(stats) => {
                let tunable = |source: &&SourceSettings| source.kind != SourceKind::System;
                // Said once for each, as its id is what --play and --remote take.
                seen.retain(|id| stats.remote_streams.iter().any(|stream| stream.id == *id));
                for stream in &stats.remote_streams {
                    if !seen.contains(&stream.id) {
                        seen.push(stream.id.clone());
                        let called = if stream.name.is_empty() { String::new() } else { format!(" ({})", stream.name) };
                        status.line(&format!("The peer sends {}{called}", stream.id));
                    }
                }
                asked.retain(|slot| stats.remote_sources.iter().filter(tunable).any(|s| s.slot == *slot));
                for source in stats.remote_sources.iter().filter(tunable) {
                    if link.remote.is_empty() || asked.contains(&source.slot) {
                        continue;
                    }
                    // Which source it is comes a moment after how it is set.
                    let Some(stream) = stats.remote_stream(source.slot) else { continue };
                    asked.push(source.slot);
                    let wanted = link.remote.iter().fold(*source, |source, remote| remote.apply(source, &stream.id));
                    if wanted != *source {
                        engine.set_remote(wanted);
                        status.line(&format!("Asking for {}: {}", stream.id, settings_text(&wanted)));
                    }
                }
                status.stats(&stats)
            }
            Event::SourceChanged { source, settings } => {
                let name = status.sent.get(source).cloned().unwrap_or_default();
                status.line(&format!("The peer set {name}: {}", settings_text(&settings)))
            }
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
    /// What each of the sources this end sends goes by.
    sent: Vec<String>,
}

const LOG_STATS_EVERY: Duration = Duration::from_secs(5);

impl Status {
    fn new(quiet: bool, tty: bool, sent: Vec<String>) -> Self {
        Status { quiet, tty, drawn: false, last_logged: None, sent }
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
            let name = self.sent.get(tx.source).map(String::as_str).unwrap_or_default();
            let stream = stream(name, tx.format, brief);
            let mut part = format!("send {stream}{filtered} {}", meter(tx.level));
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
                stream(&remote_name(stats, rx.slot, rx.kind), rx.format, brief),
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

/// What a stream the peer sends goes by: the name it has there, or failing
/// that the id of its source.
fn remote_name(stats: &Stats, slot: u8, kind: SourceKind) -> String {
    match stats.remote_stream(slot) {
        Some(stream) if !stream.name.is_empty() => stream.name.clone(),
        Some(stream) => stream.id.clone(),
        None => SourceConfig::id_of(kind, None),
    }
}

/// A stream in the statistics line: at length, or just what it goes by.
fn stream(name: &str, format: StreamFormat, brief: bool) -> String {
    if brief { name.to_string() } else { format!("{name} {format}") }
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
        let source = parse_source("Desk: left:mic=alsa:hw:1").unwrap();
        assert_eq!(
            (source.name.as_deref(), source.kind, source.device.as_deref()),
            (Some("Desk: left"), SourceKind::Mic, Some("alsa:hw:1"))
        );
        assert!(parse_source(":tone").unwrap().name.is_none());
        assert!(parse_source("mic:loud").is_err());

        let sink = parse_sink("speaker=wasapi:{0.0}").unwrap();
        assert_eq!(
            (sink.from, sink.kind, sink.device.as_deref()),
            (Streams::All, Some(SinkKind::Speaker), Some("wasapi:{0.0}"))
        );
        assert!(parse_sink("none").unwrap().kind.is_none());
        assert!(parse_sink("mic:loud").is_err());
        assert!(parse_sink(":speaker").is_err());

        // A stream is picked by the id of its source, which may hold what
        // the rest is told apart by.
        let source = |id: &str| Streams::Source(id.into());
        let sink = parse_sink("mic:virtual-mic").unwrap();
        assert_eq!((sink.from, sink.kind), (source("mic"), Some(SinkKind::VirtualMic)));
        let sink = parse_sink("pipewire:alsa_input.usb=1:speaker=pipewire:out:null").unwrap();
        assert_eq!(
            (sink.from, sink.kind, sink.device.as_deref()),
            (source("pipewire:alsa_input.usb=1"), Some(SinkKind::Speaker), Some("pipewire:out:null"))
        );
        let sink = parse_sink("alsa:null:none").unwrap();
        assert_eq!((sink.from, sink.kind), (source("alsa:null"), None));
        let sink = parse_sink("speaker=alsa:null").unwrap();
        assert_eq!((sink.from, sink.device.as_deref()), (Streams::All, Some("alsa:null")));
    }

    #[test]
    fn one_stream_somewhere_else_or_nowhere() {
        let cli = Cli::try_parse_from([
            "soundbridge", "listen", "--play", "mic:virtual-mic", "--play", "wasapi:{0.1}:none", "--play", "none",
            "--play", "speaker",
        ])
        .unwrap();
        let Command::Listen { link, .. } = cli.command else { panic!() };
        let played: Vec<_> = link.config(0, true).play.into_iter().map(|s| (s.from, s.kind)).collect();
        assert_eq!(
            played,
            [
                (Streams::Source("mic".into()), SinkKind::VirtualMic),
                (Streams::Source("wasapi:{0.1}".into()), SinkKind::Nothing),
                (Streams::All, SinkKind::Speaker)
            ]
        );
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
        let sent: Vec<_> = config.send.iter().map(|s| (s.id.as_str(), s.kind, s.denoise)).collect();
        assert_eq!(sent, [("mic", SourceKind::Mic, Mode::VOICE), ("system", SourceKind::System, Mode::Off)]);
        let played: Vec<_> = config.play.into_iter().map(|s| (s.from, s.kind)).collect();
        assert_eq!(
            played,
            [
                (Streams::Source("mic".into()), SinkKind::VirtualMic),
                (Streams::Source("system".into()), SinkKind::Speaker)
            ]
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
    fn settings_for_the_peer() {
        let remote = |s| parse_remote(s).unwrap();
        assert_eq!(remote("denoise=whole"), Remote { source: None, change: Change::Denoise(Mode::WHOLE_WORDS) });
        assert_eq!(
            remote("pipewire:usb=1:denoise=gate:80:400:0.4"),
            Remote {
                source: Some("pipewire:usb=1".into()),
                change: Change::Denoise(Mode::Gate(Gate { lookahead_ms: 80, hold_ms: 400, threshold: 0.4 }))
            }
        );
        assert_eq!(remote("auto-gain=off").change, Change::AutoGain(false));
        assert_eq!(remote("mic:gain=-6"), Remote { source: Some("mic".into()), change: Change::Gain(-6.0) });
        for bad in ["denoise", "loud=1", "gain=90", "auto-gain=maybe", "mic:loud=6", "denoise=gate:80"] {
            assert!(parse_remote(bad).is_err(), "{bad}");
        }

        // Each stream takes what is said of all of them and of itself, in order.
        let cli = Cli::try_parse_from([
            "soundbridge", "listen", "--remote", "denoise=voice", "--remote", "gain=6", "--remote", "desk:gain=-6",
        ])
        .unwrap();
        let Command::Listen { link, .. } = cli.command else { panic!() };
        let source =
            SourceSettings { slot: 0, kind: SourceKind::Mic, denoise: Mode::Off, auto_gain: true, gain_db: 0.0 };
        let wanted = |id| link.remote.iter().fold(source, |source, remote| remote.apply(source, id));
        assert_eq!(wanted("mic"), SourceSettings { denoise: Mode::VOICE, gain_db: 6.0, ..source });
        assert_eq!(wanted("desk"), SourceSettings { denoise: Mode::VOICE, gain_db: -6.0, ..source });
    }

    #[test]
    fn a_stream_is_its_source_and_goes_by_a_name() {
        let cli = Cli::try_parse_from([
            "soundbridge", "connect", "host", "--send", "headset:mic=alsa:hw:1", "--send", "tone",
        ]);
        let Command::Connect { link, .. } = cli.unwrap().command else { panic!() };
        let sent: Vec<_> = link.config(0, false).send.into_iter().map(|s| (s.id, s.name)).collect();
        assert_eq!(sent, [("alsa:hw:1".to_string(), "headset".to_string()), ("tone".to_string(), String::new())]);

        let format = StreamFormat { sample_rate: 48_000, channels: 1 };
        assert_eq!(stream("headset", format, true), "headset");
        assert_eq!(stream("headset", format, false), "headset 48.0 kHz mono");
    }

    #[test]
    fn meter_scale() {
        assert_eq!(meter(0.0), "[-----]");
        assert_eq!(meter(1.0), "[#####]");
        assert_eq!(meter(0.1), "[###--]");
    }
}
