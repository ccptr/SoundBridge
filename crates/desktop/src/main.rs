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
    App, Application, Bounds, ClickEvent, Context, Div, ElementId, Entity, SharedString, Stateful,
    TitlebarOptions, Window, WindowBounds, WindowOptions, div, prelude::*, px, relative, rgb, size,
};
use soundbridge_core::cpal_backend::CpalBackend;
use soundbridge_core::{
    AudioBackend, BufferMode, Config, DEFAULT_PORT, DeviceDirection, DeviceInfo, Engine, Event,
    Peer, SinkConfig, SinkKind, SourceConfig, SourceKind, State, Stats,
};

use settings::{Kind, Play, Route, Settings, Source};
use text_input::{Submitted, TextInput};

/// Jitter-buffer choices offered in the UI; `None` is automatic.
const BUFFER_CHOICES: [(Option<u32>, &str); 5] =
    [(None, "Auto"), (Some(5), "5 ms"), (Some(10), "10 ms"), (Some(20), "20 ms"), (Some(40), "40 ms")];

/// The device list that is open, if any: for one of the sources, or for
/// where one kind of received stream is played.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Picker {
    Source(usize),
    Sink(Kind),
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
    host: Entity<TextInput>,
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
            host,
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
                self.state = state;
            }
            Event::Stats(stats) => self.stats = stats,
            Event::Peers(peers) => self.peers = peers,
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
            Picker::Sink(kind) => Some(&mut self.route_mut(kind).device),
        }
    }

    fn route(&self, kind: Kind) -> &Route {
        match kind {
            Kind::Mic => &self.settings.play_mic,
            Kind::System => &self.settings.play_system,
        }
    }

    fn route_mut(&mut self, kind: Kind) -> &mut Route {
        match kind {
            Kind::Mic => &mut self.settings.play_mic,
            Kind::System => &mut self.settings.play_system,
        }
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
                denoise: source.denoise && source.kind == Kind::Mic,
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
    let (mic, system) = (
        sink(&settings.play_mic, Some(SourceKind::Mic)),
        sink(&settings.play_system, Some(SourceKind::System)),
    );
    // Both kinds going to one place means everything does, test tones included.
    let same = sink(&settings.play_mic, None);
    if same.is_some() && same == sink(&settings.play_system, None) {
        return same.into_iter().collect();
    }
    mic.into_iter().chain(system).collect()
}

fn kind_name(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Mic => "microphone",
        SourceKind::System => "device audio",
        SourceKind::Tone => "test tone",
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
            Picker::Sink(Kind::Mic) => "mic-sink-device".into(),
            Picker::Sink(Kind::System) => "system-sink-device".into(),
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
                        .child(div().text_size(px(13.)).child(name))
                        .child(remove),
                )
                .child(self.device_picker(Picker::Source(index), &source.device, direction, cx));
            block = match source.kind {
                Kind::Mic => {
                    let options = [(false, "Everything it hears"), (true, "Voice only")];
                    let pick = cx.listener(move |this, &voice_only: &bool, _, cx| {
                        if let Some(source) = this.settings.sources.get_mut(index) {
                            source.denoise = voice_only;
                        }
                        this.open_picker = None;
                        this.settings_changed(cx);
                    });
                    let block = block.child(segmented(("denoise", index), &options, source.denoise, pick));
                    match source.denoise {
                        true => block.child(detail(
                            "Typing, mouse clicks, breathing and background noise are removed. \
                             Adds about 50 ms of delay.",
                        )),
                        false => block,
                    }
                }
                Kind::System => block.child(detail("Sends whatever is playing on that output.")),
            };
            section = section.child(block);
        }
        let add = |id: &'static str, label: &'static str, kind, cx: &mut Context<Self>| {
            button(id, label, false).on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.settings.sources.push(Source { kind, device: None, denoise: false });
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

    /// Where one kind of stream from the other device is played.
    fn play_section(&self, kind: Kind, cx: &mut Context<Self>) -> Div {
        let (title, id) = match kind {
            Kind::Mic => ("PLAY THE OTHER DEVICE'S MICROPHONE", "play-mic"),
            Kind::System => ("PLAY WHAT THE OTHER DEVICE IS PLAYING", "play-system"),
        };
        let mut options = vec![(Play::Off, "Nothing"), (Play::Speaker, "Speakers")];
        // Only PipeWire lets us create the device ourselves.
        if cfg!(target_os = "linux") {
            options.push((Play::VirtualMic, "Virtual mic"));
        }
        let route = self.route(kind);
        let pick = cx.listener(move |this, &play: &Play, _, cx| {
            this.route_mut(kind).play = play;
            this.open_picker = None;
            this.settings_changed(cx);
        });
        let section = section(title).child(segmented(id, &options, route.play, pick));
        match route.play {
            Play::Off => section,
            Play::Speaker => {
                section.child(self.device_picker(Picker::Sink(kind), &route.device, DeviceDirection::Output, cx))
            }
            Play::VirtualMic => section.child(detail(
                "Appears to other apps as the input device \"SoundBridge Microphone\" while connected.",
            )),
        }
    }

    fn buffer_section(&self, cx: &mut Context<Self>) -> Div {
        let pick = cx.listener(|this, &ms: &Option<u32>, _, cx| {
            this.settings.buffer_ms = ms;
            this.open_picker = None;
            this.settings_changed(cx);
        });
        let hint = match self.settings.buffer_ms {
            None => "Follows the network: as small as it allows, larger when it gets rough.",
            Some(_) => "Fixed. Lower is less delay; too low for the network and audio breaks up.",
        };
        section("JITTER BUFFER")
            .child(segmented("buffer", &BUFFER_CHOICES, self.settings.buffer_ms, pick))
            .child(detail(hint))
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
                let filtered = if tx.denoise { " · voice only" } else { "" };
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
                let ignoring = sinks(&self.settings).is_empty();
                section = section.child(detail(match ignoring {
                    true => "Ignoring incoming audio.",
                    false => "The other device is not sending anything played here.",
                }));
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
                let title = format!("Receiving {} · {}{to}", kind_name(rx.kind), rx.format);
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
        column = column
            .child(self.send_section(cx))
            .child(self.play_section(Kind::Mic, cx))
            .child(self.play_section(Kind::System, cx))
            .child(self.buffer_section(cx));
        div()
            .size_full()
            .bg(rgb(theme::BACKGROUND))
            .text_color(rgb(theme::TEXT))
            .child(column)
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
