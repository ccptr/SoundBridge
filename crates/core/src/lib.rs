//! SoundBridge core: a low-latency audio link between two machines.
//!
//! One side captures (a microphone, or whatever the machine is playing),
//! packs raw PCM into small UDP datagrams and sends them; the other feeds
//! them through an adaptive jitter buffer into an output device. Any number
//! of streams can run at once in each direction over the same socket.
//!
//! The latency budget is spent in three places, and the code is organised
//! around keeping each small:
//!
//! * **Capture and playback buffers** — the backends ask for the smallest
//!   period the platform will grant ([`audio`]).
//! * **Packetization** — packets hold a few milliseconds and are sent from
//!   the capture callback itself, with no thread hop ([`sender`]).
//! * **The jitter buffer** — sized from the jitter actually observed rather
//!   than a fixed guess, and held there by resampling ([`playout`]).

pub mod audio;
pub mod denoise;
#[cfg(all(feature = "cpal", not(target_os = "android")))]
pub mod cpal_backend;
pub mod engine;
pub mod gain;
pub mod playout;
pub mod protocol;
pub mod sender;
mod synthetic;

pub use audio::{
    AudioBackend, CaptureRequest, DeviceDirection, DeviceInfo, PlaybackRequest, SinkKind,
    SourceKind, StreamFormat, StreamHandle,
};
pub use engine::{
    BufferMode, Config, Engine, Event, Peer, RemoteStream, RxStats, SinkConfig, SourceConfig, State,
    Stats, Streams, TxStats,
};
pub use protocol::{DEFAULT_PORT, Report, SourceSettings};

/// This machine's hostname, as the name to show peers by default.
pub fn default_name() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: gethostname writes at most `len` bytes into `buf`.
        if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0 {
            let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            if let Some(name) = std::str::from_utf8(&buf[..len]).ok().filter(|n| !n.is_empty()) {
                return name.to_string();
            }
        }
    }
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "SoundBridge".into())
}
