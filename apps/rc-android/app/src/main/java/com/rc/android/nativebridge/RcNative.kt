package com.rc.android.nativebridge

interface NativeCallback {
    fun onDevicesUpdated(devicesJson: String)
    fun onSessionState(state: String, message: String)
    fun onStreamInfo(width: Int, height: Int, encoder: String, fps: Int, maxKbps: Int)
    fun onMonitors(monitorsJson: String, currentIndex: Int)
    fun onWindows(windowsJson: String)
    fun onAppIcon(appKey: String, pngBytes: ByteArray)
    fun onWindowThumb(hwnd: Long, pngBytes: ByteArray)
    fun onStats(fps: Int, kbps: Int, rttMs: Int, pathKind: String)
    fun onHint(code: Int, message: String)
    fun onNotice(message: String)
    fun onCursorShape(id: Int, width: Int, height: Int, hotX: Int, hotY: Int, rgba: ByteArray)
    fun onCursorUse(id: Int)
    fun onCursorHidden()
    fun onCursorMeta(kind: Int)
    fun onTextFocus(editable: Boolean)
    fun onVirtualDisplay(active: Boolean, width: Int, height: Int, dpiPercent: Int)
    fun onRouteChanged(relayOnly: Boolean)
    fun onVideoFrame(codec: Int, key: Boolean, width: Int, height: Int, hostUs: Long, data: ByteArray)
}

object RcNative {
    init {
        System.loadLibrary("rc_android")
    }

    external fun nativeInit(filesDir: String, deviceName: String, callback: NativeCallback): String
    external fun nativeSetBroadcasts(csv: String)
    external fun nativeSetDeviceName(name: String)
    external fun nativeTriggerScan()
    external fun nativePasteCode(text: String): String
    external fun nativeConnect(idHex: String, fitW: Int, fitH: Int, fps: Int, relayOnly: Boolean): String
    external fun nativeSetRelayOnly(relayOnly: Boolean)
    external fun nativeDisconnect()
    external fun nativeSelectMonitor(index: Int)
    external fun nativeFocusWindow(hwnd: Long, viewWidth: Int, viewHeight: Int)
    external fun nativeSetVirtualDisplay(active: Boolean, width: Int, height: Int, dpiPercent: Int)
    external fun nativeRequestKeyframe()
    external fun nativeSetQuality(
        fitW: Int,
        fitH: Int,
        shortSide: Int,
        fps: Int,
        kbps: Int,
        yuv444: Boolean,
        autoFps: Boolean,
    )
    external fun nativeSendMouseMove(x: Int, y: Int)
    external fun nativeSendMouseDelta(dx: Int, dy: Int)
    external fun nativeSendMouseButton(button: Int, down: Boolean)
    external fun nativeSendTouch(id: Int, phase: Int, x: Int, y: Int)
    external fun nativeSendTouchpad(phase: Int, dx: Int, dy: Int, scaleMilli: Int)
    external fun nativeSendPen(
        phase: Int,
        x: Int,
        y: Int,
        pressure: Int,
        tiltX: Int,
        tiltY: Int,
        flags: Int,
    )
    external fun nativeSendWheel(delta: Int, horizontal: Boolean)
    external fun nativeSendKey(scan: Int, down: Boolean)
    external fun nativeSendText(text: String)
    external fun nativeReleaseAll()
    external fun nativeRememberDevice(idHex: String)
    external fun nativeForgetDevice(idHex: String)
    external fun nativeAgreeRemember(idHex: String)
    external fun nativeRefuseRemember(idHex: String)
    external fun nativeCancelRemember(idHex: String)
    external fun nativeRetryDevice(idHex: String)
}
