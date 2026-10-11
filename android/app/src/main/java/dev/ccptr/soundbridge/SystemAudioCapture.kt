package dev.ccptr.soundbridge

import android.annotation.SuppressLint
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioPlaybackCaptureConfiguration
import android.media.AudioRecord
import android.media.projection.MediaProjection
import android.os.Process
import java.nio.ByteBuffer
import java.nio.ByteOrder
import kotlin.concurrent.thread

/**
 * Records what the phone is playing and feeds it to the engine.
 *
 * Android only offers this through [AudioRecord] with a [MediaProjection],
 * and only for apps that allow their audio to be captured: media, games and
 * unclassified playback. Calls, and apps that opt out (most DRM-protected
 * players), come through as silence.
 */
class SystemAudioCapture(projection: MediaProjection) {
    private val record: AudioRecord
    @Volatile private var running = true
    private val worker: Thread

    init {
        val capture = AudioPlaybackCaptureConfiguration.Builder(projection)
            .addMatchingUsage(AudioAttributes.USAGE_MEDIA)
            .addMatchingUsage(AudioAttributes.USAGE_GAME)
            .addMatchingUsage(AudioAttributes.USAGE_UNKNOWN)
            .build()
        // Must match what the engine expects: see crates/android/src/system_audio.rs.
        val format = AudioFormat.Builder()
            .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
            .setSampleRate(SAMPLE_RATE)
            .setChannelMask(AudioFormat.CHANNEL_IN_STEREO)
            .build()
        val smallest = AudioRecord.getMinBufferSize(
            SAMPLE_RATE, AudioFormat.CHANNEL_IN_STEREO, AudioFormat.ENCODING_PCM_16BIT,
        )
        // The caller has checked RECORD_AUDIO; lint cannot see that from here.
        @SuppressLint("MissingPermission")
        record = AudioRecord.Builder()
            .setAudioFormat(format)
            .setBufferSizeInBytes(maxOf(smallest, CHUNK_FRAMES * BYTES_PER_FRAME * 2))
            .setAudioPlaybackCaptureConfig(capture)
            .build()

        worker = thread(name = "sb-system-audio") {
            Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_AUDIO)
            val buffer = ByteBuffer.allocateDirect(CHUNK_FRAMES * BYTES_PER_FRAME)
                .order(ByteOrder.nativeOrder())
            runCatching { record.startRecording() }
            while (running) {
                // Small blocking reads: each chunk goes out as soon as it exists.
                val bytes = record.read(buffer, buffer.capacity(), AudioRecord.READ_BLOCKING)
                if (bytes < 0) break
                if (bytes > 0) Native.pushSystemAudio(buffer, bytes / BYTES_PER_FRAME)
            }
        }
    }

    fun stop() {
        running = false
        runCatching { record.stop() }
        worker.join(500)
        record.release()
    }

    private companion object {
        const val SAMPLE_RATE = 48_000
        const val BYTES_PER_FRAME = 4
        /** 5 ms. */
        const val CHUNK_FRAMES = 240
    }
}
