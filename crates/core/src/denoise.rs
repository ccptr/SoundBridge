//! Voice-only filtering for a microphone: what a phone lying on a desk picks
//! up besides speech (keys, mouse clicks, breathing, fans) is removed before
//! it is sent.
//!
//! Two stages, because neither is enough alone. RNNoise (the `nnnoiseless`
//! port) subtracts noise from underneath speech, but lets a good part of a
//! sharp click through. A gate then closes completely between phrases, which
//! is where nearly all of a keyboard is heard. The gate opens only for frames
//! that RNNoise rates as speech *and* that have a pitch: its rating alone is
//! fooled by breathing, and pitch alone by anything that hums.
//!
//! The cost is delay. Audio is handled in 10 ms frames, RNNoise's output
//! trails its input by one, and the gate looks some frames ahead so it is
//! already open for the consonant in front of a vowel. How far it looks is
//! the trade a [`Mode`] makes: the further, the less of a word's start is
//! lost after a pause, and the later the stream.

use nnnoiseless::DenoiseState;

/// The only sample rate the model was trained for.
pub const RATE: u32 = 48_000;
pub const FRAME: usize = DenoiseState::FRAME_SIZE;

/// How much is taken out of a microphone, from nothing to all but speech.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Mode {
    #[default]
    Off,
    /// RNNoise alone, with no gate: steady noise goes, nothing is ever cut
    /// off, and sharp sounds come through in part. 10 to 20 ms of delay.
    Reduce,
    /// RNNoise and the gate: silent between phrases.
    Gate(Gate),
}

/// The gate's tuning: what it trades between cutting into speech, letting
/// noise through around it, and delay.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gate {
    /// How far the gate looks ahead, so that it is already open for what
    /// comes before the voiced sound that opens it: words often start
    /// unvoiced ("s", "f", "t"), and a voice takes a few frames to be
    /// recognized after silence. The stream is this much later for it. In
    /// steps of 10 ms, up to [`Gate::MAX_LOOKAHEAD_MS`].
    pub lookahead_ms: u32,
    /// How long the gate stays open after the last voiced sound, covering
    /// unvoiced consonants and the gaps between syllables. Whatever noise
    /// follows a phrase is heard for as long. Up to [`Gate::MAX_HOLD_MS`].
    pub hold_ms: u32,
    /// What a frame needs to count as voiced, 0..1: both RNNoise's speech
    /// probability and the pitch strength (see [`VoiceFilter::pitch`]).
    /// Vowels sit well above 0.5, noise of any colour well below. Lower
    /// opens for a quieter voice, and for more that is not one.
    pub threshold: f32,
}

impl Gate {
    pub const MAX_LOOKAHEAD_MS: u32 = (MAX_LOOKAHEAD * FRAME_MS as usize) as u32;
    pub const MAX_HOLD_MS: u32 = 2000;
    pub const THRESHOLDS: std::ops::RangeInclusive<f32> = 0.05..=0.95;

    /// The least delay: 50 to 60 ms. A word that starts unvoiced after a
    /// pause can lose its first sound.
    pub const VOICE: Gate = Gate { lookahead_ms: 40, hold_ms: 250, threshold: 0.5 };
    /// Looks far enough ahead for a drawn-out "s" or "sh" in front of the
    /// first vowel: 130 to 140 ms of delay.
    pub const WHOLE_WORDS: Gate = Gate { lookahead_ms: 120, ..Gate::VOICE };

    fn lookahead(self) -> usize {
        (self.lookahead_ms / FRAME_MS) as usize
    }
}

impl Mode {
    pub const VOICE: Mode = Mode::Gate(Gate::VOICE);
    pub const WHOLE_WORDS: Mode = Mode::Gate(Gate::WHOLE_WORDS);
    /// The ready-made choices, by the name each goes by on the command line.
    pub const PRESETS: [(&str, Mode); 4] =
        [("off", Mode::Off), ("reduce", Mode::Reduce), ("voice", Mode::VOICE), ("whole", Mode::WHOLE_WORDS)];

    pub fn is_on(self) -> bool {
        self != Mode::Off
    }

    /// Whether the stream is silent between phrases.
    pub fn gated(self) -> bool {
        matches!(self, Mode::Gate(_))
    }

    /// The gate's tuning brought within what the filter can do.
    pub fn clamped(self) -> Mode {
        match self {
            Mode::Gate(gate) => Mode::Gate(Gate {
                lookahead_ms: gate.lookahead_ms.min(Gate::MAX_LOOKAHEAD_MS) / FRAME_MS * FRAME_MS,
                hold_ms: gate.hold_ms.clamp(FRAME_MS, Gate::MAX_HOLD_MS),
                threshold: if gate.threshold.is_nan() {
                    Gate::VOICE.threshold
                } else {
                    gate.threshold.clamp(*Gate::THRESHOLDS.start(), *Gate::THRESHOLDS.end())
                },
            }),
            other => other,
        }
    }

    /// The least a stream filtered this way trails an unfiltered one, ms; it
    /// varies by up to a frame more.
    pub fn delay_ms(self) -> u32 {
        match self.clamped() {
            Mode::Off => 0,
            Mode::Reduce => FRAME_MS,
            Mode::Gate(gate) => gate.lookahead_ms + FRAME_MS,
        }
    }
}

/// `off`, `reduce`, or `gate:LOOKAHEAD:HOLD:THRESHOLD`.
impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Mode::Off => f.write_str("off"),
            Mode::Reduce => f.write_str("reduce"),
            Mode::Gate(gate) => write!(f, "gate:{}:{}:{}", gate.lookahead_ms, gate.hold_ms, gate.threshold),
        }
    }
}

/// Reads what [`Mode`] prints, and the names of the [`Mode::PRESETS`].
impl std::str::FromStr for Mode {
    type Err = String;

    fn from_str(s: &str) -> Result<Mode, String> {
        if let Some(&(_, mode)) = Mode::PRESETS.iter().find(|(name, _)| *name == s) {
            return Ok(mode);
        }
        let gate = || {
            let mut fields = s.strip_prefix("gate:")?.split(':');
            let gate = Gate {
                lookahead_ms: fields.next()?.parse().ok()?,
                hold_ms: fields.next()?.parse().ok()?,
                threshold: fields.next()?.parse().ok()?,
            };
            fields.next().is_none().then_some(Mode::Gate(gate).clamped())
        };
        gate().ok_or_else(|| {
            let names: Vec<_> = Mode::PRESETS.iter().map(|(name, _)| *name).collect();
            format!("expected one of {}", names.join(", "))
        })
    }
}

const FRAME_MS: u32 = (FRAME as u32 * 1000) / RATE;
/// The most frames the output can be held back.
const MAX_LOOKAHEAD: usize = 20;
/// A click or a thump can pass for a voiced frame or two; speech keeps it up.
const OPEN_FRAMES: u32 = 3;

/// RNNoise works on samples scaled like 16-bit PCM.
const SCALE: f32 = 32_768.0;

/// Pitch is searched at a quarter of the sample rate, which is plenty for
/// the fundamental and sixteen times cheaper.
const DECIMATE: usize = 4;
const PITCH_FRAME: usize = FRAME / DECIMATE;
/// 20 ms. Longer would rate pitch more surely, but notice a vowel later.
const PITCH_WINDOW: usize = 2 * PITCH_FRAME;
/// Lags for 400 Hz down to 80 Hz.
const MIN_LAG: usize = RATE as usize / DECIMATE / 400;
const MAX_LAG: usize = RATE as usize / DECIMATE / 80;

pub struct VoiceFilter {
    mode: Mode,
    net: Box<DenoiseState<'static>>,
    input: [f32; FRAME],
    filled: usize,
    /// Denoised frames waiting out the look-ahead, in the first `lookahead`
    /// places; `at` is the oldest.
    held: [[f32; FRAME]; MAX_LOOKAHEAD],
    lookahead: usize,
    at: usize,
    out: [f32; FRAME],
    recent: [f32; PITCH_WINDOW],
    /// Consecutive voiced frames.
    run: u32,
    /// Frames the gate stays open after the last voiced one.
    hold_frames: u32,
    /// Frames left before the gate closes; 0 is closed.
    hold: u32,
    gain: f32,
}

impl VoiceFilter {
    /// [`Mode::Off`] is not a filter; asked for one, this reduces noise.
    pub fn new(mode: Mode) -> Self {
        let mode = mode.clamped();
        let gate = match mode {
            Mode::Gate(gate) => Some(gate),
            _ => None,
        };
        VoiceFilter {
            mode,
            net: DenoiseState::new(),
            input: [0.0; FRAME],
            filled: 0,
            held: [[0.0; FRAME]; MAX_LOOKAHEAD],
            lookahead: gate.map_or(0, Gate::lookahead),
            at: 0,
            out: [0.0; FRAME],
            recent: [0.0; PITCH_WINDOW],
            run: 0,
            hold_frames: gate.map_or(0, |gate| gate.hold_ms / FRAME_MS),
            hold: 0,
            gain: 0.0,
        }
    }

    /// Takes one mono sample at [`RATE`]. Every [`FRAME`]th call completes a
    /// frame and returns one frame of filtered audio.
    ///
    /// Made for the capture callback: it neither allocates nor locks, except
    /// that the first frame on a given thread sets up RNNoise's FFT there.
    /// That lands at the very start of a stream, before there is anything to
    /// interrupt.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn push(&mut self, sample: f32) -> Option<&[f32; FRAME]> {
        self.input[self.filled] = sample * SCALE;
        self.filled += 1;
        if self.filled < FRAME {
            return None;
        }
        self.filled = 0;

        let speech = if self.lookahead == 0 {
            self.net.process_frame(&mut self.out, &self.input)
        } else {
            // The frame leaving the look-ahead goes out; the new one takes its place.
            self.out = self.held[self.at];
            let speech = self.net.process_frame(&mut self.held[self.at], &self.input);
            self.at = (self.at + 1) % self.lookahead;
            speech
        };
        let Mode::Gate(gate) = self.mode else {
            for v in &mut self.out {
                *v *= 1.0 / SCALE;
            }
            return Some(&self.out);
        };

        let voiced = speech >= gate.threshold && self.pitch() >= gate.threshold;
        self.run = if voiced { self.run + 1 } else { 0 };
        if self.run >= OPEN_FRAMES || (self.hold > 0 && voiced) {
            self.hold = self.hold_frames;
        } else {
            self.hold = self.hold.saturating_sub(1);
        }

        let target = if self.hold > 0 { 1.0 } else { 0.0 };
        let step = (target - self.gain) / FRAME as f32;
        for v in &mut self.out {
            self.gain += step;
            *v *= self.gain * (1.0 / SCALE);
        }
        self.gain = target;
        Some(&self.out)
    }

    /// How periodic the last 20 ms of input are, 0..=1: the best correlation
    /// of the signal with itself one pitch period later. It is normalized by
    /// the mean of the two energies rather than their geometric mean, so that
    /// a thump ringing down (a key bottoming out, a click through the desk)
    /// does not score like a held note.
    fn pitch(&mut self) -> f32 {
        self.recent.copy_within(PITCH_FRAME.., 0);
        let fresh = &mut self.recent[PITCH_WINDOW - PITCH_FRAME..];
        for (slot, block) in fresh.iter_mut().zip(self.input.chunks_exact(DECIMATE)) {
            *slot = block.iter().sum::<f32>() * (1.0 / DECIMATE as f32);
        }
        let x = &self.recent;
        let mut best = 0.0f32;
        for lag in MIN_LAG..=MAX_LAG {
            let (a, b) = (&x[..PITCH_WINDOW - lag], &x[lag..]);
            let (mut ab, mut aa, mut bb) = (0.0f32, 0.0f32, 0.0f32);
            for (p, q) in a.iter().zip(b) {
                ab += p * q;
                aa += p * p;
                bb += q * q;
            }
            // The floor keeps digital silence and near-silence from scoring.
            best = best.max(2.0 * ab / (aa + bb).max(1e3));
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::{PI, TAU};

    const SECOND: usize = RATE as usize;

    struct Noise(u64);

    impl Noise {
        /// White noise in -1..1.
        fn next(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 40) as f32 / (1u64 << 23) as f32 - 1.0
        }
    }

    /// A stand-in for speech: a pulse train near 110 Hz through three vowel
    /// formants, in syllables, saying something from `from` to `to`.
    fn voice(len: usize, from: usize, to: usize) -> Vec<f32> {
        let formants = [(650.0f32, 80.0f32), (1100.0, 90.0), (2500.0, 120.0)];
        let mut state = [[0.0f32; 2]; 3];
        let mut phase = 0.0f32;
        (0..len)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                phase += (110.0 + 25.0 * (t * 1.3).sin()) / RATE as f32;
                let pulse = if phase >= 1.0 { 1.0 } else { 0.0 };
                phase %= 1.0;
                let mut y = 0.0;
                for (s, (freq, width)) in state.iter_mut().zip(formants) {
                    let r = (-PI * width / RATE as f32).exp();
                    let out = pulse + 2.0 * r * (TAU * freq / RATE as f32).cos() * s[0] - r * r * s[1];
                    *s = [out, s[0]];
                    y += out;
                }
                let syllable = 0.6 + 0.4 * (t * 4.0 * TAU).sin();
                if (from..to).contains(&i) { 0.015 * syllable * y } else { 0.0 }
            })
            .collect()
    }

    /// Key or mouse clicks: a burst of noise `burst` samples long with the
    /// thump it sends through the desk, every `every` samples or so.
    fn clicks(len: usize, every: usize, burst: usize, noise: &mut Noise) -> Vec<f32> {
        let mut out = vec![0.0f32; len];
        let mut at = every / 3;
        while at + 2000 < len {
            for i in 0..burst {
                out[at + i] += 0.4 * (-5.0 * i as f32 / burst as f32).exp() * noise.next();
            }
            for i in 0..1500 {
                out[at + i] += 0.25 * (-(i as f32) / 300.0).exp() * (i as f32 * 90.0 / RATE as f32 * TAU).sin();
            }
            at += every + (noise.next() * every as f32 * 0.4) as usize;
        }
        out
    }

    /// Breathing: band-limited noise that swells and fades.
    fn breath(len: usize, noise: &mut Noise) -> Vec<f32> {
        let (mut low, mut lower) = (0.0f32, 0.0f32);
        (0..len)
            .map(|i| {
                let x = noise.next();
                low += 0.12 * (x - low);
                lower += 0.02 * (x - lower);
                let swell = (i as f32 / RATE as f32 * 0.35 * TAU).sin().max(0.0).powi(2);
                0.3 * swell * (low - lower)
            })
            .collect()
    }

    /// The hiss of an "s": noise with the lows taken out, from `from` to `to`.
    fn sibilant(len: usize, from: usize, to: usize, noise: &mut Noise) -> Vec<f32> {
        let mut low = 0.0f32;
        (0..len)
            .map(|i| {
                let x = noise.next();
                low += 0.5 * (x - low);
                if (from..to).contains(&i) { 0.05 * (x - low) } else { 0.0 }
            })
            .collect()
    }

    /// How far the output of a gated `mode` trails its input.
    fn delay(mode: Mode) -> usize {
        (mode.delay_ms() * RATE / 1000) as usize
    }

    fn filter(mode: Mode, input: &[f32]) -> Vec<f32> {
        let mut filter = VoiceFilter::new(mode);
        let mut out = Vec::with_capacity(input.len());
        for &sample in input {
            if let Some(frame) = filter.push(sample) {
                out.extend_from_slice(frame);
            }
        }
        out
    }

    fn energy(samples: &[f32]) -> f32 {
        samples.iter().map(|v| v * v).sum()
    }

    #[test]
    fn noise_alone_never_opens_the_gate() {
        let mut noise = Noise(0x1234_5678_9abc_def1);
        let len = 8 * SECOND;
        let hiss: Vec<f32> = (0..len).map(|_| 0.01 * noise.next()).collect();
        for (what, input) in [
            ("typing", clicks(len, 6000, 900, &mut noise)),
            ("mouse clicks", clicks(len, 14_000, 150, &mut noise)),
            ("breathing", breath(len, &mut noise)),
            ("hiss", hiss),
        ] {
            for mode in [Mode::VOICE, Mode::WHOLE_WORDS] {
                let out = filter(mode, &input);
                assert!(out.iter().all(|&v| v == 0.0), "{what} got through {mode:?}");
            }
        }
    }

    #[test]
    fn speech_passes_and_the_noise_around_it_does_not() {
        let mut noise = Noise(0x0dd_ba11);
        let len = 6 * SECOND;
        let (from, to) = (2 * SECOND, 4 * SECOND);
        let speech = voice(len, from, to);
        let keys = clicks(len, 6000, 900, &mut noise);
        let input: Vec<f32> = speech.iter().zip(&keys).map(|(a, b)| a + 0.2 * b).collect();
        let out = filter(Mode::VOICE, &input);

        let delay = delay(Mode::VOICE);
        let spoken = &out[from + delay + SECOND / 10..to + delay];
        let kept = energy(spoken) / energy(&speech[from + SECOND / 10..to]);
        assert!((0.5..2.0).contains(&kept), "speech came out at {kept} of its energy");
        // Typing before the phrase, and from soon after it, is gone entirely.
        let hold = (Gate::VOICE.hold_ms * RATE / 1000) as usize;
        assert!(out[..from].iter().all(|&v| v == 0.0));
        assert!(out[to + delay + hold + FRAME..].iter().all(|&v| v == 0.0));
    }

    #[test]
    fn the_gate_is_open_before_the_sound_that_opened_it() {
        let onset = SECOND + 100;
        let out = filter(Mode::VOICE, &voice(3 * SECOND, onset, 2 * SECOND));
        let first = out.iter().position(|&v| v != 0.0).expect("the gate never opened");
        // In input time: what was heard starts ahead of the first voiced frame.
        let delay = delay(Mode::VOICE);
        assert!(first < onset + delay, "the first {} samples were cut off", first - (onset + delay));
    }

    #[test]
    fn a_longer_look_ahead_keeps_the_consonant_a_word_starts_with() {
        let mut noise = Noise(0x5eed_5eed);
        let len = 3 * SECOND;
        // 90 ms of "s", then the vowel.
        let (onset, vowel) = (SECOND + 100, SECOND + 100 + 9 * FRAME);
        let hiss = sibilant(len, onset, vowel, &mut noise);
        let input: Vec<f32> = voice(len, vowel, 2 * SECOND).iter().zip(&hiss).map(|(a, b)| a + b).collect();
        // Where what is heard starts, in input time, relative to the "s".
        let lost = |mode| {
            let out = filter(mode, &input);
            let first = out.iter().position(|&v| v != 0.0).expect("the gate never opened");
            first as isize - (onset + delay(mode)) as isize
        };
        let (short, long) = (lost(Mode::VOICE), lost(Mode::WHOLE_WORDS));
        assert!(short > 4 * FRAME as isize, "the short look-ahead lost only {short} samples");
        assert!(long <= 0, "the first {long} samples were cut off");
    }

    #[test]
    fn reducing_noise_never_cuts_anything_off() {
        let mut noise = Noise(0xfeed_f00d);
        let len = 6 * SECOND;
        let (from, to) = (2 * SECOND, 4 * SECOND);
        let speech = voice(len, from, to);
        // A fan: noise with most of it in the lows.
        let mut low = 0.0f32;
        let fan: Vec<f32> = (0..len)
            .map(|_| {
                low += 0.05 * (noise.next() - low);
                0.05 * low
            })
            .collect();
        let input: Vec<f32> = speech.iter().zip(&fan).map(|(a, b)| a + b).collect();
        let out = filter(Mode::Reduce, &input);

        // One frame late, with no look-ahead to wait out.
        let spoken = &out[from + FRAME..to + FRAME];
        let kept = energy(spoken) / energy(&speech[from..to]);
        assert!((0.5..2.0).contains(&kept), "speech came out at {kept} of its energy");
        // The stream is never shut, and the fan between phrases is far quieter.
        assert!(out[SECOND..from].iter().any(|&v| v != 0.0));
        let left = energy(&out[SECOND..from]) / energy(&fan[SECOND..from]);
        assert!(left < 0.01, "{left} of the fan is left");
    }

    #[test]
    fn a_shorter_hold_closes_the_gate_sooner() {
        let (from, to) = (SECOND, 2 * SECOND);
        let input = voice(3 * SECOND, from, to);
        let last = |hold_ms| {
            let out = filter(Mode::Gate(Gate { hold_ms, ..Gate::VOICE }), &input);
            out.iter().rposition(|&v| v != 0.0).expect("the gate never opened")
        };
        let (short, long) = (last(100), last(500));
        assert!((19 * SECOND / 50..=21 * SECOND / 50).contains(&(long - short)), "{short} and {long}");
    }

    #[test]
    fn modes_are_written_and_read_back() {
        let custom = Mode::Gate(Gate { lookahead_ms: 80, hold_ms: 400, threshold: 0.35 });
        for mode in [Mode::Off, Mode::Reduce, Mode::VOICE, Mode::WHOLE_WORDS, custom] {
            assert_eq!(mode.to_string().parse(), Ok(mode));
        }
        assert_eq!("whole".parse(), Ok(Mode::WHOLE_WORDS));
        assert!("loud".parse::<Mode>().is_err());
        assert!("gate:40:250".parse::<Mode>().is_err());
        // Out of range is brought in, and the look-ahead onto whole frames.
        let wild = Gate { lookahead_ms: 999, hold_ms: 0, threshold: 7.0 };
        assert_eq!("gate:999:0:7".parse(), Ok(Mode::Gate(wild).clamped()));
        assert_eq!(
            Mode::Gate(Gate { lookahead_ms: 45, ..wild }).clamped(),
            Mode::Gate(Gate { lookahead_ms: 40, hold_ms: 10, threshold: 0.95 })
        );
    }
}
