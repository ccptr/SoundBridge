//! The receive-side jitter buffer.
//!
//! Packets go in on the network thread ([`PlayoutWriter`]), samples come out
//! on the audio thread ([`PlayoutReader`]), joined by a lock-free ring. Two
//! problems are solved here:
//!
//! * **How much to buffer.** Too little and every late packet is a dropout;
//!   too much is latency for nothing. The writer measures how late packets
//!   actually arrive and publishes a margin just above that.
//! * **Staying there.** The sender's sample clock and the output device's
//!   never agree exactly, so an uncorrected buffer slowly drains or fills.
//!   The reader resamples by a fraction of a percent to hold the fill level
//!   on the margin, which also absorbs any difference in nominal sample rate.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering::Relaxed};

use rtrb::{Consumer, Producer, RingBuffer};

use crate::audio::StreamFormat;

/// How the jitter margin is chosen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BufferMode {
    /// Follow measured network jitter, within bounds.
    Auto { min_ms: f32, max_ms: f32 },
    /// Hold a fixed margin regardless of conditions.
    Fixed { ms: f32 },
}

impl Default for BufferMode {
    fn default() -> Self {
        BufferMode::Auto { min_ms: 2.0, max_ms: 80.0 }
    }
}

const NOT_READY: u32 = u32::MAX;
/// Ring capacity. Only an upper bound on buffering; normal fill is far lower.
const CAPACITY_MS: u32 = 500;
/// A timestamp jump larger than this is a new stream, not packet loss.
const MAX_GAP_MS: u32 = 250;
/// Auto mode listens this long before committing to a margin, so playback
/// does not start on a guess and immediately underrun.
const WARMUP_US: i64 = 100_000;
/// Arrival times this soon after a stream starts are not trusted: opening
/// the output device stalls the network thread, and the packets that queued
/// up meanwhile are then read in one burst that says nothing about the link.
const SETTLE_US: i64 = 20_000;
/// Frames over which edges are ramped to avoid clicks.
const FADE_FRAMES: usize = 48;

/// Time constant of the fill-level average the drift loop steers by.
const FILL_TAU_S: f64 = 0.5;
/// Loop gain: fractional speed change per second of fill error. With the
/// averaging above this gives a damping ratio of about 0.7.
const DRIFT_GAIN: f64 = 1.0;
/// Speed is never bent further than this (0.5 %, under 9 cents of pitch).
const MAX_DRIFT: f64 = 0.005;

/// State both halves touch.
pub struct Shared {
    /// Frames to keep in hand beyond one callback's worth, or [`NOT_READY`].
    margin_frames: AtomicU32,
    buffer_us: AtomicU32,
    underruns: AtomicU32,
    skips: AtomicU32,
    playing: AtomicBool,
    drift_ppm: AtomicI32,
    peak: AtomicU32,
}

impl Shared {
    /// Average time audio spends waiting in the buffer.
    pub fn buffer_us(&self) -> u32 {
        self.buffer_us.load(Relaxed)
    }
    pub fn underruns(&self) -> u32 {
        self.underruns.load(Relaxed)
    }
    /// Times the buffer was found far too full and jumped forward.
    pub fn skips(&self) -> u32 {
        self.skips.load(Relaxed)
    }
    pub fn playing(&self) -> bool {
        self.playing.load(Relaxed)
    }
    /// Current resampling correction, parts per million.
    pub fn drift_ppm(&self) -> i32 {
        self.drift_ppm.load(Relaxed)
    }
    /// Peak output level since the last call, 0..=1.
    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.peak.swap(0, Relaxed))
    }
}

/// Creates the two halves of a jitter buffer for one incoming stream.
/// `device` is the format the output callback will ask for.
pub fn playout(
    stream: StreamFormat,
    device: StreamFormat,
    mode: BufferMode,
) -> (PlayoutWriter, PlayoutReader, Arc<Shared>) {
    let ch = stream.channels as usize;
    let capacity = (stream.sample_rate as usize * CAPACITY_MS as usize / 1000) * ch;
    let (prod, cons) = RingBuffer::new(capacity);
    let shared = Arc::new(Shared {
        margin_frames: AtomicU32::new(NOT_READY),
        buffer_us: AtomicU32::new(0),
        underruns: AtomicU32::new(0),
        skips: AtomicU32::new(0),
        playing: AtomicBool::new(false),
        drift_ppm: AtomicI32::new(0),
        peak: AtomicU32::new(0),
    });
    let writer = PlayoutWriter {
        prod,
        ch,
        rate: stream.sample_rate,
        mode,
        shared: shared.clone(),
        expected: None,
        last: [0.0; 2],
        jitter: Jitter::new(),
        first_us: None,
        stats: WriterStats::default(),
    };
    let stream_rate = stream.sample_rate as f64;
    let device_rate = device.sample_rate as f64;
    let reader = PlayoutReader {
        cons,
        ch,
        shared: shared.clone(),
        stream_rate,
        device_rate,
        base_ratio: stream_rate / device_rate,
        phase: 0.0,
        taps: [[0.0; 2]; 4],
        playing: false,
        gain: 0.0,
        gain_step: 0.0,
        fade_step: 1.0 / (0.003 * device_rate) as f32,
        pending_skip: 0,
        fill_avg: 0.0,
        need: 0.0,
        last: [0.0; 2],
    };
    (writer, reader, shared)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct WriterStats {
    pub packets: u64,
    /// Packets that never arrived (estimated from timestamp gaps).
    pub lost: u32,
    /// Packets that arrived after their slot had already been filled in.
    pub late: u32,
    /// Packets dropped because the ring was full.
    pub overflows: u32,
    pub jitter_us: u32,
    /// The margin currently asked of the reader.
    pub target_us: u32,
}

pub struct PlayoutWriter {
    prod: Producer<f32>,
    ch: usize,
    rate: u32,
    mode: BufferMode,
    shared: Arc<Shared>,
    /// Timestamp the next in-order packet should carry.
    expected: Option<u32>,
    last: [f32; 2],
    jitter: Jitter,
    first_us: Option<i64>,
    stats: WriterStats,
}

impl PlayoutWriter {
    pub fn stats(&self) -> WriterStats {
        self.stats
    }

    /// Forgets the current timeline; the next packet starts a new one.
    pub fn restart(&mut self) {
        self.expected = None;
    }

    /// Queues one packet of interleaved s16le. `now_us` is the arrival time on
    /// any monotonic clock.
    ///
    /// Packets are not reordered: at these buffer depths a packet that shows
    /// up after its successor is already too late to play, so it is dropped
    /// and its slot stays concealed.
    pub fn push(&mut self, timestamp: u32, payload: &[u8], now_us: i64) {
        let frames = payload.len() / (2 * self.ch);
        if frames == 0 {
            return;
        }
        let max_gap = (self.rate / 1000 * MAX_GAP_MS) as i32;
        let mut gap = 0usize;
        let mut restart = self.expected.is_none();
        if let Some(expected) = self.expected {
            let d = timestamp.wrapping_sub(expected) as i32;
            if d < 0 {
                if d > -max_gap {
                    self.stats.late += 1;
                    return;
                }
                restart = true;
            } else if d > max_gap {
                restart = true;
            } else {
                gap = d as usize;
            }
        }
        if restart {
            // A new stream: its timestamps bear no relation to the old ones.
            // The reader notices on its own by running dry.
            self.jitter = Jitter::new();
            self.first_us = Some(now_us);
        }
        if gap > 0 {
            self.stats.lost += (gap / frames).max(1) as u32;
            self.conceal(gap);
        }
        self.expected = Some(timestamp.wrapping_add(frames as u32));
        self.stats.packets += 1;

        let since_start = now_us - self.first_us.unwrap_or(now_us);
        let jitter_us = match since_start >= SETTLE_US {
            true => self.jitter.observe(now_us, timestamp, self.rate),
            false => 0,
        };
        let margin_us = match self.mode {
            BufferMode::Fixed { ms } => ms * 1000.0,
            BufferMode::Auto { min_ms, max_ms } => {
                let max = max_ms.max(min_ms) * 1000.0;
                (jitter_us as f32 * 1.25 + 500.0).clamp(min_ms * 1000.0, max)
            }
        };
        self.stats.jitter_us = jitter_us;
        self.stats.target_us = margin_us as u32;
        let warmed_up = matches!(self.mode, BufferMode::Fixed { .. }) || since_start >= WARMUP_US;
        if warmed_up {
            // One packet on top of the jitter margin: fill is sampled at
            // arbitrary points of the arrival sawtooth.
            let margin = (margin_us as f64 * self.rate as f64 / 1e6) as u32 + frames as u32;
            self.shared.margin_frames.store(margin, Relaxed);
        }

        self.write(payload, gap > 0 || restart);
    }

    fn write(&mut self, payload: &[u8], fade_in: bool) {
        let ch = self.ch;
        let wanted = payload.len() / 2;
        let n = wanted.min(self.prod.slots()) / ch * ch;
        if n < wanted {
            self.stats.overflows += 1;
        }
        let Ok(mut chunk) = self.prod.write_chunk(n) else { return };
        let (a, b) = chunk.as_mut_slices();
        let samples = payload
            .chunks_exact(2)
            .map(|s| i16::from_le_bytes([s[0], s[1]]) as f32 * (1.0 / 32768.0));
        for (i, (slot, mut v)) in a.iter_mut().chain(b.iter_mut()).zip(samples).enumerate() {
            let frame = i / ch;
            if fade_in && frame < FADE_FRAMES {
                v *= frame as f32 / FADE_FRAMES as f32;
            }
            self.last[i % ch] = v;
            *slot = v;
        }
        chunk.commit_all();
    }

    /// Stands in for `frames` lost frames: a quick decay from the last good
    /// sample to silence, so the hole is a dip rather than a click.
    fn conceal(&mut self, frames: usize) {
        let ch = self.ch;
        let n = (frames * ch).min(self.prod.slots()) / ch * ch;
        let Ok(mut chunk) = self.prod.write_chunk(n) else { return };
        let (a, b) = chunk.as_mut_slices();
        for (i, slot) in a.iter_mut().chain(b.iter_mut()).enumerate() {
            let frame = i / ch;
            *slot = if frame < FADE_FRAMES {
                self.last[i % ch] * (1.0 - frame as f32 / FADE_FRAMES as f32)
            } else {
                0.0
            };
        }
        chunk.commit_all();
        self.last = [0.0; 2];
    }
}

/// Estimates how late packets can be relative to the best case.
///
/// Each packet's arrival time is compared with its timestamp; the smallest
/// such offset seen recently is "on time", and the worst excess over it in
/// the last several seconds is the jitter the buffer has to cover. Both are
/// windowed so the estimate follows changing conditions and a slow clock
/// drift between the two machines does not read as jitter.
struct Jitter {
    ts_ext: i64,
    last_ts: Option<u32>,
    bucket_start_us: i64,
    mins: [i64; Self::MIN_BUCKETS],
    maxes: [u32; Self::MAX_BUCKETS],
    cursor: usize,
}

impl Jitter {
    const BUCKET_US: i64 = 1_000_000;
    const MIN_BUCKETS: usize = 4;
    const MAX_BUCKETS: usize = 12;

    fn new() -> Self {
        Jitter {
            ts_ext: 0,
            last_ts: None,
            bucket_start_us: 0,
            mins: [i64::MAX; Self::MIN_BUCKETS],
            maxes: [0; Self::MAX_BUCKETS],
            cursor: 0,
        }
    }

    fn observe(&mut self, now_us: i64, ts: u32, rate: u32) -> u32 {
        match self.last_ts {
            None => self.bucket_start_us = now_us,
            Some(last) => self.ts_ext += ts.wrapping_sub(last) as i32 as i64,
        }
        self.last_ts = Some(ts);

        while now_us - self.bucket_start_us >= Self::BUCKET_US {
            self.bucket_start_us += Self::BUCKET_US;
            self.cursor += 1;
            self.mins[self.cursor % Self::MIN_BUCKETS] = i64::MAX;
            self.maxes[self.cursor % Self::MAX_BUCKETS] = 0;
        }

        let offset = now_us - self.ts_ext * 1_000_000 / rate as i64;
        let min = &mut self.mins[self.cursor % Self::MIN_BUCKETS];
        *min = (*min).min(offset);
        let base = self.mins.iter().copied().min().unwrap();
        let excess = (offset - base).clamp(0, u32::MAX as i64) as u32;
        let max = &mut self.maxes[self.cursor % Self::MAX_BUCKETS];
        *max = (*max).max(excess);
        self.maxes.iter().copied().max().unwrap()
    }
}

pub struct PlayoutReader {
    cons: Consumer<f32>,
    ch: usize,
    shared: Arc<Shared>,
    stream_rate: f64,
    device_rate: f64,
    /// Input frames per output frame before drift correction.
    base_ratio: f64,
    /// Position between `taps[1]` and `taps[2]`, in input frames.
    phase: f64,
    taps: [[f32; 2]; 4],
    playing: bool,
    gain: f32,
    gain_step: f32,
    fade_step: f32,
    /// Frames to drop once the current fade-out reaches silence.
    pending_skip: usize,
    fill_avg: f64,
    /// Input frames one callback consumes, peak-held across varying sizes.
    need: f64,
    last: [f32; 2],
}

impl PlayoutReader {
    /// Fills `out` (interleaved, `out_ch` channels) with the next audio, or
    /// with silence while the buffer is (re)filling. Never blocks.
    pub fn render(&mut self, out: &mut [f32], out_ch: usize) {
        let n_out = out.len() / out_ch.max(1);
        if n_out == 0 {
            return;
        }
        let ch = self.ch;
        let need = n_out as f64 * self.base_ratio;
        if need > self.need {
            self.need = need;
        } else {
            self.need += (need - self.need) * 0.01;
        }
        let margin = self.shared.margin_frames.load(Relaxed);
        let mut avail = self.cons.slots() / ch;

        if !self.playing {
            // Hold until there is a full margin plus this callback's worth,
            // rather than starting early and stuttering on the edge.
            let start = margin as usize + self.need.ceil() as usize + 4;
            if margin == NOT_READY || avail < start {
                self.silence(out, out_ch, 0);
                return;
            }
            // Anything beyond that is latency we never need to take on.
            if let Ok(chunk) = self.cons.read_chunk((avail - start) * ch) {
                chunk.commit_all();
            }
            avail = start;
            self.playing = true;
            self.shared.playing.store(true, Relaxed);
            self.taps = [[0.0; 2]; 4];
            self.phase = 0.0;
            self.gain = 0.0;
            self.gain_step = self.fade_step;
            self.pending_skip = 0;
            self.fill_avg = start as f64;
        }

        let target = margin as f64 + self.need;
        let dt = n_out as f64 / self.device_rate;
        self.fill_avg += (avail as f64 - self.fill_avg) * (1.0 - (-dt / FILL_TAU_S).exp());
        let mut excess = self.fill_avg - target;
        if excess > target.max(0.030 * self.stream_rate) && self.pending_skip == 0 {
            // Far too full (the sender stalled and then caught up, or the
            // margin dropped). Resampling would take many seconds to work
            // that off; fade out, jump, fade back in.
            self.pending_skip = (avail as f64 - target).max(0.0) as usize;
            self.gain_step = -self.fade_step;
            self.fill_avg = target;
            self.shared.skips.fetch_add(1, Relaxed);
            excess = 0.0;
        }
        let drift = (excess / self.stream_rate * DRIFT_GAIN).clamp(-MAX_DRIFT, MAX_DRIFT);
        let ratio = self.base_ratio * (1.0 + drift);

        let chunk = self.cons.read_chunk(avail * ch).expect("slots() frames are readable");
        let (a, b) = chunk.as_slices();
        let mut idx = 0;
        let mut produced = 0;
        let mut peak = 0.0f32;
        'frames: while produced < n_out {
            while self.phase >= 1.0 {
                if idx >= avail {
                    break 'frames;
                }
                let at = idx * ch;
                let src = if at < a.len() { &a[at..] } else { &b[at - a.len()..] };
                self.taps.rotate_left(1);
                for c in 0..ch {
                    self.taps[3][c] = src[c];
                }
                idx += 1;
                self.phase -= 1.0;
            }
            let t = self.phase as f32;
            let mut frame = [0.0f32; 2];
            for c in 0..ch {
                let [y0, y1, y2, y3] = [self.taps[0][c], self.taps[1][c], self.taps[2][c], self.taps[3][c]];
                // Catmull-Rom: cheap, and unlike linear interpolation does
                // not audibly dull the top end as the phase sweeps.
                let c1 = 0.5 * (y2 - y0);
                let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
                let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
                frame[c] = (((c3 * t + c2) * t + c1) * t + y1) * self.gain;
                peak = peak.max(frame[c].abs());
            }
            if self.gain_step != 0.0 {
                self.gain += self.gain_step;
                if self.gain >= 1.0 {
                    self.gain = 1.0;
                    self.gain_step = 0.0;
                } else if self.gain <= 0.0 {
                    self.gain = 0.0;
                    idx = (idx + self.pending_skip).min(avail);
                    self.pending_skip = 0;
                    self.gain_step = self.fade_step;
                }
            }
            write_frame(&mut out[produced * out_ch..][..out_ch], frame, ch);
            self.last = frame;
            produced += 1;
            self.phase += ratio;
        }
        chunk.commit(idx * ch);

        if produced < n_out {
            self.playing = false;
            self.shared.playing.store(false, Relaxed);
            self.shared.underruns.fetch_add(1, Relaxed);
            self.silence(out, out_ch, produced);
        }
        let waiting = (self.fill_avg - self.need / 2.0).max(0.0) / self.stream_rate;
        self.shared.buffer_us.store((waiting * 1e6) as u32, Relaxed);
        self.shared.drift_ppm.store((drift * 1e6) as i32, Relaxed);
        self.shared.peak.fetch_max(peak.to_bits(), Relaxed);
    }

    /// Writes silence from frame `from` on, easing out of the last sample
    /// played instead of stepping to zero.
    fn silence(&mut self, out: &mut [f32], out_ch: usize, from: usize) {
        for frame in out[from * out_ch..].chunks_exact_mut(out_ch) {
            for v in &mut self.last {
                *v *= 0.95;
                if v.abs() < 1e-5 {
                    *v = 0.0;
                }
            }
            write_frame(frame, self.last, self.ch);
        }
    }
}

/// Maps a mono or stereo frame onto however many channels the device has.
#[inline]
fn write_frame(out: &mut [f32], frame: [f32; 2], ch: usize) {
    match (ch, out.len()) {
        (_, 0) => {}
        (1, _) => {
            let n = out.len().min(2);
            out[..n].fill(frame[0]);
            out[n..].fill(0.0);
        }
        (_, 1) => out[0] = 0.5 * (frame[0] + frame[1]),
        _ => {
            out[0] = frame[0];
            out[1] = frame[1];
            out[2..].fill(0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;
    const PACKET: usize = 120;

    fn sine_packet(start: u64, frames: usize, ch: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(frames * ch * 2);
        for i in 0..frames {
            let phase = (start + i as u64) as f64 * 440.0 / RATE as f64 * std::f64::consts::TAU;
            let s = (phase.sin() * 0.5 * 32767.0) as i16;
            for _ in 0..ch {
                out.extend_from_slice(&s.to_le_bytes());
            }
        }
        out
    }

    /// Drives a writer and a reader on simulated clocks.
    struct Sim {
        writer: PlayoutWriter,
        reader: PlayoutReader,
        shared: Arc<Shared>,
        /// Sender frames per receiver frame; 1.0 means perfectly matched clocks.
        clock_skew: f64,
        period: usize,
        /// Delay added to each packet's arrival, microseconds. Delivery stays
        /// in order, as on a real link: a held-up packet holds up those behind.
        delay: Box<dyn FnMut(u64) -> i64>,
        last_arrival: i64,
        drop: Box<dyn FnMut(u64) -> bool>,
        now_us: f64,
        sent: u64,
        output: Vec<f32>,
    }

    impl Sim {
        fn new(mode: BufferMode) -> Sim {
            let fmt = StreamFormat { sample_rate: RATE, channels: 1 };
            let (writer, reader, shared) = playout(fmt, fmt, mode);
            Sim {
                writer,
                reader,
                shared,
                clock_skew: 1.0,
                period: 256,
                delay: Box::new(|_| 0),
                last_arrival: 0,
                drop: Box::new(|_| false),
                now_us: 0.0,
                sent: 0,
                output: Vec::new(),
            }
        }

        fn run(&mut self, seconds: f64) {
            let period_us = self.period as f64 / RATE as f64 * 1e6;
            let packet_us = PACKET as f64 / (RATE as f64 * self.clock_skew) * 1e6;
            let end = self.now_us + seconds * 1e6;
            let mut pending: Vec<(i64, u64)> = Vec::new();
            let mut next_callback = self.now_us;
            while next_callback < end {
                // Packets whose (delayed) arrival falls before this callback.
                while (self.sent as f64 + 1.0) * packet_us <= next_callback {
                    let n = self.sent;
                    self.sent += 1;
                    if !(self.drop)(n) {
                        let at = ((n + 1) as f64 * packet_us) as i64 + (self.delay)(n);
                        self.last_arrival = self.last_arrival.max(at);
                        pending.push((self.last_arrival, n));
                    }
                }
                pending.sort();
                while pending.first().is_some_and(|p| p.0 as f64 <= next_callback) {
                    let (at, n) = pending.remove(0);
                    let start = n * PACKET as u64;
                    self.writer.push(start as u32, &sine_packet(start, PACKET, 1), at);
                }
                let mut buf = vec![0.0f32; self.period];
                self.reader.render(&mut buf, 1);
                self.output.extend_from_slice(&buf);
                next_callback += period_us;
            }
            self.now_us = next_callback;
        }

        /// Largest sample-to-sample step in the output so far. A clean
        /// 440 Hz sine at half scale never steps by more than about 0.03.
        fn max_step(&self) -> f32 {
            self.output.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max)
        }
    }

    #[test]
    fn steady_stream_plays_without_underruns() {
        let mut sim = Sim::new(BufferMode::default());
        sim.run(5.0);
        assert!(sim.shared.playing());
        assert_eq!(sim.shared.underruns(), 0);
        assert_eq!(sim.shared.skips(), 0);
        assert!(sim.max_step() < 0.05, "discontinuity: {}", sim.max_step());
        // A clean link should settle on a buffer of a few milliseconds.
        let buffer_ms = sim.shared.buffer_us() as f32 / 1000.0;
        assert!((1.0..12.0).contains(&buffer_ms), "buffer {buffer_ms} ms");
    }

    #[test]
    fn clock_drift_is_absorbed_in_both_directions() {
        for skew in [1.0 + 300e-6, 1.0 - 300e-6] {
            let mut sim = Sim::new(BufferMode::default());
            sim.clock_skew = skew;
            // 300 ppm over 60 s is 18 ms: far more than the buffer holds, so
            // this only passes if the resampler is tracking.
            sim.run(60.0);
            assert_eq!(sim.shared.underruns(), 0, "skew {skew}");
            assert_eq!(sim.shared.skips(), 0, "skew {skew}");
            let ppm = sim.shared.drift_ppm() as f64;
            let want = (skew - 1.0) * 1e6;
            assert!((ppm - want).abs() < 120.0, "correction {ppm} ppm for skew {want} ppm");
            assert!(sim.max_step() < 0.05);
        }
    }

    #[test]
    fn margin_grows_to_cover_jitter() {
        let mut sim = Sim::new(BufferMode::default());
        // Every eighth packet (20 ms apart) is held 12 ms, and holds up the
        // four behind it; the rest are on time.
        sim.delay = Box::new(|n| if n % 8 == 0 { 12_000 } else { 0 });
        sim.run(3.0);
        let before = sim.shared.underruns();
        sim.run(10.0);
        assert_eq!(sim.shared.underruns(), before, "still underrunning after adapting");
        let stats = sim.writer.stats();
        assert!(stats.jitter_us >= 11_000, "jitter {} us", stats.jitter_us);
        assert!(sim.shared.buffer_us() >= 12_000);
    }

    #[test]
    fn margin_shrinks_again_when_jitter_stops() {
        let mut sim = Sim::new(BufferMode::default());
        let calm_after = (5.0 * RATE as f64 / PACKET as f64) as u64;
        sim.delay = Box::new(move |n| if n < calm_after && n % 16 == 0 { 20_000 } else { 0 });
        sim.run(5.0);
        assert!(sim.shared.buffer_us() >= 15_000);
        sim.run(30.0);
        let buffer_ms = sim.shared.buffer_us() as f32 / 1000.0;
        assert!(buffer_ms < 12.0, "buffer stuck at {buffer_ms} ms");
    }

    #[test]
    fn lost_packets_are_concealed_without_losing_position() {
        let mut sim = Sim::new(BufferMode::default());
        sim.drop = Box::new(|n| n > 400 && n % 97 == 0);
        sim.run(10.0);
        assert!(sim.writer.stats().lost >= 30);
        // Loss alone must not drain the buffer: the gap is filled in place.
        assert_eq!(sim.shared.underruns(), 0);
        assert_eq!(sim.shared.skips(), 0);
    }

    #[test]
    fn late_and_duplicate_packets_are_dropped() {
        let fmt = StreamFormat { sample_rate: RATE, channels: 1 };
        let (mut w, _r, _s) = playout(fmt, fmt, BufferMode::Fixed { ms: 5.0 });
        let p = sine_packet(0, PACKET, 1);
        w.push(0, &p, 0);
        w.push(240, &p, 5_000); // 120 lost
        w.push(120, &p, 6_000); // the lost one, too late
        w.push(240, &p, 7_000); // duplicate
        let stats = w.stats();
        assert_eq!((stats.packets, stats.lost, stats.late), (2, 1, 2));
    }

    #[test]
    fn a_stall_recovers_to_the_target_instead_of_staying_late() {
        let mut sim = Sim::new(BufferMode::Fixed { ms: 5.0 });
        sim.run(2.0);
        // The network holds everything for 150 ms, then delivers it at once.
        let from = sim.sent;
        let stall = (0.150 * RATE as f64 / PACKET as f64) as u64;
        let packet_us = (PACKET as f64 / RATE as f64 * 1e6) as i64;
        sim.delay = Box::new(move |n| {
            if n >= from && n < from + stall { (from + stall - n) as i64 * packet_us } else { 0 }
        });
        sim.run(3.0);
        assert!(sim.shared.underruns() >= 1);
        let buffer_ms = sim.shared.buffer_us() as f32 / 1000.0;
        assert!(buffer_ms < 15.0, "latency stuck at {buffer_ms} ms after a stall");
    }

    #[test]
    fn converts_sample_rate_and_channel_count() {
        let stream = StreamFormat { sample_rate: 44_100, channels: 2 };
        let device = StreamFormat { sample_rate: 48_000, channels: 2 };
        let (mut w, mut r, shared) = playout(stream, device, BufferMode::Fixed { ms: 5.0 });
        let mut ts = 0u32;
        let mut out = Vec::new();
        // 1 s of device time; feed just ahead of what is consumed.
        for cb in 0..375 {
            let fed_until = ((cb + 1) as f64 * 128.0 * 44_100.0 / 48_000.0) as u32 + 441;
            while ts < fed_until {
                let mut p = Vec::new();
                for i in 0..147u32 {
                    let x = ((ts + i) as f64 * 1000.0 / 44_100.0 * std::f64::consts::TAU).sin();
                    p.extend_from_slice(&((x * 16000.0) as i16).to_le_bytes());
                    p.extend_from_slice(&0i16.to_le_bytes());
                }
                w.push(ts, &p, cb as i64 * 2667);
                ts += 147;
            }
            let mut buf = [0.0f32; 256];
            r.render(&mut buf, 2);
            out.extend_from_slice(&buf);
        }
        assert_eq!(shared.underruns(), 0);
        // Left carries a 1 kHz tone at the device rate; right stays silent.
        let left: Vec<f32> = out.chunks(2).map(|f| f[0]).skip(24_000).collect();
        let crossings = left.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
        assert!((495..=505).contains(&crossings), "{crossings} cycles in 0.5 s");
        assert!(out.chunks(2).all(|f| f[1] == 0.0));
    }
}
