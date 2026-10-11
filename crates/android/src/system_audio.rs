//! The phone's own playback as a source.
//!
//! Android only exposes that through `AudioRecord` with a MediaProjection,
//! which is Java API. So the app records on its side and pushes the PCM in
//! through JNI; this module is where it lands.

use std::sync::Mutex;

use soundbridge_core::audio::{CaptureFn, MakeCapture, StreamFormat, StreamHandle};

/// What the Kotlin side records. Fixed, so both sides agree without asking.
pub const FORMAT: StreamFormat = StreamFormat { sample_rate: 48_000, channels: 2 };

/// The engine's callback while a session wants system audio, else `None`.
static SINK: Mutex<Option<CaptureFn>> = Mutex::new(None);

struct Registration;

impl StreamHandle for Registration {}

impl Drop for Registration {
    fn drop(&mut self) {
        *SINK.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

pub fn open(make: MakeCapture) -> Box<dyn StreamHandle> {
    *SINK.lock().unwrap_or_else(|e| e.into_inner()) = Some(make(FORMAT));
    Box::new(Registration)
}

/// Called from the app's recording thread with interleaved 16-bit stereo.
/// Dropped on the floor when no session is taking system audio.
pub fn push(samples: &[i16]) {
    let Ok(mut sink) = SINK.try_lock() else { return };
    let Some(callback) = sink.as_mut() else { return };
    let mut scratch = [0.0f32; 1024];
    for chunk in samples.chunks(scratch.len()) {
        let out = &mut scratch[..chunk.len()];
        for (o, &s) in out.iter_mut().zip(chunk) {
            *o = s as f32 * (1.0 / 32_768.0);
        }
        callback(out);
    }
}
