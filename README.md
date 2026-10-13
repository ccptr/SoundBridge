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
and where it **plays** what the other end sends, stream by stream. Then one
end connects to the other. Nothing a peer says can make a device start
recording.

| Goal | Phone | PC |
|---|---|---|
| Phone as the PC's microphone | Send: Microphone | Play Microphone: Virtual microphone |
| Hear the PC on the phone | Play: on | Send: PC audio |
| Hear the phone on the PC | Send: Phone audio | Play Device audio: Default output |
| PC microphone to the phone | Play: on | Send: Microphone |
| Phone as microphone, and hear the phone too | Send: Microphone + Phone audio | Play Microphone: Virtual microphone, Device audio: Default output |

The apps find each other on the local network; an address can also be typed
in. With the CLI:

```sh
soundbridge listen --play virtual-mic          # PC: wait for the phone, expose it as a mic
soundbridge connect 192.168.1.20 --send system --play none   # PC: send its audio to a listening phone
soundbridge discover                           # who is listening?
soundbridge devices                            # device ids for --send mic=ID / --play speaker=ID
soundbridge listen --play mic:virtual-mic --play system:speaker  # each stream somewhere else
soundbridge listen --play system:speaker=ID --play speaker       # one stream on a device of its own
soundbridge connect HOST --send mic --send system --denoise      # two streams; the mic voice only
soundbridge connect HOST --send mic --denoise=whole              # voice only, keeping the start of every word
soundbridge listen --play virtual-mic --remote denoise=whole     # the same, set from the end that hears it
soundbridge listen --play null                 # link test without sound:
soundbridge connect HOST --send tone --play none
```

`soundbridge help listen` lists the tuning options (`--buffer`, `--packet`,
`--period`, ...).

### Where each stream plays

A stream is known at both ends by its **source**: the id of the device it is
captured from, as `soundbridge devices` lists it on the sending machine. The
default microphone is `mic` and what the machine plays on its default output
is `system`; those are also the two a phone has. The id does not change when
other sources are added or removed, or when the stream is renamed.

Next to the id a stream has a **name**, which is only what it is shown as.
It starts as the name of the device and can be changed on the sending end:
"Rename" in the desktop app, `--send NAME:mic=ID` on the command line.

The receiving end gives each stream its place by its source ("Play what the
other device sends" in the desktop app, `--play SOURCE:SINK` on the command
line). Two microphones from the same machine go to different devices like
this:

```sh
soundbridge connect HOST --send headset:mic=HEADSET --send mic=WEBCAM --send system
soundbridge listen --play HEADSET:virtual-mic --play WEBCAM:speaker=ID --play speaker
```

- The desktop app lists each stream the other device sends under its name,
  with a button for nothing, a button for the virtual microphone, and
  beside them a list of the output devices, the default one first. What
  was chosen is kept with the source's id for the next time that device
  sends it.
- A stream that is given no place plays on the default output in the desktop
  app, and on the command line wherever a `--play` without a source says
  (`--play speaker` if there is none). Above, that is what the PC plays.
- A stream that is given a place plays only there. On the command line it
  can be given two, and plays in both.
- `--play SOURCE:none` ("Nothing" in the app) plays it nowhere, while the
  rest carry on.
- A source can be sent once: a second stream from the same device is not
  sent, and that is said.
- The command line prints each source the peer sends when it first arrives
  (`The peer sends ID (name)`), and shows streams by name after that. The
  Android app plays everything in one place.

Through 0.3.0 a place was given to a kind of stream or to its number in the
sender's list (`--play mic:SINK`, `--play 2:SINK`, "Play the other device's
microphone" and numbered streams in the desktop app). Numbers are gone: the
command line reads what is before the colon as a source, and the desktop app
forgets what was set by kind or number, so everything plays on the default
output until the streams are given their places again. A peer still on 0.3.0
does not say what its sources are; its microphones are all taken to be `mic`
and what it plays `system`.

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

The Android app offers a few steps of each. The desktop app has a slider for
each, as it has for the gain and a fixed jitter buffer; a right click on a
slider turns it into a field that takes any value. So does the command line.

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

### Setting a microphone from the other end

Filtering and levelling happen where the microphone is, but it is the other
end that hears the result, and the phone that is the microphone is usually
not the device in front of you. So each end tells the other what it sends
and how each microphone is set, and either end can change those settings:

- The desktop and Android apps show "The other device's microphone" while
  connected, with the same choices as for a microphone of their own: what is
  filtered out, the gate's tuning, automatic level and gain.
- On the command line, `--remote SETTING=VALUE` sets the microphones the peer
  sends: `denoise=MODE` (`off`, `reduce`, `voice`, `whole`, or
  `gate:LOOKAHEAD:HOLD:THRESHOLD` for a tuning of the gate),
  `auto-gain=on|off` or `gain=DB`. A source in front is for that stream only.
  What is not mentioned stays as the peer has it.

```sh
soundbridge listen --play virtual-mic --remote denoise=voice --remote gain=6
soundbridge listen --play speaker --remote mic:denoise=gate:80:400:0.4 --remote mic:auto-gain=on
```

- The change is made on the sending device, as if it had been chosen there:
  its app shows it and keeps it for next time. Whichever end sets something
  last wins.
- The command line asks once for each stream, when it first sees it. What is
  changed on the sending device after that stands.
- Only microphones can be set this way. Which sources are sent, and from
  which devices, is still decided by the sending end alone.
- The stream restarts with the new setting, which is a moment of silence.
- An end running a release from before this neither says how it is set nor
  takes requests; the choices are just not offered for it.

### Updates

The programs in a release's Linux and Windows archives keep themselves
current. Once a day, when the desktop app starts or `soundbridge listen` or
`connect` runs, they ask GitHub for the latest release, and if there is a
newer one they replace both programs where they are; the next start runs it.
Nothing is asked or shown beyond a line saying it happened.

To turn that off, set "Updates" to Off in the desktop app or run
`soundbridge update --auto off`; the switch is shared by both programs.

Whatever the switch says, a release can be looked for at any time: "Check
now" in the desktop app says whether there is a newer one and offers to
install it, `soundbridge update --check` only says so, and `soundbridge
update` installs it on the spot.

- An update is only as trustworthy as the HTTPS connection to GitHub:
  releases are not signed.
- The other end is not updated with it. When a release changes the protocol
  version, an updated PC stops connecting to a phone still on the old app
  until that is updated too.
- Programs built from source or installed as a package do not have any of
  this; see the `self-update` feature under Building. A package made from
  the released binaries turns it off by starting them with
  `SOUNDBRIDGE_NO_UPDATE` set, which leaves no switch to turn it back on.

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
- `--features self-update` builds in the updater described above, as the
  release workflow does for the archives. Leave it out of anything a package
  manager installs.
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
- For the same reason, whoever is connected can change how this end's
  microphones are filtered and levelled (see above). It cannot be turned off.
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
crates/update    self-update for the released binaries
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
