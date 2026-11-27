package com.rc.android.model

import androidx.compose.ui.graphics.ImageBitmap

enum class AppLayer {
    Devices,
    Hub,
    Fullscreen,
}

enum class TouchMode {
    Trackpad,
    Direct,
}

data class CursorGlyph(
    val bitmap: ImageBitmap,
    val width: Int,
    val height: Int,
    val hotX: Int,
    val hotY: Int,
)

enum class SessionPhase {
    Idle,
    Connecting,
    WaitingConsent,
    Connected,
    Paused,
    Disconnected,
    Error,
}

data class DeviceItem(
    val id: String,
    val shortId: String,
    val name: String,
    val status: String, // "online", "checking", "offline", "refused"
    val rttMs: Int,
    val path: String,   // "direct", "relay", "public", "none"
    val onLan: Boolean,
    val remembered: Boolean,
    val hasSelfHostedRelay: Boolean = false,
    val ask: String = "none", // "none", "in", "out"
    val error: String,
) {
    val isOnline: Boolean get() = status == "online"
    val pathLabel: String
        get() = when (path) {
            "direct" -> if (onLan) "局域网直连" else "直连"
            "relay" -> "自建中转"
            "public" -> "公共中转"
            else -> if (onLan) "局域网" else ""
        }
}

data class MonitorInfo(
    val index: Int,
    val name: String,
    val x: Int,
    val y: Int,
    val width: Int,
    val height: Int,
    val primary: Boolean,
    val rotation: Int,
) {
    val displayTitle: String
        get() = if (primary) "主屏 ${index + 1}" else "副屏 ${index + 1}"

    val shortRes: String
        get() {
            val shortSide = minOf(width, height)
            return when {
                shortSide >= 2160 -> "4K"
                shortSide > 0 -> "${shortSide}p"
                else -> ""
            }
        }
}

data class RunningWindowItem(
    val hwnd: Long,
    val id: String,
    val appKey: String,
    val appName: String,
    val title: String,
    val monitor: Int = 0,
    val minimized: Boolean = false,
    val rx: Float = 0f,
    val ry: Float = 0f,
    val rw: Float = 0f,
    val rh: Float = 0f,
) {
    val hasBounds: Boolean
        get() = rw > 0.02f && rh > 0.02f
}

data class StreamMeta(
    val width: Int = 1920,
    val height: Int = 1080,
    val encoder: String = "",
    val fps: Int = 60,
    val maxKbps: Int = 20_000,
)

data class LiveStats(
    val fps: Int = 0,
    val kbps: Int = 0,
    val rttMs: Int = 0,
    val pathKind: String = "",
)

data class QualityPrefs(
    /** 0 = 自动适配屏幕, 1 = 原生(显示器原分辨率), 1440 / 1080 / 720 = 限制短边 */
    val resPreset: Int = 0,
    /** 0 = 自动, 30 / 60 / 120 / 144 */
    val fpsPreset: Int = 0,
    /** 0 = 自动, 10 / 20 / 40 / 80 Mbps */
    val mbpsPreset: Int = 0,
    /** YUV 4:4:4 文字清晰模式 */
    val sharpText: Boolean = false,
    /** false = 自动(优先直连), true = 只走自建中转 */
    val relayOnly: Boolean = false,
    /** 全屏界面常驻显示实时帧率与延迟角标 */
    val showStatsHud: Boolean = false,
)

data class VirtualDisplayState(
    val active: Boolean = false,
    val width: Int = 0,
    val height: Int = 0,
    val dpiPercent: Int = 0,
)

data class PasteFeedback(
    val ok: Boolean,
    val message: String,
)

data class RcUiState(
    val layer: AppLayer = AppLayer.Devices,
    val localDeviceId: String = "",
    val ownRelayCount: Int = 0,
    val devices: List<DeviceItem> = emptyList(),
    val activeDevice: DeviceItem? = null,
    val sessionPhase: SessionPhase = SessionPhase.Idle,
    val sessionMessage: String = "",
    val streamMeta: StreamMeta = StreamMeta(),
    val quality: QualityPrefs = QualityPrefs(),
    val monitors: List<MonitorInfo> = emptyList(),
    val currentMonitorIndex: Int = 0,
    val windows: List<RunningWindowItem> = emptyList(),
    val activeWindow: RunningWindowItem? = null,
    val virtualDisplay: VirtualDisplayState = VirtualDisplayState(),
    val appIcons: Map<String, ImageBitmap> = emptyMap(),
    val windowThumbs: Map<Long, ImageBitmap> = emptyMap(),
    val liveStats: LiveStats = LiveStats(),
    val hintCode: Int = 0,
    val hintMessage: String = "",
    val pasteFeedback: PasteFeedback? = null,
    val pasteBusy: Boolean = false,
    val hasFirstFrame: Boolean = false,
    val touchMode: TouchMode = TouchMode.Trackpad,
    val cursorGlyphs: Map<Int, CursorGlyph> = emptyMap(),
    val currentCursorId: Int = 0,
    val cursorVisible: Boolean = true,
    val cursorMetaKind: Int = 0, // 0 normal, 1 interactive/hand/resize, 2 text ibeam
    val remoteEditable: Boolean = false,
    val remoteEditableTick: Long = 0L,
    val toastMessage: String? = null,
    val initError: String? = null,
    /** Name computers see; empty [customName] means the phone model ([modelName]). */
    val customName: String = "",
    val modelName: String = "",
)
