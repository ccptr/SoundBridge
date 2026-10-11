//! What the user last chose, kept in a small `key=value` file so the app
//! comes back the way it was left.

use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Send {
    #[default]
    Off,
    Mic,
    System,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Play {
    Off,
    #[default]
    Speaker,
    VirtualMic,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    pub send: Send,
    pub play: Play,
    /// Device ids; `None` follows the system default.
    pub mic_device: Option<String>,
    pub system_device: Option<String>,
    pub speaker_device: Option<String>,
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
            let device = || Some(value.to_string()).filter(|v| !v.is_empty());
            match key {
                "send" => {
                    s.send = match value {
                        "mic" => Send::Mic,
                        "system" => Send::System,
                        _ => Send::Off,
                    }
                }
                "play" => {
                    s.play = match value {
                        "off" => Play::Off,
                        "virtual-mic" => Play::VirtualMic,
                        _ => Play::Speaker,
                    }
                }
                "mic_device" => s.mic_device = device(),
                "system_device" => s.system_device = device(),
                "speaker_device" => s.speaker_device = device(),
                "buffer_ms" => s.buffer_ms = value.parse().ok(),
                "host" => s.host = value.to_string(),
                _ => {}
            }
        }
        s
    }

    fn serialize(&self) -> String {
        let send = match self.send {
            Send::Off => "off",
            Send::Mic => "mic",
            Send::System => "system",
        };
        let play = match self.play {
            Play::Off => "off",
            Play::Speaker => "speaker",
            Play::VirtualMic => "virtual-mic",
        };
        let device = |d: &Option<String>| d.clone().unwrap_or_default();
        format!(
            "send={send}\nplay={play}\nmic_device={}\nsystem_device={}\nspeaker_device={}\nbuffer_ms={}\nhost={}\n",
            device(&self.mic_device),
            device(&self.system_device),
            device(&self.speaker_device),
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
            send: Send::System,
            play: Play::VirtualMic,
            mic_device: None,
            system_device: Some("pipewire:alsa_output.pci-0000_0e_00.4.analog-stereo".into()),
            speaker_device: Some("wasapi:{0.0.0.00000000}.{a=b}".into()),
            buffer_ms: Some(10),
            host: "192.168.1.20:47800".into(),
        };
        assert_eq!(Settings::parse(&settings.serialize()), settings);
        assert_eq!(Settings::parse(&Settings::default().serialize()), Settings::default());
    }

    #[test]
    fn tolerates_junk() {
        let s = Settings::parse("nonsense\nsend=loud\nbuffer_ms=soon\nplay=off\n");
        assert_eq!(s, Settings { play: Play::Off, ..Settings::default() });
    }
}
