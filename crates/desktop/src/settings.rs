//! What the user last chose, kept in a small `key=value` file so the app
//! comes back the way it was left.

use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Mic,
    System,
}

/// One stream this PC sends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub kind: Kind,
    /// Device id; `None` follows the system default.
    pub device: Option<String>,
    /// Send speech only. Offered for microphones.
    pub denoise: bool,
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

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    pub sources: Vec<Source>,
    /// Microphones the other device sends.
    pub play_mic: Route,
    /// What the other device is playing.
    pub play_system: Route,
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
                // kind|denoise|device; the device comes last as it may hold anything.
                "source" => {
                    let mut fields = value.splitn(3, '|');
                    let kind = match fields.next() {
                        Some("mic") => Kind::Mic,
                        Some("system") => Kind::System,
                        _ => continue,
                    };
                    let denoise = fields.next() == Some("1");
                    s.sources.push(Source { kind, device: fields.next().and_then(device), denoise });
                }
                "play_mic" => s.play_mic.play = play(value),
                "play_mic_device" => s.play_mic.device = device(value),
                "play_system" => s.play_system.play = play(value),
                "play_system_device" => s.play_system.device = device(value),
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
            out += &format!("source={kind}|{}|{}\n", source.denoise as u8, device(&source.device));
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

    #[test]
    fn round_trips() {
        let settings = Settings {
            sources: vec![
                Source { kind: Kind::Mic, device: None, denoise: true },
                Source { kind: Kind::System, device: Some("pipewire:alsa_output.pci-0000_0e_00.4|x".into()), denoise: false },
                Source { kind: Kind::Mic, device: Some("wasapi:{0.0.0.00000000}.{a=b}".into()), denoise: false },
            ],
            play_mic: Route { play: Play::VirtualMic, device: None },
            play_system: Route { play: Play::Speaker, device: Some("wasapi:{0.0.0.00000000}.{a=b}".into()) },
            buffer_ms: Some(10),
            host: "192.168.1.20:47800".into(),
        };
        assert_eq!(Settings::parse(&settings.serialize()), settings);
        assert_eq!(Settings::parse(&Settings::default().serialize()), Settings::default());
    }

    #[test]
    fn tolerates_junk() {
        let s = Settings::parse("nonsense\nsource=loud\nsend=mic\nbuffer_ms=soon\nplay_mic=off\n");
        let play_mic = Route { play: Play::Off, device: None };
        assert_eq!(s, Settings { play_mic, ..Settings::default() });
    }
}
