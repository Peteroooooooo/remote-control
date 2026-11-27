package com.rc.android.ui

import android.os.Build
import androidx.compose.runtime.Composable
import androidx.compose.runtime.Stable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.composed
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.BlurEffect
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.TileMode
import androidx.compose.ui.graphics.drawscope.translate
import androidx.compose.ui.graphics.layer.GraphicsLayer
import androidx.compose.ui.graphics.layer.drawLayer
import androidx.compose.ui.graphics.rememberGraphicsLayer
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.layout.positionInRoot
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp

/**
 * Real frosted glass. The page records what it draws into [layer]; glass surfaces placed
 * above it draw that recording again, blurred, behind their own content.
 */
@Stable
class GlassBackdrop internal constructor(internal val layer: GraphicsLayer) {
    internal var origin by mutableStateOf(Offset.Zero)
}

/** Backdrop blur needs RenderEffect (Android 12+); older systems keep the plain tinted glass. */
val supportsBackdropBlur: Boolean = Build.VERSION.SDK_INT >= Build.VERSION_CODES.S

@Composable
fun rememberGlassBackdrop(): GlassBackdrop {
    val layer = rememberGraphicsLayer()
    return remember(layer) { GlassBackdrop(layer) }
}

/** Marks the content that glass surfaces should see through. Must not contain those surfaces. */
fun Modifier.glassBackdropSource(backdrop: GlassBackdrop): Modifier = this
    .onGloballyPositioned { backdrop.origin = it.positionInRoot() }
    .drawWithContent {
        backdrop.layer.record { this@drawWithContent.drawContent() }
        drawLayer(backdrop.layer)
    }

/** Draws the blurred backdrop under this element. Put it after the element's clip. */
fun Modifier.glassBackdropBlur(
    backdrop: GlassBackdrop,
    pageColor: Color,
    radius: Dp = 26.dp,
): Modifier = composed {
    if (!supportsBackdropBlur) return@composed this
    val blurLayer = rememberGraphicsLayer()
    var origin by remember { mutableStateOf(Offset.Zero) }
    this
        .onGloballyPositioned { origin = it.positionInRoot() }
        .drawBehind {
            val r = radius.toPx()
            blurLayer.renderEffect = BlurEffect(r, r, TileMode.Clamp)
            blurLayer.record {
                // Opaque page colour first, so the blurred copy fully hides the sharp page below.
                drawRect(pageColor)
                translate(backdrop.origin.x - origin.x, backdrop.origin.y - origin.y) {
                    drawLayer(backdrop.layer)
                }
            }
            drawLayer(blurLayer)
        }
}
