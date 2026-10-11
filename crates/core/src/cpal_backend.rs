//! Desktop audio through cpal: PipeWire (or ALSA) on Linux, WASAPI on
//! Windows, CoreAudio on macOS.

use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, StreamConfig, SupportedBufferSize, SupportedStreamConfigRange};

use crate::audio::{
    AudioBackend, CaptureRequest, DeviceDirection, DeviceInfo, MakeCapture, MakeRender,
    PlaybackRequest, SinkKind, SourceKind, StreamFormat, StreamHandle,
};

/// How long a backend may take to bring a stream up before we call it failed.
const OPEN_TIMEOUT: Option<Duration> = Some(Duration::from_secs(3));

#[derive(Default)]
pub struct CpalBackend;

impl CpalBackend {
    pub fn new() -> Self {
        CpalBackend
    }

    /// Name of the audio system in use, for display.
    pub fn host_name(&self) -> String {
        host().id().name().to_string()
    }
}

/// A fresh host each time: the PipeWire host snapshots the device list when
/// it is created, and a stale one would miss anything plugged in since.
fn host() -> cpal::Host {
    #[cfg(target_os = "linux")]
    if let Ok(host) = cpal::host_from_id(cpal::HostId::PipeWire) {
        return host;
    }
    cpal::default_host()
}

struct Stream {
    dead: Arc<AtomicBool>,
    _stream: cpal::Stream,
    // Declared after the stream so they outlive it on drop.
    _host: cpal::Host,
    #[cfg(target_os = "linux")]
    _virtual_mic: Option<virtual_mic::Loopback>,
}

impl StreamHandle for Stream {
    fn is_alive(&self) -> bool {
        !self.dead.load(Relaxed)
    }
}

fn err(context: &str, e: cpal::Error) -> String {
    format!("{context}: {e}")
}

fn find_device(host: &cpal::Host, id: &str) -> Result<cpal::Device, String> {
    let parsed = cpal::DeviceId::from_str(id).map_err(|_| format!("'{id}' is not a device id"))?;
    host.device_by_id(&parsed).ok_or_else(|| format!("device '{id}' is not available"))
}

/// Ranks how well a supported range fits what we want; lower is better.
fn score(range: &SupportedStreamConfigRange, rate: u32, channels: u16) -> Option<u32> {
    let format = match range.sample_format() {
        SampleFormat::F32 => 0,
        SampleFormat::I16 => 1,
        SampleFormat::I32 => 2,
        _ => return None,
    };
    let rate_miss = if range.contains_rate(rate) { 0 } else { 100 };
    let ch = range.channels();
    let channel_miss = match ch {
        _ if ch == channels => 0,
        // More than asked for is fine (extra channels are ignored)...
        _ if ch > channels => 10 + (ch - channels) as u32,
        // ...fewer loses a channel.
        _ => 50,
    };
    Some(rate_miss + channel_miss + format)
}

fn choose(
    ranges: Vec<SupportedStreamConfigRange>,
    rate: u32,
    channels: u16,
    period: u32,
) -> Option<(StreamConfig, SampleFormat)> {
    let best = ranges
        .into_iter()
        .filter_map(|r| score(&r, rate, channels).map(|s| (s, r)))
        .min_by_key(|(s, _)| *s)?
        .1;
    let sample_rate = if best.contains_rate(rate) {
        rate
    } else {
        // Whichever end of the range is nearer; the receiver resamples.
        let (lo, hi) = (best.min_sample_rate(), best.max_sample_rate());
        if rate < lo { lo } else { hi }
    };
    let buffer_size = match *best.buffer_size() {
        SupportedBufferSize::Range { min, max } if max > 0 => {
            BufferSize::Fixed(period.clamp(min, max))
        }
        _ => BufferSize::Default,
    };
    let config = StreamConfig { channels: best.channels(), sample_rate, buffer_size };
    Some((config, best.sample_format()))
}

/// A callback that a stream closure claims on its first run.
///
/// cpal consumes the closure it is given even when building fails, so the
/// real callback waits in a shared slot: a build that fails never ran its
/// closure, and the retry finds the callback still there. After the first
/// audio callback the lock is never touched again.
struct Claim<F> {
    slot: Arc<Mutex<Option<F>>>,
    mine: Option<F>,
}

impl<F> Claim<F> {
    fn new(slot: &Arc<Mutex<Option<F>>>) -> Self {
        Claim { slot: slot.clone(), mine: None }
    }

    fn get(&mut self) -> Option<&mut F> {
        if self.mine.is_none() {
            self.mine = self.slot.try_lock().ok()?.take();
        }
        self.mine.as_mut()
    }
}

/// Runs `build` with the chosen config, falling back to the device's own
/// buffer size if it rejects the small one we asked for.
fn build_with_fallback<T>(
    config: StreamConfig,
    mut build: impl FnMut(StreamConfig) -> Result<T, cpal::Error>,
) -> Result<T, cpal::Error> {
    match build(config) {
        Err(_) if config.buffer_size != BufferSize::Default => {
            build(StreamConfig { buffer_size: BufferSize::Default, ..config })
        }
        other => other,
    }
}

/// The error callback for a stream: notes fatal errors in `dead` so the
/// engine reopens the stream, and logs anything else unusual.
fn on_stream_error(
    what: &'static str,
    dead: &Arc<AtomicBool>,
) -> impl FnMut(cpal::Error) + Send + 'static {
    let dead = dead.clone();
    move |e| match e.kind() {
        // Counted by the jitter buffer; printing each would only make the
        // next one more likely.
        cpal::ErrorKind::Xrun => {}
        // The backend already moved the stream to the new default device.
        cpal::ErrorKind::DeviceChanged => {}
        cpal::ErrorKind::DeviceNotAvailable
        | cpal::ErrorKind::StreamInvalidated
        | cpal::ErrorKind::HostUnavailable => {
            dead.store(true, Relaxed)
        }
        _ => eprintln!("soundbridge: {what} stream: {e}"),
    }
}

impl AudioBackend for CpalBackend {
    fn devices(&self) -> Vec<DeviceInfo> {
        let host = host();
        let pipewire = host.id().name().eq_ignore_ascii_case("pipewire");
        let Ok(devices) = host.devices() else { return Vec::new() };
        let mut out = Vec::new();
        for device in devices {
            let (Ok(id), Ok(description)) = (device.id(), device.description()) else { continue };
            // PipeWire's "follow the default" pseudo-devices; choosing no
            // device already means that.
            if pipewire && id.id().ends_with("_default") {
                continue;
            }
            let id = id.to_string();
            let name = description.name().to_string();
            // PipeWire's list mixes real devices with other applications'
            // streams. Sinks come back as duplex (their monitor can be
            // captured, which makes them system-audio sources rather than
            // inputs); a plain "output" is somebody's playback stream, and an
            // input without a description is somebody's recording stream.
            let (is_input, is_output) = match description.direction() {
                cpal::DeviceDirection::Input => (!pipewire || name != "unknown", false),
                cpal::DeviceDirection::Duplex => (!pipewire, true),
                cpal::DeviceDirection::Output => (false, !pipewire),
                _ => (false, false),
            };
            if is_input {
                out.push(DeviceInfo { id: id.clone(), name: name.clone(), direction: DeviceDirection::Input });
            }
            if is_output {
                out.push(DeviceInfo { id, name, direction: DeviceDirection::Output });
            }
        }
        out
    }

    fn open_capture(
        &self,
        request: &CaptureRequest,
        make: MakeCapture,
    ) -> Result<Box<dyn StreamHandle>, String> {
        let host = host();
        let device = match (&request.device, request.kind) {
            (Some(id), _) => find_device(&host, id)?,
            (None, SourceKind::System) => {
                host.default_output_device().ok_or("no default output device to capture")?
            }
            (None, _) => host.default_input_device().ok_or("no default input device")?,
        };
        if !device.supports_input() {
            return Err(match request.kind {
                SourceKind::System => format!(
                    "the {} audio system cannot capture what a device is playing",
                    host.id().name()
                ),
                _ => "that device has no input".into(),
            });
        }
        let ranges = device.supported_input_configs().map(|c| c.collect()).unwrap_or_default();
        let (config, sample_format) =
            choose(ranges, request.sample_rate, request.channels, request.period_frames)
                .or_else(|| {
                    let d = device.default_input_config().ok()?;
                    Some((d.config(), d.sample_format()))
                })
                .ok_or("the device offers no usable input format")?;
        let format = StreamFormat { sample_rate: config.sample_rate, channels: config.channels };
        let slot = Arc::new(Mutex::new(Some(make(format))));
        let dead = Arc::new(AtomicBool::new(false));

        let stream = build_with_fallback(config, |config| {
            let mut claim = Claim::new(&slot);
            match sample_format {
                SampleFormat::F32 => device.build_input_stream(
                    config,
                    move |data: &[f32], _| {
                        if let Some(callback) = claim.get() {
                            callback(data)
                        }
                    },
                    on_stream_error("capture", &dead),
                    OPEN_TIMEOUT,
                ),
                SampleFormat::I16 => {
                    let mut scratch = Vec::with_capacity(16_384);
                    device.build_input_stream(
                        config,
                        move |data: &[i16], _| {
                            if let Some(callback) = claim.get() {
                                convert(data, &mut scratch, |s| s as f32 / 32_768.0, callback)
                            }
                        },
                        on_stream_error("capture", &dead),
                        OPEN_TIMEOUT,
                    )
                }
                _ => {
                    let mut scratch = Vec::with_capacity(16_384);
                    device.build_input_stream(
                        config,
                        move |data: &[i32], _| {
                            if let Some(callback) = claim.get() {
                                convert(data, &mut scratch, |s| s as f32 / 2_147_483_648.0, callback)
                            }
                        },
                        on_stream_error("capture", &dead),
                        OPEN_TIMEOUT,
                    )
                }
            }
        });
        let stream = match stream {
            Ok(stream) => stream,
            Err(e) => return Err(err("opening the input", e)),
        };
        stream.play().map_err(|e| err("starting the input", e))?;
        Ok(Box::new(Stream {
            dead,
            _stream: stream,
            _host: host,
            #[cfg(target_os = "linux")]
            _virtual_mic: None,
        }))
    }

    fn open_playback(
        &self,
        request: &PlaybackRequest,
        make: MakeRender,
    ) -> Result<Box<dyn StreamHandle>, String> {
        #[cfg(target_os = "linux")]
        let (virtual_mic, host, device) = if request.kind == SinkKind::VirtualMic {
            let loopback = virtual_mic::Loopback::spawn()?;
            let (host, device) = loopback.wait_for_sink()?;
            (Some(loopback), host, Some(device))
        } else {
            (None, host(), None)
        };
        #[cfg(not(target_os = "linux"))]
        let (host, device) = {
            if request.kind == SinkKind::VirtualMic {
                return Err("a virtual microphone needs a loopback driver on this platform \
                            (VB-CABLE on Windows, BlackHole on macOS); pick its device as the \
                            output instead"
                    .into());
            }
            (host(), None::<cpal::Device>)
        };

        let device = match (device, &request.device) {
            (Some(device), _) => device,
            (None, Some(id)) => find_device(&host, id)?,
            (None, None) => host.default_output_device().ok_or("no default output device")?,
        };
        let ranges = device.supported_output_configs().map(|c| c.collect()).unwrap_or_default();
        let (config, sample_format) =
            choose(ranges, request.sample_rate, request.channels, request.period_frames)
                .or_else(|| {
                    let d = device.default_output_config().ok()?;
                    Some((d.config(), d.sample_format()))
                })
                .ok_or("the device offers no usable output format")?;
        let format = StreamFormat { sample_rate: config.sample_rate, channels: config.channels };
        let slot = Arc::new(Mutex::new(Some(make(format))));
        let dead = Arc::new(AtomicBool::new(false));

        let stream = build_with_fallback(config, |config| {
            let mut claim = Claim::new(&slot);
            // cpal hands output buffers over already silenced, so a callback
            // that is not there yet simply leaves them that way.
            match sample_format {
                SampleFormat::F32 => device.build_output_stream(
                    config,
                    move |data: &mut [f32], _| {
                        if let Some(callback) = claim.get() {
                            callback(data)
                        }
                    },
                    on_stream_error("playback", &dead),
                    OPEN_TIMEOUT,
                ),
                SampleFormat::I16 => {
                    let mut scratch = vec![0.0f32; 16_384];
                    device.build_output_stream(
                        config,
                        move |data: &mut [i16], _| {
                            if let Some(callback) = claim.get() {
                                render(data, &mut scratch, |v| (v * 32_767.0) as i16, callback)
                            }
                        },
                        on_stream_error("playback", &dead),
                        OPEN_TIMEOUT,
                    )
                }
                _ => {
                    let mut scratch = vec![0.0f32; 16_384];
                    device.build_output_stream(
                        config,
                        move |data: &mut [i32], _| {
                            if let Some(callback) = claim.get() {
                                render(data, &mut scratch, |v| (v as f64 * 2_147_483_647.0) as i32, callback)
                            }
                        },
                        on_stream_error("playback", &dead),
                        OPEN_TIMEOUT,
                    )
                }
            }
        });
        let stream = match stream {
            Ok(stream) => stream,
            Err(e) => return Err(err("opening the output", e)),
        };
        stream.play().map_err(|e| err("starting the output", e))?;
        Ok(Box::new(Stream {
            dead,
            _stream: stream,
            _host: host,
            #[cfg(target_os = "linux")]
            _virtual_mic: virtual_mic,
        }))
    }
}

/// Feeds integer input to a float callback through a fixed scratch buffer,
/// in pieces if a callback is ever larger than the buffer.
fn convert<T: Copy>(
    data: &[T],
    scratch: &mut Vec<f32>,
    to_f32: impl Fn(T) -> f32,
    callback: &mut impl FnMut(&[f32]),
) {
    for chunk in data.chunks(scratch.capacity()) {
        scratch.clear();
        scratch.extend(chunk.iter().map(|&s| to_f32(s)));
        callback(scratch);
    }
}

/// The output-side counterpart of [`convert`]. Chunks stay frame-aligned
/// because the scratch length is a multiple of every channel count in use.
fn render<T>(
    data: &mut [T],
    scratch: &mut [f32],
    from_f32: impl Fn(f32) -> T,
    callback: &mut impl FnMut(&mut [f32]),
) {
    for chunk in data.chunks_mut(scratch.len()) {
        let buf = &mut scratch[..chunk.len()];
        callback(buf);
        for (out, v) in chunk.iter_mut().zip(buf.iter()) {
            *out = from_f32(v.clamp(-1.0, 1.0));
        }
    }
}

/// A microphone other applications can record from, on PipeWire.
///
/// `pw-loopback` creates a sink we play into and a source wired to it. The
/// source is a real `Audio/Source` node, so it shows up as an input device
/// everywhere, including in apps that hide monitor devices.
#[cfg(target_os = "linux")]
mod virtual_mic {
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use cpal::traits::HostTrait;

    const SINK: &str = "soundbridge_mic_sink";

    pub struct Loopback(Child);

    impl Loopback {
        pub fn spawn() -> Result<Loopback, String> {
            let mut command = Command::new("pw-loopback");
            command
                .args(["--name", "soundbridge-mic", "--latency", "5"])
                .arg(format!(
                    "--capture-props=media.class=Audio/Sink node.name={SINK} \
                     node.description=\"SoundBridge mic feed\""
                ))
                .arg(
                    "--playback-props=media.class=Audio/Source node.name=soundbridge_mic \
                     node.description=\"SoundBridge Microphone\"",
                )
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            // If we die without running Drop, the virtual device must not
            // outlive us as an orphan.
            // SAFETY: prctl is async-signal-safe and touches no shared state.
            unsafe {
                command.pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                    Ok(())
                });
            }
            let child = command.spawn().map_err(|e| {
                format!("could not run pw-loopback ({e}); the virtual microphone needs PipeWire")
            })?;
            Ok(Loopback(child))
        }

        /// Waits for the feed sink to show up in the graph and returns it.
        pub fn wait_for_sink(&self) -> Result<(cpal::Host, cpal::Device), String> {
            let id = cpal::DeviceId::new(cpal::HostId::PipeWire, SINK);
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                if let Ok(host) = cpal::host_from_id(cpal::HostId::PipeWire)
                    && let Some(device) = host.device_by_id(&id)
                {
                    return Ok((host, device));
                }
                if Instant::now() > deadline {
                    return Err("the virtual microphone did not appear in PipeWire".into());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    impl Drop for Loopback {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
