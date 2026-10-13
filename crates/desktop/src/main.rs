//! SoundBridge for the desktop: pick what this PC sends and where received
//! audio goes, then connect to a device found on the network (or let one
//! connect here).

// A release build on Windows should not open a console window behind the app.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod settings;
mod text_input;
mod theme;

use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use gpui::{
    App, Application, Bounds, ClickEvent, Context, CursorStyle, DispatchPhase, Div, ElementId, Entity,
    Focusable, HitboxBehavior, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, SharedString,
    Stateful, Subscription, TitlebarOptions, Window, WindowBounds, WindowOptions, canvas, div,
    prelude::*, px, relative, rgb, size,
};
use soundbridge_core::cpal_backend::CpalBackend;
use soundbridge_core::denoise::{Gate, Mode};
use soundbridge_core::gain::OFFSET_RANGE_DB;
use soundbridge_core::{
    AudioBackend, BufferMode, Config, DEFAULT_PORT, DeviceDirection, DeviceInfo, Engine, Event,
    Peer, SinkConfig, SinkKind, SourceConfig, SourceKind, SourceSettings, State, Stats, Streams,
};
use soundbridge_core::protocol::MAX_STREAMS;

use settings::{Kind, Play, Route, Settings, Source, StreamRoute};
use text_input::{Cancelled, Submitted, TextInput};

/// What a microphone can be filtered down to. A tuning that is none of
/// these leaves all of them unlit.
const DENOISE_CHOICES: [(Mode, &str); 4] = [
    (Mode::Off, "Everything"),
    (Mode::Reduce, "Less noise"),
    (Mode::VOICE, "Voice only"),
    (Mode::WHOLE_WORDS, "Whole words"),
];
const BUFFER_CHOICES: [(bool, &str); 2] = [(false, "Automatic"), (true, "Fixed")];
/// The fixed jitter buffer to start from, in ms.
const BUFFER_MS: u32 = 20;
/// The fixed jitter buffers that can be typed in, in ms.
const BUFFER_RANGE_MS: std::ops::RangeInclusive<u32> = 1..=500;
/// How far a slider's thumb reaches past each end of its track.
const THUMB_REACH: f32 = 7.;
#[cfg(feature = "self-update")]
const UPDATE_CHOICES: [(bool, &str); 2] = [(true, "Automatic"), (false, "Off")];

/// The device list that is open, if any: for one of the sources, or for
/// where one received stream is played. That stream is one of those listed
/// in the settings, by its place there, or one the other device sends that
/// is not listed yet, by its slot.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Picker {
    Source(usize),
    Stream(usize),
    Unlisted(u8),
}

/// Whose microphone a set of controls is for: one this PC sends, by its place
/// in the list, or one the other device sends, by its slot there.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mic {
    Local(usize),
    Remote(u8),
}

/// What those controls set.
#[derive(Clone, Copy, PartialEq)]
struct Tuning {
    denoise: Mode,
    auto_gain: bool,
    gain_db: i32,
}

/// A number set with a slider, or typed in exactly where the slider is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Exact {
    Gain(Mic),
    Lookahead(Mic),
    Hold(Mic),
    Threshold(Mic),
    Buffer,
}

impl Exact {
    /// What follows the number where it is shown.
    fn unit(self) -> &'static str {
        match self {
            Exact::Gain(_) => " dB",
            Exact::Lookahead(_) | Exact::Hold(_) | Exact::Buffer => " ms",
            Exact::Threshold(_) => "",
        }
    }

    /// How the number is shown beside its slider.
    fn label(self, value: f32) -> String {
        match self {
            Exact::Gain(_) if value != 0.0 => format!("{value:+} dB"),
            _ => format!("{value}{}", self.unit()),
        }
    }

    /// What the slider covers, and in what steps. Less usual values are typed.
    fn slider(self) -> (std::ops::RangeInclusive<f32>, f32) {
        match self {
            Exact::Gain(_) => (-24.0..=24.0, 1.0),
            Exact::Lookahead(_) => (0.0..=Gate::MAX_LOOKAHEAD_MS as f32, 10.0),
            Exact::Hold(_) => (50.0..=1000.0, 50.0),
            Exact::Threshold(_) => (0.1..=0.9, 0.05),
            Exact::Buffer => (1.0..=100.0, 1.0),
        }
    }

    /// What may be typed.
    fn hint(self) -> String {
        match self {
            Exact::Gain(_) => format!("In dB, from {} to {}.", OFFSET_RANGE_DB.start(), OFFSET_RANGE_DB.end()),
            Exact::Lookahead(_) => format!("In ms, up to {}, in steps of 10.", Gate::MAX_LOOKAHEAD_MS),
            Exact::Hold(_) => format!("In ms, from 10 to {}.", Gate::MAX_HOLD_MS),
            Exact::Threshold(_) => {
                format!("From {} to {}; lower is more lenient.", Gate::THRESHOLDS.start(), Gate::THRESHOLDS.end())
            }
            Exact::Buffer => format!("In ms, from {} to {}.", BUFFER_RANGE_MS.start(), BUFFER_RANGE_MS.end()),
        }
    }
}

/// The field one of those is being typed into.
struct ExactInput {
    what: Exact,
    input: Entity<TextInput>,
    /// Set once what was typed turned out not to be a number.
    invalid: bool,
    _subscriptions: [Subscription; 2],
}

/// How far a look for a new release, asked for by hand, has got.
#[cfg(feature = "self-update")]
enum UpdateCheck {
    Idle,
    Checking,
    /// Nothing newer has been released.
    Current,
    Available(soundbridge_update::Version),
    Installing(soundbridge_update::Version),
    Failed(SharedString),
}

struct Notice {
    text: SharedString,
    error: bool,
}

struct Bridge {
    backend: Arc<CpalBackend>,
    /// `None` only if no socket could be opened at all.
    engine: Option<Engine>,
    /// False when the usual port was taken and we fell back to another.
    discoverable: bool,
    settings: Settings,
    state: State,
    peers: Vec<Peer>,
    stats: Stats,
    notice: Option<Notice>,
    devices: Vec<DeviceInfo>,
    open_picker: Option<Picker>,
    /// The number being typed in, if any.
    exact: Option<ExactInput>,
    /// The slider being dragged and where it is, which is set on letting go.
    drag: Option<(Exact, f32)>,
    host: Entity<TextInput>,
    /// False for a copy that something else keeps current.
    #[cfg(feature = "self-update")]
    self_updating: bool,
    /// Whether new releases install themselves.
    #[cfg(feature = "self-update")]
    auto_update: bool,
    /// The release installed while the app was running, which it is not yet.
    #[cfg(feature = "self-update")]
    updated: Option<soundbridge_update::Version>,
    #[cfg(feature = "self-update")]
    update_check: UpdateCheck,
}

impl Bridge {
    fn new(cx: &mut Context<Self>) -> Self {
        let settings = Settings::load();
        let backend = Arc::new(CpalBackend::new());
        let host = cx.new(|cx| {
            let mut input = TextInput::new(cx, "Address, e.g. 192.168.1.20");
            input.set_content(settings.host.clone(), cx);
            input
        });
        cx.subscribe(&host, |this, _, _: &Submitted, cx| this.connect_typed(cx)).detach();

        // The engine reports from its own thread; this carries events to ours.
        let (tx, rx) = async_channel::unbounded();
        let started = Engine::start(config(&settings), backend.clone(), move |event| {
            let _ = tx.send_blocking(event);
        });
        cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                if this.update(cx, |this, cx| this.on_event(event, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        #[cfg(feature = "self-update")]
        {
            let (tx, rx) = async_channel::bounded(1);
            soundbridge_update::auto(move |version| {
                let _ = tx.send_blocking(version);
            });
            cx.spawn(async move |this, cx| {
                if let Ok(version) = rx.recv().await {
                    let _ = this.update(cx, |this, cx| {
                        this.updated = Some(version);
                        cx.notify();
                    });
                }
            })
            .detach();
        }
        // Hang up properly on quit so the peer is not left to time out.
        cx.on_app_quit(|this, _| {
            this.engine = None;
            async {}
        })
        .detach();

        let (engine, notice) = match started {
            Ok(engine) => (Some(engine), None),
            Err(e) => {
                let text = format!("Could not open a network socket: {e}").into();
                (None, Some(Notice { text, error: true }))
            }
        };
        let mut this = Bridge {
            backend,
            discoverable: engine.as_ref().is_some_and(|e| e.local_port() == DEFAULT_PORT),
            engine,
            settings,
            state: State::Idle,
            peers: Vec::new(),
            stats: Stats::default(),
            notice,
            devices: Vec::new(),
            open_picker: None,
            exact: None,
            drag: None,
            host,
            #[cfg(feature = "self-update")]
            self_updating: soundbridge_update::available(),
            #[cfg(feature = "self-update")]
            auto_update: soundbridge_update::auto_enabled(),
            #[cfg(feature = "self-update")]
            updated: None,
            #[cfg(feature = "self-update")]
            update_check: UpdateCheck::Idle,
        };
        this.refresh_devices(cx);
        this
    }

    fn on_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::State(state) => {
                if !matches!(state, State::Connected { .. }) {
                    self.stats = Stats::default();
                }
                if matches!(state, State::Connecting { .. }) {
                    self.notice = None;
                }
                // The other device's settings go with the connection.
                let remote = |exact: &ExactInput| {
                    use Exact::*;
                    matches!(exact.what, Gain(Mic::Remote(_)) | Lookahead(Mic::Remote(_)) | Hold(Mic::Remote(_)) | Threshold(Mic::Remote(_)))
                };
                if !matches!(state, State::Connected { .. }) && self.exact.as_ref().is_some_and(remote) {
                    self.exact = None;
                }
                self.state = state;
            }
            Event::Stats(stats) => self.stats = stats,
            Event::Peers(peers) => self.peers = peers,
            Event::SourceChanged(changed) => {
                let source = self.settings.sources.get_mut(changed.slot as usize);
                let Some(source) = source.filter(|source| wire_kind(source.kind) == changed.kind) else { return };
                source.denoise = changed.denoise;
                source.auto_gain = changed.auto_gain;
                source.gain_db = changed.gain_db.round() as i32;
                let from = match &self.state {
                    State::Connected { name, .. } => name.as_str(),
                    _ => "the other device",
                };
                let text = format!("Stream {} was set from {from}", changed.slot + 1).into();
                self.notice = Some(Notice { text, error: false });
                // Saved, and handed back to the engine in case the gain was rounded.
                self.settings_changed(cx);
            }
            Event::Info(text) => self.notice = Some(Notice { text: text.into(), error: false }),
            Event::Error(text) => self.notice = Some(Notice { text: text.into(), error: true }),
        }
        cx.notify();
    }

    /// Enumerating devices talks to the audio server, so it stays off the UI thread.
    fn refresh_devices(&mut self, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let devices = cx.background_executor().spawn(async move { backend.devices() }).await;
            let _ = this.update(cx, |this, cx| {
                this.devices = devices;
                cx.notify();
            });
        })
        .detach();
    }

    fn settings_changed(&mut self, cx: &mut Context<Self>) {
        self.settings.save();
        if let Some(engine) = &self.engine {
            engine.configure(config(&self.settings));
        }
        cx.notify();
    }

    /// Changes how a microphone is filtered or levelled: in the settings for
    /// one of ours, by asking the other device for one of its own.
    fn tune(&mut self, mic: Mic, change: impl FnOnce(&mut Tuning), cx: &mut Context<Self>) {
        self.open_picker = None;
        match mic {
            Mic::Local(index) => {
                if let Some(source) = self.settings.sources.get_mut(index) {
                    let mut tuning =
                        Tuning { denoise: source.denoise, auto_gain: source.auto_gain, gain_db: source.gain_db };
                    change(&mut tuning);
                    (source.denoise, source.auto_gain, source.gain_db) =
                        (tuning.denoise, tuning.auto_gain, tuning.gain_db);
                }
                self.settings_changed(cx);
            }
            Mic::Remote(slot) => {
                // Changed in what is shown as well, which the engine's next
                // report confirms or takes back.
                let Some(source) = self.stats.remote_sources.iter_mut().find(|s| s.slot == slot) else { return };
                let before = remote_tuning(source);
                let mut tuning = before;
                change(&mut tuning);
                (source.denoise, source.auto_gain) = (tuning.denoise, tuning.auto_gain);
                // A gain between two whole numbers stays unless it is the gain that changed.
                if tuning.gain_db != before.gain_db {
                    source.gain_db = tuning.gain_db as f32;
                }
                if let Some(engine) = &self.engine {
                    engine.set_remote(*source);
                }
                cx.notify();
            }
        }
    }

    /// How a microphone is filtered and levelled now.
    fn tuning(&self, mic: Mic) -> Option<Tuning> {
        match mic {
            Mic::Local(index) => self.settings.sources.get(index).map(|source| Tuning {
                denoise: source.denoise,
                auto_gain: source.auto_gain,
                gain_db: source.gain_db,
            }),
            Mic::Remote(slot) => self.stats.remote_sources.iter().find(|s| s.slot == slot).map(remote_tuning),
        }
    }

    /// What one of the numbers is now, if it is set to anything.
    fn exact_value(&self, what: Exact) -> Option<f32> {
        let gate = |mic| match self.tuning(mic)?.denoise {
            Mode::Gate(gate) => Some(gate),
            _ => None,
        };
        match what {
            Exact::Gain(mic) => Some(self.tuning(mic)?.gain_db as f32),
            Exact::Lookahead(mic) => Some(gate(mic)?.lookahead_ms as f32),
            Exact::Hold(mic) => Some(gate(mic)?.hold_ms as f32),
            Exact::Threshold(mic) => Some(gate(mic)?.threshold),
            Exact::Buffer => self.settings.buffer_ms.map(|ms| ms as f32),
        }
    }

    /// Puts a field where a number's choices are, holding what it is now.
    fn open_exact(&mut self, what: Exact, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.exact_value(what).map(|value| value.to_string()).unwrap_or_default();
        let input = cx.new(|cx| {
            let mut input = TextInput::new(cx, "");
            input.set_content(value, cx);
            input.select_all_content(cx);
            input
        });
        window.focus(&input.focus_handle(cx));
        let _subscriptions = [
            cx.subscribe(&input, |this, _, _: &Submitted, cx| this.apply_exact(cx)),
            cx.subscribe(&input, |this, _, _: &Cancelled, cx| this.close_exact(cx)),
        ];
        self.open_picker = None;
        self.exact = Some(ExactInput { what, input, invalid: false, _subscriptions });
        cx.notify();
    }

    fn close_exact(&mut self, cx: &mut Context<Self>) {
        self.exact = None;
        cx.notify();
    }

    /// Sets the number to what was typed.
    fn apply_exact(&mut self, cx: &mut Context<Self>) {
        let Some(exact) = &mut self.exact else { return };
        let Some(value) = parse_number(&exact.input.read(cx).content()) else {
            exact.invalid = true;
            cx.notify();
            return;
        };
        let what = exact.what;
        self.exact = None;
        self.set_number(what, value, cx);
    }

    /// Moves the slider being dragged to a place along its track, from 0 to 1.
    fn drag(&mut self, what: Exact, along: f32, cx: &mut Context<Self>) {
        let (range, step) = what.slider();
        let value = range.start() + along.clamp(0.0, 1.0) * (range.end() - range.start());
        // Whole steps, without what adding them up leaves behind.
        let value = ((value / step).round() * step * 100.0).round() / 100.0;
        self.drag = Some((what, value));
        cx.notify();
    }

    fn end_drag(&mut self, cx: &mut Context<Self>) {
        if let Some((what, value)) = self.drag.take()
            && self.exact_value(what) != Some(value)
        {
            self.set_number(what, value, cx);
        }
        cx.notify();
    }

    /// Sets a number, brought into what it can be.
    fn set_number(&mut self, what: Exact, value: f32, cx: &mut Context<Self>) {
        let gate = |set: fn(&mut Gate, f32)| {
            move |tuning: &mut Tuning| {
                if let Mode::Gate(gate) = &mut tuning.denoise {
                    set(gate, value);
                }
                tuning.denoise = tuning.denoise.clamped();
            }
        };
        match what {
            Exact::Gain(mic) => {
                let db = value.clamp(*OFFSET_RANGE_DB.start(), *OFFSET_RANGE_DB.end()).round() as i32;
                self.tune(mic, |tuning| tuning.gain_db = db, cx);
            }
            Exact::Lookahead(mic) => self.tune(mic, gate(|gate, ms| gate.lookahead_ms = ms.round() as u32), cx),
            Exact::Hold(mic) => self.tune(mic, gate(|gate, ms| gate.hold_ms = ms.round() as u32), cx),
            Exact::Threshold(mic) => self.tune(mic, gate(|gate, level| gate.threshold = level), cx),
            Exact::Buffer => {
                let ms = value.round() as u32;
                self.settings.buffer_ms = Some(ms.clamp(*BUFFER_RANGE_MS.start(), *BUFFER_RANGE_MS.end()));
                self.settings_changed(cx);
            }
        }
    }

    fn connect(&mut self, addr: SocketAddr, cx: &mut Context<Self>) {
        self.open_picker = None;
        if let Some(engine) = &self.engine {
            engine.connect(addr);
        }
        cx.notify();
    }

    fn connect_typed(&mut self, cx: &mut Context<Self>) {
        let typed = self.host.read(cx).content().trim().to_string();
        if typed.is_empty() {
            return;
        }
        match resolve(&typed) {
            Ok(addr) => {
                self.settings.host = typed;
                self.settings.save();
                self.connect(addr, cx);
            }
            Err(text) => {
                self.notice = Some(Notice { text: text.into(), error: true });
                cx.notify();
            }
        }
    }

    fn disconnect(&mut self, cx: &mut Context<Self>) {
        if let Some(engine) = &self.engine {
            engine.disconnect();
        }
        cx.notify();
    }

    fn toggle_picker(&mut self, picker: Picker, cx: &mut Context<Self>) {
        self.open_picker = if self.open_picker == Some(picker) { None } else { Some(picker) };
        if self.open_picker.is_some() {
            self.refresh_devices(cx);
        }
        cx.notify();
    }

    /// The device setting a picker edits.
    fn picked_device(&mut self, picker: Picker) -> Option<&mut Option<String>> {
        match picker {
            Picker::Source(index) => self.settings.sources.get_mut(index).map(|s| &mut s.device),
            Picker::Stream(index) => self.settings.play_streams.get_mut(index).map(|s| &mut s.route.device),
            // Choosing a device for it is what lists it.
            Picker::Unlisted(slot) => {
                self.settings.play_streams.push(StreamRoute { slot, route: Route::default() });
                self.settings.play_streams.last_mut().map(|s| &mut s.route.device)
            }
        }
    }

    /// Looks for a newer release without installing it.
    #[cfg(feature = "self-update")]
    fn check_for_update(&mut self, cx: &mut Context<Self>) {
        self.update_check = UpdateCheck::Checking;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let found = cx.background_executor().spawn(async { soundbridge_update::check() }).await;
            let _ = this.update(cx, |this, cx| {
                this.update_check = match found {
                    Ok(Some(version)) => UpdateCheck::Available(version),
                    Ok(None) => UpdateCheck::Current,
                    Err(e) => UpdateCheck::Failed(format!("Could not look for a new release: {e}.").into()),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// Installs the release a check found.
    #[cfg(feature = "self-update")]
    fn install_update(&mut self, version: soundbridge_update::Version, cx: &mut Context<Self>) {
        use soundbridge_update::Outcome;
        self.update_check = UpdateCheck::Installing(version);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let outcome = cx.background_executor().spawn(async { soundbridge_update::update() }).await;
            let _ = this.update(cx, |this, cx| {
                this.update_check = match outcome {
                    Ok(Outcome::Updated(version)) => {
                        this.updated = Some(version);
                        UpdateCheck::Idle
                    }
                    Ok(Outcome::Current(_)) => UpdateCheck::Current,
                    Err(e) => UpdateCheck::Failed(format!("Could not install {version}: {e}.").into()),
                };
                cx.notify();
            });
        })
        .detach();
    }

    fn device_name(&self, id: &Option<String>, direction: DeviceDirection) -> SharedString {
        match id {
            None => "System default".into(),
            Some(id) => self
                .devices
                .iter()
                .find(|d| d.id == *id && d.direction == direction)
                .map(|d| d.name.clone())
                // Unplugged, or the list has not loaded yet.
                .unwrap_or_else(|| id.clone())
                .into(),
        }
    }
}

fn config(settings: &Settings) -> Config {
    Config {
        name: soundbridge_core::default_name(),
        port: DEFAULT_PORT,
        // A second instance (or the CLI) may hold the port; still be usable.
        port_fallback: true,
        listen: true,
        discover: true,
        send: (settings.sources.iter())
            .map(|source| SourceConfig {
                kind: wire_kind(source.kind),
                device: source.device.clone(),
                channels: None,
                denoise: if source.kind == Kind::Mic { source.denoise } else { Mode::Off },
                auto_gain: source.auto_gain && source.kind == Kind::Mic,
                gain_db: if source.kind == Kind::Mic { source.gain_db as f32 } else { 0.0 },
            })
            .collect(),
        play: sinks(settings),
        buffer: match settings.buffer_ms {
            None => BufferMode::default(),
            Some(ms) => BufferMode::Fixed { ms: ms as f32 },
        },
        ..Config::default()
    }
}

fn remote_tuning(source: &SourceSettings) -> Tuning {
    Tuning { denoise: source.denoise, auto_gain: source.auto_gain, gain_db: source.gain_db.round() as i32 }
}

fn wire_kind(kind: Kind) -> SourceKind {
    match kind {
        Kind::Mic => SourceKind::Mic,
        Kind::System => SourceKind::System,
    }
}

fn sinks(settings: &Settings) -> Vec<SinkConfig> {
    let sink = |route: &Route, from| {
        Some(match route.play {
            Play::Off => return None,
            Play::Speaker => SinkConfig { kind: SinkKind::Speaker, device: route.device.clone(), from },
            Play::VirtualMic => SinkConfig { kind: SinkKind::VirtualMic, device: None, from },
        })
    };
    // A stream that is listed, even to play nowhere, is kept from where
    // the ones that are not listed go.
    let streams = settings.play_streams.iter().map(|stream| {
        let from = Streams::Slot(stream.slot);
        sink(&stream.route, from).unwrap_or(SinkConfig { from, ..SinkConfig::new(SinkKind::Nothing) })
    });
    streams.chain(sink(&Route::default(), Streams::All)).collect()
}

fn kind_name(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Mic => "microphone",
        SourceKind::System => "device audio",
        SourceKind::Tone => "test tone",
    }
}

/// A number as it gets typed: with a decimal comma, or with its unit after it.
fn parse_number(text: &str) -> Option<f32> {
    let number = text.trim().trim_end_matches(|c: char| c.is_alphabetic() || c.is_whitespace());
    number.replace(',', ".").replace('\u{2212}', "-").parse().ok().filter(|number: &f32| number.is_finite())
}

fn resolve(peer: &str) -> Result<SocketAddr, String> {
    let with_port = if peer.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) {
        peer.to_string()
    } else {
        format!("{peer}:{DEFAULT_PORT}")
    };
    with_port
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.find(SocketAddr::is_ipv4))
        .ok_or_else(|| format!("Cannot find \"{peer}\" on the network"))
}

// ---- view ----------------------------------------------------------------

fn section(title: &'static str) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .rounded_lg()
        .bg(rgb(theme::PANEL))
        .child(div().text_size(px(11.)).text_color(rgb(theme::MUTED)).child(title))
}

fn button(id: impl Into<ElementId>, label: impl Into<SharedString>, primary: bool) -> Stateful<Div> {
    let (bg, hover) = if primary {
        (theme::ACCENT, theme::ACCENT_HOVER)
    } else {
        (theme::FIELD, theme::FIELD_HOVER)
    };
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .h(px(32.))
        .px_4()
        .rounded_md()
        .bg(rgb(bg))
        .hover(|s| s.bg(rgb(hover)))
        .cursor_pointer()
        .text_size(px(13.))
        .child(label.into())
}

/// A level bar covering the top 48 dB.
fn meter(level: f32) -> Div {
    let db = 20.0 * level.max(1e-6).log10();
    let fraction = ((db + 48.0) / 48.0).clamp(0.0, 1.0);
    let colour = if db > -1.0 { theme::BAD } else { theme::GOOD };
    div()
        .h(px(6.))
        .w_full()
        .rounded_full()
        .bg(rgb(theme::FIELD))
        .child(div().h_full().w(relative(fraction)).rounded_full().bg(rgb(colour)))
}

/// A row of mutually exclusive choices; `pick` is told which one was clicked.
fn segmented<T: Copy + PartialEq + 'static>(
    id: impl Into<ElementId>,
    options: &[(T, &'static str)],
    current: T,
    pick: impl Fn(&T, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let pick = std::rc::Rc::new(pick);
    div().id(id).flex().gap_1().p_1().rounded_md().bg(rgb(theme::FIELD)).children(
        options.iter().enumerate().map(|(i, &(value, label))| {
            let selected = value == current;
            let pick = pick.clone();
            div()
                .id(i)
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .h(px(28.))
                .rounded_sm()
                .text_size(px(13.))
                .cursor_pointer()
                .when(selected, |d| d.bg(rgb(theme::ACCENT)))
                .when(!selected, |d| d.text_color(rgb(theme::MUTED)).hover(|s| s.bg(rgb(theme::FIELD_HOVER))))
                .child(label)
                .on_click(move |_, window, cx| pick(&value, window, cx))
        }),
    )
}

fn detail(text: impl Into<SharedString>) -> Div {
    div().text_size(px(12.)).text_color(rgb(theme::MUTED)).child(text.into())
}

impl Bridge {
    /// A slider for a number, which a right click turns into a field for
    /// typing any value, one the slider does not reach included.
    fn number_row(&self, id: impl Into<ElementId>, what: Exact, cx: &mut Context<Self>) -> Div {
        if let Some(exact) = self.exact.as_ref().filter(|exact| exact.what == what) {
            let hint = format!("{} Enter sets it, Escape leaves it as it was.", what.hint());
            return div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(exact.input.clone())
                        .child(
                            button("exact-set", "Set", true)
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.apply_exact(cx))),
                        )
                        .child(
                            button("exact-cancel", "Cancel", false)
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.close_exact(cx))),
                        ),
                )
                .child(match exact.invalid {
                    true => detail(format!("That is not a number. {hint}")).text_color(rgb(theme::BAD)),
                    false => detail(hint),
                });
        }
        let dragged = self.drag.filter(|&(dragged, _)| dragged == what).map(|(_, value)| value);
        let Some(value) = dragged.or_else(|| self.exact_value(what)) else { return div() };
        let (range, _) = what.slider();
        // A typed value past either end leaves the thumb at that end.
        let along = ((value - range.start()) / (range.end() - range.start())).clamp(0.0, 1.0);

        // Painted over the whole slider to follow the mouse, off the track too
        // while it is dragged.
        let bridge = cx.entity();
        let dragging = dragged.is_some();
        let mouse = canvas(
            |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
            move |bounds, hitbox, window, _| {
                window.set_cursor_style(CursorStyle::PointingHand, &hitbox);
                let along = move |event_x| {
                    let track = f32::from(bounds.size.width) - 2.0 * THUMB_REACH;
                    (f32::from(event_x - bounds.left()) - THUMB_REACH) / track.max(1.0)
                };
                let pressed = bridge.clone();
                window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                        return;
                    }
                    match event.button {
                        MouseButton::Left => pressed.update(cx, |this, cx| this.drag(what, along(event.position.x), cx)),
                        MouseButton::Right => pressed.update(cx, |this, cx| this.open_exact(what, window, cx)),
                        _ => {}
                    }
                });
                if !dragging {
                    return;
                }
                let moved = bridge.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }
                    moved.update(cx, |this, cx| match event.pressed_button {
                        Some(MouseButton::Left) => this.drag(what, along(event.position.x), cx),
                        // Let go where we were not told, outside the window.
                        _ => this.end_drag(cx),
                    });
                });
                window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble && event.button == MouseButton::Left {
                        bridge.update(cx, |this, cx| this.end_drag(cx));
                    }
                });
            },
        )
        .absolute()
        .top_0()
        .bottom_0()
        .left(px(-THUMB_REACH))
        .right(px(-THUMB_REACH));

        let thumb = 2.0 * THUMB_REACH;
        let track = div()
            .id(id)
            .relative()
            .flex_1()
            .flex()
            .items_center()
            .h(px(28.))
            .mx(px(THUMB_REACH))
            .child(
                div()
                    .w_full()
                    .h(px(4.))
                    .rounded_full()
                    .bg(rgb(theme::FIELD_HOVER))
                    .child(div().h_full().w(relative(along)).rounded_full().bg(rgb(theme::ACCENT))),
            )
            .child(
                div()
                    .absolute()
                    .top(px((28. - thumb) / 2.))
                    .left(relative(along))
                    .ml(px(-THUMB_REACH))
                    .size(px(thumb))
                    .rounded_full()
                    .bg(rgb(if dragging { theme::ACCENT_HOVER } else { theme::TEXT })),
            )
            .child(mouse);
        div().flex().items_center().gap_2().child(track).child(
            div().flex_none().w(px(60.)).flex().justify_end().text_size(px(13.)).child(what.label(value)),
        )
    }

    /// The current device, expanding in place into the list of alternatives.
    fn device_picker(
        &self,
        picker: Picker,
        current: &Option<String>,
        direction: DeviceDirection,
        cx: &mut Context<Self>,
    ) -> Div {
        let open = self.open_picker == Some(picker);
        let id: ElementId = match picker {
            Picker::Source(index) => ("source-device", index).into(),
            Picker::Stream(index) => ("stream-sink-device", index).into(),
            Picker::Unlisted(slot) => ("unlisted-sink-device", slot as usize).into(),
        };
        let header = div()
            .id(id)
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .h(px(32.))
            .px_2()
            .rounded_md()
            .border_1()
            .border_color(rgb(theme::BORDER))
            .hover(|s| s.bg(rgb(theme::FIELD)))
            .cursor_pointer()
            .text_size(px(13.))
            .child(div().truncate().child(self.device_name(current, direction)))
            .child(div().text_color(rgb(theme::MUTED)).child(if open { "Close" } else { "Change" }))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle_picker(picker, cx)));

        let mut column = div().flex().flex_col().gap_1().child(header);
        if open {
            let choices = std::iter::once((None, SharedString::from("System default"))).chain(
                self.devices
                    .iter()
                    .filter(|d| d.direction == direction)
                    .map(|d| (Some(d.id.clone()), SharedString::from(d.name.clone()))),
            );
            for (i, (choice, name)) in choices.enumerate() {
                let selected = choice == *current;
                column = column.child(
                    div()
                        // Only one list is ever open, so the index is enough.
                        .id(("device-choice", i))
                        .flex()
                        .items_center()
                        .h(px(28.))
                        .px_2()
                        .rounded_sm()
                        .text_size(px(13.))
                        .cursor_pointer()
                        .when(selected, |d| d.text_color(rgb(theme::ACCENT)))
                        .hover(|s| s.bg(rgb(theme::FIELD)))
                        .child(div().truncate().child(name))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            if let Some(slot) = this.picked_device(picker) {
                                *slot = choice.clone();
                            }
                            this.open_picker = None;
                            this.settings_changed(cx);
                        })),
                );
            }
        }
        column
    }

    fn status_line(&self) -> Div {
        let (colour, text): (u32, SharedString) = match &self.state {
            State::Connected { name, .. } => (theme::GOOD, format!("Connected to {name}").into()),
            State::Connecting { addr } => (theme::WARN, format!("Connecting to {addr}...").into()),
            State::Idle if self.engine.is_none() => (theme::BAD, "Offline".into()),
            State::Idle if self.discoverable => (
                theme::ACCENT,
                format!("Ready. Others see this PC as \"{}\"", soundbridge_core::default_name()).into(),
            ),
            State::Idle => (
                theme::WARN,
                format!("Port {DEFAULT_PORT} is in use: you can connect out, but others cannot find this PC")
                    .into(),
            ),
        };
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().flex_none().size(px(8.)).rounded_full().bg(rgb(colour)))
            .child(div().text_size(px(13.)).child(text))
    }

    fn send_section(&self, cx: &mut Context<Self>) -> Div {
        let mut section = section("SEND FROM THIS PC");
        if self.settings.sources.is_empty() {
            section = section.child(detail("Nothing. Add a microphone, or what this PC is playing."));
        }
        for (index, source) in self.settings.sources.iter().enumerate() {
            let (name, direction) = match source.kind {
                Kind::Mic => ("Microphone", DeviceDirection::Input),
                Kind::System => ("PC audio", DeviceDirection::Output),
            };
            let remove = div()
                .id(("remove-source", index))
                .text_size(px(12.))
                .text_color(rgb(theme::MUTED))
                .hover(|s| s.text_color(rgb(theme::TEXT)))
                .cursor_pointer()
                .child("Remove")
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    if index < this.settings.sources.len() {
                        this.settings.sources.remove(index);
                    }
                    this.open_picker = None;
                    // The ones after it are numbered anew.
                    (this.exact, this.drag) = (None, None);
                    this.settings_changed(cx);
                }));
            let mut block = div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(div().text_size(px(13.)).child(format!("{name} · stream {}", index + 1)))
                        .child(remove),
                )
                .child(self.device_picker(Picker::Source(index), &source.device, direction, cx));
            block = match source.kind {
                Kind::Mic => {
                    let tuning =
                        Tuning { denoise: source.denoise, auto_gain: source.auto_gain, gain_db: source.gain_db };
                    self.mic_controls(block, Mic::Local(index), tuning, cx)
                }
                Kind::System => block.child(detail("Sends whatever is playing on that output.")),
            };
            section = section.child(block);
        }
        let add = |id: &'static str, label: &'static str, kind, cx: &mut Context<Self>| {
            button(id, label, false).on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.settings.sources.push(Source { kind, ..Source::default() });
                this.open_picker = None;
                this.settings_changed(cx);
            }))
        };
        section.child(
            div()
                .flex()
                .gap_2()
                .child(add("add-mic", "Add microphone", Kind::Mic, cx))
                .child(add("add-system", "Add PC audio", Kind::System, cx)),
        )
    }

    /// Adds to `block` the choices of how a microphone is filtered and levelled.
    fn mic_controls(&self, block: Div, mic: Mic, tuning: Tuning, cx: &mut Context<Self>) -> Div {
        let id = |name: &'static str, remote: &'static str| -> ElementId {
            match mic {
                Mic::Local(index) => (name, index).into(),
                Mic::Remote(slot) => (remote, slot as usize).into(),
            }
        };
        let pick = cx.listener(move |this, &mode: &Mode, _, cx| this.tune(mic, |tuning| tuning.denoise = mode, cx));
        let block = block.child(segmented(id("denoise", "remote-denoise"), &DENOISE_CHOICES, tuning.denoise, pick));
        let delay = tuning.denoise.delay_ms();
        let block = match tuning.denoise {
            Mode::Off => block,
            Mode::Reduce => block.child(detail(format!(
                "Background noise is taken out from under the voice and nothing is cut off. \
                 Typing and clicks still come through in part. Adds about {delay} ms of delay."
            ))),
            Mode::Gate(_) => {
                let tuned = !DENOISE_CHOICES.iter().any(|&(mode, _)| mode == tuning.denoise);
                block
                    .child(detail(format!(
                        "{}Typing, mouse clicks, breathing and background noise are removed. \
                         Adds about {delay} ms of delay.",
                        if tuned { "Your own tuning. " } else { "" }
                    )))
                    .child(self.number_row(id("gate-lookahead", "remote-gate-lookahead"), Exact::Lookahead(mic), cx))
                    .child(detail(
                        "How far it listens ahead. More keeps the start of a word after a pause, \
                         and delays the voice as much.",
                    ))
                    .child(self.number_row(id("gate-hold", "remote-gate-hold"), Exact::Hold(mic), cx))
                    .child(detail(
                        "How long it stays open after the voice stops. More bridges longer gaps, \
                         and lets more noise trail a phrase.",
                    ))
                    .child(self.number_row(id("gate-threshold", "remote-gate-threshold"), Exact::Threshold(mic), cx))
                    .child(detail(
                        "How sure it must be that it hears a voice. Lower opens for quiet speech, \
                         and for more that is not speech.",
                    ))
            }
        };
        let options = [(false, "Level as it is"), (true, "Automatic level")];
        let pick =
            cx.listener(move |this, &auto_gain: &bool, _, cx| this.tune(mic, |tuning| tuning.auto_gain = auto_gain, cx));
        let block = block.child(segmented(id("auto-gain", "remote-auto-gain"), &options, tuning.auto_gain, pick));
        block.child(self.number_row(id("gain", "remote-gain"), Exact::Gain(mic), cx)).child(detail(
            match (tuning.auto_gain, tuning.gain_db) {
                (true, 0) => "Speech is brought to a steady level, however quiet the microphone is.",
                (true, _) => "Speech is brought to a steady level, then louder or quieter by this much.",
                (false, 0) => "Sent as loud as the microphone delivers it.",
                (false, _) => "Louder or quieter by this much. A limiter keeps it from clipping.",
            },
        ))
    }

    /// The microphones the other device sends, each with the settings it has
    /// there, to be changed from here.
    fn remote_section(&self, cx: &mut Context<Self>) -> Option<Div> {
        let State::Connected { name, .. } = &self.state else { return None };
        let mut mics = self.stats.remote_sources.iter().filter(|source| source.kind == SourceKind::Mic).peekable();
        mics.peek()?;
        let mut section = section("THE OTHER DEVICE'S MICROPHONE").child(detail(format!(
            "Filtered and levelled on {name} before it is sent. These are its own settings: \
             a change made here is made there, and stays."
        )));
        for source in mics {
            let title = format!("Microphone · stream {}", source.slot + 1);
            let block = div().flex().flex_col().gap_1().child(div().text_size(px(13.)).child(title));
            section = section.child(self.mic_controls(block, Mic::Remote(source.slot), remote_tuning(source), cx));
        }
        Some(section)
    }

    /// Where each of the other device's streams is played, by number: the
    /// ones listed in the settings, then the ones it sends that are not.
    fn streams_section(&self, cx: &mut Context<Self>) -> Div {
        let mut options = vec![(Play::Off, "Nothing"), (Play::Speaker, "Speakers")];
        // Only PipeWire lets us create the device ourselves.
        if cfg!(target_os = "linux") {
            options.push((Play::VirtualMic, "Virtual mic"));
        }
        let mut section = section("PLAY WHAT THE OTHER DEVICE SENDS").child(detail(
            "Each stream has a place of its own. They are numbered in the order the other device \
             sends them. One that is not listed plays on the default speakers; listed twice, in both places.",
        ));
        // What the other device says it sends, or for one too old to say,
        // what is arriving from it.
        let sent = (self.stats.remote_sources.iter().map(|source| (source.slot, source.kind)))
            .chain(self.stats.rx.iter().map(|rx| (rx.slot, rx.kind)));
        let title = |slot: u8| match sent.clone().find(|&(sent, _)| sent == slot) {
            Some((_, kind)) => format!("Stream {} · {}", slot + 1, kind_name(kind)),
            None => format!("Stream {}", slot + 1),
        };
        let link = |id: &'static str, index: usize, label: &'static str| {
            div()
                .id((id, index))
                .text_size(px(12.))
                .text_color(rgb(theme::MUTED))
                .hover(|s| s.text_color(rgb(theme::TEXT)))
                .cursor_pointer()
                .child(label)
        };
        let block = |heading: Div, choices: Stateful<Div>| div().flex().flex_col().gap_1().child(heading).child(choices);
        let virtual_mic =
            || detail("Appears to other apps as the input device \"SoundBridge Microphone\" while connected.");
        for (index, stream) in self.settings.play_streams.iter().enumerate() {
            let renumber = |id, label, by: i32, cx: &mut Context<Self>| {
                link(id, index, label).on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    if let Some(stream) = this.settings.play_streams.get_mut(index) {
                        stream.slot = (stream.slot as i32 + by).clamp(0, MAX_STREAMS as i32 - 1) as u8;
                    }
                    this.settings_changed(cx);
                }))
            };
            let remove =
                link("remove-stream", index, "Remove").on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    if index < this.settings.play_streams.len() {
                        this.settings.play_streams.remove(index);
                    }
                    this.open_picker = None;
                    this.settings_changed(cx);
                }));
            let pick = cx.listener(move |this, &play: &Play, _, cx| {
                if let Some(stream) = this.settings.play_streams.get_mut(index) {
                    stream.route.play = play;
                }
                this.open_picker = None;
                this.settings_changed(cx);
            });
            let heading = div()
                .flex()
                .items_center()
                .gap_3()
                .child(div().flex_1().text_size(px(13.)).child(title(stream.slot)))
                .child(renumber("stream-earlier", "Earlier", -1, cx))
                .child(renumber("stream-later", "Later", 1, cx))
                .child(remove);
            let block = block(heading, segmented(("play-stream", index), &options, stream.route.play, pick));
            section = section.child(match stream.route.play {
                Play::Off => block,
                Play::Speaker => block.child(self.device_picker(
                    Picker::Stream(index),
                    &stream.route.device,
                    DeviceDirection::Output,
                    cx,
                )),
                Play::VirtualMic => block.child(virtual_mic()),
            });
        }
        // The rest of what is being sent, where it plays for not being
        // listed. Setting one of these lists it.
        let listed = |slot: u8| self.settings.play_streams.iter().any(|stream| stream.slot == slot);
        let mut unlisted: Vec<u8> = sent.clone().map(|(slot, _)| slot).filter(|&slot| !listed(slot)).collect();
        unlisted.sort_unstable();
        unlisted.dedup();
        for slot in unlisted {
            let pick = cx.listener(move |this, &play: &Play, _, cx| {
                let route = Route { play, device: None };
                this.settings.play_streams.push(StreamRoute { slot, route });
                this.open_picker = None;
                this.settings_changed(cx);
            });
            let heading = div().text_size(px(13.)).child(title(slot));
            let choices = segmented(("play-unlisted", slot as usize), &options, Play::Speaker, pick);
            section = section.child(block(heading, choices).child(self.device_picker(
                Picker::Unlisted(slot),
                &None,
                DeviceDirection::Output,
                cx,
            )));
        }
        section.child(div().flex().child(button("add-stream", "Add a stream", false).on_click(cx.listener(
            |this, _: &ClickEvent, _, cx| {
                // The first one that is not listed yet.
                let taken = |slot: &u8| this.settings.play_streams.iter().any(|s| s.slot == *slot);
                let slot = (0..MAX_STREAMS as u8).find(|slot| !taken(slot)).unwrap_or(0);
                this.settings.play_streams.push(StreamRoute { slot, route: Route::default() });
                this.open_picker = None;
                this.settings_changed(cx);
            },
        ))))
    }

    fn buffer_section(&self, cx: &mut Context<Self>) -> Div {
        let pick = cx.listener(|this, &fixed: &bool, _, cx| {
            this.settings.buffer_ms = fixed.then_some(this.settings.buffer_ms.unwrap_or(BUFFER_MS));
            this.open_picker = None;
            if this.exact.as_ref().is_some_and(|exact| exact.what == Exact::Buffer) {
                this.exact = None;
            }
            this.settings_changed(cx);
        });
        let hint = match self.settings.buffer_ms {
            None => "Follows the network: as small as it allows, larger when it gets rough.",
            Some(_) => "Fixed. Lower is less delay; too low for the network and audio breaks up.",
        };
        let fixed = self.settings.buffer_ms.is_some();
        section("JITTER BUFFER")
            .child(segmented("buffer", &BUFFER_CHOICES, fixed, pick))
            .when(fixed, |section| section.child(self.number_row("buffer-ms", Exact::Buffer, cx)))
            .child(detail(hint))
    }

    #[cfg(feature = "self-update")]
    fn updates_section(&self, cx: &mut Context<Self>) -> Div {
        let pick = cx.listener(|this, &auto: &bool, _, cx| {
            this.auto_update = auto;
            soundbridge_update::set_auto_enabled(auto);
            cx.notify();
        });
        let hint: SharedString = match (self.updated, self.auto_update) {
            (Some(version), _) => format!("Updated to {version}. Restart SoundBridge to use it.").into(),
            (None, true) => "New releases are looked for once a day and installed for the next start.".into(),
            (None, false) => format!("Stays at {} until this is turned back on.", env!("CARGO_PKG_VERSION")).into(),
        };
        let section =
            section("UPDATES").child(segmented("updates", &UPDATE_CHOICES, self.auto_update, pick)).child(detail(hint));
        if self.updated.is_some() {
            return section;
        }
        let current = env!("CARGO_PKG_VERSION");
        let check = || {
            button("check-update", "Check now", false)
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.check_for_update(cx)))
        };
        let row = div().flex().items_center().gap_2();
        section.child(match &self.update_check {
            UpdateCheck::Idle => row.child(check()),
            UpdateCheck::Checking => row.child(detail("Looking for a new release...")),
            UpdateCheck::Current => row.child(check()).child(detail(format!("{current} is the latest release."))),
            &UpdateCheck::Available(version) => row
                .child(button("install-update", format!("Install {version}"), true).on_click(
                    cx.listener(move |this, _: &ClickEvent, _, cx| this.install_update(version, cx)),
                ))
                .child(detail(format!("This is {current}."))),
            UpdateCheck::Installing(version) => row.child(detail(format!("Installing {version}..."))),
            UpdateCheck::Failed(error) => row.child(check()).child(
                div().flex_1().text_size(px(12.)).text_color(rgb(theme::BAD)).child(error.clone()),
            ),
        })
    }

    fn peers_section(&self, cx: &mut Context<Self>) -> Div {
        let mut section = section("DEVICES ON THIS NETWORK");
        if self.peers.is_empty() {
            section = section.child(detail(
                "Looking... Open SoundBridge on the other device, on the same network.",
            ));
        }
        for (i, peer) in self.peers.iter().enumerate() {
            let addr = peer.addr;
            let row = div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .overflow_hidden()
                        .child(div().text_size(px(14.)).truncate().child(peer.name.clone()))
                        .child(detail(peer.addr.ip().to_string())),
                )
                .child(if peer.busy {
                    detail("In use").into_any_element()
                } else {
                    button(("peer", i), "Connect", true)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.connect(addr, cx)))
                        .into_any_element()
                });
            section = section.child(row);
        }
        section.child(
            div().flex().items_center().gap_2().child(self.host.clone()).child(
                button("connect-typed", "Connect", self.peers.is_empty())
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.connect_typed(cx))),
            ),
        )
    }

    fn session_section(&self, cx: &mut Context<Self>) -> Div {
        let (title, addr) = match &self.state {
            State::Connected { addr, .. } => ("CONNECTED", *addr),
            State::Connecting { addr } => ("CONNECTING", *addr),
            State::Idle => unreachable!("only shown during a session"),
        };
        let connected = matches!(self.state, State::Connected { .. });
        let mut section = section(title);
        if connected {
            let rtt = match self.stats.rtt_ms {
                Some(rtt) => format!("{} · network round trip {rtt:.1} ms", addr.ip()),
                None => addr.ip().to_string(),
            };
            section = section.child(detail(rtt));

            // What we send, source by source.
            if self.settings.sources.is_empty() {
                section = section.child(detail("Not sending anything."));
            }
            for (slot, source) in self.settings.sources.iter().enumerate() {
                let name = kind_name(wire_kind(source.kind));
                let Some(tx) = self.stats.tx.iter().find(|tx| tx.slot as usize == slot) else {
                    section = section.child(detail(format!("Not sending the {name}: it could not be opened.")));
                    continue;
                };
                let far_end = match &tx.remote {
                    Some(r) if r.playing => {
                        let mut text = format!("Buffered {:.1} ms at the other end", r.buffer_us as f32 / 1000.0);
                        if r.lost > 0 || r.underruns > 0 {
                            text += &format!(" · {} lost · {} dropouts", r.lost, r.underruns);
                        }
                        text
                    }
                    Some(_) => "The other end is buffering".to_string(),
                    None => "The other end is not playing it".to_string(),
                };
                let filtered = match tx.denoise {
                    Mode::Off => "",
                    Mode::Reduce => " · less noise",
                    Mode::Gate(_) => " · voice only",
                };
                section = section.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(div().text_size(px(13.)).child(format!("Sending {name} · {}{filtered}", tx.format)))
                        .child(meter(tx.level))
                        .child(detail(far_end)),
                );
            }

            // What we receive.
            if self.stats.rx.is_empty() {
                section = section.child(detail("The other device is not sending anything played here."));
            }
            for rx in &self.stats.rx {
                let mut text = format!("Buffer {:.1} ms · jitter {:.1} ms", rx.buffer_ms, rx.jitter_ms);
                if !rx.playing {
                    text = "Buffering...".into();
                } else if rx.lost > 0 || rx.underruns > 0 {
                    text += &format!(" · {} lost · {} dropouts", rx.lost, rx.underruns);
                }
                let to = match rx.sink {
                    SinkKind::VirtualMic => " · to the virtual mic",
                    _ => "",
                };
                let title = format!("Receiving stream {}, {} · {}{to}", rx.slot + 1, kind_name(rx.kind), rx.format);
                section = section.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(div().text_size(px(13.)).child(title))
                        .child(meter(rx.level))
                        .child(detail(text)),
                );
            }
        }
        section.child(div().flex().child(
            button("disconnect", if connected { "Disconnect" } else { "Cancel" }, false)
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.disconnect(cx))),
        ))
    }
}

impl Render for Bridge {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let in_session = !matches!(self.state, State::Idle);
        let mut column = div()
            .id("content")
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .child(self.status_line());
        if let Some(notice) = &self.notice {
            let colour = if notice.error { theme::BAD } else { theme::MUTED };
            column = column.child(div().text_size(px(13.)).text_color(rgb(colour)).child(notice.text.clone()));
        }
        column = column.child(if in_session { self.session_section(cx) } else { self.peers_section(cx) });
        if let Some(section) = self.remote_section(cx) {
            column = column.child(section);
        }
        column = column
            .child(self.send_section(cx))
            .child(self.streams_section(cx))
            .child(self.buffer_section(cx));
        #[cfg(feature = "self-update")]
        if self.self_updating {
            column = column.child(self.updates_section(cx));
        }
        div()
            .size_full()
            .bg(rgb(theme::BACKGROUND))
            .text_color(rgb(theme::TEXT))
            .child(column)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_numbers_as_they_are_typed() {
        assert_eq!(parse_number(" 7 "), Some(7.0));
        assert_eq!(parse_number("+6 dB"), Some(6.0));
        assert_eq!(parse_number("-3,5"), Some(-3.5));
        assert_eq!(parse_number("\u{2212}12dB"), Some(-12.0));
        assert_eq!(parse_number("35ms"), Some(35.0));
        for junk in ["", "dB", "loud", "inf", "NaN", "1 2"] {
            assert_eq!(parse_number(junk), None, "{junk:?}");
        }
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        text_input::bind_keys(cx);
        let bounds = Bounds::centered(None, size(px(440.), px(720.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("SoundBridge".into()),
                    ..Default::default()
                }),
                app_id: Some("soundbridge".into()),
                window_min_size: Some(size(px(360.), px(420.))),
                ..Default::default()
            },
            |_, cx| cx.new(Bridge::new),
        )
        .expect("failed to open the main window");
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}
