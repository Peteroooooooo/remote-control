package com.rc.android.ui

import android.view.Surface
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import com.rc.android.video.GlVideoSurfaceView

/**
 * Tear-free GPU video canvas powered by MediaCodec -> SurfaceTexture -> GLES 2.0 -> SurfaceView.
 * Preserves strict 1:1 aspect ratio, supports smooth matrix zoom/pan, GPU rounded corners,
 * and adaptive zoom sharpening without SurfaceView/Compose punch-through artifacts.
 */
@Composable
fun VideoSurface(
    streamWidth: Int,
    streamHeight: Int,
    onSurfaceReady: (Surface) -> Unit,
    onSurfaceDestroyed: (Surface) -> Unit,
    modifier: Modifier = Modifier,
    zoomScale: Float = 1f,
    panX: Float = 0f,
    panY: Float = 0f,
    cornerRadius: Dp = 0.dp,
    outsideColor: Color = Color.Black,
) {
    val density = LocalDensity.current
    val cornerRadiusPx = with(density) { cornerRadius.toPx() }
    var glViewRef by remember { mutableStateOf<GlVideoSurfaceView?>(null) }

    DisposableEffect(Unit) {
        onDispose {
            glViewRef?.release()
            glViewRef = null
        }
    }

    Box(modifier = modifier.background(outsideColor)) {
        AndroidView(
            modifier = Modifier.fillMaxSize(),
            factory = { ctx ->
                GlVideoSurfaceView(
                    context = ctx,
                    onDecoderSurfaceReady = onSurfaceReady,
                    onDecoderSurfaceDestroyed = onSurfaceDestroyed,
                ).also { view ->
                    glViewRef = view
                    view.updateRenderParams(
                        streamW = streamWidth,
                        streamH = streamHeight,
                        zoom = zoomScale,
                        px = panX,
                        py = panY,
                        cornerRadius = cornerRadiusPx,
                        outsideR = outsideColor.red,
                        outsideG = outsideColor.green,
                        outsideB = outsideColor.blue,
                    )
                }
            },
            update = { view ->
                view.updateRenderParams(
                    streamW = streamWidth,
                    streamH = streamHeight,
                    zoom = zoomScale,
                    px = panX,
                    py = panY,
                    cornerRadius = cornerRadiusPx,
                    outsideR = outsideColor.red,
                    outsideG = outsideColor.green,
                    outsideB = outsideColor.blue,
                )
            },
        )
    }
}
