package dev.ccptr.soundbridge

import java.nio.ByteBuffer

/**
 * The SoundBridge engine (crates/android, Rust). The same code runs the
 * desktop app and the CLI, so every end speaks one implementation of the
 * protocol.
 *
 * Calls that take a handle must all come from one thread ([Bridge] uses the
 * main thread) and never after [stop].
 */
object Native {
    init {
        System.loadLibrary("soundbridge_android")
    }

    /** Bits of the `send` argument: any combination of sources. */
    const val SEND_MIC = 1
    const val SEND_SYSTEM = 2

    /** The `denoise` argument: nothing, background noise, or all but speech. */
    const val DENOISE_OFF = 0
    const val DENOISE_REDUCE = 1
    const val DENOISE_GATE = 2

    /** Any free jitter buffer size; the engine sizes it from the network. */
    const val BUFFER_AUTO = -1

    fun interface Listener {
        /** One JSON event, delivered on the engine's thread. Must not block. */
        fun onEvent(json: String)
    }

    /**
     * Returns a handle, or 0 if no network socket could be opened. [denoise]
     * is one of the `DENOISE_` values for the microphone, and the `gate`
     * arguments tune [DENOISE_GATE]: how far it looks ahead, how long it
     * stays open after the voice, and how sure of a voice it must be (0..1).
     * [autoGain] brings the microphone to a steady level, and [gainDb] is
     * added to that, or is the whole gain without it. [micName] and
     * [systemName] are what the other device is told to show the two sources
     * as, empty for nothing. [mute] holds the ids of the other device's
     * sources that are played nowhere; the rest play.
     */
    @JvmStatic
    external fun start(
        name: String, send: Int, micName: String, systemName: String, denoise: Int, gateLookaheadMs: Int,
        gateHoldMs: Int, gateThreshold: Float, autoGain: Boolean, gainDb: Int, mute: Array<String>, bufferMs: Int,
        listener: Listener,
    ): Long

    /** Hangs up, stops the engine and waits for its thread to finish. */
    @JvmStatic
    external fun stop(handle: Long)

    @JvmStatic
    external fun configure(
        handle: Long, name: String, send: Int, micName: String, systemName: String, denoise: Int,
        gateLookaheadMs: Int, gateHoldMs: Int, gateThreshold: Float, autoGain: Boolean, gainDb: Int,
        mute: Array<String>, bufferMs: Int,
    )

    /**
     * Asks the other device to filter and level the microphone it sends in
     * [slot] this way, as if that had been set there. The arguments are those
     * of [start] for our own microphone.
     */
    @JvmStatic
    external fun setRemoteMic(
        handle: Long, slot: Int, denoise: Int, gateLookaheadMs: Int, gateHoldMs: Int, gateThreshold: Float,
        autoGain: Boolean, gainDb: Int,
    )

    /** [host] should be an IP literal: a name would be resolved on the calling thread. */
    @JvmStatic
    external fun connect(handle: Long, host: String, port: Int): Boolean

    @JvmStatic
    external fun disconnect(handle: Long)

    /** Feeds captured playback: [frames] frames of 48 kHz 16-bit stereo in a direct buffer. */
    @JvmStatic
    external fun pushSystemAudio(buffer: ByteBuffer, frames: Int)
}
