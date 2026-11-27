package com.rc.android.ui

import android.graphics.Bitmap
import android.graphics.BlurMaskFilter
import android.graphics.Matrix
import android.graphics.Paint
import android.graphics.Path as NativePath
import android.graphics.RectF
import android.graphics.SweepGradient
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.Stable
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.withFrameNanos
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.graphics.drawscope.DrawScope
import kotlin.math.ceil
import kotlinx.coroutines.delay
import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.core.animateDpAsState
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.width
import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.Spring
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.spring
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.collectIsPressedAsState
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxScope
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.composed
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.draw.scale
import androidx.compose.ui.draw.shadow
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.PathEffect
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.StrokeJoin
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.nativeCanvas
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.rc.android.model.QualityPrefs

/**
 * Floating Liquid Glass container — reserved strictly for floating controls
 * (video HUD badges, fullscreen capsule, macOS floating Dock, popovers, bottom sheets).
 * Content lists and cards stay clean flat Apple surfaces without borders or shadows.
 */
@Composable
fun FloatingGlass(
    modifier: Modifier = Modifier,
    cornerRadius: Dp = 20.dp,
    darkOverlay: Boolean = false,
    elevation: Dp = 12.dp,
    backdrop: GlassBackdrop? = null,
    content: @Composable BoxScope.() -> Unit,
) {
    val c = RcThemeColors.current
    val shape = RoundedCornerShape(cornerRadius)
    val isDarkGlass = darkOverlay || c.isDark
    val frosted = backdrop != null && supportsBackdropBlur
    val glassFill = when {
        // Over a real blur the tint can be light, so the frosted content shows through.
        frosted && isDarkGlass -> Brush.verticalGradient(
            listOf(Color(0x6B2C2D34), Color(0x5C17181C)),
        )
        frosted -> Brush.verticalGradient(
            listOf(Color(0x8CFFFFFF), Color(0x73F4F4F9)),
        )
        darkOverlay -> Brush.verticalGradient(
            listOf(Color(0xC224242A), Color(0xB3151519)),
        )
        c.isDark -> Brush.verticalGradient(
            listOf(Color(0xC428282E), Color(0xB518181D)),
        )
        else -> Brush.verticalGradient(
            listOf(Color(0xD9FFFFFF), Color(0xC4F4F4F9)),
        )
    }
    val borderBrush = if (isDarkGlass) {
        Brush.verticalGradient(
            0.0f to Color(0x52FFFFFF),
            0.35f to Color(0x29FFFFFF),
            1.0f to Color(0x14FFFFFF),
        )
    } else {
        Brush.verticalGradient(
            0.0f to Color(0xE6FFFFFF),
            0.4f to Color(0x26000000),
            1.0f to Color(0x18000000),
        )
    }
    val specularTop = if (darkOverlay) Color(0x47FFFFFF) else c.dockSpecularTop
    val shadowAmbient = if (isDarkGlass) Color(0x9E000000) else Color(0x2E000000)

    Box(
        modifier = modifier
            .shadow(
                elevation = elevation,
                shape = shape,
                ambientColor = shadowAmbient,
                spotColor = shadowAmbient,
            )
            .clip(shape)
            .then(if (frosted) Modifier.glassBackdropBlur(backdrop!!, pageColor = c.bg) else Modifier)
            .background(glassFill)
            .drawWithContent {
                drawContent()
                val r = cornerRadius.toPx().coerceAtMost(size.minDimension / 2f)
                drawRoundRect(
                    brush = Brush.verticalGradient(
                        0.0f to specularTop.copy(alpha = if (isDarkGlass) 0.16f else 0.55f),
                        0.16f to Color.Transparent,
                    ),
                    cornerRadius = CornerRadius(r, r),
                )
            }
            .border(width = 0.75.dp, brush = borderBrush, shape = shape),
        content = content,
    )
}

private val AmbientSweepColors = intArrayOf(
    0xFF5AC8FA.toInt(),
    0xFF0A84FF.toInt(),
    0xFFBF5AF2.toInt(),
    0xFFFF375F.toInt(),
    0xFFFF9F0A.toInt(),
    0xFF5AC8FA.toInt(),
)

/** One blurred stroke of the halo: stroke width, blur radius (dp) and opacity. */
private class HaloStroke(val strokeDp: Float, val blurDp: Float, val alpha: Float)

// Four blurred strokes on the frame edge, from tight to very wide. Each is a Gaussian that
// peaks at the edge, so together the light fades from the frame outward with no visible line.
private val HaloStrokes = listOf(
    HaloStroke(strokeDp = 4f, blurDp = 8f, alpha = 0.55f),
    HaloStroke(strokeDp = 10f, blurDp = 24f, alpha = 0.36f),
    HaloStroke(strokeDp = 18f, blurDp = 52f, alpha = 0.26f),
    HaloStroke(strokeDp = 28f, blurDp = 90f, alpha = 0.16f),
)

/** How far the light reaches past the frame (the widest blur fades out by about 1.7x its radius). */
private const val HALO_REACH_DP = 170f

/** The mask is very soft, so half the screen's pixels are plenty. */
private const val HALO_MASK_SCALE = 0.5f

private const val HALO_TURN_MS = 7000L

/** About 30 frames a second: the slow turn looks the same, at a quarter of a 120 Hz screen's redraws. */
private const val HALO_FRAME_MS = 28L

/**
 * The halo's shape, blurred once for a frame size: an alpha-only bitmap. Each frame draws it
 * once, coloured by the rotating sweep gradient, instead of blurring four strokes again.
 */
internal class HaloCache {
    private var w = 0f
    private var h = 0f
    var mask: Bitmap? = null
        private set
    val dst = RectF()
    val frame = NativePath()
    val matrix = Matrix()
    var shader: SweepGradient? = null
        private set
    val paint = Paint(Paint.ANTI_ALIAS_FLAG or Paint.FILTER_BITMAP_FLAG)

    fun release() {
        mask?.recycle()
        mask = null
    }

    fun update(w: Float, h: Float, cornerPx: Float, density: Float) {
        if (mask != null && w == this.w && h == this.h) return
        this.w = w
        this.h = h
        val padPx = HALO_REACH_DP * density
        val bw = ceil((w + 2 * padPx) * HALO_MASK_SCALE).toInt()
        val bh = ceil((h + 2 * padPx) * HALO_MASK_SCALE).toInt()
        mask?.recycle()
        val bmp = Bitmap.createBitmap(bw, bh, Bitmap.Config.ALPHA_8)
        val cv = android.graphics.Canvas(bmp)
        cv.scale(HALO_MASK_SCALE, HALO_MASK_SCALE)
        cv.translate(padPx, padPx)
        val p = Paint(Paint.ANTI_ALIAS_FLAG).apply { style = Paint.Style.STROKE }
        for (s in HaloStrokes) {
            p.strokeWidth = s.strokeDp * density
            p.alpha = (s.alpha * 255f).toInt()
            p.maskFilter = BlurMaskFilter(s.blurDp * density, BlurMaskFilter.Blur.NORMAL)
            cv.drawRoundRect(0f, 0f, w, h, cornerPx, cornerPx, p)
        }
        mask = bmp
        dst.set(-padPx, -padPx, w + padPx, h + padPx)
        frame.reset()
        frame.addRoundRect(0f, 0f, w, h, cornerPx, cornerPx, NativePath.Direction.CW)
        shader = SweepGradient(w / 2f, h / 2f, AmbientSweepColors, null)
        paint.shader = shader
    }
}

/**
 * Apple Intelligence / visionOS style rotating colour halo: soft light that fades out from a
 * frame's edge with no hard outline. One per screen; draw it with [drawHalo] or [ambientHalo].
 */
@Stable
class HaloState internal constructor() {
    internal val angle = mutableFloatStateOf(0f)
    internal val cache = HaloCache()
}

@Composable
fun rememberHalo(): HaloState {
    val halo = remember { HaloState() }
    LaunchedEffect(halo) {
        val start = withFrameNanos { it }
        while (true) {
            withFrameNanos { now ->
                val ms = (now - start) / 1_000_000L
                halo.angle.floatValue = (ms % HALO_TURN_MS) * 360f / HALO_TURN_MS
            }
            delay(HALO_FRAME_MS)
        }
    }
    DisposableEffect(halo) { onDispose { halo.cache.release() } }
    return halo
}

/**
 * Draws [halo] around [frame] (in this scope's coordinates), never inside it, so a live
 * picture there stays untouched. Reads the angle, so the caller redraws as it turns.
 */
fun DrawScope.drawHalo(halo: HaloState, frame: Rect, cornerRadius: Dp) {
    if (frame.width <= 0f || frame.height <= 0f) return
    val cache = halo.cache
    cache.update(frame.width, frame.height, cornerRadius.toPx(), density)
    val mask = cache.mask ?: return
    cache.matrix.setRotate(halo.angle.floatValue, frame.width / 2f, frame.height / 2f)
    cache.shader?.setLocalMatrix(cache.matrix)
    val canvas = drawContext.canvas.nativeCanvas
    canvas.save()
    canvas.translate(frame.left, frame.top)
    canvas.clipOutPath(cache.frame)
    // An alpha-only bitmap is drawn in the paint's shader colours.
    canvas.drawBitmap(mask, null, cache.dst, cache.paint)
    canvas.restore()
}

fun Modifier.ambientHalo(halo: HaloState, frame: () -> Rect, cornerRadius: Dp): Modifier =
    drawBehind { drawHalo(halo, frame(), cornerRadius) }

/**
 * Minimalist WeChat / iOS navigation left chevron (`<`).
 * Slim rounded 90° stroke, optically centered, frameless.
 */
@Composable
fun BackArrowIcon(
    tint: Color,
    size: Dp = 20.dp,
    strokeWidth: Dp = 1.85.dp,
    modifier: Modifier = Modifier,
) {
    Canvas(modifier = modifier.size(size)) {
        val w = this.size.width
        val h = this.size.height
        val stroke = Stroke(
            width = strokeWidth.toPx(),
            cap = StrokeCap.Round,
            join = StrokeJoin.Round,
        )
        val chevron = Path().apply {
            moveTo(w * 0.64f, h * 0.19f)
            lineTo(w * 0.33f, h * 0.50f)
            lineTo(w * 0.64f, h * 0.81f)
        }
        drawPath(path = chevron, color = tint, style = stroke)
    }
}

/**
 * Thin settings gear in the Windows Fluent style the PC lobby uses (Segoe Fluent U+E713):
 * 6 wide flat-topped teeth, rounded U valleys, an open hub, every corner softened.
 */
@Composable
fun GearIcon(
    tint: Color,
    size: Dp = 21.dp,
    strokeWidth: Dp = 1.5.dp,
    modifier: Modifier = Modifier,
) {
    Canvas(modifier = modifier.size(size)) {
        val s = this.size.minDimension
        val cx = this.size.width / 2f
        val cy = this.size.height / 2f
        val teeth = 6
        val outer = s * 0.45f
        val root = s * 0.345f
        val pitch = (2.0 * Math.PI / teeth).toFloat()
        val topHalf = pitch * 0.22f
        val baseHalf = pitch * 0.34f
        fun pt(r: Float, ang: Float) = Offset(cx + r * kotlin.math.cos(ang), cy + r * kotlin.math.sin(ang))
        val gear = Path()
        for (i in 0 until teeth) {
            val a = i * pitch - (Math.PI / 2).toFloat()
            val pts = listOf(
                pt(root, a - baseHalf),
                pt(outer, a - topHalf),
                pt(outer, a + topHalf),
                pt(root, a + baseHalf),
            )
            pts.forEachIndexed { k, p -> if (i == 0 && k == 0) gear.moveTo(p.x, p.y) else gear.lineTo(p.x, p.y) }
        }
        gear.close()
        val stroke = Stroke(
            width = strokeWidth.toPx(),
            cap = StrokeCap.Round,
            join = StrokeJoin.Round,
            pathEffect = PathEffect.cornerPathEffect(s * 0.075f),
        )
        drawPath(path = gear, color = tint, style = stroke)
        drawCircle(
            color = tint,
            radius = s * 0.15f,
            center = Offset(cx, cy),
            style = Stroke(width = strokeWidth.toPx()),
        )
    }
}

/**
 * Frameless WeChat-style top-bar back button (pure `<` icon, no circle rim, no text).
 */
@Composable
fun NavBackButton(
    onClick: () -> Unit,
    compact: Boolean = false,
    modifier: Modifier = Modifier,
) {
    val c = RcThemeColors.current
    Box(
        modifier = modifier
            .size(if (compact) 34.dp else 38.dp)
            .clip(CircleShape)
            .liquidPress(pressedScale = 0.88f, onClick = onClick),
        contentAlignment = Alignment.Center,
    ) {
        BackArrowIcon(
            tint = c.label,
            size = if (compact) 20.dp else 22.dp,
            strokeWidth = 1.9.dp,
        )
    }
}

/**
 * Frameless Apple nav-bar icon button (no circle outline, no bright rim).
 */
@Composable
fun FramelessIconButton(
    icon: ImageVector,
    contentDescription: String?,
    tint: Color = RcThemeColors.current.label,
    iconSize: Dp = 21.dp,
    enabled: Boolean = true,
    modifier: Modifier = Modifier,
    onClick: () -> Unit,
) {
    Box(
        modifier = modifier
            .size(36.dp)
            .clip(CircleShape)
            .liquidPress(enabled = enabled, pressedScale = 0.88f, onClick = onClick),
        contentAlignment = Alignment.Center,
    ) {
        Icon(
            imageVector = icon,
            contentDescription = contentDescription,
            tint = tint,
            modifier = Modifier.size(iconSize),
        )
    }
}

/**
 * Hairline separator inside Apple Inset Grouped Lists.
 */
@Composable
fun HairlineDivider(
    modifier: Modifier = Modifier,
) {
    val c = RcThemeColors.current
    Box(
        modifier = modifier
            .fillMaxWidth()
            .height(0.5.dp)
            .background(c.separator),
    )
}

/**
 * Fluid spring press scale + subtle tactile feedback.
 */
fun Modifier.liquidPress(
    enabled: Boolean = true,
    pressedScale: Float = 0.968f,
    haptic: Boolean = true,
    onClick: () -> Unit,
): Modifier = composed {
    val interaction = remember { MutableInteractionSource() }
    val pressed by interaction.collectIsPressedAsState()
    val scale by animateFloatAsState(
        targetValue = if (pressed && enabled) pressedScale else 1f,
        animationSpec = spring(
            dampingRatio = Spring.DampingRatioNoBouncy,
            stiffness = Spring.StiffnessMediumLow,
        ),
        label = "liquidScale",
    )
    val haptics = LocalHapticFeedback.current
    this
        .scale(scale)
        .clickable(
            interactionSource = interaction,
            indication = null,
            enabled = enabled,
            onClick = {
                if (haptic) {
                    haptics.performHapticFeedback(HapticFeedbackType.TextHandleMove)
                }
                onClick()
            },
        )
}

/**
 * Quiet Apple status indicator dot.
 */
@Composable
fun StatusDot(
    color: Color,
    breathing: Boolean = false,
    size: Dp = 6.5.dp,
    modifier: Modifier = Modifier,
) {
    val alpha = if (breathing) {
        val inf = rememberInfiniteTransition(label = "dotBreath")
        val a by inf.animateFloat(
            initialValue = 0.45f,
            targetValue = 1.0f,
            animationSpec = infiniteRepeatable(
                animation = tween(1500, easing = FastOutSlowInEasing),
                repeatMode = RepeatMode.Reverse,
            ),
            label = "dotAlpha",
        )
        a
    } else {
        1f
    }

    Box(
        modifier = modifier
            .size(size)
            .clip(CircleShape)
            .background(color.copy(alpha = alpha)),
    )
}

/**
 * Subtle neutral ripple for connecting / loading states.
 */
@Composable
fun SubtleGlassRing(
    size: Dp = 60.dp,
    modifier: Modifier = Modifier,
    center: @Composable BoxScope.() -> Unit,
) {
    val inf = rememberInfiniteTransition(label = "glassRing")
    val p by inf.animateFloat(
        initialValue = 0f,
        targetValue = 1f,
        animationSpec = infiniteRepeatable(
            animation = tween(1900, easing = FastOutSlowInEasing),
            repeatMode = RepeatMode.Restart,
        ),
        label = "ringP",
    )

    Box(modifier = modifier.size(size), contentAlignment = Alignment.Center) {
        Canvas(modifier = Modifier.fillMaxSize()) {
            val minR = this.size.minDimension * 0.28f
            val maxR = this.size.minDimension * 0.48f
            val r = minR + (maxR - minR) * p
            drawCircle(
                color = Color.White.copy(alpha = (1f - p) * 0.24f),
                radius = r,
                style = Stroke(width = 1.dp.toPx()),
            )
        }
        center()
    }
}

val QualityResPresets = listOf(
    0 to "自动",
    1 to "原生",
    1440 to "1440p",
    1080 to "1080p",
    720 to "720p",
)

val QualityFpsPresets = listOf(
    0 to "自动",
    30 to "30",
    60 to "60",
    120 to "120",
    144 to "144",
)

val QualityMbpsPresets = listOf(
    0 to "自动",
    10 to "10M",
    20 to "20M",
    40 to "40M",
    80 to "80M",
)

/** User-facing name of the live connection path reported by the engine. */
fun pathKindLabel(kind: String): String = when (kind) {
    "direct" -> "直连"
    "relay" -> "自建中转"
    "public" -> "公共中转"
    else -> ""
}

/** Text colors used on dark floating surfaces (popovers, capsule, key bar). */
object RcInk {
    val primary = Color(0xFFFFFFFF)
    val secondary = Color(0x99EBEBF5)
    val tertiary = Color(0x4DEBEBF5)
    val hairline = Color(0x33FFFFFF)
    val fill = Color(0x3D767680)
    val thumb = Color(0xFF636366)
    val accent = Color(0xFF0A84FF)
    val accentTint = Color(0x330A84FF)
}

/**
 * iOS-style switch: green track when on, white knob sliding with a soft spring.
 */
@Composable
fun RcSwitch(
    checked: Boolean,
    onCheckedChange: (Boolean) -> Unit,
    modifier: Modifier = Modifier,
) {
    val haptics = LocalHapticFeedback.current
    val track by animateColorAsState(
        targetValue = if (checked) Color(0xFF30D158) else RcInk.fill,
        animationSpec = tween(180),
        label = "switchTrack",
    )
    val knobX by animateDpAsState(
        targetValue = if (checked) 20.dp else 0.dp,
        animationSpec = spring(dampingRatio = 0.8f, stiffness = Spring.StiffnessMediumLow),
        label = "switchKnob",
    )
    Box(
        modifier = modifier
            .size(width = 51.dp, height = 31.dp)
            .clip(CircleShape)
            .background(track)
            .clickable(
                interactionSource = remember { MutableInteractionSource() },
                indication = null,
            ) {
                haptics.performHapticFeedback(HapticFeedbackType.TextHandleMove)
                onCheckedChange(!checked)
            }
            .padding(2.dp),
    ) {
        Box(
            modifier = Modifier
                .offset(x = knobX)
                .size(27.dp)
                .shadow(3.dp, CircleShape)
                .clip(CircleShape)
                .background(Color.White),
        )
    }
}

/**
 * Shared live stream quality selector (Hub telemetry popover and Fullscreen `⋯` popover):
 * three iOS segmented rows plus a "只走中转" switch.
 */
@Composable
fun QualitySettingsSection(
    quality: QualityPrefs,
    onUpdateQuality: ((QualityPrefs) -> QualityPrefs) -> Unit,
    showRouteOption: Boolean = false,
    routeHint: String = "",
    compact: Boolean = false,
    modifier: Modifier = Modifier,
) {
    Column(
        modifier = modifier.fillMaxWidth(),
        verticalArrangement = Arrangement.spacedBy(if (compact) 8.dp else 12.dp),
    ) {
        QualitySegmentedRow(
            title = "分辨率",
            options = QualityResPresets,
            selectedValue = quality.resPreset,
            compact = compact,
            onSelect = { v -> onUpdateQuality { it.copy(resPreset = v) } },
        )
        QualitySegmentedRow(
            title = "帧率",
            options = QualityFpsPresets,
            selectedValue = quality.fpsPreset,
            compact = compact,
            onSelect = { v -> onUpdateQuality { it.copy(fpsPreset = v) } },
        )
        QualitySegmentedRow(
            title = "码率上限",
            options = QualityMbpsPresets,
            selectedValue = quality.mbpsPreset,
            compact = compact,
            onSelect = { v -> onUpdateQuality { it.copy(mbpsPreset = v) } },
        )
        if (showRouteOption) {
            Box(
                modifier = Modifier
                    .fillMaxWidth()
                    .height(0.5.dp)
                    .background(RcInk.hairline),
            )
            Row(
                modifier = Modifier.fillMaxWidth(),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Column(modifier = Modifier.weight(1f)) {
                    Text(
                        text = "只走中转",
                        color = RcInk.primary,
                        fontSize = 15.sp,
                        fontWeight = FontWeight.Normal,
                    )
                    if (routeHint.isNotEmpty()) {
                        Text(
                            text = routeHint,
                            color = RcInk.secondary,
                            fontSize = 12.sp,
                            modifier = Modifier.padding(top = 1.dp),
                        )
                    }
                }
                RcSwitch(
                    checked = quality.relayOnly,
                    onCheckedChange = { on -> onUpdateQuality { it.copy(relayOnly = on) } },
                )
            }
        }
    }
}

@Composable
fun QualitySegmentedRow(
    title: String,
    options: List<Pair<Int, String>>,
    selectedValue: Int,
    compact: Boolean = false,
    onSelect: (Int) -> Unit,
) {
    val currentOnSelect by rememberUpdatedState(onSelect)
    val haptics = LocalHapticFeedback.current
    val selectedIndex = options.indexOfFirst { it.first == selectedValue }.coerceAtLeast(0)
    val thumbPos by animateFloatAsState(
        targetValue = selectedIndex.toFloat(),
        animationSpec = spring(dampingRatio = 0.85f, stiffness = Spring.StiffnessMediumLow),
        label = "segThumb",
    )
    Column(verticalArrangement = Arrangement.spacedBy(if (compact) 5.dp else 6.dp)) {
        Text(
            text = title,
            color = RcInk.secondary,
            fontSize = if (compact) 12.sp else 13.sp,
            fontWeight = FontWeight.Normal,
        )
        BoxWithConstraints(
            modifier = Modifier
                .fillMaxWidth()
                .height(if (compact) 30.dp else 32.dp)
                .clip(RoundedCornerShape(9.dp))
                .background(RcInk.fill)
                .padding(2.dp),
        ) {
            val segW = maxWidth / options.size
            // Sliding thumb under the selected label
            Box(
                modifier = Modifier
                    .offset(x = segW * thumbPos)
                    .width(segW)
                    .fillMaxHeight()
                    .shadow(2.dp, RoundedCornerShape(7.dp))
                    .clip(RoundedCornerShape(7.dp))
                    .background(RcInk.thumb),
            )
            Row(modifier = Modifier.fillMaxSize(), verticalAlignment = Alignment.CenterVertically) {
                options.forEachIndexed { i, (value, label) ->
                    key(value) {
                        val selected = i == selectedIndex
                        // Thin separators between unselected neighbours, like iOS
                        val showSep = i > 0 && i != selectedIndex && i - 1 != selectedIndex
                        Box(
                            modifier = Modifier
                                .weight(1f)
                                .fillMaxHeight()
                                .clickable(
                                    interactionSource = remember { MutableInteractionSource() },
                                    indication = null,
                                ) {
                                    if (!selected) {
                                        haptics.performHapticFeedback(HapticFeedbackType.TextHandleMove)
                                        currentOnSelect(value)
                                    }
                                },
                            contentAlignment = Alignment.Center,
                        ) {
                            if (showSep) {
                                Box(
                                    modifier = Modifier
                                        .align(Alignment.CenterStart)
                                        .width(0.5.dp)
                                        .height(14.dp)
                                        .background(RcInk.hairline),
                                )
                            }
                            Text(
                                text = label,
                                color = RcInk.primary,
                                fontSize = if (compact) 12.sp else 13.sp,
                                fontWeight = if (selected) FontWeight.SemiBold else FontWeight.Medium,
                                maxLines = 1,
                                overflow = TextOverflow.Clip,
                            )
                        }
                    }
                }
            }
        }
    }
}
