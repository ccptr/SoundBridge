//! What the user last chose, kept in a small `key=value` file so the app
//! comes back the way it was left.

use std::path::PathBuf;

use soundbridge_core::denoise::Mode;
use soundbridge_core::protocol::MAX_STREAMS;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
    #[default]
    Mic,
    System,
}

/// One stream this PC sends.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Source {
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

/// Where one kind of received stream goes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Route {
    pub play: Play,
    /// Output device for [`Play::Speaker`]; `None` follows the system default.
    pub device: Option<String>,
}

/// Where one of the other device's streams goes, instead of where its kind does.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StreamRoute {
    /// Which stream, counted from 0 in the order the other device sends them.
    pub slot: u8,
    pub route: Route,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    pub sources: Vec<Source>,
    /// Microphones the other device sends.
    pub play_mic: Route,
    /// What the other device is playing.
    pub play_system: Route,
    /// Streams played somewhere of their own. One listed twice plays in both places.
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
                    s.sources.push(Source { kind, device, denoise, auto_gain, gain_db });
                }
                "play_mic" => s.play_mic.play = play(value),
                "play_mic_device" => s.play_mic.device = device(value),
                "play_system" => s.play_system.play = play(value),
                "play_system_device" => s.play_system.device = device(value),
                // number|play|device, the number counted from 1 as it is shown.
                "play_stream" => {
                    let mut fields = value.splitn(3, '|');
                    let number = fields.next().and_then(|n| n.parse::<usize>().ok());
                    let Some(slot) = number.filter(|n| (1..=MAX_STREAMS).contains(n)) else { continue };
                    let route = Route { play: play(fields.next().unwrap_or_default()), device: fields.next().and_then(device) };
                    s.play_streams.push(StreamRoute { slot: slot as u8 - 1, route });
                }
                "buffer_ms" => s.buffer_ms = value.parse().ok(),
                "host" => s.host = value.to_string(),
                _ => {}
            }
        }
        s
    }

    fn serialize(&self) -> String {
        let play = |play| match play {
            Play::Off => "off",
            Play::Speaker => "speaker",
            Play::VirtualMic => "virtual-mic",
        };
        let device = |d: &Option<String>| d.clone().unwrap_or_default();
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
        }
        for stream in &self.play_streams {
            let route = &stream.route;
            out += &format!("play_stream={}|{}|{}\n", stream.slot as usize + 1, play(route.play), device(&route.device));
        }
        out + &format!(
            "play_mic={}\nplay_mic_device={}\nplay_system={}\nplay_system_device={}\nbuffer_ms={}\nhost={}\n",
            play(self.play_mic.play),
            device(&self.play_mic.device),
            play(self.play_system.play),
            device(&self.play_system.device),
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
                Source { kind: Kind::Mic, denoise: Mode::Reduce, auto_gain: true, gain_db: -6, ..Source::default() },
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
            play_mic: Route { play: Play::VirtualMic, device: None },
            play_system: Route { play: Play::Speaker, device: Some("wasapi:{0.0.0.00000000}.{a=b}".into()) },
            play_streams: vec![
                StreamRoute { slot: 1, route: Route { play: Play::Speaker, device: Some("pipewire:a|b".into()) } },
                StreamRoute { slot: 1, route: Route::default() },
                StreamRoute { slot: 63, route: Route { play: Play::Off, device: None } },
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
        let junk = "nonsense\nsource=loud\nsend=mic\nbuffer_ms=soon\nplay_mic=off\nplay_stream=0|off|\nplay_stream=x\n";
        let s = Settings::parse(junk);
        let play_mic = Route { play: Play::Off, device: None };
        assert_eq!(s, Settings { play_mic, ..Settings::default() });
    }
}
