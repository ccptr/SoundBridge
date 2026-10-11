//! The seam between the engine and whatever plays and records sound on a
//! given platform (cpal on desktops, AAudio on Android).

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

impl fmt::Display for StreamFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ch = match self.channels {
            1 => "mono".to_string(),
            2 => "stereo".to_string(),
            n => format!("{n}ch"),
        };
        write!(f, "{:.1} kHz {ch}", self.sample_rate as f32 / 1000.0)
    }
}

/// What this end captures and sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    /// A microphone or other input device.
    Mic,
    /// Whatever the machine is playing (loopback / playback capture).
    System,
    /// A generated 440 Hz tone, for testing a link without hardware.
    Tone,
}

/// Where received audio goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SinkKind {
    /// An output device.
    Speaker,
    /// A virtual microphone other applications can record from.
    VirtualMic,
    /// Nowhere; the stream is clocked and measured but not heard.
    Null,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureRequest {
    pub kind: SourceKind,
    /// Backend device id; `None` is the default device.
    pub device: Option<String>,
    pub sample_rate: u32,
    pub channels: u16,
    /// Preferred callback size in frames. Backends treat it as a hint.
    pub period_frames: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaybackRequest {
    pub kind: SinkKind,
    pub device: Option<String>,
    pub sample_rate: u32,
    pub channels: u16,
    pub period_frames: u32,
}

/// Receives captured audio: interleaved `f32` in the format the backend
/// reported. Runs on the audio thread, so it must not block or allocate.
pub type CaptureFn = Box<dyn FnMut(&[f32]) + Send>;
/// Fills an output buffer with interleaved `f32`. Same constraints.
pub type RenderFn = Box<dyn FnMut(&mut [f32]) + Send>;

/// Builds the callback once the backend knows which format it actually got,
/// which is rarely knowable before the device is open.
pub type MakeCapture = Box<dyn FnOnce(StreamFormat) -> CaptureFn + Send>;
pub type MakeRender = Box<dyn FnOnce(StreamFormat) -> RenderFn + Send>;

/// A running stream. Dropping it stops the stream.
pub trait StreamHandle: Send {
    /// False once the stream has died underneath us (its device was
    /// unplugged, or the platform rerouted audio); the engine then reopens it.
    fn is_alive(&self) -> bool {
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceDirection {
    Input,
    Output,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub direction: DeviceDirection,
}

pub trait AudioBackend: Send + Sync {
    /// Devices a user can pick. Output devices double as system-audio
    /// sources on backends that can capture what a device is playing.
    fn devices(&self) -> Vec<DeviceInfo>;

    fn open_capture(
        &self,
        request: &CaptureRequest,
        make: MakeCapture,
    ) -> Result<Box<dyn StreamHandle>, String>;

    fn open_playback(
        &self,
        request: &PlaybackRequest,
        make: MakeRender,
    ) -> Result<Box<dyn StreamHandle>, String>;
}
