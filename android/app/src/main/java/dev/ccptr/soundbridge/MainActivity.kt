package dev.ccptr.soundbridge

import android.Manifest
import android.app.Activity
import android.content.Context
import android.content.pm.PackageManager
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.foundation.background
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.SegmentedButton
import androidx.compose.material3.SegmentedButtonDefaults
import androidx.compose.material3.SingleChoiceSegmentedButtonRow
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.material3.dynamicLightColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import kotlin.math.log10

/** API level of Android 17, which introduced ACCESS_LOCAL_NETWORK. */
private const val ANDROID_17 = 37

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContent {
            val dark = isSystemInDarkTheme()
            val colors = when {
                Build.VERSION.SDK_INT >= 31 && dark -> dynamicDarkColorScheme(this)
                Build.VERSION.SDK_INT >= 31 -> dynamicLightColorScheme(this)
                dark -> darkColorScheme()
                else -> lightColorScheme()
            }
            MaterialTheme(colorScheme = colors) { Screen() }
        }
    }

    // The engine follows the window: on screen it finds peers and can be
    // found; off screen it only stays up for a session (see Bridge).
    override fun onStart() {
        super.onStart()
        Bridge.onUiVisible(true)
    }

    override fun onStop() {
        Bridge.onUiVisible(false)
        super.onStop()
    }
}

private fun Context.granted(permission: String) =
    ContextCompat.checkSelfPermission(this, permission) == PackageManager.PERMISSION_GRANTED

@Composable
private fun Screen() {
    val state by Bridge.state.collectAsStateWithLifecycle()
    val context = LocalContext.current

    // Ask up front for what every use of the app needs: reaching the local
    // network at all (Android 17), and showing the session notification.
    val askStartup = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestMultiplePermissions(),
    ) { Bridge.refresh() }
    LaunchedEffect(Unit) {
        val wanted = buildList {
            if (Build.VERSION.SDK_INT >= ANDROID_17) add(Manifest.permission.ACCESS_LOCAL_NETWORK)
            if (Build.VERSION.SDK_INT >= 33) add(Manifest.permission.POST_NOTIFICATIONS)
        }.filterNot(context::granted)
        if (wanted.isNotEmpty()) askStartup.launch(wanted.toTypedArray())
    }

    // Phone audio needs the screen-capture consent dialog each time; the
    // result is redeemed by the service, which must be in the foreground first.
    val askProjection = rememberLauncherForActivityResult(
        ActivityResultContracts.StartActivityForResult(),
    ) { result ->
        val data = result.data
        if (result.resultCode == Activity.RESULT_OK && data != null) {
            BridgeService.startProjection(context, result.resultCode, data)
        }
    }
    val startProjection = {
        val manager = context.getSystemService(MediaProjectionManager::class.java)
        askProjection.launch(manager.createScreenCaptureIntent())
    }

    // Both sources record audio; which one was being chosen survives the
    // permission dialog (and a rotation during it).
    var pendingSend by rememberSaveable { mutableStateOf(Send.Nothing) }
    val askMicrophone = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { granted ->
        when {
            !granted -> Bridge.notice("Without the microphone permission there is nothing to send", error = true)
            pendingSend == Send.PhoneAudio -> startProjection()
            else -> Bridge.update { it.copy(send = pendingSend) }
        }
    }
    val chooseSend = { send: Send ->
        when {
            send == Send.Nothing -> Bridge.update { it.copy(send = send) }
            !context.granted(Manifest.permission.RECORD_AUDIO) -> {
                pendingSend = send
                askMicrophone.launch(Manifest.permission.RECORD_AUDIO)
            }
            send == Send.PhoneAudio -> startProjection()
            else -> Bridge.update { it.copy(send = send) }
        }
    }

    Surface(color = MaterialTheme.colorScheme.background, modifier = Modifier.fillMaxSize()) {
        Column(
            verticalArrangement = Arrangement.spacedBy(12.dp),
            modifier = Modifier
                .safeDrawingPadding()
                .verticalScroll(rememberScrollState())
                .padding(16.dp),
        ) {
            StatusLine(state)
            state.notice?.let {
                Text(
                    it.text,
                    style = MaterialTheme.typography.bodyMedium,
                    color = if (it.error) MaterialTheme.colorScheme.error
                    else MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            if (state.link == Link.Idle) PeersCard(state) else SessionCard(state)

            Section("Send from this phone") {
                Choices(
                    options = listOf(
                        Send.Nothing to "Nothing",
                        Send.Microphone to "Microphone",
                        Send.PhoneAudio to "Phone audio",
                    ),
                    selected = state.settings.send,
                    onSelect = chooseSend,
                )
                if (state.settings.send == Send.PhoneAudio) {
                    Hint("Sends what apps are playing. Calls, and apps that forbid capture, stay silent.")
                }
            }

            Section("Play what the other device sends") {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(
                        if (state.settings.play) "On this phone's speaker or headphones" else "Off",
                        modifier = Modifier.weight(1f),
                    )
                    Switch(
                        checked = state.settings.play,
                        onCheckedChange = { on -> Bridge.update { it.copy(play = on) } },
                    )
                }
            }

            Section("Jitter buffer") {
                Choices(
                    options = listOf(
                        Native.BUFFER_AUTO to "Auto", 5 to "5 ms", 10 to "10 ms", 20 to "20 ms", 40 to "40 ms",
                    ),
                    selected = state.settings.bufferMs,
                    onSelect = { ms -> Bridge.update { it.copy(bufferMs = ms) } },
                )
                Hint(
                    if (state.settings.bufferMs == Native.BUFFER_AUTO) {
                        "Follows the network: as small as it allows, larger when it gets rough."
                    } else {
                        "Fixed. Lower is less delay; too low for the network and audio breaks up."
                    },
                )
            }
        }
    }
}

@Composable
private fun StatusLine(state: UiState) {
    val (color, text) = when (val link = state.link) {
        is Link.Connected -> Color(0xFF2EAD6D) to "Connected to ${link.name}"
        is Link.Connecting -> Color(0xFFE0A030) to "Connecting to ${link.addr.substringBefore(':')}…"
        Link.Idle ->
            if (state.online) MaterialTheme.colorScheme.primary to "Ready. Others see this phone as \"${Build.MODEL}\""
            else MaterialTheme.colorScheme.error to "Offline"
    }
    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        Box(Modifier.size(10.dp).clip(CircleShape).background(color))
        Text(text, style = MaterialTheme.typography.titleMedium)
    }
}

@Composable
private fun Section(title: String, content: @Composable ColumnScope.() -> Unit) {
    Card(modifier = Modifier.fillMaxWidth()) {
        Column(verticalArrangement = Arrangement.spacedBy(8.dp), modifier = Modifier.padding(16.dp)) {
            Text(
                title.uppercase(),
                style = MaterialTheme.typography.labelMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            content()
        }
    }
}

@Composable
private fun Hint(text: String) {
    Text(text, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
}

@Composable
private fun <T> Choices(options: List<Pair<T, String>>, selected: T, onSelect: (T) -> Unit) {
    SingleChoiceSegmentedButtonRow(modifier = Modifier.fillMaxWidth()) {
        options.forEachIndexed { index, (value, label) ->
            SegmentedButton(
                selected = value == selected,
                onClick = { onSelect(value) },
                shape = SegmentedButtonDefaults.itemShape(index, options.size),
                // The check mark would squeeze five options off a narrow screen.
                icon = {},
            ) {
                Text(label, maxLines = 1, overflow = TextOverflow.Ellipsis)
            }
        }
    }
}

@Composable
private fun PeersCard(state: UiState) {
    var host by rememberSaveable { mutableStateOf(state.settings.host) }
    val connectTyped = {
        if (host.isNotBlank()) {
            Bridge.update { it.copy(host = host.trim()) }
            Bridge.connect(host)
        }
    }
    Section("Devices on this network") {
        if (state.peers.isEmpty()) {
            Hint("Looking… Open SoundBridge on the other device, on the same network.")
        }
        for (peer in state.peers) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Column(Modifier.weight(1f)) {
                    Text(peer.name, maxLines = 1, overflow = TextOverflow.Ellipsis)
                    Hint(peer.addr.substringBefore(':'))
                }
                if (peer.busy) Hint("In use") else Button(onClick = { Bridge.connect(peer.addr) }) { Text("Connect") }
            }
        }
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            OutlinedTextField(
                value = host,
                onValueChange = { host = it },
                singleLine = true,
                placeholder = { Text("Address, e.g. 192.168.1.20") },
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri, imeAction = ImeAction.Go),
                keyboardActions = KeyboardActions(onGo = { connectTyped() }),
                modifier = Modifier.weight(1f),
            )
            OutlinedButton(onClick = connectTyped, enabled = host.isNotBlank()) { Text("Connect") }
        }
    }
}

@Composable
private fun SessionCard(state: UiState) {
    val link = state.link
    Section(if (link is Link.Connected) "Connected" else "Connecting") {
        if (link is Link.Connected) {
            val stats = state.stats
            Hint(
                link.addr.substringBefore(':') +
                    (stats.rttMs?.let { " · network round trip ${"%.1f".format(it)} ms" } ?: ""),
            )

            val tx = stats.tx
            when {
                tx != null -> {
                    Text("Sending · ${formatOf(tx.rate, tx.channels)}")
                    Meter(tx.level)
                    val remote = stats.remote
                    Hint(
                        when {
                            remote == null -> "The other end is not playing it"
                            !remote.playing -> "The other end is buffering"
                            else -> "Buffered ${"%.1f".format(remote.bufferMs)} ms at the other end" +
                                problems(remote.lost, remote.underruns)
                        },
                    )
                }
                state.settings.send == Send.Nothing -> Hint("Not sending anything.")
                else -> Hint("Not sending: the source could not be opened.")
            }

            val rx = stats.rx
            when {
                rx != null -> {
                    Text("Receiving · ${formatOf(rx.rate, rx.channels)}")
                    Meter(rx.level)
                    Hint(
                        if (!rx.playing) "Buffering…"
                        else "Buffer ${"%.1f".format(rx.bufferMs)} ms · jitter ${"%.1f".format(rx.jitterMs)} ms" +
                            problems(rx.lost, rx.underruns),
                    )
                }
                !state.settings.play -> Hint("Ignoring incoming audio.")
                else -> Hint("The other device is not sending anything.")
            }
        }
        OutlinedButton(onClick = { Bridge.disconnect() }) {
            Text(if (link is Link.Connected) "Disconnect" else "Cancel")
        }
    }
}

private fun formatOf(rate: Int, channels: Int) =
    "${"%.1f".format(rate / 1000f)} kHz ${if (channels == 1) "mono" else "stereo"}"

private fun problems(lost: Int, underruns: Int) =
    if (lost > 0 || underruns > 0) " · $lost lost · $underruns dropouts" else ""

/** A level bar covering the top 48 dB. */
@Composable
private fun Meter(level: Float) {
    val db = 20f * log10(level.coerceAtLeast(1e-6f))
    LinearProgressIndicator(
        progress = { ((db + 48f) / 48f).coerceIn(0f, 1f) },
        color = if (db > -1f) MaterialTheme.colorScheme.error else Color(0xFF2EAD6D),
        modifier = Modifier.fillMaxWidth(),
    )
}
