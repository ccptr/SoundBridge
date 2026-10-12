//! Levelling for a microphone, on the sending end and before the samples are
//! cut down to 16 bits: a phone's microphone arrives 30 dB or more below full
//! scale, and gain added at the far end would bring the rounding noise up
//! with the voice.
//!
//! Automatic gain follows how loud speech is and steers it to [`TARGET_DB`].
//! It moves only while there is something to hear, so a pause is not slowly
//! turned up into room noise. A manual offset comes on top of it, or stands
//! alone as a fixed gain. Either way a limiter has the last word, and nothing
//! leaves above [`CEILING`].
//!
//! None of it delays the audio: the gain is worked out from what has already
//! gone by, and the limiter acts on the sample in hand rather than looking
//! ahead.

/// Level the loud parts of speech are brought to, dBFS RMS. Their peaks then
/// sit some 10 dB higher, short of the limiter.
const TARGET_DB: f32 = -18.0;
/// The most automatic gain will add. More would mostly amplify the room.
const MAX_BOOST_DB: f32 = 36.0;
/// The most it will take away, for a source that is too hot already.
const MAX_CUT_DB: f32 = -12.0;
/// The range of the manual offset.
pub const OFFSET_RANGE_DB: std::ops::RangeInclusive<f32> = -40.0..=40.0;

/// Levels are measured over blocks this long.
const BLOCK_S: f32 = 0.01;
/// Nothing quieter than this is taken for speech, dBFS RMS.
const GATE_DB: f32 = -65.0;
/// Speech stands at least this far above the background.
const ABOVE_FLOOR_DB: f32 = 10.0;
/// How fast the background estimate creeps up, dB per block. It drops at
/// once, so it sits at the quietest level of the last several seconds.
const FLOOR_RISE_DB: f32 = 2.0 * BLOCK_S;
/// Share of the way the level estimate moves per block: fast towards
/// something louder (0.1 s), slowly back down (5 s), so that it rides the
/// loud parts of speech instead of sagging in every gap between words.
const ATTACK: f32 = 0.095;
const RELEASE: f32 = 0.002;
/// The first blocks of speech set the level outright, in about 0.1 s.
const SETTLE_BLOCKS: u32 = 30;

/// Nothing is sent above this (-1 dBFS).
pub const CEILING: f32 = 0.891;
/// How long the limiter takes to let go again.
const LIMIT_RELEASE_S: f32 = 0.08;

fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

pub struct Leveler {
    auto: bool,
    offset: f32,
    block: usize,
    seen: usize,
    energy: f32,
    floor_db: f32,
    /// How loud speech is, dBFS RMS; `None` until some has been heard.
    level_db: Option<f32>,
    settling: u32,
    /// Gain in use, ramped to its new value over the following block.
    gain: f32,
    step: f32,
    /// What the limiter is taking off, 1 when idle.
    limit: f32,
    limit_release: f32,
}

impl Leveler {
    /// `None` if there is nothing to do, so an untouched stream stays bit-exact.
    pub fn new(sample_rate: u32, auto: bool, offset_db: f32) -> Option<Leveler> {
        let offset_db = offset_db.clamp(*OFFSET_RANGE_DB.start(), *OFFSET_RANGE_DB.end());
        if !auto && offset_db == 0.0 {
            return None;
        }
        let rate = sample_rate.max(1) as f32;
        Some(Leveler {
            auto,
            offset: db_to_gain(offset_db),
            block: ((rate * BLOCK_S) as usize).max(1),
            seen: 0,
            energy: 0.0,
            floor_db: -90.0,
            level_db: None,
            settling: SETTLE_BLOCKS,
            gain: 1.0,
            step: 0.0,
            limit: 1.0,
            limit_release: 1.0 - (-1.0 / (LIMIT_RELEASE_S * rate)).exp(),
        })
    }

    /// Takes one frame (mono is the same sample twice) and returns it levelled.
    ///
    /// Made for the capture callback: no allocation, no locks.
    #[inline]
    pub fn apply(&mut self, frame: [f32; 2]) -> [f32; 2] {
        if self.auto {
            self.energy += 0.5 * (frame[0] * frame[0] + frame[1] * frame[1]);
            self.seen += 1;
            if self.seen == self.block {
                self.measure();
            }
            self.gain += self.step;
        }
        let gain = self.gain * self.offset;
        let peak = frame[0].abs().max(frame[1].abs()) * gain;
        let needed = if peak > CEILING { CEILING / peak } else { 1.0 };
        // Down at once, since there is no looking ahead; back up gradually.
        self.limit = (self.limit + (1.0 - self.limit) * self.limit_release).min(needed);
        let gain = gain * self.limit;
        [frame[0] * gain, frame[1] * gain]
    }

    /// Automatic gain now in use, dB, without the offset or the limiter.
    pub fn auto_gain_db(&self) -> f32 {
        20.0 * self.gain.log10()
    }

    fn measure(&mut self) {
        let db = 10.0 * (self.energy / self.block as f32 + 1e-12).log10();
        self.energy = 0.0;
        self.seen = 0;
        self.floor_db = db.min(self.floor_db + FLOOR_RISE_DB);
        if db > GATE_DB && db > self.floor_db + ABOVE_FLOOR_DB {
            let level = match self.level_db {
                None => db,
                Some(level) => {
                    let rate = if self.settling > 0 || db > level { ATTACK } else { RELEASE };
                    level + (db - level) * rate
                }
            };
            self.settling = self.settling.saturating_sub(1);
            self.level_db = Some(level);
        }
        let target = match self.level_db {
            Some(level) => db_to_gain((TARGET_DB - level).clamp(MAX_CUT_DB, MAX_BOOST_DB)),
            None => 1.0,
        };
        self.step = (target - self.gain) / self.block as f32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    const RATE: u32 = 48_000;

    /// Phrases of a 200 Hz tone at `speech_db` (RMS), half a second on and
    /// half a second off, over steady noise at `noise_db`.
    fn talk(seconds: usize, speech_db: f32, noise_db: f32) -> Vec<f32> {
        let mut seed = 0x2545_f491u32;
        (0..seconds * RATE as usize)
            .map(|i| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                // Uniform noise has an RMS of its amplitude over sqrt(3).
                let noise = (seed as f32 / u32::MAX as f32 * 2.0 - 1.0) * db_to_gain(noise_db) * 3f32.sqrt();
                let speaking = (i / (RATE as usize / 2)).is_multiple_of(2);
                let tone = (i as f32 * 200.0 * TAU / RATE as f32).sin() * db_to_gain(speech_db) * 2f32.sqrt();
                noise + if speaking { tone } else { 0.0 }
            })
            .collect()
    }

    fn rms_db(samples: &[f32]) -> f32 {
        10.0 * (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).log10()
    }

    fn run(leveler: &mut Leveler, input: &[f32]) -> Vec<f32> {
        input.iter().map(|&s| leveler.apply([s, s])[0]).collect()
    }

    #[test]
    fn does_nothing_unless_asked() {
        assert!(Leveler::new(RATE, false, 0.0).is_none());
        assert!(Leveler::new(RATE, false, 6.0).is_some());
        assert!(Leveler::new(RATE, true, 0.0).is_some());
    }

    #[test]
    fn brings_quiet_speech_up_to_the_target() {
        let mut leveler = Leveler::new(RATE, true, 0.0).unwrap();
        let out = run(&mut leveler, &talk(6, -48.0, -80.0));
        // The first half of the last second is a phrase.
        let phrase = &out[5 * RATE as usize..][..RATE as usize / 2];
        let level = rms_db(phrase);
        assert!((level - TARGET_DB).abs() < 2.0, "speech came out at {level} dBFS");
        // Settled within the first phrase, not only by the end.
        let first = rms_db(&out[RATE as usize / 4..][..RATE as usize / 4]);
        assert!((first - TARGET_DB).abs() < 3.0, "the first phrase came out at {first} dBFS");
    }

    #[test]
    fn turns_a_hot_source_down() {
        let mut leveler = Leveler::new(RATE, true, 0.0).unwrap();
        let out = run(&mut leveler, &talk(4, -8.0, -80.0));
        let level = rms_db(&out[3 * RATE as usize..][..RATE as usize / 2]);
        assert!((level - TARGET_DB).abs() < 2.0, "speech came out at {level} dBFS");
    }

    #[test]
    fn holds_its_gain_through_a_pause() {
        let mut leveler = Leveler::new(RATE, true, 0.0).unwrap();
        run(&mut leveler, &talk(4, -48.0, -75.0));
        let before = leveler.auto_gain_db();
        // Half a minute of the room alone.
        let room: Vec<f32> = talk(30, -48.0, -75.0)
            .chunks(RATE as usize / 2)
            .skip(1)
            .step_by(2)
            .flatten()
            .copied()
            .collect();
        run(&mut leveler, &room);
        let after = leveler.auto_gain_db();
        assert!((after - before).abs() < 0.5, "gain went from {before} to {after} dB in a pause");
    }

    #[test]
    fn the_offset_shifts_the_result() {
        let level = |offset| {
            let mut leveler = Leveler::new(RATE, true, offset).unwrap();
            let out = run(&mut leveler, &talk(4, -48.0, -80.0));
            rms_db(&out[3 * RATE as usize..][..RATE as usize / 2])
        };
        let (plain, lower) = (level(0.0), level(-6.0));
        assert!((plain - lower - 6.0).abs() < 0.2, "{plain} and {lower} dBFS");
    }

    #[test]
    fn a_fixed_gain_is_exact_below_the_ceiling() {
        let mut leveler = Leveler::new(RATE, false, 20.0).unwrap();
        for s in [0.0, 0.01, -0.05, 0.08] {
            let out = leveler.apply([s, -s]);
            assert!((out[0] - s * 10.0).abs() < 1e-6 && (out[1] + s * 10.0).abs() < 1e-6);
        }
    }

    #[test]
    fn nothing_passes_the_ceiling() {
        for auto in [false, true] {
            let mut leveler = Leveler::new(RATE, auto, 30.0).unwrap();
            // Quiet, then suddenly loud: the worst case for a gain that was
            // set by what came before.
            let mut input = talk(2, -50.0, -80.0);
            input.extend((0..RATE as usize).map(|i| (i as f32 * 0.05).sin() * 0.9));
            let peak = run(&mut leveler, &input).iter().fold(0.0f32, |m, s| m.max(s.abs()));
            assert!(peak <= CEILING + 1e-6, "peak {peak}");
        }
    }

    #[test]
    fn the_limiter_lets_go_again() {
        let mut leveler = Leveler::new(RATE, false, 20.0).unwrap();
        leveler.apply([0.5, 0.5]);
        for _ in 0..RATE / 2 {
            leveler.apply([0.0, 0.0]);
        }
        assert!((leveler.apply([0.01, 0.01])[0] - 0.1).abs() < 1e-3);
    }
}
