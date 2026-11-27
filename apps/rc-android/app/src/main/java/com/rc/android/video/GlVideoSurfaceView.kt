package com.rc.android.video

import android.content.Context
import android.graphics.SurfaceTexture
import android.opengl.EGL14
import android.opengl.EGLConfig
import android.opengl.EGLContext
import android.opengl.EGLDisplay
import android.opengl.EGLExt
import android.opengl.EGLSurface
import android.opengl.GLES11Ext
import android.opengl.GLES20
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.nio.FloatBuffer
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.math.abs
import kotlin.math.roundToInt

/**
 * Hardware zero-copy MediaCodec -> SurfaceTexture (OES) -> EGL/GLES 2.0 -> SurfaceView renderer.
 *
 * Benefits over SurfaceView + Compose graphicsLayer:
 * - Tear-free GPU matrix zoom/pan aligned to exact pixel boundaries on every frame.
 * - True GPU SDF rounded-corner clipping (no black square punch-through in Hub cards).
 * - Exact Windows-viewer resampling (`torgb.rs`): 1:1 native sampling, 5-tap luma Catmull-Rom
 *   cubic filtering on zoom/mild scale, and 2x2 / 4x4 box supersampling on downscale.
 */
class GlVideoSurfaceView(
    context: Context,
    private val onDecoderSurfaceReady: (Surface) -> Unit,
    private val onDecoderSurfaceDestroyed: (Surface) -> Unit,
    ambient: Boolean = false,
    onAmbientColors: ((top: Int, right: Int, bottom: Int, left: Int) -> Unit)? = null,
) : SurfaceView(context), SurfaceHolder.Callback {

    @Volatile private var streamWidth: Int = 1920
    @Volatile private var streamHeight: Int = 1080
    @Volatile private var zoomScale: Float = 1f
    @Volatile private var panX: Float = 0f
    @Volatile private var panY: Float = 0f
    @Volatile private var cornerRadiusPx: Float = 0f
    @Volatile private var bgR: Float = 0f
    @Volatile private var bgG: Float = 0f
    @Volatile private var bgB: Float = 0f
    @Volatile private var ambientEnabled: Boolean = ambient
    @Volatile private var ambientColorsCallback: ((top: Int, right: Int, bottom: Int, left: Int) -> Unit)? =
        onAmbientColors

    private val mainHandler = Handler(Looper.getMainLooper())
    private var renderThread: RenderThread? = null

    init {
        holder.addCallback(this)
    }

    fun updateRenderParams(
        streamW: Int,
        streamH: Int,
        zoom: Float,
        px: Float,
        py: Float,
        cornerRadius: Float,
        outsideR: Float = 0f,
        outsideG: Float = 0f,
        outsideB: Float = 0f,
        ambient: Boolean = ambientEnabled,
        onAmbientColors: ((top: Int, right: Int, bottom: Int, left: Int) -> Unit)? = ambientColorsCallback,
    ) {
        val changed = streamWidth != streamW ||
            streamHeight != streamH ||
            zoomScale != zoom ||
            panX != px ||
            panY != py ||
            cornerRadiusPx != cornerRadius ||
            bgR != outsideR ||
            bgG != outsideG ||
            bgB != outsideB ||
            ambientEnabled != ambient
        streamWidth = streamW.coerceAtLeast(1)
        streamHeight = streamH.coerceAtLeast(1)
        zoomScale = zoom.coerceIn(1f, 6f)
        panX = px
        panY = py
        cornerRadiusPx = cornerRadius.coerceAtLeast(0f)
        bgR = outsideR.coerceIn(0f, 1f)
        bgG = outsideG.coerceIn(0f, 1f)
        bgB = outsideB.coerceIn(0f, 1f)
        ambientEnabled = ambient
        ambientColorsCallback = onAmbientColors
        if (changed) {
            renderThread?.requestRender()
        }
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        stopThread()
        val rt = RenderThread(holder.surface, width.coerceAtLeast(1), height.coerceAtLeast(1))
        renderThread = rt
        rt.start()
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        renderThread?.onViewportChanged(width.coerceAtLeast(1), height.coerceAtLeast(1))
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        stopThread()
    }

    fun release() {
        stopThread()
    }

    private fun stopThread() {
        val rt = renderThread
        renderThread = null
        rt?.shutdownAndJoin()
        mainHandler.removeCallbacksAndMessages(null)
    }

    private inner class RenderThread(
        private val windowSurface: Surface,
        initW: Int,
        initH: Int,
    ) : Thread("rc-gl-video") {

        private val lock = Object()
        private val running = AtomicBoolean(true)
        private var frameAvailable = false
        private var renderRequested = true
        private var viewportW = initW
        private var viewportH = initH

        private var eglDisplay: EGLDisplay = EGL14.EGL_NO_DISPLAY
        private var eglContext: EGLContext = EGL14.EGL_NO_CONTEXT
        private var eglSurface: EGLSurface = EGL14.EGL_NO_SURFACE
        private var program = 0
        private var oesTexId = 0
        private var surfaceTexture: SurfaceTexture? = null
        private var decoderSurface: Surface? = null

        private val stMatrix = FloatArray(16)
        private var uStMatrixLoc = -1
        private var uViewSizeLoc = -1
        private var uVideoRectLoc = -1
        private var uCornerRadiusLoc = -1
        private var uTexSizeLoc = -1
        private var uTexelsPerPxLoc = -1
        private var uBgColorLoc = -1
        private var uAmbientEnabledLoc = -1
        private var uAmbientTexelSizeLoc = -1
        private var uAmbientCoverScaleLoc = -1
        private var aPosLoc = -1
        private var aTexLoc = -1

        // Lazily initialized only when ambient mode is enabled
        private var ambientProgram = 0
        private var ambAPosLoc = -1
        private var ambATexLoc = -1
        private var ambUStMatrixLoc = -1
        private var ambUTexelSizeLoc = -1
        private var ambientFboId = 0
        private var ambientTexId = 0
        private var ambientFboW = 0
        private var ambientFboH = 0
        private var ambientReadBuffer: ByteBuffer? = null
        private var ambientInitFailed = false
        private var lastAmbientSampleMs = 0L
        private var hasLastAmbientColors = false
        private var lastTopColor = 0
        private var lastRightColor = 0
        private var lastBottomColor = 0
        private var lastLeftColor = 0

        private val quadBuffer: FloatBuffer = ByteBuffer
            .allocateDirect(4 * 4 * 4)
            .order(ByteOrder.nativeOrder())
            .asFloatBuffer()
            .apply {
                // Full-screen triangle strip: x, y, u, v
                put(
                    floatArrayOf(
                        -1f, -1f, 0f, 0f,
                        1f, -1f, 1f, 0f,
                        -1f, 1f, 0f, 1f,
                        1f, 1f, 1f, 1f,
                    ),
                )
                position(0)
            }

        fun requestRender() {
            synchronized(lock) {
                renderRequested = true
                lock.notifyAll()
            }
        }

        fun onViewportChanged(w: Int, h: Int) {
            synchronized(lock) {
                viewportW = w
                viewportH = h
                renderRequested = true
                lock.notifyAll()
            }
        }

        fun shutdownAndJoin() {
            running.set(false)
            synchronized(lock) {
                lock.notifyAll()
            }
            try {
                join(1500)
            } catch (_: InterruptedException) {
            }
        }

        override fun run() {
            if (!initGl()) {
                // Ensure EGL is completely detached from windowSurface BEFORE handing it to
                // MediaCodec, otherwise native_window_api_connect fails with EINVAL (-22).
                releaseGl()
                Log.e(TAG, "GL init failed; falling back to direct window Surface")
                onDecoderSurfaceReady(windowSurface)
                while (running.get()) {
                    synchronized(lock) {
                        if (running.get()) {
                            try {
                                lock.wait(200)
                            } catch (_: InterruptedException) {
                            }
                        }
                    }
                }
                onDecoderSurfaceDestroyed(windowSurface)
                return
            }

            val st = SurfaceTexture(oesTexId)
            st.setOnFrameAvailableListener {
                synchronized(lock) {
                    frameAvailable = true
                    renderRequested = true
                    lock.notifyAll()
                }
            }
            val decSurf = Surface(st)
            surfaceTexture = st
            decoderSurface = decSurf
            onDecoderSurfaceReady(decSurf)

            var hasReceivedFrame = false
            var latchedW = 0
            var latchedH = 0

            while (running.get()) {
                var doUpdateTex = false
                var vw = 0
                var vh = 0
                var keepGoing = true
                synchronized(lock) {
                    while (running.get() && !renderRequested && !frameAvailable) {
                        try {
                            lock.wait()
                        } catch (_: InterruptedException) {
                            keepGoing = false
                            break
                        }
                    }
                    if (!running.get() || !keepGoing) {
                        keepGoing = false
                    } else {
                        doUpdateTex = frameAvailable
                        frameAvailable = false
                        renderRequested = false
                        vw = viewportW
                        vh = viewportH
                    }
                }
                if (!keepGoing) break

                if (doUpdateTex) {
                    try {
                        st.updateTexImage()
                        st.getTransformMatrix(stMatrix)
                        val packed = DecodedFrameSizeLedger.lookupPacked(st.timestamp)
                        if (packed != 0L) {
                            val fw = (packed ushr 32).toInt()
                            val fh = (packed and 0xffffffffL).toInt()
                            if (fw > 0 && fh > 0) {
                                latchedW = fw
                                latchedH = fh
                            }
                        }
                        hasReceivedFrame = true
                    } catch (e: Throwable) {
                        Log.w(TAG, "updateTexImage failed: ${e.message}")
                    }
                }

                drawFrame(vw, vh, hasReceivedFrame, latchedW, latchedH)
            }

            onDecoderSurfaceDestroyed(decSurf)
            releaseGl()
        }

        private fun drawFrame(
            vw: Int,
            vh: Int,
            hasFrame: Boolean,
            latchedW: Int,
            latchedH: Int,
        ) {
            val useAmbient = ambientEnabled
            if (!hasFrame) {
                GLES20.glViewport(0, 0, vw, vh)
                if (useAmbient) {
                    GLES20.glClearColor(bgR, bgG, bgB, 1f)
                } else {
                    GLES20.glClearColor(0f, 0f, 0f, 1f)
                }
                GLES20.glClear(GLES20.GL_COLOR_BUFFER_BIT)
                EGL14.eglSwapBuffers(eglDisplay, eglSurface)
                return
            }

            val cw = vw.toFloat().coerceAtLeast(1f)
            val ch = vh.toFloat().coerceAtLeast(1f)
            // Prefer the exact pixel dimensions stamped onto THIS SurfaceTexture frame
            // so monitor switches never draw a new frame with the old aspect ratio (or vice versa).
            val sw = (if (latchedW > 0) latchedW else streamWidth).coerceAtLeast(1).toFloat()
            val sh = (if (latchedH > 0) latchedH else streamHeight).coerceAtLeast(1).toFloat()
            val streamAspect = (sw / sh).coerceIn(0.25f, 4.0f)
            val viewAspect = cw / ch

            val baseW: Float
            val baseH: Float
            if (viewAspect > streamAspect) {
                baseH = ch
                baseW = ch * streamAspect
            } else {
                baseW = cw
                baseH = cw / streamAspect
            }

            val z = zoomScale
            val scaledW = baseW * z
            val scaledH = baseH * z
            // Center of video in top-left-origin screen coordinates (matches Compose graphicsLayer)
            val left = cw * 0.5f + panX - scaledW * 0.5f
            val top = ch * 0.5f + panY - scaledH * 0.5f

            var ambientReady = false
            var coverScaleX = 1f
            var coverScaleY = 1f
            if (useAmbient) {
                val targetW: Int
                val targetH: Int
                if (sw >= sh) {
                    targetW = AMBIENT_LONG_EDGE
                    targetH = (AMBIENT_LONG_EDGE / streamAspect).roundToInt().coerceIn(8, AMBIENT_LONG_EDGE)
                } else {
                    targetH = AMBIENT_LONG_EDGE
                    targetW = (AMBIENT_LONG_EDGE * streamAspect).roundToInt().coerceIn(8, AMBIENT_LONG_EDGE)
                }
                ambientReady = renderAmbientPass(targetW, targetH)
                if (viewAspect > streamAspect) {
                    coverScaleX = 1f
                    coverScaleY = streamAspect / viewAspect
                } else {
                    coverScaleX = viewAspect / streamAspect
                    coverScaleY = 1f
                }
            } else {
                hasLastAmbientColors = false
                lastAmbientSampleMs = 0L
            }

            GLES20.glViewport(0, 0, vw, vh)
            GLES20.glClearColor(0f, 0f, 0f, 1f)
            GLES20.glClear(GLES20.GL_COLOR_BUFFER_BIT)

            GLES20.glUseProgram(program)

            GLES20.glActiveTexture(GLES20.GL_TEXTURE0)
            GLES20.glBindTexture(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, oesTexId)

            if (ambientReady) {
                GLES20.glActiveTexture(GLES20.GL_TEXTURE1)
                GLES20.glBindTexture(GLES20.GL_TEXTURE_2D, ambientTexId)
                GLES20.glUniform1f(uAmbientEnabledLoc, 1f)
                GLES20.glUniform2f(uAmbientTexelSizeLoc, 1f / ambientFboW.toFloat(), 1f / ambientFboH.toFloat())
                GLES20.glUniform2f(uAmbientCoverScaleLoc, coverScaleX, coverScaleY)
            } else if (useAmbient) {
                GLES20.glUniform1f(uAmbientEnabledLoc, 2f)
            } else {
                GLES20.glUniform1f(uAmbientEnabledLoc, 0f)
            }

            GLES20.glUniformMatrix4fv(uStMatrixLoc, 1, false, stMatrix, 0)
            GLES20.glUniform2f(uViewSizeLoc, cw, ch)
            GLES20.glUniform4f(uVideoRectLoc, left, top, scaledW, scaledH)
            GLES20.glUniform1f(uCornerRadiusLoc, cornerRadiusPx)
            val texelsPerPx = (sw / scaledW.coerceAtLeast(1f)).coerceIn(0.05f, 8f)
            GLES20.glUniform2f(uTexSizeLoc, sw, sh)
            GLES20.glUniform1f(uTexelsPerPxLoc, texelsPerPx)
            GLES20.glUniform3f(uBgColorLoc, bgR, bgG, bgB)

            quadBuffer.position(0)
            GLES20.glEnableVertexAttribArray(aPosLoc)
            GLES20.glVertexAttribPointer(aPosLoc, 2, GLES20.GL_FLOAT, false, 16, quadBuffer)

            quadBuffer.position(2)
            GLES20.glEnableVertexAttribArray(aTexLoc)
            GLES20.glVertexAttribPointer(aTexLoc, 2, GLES20.GL_FLOAT, false, 16, quadBuffer)

            GLES20.glDrawArrays(GLES20.GL_TRIANGLE_STRIP, 0, 4)

            GLES20.glDisableVertexAttribArray(aPosLoc)
            GLES20.glDisableVertexAttribArray(aTexLoc)
            if (ambientReady) {
                GLES20.glActiveTexture(GLES20.GL_TEXTURE1)
                GLES20.glBindTexture(GLES20.GL_TEXTURE_2D, 0)
            }
            GLES20.glActiveTexture(GLES20.GL_TEXTURE0)
            GLES20.glBindTexture(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, 0)

            EGL14.eglSwapBuffers(eglDisplay, eglSurface)
        }

        private fun ensureAmbientGl(targetW: Int, targetH: Int): Boolean {
            if (ambientInitFailed) return false
            if (ambientProgram == 0) {
                val prog = buildProgram(AMBIENT_VERTEX_SHADER, AMBIENT_FRAGMENT_SHADER)
                if (prog == 0) {
                    ambientInitFailed = true
                    return false
                }
                ambientProgram = prog
                ambAPosLoc = GLES20.glGetAttribLocation(prog, "aPos")
                ambATexLoc = GLES20.glGetAttribLocation(prog, "aTex")
                ambUStMatrixLoc = GLES20.glGetUniformLocation(prog, "uStMatrix")
                ambUTexelSizeLoc = GLES20.glGetUniformLocation(prog, "uTexelSize")
                val ambUTexLoc = GLES20.glGetUniformLocation(prog, "uTex")
                if (ambUTexLoc >= 0) {
                    GLES20.glUseProgram(prog)
                    GLES20.glUniform1i(ambUTexLoc, 0)
                    GLES20.glUseProgram(0)
                }
            }
            if (ambientTexId == 0) {
                val tex = IntArray(1)
                GLES20.glGenTextures(1, tex, 0)
                ambientTexId = tex[0]
                if (ambientTexId == 0) {
                    ambientInitFailed = true
                    return false
                }
                GLES20.glBindTexture(GLES20.GL_TEXTURE_2D, ambientTexId)
                GLES20.glTexParameteri(GLES20.GL_TEXTURE_2D, GLES20.GL_TEXTURE_MIN_FILTER, GLES20.GL_LINEAR)
                GLES20.glTexParameteri(GLES20.GL_TEXTURE_2D, GLES20.GL_TEXTURE_MAG_FILTER, GLES20.GL_LINEAR)
                GLES20.glTexParameteri(GLES20.GL_TEXTURE_2D, GLES20.GL_TEXTURE_WRAP_S, GLES20.GL_CLAMP_TO_EDGE)
                GLES20.glTexParameteri(GLES20.GL_TEXTURE_2D, GLES20.GL_TEXTURE_WRAP_T, GLES20.GL_CLAMP_TO_EDGE)
                GLES20.glBindTexture(GLES20.GL_TEXTURE_2D, 0)
            }
            if (ambientFboId == 0) {
                val fbo = IntArray(1)
                GLES20.glGenFramebuffers(1, fbo, 0)
                ambientFboId = fbo[0]
                if (ambientFboId == 0) {
                    ambientInitFailed = true
                    return false
                }
            }
            if (ambientFboW != targetW || ambientFboH != targetH) {
                GLES20.glBindTexture(GLES20.GL_TEXTURE_2D, ambientTexId)
                GLES20.glTexImage2D(
                    GLES20.GL_TEXTURE_2D,
                    0,
                    GLES20.GL_RGBA,
                    targetW,
                    targetH,
                    0,
                    GLES20.GL_RGBA,
                    GLES20.GL_UNSIGNED_BYTE,
                    null,
                )
                GLES20.glBindTexture(GLES20.GL_TEXTURE_2D, 0)

                GLES20.glBindFramebuffer(GLES20.GL_FRAMEBUFFER, ambientFboId)
                GLES20.glFramebufferTexture2D(
                    GLES20.GL_FRAMEBUFFER,
                    GLES20.GL_COLOR_ATTACHMENT0,
                    GLES20.GL_TEXTURE_2D,
                    ambientTexId,
                    0,
                )
                val status = GLES20.glCheckFramebufferStatus(GLES20.GL_FRAMEBUFFER)
                GLES20.glBindFramebuffer(GLES20.GL_FRAMEBUFFER, 0)
                if (status != GLES20.GL_FRAMEBUFFER_COMPLETE) {
                    Log.w(TAG, "Ambient FBO incomplete: 0x${status.toString(16)}")
                    ambientInitFailed = true
                    return false
                }
                ambientFboW = targetW
                ambientFboH = targetH
                ambientReadBuffer = ByteBuffer.allocateDirect(targetW * targetH * 4).order(ByteOrder.nativeOrder())
            }
            return true
        }

        private fun renderAmbientPass(targetW: Int, targetH: Int): Boolean {
            if (!ensureAmbientGl(targetW, targetH)) return false

            GLES20.glBindFramebuffer(GLES20.GL_FRAMEBUFFER, ambientFboId)
            GLES20.glViewport(0, 0, targetW, targetH)

            GLES20.glUseProgram(ambientProgram)
            GLES20.glActiveTexture(GLES20.GL_TEXTURE0)
            GLES20.glBindTexture(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, oesTexId)

            GLES20.glUniformMatrix4fv(ambUStMatrixLoc, 1, false, stMatrix, 0)
            GLES20.glUniform2f(ambUTexelSizeLoc, 1f / targetW.toFloat(), 1f / targetH.toFloat())

            quadBuffer.position(0)
            GLES20.glEnableVertexAttribArray(ambAPosLoc)
            GLES20.glVertexAttribPointer(ambAPosLoc, 2, GLES20.GL_FLOAT, false, 16, quadBuffer)

            quadBuffer.position(2)
            GLES20.glEnableVertexAttribArray(ambATexLoc)
            GLES20.glVertexAttribPointer(ambATexLoc, 2, GLES20.GL_FLOAT, false, 16, quadBuffer)

            GLES20.glDrawArrays(GLES20.GL_TRIANGLE_STRIP, 0, 4)

            GLES20.glDisableVertexAttribArray(ambAPosLoc)
            GLES20.glDisableVertexAttribArray(ambATexLoc)
            GLES20.glBindTexture(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, 0)

            maybeSampleAmbientEdges(targetW, targetH)

            GLES20.glBindFramebuffer(GLES20.GL_FRAMEBUFFER, 0)
            return true
        }

        private fun maybeSampleAmbientEdges(w: Int, h: Int) {
            if (ambientColorsCallback == null) {
                hasLastAmbientColors = false
                return
            }
            val now = SystemClock.uptimeMillis()
            if (hasLastAmbientColors && now - lastAmbientSampleMs < AMBIENT_SAMPLE_INTERVAL_MS) {
                return
            }
            val buf = ambientReadBuffer ?: return
            buf.position(0)
            GLES20.glReadPixels(0, 0, w, h, GLES20.GL_RGBA, GLES20.GL_UNSIGNED_BYTE, buf)
            lastAmbientSampleMs = now

            val bandY = AMBIENT_EDGE_BAND.coerceAtMost(h)
            val bandX = AMBIENT_EDGE_BAND.coerceAtMost(w)

            // FBO is OpenGL bottom-left origin: y = 0 is video bottom, y = h - 1 is video top.
            val topColor = averageRegionColor(buf, w, 0, w, h - bandY, h)
            val bottomColor = averageRegionColor(buf, w, 0, w, 0, bandY)
            val leftColor = averageRegionColor(buf, w, 0, bandX, 0, h)
            val rightColor = averageRegionColor(buf, w, w - bandX, w, 0, h)

            val changed = !hasLastAmbientColors ||
                colorChangedNoticeably(topColor, lastTopColor) ||
                colorChangedNoticeably(rightColor, lastRightColor) ||
                colorChangedNoticeably(bottomColor, lastBottomColor) ||
                colorChangedNoticeably(leftColor, lastLeftColor)
            if (!changed) return

            lastTopColor = topColor
            lastRightColor = rightColor
            lastBottomColor = bottomColor
            lastLeftColor = leftColor
            hasLastAmbientColors = true

            mainHandler.post {
                ambientColorsCallback?.invoke(topColor, rightColor, bottomColor, leftColor)
            }
        }

        private fun averageRegionColor(
            buf: ByteBuffer,
            strideW: Int,
            x0: Int,
            x1: Int,
            y0: Int,
            y1: Int,
        ): Int {
            var sumR = 0L
            var sumG = 0L
            var sumB = 0L
            var count = 0
            for (y in y0 until y1) {
                var off = (y * strideW + x0) * 4
                for (x in x0 until x1) {
                    sumR += (buf.get(off).toInt() and 0xFF)
                    sumG += (buf.get(off + 1).toInt() and 0xFF)
                    sumB += (buf.get(off + 2).toInt() and 0xFF)
                    off += 4
                    count++
                }
            }
            if (count == 0) return 0xFF000000.toInt()
            val inv = 1f / (count * 255f)
            return boostSaturationToArgb(sumR * inv, sumG * inv, sumB * inv)
        }

        private fun boostSaturationToArgb(rIn: Float, gIn: Float, bIn: Float): Int {
            val luma = 0.2126f * rIn + 0.7152f * gIn + 0.0722f * bIn
            val r = (luma + (rIn - luma) * AMBIENT_SATURATION).coerceIn(0f, 1f)
            val g = (luma + (gIn - luma) * AMBIENT_SATURATION).coerceIn(0f, 1f)
            val b = (luma + (bIn - luma) * AMBIENT_SATURATION).coerceIn(0f, 1f)
            val ri = (r * 255f + 0.5f).toInt().coerceIn(0, 255)
            val gi = (g * 255f + 0.5f).toInt().coerceIn(0, 255)
            val bi = (b * 255f + 0.5f).toInt().coerceIn(0, 255)
            return (0xFF shl 24) or (ri shl 16) or (gi shl 8) or bi
        }

        private fun colorChangedNoticeably(a: Int, b: Int): Boolean {
            val dr = abs(((a ushr 16) and 0xFF) - ((b ushr 16) and 0xFF))
            val dg = abs(((a ushr 8) and 0xFF) - ((b ushr 8) and 0xFF))
            val db = abs((a and 0xFF) - (b and 0xFF))
            return dr >= 6 || dg >= 6 || db >= 6 || (dr + dg + db) >= 12
        }

        private fun releaseAmbientGl() {
            if (ambientFboId != 0) {
                GLES20.glDeleteFramebuffers(1, intArrayOf(ambientFboId), 0)
                ambientFboId = 0
            }
            if (ambientTexId != 0) {
                GLES20.glDeleteTextures(1, intArrayOf(ambientTexId), 0)
                ambientTexId = 0
            }
            if (ambientProgram != 0) {
                GLES20.glDeleteProgram(ambientProgram)
                ambientProgram = 0
            }
            ambientFboW = 0
            ambientFboH = 0
            ambientReadBuffer = null
            ambientInitFailed = false
            hasLastAmbientColors = false
            lastAmbientSampleMs = 0L
        }

        private fun initGl(): Boolean {
            eglDisplay = EGL14.eglGetDisplay(EGL14.EGL_DEFAULT_DISPLAY)
            if (eglDisplay == EGL14.EGL_NO_DISPLAY) return false
            val ver = IntArray(2)
            if (!EGL14.eglInitialize(eglDisplay, ver, 0, ver, 1)) return false

            val cfgAttrs = intArrayOf(
                EGL14.EGL_RED_SIZE, 8,
                EGL14.EGL_GREEN_SIZE, 8,
                EGL14.EGL_BLUE_SIZE, 8,
                EGL14.EGL_ALPHA_SIZE, 8,
                EGL14.EGL_RENDERABLE_TYPE, EGL14.EGL_OPENGL_ES2_BIT,
                EGL14.EGL_SURFACE_TYPE, EGL14.EGL_WINDOW_BIT,
                EGL14.EGL_NONE,
            )
            val configs = arrayOfNulls<EGLConfig>(1)
            val num = IntArray(1)
            if (!EGL14.eglChooseConfig(eglDisplay, cfgAttrs, 0, configs, 0, 1, num, 0) || num[0] == 0) {
                return false
            }
            val cfg = configs[0] ?: return false

            val ctxAttrs = intArrayOf(EGL14.EGL_CONTEXT_CLIENT_VERSION, 2, EGL14.EGL_NONE)
            eglContext = EGL14.eglCreateContext(eglDisplay, cfg, EGL14.EGL_NO_CONTEXT, ctxAttrs, 0)
            if (eglContext == EGL14.EGL_NO_CONTEXT) return false

            val surfAttrs = intArrayOf(EGL14.EGL_NONE)
            eglSurface = EGL14.eglCreateWindowSurface(eglDisplay, cfg, windowSurface, surfAttrs, 0)
            if (eglSurface == EGL14.EGL_NO_SURFACE) return false

            if (!EGL14.eglMakeCurrent(eglDisplay, eglSurface, eglSurface, eglContext)) return false
            EGL14.eglSwapInterval(eglDisplay, 1)

            program = buildProgram(VERTEX_SHADER, FRAGMENT_SHADER)
            if (program == 0) return false

            aPosLoc = GLES20.glGetAttribLocation(program, "aPos")
            aTexLoc = GLES20.glGetAttribLocation(program, "aTex")
            uStMatrixLoc = GLES20.glGetUniformLocation(program, "uStMatrix")
            uViewSizeLoc = GLES20.glGetUniformLocation(program, "uViewSize")
            uVideoRectLoc = GLES20.glGetUniformLocation(program, "uVideoRect")
            uCornerRadiusLoc = GLES20.glGetUniformLocation(program, "uCornerRadius")
            uTexSizeLoc = GLES20.glGetUniformLocation(program, "uTexSize")
            uTexelsPerPxLoc = GLES20.glGetUniformLocation(program, "uTexelsPerPx")
            uBgColorLoc = GLES20.glGetUniformLocation(program, "uBgColor")
            uAmbientEnabledLoc = GLES20.glGetUniformLocation(program, "uAmbientEnabled")
            uAmbientTexelSizeLoc = GLES20.glGetUniformLocation(program, "uAmbientTexelSize")
            uAmbientCoverScaleLoc = GLES20.glGetUniformLocation(program, "uAmbientCoverScale")

            // Bind distinct texture units for samplerExternalOES (0) and sampler2D (1)
            GLES20.glUseProgram(program)
            val uTexLoc = GLES20.glGetUniformLocation(program, "uTex")
            if (uTexLoc >= 0) GLES20.glUniform1i(uTexLoc, 0)
            val uAmbientTexLoc = GLES20.glGetUniformLocation(program, "uAmbientTex")
            if (uAmbientTexLoc >= 0) GLES20.glUniform1i(uAmbientTexLoc, 1)
            GLES20.glUseProgram(0)

            val tex = IntArray(1)
            GLES20.glGenTextures(1, tex, 0)
            oesTexId = tex[0]
            GLES20.glBindTexture(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, oesTexId)
            GLES20.glTexParameteri(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, GLES20.GL_TEXTURE_MIN_FILTER, GLES20.GL_LINEAR)
            GLES20.glTexParameteri(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, GLES20.GL_TEXTURE_MAG_FILTER, GLES20.GL_LINEAR)
            GLES20.glTexParameteri(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, GLES20.GL_TEXTURE_WRAP_S, GLES20.GL_CLAMP_TO_EDGE)
            GLES20.glTexParameteri(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, GLES20.GL_TEXTURE_WRAP_T, GLES20.GL_CLAMP_TO_EDGE)
            GLES20.glBindTexture(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, 0)

            // Default identity matrix until first frame arrives
            android.opengl.Matrix.setIdentityM(stMatrix, 0)
            return true
        }

        private fun releaseGl() {
            try {
                decoderSurface?.release()
            } catch (_: Throwable) {
            }
            decoderSurface = null
            try {
                surfaceTexture?.release()
            } catch (_: Throwable) {
            }
            surfaceTexture = null

            releaseAmbientGl()

            if (oesTexId != 0) {
                GLES20.glDeleteTextures(1, intArrayOf(oesTexId), 0)
                oesTexId = 0
            }
            if (program != 0) {
                GLES20.glDeleteProgram(program)
                program = 0
            }
            if (eglDisplay != EGL14.EGL_NO_DISPLAY) {
                EGL14.eglMakeCurrent(eglDisplay, EGL14.EGL_NO_SURFACE, EGL14.EGL_NO_SURFACE, EGL14.EGL_NO_CONTEXT)
                if (eglSurface != EGL14.EGL_NO_SURFACE) {
                    EGL14.eglDestroySurface(eglDisplay, eglSurface)
                }
                if (eglContext != EGL14.EGL_NO_CONTEXT) {
                    EGL14.eglDestroyContext(eglDisplay, eglContext)
                }
                EGL14.eglTerminate(eglDisplay)
            }
            eglSurface = EGL14.EGL_NO_SURFACE
            eglContext = EGL14.EGL_NO_CONTEXT
            eglDisplay = EGL14.EGL_NO_DISPLAY
        }
    }

    companion object {
        private const val TAG = "GlVideoSurfaceView"
        private const val AMBIENT_LONG_EDGE = 32
        private const val AMBIENT_EDGE_BAND = 3
        private const val AMBIENT_SAMPLE_INTERVAL_MS = 300L
        private const val AMBIENT_SATURATION = 1.4f

        private const val AMBIENT_VERTEX_SHADER = """
            precision mediump float;
            attribute vec2 aPos;
            attribute vec2 aTex;
            varying vec2 vUv;
            void main() {
                gl_Position = vec4(aPos, 0.0, 1.0);
                vUv = aTex;
            }
        """

        private const val AMBIENT_FRAGMENT_SHADER = """
            #extension GL_OES_EGL_image_external : require
            precision mediump float;

            uniform samplerExternalOES uTex;
            uniform mat4 uStMatrix;
            uniform vec2 uTexelSize;

            varying vec2 vUv;

            vec3 sampleOes(vec2 p) {
                vec2 c = clamp(p, vec2(0.001), vec2(0.999));
                vec2 uv = (uStMatrix * vec4(c, 0.0, 1.0)).xy;
                return texture2D(uTex, uv).rgb;
            }

            void main() {
                vec2 q = uTexelSize * 0.25;
                vec3 col = 0.25 * (
                    sampleOes(vUv + vec2(-q.x, -q.y)) +
                    sampleOes(vUv + vec2( q.x, -q.y)) +
                    sampleOes(vUv + vec2(-q.x,  q.y)) +
                    sampleOes(vUv + vec2( q.x,  q.y))
                );
                gl_FragColor = vec4(col, 1.0);
            }
        """

        private const val VERTEX_SHADER = """
            precision highp float;
            attribute vec2 aPos;
            attribute vec2 aTex;
            varying vec2 vScreenUv;
            void main() {
                gl_Position = vec4(aPos, 0.0, 1.0);
                // Top-left origin UV matching Android/Compose coordinates
                vScreenUv = vec2(aTex.x, 1.0 - aTex.y);
            }
        """

        private const val FRAGMENT_SHADER = """
            #extension GL_OES_EGL_image_external : require
            precision highp float;

            const vec3 LUMA_W = vec3(0.2126, 0.7152, 0.0722);

            uniform samplerExternalOES uTex;
            uniform sampler2D uAmbientTex;
            uniform mat4 uStMatrix;
            uniform vec2 uViewSize;
            uniform vec4 uVideoRect; // left, top, width, height in top-left px
            uniform float uCornerRadius;
            uniform vec2 uTexSize;
            uniform float uTexelsPerPx;
            uniform vec3 uBgColor;
            uniform float uAmbientEnabled;
            uniform vec2 uAmbientTexelSize;
            uniform vec2 uAmbientCoverScale;

            varying vec2 vScreenUv;

            float sdRoundedBox(vec2 p, vec2 halfSize, float r) {
                vec2 q = abs(p) - halfSize + vec2(r);
                return min(max(q.x, q.y), 0.0) + length(max(q, 0.0)) - r;
            }

            vec3 sampleVideo(vec2 stNorm) {
                vec2 halfTexel = 0.5 / uTexSize;
                vec2 clamped = clamp(stNorm, halfTexel, vec2(1.0) - halfTexel);
                vec2 uv = (uStMatrix * vec4(clamped, 0.0, 1.0)).xy;
                return texture2D(uTex, uv).rgb;
            }

            vec3 sampleAmbient(vec2 uv) {
                vec2 t = uAmbientTexelSize;
                vec3 acc =
                    texture2D(uAmbientTex, uv + vec2(-t.x, -t.y)).rgb * 1.0 +
                    texture2D(uAmbientTex, uv + vec2( 0.0, -t.y)).rgb * 2.0 +
                    texture2D(uAmbientTex, uv + vec2( t.x, -t.y)).rgb * 1.0 +
                    texture2D(uAmbientTex, uv + vec2(-t.x,  0.0)).rgb * 2.0 +
                    texture2D(uAmbientTex, uv                   ).rgb * 4.0 +
                    texture2D(uAmbientTex, uv + vec2( t.x,  0.0)).rgb * 2.0 +
                    texture2D(uAmbientTex, uv + vec2(-t.x,  t.y)).rgb * 1.0 +
                    texture2D(uAmbientTex, uv + vec2( 0.0,  t.y)).rgb * 2.0 +
                    texture2D(uAmbientTex, uv + vec2( t.x,  t.y)).rgb * 1.0;
                return acc * 0.0625;
            }

            // 5-tap Catmull-Rom cubic luma resampling matching the Windows viewer (torgb.rs):
            // keeps chroma purely smooth bilinear while sharpening luma curves with zero overshoot
            // and zero hue shift on colored text.
            vec3 sampleCatmullRomLuma(vec2 stNorm) {
                vec3 base = sampleVideo(stNorm);
                float yBase = dot(base, LUMA_W);

                vec2 p = stNorm * uTexSize;
                vec2 t1 = floor(p - 0.5) + 0.5;
                vec2 f = p - t1;
                vec2 w0 = f * (-0.5 + f * (1.0 - 0.5 * f));
                vec2 w1 = 1.0 + f * f * (-2.5 + 1.5 * f);
                vec2 w2 = f * (0.5 + f * (2.0 - 1.5 * f));
                vec2 w3 = f * f * (-0.5 + 0.5 * f);
                vec2 w12 = w1 + w2;
                vec2 t0 = t1 - 1.0;
                vec2 t12 = t1 + w2 / w12;
                vec2 t3 = t1 + 2.0;

                vec2 invSize = 1.0 / uTexSize;
                float yTop = dot(sampleVideo(vec2(t12.x, t0.y) * invSize), LUMA_W);
                float yLeft = dot(sampleVideo(vec2(t0.x, t12.y) * invSize), LUMA_W);
                float yMid = dot(sampleVideo(t12 * invSize), LUMA_W);
                float yRight = dot(sampleVideo(vec2(t3.x, t12.y) * invSize), LUMA_W);
                float yBot = dot(sampleVideo(vec2(t12.x, t3.y) * invSize), LUMA_W);

                float wt = w12.x * w0.y;
                float wl = w0.x * w12.y;
                float wm = w12.x * w12.y;
                float wr = w3.x * w12.y;
                float wb = w12.x * w3.y;
                float yCr = (yTop * wt + yLeft * wl + yMid * wm + yRight * wr + yBot * wb)
                    / (wt + wl + wm + wr + wb);

                // Ringing-free & hue-preserving clamp: never overshoot local luma or clip one channel
                float minY = min(yMid, min(min(yTop, yBot), min(yLeft, yRight)));
                float maxY = max(yMid, max(max(yTop, yBot), max(yLeft, yRight)));
                float dY = clamp(yCr, minY, maxY) - yBase;
                float minC = min(base.r, min(base.g, base.b));
                float maxC = max(base.r, max(base.g, base.b));
                dY = clamp(dY, -minC, 1.0 - maxC);
                return base + vec3(dY);
            }

            float catmullRom(float x) {
                x = abs(x);
                if (x < 1.0) return (1.5 * x - 2.5) * x * x + 1.0;
                if (x < 2.0) return ((-0.5 * x + 2.5) * x - 4.0) * x + 2.0;
                return 0.0;
            }

            // Mild shrink (e.g. 2560x1440 on a 1920x1080 view, 1.33 texels per px): Catmull-Rom
            // with its kernel widened by the shrink factor, i.e. a proper bicubic downscale.
            // Every stroke keeps the same weight wherever it lands on the screen grid, so thin
            // text stops alternating between crisp and doubled. Supports up to 1.5 texels/px.
            vec3 sampleScaledCatmullRom(vec2 stNorm) {
                float k = uTexelsPerPx;
                vec2 p = stNorm * uTexSize;
                vec2 c0 = floor(p - 0.5) - 2.0;
                vec2 invSize = 1.0 / uTexSize;
                vec2 halfTexel = 0.5 * invSize;
                // The SurfaceTexture transform is affine: map the first tap once, then step.
                vec2 uv0 = (uStMatrix * vec4(c0 * invSize, 0.0, 1.0)).xy;
                vec2 du = (uStMatrix * vec4(invSize.x, 0.0, 0.0, 0.0)).xy;
                vec2 dv = (uStMatrix * vec4(0.0, invSize.y, 0.0, 0.0)).xy;
                vec2 lo = halfTexel;
                vec2 hi = vec2(1.0) - halfTexel;
                float wx[6];
                float sx[6];
                for (int i = 0; i < 6; i++) {
                    float tx = c0.x + float(i) + 0.5;
                    wx[i] = catmullRom((tx - p.x) / k);
                    // Taps past the video edge reuse the edge texel (clamp in st space).
                    sx[i] = clamp(tx * invSize.x, lo.x, hi.x) * uTexSize.x - c0.x;
                }
                vec3 acc = vec3(0.0);
                float ws = 0.0;
                for (int j = 0; j < 6; j++) {
                    float ty = c0.y + float(j) + 0.5;
                    float wy = catmullRom((ty - p.y) / k);
                    float sy = clamp(ty * invSize.y, lo.y, hi.y) * uTexSize.y - c0.y;
                    vec2 rowUv = uv0 + dv * sy;
                    for (int i = 0; i < 6; i++) {
                        float w = wx[i] * wy;
                        acc += w * texture2D(uTex, rowUv + du * sx[i]).rgb;
                        ws += w;
                    }
                }
                return clamp(acc / ws, 0.0, 1.0);
            }

            void main() {
                vec2 px = vScreenUv * uViewSize;

                // Optional GPU rounded-corner clip over the view bounds
                if (uCornerRadius > 0.5) {
                    vec2 halfView = uViewSize * 0.5;
                    float d = sdRoundedBox(px - halfView, halfView, uCornerRadius);
                    if (d > 0.75) {
                        gl_FragColor = vec4(uBgColor, 1.0);
                        return;
                    }
                }

                // Map screen pixel into video normalized [0..1] (top-left origin)
                vec2 vidUv = (px - uVideoRect.xy) / uVideoRect.zw;
                if (vidUv.x < 0.0 || vidUv.x > 1.0 || vidUv.y < 0.0 || vidUv.y > 1.0) {
                    vec3 barCol;
                    if (uAmbientEnabled > 1.5) {
                        barCol = uBgColor;
                    } else if (uAmbientEnabled > 0.5) {
                        vec2 covUv = (vScreenUv - 0.5) * uAmbientCoverScale + 0.5;
                        vec2 ambUv = vec2(covUv.x, 1.0 - covUv.y);
                        vec3 amb = sampleAmbient(ambUv);
                        float luma = dot(amb, LUMA_W);
                        amb = clamp(mix(vec3(luma), amb, 1.4), 0.0, 1.0) * 0.5;

                        vec2 clampedPx = clamp(px, uVideoRect.xy, uVideoRect.xy + uVideoRect.zw);
                        float distToVideo = length(px - clampedPx);
                        float shadow = mix(0.55, 1.0, smoothstep(0.0, 14.0, distToVideo));
                        barCol = amb * shadow;
                    } else {
                        barCol = vec3(0.0);
                    }
                    if (uCornerRadius > 0.5) {
                        vec2 halfView = uViewSize * 0.5;
                        float d = sdRoundedBox(px - halfView, halfView, uCornerRadius);
                        float mask = 1.0 - smoothstep(-0.75, 0.75, d);
                        barCol = mix(uBgColor, barCol, mask);
                    }
                    gl_FragColor = vec4(barCol, 1.0);
                    return;
                }

                // SurfaceTexture transform expects bottom-left origin UV
                vec2 stIn = vec2(vidUv.x, 1.0 - vidUv.y);

                vec3 col;
                if (abs(uTexelsPerPx - 1.0) < 0.03) {
                    // Exact 1:1 native pixel match
                    col = sampleVideo(stIn);
                } else if (uTexelsPerPx < 0.97) {
                    // Zooming in: 5-tap luma Catmull-Rom + pure bilinear chroma
                    col = sampleCatmullRomLuma(stIn);
                } else if (uTexelsPerPx < 1.5) {
                    col = sampleScaledCatmullRom(stIn);
                } else if (uTexelsPerPx < 2.75) {
                    // Shrinking (e.g. 2560x1440 on 1272p phone): 2x2 box supersampling
                    vec2 stepSt = (uTexelsPerPx * 0.25) / uTexSize;
                    col = 0.25 * (
                        sampleVideo(stIn + vec2(-stepSt.x, -stepSt.y)) +
                        sampleVideo(stIn + vec2( stepSt.x, -stepSt.y)) +
                        sampleVideo(stIn + vec2(-stepSt.x,  stepSt.y)) +
                        sampleVideo(stIn + vec2( stepSt.x,  stepSt.y))
                    );
                } else {
                    // Strong shrink (Hub live preview): 4x4 box supersampling
                    vec2 pixelSpan = uTexelsPerPx / uTexSize;
                    vec3 acc = vec3(0.0);
                    for (int j = 0; j < 4; j++) {
                        float fy = (float(j) + 0.5) * 0.25 - 0.5;
                        for (int i = 0; i < 4; i++) {
                            float fx = (float(i) + 0.5) * 0.25 - 0.5;
                            acc += sampleVideo(stIn + vec2(fx, fy) * pixelSpan);
                        }
                    }
                    col = acc * 0.0625;
                }

                // Smooth anti-aliased edge blended into container background color (light & dark themes)
                if (uCornerRadius > 0.5) {
                    vec2 halfView = uViewSize * 0.5;
                    float d = sdRoundedBox(px - halfView, halfView, uCornerRadius);
                    float mask = 1.0 - smoothstep(-0.75, 0.75, d);
                    col = mix(uBgColor, col, mask);
                }

                gl_FragColor = vec4(col, 1.0);
            }
        """

        private fun buildProgram(vsSrc: String, fsSrc: String): Int {
            val vs = compileShader(GLES20.GL_VERTEX_SHADER, vsSrc)
            if (vs == 0) return 0
            val fs = compileShader(GLES20.GL_FRAGMENT_SHADER, fsSrc)
            if (fs == 0) {
                GLES20.glDeleteShader(vs)
                return 0
            }
            val prog = GLES20.glCreateProgram()
            GLES20.glAttachShader(prog, vs)
            GLES20.glAttachShader(prog, fs)
            GLES20.glLinkProgram(prog)
            val status = IntArray(1)
            GLES20.glGetProgramiv(prog, GLES20.GL_LINK_STATUS, status, 0)
            GLES20.glDeleteShader(vs)
            GLES20.glDeleteShader(fs)
            if (status[0] == 0) {
                Log.e(TAG, "Shader link failed: ${GLES20.glGetProgramInfoLog(prog)}")
                GLES20.glDeleteProgram(prog)
                return 0
            }
            return prog
        }

        private fun compileShader(type: Int, src: String): Int {
            val shader = GLES20.glCreateShader(type)
            // Trim leading newline/indent so #extension sits strictly at line 1 col 0 on strict
            // mobile GLSL ES 1.00 compilers (e.g. Huawei Maleoon / Mali).
            GLES20.glShaderSource(shader, src.trimIndent().trimStart())
            GLES20.glCompileShader(shader)
            val status = IntArray(1)
            GLES20.glGetShaderiv(shader, GLES20.GL_COMPILE_STATUS, status, 0)
            if (status[0] == 0) {
                Log.e(TAG, "Shader compile failed: ${GLES20.glGetShaderInfoLog(shader)}")
                GLES20.glDeleteShader(shader)
                return 0
            }
            return shader
        }
    }
}
