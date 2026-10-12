# SoundBridge

A low-latency audio link between a phone and a PC (or any two machines on the
same network): phone mic → PC, PC audio → phone, phone audio → PC, PC mic →
phone, or any number of those at once.

- **`soundbridge`** — command-line tool (Rust, clap)
- **`soundbridge-desktop`** — desktop app (Rust, gpui)
- **Android app** — Kotlin/Compose, with the engine as a native library

All three run the same engine (`crates/core`), so there is one implementation
of the protocol.

## How it is used

Each end decides two things for itself: what it **sends** (any number of
microphones and of whatever the machine is playing, each as its own stream)
and where it **plays** what the other end sends, which can be a different
place for microphones than for device audio. Then one end connects to the
other. Nothing a peer says can make a device start recording.

| Goal | Phone | PC |
|---|---|---|
| Phone as the PC's microphone | Send: Microphone | Play: Virtual mic |
| Hear the PC on the phone | Play: on | Send: PC audio |
| Hear the phone on the PC | Send: Phone audio | Play: Speakers |
| PC microphone to the phone | Play: on | Send: Microphone |
| Phone as microphone, and hear the phone too | Send: Microphone + Phone audio | Play microphone: Virtual mic, device audio: Speakers |

The apps find each other on the local network; an address can also be typed
in. With the CLI:

```sh
soundbridge listen --play virtual-mic          # PC: wait for the phone, expose it as a mic
soundbridge connect 192.168.1.20 --send system --play none   # PC: send its audio to a listening phone
soundbridge discover                           # who is listening?
soundbridge devices                            # device ids for --send mic=ID / --play speaker=ID
soundbridge listen --play mic:virtual-mic --play system:speaker   # each kind of stream somewhere else
soundbridge connect HOST --send mic --send system --denoise      # two streams; the mic voice only
soundbridge connect HOST --send mic --denoise=whole              # voice only, keeping the start of every word
soundbridge listen --play null                 # link test without sound:
soundbridge connect HOST --send tone --play none
```

`soundbridge help listen` lists the tuning options (`--buffer`, `--packet`,
`--period`, ...).

### Voice only

A microphone can be sent as speech and nothing else ("Voice only" in the
apps, `--denoise` on the command line), for a phone lying on the desk next to
a keyboard: typing, mouse clicks, breathing and background noise are removed,
and between phrases the stream is silent. RNNoise takes the noise out from
under the voice, and a gate that opens only for pitched speech shuts out the
rest. It runs on the sending end, costs about a percent of a desktop core,
and needs the microphone to run at 48 kHz (it is sent unfiltered otherwise).
Anything with a voice or a pitch of its own still gets through: other people
talking, music, a steady hum.

How much is filtered is a choice between presets, and the gate behind the
last two can be tuned from there:

| Preset | `--denoise=` | What it does | Delay |
|---|---|---|---|
| Less noise | `reduce` | RNNoise without the gate: background noise goes, nothing is ever cut off, typing and clicks come through in part | 10 to 20 ms |
| Voice only | `voice` (or bare `--denoise`) | The gate, looking 40 ms ahead. A word that starts unvoiced after a pause ("s", "f", "h") can lose its first sound | 50 to 60 ms |
| Whole words | `whole` | The gate, looking 120 ms ahead, which keeps those starts | 130 to 140 ms |

| Gate setting | Option | Presets | More of it |
|---|---|---|---|
| Look-ahead | `--gate-lookahead MS`, 0 to 200 | 40, 120 | keeps more of the start of a word, and delays the voice as much |
| Hold | `--gate-hold MS`, 10 to 2000 | 250 | bridges longer gaps in speech, and lets more noise trail a phrase |
| Threshold | `--gate-threshold LEVEL`, 0.05 to 0.95 | 0.5 | shuts out more that is not speech, and more quiet speech with it |

The apps offer a few steps of each; the command line takes any value.

### Microphone level

A microphone can be levelled before it is sent ("Automatic level" in the
apps, `--auto-gain` on the command line): speech is brought to the same
loudness however quiet or loud the microphone delivers it, and the gain holds
still through pauses instead of turning up the room. The Android app has it on
by default, because a phone's microphone arrives some 30 dB below a headset's.
A gain in dB can be set as well (`--gain`), which makes the result louder or
quieter, or is the whole gain when the automatic one is off. With either, a
limiter keeps the stream from clipping. It runs on the sending end, before
the audio is reduced to 16 bits, and adds no delay.

## Building

```sh
cargo build --release            # CLI and desktop app, in target/release/
cd android && ./gradlew assembleDebug
```

- Linux needs the PipeWire and ALSA development packages (`libpipewire-0.3`,
  `alsa`), and `pw-loopback` at run time for the virtual microphone.
- The Android build compiles `crates/android` with cargo for each ABI; it
  needs the NDK (27.2) and the Rust targets
  `aarch64-linux-android armv7-linux-androideabi`.
- `Cargo.lock` pins `libc` to 0.2.186: a dependency of gpui 0.2.2 (`xattr`
  0.2.3) does not compile against later releases.

## Where the latency goes

| Stage | Expected | What sets it |
|---|---|---|
| Capture buffer | 2–4 ms | device period (`--period`; AAudio low-latency on Android) |
| Packetization | 2.5 ms | `--packet`; packets are sent from the capture callback |
| Network | < 1 ms wired, 1–5 ms Wi-Fi | marked DSCP EF, so Wi-Fi gives it voice priority |
| Jitter buffer | 3–10 ms | sized from measured jitter (`--buffer auto`), or fixed |
| Playback buffer | 2–6 ms | device period |
| Voice only, if on | 50–60 ms | 10 ms frames, RNNoise's own delay, 40 ms of look-ahead |

Audio is uncompressed 16-bit PCM (about 0.8 Mbit/s mono, 1.5 Mbit/s stereo at
48 kHz), so there is no codec delay. The receiver resamples by a fraction of
a percent to follow the sender's clock, which keeps the buffer at its target
indefinitely instead of drifting into an underrun or a growing delay.

These are design figures, not measurements of a real phone-to-PC link; the
apps show the actual jitter buffer and network round trip live. Phone audio
capture is the exception to "low": Android's playback capture adds some tens
of milliseconds of its own.

## Limits

- **No encryption or authentication.** Audio crosses the network in the
  clear, and a listening instance accepts the first peer that connects. Use
  it on networks you trust.
- One peer at a time: any number of streams, but all with the same device.
- Both ends must run the same protocol version. This one (2, the first with
  several streams) does not talk to version 1; an old and a new end just
  never connect.
- IPv4 only; discovery uses broadcast, so it does not cross routers (type the
  address instead).
- The virtual microphone is created on Linux/PipeWire only. On Windows or
  macOS, install a loopback driver (VB-CABLE, BlackHole) and choose it as the
  output device.
- System-audio capture needs PipeWire, WASAPI or CoreAudio; plain ALSA cannot.
- Android captures only audio that apps allow to be captured (not calls, not
  most DRM-protected players), and asks for screen-capture consent each time.

## Layout

```
crates/core      protocol, packetizer, noise filter, jitter buffer, session engine, cpal backend
crates/cli       soundbridge
crates/desktop   soundbridge-desktop
crates/android   JNI library: AAudio backend + engine bindings
android/         the Android app
```

## License

SoundBridge is free software, licensed under the GNU General Public License,
either version 2 or (at your option) any later version. See [LICENSE](LICENSE).

`crates/desktop/src/text_input.rs` is adapted from gpui's `input` example,
which is Apache-2.0. The noise filter is the `nnnoiseless` port of Xiph's
RNNoise, BSD-3-Clause.
