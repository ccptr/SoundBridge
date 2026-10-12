package dev.ccptr.soundbridge

import android.content.Context
import android.content.Intent
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import androidx.core.content.edit
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.update
import org.json.JSONObject
import java.net.InetAddress
import java.net.Inet4Address
import java.util.concurrent.Executors

const val DEFAULT_PORT = 47800

/** How the gate of [Native.DENOISE_GATE] is tuned. */
data class Gate(
    /** How far it looks ahead of the voice that opens it, which is as much delay. */
    val lookaheadMs: Int,
    /** How long it stays open after the voice stops. */
    val holdMs: Int,
    /** How sure of a voice it must be, 0..1. */
    val threshold: Float,
) {
    companion object {
        /** The least delay; a word after a pause can lose its first sound. */
        val VOICE = Gate(lookaheadMs = 40, holdMs = 250, threshold = 0.5f)

        /** Looks far enough ahead to keep the start of such a word. */
        val WHOLE_WORDS = VOICE.copy(lookaheadMs = 120)
    }
}

data class Settings(
    val sendMic: Boolean = false,
    /**
     * What is filtered out of the microphone, a `Native.DENOISE_` value:
     * nothing, background noise, or everything but speech.
     */
    val denoise: Int = Native.DENOISE_OFF,
    /** Kept while the gate is off, so a tuning survives a look at the other modes. */
    val gate: Gate = Gate.VOICE,
    /**
     * Bring speech to a steady level. On unless switched off: a phone's
     * microphone is far too quiet as it comes.
     */
    val autoGain: Boolean = true,
    /** Gain in dB, on top of the automatic one or on its own. */
    val gainDb: Int = 0,
    val sendPhoneAudio: Boolean = false,
    val play: Boolean = true,
    /** Fixed jitter buffer in ms, or [Native.BUFFER_AUTO]. */
    val bufferMs: Int = Native.BUFFER_AUTO,
    /** The last address typed into the connect box. */
    val host: String = "",
) {
    /** What is sent, in the engine's order, which is the order of its stream slots. */
    val sources: List<String>
        get() = listOfNotNull("mic".takeIf { sendMic }, "system".takeIf { sendPhoneAudio })

    val sendFlags: Int
        get() = (if (sendMic) Native.SEND_MIC else 0) or (if (sendPhoneAudio) Native.SEND_SYSTEM else 0)
}

sealed interface Link {
    data object Idle : Link
    data class Connecting(val addr: String) : Link
    data class Connected(val addr: String, val name: String) : Link
}

data class Peer(val addr: String, val name: String, val busy: Boolean)

/** How a stream we send is faring at the other end. */
data class RemoteStats(val playing: Boolean, val bufferMs: Float, val lost: Int, val underruns: Int)

data class TxStats(
    /** Index into [Settings.sources]. */
    val slot: Int,
    val rate: Int,
    val channels: Int,
    val level: Float,
    /** "off", "reduce" or "gate". */
    val denoise: String,
    val remote: RemoteStats?,
)

data class RxStats(
    /** "mic", "system" or "tone". */
    val kind: String,
    val rate: Int,
    val channels: Int,
    val level: Float,
    val playing: Boolean,
    val bufferMs: Float,
    val jitterMs: Float,
    val lost: Int,
    val underruns: Int,
)

data class Stats(
    val rttMs: Float? = null,
    /** Streams being sent. A source missing here could not be opened. */
    val tx: List<TxStats> = emptyList(),
    val rx: List<RxStats> = emptyList(),
)

data class Notice(val text: String, val error: Boolean)

data class UiState(
    val settings: Settings = Settings(),
    val link: Link = Link.Idle,
    val peers: List<Peer> = emptyList(),
    val stats: Stats = Stats(),
    val notice: Notice? = null,
    /** False if the engine could not open a socket. */
    val online: Boolean = true,
)

/**
 * The one engine of the process and everything the UI shows about it.
 *
 * The engine runs while there is a reason for it to: the app is on screen
 * (so it can find and be found), a link is up or being dialled, or phone
 * audio is being captured. [BridgeService] keeps the process alive for the
 * last two. All lifecycle decisions are made on the main thread.
 */
object Bridge : Native.Listener {
    private lateinit var app: Context
    private val main = Handler(Looper.getMainLooper())
    private val resolver = Executors.newSingleThreadExecutor()

    private val _state = MutableStateFlow(UiState())
    val state: StateFlow<UiState> = _state

    private var handle = 0L
    private var uiVisible = false
    private var multicastLock: WifiManager.MulticastLock? = null

    private val deviceName: String
        get() = Build.MODEL?.takeIf { it.isNotBlank() } ?: "Android"

    fun init(context: Context) {
        app = context.applicationContext
        val prefs = app.getSharedPreferences("settings", Context.MODE_PRIVATE)
        val saved = Settings(
            // Capturing phone audio needs fresh consent every time, so it
            // is not restored. ("send" is how the choice was stored before
            // there could be several.)
            sendMic = prefs.getBoolean("sendMic", prefs.getInt("send", 0) == 1),
            // "denoise" is the switch that voice only was before there were modes.
            denoise = prefs.getInt(
                "denoiseMode",
                if (prefs.getBoolean("denoise", false)) Native.DENOISE_GATE else Native.DENOISE_OFF,
            ),
            gate = Gate(
                lookaheadMs = prefs.getInt("gateLookaheadMs", Gate.VOICE.lookaheadMs),
                holdMs = prefs.getInt("gateHoldMs", Gate.VOICE.holdMs),
                threshold = prefs.getFloat("gateThreshold", Gate.VOICE.threshold),
            ),
            autoGain = prefs.getBoolean("autoGain", true),
            gainDb = prefs.getInt("gainDb", 0),
            play = prefs.getBoolean("play", true),
            bufferMs = prefs.getInt("bufferMs", Native.BUFFER_AUTO),
            host = prefs.getString("host", "") ?: "",
        )
        _state.value = UiState(settings = saved)
    }

    fun onUiVisible(visible: Boolean) {
        uiVisible = visible
        reconcile()
    }

    fun update(change: (Settings) -> Settings) {
        val old = _state.value.settings
        val new = change(old)
        if (new == old) return
        _state.update { it.copy(settings = new) }
        app.getSharedPreferences("settings", Context.MODE_PRIVATE).edit {
            putBoolean("sendMic", new.sendMic)
            remove("denoise")
            putInt("denoiseMode", new.denoise)
            putInt("gateLookaheadMs", new.gate.lookaheadMs)
            putInt("gateHoldMs", new.gate.holdMs)
            putFloat("gateThreshold", new.gate.threshold)
            putBoolean("autoGain", new.autoGain)
            putInt("gainDb", new.gainDb)
            putBoolean("play", new.play)
            putInt("bufferMs", new.bufferMs)
            putString("host", new.host)
        }
        if (old.sendPhoneAudio && !new.sendPhoneAudio) {
            BridgeService.stopProjection(app)
        }
        if (handle != 0L) {
            Native.configure(
                handle, deviceName, new.sendFlags, new.denoise, new.gate.lookaheadMs, new.gate.holdMs,
                new.gate.threshold, new.autoGain, new.gainDb, new.play, new.bufferMs,
            )
        }
        reconcile()
    }

    /** Connects to "host" or "host:port". Name lookup happens off the main thread. */
    fun connect(target: String) {
        val trimmed = target.trim()
        if (trimmed.isEmpty()) return
        val port = trimmed.substringAfterLast(':', "").toIntOrNull()
        val host = if (port != null) trimmed.substringBeforeLast(':') else trimmed
        resolver.execute {
            val ip = runCatching {
                InetAddress.getAllByName(host).firstOrNull { it is Inet4Address }?.hostAddress
            }.getOrNull()
            main.post {
                if (ip == null) {
                    notice("Cannot find \"$host\" on the network", error = true)
                } else if (handle != 0L) {
                    _state.update { it.copy(notice = null) }
                    Native.connect(handle, ip, port ?: DEFAULT_PORT)
                }
            }
        }
    }

    fun disconnect() {
        if (handle != 0L) Native.disconnect(handle)
    }

    fun notice(text: String, error: Boolean) {
        _state.update { it.copy(notice = Notice(text, error)) }
    }

    /** Re-evaluates what should be running, after something outside changed. */
    fun refresh() = reconcile()

    /** Starts or stops the engine and the service to match what is needed now. */
    private fun reconcile() {
        val link = _state.value.link
        val needed = uiVisible || link != Link.Idle || BridgeService.projecting
        if (needed && handle == 0L) startEngine()
        if (!needed && handle != 0L) stopEngine()

        val serviceNeeded = link != Link.Idle || BridgeService.projecting
        if (serviceNeeded && !BridgeService.running) {
            // Allowed here: a link only ever comes up while the app is on
            // screen or the service is already running.
            runCatching { app.startForegroundService(Intent(app, BridgeService::class.java)) }
        } else if (!serviceNeeded && BridgeService.running) {
            app.stopService(Intent(app, BridgeService::class.java))
            BridgeService.running = false
        }
    }

    private fun startEngine() {
        val wifi = app.getSystemService(Context.WIFI_SERVICE) as WifiManager
        // Without this the Wi-Fi chip filters out broadcast packets, and
        // discovery probes from other devices never reach us.
        multicastLock = wifi.createMulticastLock("soundbridge:discovery").apply {
            setReferenceCounted(false)
            acquire()
        }
        val s = _state.value.settings
        handle = Native.start(
            deviceName, s.sendFlags, s.denoise, s.gate.lookaheadMs, s.gate.holdMs, s.gate.threshold, s.autoGain,
            s.gainDb, s.play, s.bufferMs, this,
        )
        _state.update { it.copy(online = handle != 0L, peers = emptyList()) }
        if (handle == 0L) notice("Could not open a network socket", error = true)
    }

    private fun stopEngine() {
        // Blocks until the engine thread is gone, so no event arrives after.
        Native.stop(handle)
        handle = 0L
        multicastLock?.release()
        multicastLock = null
        _state.update { it.copy(link = Link.Idle, peers = emptyList(), stats = Stats()) }
    }

    /** Engine thread. Only touches the state flow, then hands off to the main thread. */
    override fun onEvent(json: String) {
        val event = runCatching { JSONObject(json) }.getOrNull() ?: return
        when (event.optString("type")) {
            "state" -> {
                val link = when (event.optString("state")) {
                    "connecting" -> Link.Connecting(event.optString("addr"))
                    "connected" -> Link.Connected(event.optString("addr"), event.optString("name"))
                    else -> Link.Idle
                }
                _state.update {
                    it.copy(
                        link = link,
                        stats = if (link is Link.Connected) it.stats else Stats(),
                        notice = if (link is Link.Connecting) null else it.notice,
                    )
                }
                // Stopping the engine joins this very thread, so that decision
                // cannot be made here.
                main.post { reconcile() }
            }
            "peers" -> {
                val array = event.optJSONArray("peers") ?: return
                val peers = (0 until array.length()).map { i ->
                    val p = array.getJSONObject(i)
                    Peer(p.optString("addr"), p.optString("name"), p.optBoolean("busy"))
                }
                _state.update { it.copy(peers = peers) }
            }
            "stats" -> _state.update { it.copy(stats = parseStats(event)) }
            "info" -> notice(event.optString("text"), error = false)
            "error" -> notice(event.optString("text"), error = true)
        }
    }

    private fun <T> JSONObject.list(name: String, item: (JSONObject) -> T): List<T> {
        val array = optJSONArray(name) ?: return emptyList()
        return (0 until array.length()).map { item(array.getJSONObject(it)) }
    }

    private fun parseStats(o: JSONObject) = Stats(
        rttMs = if (o.has("rtt")) o.optDouble("rtt").toFloat() else null,
        tx = o.list("tx") {
            TxStats(
                slot = it.optInt("slot"),
                rate = it.optInt("rate"),
                channels = it.optInt("channels"),
                level = it.optDouble("level").toFloat(),
                denoise = it.optString("denoise", "off"),
                remote = it.optJSONObject("remote")?.let { r ->
                    RemoteStats(
                        playing = r.optBoolean("playing"),
                        bufferMs = r.optDouble("buffer").toFloat(),
                        lost = r.optInt("lost"),
                        underruns = r.optInt("underruns"),
                    )
                },
            )
        },
        rx = o.list("rx") {
            RxStats(
                kind = it.optString("kind"),
                rate = it.optInt("rate"),
                channels = it.optInt("channels"),
                level = it.optDouble("level").toFloat(),
                playing = it.optBoolean("playing"),
                bufferMs = it.optDouble("buffer").toFloat(),
                jitterMs = it.optDouble("jitter").toFloat(),
                lost = it.optInt("lost"),
                underruns = it.optInt("underruns"),
            )
        },
    )
}
