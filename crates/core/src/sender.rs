//! The capture side: turns whatever the audio callback delivers into
//! fixed-size packets and puts them on the wire.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

use crate::audio::{SourceKind, StreamFormat};
use crate::denoise::{self, VoiceFilter};
use crate::gain::Leveler;
use crate::protocol::{AUDIO_HEADER_LEN, AudioHeader, CODEC_PCM16, MAX_DATAGRAM, write_audio_header};

#[derive(Default)]
pub struct TxShared {
    packets: AtomicU64,
    errors: AtomicU32,
    peak: AtomicU32,
}

impl TxShared {
    pub fn packets(&self) -> u64 {
        self.packets.load(Relaxed)
    }
    pub fn errors(&self) -> u32 {
        self.errors.load(Relaxed)
    }
    /// Peak level sent since the last call, 0..=1.
    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.peak.swap(0, Relaxed))
    }
}

/// What one outgoing stream is, as far as the wire is concerned.
#[derive(Clone, Copy, Debug)]
pub struct StreamSpec {
    pub slot: u8,
    pub kind: SourceKind,
    pub stream_id: u32,
    /// Channels to put on the wire (1 or 2).
    pub channels: u16,
    pub packet_us: u32,
    /// What to filter out. Takes effect if the device runs at [`denoise::RATE`].
    pub denoise: denoise::Mode,
    /// Bring speech to a steady level (see [`crate::gain`]).
    pub auto_gain: bool,
    /// Gain added on top, dB. Any gain at all also brings in the limiter.
    pub gain_db: f32,
}

/// Lives inside the capture callback. Sending happens right there rather
/// than on a separate thread: a UDP send to a LAN peer is a few microseconds
/// and effectively never blocks, whereas a hand-off would add a wake-up of
/// unpredictable length to every packet.
pub struct Packetizer {
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    header: AudioHeader,
    device_ch: usize,
    wire_ch: usize,
    packet_frames: usize,
    buf: Vec<u8>,
    filled: usize,
    shared: Arc<TxShared>,
    filter: Option<Box<VoiceFilter>>,
    /// After the filter, so that it levels the voice and not what is removed.
    leveler: Option<Leveler>,
}

impl Packetizer {
    /// `device` is the format the callback will deliver. Returns the
    /// resulting wire format.
    pub fn new(
        socket: Arc<UdpSocket>,
        peer: SocketAddr,
        spec: StreamSpec,
        device: StreamFormat,
    ) -> (Packetizer, Arc<TxShared>, StreamFormat) {
        let filter = (spec.denoise.is_on() && device.sample_rate == denoise::RATE)
            .then(|| Box::new(VoiceFilter::new(spec.denoise)));
        // The filter works on one channel, and a voice needs no more.
        let wanted_ch = if filter.is_some() { 1 } else { spec.channels };
        let wire_ch = wanted_ch.clamp(1, 2).min(device.channels.max(1)) as usize;
        let max_frames = (MAX_DATAGRAM - AUDIO_HEADER_LEN) / (2 * wire_ch);
        let wanted = (device.sample_rate as u64 * spec.packet_us as u64 / 1_000_000) as usize;
        let packet_frames = match &filter {
            // The filter delivers a frame at a time; cutting that into
            // smaller packets would only send them in bursts.
            Some(_) => denoise::FRAME,
            None => wanted.clamp(16, max_frames),
        };
        let shared = Arc::new(TxShared::default());
        let wire = StreamFormat { sample_rate: device.sample_rate, channels: wire_ch as u16 };
        let packetizer = Packetizer {
            socket,
            peer,
            header: AudioHeader {
                slot: spec.slot,
                kind: spec.kind,
                stream_id: spec.stream_id,
                timestamp: 0,
                sample_rate: device.sample_rate,
                channels: wire_ch as u8,
                codec: CODEC_PCM16,
            },
            device_ch: device.channels.max(1) as usize,
            wire_ch,
            packet_frames,
            buf: vec![0; AUDIO_HEADER_LEN + packet_frames * wire_ch * 2],
            filled: 0,
            shared: shared.clone(),
            filter,
            leveler: Leveler::new(device.sample_rate, spec.auto_gain, spec.gain_db),
        };
        (packetizer, shared, wire)
    }

    pub fn packet_frames(&self) -> usize {
        self.packet_frames
    }

    /// How the stream is being filtered, which is not at all if the device
    /// runs at a rate the filter cannot take.
    pub fn filtered(&self) -> denoise::Mode {
        self.filter.as_ref().map_or(denoise::Mode::Off, |filter| filter.mode())
    }

    /// Takes interleaved device-format samples; sends every packet they complete.
    pub fn push(&mut self, input: &[f32]) {
        let mut peak = 0.0f32;
        // Taken out for the loop so its output can be handed to `put`.
        let mut filter = self.filter.take();
        for frame in input.chunks_exact(self.device_ch) {
            let wire = match (self.device_ch, self.wire_ch) {
                (1, _) => [frame[0], frame[0]],
                (_, 1) => [0.5 * (frame[0] + frame[1]); 2],
                _ => [frame[0], frame[1]],
            };
            match &mut filter {
                None => peak = peak.max(self.put(wire)),
                Some(filter) => {
                    for &v in filter.push(wire[0]).into_iter().flatten() {
                        peak = peak.max(self.put([v, v]));
                    }
                }
            }
        }
        self.filter = filter;
        self.shared.peak.fetch_max(peak.min(1.0).to_bits(), Relaxed);
    }

    /// Queues one frame for the wire and returns its peak.
    fn put(&mut self, frame: [f32; 2]) -> f32 {
        let frame = match &mut self.leveler {
            Some(leveler) => leveler.apply(frame),
            None => frame,
        };
        let mut peak = 0.0f32;
        let at = AUDIO_HEADER_LEN + self.filled * self.wire_ch * 2;
        for (c, v) in frame[..self.wire_ch].iter().enumerate() {
            peak = peak.max(v.abs());
            let s = (v.clamp(-1.0, 1.0) * 32767.0) as i16;
            self.buf[at + c * 2..at + c * 2 + 2].copy_from_slice(&s.to_le_bytes());
        }
        self.filled += 1;
        if self.filled == self.packet_frames {
            self.flush();
        }
        peak
    }

    fn flush(&mut self) {
        write_audio_header(&mut self.buf, &self.header);
        match self.socket.send_to(&self.buf, self.peer) {
            Ok(_) => self.shared.packets.fetch_add(1, Relaxed),
            Err(_) => self.shared.errors.fetch_add(1, Relaxed) as u64,
        };
        self.header.timestamp = self.header.timestamp.wrapping_add(self.filled as u32);
        self.filled = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Packet, decode};
    use std::time::Duration;

    fn spec(channels: u16, packet_us: u32) -> StreamSpec {
        StreamSpec {
            slot: 3,
            kind: SourceKind::Mic,
            stream_id: 42,
            channels,
            packet_us,
            denoise: denoise::Mode::Off,
            auto_gain: false,
            gain_db: 0.0,
        }
    }

    #[test]
    fn packetizes_across_callback_boundaries() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let tx = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        let device = StreamFormat { sample_rate: 48_000, channels: 2 };
        let (mut p, shared, wire) = Packetizer::new(tx, rx.local_addr().unwrap(), spec(1, 2_500), device);
        assert_eq!(wire, StreamFormat { sample_rate: 48_000, channels: 1 });
        assert_eq!(p.packet_frames(), 120);

        // 300 stereo frames in uneven callbacks: left = 0.5, right = -0.25.
        let frames: Vec<f32> = (0..300).flat_map(|_| [0.5, -0.25]).collect();
        for chunk in frames.chunks(2 * 77) {
            p.push(chunk);
        }
        assert_eq!(shared.packets(), 2, "two full packets, 60 frames still pending");
        assert_eq!(shared.take_peak(), 0.125);

        let mut buf = [0u8; 2048];
        for expect_ts in [0u32, 120] {
            let n = rx.recv(&mut buf).unwrap();
            let Some(Packet::Audio { header, payload }) = decode(&buf[..n]) else { panic!() };
            assert_eq!((header.stream_id, header.timestamp, header.channels), (42, expect_ts, 1));
            assert_eq!((header.slot, header.kind), (3, SourceKind::Mic));
            assert_eq!(payload.len(), 240);
            // Downmixed to mono: (0.5 - 0.25) / 2.
            let s = i16::from_le_bytes([payload[0], payload[1]]);
            assert_eq!(s, (0.125f32 * 32767.0) as i16);
        }
    }

    #[test]
    fn gain_is_applied_before_the_wire_and_limited() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let tx = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        let device = StreamFormat { sample_rate: 48_000, channels: 1 };
        let spec = StreamSpec { gain_db: 20.0, ..spec(1, 2_500) };
        let (mut p, shared, _) = Packetizer::new(tx, rx.local_addr().unwrap(), spec, device);

        // A quiet packet, then one that the gain would push past full scale.
        p.push(&[0.01; 120]);
        p.push(&[0.5; 120]);
        assert_eq!(shared.take_peak(), crate::gain::CEILING);
        let mut buf = [0u8; 2048];
        for expect in [0.1, crate::gain::CEILING] {
            let n = rx.recv(&mut buf).unwrap();
            let Some(Packet::Audio { payload, .. }) = decode(&buf[..n]) else { panic!() };
            let s = i16::from_le_bytes([payload[0], payload[1]]);
            assert!((s as f32 / 32767.0 - expect).abs() < 1e-3, "sent {s}");
        }
    }

    #[test]
    fn packets_never_exceed_the_datagram_limit() {
        let tx = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        let device = StreamFormat { sample_rate: 96_000, channels: 2 };
        let (p, _, _) = Packetizer::new(tx, "127.0.0.1:9".parse().unwrap(), spec(2, 20_000), device);
        assert!(AUDIO_HEADER_LEN + p.packet_frames() * 4 <= MAX_DATAGRAM);
    }
}
