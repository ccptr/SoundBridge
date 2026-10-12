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
//! trails its input by one, and the gate looks [`LOOKAHEAD`] frames ahead so
//! it is already open for the consonant in front of a vowel: a filtered
//! stream is 50 to 60 ms later than an unfiltered one.

use nnnoiseless::DenoiseState;

/// The only sample rate the model was trained for.
pub const RATE: u32 = 48_000;
pub const FRAME: usize = DenoiseState::FRAME_SIZE;

/// Frames the output is held back, so the gate can open ahead of the voiced
/// sound that triggers it. Words often start unvoiced ("s", "f", "t").
const LOOKAHEAD: usize = 4;
/// RNNoise speech probability a voiced frame needs.
const SPEECH: f32 = 0.5;
/// Pitch strength (see [`VoiceFilter::pitch`]) a voiced frame needs. Vowels
/// sit well above this, noise of any colour well below.
const PITCH: f32 = 0.5;
/// A click or a thump can pass for a voiced frame or two; speech keeps it up.
const OPEN_FRAMES: u32 = 3;
/// Frames the gate stays open after the last voiced one, covering unvoiced
/// consonants and the gaps between syllables.
const HOLD_FRAMES: u32 = 25;

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
    net: Box<DenoiseState<'static>>,
    input: [f32; FRAME],
    filled: usize,
    /// Denoised frames waiting out the look-ahead; `at` is the oldest.
    held: [[f32; FRAME]; LOOKAHEAD],
    at: usize,
    out: [f32; FRAME],
    recent: [f32; PITCH_WINDOW],
    /// Consecutive voiced frames.
    run: u32,
    /// Frames left before the gate closes; 0 is closed.
    hold: u32,
    gain: f32,
}

impl Default for VoiceFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl VoiceFilter {
    pub fn new() -> Self {
        VoiceFilter {
            net: DenoiseState::new(),
            input: [0.0; FRAME],
            filled: 0,
            held: [[0.0; FRAME]; LOOKAHEAD],
            at: 0,
            out: [0.0; FRAME],
            recent: [0.0; PITCH_WINDOW],
            run: 0,
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
    pub fn push(&mut self, sample: f32) -> Option<&[f32; FRAME]> {
        self.input[self.filled] = sample * SCALE;
        self.filled += 1;
        if self.filled < FRAME {
            return None;
        }
        self.filled = 0;

        // The frame leaving the look-ahead goes out; the new one takes its place.
        self.out = self.held[self.at];
        let speech = self.net.process_frame(&mut self.held[self.at], &self.input);
        self.at = (self.at + 1) % LOOKAHEAD;

        let voiced = speech >= SPEECH && self.pitch() >= PITCH;
        self.run = if voiced { self.run + 1 } else { 0 };
        if self.run >= OPEN_FRAMES || (self.hold > 0 && voiced) {
            self.hold = HOLD_FRAMES;
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

    fn filter(input: &[f32]) -> Vec<f32> {
        let mut filter = VoiceFilter::new();
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
            let out = filter(&input);
            assert!(out.iter().all(|&v| v == 0.0), "{what} got through");
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
        let out = filter(&input);

        let delay = (LOOKAHEAD + 1) * FRAME;
        let spoken = &out[from + delay + SECOND / 10..to + delay];
        let kept = energy(spoken) / energy(&speech[from + SECOND / 10..to]);
        assert!((0.5..2.0).contains(&kept), "speech came out at {kept} of its energy");
        // Typing before the phrase, and from soon after it, is gone entirely.
        let hold = HOLD_FRAMES as usize * FRAME;
        assert!(out[..from].iter().all(|&v| v == 0.0));
        assert!(out[to + delay + hold + FRAME..].iter().all(|&v| v == 0.0));
    }

    #[test]
    fn the_gate_is_open_before_the_sound_that_opened_it() {
        let onset = SECOND + 100;
        let out = filter(&voice(3 * SECOND, onset, 2 * SECOND));
        let first = out.iter().position(|&v| v != 0.0).expect("the gate never opened");
        // In input time: what was heard starts ahead of the first voiced frame.
        let delay = (LOOKAHEAD + 1) * FRAME;
        assert!(first < onset + delay, "the first {} samples were cut off", first - (onset + delay));
    }
}
