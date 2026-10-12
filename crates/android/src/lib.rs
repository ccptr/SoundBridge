//! JNI surface of the SoundBridge engine, used by the app's `Native` object.
//!
//! The app owns one engine at a time through an opaque handle. Everything the
//! engine has to say comes back as a small JSON document passed to
//! `Native.Listener.onEvent`, called on the engine's own thread.
#![cfg(target_os = "android")]

mod aaudio;
mod system_audio;

use std::ffi::CString;
use std::fmt::Write;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use jni::JNIEnv;
use jni::objects::{JByteBuffer, JClass, JObject, JString, JValue};
use jni::sys::{jboolean, jint, jlong};
use soundbridge_core::{
    BufferMode, Config, DEFAULT_PORT, Engine, Event, SinkConfig, SinkKind, SourceConfig,
    SourceKind, State,
};

/// Bits of the `send` argument: any combination of sources.
const SEND_MIC: jint = 1;
const SEND_SYSTEM: jint = 2;

fn log_error(message: &str) {
    if let (Ok(tag), Ok(message)) = (CString::new("SoundBridge"), CString::new(message)) {
        // SAFETY: both pointers are valid NUL-terminated strings for the call.
        unsafe {
            ndk_sys::__android_log_write(
                ndk_sys::android_LogPriority::ANDROID_LOG_ERROR.0 as i32,
                tag.as_ptr(),
                message.as_ptr(),
            );
        }
    }
}

fn config(name: String, send: jint, denoise: bool, play: bool, buffer_ms: jint) -> Config {
    let mut sources = Vec::new();
    if send & SEND_MIC != 0 {
        sources.push(SourceConfig { denoise, ..SourceConfig::new(SourceKind::Mic) });
    }
    if send & SEND_SYSTEM != 0 {
        sources.push(SourceConfig::new(SourceKind::System));
    }
    Config {
        name,
        port: DEFAULT_PORT,
        port_fallback: true,
        listen: true,
        discover: true,
        send: sources,
        play: play.then(|| SinkConfig::new(SinkKind::Speaker)).into_iter().collect(),
        buffer: match buffer_ms {
            ms if ms >= 0 => BufferMode::Fixed { ms: ms as f32 },
            _ => BufferMode::default(),
        },
        ..Config::default()
    }
}

fn string(env: &mut JNIEnv, s: &JString) -> String {
    env.get_string(s).map(Into::into).unwrap_or_default()
}

fn json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Non-finite numbers are not JSON; a level or delay that is one becomes 0.
fn num(v: f32) -> f32 {
    if v.is_finite() { v } else { 0.0 }
}

fn kind_name(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Mic => "mic",
        SourceKind::System => "system",
        SourceKind::Tone => "tone",
    }
}

fn event_json(event: &Event) -> String {
    let mut o = String::with_capacity(256);
    match event {
        Event::State(state) => {
            let (kind, addr, name) = match state {
                State::Idle => ("idle", String::new(), ""),
                State::Connecting { addr } => ("connecting", addr.to_string(), ""),
                State::Connected { addr, name } => ("connected", addr.to_string(), name.as_str()),
            };
            let _ = write!(o, r#"{{"type":"state","state":"{kind}","addr":"{addr}","name":"#);
            json_string(&mut o, name);
            o.push('}');
        }
        Event::Peers(peers) => {
            o.push_str(r#"{"type":"peers","peers":["#);
            for (i, peer) in peers.iter().enumerate() {
                if i > 0 {
                    o.push(',');
                }
                let _ = write!(o, r#"{{"addr":"{}","busy":{},"name":"#, peer.addr, peer.busy);
                json_string(&mut o, &peer.name);
                o.push('}');
            }
            o.push_str("]}");
        }
        Event::Stats(stats) => {
            o.push_str(r#"{"type":"stats""#);
            if let Some(rtt) = stats.rtt_ms {
                let _ = write!(o, r#","rtt":{}"#, num(rtt));
            }
            o.push_str(r#","tx":["#);
            for (i, tx) in stats.tx.iter().enumerate() {
                if i > 0 {
                    o.push(',');
                }
                let _ = write!(
                    o,
                    r#"{{"slot":{},"kind":"{}","rate":{},"channels":{},"level":{},"denoise":{}"#,
                    tx.slot,
                    kind_name(tx.kind),
                    tx.format.sample_rate,
                    tx.format.channels,
                    num(tx.level),
                    tx.denoise
                );
                if let Some(remote) = &tx.remote {
                    let _ = write!(
                        o,
                        r#","remote":{{"playing":{},"buffer":{},"lost":{},"underruns":{}}}"#,
                        remote.playing,
                        remote.buffer_us as f32 / 1000.0,
                        remote.lost,
                        remote.underruns
                    );
                }
                o.push('}');
            }
            o.push_str(r#"],"rx":["#);
            for (i, rx) in stats.rx.iter().enumerate() {
                if i > 0 {
                    o.push(',');
                }
                let _ = write!(
                    o,
                    r#"{{"kind":"{}","rate":{},"channels":{},"level":{},"playing":{},"buffer":{},"jitter":{},"lost":{},"underruns":{}}}"#,
                    kind_name(rx.kind),
                    rx.format.sample_rate,
                    rx.format.channels,
                    num(rx.level),
                    rx.playing,
                    num(rx.buffer_ms),
                    num(rx.jitter_ms),
                    rx.lost,
                    rx.underruns
                );
            }
            o.push(']');
            o.push('}');
        }
        Event::Info(text) | Event::Error(text) => {
            let kind = if matches!(event, Event::Error(_)) { "error" } else { "info" };
            let _ = write!(o, r#"{{"type":"{kind}","text":"#);
            json_string(&mut o, text);
            o.push('}');
        }
    }
    o
}

/// Starts an engine and returns its handle, or 0 if no socket could be opened.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_ccptr_soundbridge_Native_start<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    name: JString<'l>,
    send: jint,
    denoise: jboolean,
    play: jboolean,
    buffer_ms: jint,
    listener: JObject<'l>,
) -> jlong {
    let name = string(&mut env, &name);
    let (Ok(vm), Ok(listener)) = (env.get_java_vm(), env.new_global_ref(listener)) else {
        return 0;
    };
    let on_event = move |event: Event| {
        let json = event_json(&event);
        // The engine thread stays attached for its lifetime; attaching again
        // is a cheap lookup.
        let Ok(mut env) = vm.attach_current_thread_as_daemon() else { return };
        let _ = env.with_local_frame(4, |env| -> jni::errors::Result<()> {
            let json = env.new_string(&json)?;
            env.call_method(&listener, "onEvent", "(Ljava/lang/String;)V", &[JValue::Object(&json)])?;
            Ok(())
        });
        // A throwing listener must not poison every later call on this thread.
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_describe();
            let _ = env.exception_clear();
        }
    };
    let config = config(name, send, denoise != 0, play != 0, buffer_ms);
    match Engine::start(config, Arc::new(aaudio::AAudioBackend), on_event) {
        Ok(engine) => Box::into_raw(Box::new(engine)) as jlong,
        Err(e) => {
            log_error(&format!("could not start the engine: {e}"));
            0
        }
    }
}

/// # Safety contract with the caller
/// `handle` must come from `start` and not have been passed to `stop`. The
/// app guards this by keeping the handle in one place, under a lock.
fn engine<'a>(handle: jlong) -> Option<&'a Engine> {
    // SAFETY: see above.
    unsafe { (handle as *const Engine).as_ref() }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_ccptr_soundbridge_Native_stop(_env: JNIEnv, _class: JClass, handle: jlong) {
    if handle != 0 {
        // SAFETY: `handle` came from `Box::into_raw` in `start`, and the app
        // never uses it again after this call. Dropping hangs up and joins
        // the engine thread.
        drop(unsafe { Box::from_raw(handle as *mut Engine) });
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_ccptr_soundbridge_Native_configure<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    handle: jlong,
    name: JString<'l>,
    send: jint,
    denoise: jboolean,
    play: jboolean,
    buffer_ms: jint,
) {
    let name = string(&mut env, &name);
    if let Some(engine) = engine(handle) {
        engine.configure(config(name, send, denoise != 0, play != 0, buffer_ms));
    }
}

/// Resolves `host` (blocking, so call it off the main thread) and starts
/// connecting. False if the name does not resolve.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_ccptr_soundbridge_Native_connect<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    handle: jlong,
    host: JString<'l>,
    port: jint,
) -> jboolean {
    let host = string(&mut env, &host);
    let port = u16::try_from(port).unwrap_or(DEFAULT_PORT);
    let addr = (host.as_str(), port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.find(SocketAddr::is_ipv4));
    match (engine(handle), addr) {
        (Some(engine), Some(addr)) => {
            engine.connect(addr);
            1
        }
        _ => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_ccptr_soundbridge_Native_disconnect(_env: JNIEnv, _class: JClass, handle: jlong) {
    if let Some(engine) = engine(handle) {
        engine.disconnect();
    }
}

/// Takes `frames` frames of 48 kHz 16-bit stereo from a direct, native-order
/// `ByteBuffer`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_ccptr_soundbridge_Native_pushSystemAudio<'l>(
    env: JNIEnv<'l>,
    _class: JClass<'l>,
    buffer: JByteBuffer<'l>,
    frames: jint,
) {
    let (Ok(ptr), Ok(capacity)) =
        (env.get_direct_buffer_address(&buffer), env.get_direct_buffer_capacity(&buffer))
    else {
        return;
    };
    let samples = (frames.max(0) as usize * system_audio::FORMAT.channels as usize).min(capacity / 2);
    if ptr.align_offset(align_of::<i16>()) != 0 {
        return;
    }
    // SAFETY: the buffer is direct, at least `samples * 2` bytes long, aligned
    // for i16 (checked), and not written by the caller during this call.
    system_audio::push(unsafe { std::slice::from_raw_parts(ptr as *const i16, samples) });
}
