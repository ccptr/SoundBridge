//! Two engines talking over loopback, with a generated tone on one end and a
//! clocked null output on the other. No sound hardware involved.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use soundbridge_core::audio::{MakeCapture, MakeRender};
use soundbridge_core::denoise;
use soundbridge_core::{
    AudioBackend, CaptureRequest, Config, DeviceInfo, Engine, Event, PlaybackRequest, SinkConfig,
    SinkKind, SourceConfig, SourceKind, State, Stats, StreamHandle, Streams,
};

/// The synthetic source and sink live in the engine; nothing here should
/// ever reach a real backend.
struct NoBackend;

impl AudioBackend for NoBackend {
    fn devices(&self) -> Vec<DeviceInfo> {
        Vec::new()
    }
    fn open_capture(&self, _: &CaptureRequest, _: MakeCapture) -> Result<Box<dyn StreamHandle>, String> {
        Err("no devices in tests".into())
    }
    fn open_playback(&self, _: &PlaybackRequest, _: MakeRender) -> Result<Box<dyn StreamHandle>, String> {
        Err("no devices in tests".into())
    }
}

fn engine(config: Config) -> (Engine, Receiver<Event>) {
    let (tx, rx) = mpsc::channel();
    let engine = Engine::start(config, Arc::new(NoBackend), move |e| {
        let _ = tx.send(e);
    })
    .unwrap();
    (engine, rx)
}

fn config(name: &str) -> Config {
    Config { name: name.into(), port: 0, send: Vec::new(), play: Vec::new(), ..Config::default() }
}

fn wait_for<T>(rx: &Receiver<Event>, what: &str, mut pick: impl FnMut(Event) -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    while let Ok(event) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        if let Some(found) = pick(event) {
            return found;
        }
    }
    panic!("timed out waiting for {what}");
}

fn local(engine: &Engine) -> SocketAddr {
    ([127, 0, 0, 1], engine.local_port()).into()
}

#[test]
fn tone_flows_one_way_and_reports_back() {
    let (listener, listener_events) = engine(Config {
        play: vec![SinkConfig::new(SinkKind::Null)],
        ..config("listener")
    });
    let (dialer, dialer_events) = engine(Config {
        send: vec![SourceConfig::new(SourceKind::Tone)],
        ..config("dialer")
    });
    dialer.connect(local(&listener));

    let name = wait_for(&dialer_events, "dialer to connect", |e| match e {
        Event::State(State::Connected { name, .. }) => Some(name),
        _ => None,
    });
    assert_eq!(name, "listener");
    let name = wait_for(&listener_events, "listener to accept", |e| match e {
        Event::State(State::Connected { name, .. }) => Some(name),
        Event::Error(e) => panic!("{e}"),
        _ => None,
    });
    assert_eq!(name, "dialer");

    // Let it run long enough to pass warm-up and settle.
    let settle = Instant::now() + Duration::from_secs(3);
    let rx = wait_for(&listener_events, "steady playback", |e| match e {
        Event::Stats(Stats { rx, .. }) if Instant::now() > settle => rx.first().copied(),
        Event::Error(e) => panic!("{e}"),
        _ => None,
    });
    assert!(rx.playing);
    assert!(rx.packets > 800, "only {} packets in 3 s", rx.packets);
    assert_eq!(rx.lost, 0);
    assert_eq!((rx.format.sample_rate, rx.format.channels), (48_000, 1));
    // The tone is a quarter-scale sine.
    assert!((0.2..0.3).contains(&rx.level), "level {}", rx.level);
    // Loopback with sleep-clocked endpoints: a handful of milliseconds.
    assert!(rx.buffer_ms < 25.0, "buffer {} ms", rx.buffer_ms);

    // The sender hears how its stream is doing at the far end, and the RTT.
    // (Its event queue also holds reports from before playback started.)
    let stats = wait_for(&dialer_events, "a receiver report", |e| match e {
        Event::Stats(s) if s.rtt_ms.is_some() && s.tx.iter().any(|tx| tx.remote.is_some_and(|r| r.playing)) => {
            Some(s)
        }
        _ => None,
    });
    assert!(stats.rtt_ms.unwrap() < 50.0);
    assert!(stats.tx[0].packets > 0);

    // Hanging up on one side idles the other without waiting for a timeout.
    let hung_up = Instant::now();
    dialer.disconnect();
    wait_for(&listener_events, "listener to go idle", |e| match e {
        Event::State(State::Idle) => Some(()),
        _ => None,
    });
    assert!(hung_up.elapsed() < Duration::from_secs(1));
}

#[test]
fn both_directions_at_once() {
    let both = |name| Config {
        send: vec![SourceConfig { channels: Some(2), ..SourceConfig::new(SourceKind::Tone) }],
        play: vec![SinkConfig::new(SinkKind::Null)],
        ..config(name)
    };
    let (a, a_events) = engine(both("a"));
    let (b, b_events) = engine(both("b"));
    b.connect(local(&a));
    for events in [&a_events, &b_events] {
        let rx = wait_for(events, "playback", |e| match e {
            Event::Stats(Stats { rx, tx, .. }) if !tx.is_empty() => rx.first().copied().filter(|rx| rx.playing),
            Event::Error(e) => panic!("{e}"),
            _ => None,
        });
        assert_eq!(rx.format.channels, 2);
    }
}

fn tone_source(channels: u16) -> SourceConfig {
    SourceConfig { channels: Some(channels), ..SourceConfig::new(SourceKind::Tone) }
}

#[test]
fn several_streams_one_way_each_go_where_their_kind_is_played() {
    let (listener, listener_events) = engine(Config {
        play: vec![
            SinkConfig::new(SinkKind::Null),
            SinkConfig { from: Streams::Kind(SourceKind::Tone), ..SinkConfig::new(SinkKind::Null) },
            SinkConfig { from: Streams::Kind(SourceKind::Mic), ..SinkConfig::new(SinkKind::Null) },
        ],
        ..config("listener")
    });
    let (dialer, dialer_events) = engine(Config {
        send: vec![tone_source(1), tone_source(2), tone_source(1)],
        ..config("dialer")
    });
    dialer.connect(local(&listener));

    // Each of the three streams is played on the two sinks that take tones,
    // and on neither twice.
    let mut rx = wait_for(&listener_events, "every stream to play", |e| match e {
        Event::Stats(Stats { rx, .. }) if rx.len() == 6 && rx.iter().all(|rx| rx.playing) => Some(rx),
        Event::Stats(Stats { rx, .. }) if rx.len() > 6 => panic!("{} outputs for 3 streams", rx.len()),
        Event::Error(e) => panic!("{e}"),
        _ => None,
    });
    rx.sort_by_key(|rx| rx.slot);
    let seen: Vec<_> = rx.iter().map(|rx| (rx.slot, rx.format.channels, rx.kind)).collect();
    let tone = SourceKind::Tone;
    assert_eq!(seen, [(0, 1, tone), (0, 1, tone), (1, 2, tone), (1, 2, tone), (2, 1, tone), (2, 1, tone)]);

    // The sender hears about each of its streams separately.
    let tx = wait_for(&dialer_events, "a report on every stream", |e| match e {
        Event::Stats(Stats { tx, .. }) if tx.iter().all(|tx| tx.remote.is_some_and(|r| r.playing)) => Some(tx),
        _ => None,
    });
    assert_eq!(tx.iter().map(|tx| (tx.slot, tx.remote.unwrap().slot)).collect::<Vec<_>>(), [(0, 0), (1, 1), (2, 2)]);

    // Dropping the last source leaves the others running, not restarted...
    dialer.configure(Config { send: vec![tone_source(1), tone_source(2)], ..config("dialer") });
    let after = wait_for(&dialer_events, "the third stream to stop", |e| match e {
        Event::Stats(Stats { tx, .. }) if tx.len() == 2 => Some(tx),
        _ => None,
    });
    assert!(after[0].packets >= tx[0].packets && after[1].packets >= tx[1].packets);
    // ...and the far end closes what it had opened for the one that stopped.
    wait_for(&listener_events, "the stopped stream's outputs to close", |e| match e {
        Event::Stats(Stats { rx, .. }) if rx.len() == 4 => Some(()),
        _ => None,
    });
}

#[test]
fn a_stream_named_by_slot_goes_only_where_it_is_sent() {
    let null = |from| SinkConfig { from, ..SinkConfig::new(SinkKind::Null) };
    let (listener, listener_events) = engine(Config {
        // Sinks 0 and 1 take the second stream away from sink 2, which is
        // left with the first; the third goes nowhere.
        play: vec![
            null(Streams::Slot(1)),
            null(Streams::Slot(1)),
            null(Streams::All),
            SinkConfig { from: Streams::Slot(2), ..SinkConfig::new(SinkKind::Nothing) },
        ],
        ..config("listener")
    });
    let (dialer, dialer_events) = engine(Config {
        send: vec![tone_source(1), tone_source(2), tone_source(1)],
        ..config("dialer")
    });
    dialer.connect(local(&listener));

    // Long enough for a stream that should not play to have turned up.
    let settle = Instant::now() + Duration::from_secs(1);
    let mut rx = wait_for(&listener_events, "the streams to play", |e| match e {
        Event::Stats(Stats { rx, .. }) if rx.len() > 3 => panic!("{} outputs", rx.len()),
        Event::Stats(Stats { rx, .. })
            if Instant::now() > settle && rx.len() == 3 && rx.iter().all(|rx| rx.playing) =>
        {
            Some(rx)
        }
        Event::Error(e) => panic!("{e}"),
        _ => None,
    });
    rx.sort_by_key(|rx| rx.slot);
    assert_eq!(rx.iter().map(|rx| (rx.slot, rx.format.channels)).collect::<Vec<_>>(), [(0, 1), (1, 2), (1, 2)]);

    // The sender is told which of its streams is not played.
    let tx = wait_for(&dialer_events, "reports on the played streams", |e| match e {
        Event::Stats(Stats { tx, .. }) if tx.len() == 3 && tx[..2].iter().all(|tx| tx.remote.is_some()) => Some(tx),
        _ => None,
    });
    assert_eq!(tx[2].remote, None);
}

#[test]
fn a_filtered_source_is_sent_as_mono_speech() {
    let (listener, listener_events) =
        engine(Config { play: vec![SinkConfig::new(SinkKind::Null)], ..config("listener") });
    let (dialer, dialer_events) = engine(Config {
        // Asked for in stereo and at another rate; the filter overrides both.
        send: vec![SourceConfig { denoise: denoise::Mode::VOICE, ..tone_source(2) }],
        sample_rate: 44_100,
        ..config("dialer")
    });
    dialer.connect(local(&listener));
    let tx = wait_for(&dialer_events, "sending to start", |e| match e {
        Event::Stats(Stats { tx, .. }) => tx.first().copied(),
        Event::Info(text) => panic!("{text}"),
        _ => None,
    });
    assert_eq!(tx.denoise, denoise::Mode::VOICE);
    assert_eq!((tx.format.sample_rate, tx.format.channels), (48_000, 1));
    // One packet per frame of the filter.
    assert_eq!(tx.packet_ms, 10.0);
    // A filtered stream still flows while its gate is shut.
    wait_for(&listener_events, "playback", |e| match e {
        Event::Stats(Stats { rx, .. }) => rx.first().filter(|rx| rx.playing).map(|_| ()),
        Event::Error(e) => panic!("{e}"),
        _ => None,
    });
}

#[test]
fn a_busy_listener_turns_a_second_caller_away() {
    let (listener, listener_events) = engine(config("listener"));
    let (first, first_events) = engine(config("first"));
    let (second, second_events) = engine(config("second"));
    first.connect(local(&listener));
    wait_for(&first_events, "first to connect", |e| match e {
        Event::State(State::Connected { .. }) => Some(()),
        _ => None,
    });
    second.connect(local(&listener));
    wait_for(&second_events, "second to be refused", |e| match e {
        Event::State(State::Idle) => Some(()),
        Event::State(State::Connected { .. }) => panic!("second caller got in"),
        _ => None,
    });
    // The first session is undisturbed.
    assert!(!listener_events.try_iter().any(|e| e == Event::State(State::Idle)));
}

#[test]
fn not_listening_means_not_reachable() {
    let (hidden, _hidden_events) = engine(Config { listen: false, ..config("hidden") });
    let (caller, caller_events) = engine(config("caller"));
    caller.connect(local(&hidden));
    wait_for(&caller_events, "refusal", |e| match e {
        Event::State(State::Idle) => Some(()),
        Event::State(State::Connected { .. }) => panic!("connected to a non-listener"),
        _ => None,
    });
}

#[test]
fn the_dialer_redials_after_the_listener_goes_silent() {
    use soundbridge_core::protocol::{Packet, decode};

    // A hand-driven peer, so it can vanish without saying goodbye.
    let fake = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    fake.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    // Answers hellos and pings like a listener for as long as `alive` says so.
    let serve = |alive: &dyn Fn() -> bool| {
        let mut buf = [0u8; 2048];
        while alive() {
            let Ok((n, from)) = fake.recv_from(&mut buf) else { continue };
            let reply = match decode(&buf[..n]) {
                Some(Packet::Hello { .. }) => Packet::HelloAck { name: "fake", caps: 0 },
                Some(Packet::Ping { t_us, .. }) => Packet::Pong { t_us },
                _ => continue,
            };
            fake.send_to(&reply.to_vec(), from).unwrap();
        }
    };

    let (dialer, events) = engine(config("dialer"));
    dialer.connect(fake.local_addr().unwrap());
    let state = |want_connected: bool| {
        events.try_iter().any(|e| match e {
            Event::State(State::Connected { .. }) => want_connected,
            Event::State(State::Connecting { .. }) => !want_connected,
            Event::State(State::Idle) => panic!("dialer gave up"),
            _ => false,
        })
    };
    serve(&|| !state(true));
    // Silence. The dialer should notice within its timeout and start over...
    let silent = Instant::now();
    while !state(false) {
        assert!(silent.elapsed() < Duration::from_secs(6), "never noticed the peer was gone");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(silent.elapsed() > Duration::from_secs(2), "gave up on a live session too eagerly");
    // ...and get back in as soon as the peer answers again.
    let back = Instant::now();
    serve(&|| {
        assert!(back.elapsed() < Duration::from_secs(3), "did not reconnect");
        !state(true)
    });
}

/// A microphone that can be made to fail to open, and to die once open.
#[derive(Default)]
struct FlakyMic {
    broken: std::sync::atomic::AtomicBool,
    opens: std::sync::atomic::AtomicU32,
    /// Sample rate the device insists on, whatever is asked; 0 is 48 kHz.
    rate: std::sync::atomic::AtomicU32,
    /// Liveness flag of the most recently opened stream.
    current: std::sync::Mutex<Option<Arc<std::sync::atomic::AtomicBool>>>,
}

struct FlakyStream(Arc<std::sync::atomic::AtomicBool>);

impl StreamHandle for FlakyStream {
    fn is_alive(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl AudioBackend for FlakyMic {
    fn devices(&self) -> Vec<DeviceInfo> {
        Vec::new()
    }
    fn open_capture(&self, _: &CaptureRequest, make: MakeCapture) -> Result<Box<dyn StreamHandle>, String> {
        use std::sync::atomic::Ordering::Relaxed;
        if self.broken.load(Relaxed) {
            return Err("unplugged".into());
        }
        self.opens.fetch_add(1, Relaxed);
        // Never called: a silent microphone is enough here.
        let sample_rate = match self.rate.load(Relaxed) {
            0 => 48_000,
            rate => rate,
        };
        let _callback = make(soundbridge_core::StreamFormat { sample_rate, channels: 1 });
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        *self.current.lock().unwrap() = Some(alive.clone());
        Ok(Box::new(FlakyStream(alive)))
    }
    fn open_playback(&self, _: &PlaybackRequest, _: MakeRender) -> Result<Box<dyn StreamHandle>, String> {
        Err("no output in this test".into())
    }
}

#[test]
fn a_failed_or_dead_source_is_retried_quietly() {
    use std::sync::atomic::Ordering::Relaxed;

    let mic = Arc::new(FlakyMic::default());
    mic.broken.store(true, Relaxed);
    let (listener, _listener_events) = engine(config("listener"));
    let (tx, events) = mpsc::channel();
    let dialer = Engine::start(
        Config {
            send: vec![SourceConfig::new(SourceKind::Mic)],
            ..config("dialer")
        },
        mic.clone(),
        move |e| {
            let _ = tx.send(e);
        },
    )
    .unwrap();
    dialer.connect(local(&listener));

    // The session comes up without the source, and says why...
    let error = wait_for(&events, "the open failure", |e| match e {
        Event::Error(e) => Some(e),
        _ => None,
    });
    assert!(error.contains("microphone") && error.contains("unplugged"), "{error}");
    // ...once, however many times it retries behind the scenes.
    std::thread::sleep(Duration::from_millis(4500));
    assert!(!events.try_iter().any(|e| matches!(e, Event::Error(_))), "the same error was repeated");

    // Plugged back in: picked up on the next retry.
    mic.broken.store(false, Relaxed);
    wait_for(&events, "sending to start", |e| match e {
        Event::Stats(Stats { tx, .. }) if !tx.is_empty() => Some(()),
        _ => None,
    });
    assert_eq!(mic.opens.load(Relaxed), 1);

    // The device dies under the running stream: reopened without being asked.
    mic.current.lock().unwrap().as_ref().unwrap().store(false, Relaxed);
    let deadline = Instant::now() + Duration::from_secs(3);
    while mic.opens.load(Relaxed) < 2 {
        assert!(Instant::now() < deadline, "dead stream was not reopened");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_microphone_at_the_wrong_rate_is_sent_unfiltered_and_says_so() {
    let mic = Arc::new(FlakyMic::default());
    mic.rate.store(44_100, std::sync::atomic::Ordering::Relaxed);
    let (listener, _listener_events) = engine(config("listener"));
    let (tx, events) = mpsc::channel();
    let dialer = Engine::start(
        Config {
            send: vec![SourceConfig { denoise: denoise::Mode::VOICE, ..SourceConfig::new(SourceKind::Mic) }],
            ..config("dialer")
        },
        mic,
        move |e| {
            let _ = tx.send(e);
        },
    )
    .unwrap();
    dialer.connect(local(&listener));
    let text = wait_for(&events, "the explanation", |e| match e {
        Event::Info(text) => Some(text),
        _ => None,
    });
    assert!(text.contains("Noise suppression") && text.contains("44.1 kHz"), "{text}");
    let tx = wait_for(&events, "sending to start", |e| match e {
        Event::Stats(Stats { tx, .. }) => tx.first().copied(),
        _ => None,
    });
    assert_eq!(tx.denoise, denoise::Mode::Off);
}
