//! What the user last chose, kept in a small `key=value` file so the app
//! comes back the way it was left.

use std::path::PathBuf;

use soundbridge_core::denoise::Mode;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
    #[default]
    Mic,
    System,
}

/// One stream this PC sends.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Source {
    /// What the other device is told to show the stream as. Empty for the
    /// name of the device it comes from.
    pub name: String,
    pub kind: Kind,
    /// Device id; `None` follows the system default.
    pub device: Option<String>,
    /// What is filtered out. Offered for microphones.
    pub denoise: Mode,
    /// Bring speech to a steady level. Offered for microphones.
    pub auto_gain: bool,
    /// Gain in dB, on top of the automatic one or on its own.
    pub gain_db: i32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Play {
    Off,
    #[default]
    Speaker,
    VirtualMic,
}

/// Where a received stream goes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Route {
    pub play: Play,
    /// Output device for [`Play::Speaker`]; `None` follows the system default.
    pub device: Option<String>,
}

/// Where one of the other device's streams goes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StreamRoute {
    /// Which stream: the id the other device knows its source by.
    pub source: String,
    /// What the other device last called it, for when it is not there to say.
    pub name: String,
    pub route: Route,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    pub sources: Vec<Source>,
    /// Where the other device's streams are played, each listed once at
    /// most. One that is not listed plays on the default output, so none is
    /// listed for that.
    pub play_streams: Vec<StreamRoute>,
    /// Fixed jitter buffer in ms, or `None` for automatic.
    pub buffer_ms: Option<u32>,
    /// The last address typed into the connect box.
    pub host: String,
}

fn path() -> Option<PathBuf> {
    let var = |name| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let dir = if cfg!(target_os = "windows") {
        var("APPDATA")?
    } else if cfg!(target_os = "macos") {
        var("HOME")?.join("Library/Application Support")
    } else {
        var("XDG_CONFIG_HOME").or_else(|| Some(var("HOME")?.join(".config")))?
    };
    Some(dir.join("soundbridge").join("desktop.conf"))
}

impl Settings {
    pub fn load() -> Settings {
        path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|text| Settings::parse(&text))
            .unwrap_or_default()
    }

    /// Best effort: a read-only home directory just means nothing is remembered.
    pub fn save(&self) {
        let Some(path) = path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, self.serialize());
    }

    fn parse(text: &str) -> Settings {
        let mut s = Settings::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            let device = |value: &str| Some(value.to_string()).filter(|v| !v.is_empty());
            let play = |value| match value {
                "off" => Play::Off,
                "virtual-mic" => Play::VirtualMic,
                _ => Play::Speaker,
            };
            match key {
                // kind|denoise[,auto gain,gain]|device; the device comes last as it
                // may hold anything. Files from before there was gain lack the rest,
                // and ones from when voice only was all there was say 1 for it.
                "source" => {
                    let mut fields = value.splitn(3, '|');
                    let kind = match fields.next() {
                        Some("mic") => Kind::Mic,
                        Some("system") => Kind::System,
                        _ => continue,
                    };
                    let mut flags = fields.next().unwrap_or_default().split(',');
                    let denoise = match flags.next() {
                        Some("1") => Mode::VOICE,
                        Some(mode) => mode.parse().unwrap_or_default(),
                        None => Mode::Off,
                    };
                    let auto_gain = flags.next() == Some("1");
                    let gain_db = flags.next().and_then(|db| db.parse().ok()).unwrap_or(0);
                    let device = fields.next().and_then(device);
                    s.sources.push(Source { name: String::new(), kind, device, denoise, auto_gain, gain_db });
                }
                // Of the source on the line before.
                "source_name" => {
                    if let Some(source) = s.sources.last_mut() {
                        source.name = value.to_string();
                    }
                }
                // A stream's source, which may hold anything, with where it
                // plays on the lines after it.
                "route" => {
                    if !value.is_empty() {
                        let route = Route { play: Play::Off, device: None };
                        s.play_streams.push(StreamRoute { source: value.to_string(), name: String::new(), route });
                    }
                }
                "route_play" | "route_device" | "route_name" => {
                    let Some(stream) = s.play_streams.last_mut() else { continue };
                    match key {
                        "route_play" => stream.route.play = play(value),
                        "route_device" => stream.route.device = device(value),
                        _ => stream.name = value.to_string(),
                    }
                }
                "buffer_ms" => s.buffer_ms = value.parse().ok(),
                "host" => s.host = value.to_string(),
                _ => {}
            }
        }
        // Each stream once, and none for where it would play anyway.
        let mut listed: Vec<StreamRoute> = Vec::new();
        for stream in s.play_streams.drain(..) {
            if stream.route != Route::default() && listed.iter().all(|listed| listed.source != stream.source) {
                listed.push(stream);
            }
        }
        s.play_streams = listed;
        s
    }

    fn serialize(&self) -> String {
        let play = |play| match play {
            Play::Off => "off",
            Play::Speaker => "speaker",
            Play::VirtualMic => "virtual-mic",
        };
        let device = |d: &Option<String>| d.clone().unwrap_or_default();
        // What comes from the other device may hold anything.
        let line = |text: &str| text.replace(['\n', '\r'], " ");
        let mut out = String::new();
        for source in &self.sources {
            let kind = match source.kind {
                Kind::Mic => "mic",
                Kind::System => "system",
            };
            out += &format!(
                "source={kind}|{},{},{}|{}\n",
                source.denoise,
                source.auto_gain as u8,
                source.gain_db,
                device(&source.device)
            );
            if !source.name.is_empty() {
                out += &format!("source_name={}\n", line(&source.name));
            }
        }
        for stream in &self.play_streams {
            let route = &stream.route;
            out += &format!(
                "route={}\nroute_play={}\nroute_device={}\nroute_name={}\n",
                line(&stream.source),
                play(route.play),
                device(&route.device),
                line(&stream.name)
            );
        }
        out + &format!(
            "buffer_ms={}\nhost={}\n",
            self.buffer_ms.map(|ms| ms.to_string()).unwrap_or_default(),
            self.host.replace('\n', " "),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundbridge_core::denoise::Gate;

    #[test]
    fn round_trips() {
        let settings = Settings {
            sources: vec![
                Source {
                    name: "Headset | left=1".into(),
                    kind: Kind::Mic,
                    denoise: Mode::Reduce,
                    auto_gain: true,
                    gain_db: -6,
                    ..Source::default()
                },
                Source {
                    kind: Kind::Mic,
                    denoise: Mode::Gate(Gate { lookahead_ms: 80, hold_ms: 500, threshold: 0.3 }),
                    ..Source::default()
                },
                Source {
                    kind: Kind::System,
                    device: Some("pipewire:alsa_output.pci-0000_0e_00.4|x".into()),
                    ..Source::default()
                },
                Source {
                    kind: Kind::Mic,
                    device: Some("wasapi:{0.0.0.00000000}.{a=b}".into()),
                    gain_db: 12,
                    ..Source::default()
                },
            ],
            play_streams: vec![
                StreamRoute {
                    source: "pipewire:alsa_input.usb={a|b}".into(),
                    name: "Phone | mic=1".into(),
                    route: Route { play: Play::Speaker, device: Some("pipewire:a|b".into()) },
                },
                StreamRoute { source: "mic".into(), name: String::new(), route: Route { play: Play::VirtualMic, device: None } },
                StreamRoute { source: "system".into(), name: "Far".into(), route: Route { play: Play::Off, device: None } },
            ],
            buffer_ms: Some(10),
            host: "192.168.1.20:47800".into(),
        };
        assert_eq!(Settings::parse(&settings.serialize()), settings);
        assert_eq!(Settings::parse(&Settings::default().serialize()), Settings::default());
    }

    #[test]
    fn reads_sources_saved_before_there_was_gain() {
        let s = Settings::parse("source=mic|1|pipewire:a|b\nsource=system|0|\n");
        let mic = Source {
            kind: Kind::Mic,
            device: Some("pipewire:a|b".into()),
            denoise: Mode::VOICE,
            ..Source::default()
        };
        assert_eq!(s.sources, [mic, Source { kind: Kind::System, ..Source::default() }]);
    }

    #[test]
    fn reads_sources_saved_when_voice_only_was_a_switch() {
        let s = Settings::parse("source=mic|1,1,-6|\nsource=mic|0,0,0|\n");
        let filtered = Source { denoise: Mode::VOICE, auto_gain: true, gain_db: -6, ..Source::default() };
        assert_eq!(s.sources, [filtered, Source::default()]);
    }

    #[test]
    fn tolerates_junk() {
        let junk = "nonsense\nsource=loud\nsend=mic\nbuffer_ms=soon\nroute=\nroute_play=off\nsource_name=x\n";
        assert_eq!(Settings::parse(junk), Settings::default());
    }

    #[test]
    fn lists_a_stream_once_and_not_for_the_default_output() {
        let saved = "route=system\nroute_play=speaker\nroute=mic\nroute_play=virtual-mic\nroute_name=Phone\n\
                     route=mic\nroute_play=off\n";
        let route = Route { play: Play::VirtualMic, device: None };
        assert_eq!(Settings::parse(saved).play_streams, [StreamRoute { source: "mic".into(), name: "Phone".into(), route }]);
    }

    #[test]
    fn drops_where_streams_were_played_by_kind_or_by_number() {
        // Neither says which source a stream comes from.
        let s = Settings::parse("play_mic=virtual-mic\nplay_system=off\nplay_stream=1|off|\nhost=pc\n");
        assert_eq!(s, Settings { host: "pc".into(), ..Settings::default() });
    }
}
