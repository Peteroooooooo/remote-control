package com.rc.android.state

import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.Surface
import androidx.compose.ui.graphics.asImageBitmap
import androidx.lifecycle.ViewModel
import com.rc.android.model.AppLayer
import com.rc.android.model.CursorGlyph
import com.rc.android.model.DeviceItem
import com.rc.android.model.LiveStats
import com.rc.android.model.MonitorInfo
import com.rc.android.model.PasteFeedback
import com.rc.android.model.QualityPrefs
import com.rc.android.model.RcUiState
import com.rc.android.model.RunningWindowItem
import com.rc.android.model.SessionPhase
import com.rc.android.model.StreamMeta
import com.rc.android.model.TouchMode
import com.rc.android.model.VirtualDisplayState
import com.rc.android.nativebridge.NativeCallback
import com.rc.android.nativebridge.RcNative
import com.rc.android.update.AppUpdater
import com.rc.android.video.VideoDecoder
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import org.json.JSONArray
import org.json.JSONObject
import java.net.Inet4Address
import java.nio.ByteBuffer
import java.util.concurrent.Executors

class RcViewModel : ViewModel(), NativeCallback {
    private val _ui = MutableStateFlow(RcUiState())
    val ui: StateFlow<RcUiState> = _ui.asStateFlow()

    private val mainHandler = Handler(Looper.getMainLooper())
    private val ioExecutor = Executors.newCachedThreadPool { r ->
        Thread(r, "rc-ui-io").apply { isDaemon = true }
    }

    @Volatile
    private var initialized = false

    @Volatile
    private var viewportWidthPx: Int = 1080

    @Volatile
    private var viewportHeightPx: Int = 2400

    private var physicalTouchMode: TouchMode = TouchMode.Trackpad
    private var virtualTouchMode: TouchMode = TouchMode.Direct
    private var appContext: Context? = null
    private var appUpdater: AppUpdater? = null

    /** Lives with the ViewModel so a download keeps going across activity recreation. */
    fun updater(context: Context): AppUpdater =
        appUpdater ?: AppUpdater(context).also { appUpdater = it }

    val decoder = VideoDecoder(
        onRequestKeyframe = { RcNative.nativeRequestKeyframe() },
        onFirstFrameRendered = {
            mainHandler.post {
                _ui.update { it.copy(hasFirstFrame = true) }
            }
        },
        onOutputSizeChanged = { outW, outH ->
            mainHandler.post {
                val cur = _ui.value.streamMeta
                if (cur.width != outW || cur.height != outH) {
                    _ui.update {
                        it.copy(streamMeta = it.streamMeta.copy(width = outW, height = outH))
                    }
                }
            }
        },
        onDecodeError = { msg ->
            mainHandler.post {
                showToast(msg)
            }
        },
    )

    fun ensureInitialized(context: Context) {
        val appCtx = context.applicationContext
        appContext = appCtx
        if (initialized) {
            triggerScan(appCtx)
            return
        }
        initialized = true
        val savedQuality = loadQualityPrefs(appCtx)
        val modelName = buildDeviceName()
        val customName = appCtx.getSharedPreferences(SETTINGS_PREFS, Context.MODE_PRIVATE)
            .getString("device_name", "").orEmpty()
        _ui.update { it.copy(quality = savedQuality, customName = customName, modelName = modelName) }
        val deviceName = customName.ifEmpty { modelName }
        val broadcasts = collectBroadcastAddresses(appCtx)
        ioExecutor.execute {
            val err = RcNative.nativeInit(appCtx.filesDir.absolutePath, deviceName, this)
            if (err.isNotEmpty()) {
                Log.e(TAG, "nativeInit failed: $err")
                _ui.update { it.copy(initError = err) }
                return@execute
            }
            if (broadcasts.isNotEmpty()) {
                RcNative.nativeSetBroadcasts(broadcasts.joinToString(","))
            }
            RcNative.nativeTriggerScan()
        }
    }

    fun refreshBroadcasts(context: Context) {
        val broadcasts = collectBroadcastAddresses(context)
        if (broadcasts.isNotEmpty()) {
            ioExecutor.execute {
                RcNative.nativeSetBroadcasts(broadcasts.joinToString(","))
            }
        }
    }

    fun triggerScan(context: Context) {
        val broadcasts = collectBroadcastAddresses(context)
        ioExecutor.execute {
            if (broadcasts.isNotEmpty()) {
                RcNative.nativeSetBroadcasts(broadcasts.joinToString(","))
            }
            RcNative.nativeTriggerScan()
        }
    }

    fun pasteCode(raw: String) {
        val text = raw.trim()
        if (text.isEmpty()) {
            _ui.update { it.copy(pasteFeedback = PasteFeedback(false, "请先粘贴连接码或自建中转码")) }
            return
        }
        _ui.update { it.copy(pasteBusy = true, pasteFeedback = null) }
        ioExecutor.execute {
            val json = RcNative.nativePasteCode(text)
            val parsed = runCatching { JSONObject(json) }.getOrNull()
            val ok = parsed?.optBoolean("ok", false) ?: false
            val msg = parsed?.optString("message", "解析失败") ?: "解析失败"
            _ui.update {
                it.copy(
                    pasteBusy = false,
                    pasteFeedback = PasteFeedback(ok, msg),
                )
            }
        }
    }

    fun clearPasteFeedback() {
        _ui.update { it.copy(pasteFeedback = null) }
    }

    /** Blank resets to the phone model. Takes effect on computers right away. */
    fun setDeviceName(raw: String) {
        val name = raw.replace(Regex("[\\t\\r\\n]"), " ").trim().take(MAX_NAME_LEN)
        val st = _ui.value
        val custom = if (name == st.modelName) "" else name
        if (custom == st.customName) return
        _ui.update { it.copy(customName = custom) }
        appContext?.getSharedPreferences(SETTINGS_PREFS, Context.MODE_PRIVATE)
            ?.edit()?.putString("device_name", custom)?.apply()
        val shown = custom.ifEmpty { st.modelName }
        ioExecutor.execute { RcNative.nativeSetDeviceName(shown) }
    }

    fun rememberDevice(idHex: String) {
        ioExecutor.execute { RcNative.nativeRememberDevice(idHex) }
    }

    fun agreeRemember(idHex: String) {
        ioExecutor.execute { RcNative.nativeAgreeRemember(idHex) }
    }

    fun refuseRemember(idHex: String) {
        ioExecutor.execute { RcNative.nativeRefuseRemember(idHex) }
    }

    fun cancelRemember(idHex: String) {
        ioExecutor.execute { RcNative.nativeCancelRemember(idHex) }
    }

    fun forgetDevice(idHex: String) {
        ioExecutor.execute { RcNative.nativeForgetDevice(idHex) }
    }

    fun retryDevice(idHex: String) {
        ioExecutor.execute { RcNative.nativeRetryDevice(idHex) }
    }

    fun setTouchMode(mode: TouchMode) {
        val isVirtual = _ui.value.virtualDisplay.active
        if (isVirtual) {
            virtualTouchMode = mode
        } else {
            physicalTouchMode = mode
        }
        _ui.update { it.copy(touchMode = mode) }
    }

    fun updateViewport(widthPx: Int, heightPx: Int, imeVisible: Boolean = false) {
        if (widthPx <= 0 || heightPx <= 0 || imeVisible) return
        if (widthPx == viewportWidthPx && heightPx == viewportHeightPx) return
        viewportWidthPx = widthPx
        viewportHeightPx = heightPx
    }

    fun connectToDevice(device: DeviceItem, screenWidthPx: Int, screenHeightPx: Int) {
        decoder.resetSession()
        viewportWidthPx = screenWidthPx.coerceAtLeast(1)
        viewportHeightPx = screenHeightPx.coerceAtLeast(1)
        // Mobile users routinely zoom in to inspect desktop text: start at native resolution
        // up to 4K (avoiding blurry non-integer downscaling like 2560x1440 -> 2260x1272) while
        // keeping fit_w/fit_h > 0 so the host's AutoSize can still step down on slow links.
        val initFitW = 3840
        val initFitH = 2160
        _ui.update {
            it.copy(
                layer = AppLayer.Hub,
                activeDevice = device,
                sessionPhase = SessionPhase.Connecting,
                sessionMessage = "正在连接 ${device.name}…",
                monitors = emptyList(),
                currentMonitorIndex = 0,
                windows = emptyList(),
                activeWindow = null,
                virtualDisplay = VirtualDisplayState(),
                appIcons = emptyMap(),
                windowThumbs = emptyMap(),
                liveStats = LiveStats(),
                hintCode = 0,
                hintMessage = "",
                hasFirstFrame = false,
                touchMode = physicalTouchMode,
                cursorGlyphs = emptyMap(),
                currentCursorId = 0,
                cursorVisible = true,
                cursorMetaKind = 0,
                remoteEditable = false,
                remoteEditableTick = 0L,
            )
        }
        val initRelayOnly = _ui.value.quality.relayOnly
        ioExecutor.execute {
            val err = RcNative.nativeConnect(device.id, initFitW, initFitH, 60, initRelayOnly)
            if (err.isEmpty()) {
                // Before the handshake goes out: the Hub preview starts at its low preset.
                sendHostQualityNow()
            } else {
                _ui.update {
                    it.copy(
                        sessionPhase = SessionPhase.Error,
                        sessionMessage = err,
                    )
                }
            }
        }
    }

    fun reconnectActive(screenWidthPx: Int, screenHeightPx: Int) {
        val dev = _ui.value.activeDevice ?: return
        connectToDevice(dev, screenWidthPx, screenHeightPx)
    }

    fun enterFullscreen() {
        val st = _ui.value
        if (st.layer == AppLayer.Hub) {
            if (st.activeWindow != null || st.virtualDisplay.active) {
                ioExecutor.execute { RcNative.nativeFocusWindow(0L, 0, 0) }
            }
            _ui.update {
                it.copy(
                    layer = AppLayer.Fullscreen,
                    activeWindow = null,
                    touchMode = physicalTouchMode,
                )
            }
            syncHostQuality()
        }
    }

    fun returnToHub() {
        val st = _ui.value
        if (st.layer == AppLayer.Fullscreen) {
            val needRestore = st.activeWindow != null || st.virtualDisplay.active
            ioExecutor.execute {
                RcNative.nativeReleaseAll()
                if (needRestore) {
                    RcNative.nativeFocusWindow(0L, 0, 0)
                }
            }
            _ui.update {
                it.copy(
                    layer = AppLayer.Hub,
                    activeWindow = null,
                    virtualDisplay = VirtualDisplayState(),
                )
            }
            syncHostQuality()
        }
    }

    fun disconnectAndReturnToDevices() {
        decoder.resetSession()
        ioExecutor.execute { RcNative.nativeDisconnect() }
        _ui.update {
            it.copy(
                layer = AppLayer.Devices,
                activeDevice = null,
                sessionPhase = SessionPhase.Idle,
                sessionMessage = "",
                monitors = emptyList(),
                currentMonitorIndex = 0,
                windows = emptyList(),
                activeWindow = null,
                virtualDisplay = VirtualDisplayState(),
                appIcons = emptyMap(),
                windowThumbs = emptyMap(),
                liveStats = LiveStats(),
                hintCode = 0,
                hintMessage = "",
                hasFirstFrame = false,
                touchMode = physicalTouchMode,
                cursorGlyphs = emptyMap(),
                currentCursorId = 0,
                cursorVisible = true,
                cursorMetaKind = 0,
                remoteEditable = false,
                remoteEditableTick = 0L,
            )
        }
    }

    fun selectMonitor(index: Int) {
        val st = _ui.value
        val targetMon = st.monitors.getOrNull(index)
        val curMon = st.monitors.getOrNull(st.currentMonitorIndex)
        if (targetMon != null && targetMon.width > 0 && targetMon.height > 0) {
            val targetWide = targetMon.width >= targetMon.height
            val curWide = if (curMon != null && curMon.width > 0 && curMon.height > 0) {
                curMon.width >= curMon.height
            } else {
                st.streamMeta.width >= st.streamMeta.height
            }
            if (targetWide != curWide) {
                decoder.awaitMonitorOrientation(targetWide)
            } else {
                decoder.clearExpectedOrientation()
            }
        }
        _ui.update { it.copy(currentMonitorIndex = index, activeWindow = null) }
        ioExecutor.execute { RcNative.nativeSelectMonitor(index) }
    }

    fun focusWindow(window: RunningWindowItem) {
        val vw = viewportWidthPx
        val vh = viewportHeightPx
        val stNow = _ui.value
        val alreadyPreviewing = stNow.layer == AppLayer.Hub && stNow.activeWindow?.hwnd == window.hwnd
        val targetMon = stNow.monitors.getOrNull(window.monitor)
        if (targetMon != null && targetMon.width > 0 && targetMon.height > 0) {
            val targetWide = targetMon.width >= targetMon.height
            val curWide = stNow.streamMeta.width >= stNow.streamMeta.height
            if (targetWide != curWide) {
                decoder.awaitMonitorOrientation(targetWide)
            } else {
                decoder.clearExpectedOrientation()
            }
        }
        ioExecutor.execute { RcNative.nativeFocusWindow(window.hwnd, vw, vh) }
        _ui.update { st ->
            val nextMonitor = if (window.monitor in st.monitors.indices) {
                window.monitor
            } else {
                st.currentMonitorIndex
            }
            // First tap brings the window to front for live preview in the Hub TV;
            // tapping the selected card again (or double-tapping) enters Fullscreen.
            val nextLayer = if (alreadyPreviewing && st.sessionPhase == SessionPhase.Connected) {
                AppLayer.Fullscreen
            } else {
                st.layer
            }
            st.copy(
                activeWindow = window,
                currentMonitorIndex = nextMonitor,
                layer = nextLayer,
                touchMode = physicalTouchMode,
            )
        }
        syncHostQuality()
    }

    fun requestKeyframe() {
        ioExecutor.execute { RcNative.nativeRequestKeyframe() }
    }

    fun setQuality(transform: (QualityPrefs) -> QualityPrefs) {
        val prev = _ui.value.quality
        val next = transform(prev)
        if (next == prev) return
        _ui.update { it.copy(quality = next) }
        appContext?.let { saveQualityPrefs(it, next) }
        if (next.relayOnly != prev.relayOnly) {
            ioExecutor.execute { RcNative.nativeSetRelayOnly(next.relayOnly) }
        }
        if (next.resPreset != prev.resPreset ||
            next.fpsPreset != prev.fpsPreset ||
            next.mbpsPreset != prev.mbpsPreset ||
            next.sharpText != prev.sharpText
        ) {
            syncHostQuality()
        }
    }

    private val hostQualityLock = Any()

    /**
     * Tells the host which picture to stream for the current layer: the Hub's small live
     * preview gets a fixed low preset, Fullscreen gets the user's choice. Reads the state
     * when it runs (under a lock) so quick Hub/Fullscreen toggles always end on the latest.
     */
    private fun syncHostQuality() {
        ioExecutor.execute { sendHostQualityNow() }
    }

    private fun sendHostQualityNow() {
        synchronized(hostQualityLock) {
            val st = _ui.value
            if (st.layer == AppLayer.Hub) {
                RcNative.nativeSetQuality(0, 0, HUB_PREVIEW_SHORT, HUB_PREVIEW_FPS, HUB_PREVIEW_KBPS, false, false)
                return
            }
            val q = st.quality
            val (fitW, fitH, shortSide) = when (q.resPreset) {
                0 -> Triple(3840, 2160, 0) // 自动：默认原生清晰度（上限 4K），弱网时电脑自动降档
                1 -> Triple(0, 0, 0)       // 原生：锁定原始分辨率不降档
                else -> Triple(0, 0, q.resPreset.coerceIn(480, 2160))
            }
            val autoFps = q.fpsPreset == 0
            val fps = if (autoFps) 60 else q.fpsPreset.coerceIn(15, 240)
            val kbps = if (q.mbpsPreset <= 0) 0 else q.mbpsPreset * 1000
            RcNative.nativeSetQuality(fitW, fitH, shortSide, fps, kbps, false, autoFps)
        }
    }

    private fun loadQualityPrefs(ctx: Context): QualityPrefs {
        val sp = ctx.getSharedPreferences("rc_quality_prefs", Context.MODE_PRIVATE)
        val rawRes = sp.getInt("res", 0)
        val rawFps = sp.getInt("fps", 0)
        val rawMbps = sp.getInt("mbps", 0)
        val rawRelayOnly = sp.getBoolean("relay_only", false)
        return QualityPrefs(
            resPreset = if (rawRes in setOf(0, 1, 1440, 1080, 720)) rawRes else 0,
            fpsPreset = if (rawFps in setOf(0, 30, 60, 120, 144)) rawFps else 0,
            mbpsPreset = if (rawMbps in setOf(0, 10, 20, 40, 80)) rawMbps else 0,
            sharpText = false,
            relayOnly = rawRelayOnly,
        )
    }

    private fun saveQualityPrefs(ctx: Context, q: QualityPrefs) {
        ctx.getSharedPreferences("rc_quality_prefs", Context.MODE_PRIVATE).edit()
            .putInt("res", q.resPreset)
            .putInt("fps", q.fpsPreset)
            .putInt("mbps", q.mbpsPreset)
            .putBoolean("relay_only", q.relayOnly)
            .apply()
    }

    fun sendMouseMove(normX: Float, normY: Float) {
        val x = (normX.coerceIn(0f, 1f) * 65535f).toInt()
        val y = (normY.coerceIn(0f, 1f) * 65535f).toInt()
        RcNative.nativeSendMouseMove(x, y)
    }

    fun sendMouseDelta(dx: Int, dy: Int) {
        if (dx != 0 || dy != 0) {
            RcNative.nativeSendMouseDelta(dx, dy)
        }
    }

    fun sendMouseButton(button: Int, down: Boolean) {
        RcNative.nativeSendMouseButton(button, down)
    }

    fun sendTouch(id: Int, phase: Int, normX: Float, normY: Float) {
        val x = (normX.coerceIn(0f, 1f) * 65535f).toInt()
        val y = (normY.coerceIn(0f, 1f) * 65535f).toInt()
        RcNative.nativeSendTouch(id, phase, x, y)
    }

    fun sendTouchpad(phase: Int, dxTenthDp: Int, dyTenthDp: Int, scaleMilli: Int = 1000) {
        RcNative.nativeSendTouchpad(phase, dxTenthDp, dyTenthDp, scaleMilli)
    }

    fun sendPen(
        phase: Int,
        normX: Float,
        normY: Float,
        pressure: Float,
        tiltXDeg: Int = 0,
        tiltYDeg: Int = 0,
        flags: Int = 0,
    ) {
        val x = (normX.coerceIn(0f, 1f) * 65535f).toInt()
        val y = (normY.coerceIn(0f, 1f) * 65535f).toInt()
        val p = (pressure.coerceIn(0f, 1f) * 1024f).toInt()
        RcNative.nativeSendPen(phase, x, y, p, tiltXDeg, tiltYDeg, flags)
    }

    fun sendWheel(delta: Int, horizontal: Boolean = false) {
        RcNative.nativeSendWheel(delta, horizontal)
    }

    fun releaseAllInput() {
        ioExecutor.execute { RcNative.nativeReleaseAll() }
    }

    fun sendText(text: String) {
        if (text.isNotEmpty()) {
            ioExecutor.execute { RcNative.nativeSendText(text) }
        }
    }

    fun sendKey(scan: Int, down: Boolean) {
        RcNative.nativeSendKey(scan, down)
    }

    fun tapKey(scan: Int) {
        ioExecutor.execute {
            RcNative.nativeSendKey(scan, true)
            RcNative.nativeSendKey(scan, false)
        }
    }

    fun tapChord(modScan: Int, keyScan: Int) {
        ioExecutor.execute {
            RcNative.nativeSendKey(modScan, true)
            RcNative.nativeSendKey(keyScan, true)
            RcNative.nativeSendKey(keyScan, false)
            RcNative.nativeSendKey(modScan, false)
        }
    }

    fun attachSurface(surface: Surface) {
        decoder.attachSurface(surface)
    }

    fun detachSurface(surface: Surface) {
        decoder.detachSurface(surface)
    }

    fun showToast(message: String) {
        _ui.update { it.copy(toastMessage = message) }
        mainHandler.removeCallbacks(clearToastRunnable)
        mainHandler.postDelayed(clearToastRunnable, 2400L)
    }

    private val clearToastRunnable = Runnable {
        _ui.update { it.copy(toastMessage = null) }
    }

    // ---- NativeCallback (invoked from Rust Tokio/worker threads) ----

    override fun onDevicesUpdated(devicesJson: String) {
        val parsed = runCatching { JSONObject(devicesJson) }.getOrNull() ?: return
        val localId = parsed.optString("device_id", "")
        val ownRelayCount = parsed.optInt("own_relay_count", 0)
        val arr = parsed.optJSONArray("devices") ?: JSONArray()
        val list = ArrayList<DeviceItem>(arr.length())
        for (i in 0 until arr.length()) {
            val o = arr.optJSONObject(i) ?: continue
            list.add(
                DeviceItem(
                    id = o.optString("id"),
                    shortId = o.optString("short_id"),
                    name = o.optString("name"),
                    status = o.optString("status"),
                    rttMs = o.optInt("rtt_ms"),
                    path = o.optString("path"),
                    onLan = o.optBoolean("on_lan"),
                    remembered = o.optBoolean("remembered"),
                    hasSelfHostedRelay = o.optBoolean("has_self_hosted_relay", false),
                    ask = o.optString("ask", "none"),
                    error = o.optString("error"),
                ),
            )
        }
        _ui.update { st ->
            val updatedActive = st.activeDevice?.let { cur -> list.find { it.id == cur.id } ?: cur }
            st.copy(
                localDeviceId = localId,
                ownRelayCount = ownRelayCount,
                devices = list,
                activeDevice = updatedActive,
            )
        }
    }

    override fun onSessionState(state: String, message: String) {
        val phase = when (state) {
            "connecting" -> SessionPhase.Connecting
            "waiting_consent" -> SessionPhase.WaitingConsent
            "connected" -> SessionPhase.Connected
            "paused" -> SessionPhase.Paused
            "disconnected" -> SessionPhase.Disconnected
            "error" -> SessionPhase.Error
            else -> SessionPhase.Idle
        }
        _ui.update {
            it.copy(
                sessionPhase = phase,
                sessionMessage = message,
            )
        }
        if (phase == SessionPhase.Connected) {
            syncHostQuality()
        }
    }

    override fun onStreamInfo(width: Int, height: Int, encoder: String, fps: Int, maxKbps: Int) {
        _ui.update { st ->
            // Once video is live, let VideoDecoder.onOutputSizeChanged drive rendered width/height
            // atomically when the new monitor's decoded frame hits SurfaceTexture (prevents
            // stretching the previous monitor's frame to the new monitor's aspect ratio).
            val nextW = if (st.hasFirstFrame) st.streamMeta.width else width.coerceAtLeast(1)
            val nextH = if (st.hasFirstFrame) st.streamMeta.height else height.coerceAtLeast(1)
            st.copy(
                streamMeta = StreamMeta(
                    width = nextW,
                    height = nextH,
                    encoder = encoder,
                    fps = fps,
                    maxKbps = maxKbps,
                ),
            )
        }
    }

    override fun onMonitors(monitorsJson: String, currentIndex: Int) {
        val arr = runCatching { JSONArray(monitorsJson) }.getOrNull() ?: return
        val list = ArrayList<MonitorInfo>(arr.length())
        for (i in 0 until arr.length()) {
            val o = arr.optJSONObject(i) ?: continue
            list.add(
                MonitorInfo(
                    index = o.optInt("index", i),
                    name = o.optString("name"),
                    x = o.optInt("x"),
                    y = o.optInt("y"),
                    width = o.optInt("width"),
                    height = o.optInt("height"),
                    primary = o.optBoolean("primary"),
                    rotation = o.optInt("rotation"),
                ),
            )
        }
        _ui.update {
            it.copy(
                monitors = list,
                currentMonitorIndex = currentIndex.coerceIn(0, (list.size - 1).coerceAtLeast(0)),
            )
        }
    }

    override fun onWindows(windowsJson: String) {
        val arr = runCatching { JSONArray(windowsJson) }.getOrNull() ?: return
        val list = ArrayList<RunningWindowItem>(arr.length())
        for (i in 0 until arr.length()) {
            val o = arr.optJSONObject(i) ?: continue
            val hwnd = o.optLong("hwnd", 0L)
            list.add(
                RunningWindowItem(
                    hwnd = hwnd,
                    id = hwnd.toString(),
                    appKey = o.optString("app_key"),
                    appName = o.optString("app_name"),
                    title = o.optString("title"),
                    monitor = o.optInt("monitor", 0),
                    minimized = o.optBoolean("minimized", false),
                    rx = o.optDouble("rx", 0.0).toFloat().coerceIn(0f, 1f),
                    ry = o.optDouble("ry", 0.0).toFloat().coerceIn(0f, 1f),
                    rw = o.optDouble("rw", 0.0).toFloat().coerceIn(0f, 1f),
                    rh = o.optDouble("rh", 0.0).toFloat().coerceIn(0f, 1f),
                ),
            )
        }
        _ui.update { st ->
            val liveHwnds = list.mapTo(HashSet(list.size)) { it.hwnd }
            val prunedThumbs = st.windowThumbs.filterKeys { it in liveHwnds }
            val nextActiveWindow = st.activeWindow?.let { cur ->
                list.firstOrNull { it.hwnd == cur.hwnd }
            }
            st.copy(
                windows = list,
                windowThumbs = prunedThumbs,
                activeWindow = nextActiveWindow,
            )
        }
    }

    override fun onAppIcon(appKey: String, pngBytes: ByteArray) {
        if (pngBytes.isEmpty()) return
        val bmp = runCatching {
            BitmapFactory.decodeByteArray(pngBytes, 0, pngBytes.size)?.asImageBitmap()
        }.getOrNull() ?: return
        _ui.update { it.copy(appIcons = it.appIcons + (appKey to bmp)) }
    }

    override fun onWindowThumb(hwnd: Long, pngBytes: ByteArray) {
        if (pngBytes.isEmpty()) return
        val bmp = runCatching {
            BitmapFactory.decodeByteArray(pngBytes, 0, pngBytes.size)?.asImageBitmap()
        }.getOrNull() ?: return
        _ui.update { it.copy(windowThumbs = it.windowThumbs + (hwnd to bmp)) }
    }

    override fun onStats(fps: Int, kbps: Int, rttMs: Int, pathKind: String) {
        _ui.update {
            it.copy(
                liveStats = LiveStats(
                    fps = fps,
                    kbps = kbps,
                    rttMs = rttMs,
                    pathKind = pathKind,
                ),
            )
        }
    }

    override fun onHint(code: Int, message: String) {
        _ui.update {
            it.copy(
                hintCode = code,
                hintMessage = message,
            )
        }
    }

    override fun onNotice(message: String) {
        if (message.isNotEmpty()) {
            mainHandler.post { showToast(message) }
        }
    }

    override fun onCursorShape(
        id: Int,
        width: Int,
        height: Int,
        hotX: Int,
        hotY: Int,
        rgba: ByteArray,
    ) {
        if (width <= 0 || height <= 0 || rgba.size < width * height * 4) return
        val glyph = runCatching {
            val bmp = Bitmap.createBitmap(width, height, Bitmap.Config.ARGB_8888)
            bmp.copyPixelsFromBuffer(ByteBuffer.wrap(rgba))
            CursorGlyph(
                bitmap = bmp.asImageBitmap(),
                width = width,
                height = height,
                hotX = hotX.coerceIn(0, width - 1),
                hotY = hotY.coerceIn(0, height - 1),
            )
        }.getOrNull() ?: return
        _ui.update { st ->
            val trimmed = if (st.cursorGlyphs.size >= 64 && id !in st.cursorGlyphs) {
                val keys = st.cursorGlyphs.keys.drop(1)
                st.cursorGlyphs.filterKeys { it in keys }
            } else {
                st.cursorGlyphs
            }
            st.copy(
                cursorGlyphs = trimmed + (id to glyph),
                currentCursorId = id,
                cursorVisible = true,
            )
        }
    }

    override fun onCursorUse(id: Int) {
        _ui.update { st ->
            st.copy(
                currentCursorId = id,
                cursorVisible = true,
            )
        }
    }

    override fun onCursorHidden() {
        _ui.update { it.copy(cursorVisible = false, cursorMetaKind = 0) }
    }

    override fun onCursorMeta(kind: Int) {
        _ui.update { it.copy(cursorMetaKind = kind) }
    }

    override fun onTextFocus(editable: Boolean) {
        _ui.update { st ->
            st.copy(
                remoteEditable = editable,
                remoteEditableTick = if (editable) st.remoteEditableTick + 1L else st.remoteEditableTick,
            )
        }
    }

    override fun onVirtualDisplay(active: Boolean, width: Int, height: Int, dpiPercent: Int) {
        _ui.update { st ->
            val wasActive = st.virtualDisplay.active
            val nextMode = when {
                active && !wasActive -> virtualTouchMode
                !active && wasActive -> physicalTouchMode
                else -> st.touchMode
            }
            st.copy(
                virtualDisplay = VirtualDisplayState(
                    active = active,
                    width = width,
                    height = height,
                    dpiPercent = dpiPercent,
                ),
                touchMode = nextMode,
            )
        }
    }

    override fun onRouteChanged(relayOnly: Boolean) {
        mainHandler.post {
            val prev = _ui.value.quality
            if (prev.relayOnly != relayOnly) {
                val next = prev.copy(relayOnly = relayOnly)
                _ui.update { it.copy(quality = next) }
                appContext?.let { saveQualityPrefs(it, next) }
            }
        }
    }

    override fun onVideoFrame(
        codec: Int,
        key: Boolean,
        width: Int,
        height: Int,
        hostUs: Long,
        data: ByteArray,
    ) {
        if (width > 0 && height > 0) {
            val cur = _ui.value.streamMeta
            if (cur.width != width || cur.height != height) {
                _ui.update {
                    it.copy(streamMeta = it.streamMeta.copy(width = width, height = height))
                }
            }
        }
        decoder.submitFrame(codec, key, width, height, data)
    }

    override fun onCleared() {
        super.onCleared()
        decoder.destroy()
        ioExecutor.shutdown()
    }

    private fun buildDeviceName(): String {
        val brand = Build.BRAND.orEmpty().trim()
        val model = Build.MODEL.orEmpty().trim()
        return when {
            model.startsWith(brand, ignoreCase = true) && model.isNotEmpty() -> model
            brand.isNotEmpty() && model.isNotEmpty() -> "$brand $model"
            model.isNotEmpty() -> model
            else -> "Android 手机"
        }
    }

    private fun collectBroadcastAddresses(context: Context): List<String> {
        val result = LinkedHashSet<String>()
        try {
            val cm = context.getSystemService(Context.CONNECTIVITY_SERVICE) as? ConnectivityManager
            val active = cm?.activeNetwork
            val props: LinkProperties? = if (active != null) cm.getLinkProperties(active) else null
            props?.linkAddresses?.forEach { la ->
                val addr = la.address
                if (addr is Inet4Address && !addr.isLoopbackAddress && !addr.isLinkLocalAddress) {
                    val prefix = la.prefixLength
                    if (prefix in 1..31) {
                        val ipInt = addr.address.fold(0) { acc, b -> (acc shl 8) or (b.toInt() and 0xFF) }
                        val mask = (-1) shl (32 - prefix)
                        val bcast = ipInt or mask.inv()
                        val str = "${(bcast ushr 24) and 0xFF}.${(bcast ushr 16) and 0xFF}.${(bcast ushr 8) and 0xFF}.${bcast and 0xFF}"
                        result.add(str)
                    }
                }
            }
        } catch (e: Throwable) {
            Log.w(TAG, "collectBroadcastAddresses error: ${e.message}")
        }
        return result.toList()
    }

    companion object {
        private const val TAG = "RcViewModel"
        private const val SETTINGS_PREFS = "rc_settings"
        const val MAX_NAME_LEN = 32

        // Hub live preview: about the size the TV frame shows it at, kept cheap on the
        // phone's WiFi and battery. Fullscreen switches back to the user's choice.
        private const val HUB_PREVIEW_SHORT = 720
        private const val HUB_PREVIEW_FPS = 15
        private const val HUB_PREVIEW_KBPS = 1500
    }
}
