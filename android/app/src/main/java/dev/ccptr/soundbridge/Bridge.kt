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

enum class Send(val code: Int) {
    Nothing(Native.SEND_NOTHING),
    Microphone(Native.SEND_MIC),
    PhoneAudio(Native.SEND_SYSTEM),
}

data class Settings(
    val send: Send = Send.Nothing,
    val play: Boolean = true,
    /** Fixed jitter buffer in ms, or [Native.BUFFER_AUTO]. */
    val bufferMs: Int = Native.BUFFER_AUTO,
    /** The last address typed into the connect box. */
    val host: String = "",
)

sealed interface Link {
    data object Idle : Link
    data class Connecting(val addr: String) : Link
    data class Connected(val addr: String, val name: String) : Link
}

data class Peer(val addr: String, val name: String, val busy: Boolean)

data class TxStats(val rate: Int, val channels: Int, val level: Float)

data class RxStats(
    val rate: Int,
    val channels: Int,
    val level: Float,
    val playing: Boolean,
    val bufferMs: Float,
    val jitterMs: Float,
    val lost: Int,
    val underruns: Int,
)

/** How the stream we send is faring at the other end. */
data class RemoteStats(val playing: Boolean, val bufferMs: Float, val lost: Int, val underruns: Int)

data class Stats(
    val rttMs: Float? = null,
    val tx: TxStats? = null,
    val rx: RxStats? = null,
    val remote: RemoteStats? = null,
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
            // cannot be restored; fall back to sending nothing.
            send = Send.entries.getOrElse(prefs.getInt("send", 0)) { Send.Nothing }
                .let { if (it == Send.PhoneAudio) Send.Nothing else it },
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
            putInt("send", new.send.ordinal)
            putBoolean("play", new.play)
            putInt("bufferMs", new.bufferMs)
            putString("host", new.host)
        }
        if (old.send == Send.PhoneAudio && new.send != Send.PhoneAudio) {
            BridgeService.stopProjection(app)
        }
        if (handle != 0L) {
            Native.configure(handle, deviceName, new.send.code, new.play, new.bufferMs)
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
        handle = Native.start(deviceName, s.send.code, s.play, s.bufferMs, this)
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

    private fun parseStats(o: JSONObject) = Stats(
        rttMs = if (o.has("rtt")) o.optDouble("rtt").toFloat() else null,
        tx = o.optJSONObject("tx")?.let {
            TxStats(it.optInt("rate"), it.optInt("channels"), it.optDouble("level").toFloat())
        },
        rx = o.optJSONObject("rx")?.let {
            RxStats(
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
        remote = o.optJSONObject("remote")?.let {
            RemoteStats(
                playing = it.optBoolean("playing"),
                bufferMs = it.optDouble("buffer").toFloat(),
                lost = it.optInt("lost"),
                underruns = it.optInt("underruns"),
            )
        },
    )
}
