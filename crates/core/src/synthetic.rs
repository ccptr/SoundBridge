//! Hardware-free stand-ins for a capture and a playback device, so a link can
//! be exercised and measured on a machine with no sound card (or no wish to
//! make noise).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::audio::{MakeCapture, MakeRender, StreamFormat, StreamHandle};

struct ThreadStream {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl StreamHandle for ThreadStream {}

impl Drop for ThreadStream {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Calls `tick` once per period on a steady clock until dropped.
fn clocked(name: &str, period: Duration, mut tick: impl FnMut() + Send + 'static) -> Box<dyn StreamHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let thread = thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let mut next = Instant::now();
            while !flag.load(Relaxed) {
                tick();
                next += period;
                match next.checked_duration_since(Instant::now()) {
                    Some(wait) => thread::sleep(wait),
                    // Fell behind (suspended, debugger): don't burst to catch up.
                    None => next = Instant::now(),
                }
            }
        })
        .expect("spawn synthetic audio thread");
    Box::new(ThreadStream { stop, thread: Some(thread) })
}

/// A 440 Hz sine "microphone".
pub fn tone(format: StreamFormat, period_frames: u32, make: MakeCapture) -> Box<dyn StreamHandle> {
    let mut callback = make(format);
    let ch = format.channels as usize;
    let frames = period_frames.max(16) as usize;
    let step = 440.0 * std::f32::consts::TAU / format.sample_rate as f32;
    let mut phase = 0.0f32;
    let mut buf = vec![0.0f32; frames * ch];
    let period = Duration::from_secs_f64(frames as f64 / format.sample_rate as f64);
    clocked("sb-tone", period, move || {
        for frame in buf.chunks_exact_mut(ch) {
            frame.fill(phase.sin() * 0.25);
            phase = (phase + step) % std::f32::consts::TAU;
        }
        callback(&buf);
    })
}

/// An output that consumes audio at the right rate and discards it.
pub fn null_sink(format: StreamFormat, period_frames: u32, make: MakeRender) -> Box<dyn StreamHandle> {
    let mut callback = make(format);
    let frames = period_frames.max(16) as usize;
    let mut buf = vec![0.0f32; frames * format.channels as usize];
    let period = Duration::from_secs_f64(frames as f64 / format.sample_rate as f64);
    clocked("sb-null", period, move || callback(&mut buf))
}
