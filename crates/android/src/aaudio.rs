//! Android audio through AAudio, asked for its low-latency path.
//!
//! The fast path is only granted to streams that take the device's own
//! sample rate and a float or 16-bit format with no effects, so nothing here
//! requests a rate: the engine is told what the device chose and resamples
//! on the receiving side.

use std::slice;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use ndk::audio::{
    AudioCallbackResult, AudioDirection, AudioFormat, AudioInputPreset, AudioPerformanceMode,
    AudioSharingMode, AudioStream, AudioStreamBuilder,
};
use soundbridge_core::audio::{
    AudioBackend, CaptureFn, CaptureRequest, DeviceInfo, MakeCapture, MakeRender, PlaybackRequest,
    RenderFn, SourceKind, StreamFormat, StreamHandle,
};

use crate::system_audio;

pub struct AAudioBackend;

struct Stream {
    stream: AudioStream,
    dead: Arc<AtomicBool>,
}

// SAFETY: an AAudio stream may be controlled from any thread as long as calls
// are not concurrent; this handle is owned by one thread at a time.
unsafe impl Send for Stream {}

impl StreamHandle for Stream {
    /// AAudio streams do not follow route changes: plugging in headphones
    /// disconnects the stream, and the engine opens a new one on the new route.
    fn is_alive(&self) -> bool {
        !self.dead.load(Relaxed)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.stream.request_stop();
    }
}

/// A callback handed to the audio thread after the stream is open: the
/// stream's real format is only known then, and the callback is built from it.
struct Claim<F> {
    slot: Arc<Mutex<Option<F>>>,
    mine: Option<F>,
}

impl<F> Claim<F> {
    fn get(&mut self) -> Option<&mut F> {
        if self.mine.is_none() {
            self.mine = self.slot.try_lock().ok()?.take();
        }
        self.mine.as_mut()
    }
}

fn slot<F>() -> (Arc<Mutex<Option<F>>>, Claim<F>) {
    let slot = Arc::new(Mutex::new(None));
    let claim = Claim { slot: slot.clone(), mine: None };
    (slot, claim)
}

fn builder(direction: AudioDirection, channels: u16, dead: &Arc<AtomicBool>) -> Result<AudioStreamBuilder, String> {
    let dead = dead.clone();
    Ok(AudioStreamBuilder::new()
        .map_err(|e| format!("AAudio is unavailable: {e}"))?
        .direction(direction)
        .format(AudioFormat::PCM_Float)
        .channel_count(channels as i32)
        .performance_mode(AudioPerformanceMode::LowLatency)
        .sharing_mode(AudioSharingMode::Shared)
        .error_callback(Box::new(move |_, _| dead.store(true, Relaxed))))
}

fn format_of(stream: &AudioStream) -> Result<StreamFormat, String> {
    if stream.format() != AudioFormat::PCM_Float {
        return Err("the device refused float samples".into());
    }
    Ok(StreamFormat {
        sample_rate: stream.sample_rate() as u32,
        channels: stream.channel_count() as u16,
    })
}

impl AudioBackend for AAudioBackend {
    /// Routing is Android's business; the app always uses the current route.
    fn devices(&self) -> Vec<DeviceInfo> {
        Vec::new()
    }

    fn open_capture(
        &self,
        request: &CaptureRequest,
        make: MakeCapture,
    ) -> Result<Box<dyn StreamHandle>, String> {
        if request.kind == SourceKind::System {
            return Ok(system_audio::open(make));
        }
        let dead = Arc::new(AtomicBool::new(false));
        let (slot, mut claim) = slot::<CaptureFn>();
        let stream = builder(AudioDirection::Input, request.channels, &dead)?
            // The preset with the least processing in the way, and the one
            // devices are required to keep on a low-latency path.
            .input_preset(AudioInputPreset::VoiceRecognition)
            .data_callback(Box::new(move |stream, data, frames| {
                let len = frames as usize * stream.channel_count() as usize;
                // SAFETY: AAudio hands us `frames` frames of the format we
                // asked for (checked after opening), valid for this call.
                let samples = unsafe { slice::from_raw_parts(data as *const f32, len) };
                if let Some(callback) = claim.get() {
                    callback(samples);
                }
                AudioCallbackResult::Continue
            }))
            .open_stream()
            .map_err(|e| format!("opening the microphone: {e}"))?;
        *slot.lock().unwrap() = Some(make(format_of(&stream)?));
        stream.request_start().map_err(|e| format!("starting the microphone: {e}"))?;
        Ok(Box::new(Stream { stream, dead }))
    }

    fn open_playback(
        &self,
        _request: &PlaybackRequest,
        make: MakeRender,
    ) -> Result<Box<dyn StreamHandle>, String> {
        let dead = Arc::new(AtomicBool::new(false));
        let (slot, mut claim) = slot::<RenderFn>();
        let stream = builder(AudioDirection::Output, 2, &dead)?
            .data_callback(Box::new(move |stream, data, frames| {
                let len = frames as usize * stream.channel_count() as usize;
                // SAFETY: as for capture; the buffer is ours to fill.
                let out = unsafe { slice::from_raw_parts_mut(data as *mut f32, len) };
                match claim.get() {
                    Some(callback) => callback(out),
                    None => out.fill(0.0),
                }
                AudioCallbackResult::Continue
            }))
            .open_stream()
            .map_err(|e| format!("opening the output: {e}"))?;
        // Two bursts is the smallest buffer that survives ordinary scheduling
        // jitter; the default is several times that.
        let _ = stream.set_buffer_size_in_frames(stream.frames_per_burst() * 2);
        *slot.lock().unwrap() = Some(make(format_of(&stream)?));
        stream.request_start().map_err(|e| format!("starting the output: {e}"))?;
        Ok(Box::new(Stream { stream, dead }))
    }
}
