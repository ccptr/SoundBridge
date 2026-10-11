package dev.ccptr.soundbridge

import android.Manifest
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.net.wifi.WifiManager
import android.os.IBinder
import android.os.PowerManager
import androidx.core.content.IntentCompat
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.launch

/**
 * Keeps a session alive when the app is not on screen, and holds what a
 * session needs from the system: the foreground-service types that permit
 * using the microphone and playing audio in the background, the locks that
 * keep the CPU and Wi-Fi awake, and the screen-capture grant behind
 * "phone audio".
 */
class BridgeService : Service() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private var wakeLock: PowerManager.WakeLock? = null
    private var wifiLock: WifiManager.WifiLock? = null
    private var projection: MediaProjection? = null
    private var capture: SystemAudioCapture? = null

    private val projectionCallback = object : MediaProjection.Callback() {
        // The user ended capture from the system UI, or another app took over.
        override fun onStop() {
            endProjection()
            Bridge.update { it.copy(send = Send.Nothing) }
        }
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        running = true
        getSystemService(NotificationManager::class.java).createNotificationChannel(
            NotificationChannel(CHANNEL, getString(R.string.channel_session), NotificationManager.IMPORTANCE_LOW)
                .apply { description = getString(R.string.channel_session_description) },
        )
        wakeLock = getSystemService(PowerManager::class.java)
            .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "soundbridge:session")
            .apply { acquire() }
        // Wi-Fi power save parks the radio between beacons, which shows up as
        // bursts of tens of milliseconds of jitter. This mode turns it off
        // (while the screen is on, which is all the platform will promise).
        wifiLock = (applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager)
            .createWifiLock(WifiManager.WIFI_MODE_FULL_LOW_LATENCY, "soundbridge:session")
            .apply { acquire() }

        // Follow the session: the notification text and the service types
        // both depend on it.
        scope.launch {
            Bridge.state.map { it.link to it.settings }.distinctUntilChanged().collect { foreground() }
        }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_DISCONNECT -> Bridge.disconnect()
            ACTION_STOP_PROJECTION -> {
                endProjection()
                // The service may have just lost its last reason to exist.
                Bridge.refresh()
                if (!running) return START_NOT_STICKY
            }
            ACTION_PROJECT -> beginProjection(intent)
        }
        foreground()
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        scope.cancel()
        endProjection()
        wakeLock?.release()
        wifiLock?.release()
        super.onDestroy()
    }

    /** (Re)enters the foreground with the types the current session needs. */
    private fun foreground(withProjection: Boolean = projecting) {
        val state = Bridge.state.value
        val linked = state.link != Link.Idle
        val micGranted =
            checkSelfPermission(Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED
        var types = 0
        if (linked && state.settings.send == Send.Microphone && micGranted) {
            types = types or ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
        }
        if (withProjection) types = types or ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION
        // Also the fallback: a service must declare at least one type.
        if ((linked && state.settings.play) || types == 0) {
            types = types or ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK
        }
        try {
            startForeground(NOTIFICATION_ID, notification(state.link), types)
        } catch (e: Exception) {
            // Refused (started from the background, or a type's permission is
            // missing). Without foreground status the session would be cut
            // off at some arbitrary later moment; say so now instead.
            Bridge.notice("Android would not let the session run in the background: ${e.message}", error = true)
        }
    }

    private fun notification(link: Link): Notification {
        val open = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE,
        )
        val text = when (link) {
            is Link.Connected -> getString(R.string.notification_connected, link.name)
            is Link.Connecting -> getString(R.string.notification_connecting)
            Link.Idle -> getString(R.string.notification_capturing)
        }
        val builder = Notification.Builder(this, CHANNEL)
            .setSmallIcon(R.drawable.ic_stat_bridge)
            .setContentTitle(getString(R.string.app_name))
            .setContentText(text)
            .setContentIntent(open)
            .setOngoing(true)
        if (link != Link.Idle) {
            val disconnect = PendingIntent.getService(
                this, 1,
                Intent(this, BridgeService::class.java).setAction(ACTION_DISCONNECT),
                PendingIntent.FLAG_IMMUTABLE,
            )
            builder.addAction(
                Notification.Action.Builder(null, getString(R.string.action_disconnect), disconnect).build(),
            )
        }
        return builder.build()
    }

    private fun beginProjection(intent: Intent) {
        endProjection()
        val resultCode = intent.getIntExtra(EXTRA_RESULT_CODE, 0)
        val data = IntentCompat.getParcelableExtra(intent, EXTRA_DATA, Intent::class.java) ?: return
        // The order is the platform's rule: the service must already be in
        // the foreground with the mediaProjection type before the grant is
        // redeemed.
        foreground(withProjection = true)
        try {
            val granted = getSystemService(MediaProjectionManager::class.java)
                .getMediaProjection(resultCode, data) ?: return
            granted.registerCallback(projectionCallback, null)
            projection = granted
            capture = SystemAudioCapture(granted)
            projecting = true
            Bridge.update { it.copy(send = Send.PhoneAudio) }
        } catch (e: Exception) {
            endProjection()
            Bridge.notice("Could not capture phone audio: ${e.message}", error = true)
        }
    }

    private fun endProjection() {
        projecting = false
        capture?.stop()
        capture = null
        projection?.let {
            it.unregisterCallback(projectionCallback)
            it.stop()
        }
        projection = null
    }

    companion object {
        private const val CHANNEL = "session"
        private const val NOTIFICATION_ID = 1
        private const val ACTION_DISCONNECT = "dev.ccptr.soundbridge.DISCONNECT"
        private const val ACTION_PROJECT = "dev.ccptr.soundbridge.PROJECT"
        private const val ACTION_STOP_PROJECTION = "dev.ccptr.soundbridge.STOP_PROJECTION"
        private const val EXTRA_RESULT_CODE = "resultCode"
        private const val EXTRA_DATA = "data"

        /** Set on create; cleared by [Bridge] when it stops the service. Main thread only. */
        var running = false

        /** Phone audio is being captured. Main thread only. */
        var projecting = false
            private set

        /** Redeems a screen-capture consent result and starts capturing phone audio. */
        fun startProjection(context: Context, resultCode: Int, data: Intent) {
            context.startForegroundService(
                Intent(context, BridgeService::class.java)
                    .setAction(ACTION_PROJECT)
                    .putExtra(EXTRA_RESULT_CODE, resultCode)
                    .putExtra(EXTRA_DATA, data),
            )
        }

        fun stopProjection(context: Context) {
            if (!running) return
            context.startService(
                Intent(context, BridgeService::class.java).setAction(ACTION_STOP_PROJECTION),
            )
        }
    }
}
