package com.rc.android.video

import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Build
import android.os.SystemClock
import android.util.Log
import android.view.Surface
import java.util.concurrent.Executors

/**
 * Lock-light ring buffer mapping decoded frame timestamps (nanoseconds = MediaCodec ptsUs * 1000)
 * to exact (width, height) pixel dimensions. Allows GlVideoSurfaceView.RenderThread to read
 * `SurfaceTexture.timestamp` right after `updateTexImage()` and draw every GPU frame with its
 * exact aspect ratio atomically — zero Compose recomposition race on monitor switches.
 */
object DecodedFrameSizeLedger {
    private const val CAPACITY = 64
    private val lock = Any()
    private val timestamps = LongArray(CAPACITY)
    private val widths = IntArray(CAPACITY)
    private val heights = IntArray(CAPACITY)
    private var cursor = 0

    fun record(ptsNs: Long, width: Int, height: Int) {
        if (ptsNs <= 0L || width <= 0 || height <= 0) return
        synchronized(lock) {
            for (i in 0 until CAPACITY) {
                if (timestamps[i] == ptsNs) {
                    widths[i] = width
                    heights[i] = height
                    return
                }
            }
            timestamps[cursor] = ptsNs
            widths[cursor] = width
            heights[cursor] = height
            cursor = (cursor + 1) % CAPACITY
        }
    }

    /** Returns packed `(width.toLong() shl 32) or (height.toLong() and 0xffffffffL)`, or 0L if absent. */
    fun lookupPacked(ptsNs: Long): Long {
        if (ptsNs <= 0L) return 0L
        synchronized(lock) {
            for (i in 0 until CAPACITY) {
                if (timestamps[i] == ptsNs) {
                    return (widths[i].toLong() shl 32) or (heights[i].toLong() and 0xffffffffL)
                }
            }
        }
        return 0L
    }

    fun clear() {
        synchronized(lock) {
            timestamps.fill(0L)
            widths.fill(0)
            heights.fill(0)
            cursor = 0
        }
    }
}

/**
 * Low-latency hardware MediaCodec decoder supporting H.264 and HEVC Annex B frames.
 * Renders directly to a SurfaceView with zero CPU pixel copy, seamless Surface switching
 * via `setOutputSurface`, and Android adaptive playback so monitor/quality resolution
 * changes avoid tearing down the hardware decoder.
 */
class VideoDecoder(
    private val onRequestKeyframe: () -> Unit,
    private val onFirstFrameRendered: () -> Unit,
    private val onOutputSizeChanged: (Int, Int) -> Unit = { _, _ -> },
    private val onDecodeError: (String) -> Unit = {},
) {
    private val executor = Executors.newSingleThreadExecutor { r ->
        Thread(r, "rc-video-decoder").apply { priority = Thread.MAX_PRIORITY }
    }

    private var codec: MediaCodec? = null
    private var activeSurface: Surface? = null
    private var configuredCodec: Int = -1 // 0 = H.264, 1 = HEVC
    private var configuredWidth: Int = 0
    private var configuredHeight: Int = 0
    private var adaptiveEnabled: Boolean = false
    private var maxConfiguredWidth: Int = 0
    private var maxConfiguredHeight: Int = 0
    private var waitingForKeyframe: Boolean = true
    private var firstFrameReported: Boolean = false
    private var outputFormatWidth: Int = 0
    private var outputFormatHeight: Int = 0
    private var lastReportedWidth: Int = 0
    private var lastReportedHeight: Int = 0
    private var expectedWide: Boolean? = null
    private var expectedWideDeadlineMs: Long = 0L
    private var ptsUs: Long = 0L
    private var minInputSizeBytes: Int = 1_572_864
    private var lastErrorReportedMs: Long = 0L
    private var minDecoderTier: Int = 0 // 0=full HW, 1=bare HW, 2=fallback/SW
    private var activeDecoderTier: Int = 0
    private var configuredAtMs: Long = 0L
    private var submittedSinceConfig: Int = 0
    private val bufferInfo = MediaCodec.BufferInfo()

    /**
     * When switching between monitors of opposite orientations (e.g., 2560x1440 <-> 1440x2560),
     * drop stale in-flight tail frames of the previous orientation so the very next rendered
     * GPU frame is guaranteed to be the target monitor's clean keyframe.
     */
    fun awaitMonitorOrientation(targetWide: Boolean) {
        executor.execute {
            expectedWide = targetWide
            expectedWideDeadlineMs = SystemClock.uptimeMillis() + 750L
            waitingForKeyframe = true
        }
    }

    fun clearExpectedOrientation() {
        executor.execute {
            expectedWide = null
            expectedWideDeadlineMs = 0L
        }
    }

    fun attachSurface(surface: Surface) {
        executor.execute {
            if (!surface.isValid) return@execute
            activeSurface = surface
            val c = codec
            if (c != null) {
                try {
                    c.setOutputSurface(surface)
                    onRequestKeyframe()
                    return@execute
                } catch (e: Throwable) {
                    Log.w(TAG, "setOutputSurface failed, will recreate decoder: ${e.message}")
                    releaseCodecLocked()
                }
            }
            waitingForKeyframe = true
            onRequestKeyframe()
        }
    }

    fun detachSurface(surface: Surface) {
        executor.execute {
            if (activeSurface === surface) {
                activeSurface = null
                // Release codec so it never renders to a destroyed Surface on OEM ROMs.
                releaseCodecLocked()
            }
        }
    }

    fun resetSession() {
        executor.execute {
            releaseCodecLocked()
            DecodedFrameSizeLedger.clear()
            firstFrameReported = false
            outputFormatWidth = 0
            outputFormatHeight = 0
            lastReportedWidth = 0
            lastReportedHeight = 0
            expectedWide = null
            expectedWideDeadlineMs = 0L
            waitingForKeyframe = true
            ptsUs = 0L
            minInputSizeBytes = 1_572_864
            minDecoderTier = 0
            activeDecoderTier = 0
            configuredAtMs = 0L
            submittedSinceConfig = 0
        }
    }

    fun submitFrame(codecType: Int, key: Boolean, width: Int, height: Int, data: ByteArray) {
        executor.execute {
            val surface = activeSurface
            if (surface == null || !surface.isValid) return@execute

            val wantWide = expectedWide
            if (wantWide != null) {
                val now = SystemClock.uptimeMillis()
                val frameWide = width >= height
                if (now < expectedWideDeadlineMs && frameWide != wantWide) {
                    // Drop stale-orientation tail frames from the previous monitor
                    return@execute
                }
                if (frameWide == wantWide) {
                    if (!key) {
                        return@execute
                    }
                    expectedWide = null
                    expectedWideDeadlineMs = 0L
                } else {
                    // Timeout elapsed; accept whatever orientation host is sending
                    expectedWide = null
                    expectedWideDeadlineMs = 0L
                }
            }

            val sizeChanged = configuredWidth != width || configuredHeight != height
            val orientationFlipped = configuredWidth > 0 && configuredHeight > 0 &&
                ((configuredWidth >= configuredHeight) != (width >= height))
            val canAdaptInPlace = codec != null &&
                configuredCodec == codecType &&
                adaptiveEnabled &&
                width <= maxConfiguredWidth &&
                height <= maxConfiguredHeight
            val needReconfig = codec == null ||
                configuredCodec != codecType ||
                (sizeChanged && !canAdaptInPlace)

            if (needReconfig) {
                if (!key) {
                    if (!waitingForKeyframe) {
                        waitingForKeyframe = true
                        onRequestKeyframe()
                    }
                    return@execute
                }
                if (!createCodecLocked(codecType, width, height, surface)) {
                    return@execute
                }
            } else if (sizeChanged && key) {
                // Flush queued old-monitor buffers when aspect flips so no stale frame renders
                // interleaved with the new monitor's first IDR frame.
                if (orientationFlipped) {
                    runCatching { codec?.flush() }.onFailure {
                        if (!createCodecLocked(codecType, width, height, surface)) {
                            return@execute
                        }
                    }
                }
                configuredWidth = width
                configuredHeight = height
            } else if (sizeChanged && !key) {
                if (!waitingForKeyframe) {
                    waitingForKeyframe = true
                    onRequestKeyframe()
                }
                return@execute
            }

            if (waitingForKeyframe && !key) {
                return@execute
            }

            val c = codec ?: return@execute
            try {
                drainOutputLocked(c)

                val inIdx = c.dequeueInputBuffer(8_000L)
                if (inIdx < 0) {
                    drainOutputLocked(c)
                    return@execute
                }
                val inBuf = c.getInputBuffer(inIdx) ?: return@execute
                inBuf.clear()
                if (inBuf.capacity() < data.size) {
                    Log.w(TAG, "Input buffer (${inBuf.capacity()}) smaller than frame (${data.size}); recreating decoder")
                    c.queueInputBuffer(inIdx, 0, 0, 0L, 0)
                    minInputSizeBytes = maxOf(minInputSizeBytes, data.size * 2)
                    if (key && createCodecLocked(codecType, width, height, surface, minInputSizeBytes)) {
                        val c2 = codec ?: return@execute
                        val inIdx2 = c2.dequeueInputBuffer(8_000L)
                        if (inIdx2 >= 0) {
                            val inBuf2 = c2.getInputBuffer(inIdx2)
                            if (inBuf2 != null && inBuf2.capacity() >= data.size) {
                                inBuf2.clear()
                                inBuf2.put(data)
                                ptsUs += 16_666L
                                DecodedFrameSizeLedger.record(ptsUs * 1000L, width, height)
                                c2.queueInputBuffer(inIdx2, 0, data.size, ptsUs, MediaCodec.BUFFER_FLAG_KEY_FRAME)
                                waitingForKeyframe = false
                                drainOutputLocked(c2)
                                return@execute
                            }
                            c2.queueInputBuffer(inIdx2, 0, 0, 0L, 0)
                        }
                    }
                    waitingForKeyframe = true
                    onRequestKeyframe()
                    return@execute
                }
                inBuf.put(data)
                ptsUs += 16_666L
                DecodedFrameSizeLedger.record(ptsUs * 1000L, width, height)
                val flags = if (key) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0
                c.queueInputBuffer(inIdx, 0, data.size, ptsUs, flags)
                waitingForKeyframe = false
                submittedSinceConfig += 1

                drainOutputLocked(c)

                // First-frame stall watchdog: if an OEM hardware decoder accepts configure() and
                // input frames but never emits a single output frame after 1.5s & >=15 frames,
                // automatically step down to the next compatibility tier (bare HW -> software).
                if (!firstFrameReported &&
                    activeDecoderTier < 2 &&
                    submittedSinceConfig >= 15 &&
                    SystemClock.uptimeMillis() - configuredAtMs >= 1_500L
                ) {
                    minDecoderTier = activeDecoderTier + 1
                    Log.w(TAG, "No output after $submittedSinceConfig frames on tier $activeDecoderTier; escalating to tier $minDecoderTier")
                    releaseCodecLocked()
                    waitingForKeyframe = true
                    onRequestKeyframe()
                }
            } catch (e: Throwable) {
                Log.w(TAG, "Decode error, resetting codec: ${e.message}")
                releaseCodecLocked()
                waitingForKeyframe = true
                onRequestKeyframe()
            }
        }
    }

    private fun createCodecLocked(
        codecType: Int,
        width: Int,
        height: Int,
        surface: Surface,
        requiredInputBytes: Int = minInputSizeBytes,
    ): Boolean {
        releaseCodecLocked()
        val mime = if (codecType == 1) MediaFormat.MIMETYPE_VIDEO_HEVC else MediaFormat.MIMETYPE_VIDEO_AVC
        val w = width.coerceAtLeast(64)
        val h = height.coerceAtLeast(64)
        val targetInputBytes = maxOf(requiredInputBytes, (w * h) / 2, 1_572_864)
        var lastError: String? = null

        // Tier 1: Default hardware decoder with feature-checked low-latency + size-checked adaptive playback
        if (minDecoderTier <= 0) {
            try {
                val mc = MediaCodec.createDecoderByType(mime)
                val caps = runCatching { mc.codecInfo.getCapabilitiesForType(mime) }.getOrNull()
                val format = MediaFormat.createVideoFormat(mime, w, h).apply {
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R &&
                        caps?.isFeatureSupported(MediaCodecInfo.CodecCapabilities.FEATURE_LowLatency) == true
                    ) {
                        setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
                    }
                    setInteger(MediaFormat.KEY_PRIORITY, 0)
                    setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, targetInputBytes)
                    setInteger(MediaFormat.KEY_COLOR_STANDARD, MediaFormat.COLOR_STANDARD_BT709)
                    setInteger(MediaFormat.KEY_COLOR_RANGE, MediaFormat.COLOR_RANGE_LIMITED)
                    setInteger(MediaFormat.KEY_COLOR_TRANSFER, MediaFormat.COLOR_TRANSFER_SDR_VIDEO)
                }
                val adaptive = caps?.isFeatureSupported(MediaCodecInfo.CodecCapabilities.FEATURE_AdaptivePlayback) == true
                var maxW = w
                var maxH = h
                var usedAdaptive = false
                if (adaptive) {
                    val vc = caps?.videoCapabilities
                    // Only advertise square 2560x2560 adaptive bounds if the decoder explicitly supports it;
                    // Huawei Kirin / MediaTek decoders often reject height > 2160 at width 2560.
                    val candSquareW = maxOf(w, 2560)
                    val candSquareH = maxOf(h, 2560)
                    val candMax = maxOf(w, h)
                    val candMin = minOf(w, h)
                    when {
                        vc?.isSizeSupported(candSquareW, candSquareH) == true -> {
                            maxW = candSquareW
                            maxH = candSquareH
                            usedAdaptive = true
                        }
                        vc?.isSizeSupported(candMax, candMin) == true -> {
                            maxW = candMax
                            maxH = candMin
                            usedAdaptive = true
                        }
                        vc?.isSizeSupported(w, h) == true -> {
                            maxW = w
                            maxH = h
                            usedAdaptive = true
                        }
                    }
                    if (usedAdaptive) {
                        format.setInteger(MediaFormat.KEY_MAX_WIDTH, maxW)
                        format.setInteger(MediaFormat.KEY_MAX_HEIGHT, maxH)
                    }
                }
                if (tryConfigureAndStart(mc, format, surface, codecType, w, h, usedAdaptive, maxW, maxH, tier = 0)) {
                    Log.i(TAG, "Configured $mime (${mc.name}) ${w}x${h} (adaptive=$usedAdaptive max=${maxW}x${maxH})")
                    return true
                }
            } catch (e: Throwable) {
                lastError = e.message ?: e.javaClass.simpleName
                Log.w(TAG, "Tier-1 configure failed for $mime ${w}x${h}: $lastError")
            }
        }

        // Tier 2: Minimal bare MediaFormat on default decoder (maximum Huawei Kirin / OEM compatibility)
        if (minDecoderTier <= 1) {
            try {
                val mc = MediaCodec.createDecoderByType(mime)
                val bare = MediaFormat.createVideoFormat(mime, w, h)
                if (tryConfigureAndStart(mc, bare, surface, codecType, w, h, false, w, h, tier = 1)) {
                    Log.i(TAG, "Configured $mime (${mc.name}) ${w}x${h} in bare compatibility mode")
                    return true
                }
            } catch (e: Throwable) {
                lastError = e.message ?: e.javaClass.simpleName
                Log.w(TAG, "Tier-2 bare configure failed for $mime ${w}x${h}: $lastError")
            }
        }

        // Tier 3: Try any other available decoder for this MIME (prefer software if escalating from HW stall)
        val defaultName = runCatching {
            val tmp = MediaCodec.createDecoderByType(mime)
            val n = tmp.name
            tmp.release()
            n
        }.getOrNull()
        val candidates = runCatching {
            MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos
                .filter { !it.isEncoder && it.supportedTypes.any { t -> t.equals(mime, ignoreCase = true) } }
                .sortedBy { info ->
                    val isSw = info.name.startsWith("c2.android.", true) || info.name.startsWith("OMX.google.", true)
                    when {
                        minDecoderTier >= 2 && isSw -> 0
                        info.name == defaultName -> 2
                        else -> 1
                    }
                }
        }.getOrDefault(emptyList())
        for (info in candidates) {
            try {
                val mc = MediaCodec.createByCodecName(info.name)
                val bare = MediaFormat.createVideoFormat(mime, w, h)
                if (tryConfigureAndStart(mc, bare, surface, codecType, w, h, false, w, h, tier = 2)) {
                    Log.i(TAG, "Configured fallback decoder ${info.name} for $mime ${w}x${h}")
                    return true
                }
            } catch (e: Throwable) {
                lastError = "${info.name}: ${e.message ?: e.javaClass.simpleName}"
                Log.w(TAG, "Fallback decoder ${info.name} failed for $mime ${w}x${h}: ${e.message}")
            }
        }

        Log.e(TAG, "All decoders failed for $mime ${w}x${h}: $lastError")
        val now = SystemClock.uptimeMillis()
        if (now - lastErrorReportedMs > 4_000L) {
            lastErrorReportedMs = now
            onDecodeError("视频解码器初始化失败（${if (codecType == 1) "HEVC" else "H.264"} ${w}x${h}）：${lastError ?: "不支持"}")
        }
        releaseCodecLocked()
        return false
    }

    private fun tryConfigureAndStart(
        mc: MediaCodec,
        format: MediaFormat,
        surface: Surface,
        codecType: Int,
        w: Int,
        h: Int,
        adaptive: Boolean,
        maxW: Int,
        maxH: Int,
        tier: Int,
    ): Boolean {
        return try {
            mc.configure(format, surface, null, 0)
            runCatching { mc.setVideoScalingMode(MediaCodec.VIDEO_SCALING_MODE_SCALE_TO_FIT) }
            mc.start()
            codec = mc
            configuredCodec = codecType
            configuredWidth = w
            configuredHeight = h
            adaptiveEnabled = adaptive
            maxConfiguredWidth = maxW
            maxConfiguredHeight = maxH
            activeDecoderTier = tier
            configuredAtMs = SystemClock.uptimeMillis()
            submittedSinceConfig = 0
            waitingForKeyframe = true
            true
        } catch (e: Throwable) {
            runCatching { mc.release() }
            throw e
        }
    }

    private fun drainOutputLocked(c: MediaCodec) {
        // Give strict hardware decoders a brief 4 ms window to emit the very first IDR frame
        // so first-frame display never stalls waiting for extra frames.
        var timeoutUs = if (!firstFrameReported) 4_000L else 0L
        while (true) {
            val outIdx = c.dequeueOutputBuffer(bufferInfo, timeoutUs)
            timeoutUs = 0L
            when {
                outIdx >= 0 -> {
                    val ptsNs = bufferInfo.presentationTimeUs * 1000L
                    val packed = DecodedFrameSizeLedger.lookupPacked(ptsNs)
                    val stampedW = (packed ushr 32).toInt()
                    val stampedH = (packed and 0xffffffffL).toInt()
                    val frameW = when {
                        stampedW > 0 -> stampedW
                        outputFormatWidth > 0 -> outputFormatWidth
                        else -> configuredWidth
                    }
                    val frameH = when {
                        stampedH > 0 -> stampedH
                        outputFormatHeight > 0 -> outputFormatHeight
                        else -> configuredHeight
                    }

                    val wantWide = expectedWide
                    if (wantWide != null &&
                        frameW > 0 && frameH > 0 &&
                        SystemClock.uptimeMillis() < expectedWideDeadlineMs &&
                        (frameW >= frameH) != wantWide
                    ) {
                        // Drop stale-orientation output buffer without rendering to SurfaceTexture
                        c.releaseOutputBuffer(outIdx, false)
                        continue
                    }

                    // Ensure ledger has exact (frameW, frameH) BEFORE releasing to SurfaceTexture
                    // so GlVideoSurfaceView.RenderThread renders this GPU texture with the exact
                    // aspect ratio on Frame 0 (zero Compose recomposition lag).
                    if (ptsNs > 0L && frameW > 0 && frameH > 0) {
                        DecodedFrameSizeLedger.record(ptsNs, frameW, frameH)
                    }
                    c.releaseOutputBuffer(outIdx, true)

                    if (frameW > 0 && frameH > 0 &&
                        (frameW != lastReportedWidth || frameH != lastReportedHeight)
                    ) {
                        lastReportedWidth = frameW
                        lastReportedHeight = frameH
                        onOutputSizeChanged(frameW, frameH)
                    }
                    if (!firstFrameReported) {
                        firstFrameReported = true
                        onFirstFrameRendered()
                    }
                }
                outIdx == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> {
                    val fmt = c.outputFormat
                    Log.d(TAG, "Output format changed: $fmt")
                    val cropLeft = if (fmt.containsKey("crop-left")) fmt.getInteger("crop-left") else 0
                    val cropRight = if (fmt.containsKey("crop-right")) fmt.getInteger("crop-right") else -1
                    val cropTop = if (fmt.containsKey("crop-top")) fmt.getInteger("crop-top") else 0
                    val cropBottom = if (fmt.containsKey("crop-bottom")) fmt.getInteger("crop-bottom") else -1
                    val outW = if (cropRight >= cropLeft) {
                        cropRight - cropLeft + 1
                    } else if (fmt.containsKey(MediaFormat.KEY_WIDTH)) {
                        fmt.getInteger(MediaFormat.KEY_WIDTH)
                    } else {
                        0
                    }
                    val outH = if (cropBottom >= cropTop) {
                        cropBottom - cropTop + 1
                    } else if (fmt.containsKey(MediaFormat.KEY_HEIGHT)) {
                        fmt.getInteger(MediaFormat.KEY_HEIGHT)
                    } else {
                        0
                    }
                    if (outW > 0 && outH > 0) {
                        outputFormatWidth = outW
                        outputFormatHeight = outH
                    }
                }
                else -> break
            }
        }
    }

    private fun releaseCodecLocked() {
        val c = codec
        codec = null
        configuredCodec = -1
        configuredWidth = 0
        configuredHeight = 0
        adaptiveEnabled = false
        maxConfiguredWidth = 0
        maxConfiguredHeight = 0
        outputFormatWidth = 0
        outputFormatHeight = 0
        if (c != null) {
            try {
                c.stop()
            } catch (_: Throwable) {
            }
            try {
                c.release()
            } catch (_: Throwable) {
            }
        }
    }

    fun destroy() {
        executor.execute {
            activeSurface = null
            releaseCodecLocked()
        }
        executor.shutdown()
    }

    companion object {
        private const val TAG = "RcVideoDecoder"
    }
}
