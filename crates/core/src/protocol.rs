//! The SoundBridge wire format.
//!
//! Everything travels as single UDP datagrams, little-endian, each starting
//! with `"SB"`, a version byte and a packet type. Audio packets describe their
//! own format, so a receiver can start (or rebuild) playback from any packet
//! without having seen a handshake.

use crate::audio::SourceKind;
use crate::denoise::{Gate, Mode};

/// Port every listening instance binds, and the one discovery probes go to.
pub const DEFAULT_PORT: u16 = 47800;

const MAGIC: [u8; 2] = *b"SB";
const VERSION: u8 = 2;

/// Largest datagram we ever build. Stays under a 1500-byte Ethernet MTU minus
/// IP and UDP headers with room for a tunnel, so packets are never fragmented.
pub const MAX_DATAGRAM: usize = 1400;
/// Bytes in front of the samples in an audio packet.
pub const AUDIO_HEADER_LEN: usize = 4 + 16;
pub const MAX_NAME_LEN: usize = 64;
/// Most streams one end sends at once. Bounds what a peer can make this end
/// open, and lets every stream's report fit in one ping.
pub const MAX_STREAMS: usize = 64;

const T_DISCOVER: u8 = 1;
const T_ANNOUNCE: u8 = 2;
const T_HELLO: u8 = 3;
const T_HELLO_ACK: u8 = 4;
const T_BYE: u8 = 5;
const T_PING: u8 = 6;
const T_PONG: u8 = 7;
const T_AUDIO: u8 = 8;
const T_SOURCES: u8 = 9;
const T_SET: u8 = 10;

/// Signed 16-bit little-endian PCM, interleaved. The only codec so far; the
/// field exists so a compressed one can be added without a version bump.
pub const CODEC_PCM16: u8 = 0;

pub const CAP_SENDS: u8 = 1;
pub const CAP_PLAYS: u8 = 2;

pub const BYE_NORMAL: u8 = 0;
pub const BYE_BUSY: u8 = 1;

/// What the receiving end of a stream tells its sender about playout, carried
/// in every ping so each side can show the other's buffer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// The sender's slot of the stream this is about.
    pub slot: u8,
    pub playing: bool,
    pub buffer_us: u32,
    pub jitter_us: u32,
    pub lost: u32,
    pub underruns: u32,
}

/// How one of an end's sources is filtered and levelled before it is sent.
/// The sending end says so for each of its sources, and the receiving end
/// can ask for something else: these are settings of the sender that only
/// the receiver hears the effect of.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SourceSettings {
    /// The sender's slot of the stream.
    pub slot: u8,
    /// What is in that slot. A request made for another kind of source than
    /// the one now there is for a stream that has gone, and is ignored.
    pub kind: SourceKind,
    pub denoise: Mode,
    pub auto_gain: bool,
    pub gain_db: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioHeader {
    /// Which of the sender's streams this is; stable while its source is.
    pub slot: u8,
    /// What the stream carries, so the receiver can decide where it goes.
    pub kind: SourceKind,
    /// Changes whenever the sender restarts capture.
    pub stream_id: u32,
    /// Index of the packet's first frame, wrapping.
    pub timestamp: u32,
    pub sample_rate: u32,
    pub channels: u8,
    pub codec: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Packet<'a> {
    Discover { instance: u32 },
    Announce { instance: u32, busy: bool, name: &'a str },
    Hello { name: &'a str, caps: u8 },
    HelloAck { name: &'a str, caps: u8 },
    Bye { reason: u8 },
    /// One report per stream being played, at most [`MAX_STREAMS`].
    Ping { t_us: u64, reports: Vec<Report> },
    Pong { t_us: u64 },
    Audio { header: AudioHeader, payload: &'a [u8] },
    /// Everything the sender of this packet sends, in slot order and at most
    /// [`MAX_STREAMS`]. Goes out with every ping and whenever it changes.
    Sources { sources: Vec<SourceSettings> },
    /// Asks the peer to send one of its sources this way from now on. The
    /// answer is its next [`Packet::Sources`].
    Set { source: SourceSettings },
}

fn head(out: &mut Vec<u8>, kind: u8) {
    out.clear();
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.push(kind);
}

fn put_name(out: &mut Vec<u8>, name: &str) {
    let mut end = name.len().min(MAX_NAME_LEN);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    out.push(end as u8);
    out.extend_from_slice(&name.as_bytes()[..end]);
}

fn kind_code(kind: SourceKind) -> u8 {
    match kind {
        SourceKind::Mic => 0,
        SourceKind::System => 1,
        SourceKind::Tone => 2,
    }
}

const DENOISE_OFF: u8 = 0;
const DENOISE_REDUCE: u8 = 1;
const DENOISE_GATE: u8 = 2;

fn put_source(out: &mut Vec<u8>, s: &SourceSettings) {
    let (mode, gate) = match s.denoise {
        Mode::Off => (DENOISE_OFF, Gate::VOICE),
        Mode::Reduce => (DENOISE_REDUCE, Gate::VOICE),
        Mode::Gate(gate) => (DENOISE_GATE, gate),
    };
    out.extend_from_slice(&[s.slot, kind_code(s.kind), mode]);
    out.extend_from_slice(&(gate.lookahead_ms.min(u16::MAX as u32) as u16).to_le_bytes());
    out.extend_from_slice(&(gate.hold_ms.min(u16::MAX as u32) as u16).to_le_bytes());
    out.extend_from_slice(&gate.threshold.to_le_bytes());
    out.push(s.auto_gain as u8);
    out.extend_from_slice(&s.gain_db.to_le_bytes());
}

fn kind_from(code: u8) -> Option<SourceKind> {
    Some(match code {
        0 => SourceKind::Mic,
        1 => SourceKind::System,
        2 => SourceKind::Tone,
        _ => return None,
    })
}

impl Packet<'_> {
    /// Serializes into `out`, replacing its contents.
    pub fn encode(&self, out: &mut Vec<u8>) {
        match *self {
            Packet::Discover { instance } => {
                head(out, T_DISCOVER);
                out.extend_from_slice(&instance.to_le_bytes());
            }
            Packet::Announce { instance, busy, name } => {
                head(out, T_ANNOUNCE);
                out.extend_from_slice(&instance.to_le_bytes());
                out.push(busy as u8);
                put_name(out, name);
            }
            Packet::Hello { name, caps } | Packet::HelloAck { name, caps } => {
                let kind = if matches!(self, Packet::Hello { .. }) { T_HELLO } else { T_HELLO_ACK };
                head(out, kind);
                out.push(caps);
                put_name(out, name);
            }
            Packet::Bye { reason } => {
                head(out, T_BYE);
                out.push(reason);
            }
            Packet::Ping { t_us, ref reports } => {
                head(out, T_PING);
                out.extend_from_slice(&t_us.to_le_bytes());
                let reports = &reports[..reports.len().min(MAX_STREAMS)];
                out.push(reports.len() as u8);
                for r in reports {
                    out.push(r.slot);
                    out.push(r.playing as u8);
                    for v in [r.buffer_us, r.jitter_us, r.lost, r.underruns] {
                        out.extend_from_slice(&v.to_le_bytes());
                    }
                }
            }
            Packet::Pong { t_us } => {
                head(out, T_PONG);
                out.extend_from_slice(&t_us.to_le_bytes());
            }
            Packet::Audio { ref header, payload } => {
                out.clear();
                out.resize(AUDIO_HEADER_LEN, 0);
                write_audio_header(out, header);
                out.extend_from_slice(payload);
            }
            Packet::Sources { ref sources } => {
                head(out, T_SOURCES);
                let sources = &sources[..sources.len().min(MAX_STREAMS)];
                out.push(sources.len() as u8);
                for source in sources {
                    put_source(out, source);
                }
            }
            Packet::Set { ref source } => {
                head(out, T_SET);
                put_source(out, source);
            }
        }
    }

    pub fn to_vec(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(96);
        self.encode(&mut out);
        out
    }
}

/// Writes an audio header into the first [`AUDIO_HEADER_LEN`] bytes of `buf`.
/// Allocation-free, for the capture callback.
pub fn write_audio_header(buf: &mut [u8], h: &AudioHeader) {
    buf[0..2].copy_from_slice(&MAGIC);
    buf[2] = VERSION;
    buf[3] = T_AUDIO;
    buf[4] = h.slot;
    buf[5] = kind_code(h.kind);
    buf[6..10].copy_from_slice(&h.stream_id.to_le_bytes());
    buf[10..14].copy_from_slice(&h.timestamp.to_le_bytes());
    buf[14..18].copy_from_slice(&h.sample_rate.to_le_bytes());
    buf[18] = h.channels;
    buf[19] = h.codec;
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_le_bytes(b.try_into().unwrap()))
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    }
    fn u64(&mut self) -> Option<u64> {
        self.take(8).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }
    fn f32(&mut self) -> Option<f32> {
        self.u32().map(f32::from_bits)
    }
    fn source(&mut self) -> Option<SourceSettings> {
        let (slot, kind, mode) = (self.u8()?, kind_from(self.u8()?)?, self.u8()?);
        let gate = Gate { lookahead_ms: self.u16()? as u32, hold_ms: self.u16()? as u32, threshold: self.f32()? };
        let denoise = match mode {
            DENOISE_OFF => Mode::Off,
            DENOISE_REDUCE => Mode::Reduce,
            DENOISE_GATE => Mode::Gate(gate),
            _ => return None,
        };
        if slot as usize >= MAX_STREAMS {
            return None;
        }
        Some(SourceSettings { slot, kind, denoise, auto_gain: self.u8()? != 0, gain_db: self.f32()? })
    }
    fn name(&mut self) -> Option<&'a str> {
        let len = self.u8()? as usize;
        if len > MAX_NAME_LEN {
            return None;
        }
        std::str::from_utf8(self.take(len)?).ok()
    }
}

/// Parses one datagram. Anything malformed, foreign or from another protocol
/// version is `None`; the caller just drops it.
pub fn decode(buf: &[u8]) -> Option<Packet<'_>> {
    let mut r = Reader(buf);
    if r.take(2)? != MAGIC || r.u8()? != VERSION {
        return None;
    }
    Some(match r.u8()? {
        T_DISCOVER => Packet::Discover { instance: r.u32()? },
        T_ANNOUNCE => Packet::Announce { instance: r.u32()?, busy: r.u8()? != 0, name: r.name()? },
        T_HELLO => {
            let caps = r.u8()?;
            Packet::Hello { name: r.name()?, caps }
        }
        T_HELLO_ACK => {
            let caps = r.u8()?;
            Packet::HelloAck { name: r.name()?, caps }
        }
        T_BYE => Packet::Bye { reason: r.u8()? },
        T_PING => {
            let t_us = r.u64()?;
            let count = r.u8()? as usize;
            if count > MAX_STREAMS {
                return None;
            }
            let mut reports = Vec::with_capacity(count);
            for _ in 0..count {
                reports.push(Report {
                    slot: r.u8()?,
                    playing: r.u8()? != 0,
                    buffer_us: r.u32()?,
                    jitter_us: r.u32()?,
                    lost: r.u32()?,
                    underruns: r.u32()?,
                });
            }
            Packet::Ping { t_us, reports }
        }
        T_PONG => Packet::Pong { t_us: r.u64()? },
        T_AUDIO => {
            let header = AudioHeader {
                slot: r.u8()?,
                kind: kind_from(r.u8()?)?,
                stream_id: r.u32()?,
                timestamp: r.u32()?,
                sample_rate: r.u32()?,
                channels: r.u8()?,
                codec: r.u8()?,
            };
            let payload = r.0;
            let frame = header.channels as usize * 2;
            if header.slot as usize >= MAX_STREAMS
                || !(1..=2).contains(&header.channels)
                || !(8_000..=192_000).contains(&header.sample_rate)
                || header.codec != CODEC_PCM16
                || payload.is_empty()
                || payload.len() % frame != 0
            {
                return None;
            }
            Packet::Audio { header, payload }
        }
        T_SOURCES => {
            let count = r.u8()? as usize;
            if count > MAX_STREAMS {
                return None;
            }
            let mut sources = Vec::with_capacity(count);
            for _ in 0..count {
                sources.push(r.source()?);
            }
            Packet::Sources { sources }
        }
        T_SET => Packet::Set { source: r.source()? },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(p: Packet) {
        let bytes = p.to_vec();
        assert_eq!(decode(&bytes), Some(p));
    }

    #[test]
    fn roundtrips() {
        roundtrip(Packet::Discover { instance: 7 });
        roundtrip(Packet::Announce { instance: 9, busy: true, name: "desk" });
        roundtrip(Packet::Hello { name: "Pixel 9", caps: CAP_SENDS | CAP_PLAYS });
        roundtrip(Packet::HelloAck { name: "", caps: 0 });
        roundtrip(Packet::Bye { reason: BYE_BUSY });
        roundtrip(Packet::Ping { t_us: 123_456_789_000, reports: Vec::new() });
        let report = Report { slot: 0, playing: true, buffer_us: 8000, jitter_us: 900, lost: 3, underruns: 1 };
        roundtrip(Packet::Ping { t_us: 1, reports: vec![report, Report { slot: 7, ..Report::default() }] });
        roundtrip(Packet::Pong { t_us: u64::MAX });
        roundtrip(Packet::Audio {
            header: AudioHeader {
                slot: 3,
                kind: SourceKind::System,
                stream_id: 5,
                timestamp: u32::MAX,
                sample_rate: 48_000,
                channels: 2,
                codec: CODEC_PCM16,
            },
            payload: &[1, 2, 3, 4, 5, 6, 7, 8],
        });
        let mic = SourceSettings {
            slot: 0,
            kind: SourceKind::Mic,
            denoise: Mode::Gate(Gate { lookahead_ms: 80, hold_ms: 2000, threshold: 0.3 }),
            auto_gain: true,
            gain_db: -4.5,
        };
        let system =
            SourceSettings { slot: 63, kind: SourceKind::System, denoise: Mode::Off, auto_gain: false, gain_db: 0.0 };
        roundtrip(Packet::Sources { sources: Vec::new() });
        roundtrip(Packet::Sources { sources: vec![mic, system, SourceSettings { denoise: Mode::Reduce, ..mic }] });
        roundtrip(Packet::Set { source: mic });
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(decode(b""), None);
        assert_eq!(decode(b"SB"), None);
        assert_eq!(decode(b"XX\x01\x01\0\0\0\0"), None);
        assert_eq!(decode(b"SB\x01\x01\0\0\0\0"), None, "other protocol version");
        // Truncated at every length must never panic.
        let full = Packet::Announce { instance: 1, busy: false, name: "name" }.to_vec();
        for n in 0..full.len() {
            assert_eq!(decode(&full[..n]), None);
        }
        // Audio whose payload is not a whole number of frames.
        let mut audio = Packet::Audio {
            header: AudioHeader {
                slot: 0,
                kind: SourceKind::Mic,
                stream_id: 1,
                timestamp: 0,
                sample_rate: 48_000,
                channels: 2,
                codec: CODEC_PCM16,
            },
            payload: &[0; 8],
        }
        .to_vec();
        audio.pop();
        assert_eq!(decode(&audio), None);
        audio.push(0);
        assert!(decode(&audio).is_some());
        // A kind of source this version has never heard of.
        audio[5] = 9;
        assert_eq!(decode(&audio), None);
        // Settings cut short, for a slot there cannot be, or filtered in a
        // way this version does not know.
        let source =
            SourceSettings { slot: 1, kind: SourceKind::Mic, denoise: Mode::VOICE, auto_gain: true, gain_db: 6.0 };
        let set = Packet::Set { source }.to_vec();
        for n in 0..set.len() {
            assert_eq!(decode(&set[..n]), None);
        }
        for (at, bad) in [(4, MAX_STREAMS as u8), (6, 3)] {
            let mut set = set.clone();
            set[at] = bad;
            assert_eq!(decode(&set), None);
        }
    }

    #[test]
    fn long_names_truncate_on_a_char_boundary() {
        let name = "é".repeat(100);
        let bytes = Packet::Hello { name: &name, caps: 0 }.to_vec();
        match decode(&bytes) {
            Some(Packet::Hello { name, .. }) => assert_eq!(name.len(), MAX_NAME_LEN),
            other => panic!("{other:?}"),
        }
    }
}
