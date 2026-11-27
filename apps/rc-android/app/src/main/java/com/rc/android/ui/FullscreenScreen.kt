package com.rc.android.ui

import android.content.res.Configuration
import android.os.Build
import android.os.SystemClock
import android.os.VibrationEffect
import android.os.Vibrator
import android.os.VibratorManager
import android.view.Surface
import androidx.activity.compose.BackHandler
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.Spring
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.spring
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.scaleIn
import androidx.compose.animation.scaleOut
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutVertically
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.collectIsPressedAsState
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.displayCutout
import androidx.compose.foundation.layout.displayCutoutPadding
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.ime
import androidx.compose.foundation.layout.imeAnimationTarget
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.isImeVisible
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBars
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.union
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.Keyboard
import androidx.compose.material.icons.rounded.KeyboardHide
import androidx.compose.material.icons.rounded.MoreHoriz
import androidx.compose.material.icons.rounded.Mouse
import androidx.compose.material.icons.rounded.PowerSettingsNew
import androidx.compose.material.icons.rounded.Refresh
import androidx.compose.material.icons.rounded.ScreenRotation
import androidx.compose.material.icons.rounded.TouchApp
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.runtime.withFrameNanos
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.scale
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.FilterQuality
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.StrokeJoin
import androidx.compose.ui.graphics.TransformOrigin
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.input.pointer.PointerId
import androidx.compose.ui.input.pointer.PointerType
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.layout.boundsInRoot
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import com.rc.android.model.MonitorInfo
import com.rc.android.model.QualityPrefs
import com.rc.android.model.RcUiState
import com.rc.android.model.TouchMode
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeoutOrNull
import kotlin.math.abs
import kotlin.math.hypot
import kotlin.math.pow
import kotlin.math.roundToInt

private enum class PadGesture {
    Pending,
    EdgeScroll,
    TrackpadMove,
    Dragging,
    LongPressed,
    LongPressDragging,
    TwoPending,
    TwoScroll,
    TwoPinchRemote,
    TwoPinchLocal,
    ThreePending,
    ThreePanZoomLocal,
    Done,
}

/** One-finger direct mode on a physical desktop (see the gesture table in the handler). */
private enum class DirectGesture {
    Pending,
    Pan,
    Hold,
    HoldDrag,
    TwoPending,
    TwoScroll,
    TwoPinch,
    Three,
    Done,
}

private typealias ModState = KeyBarModState

@OptIn(ExperimentalLayoutApi::class)
@Composable
fun FullscreenScreen(
    state: RcUiState,
    menuExpanded: Boolean,
    onMenuExpandedChange: (Boolean) -> Unit,
    onReturnToHub: () -> Unit,
    onDisconnect: () -> Unit,
    onToggleOrientation: () -> Unit,
    onSetTouchMode: (TouchMode) -> Unit,
    onUpdateQuality: ((QualityPrefs) -> QualityPrefs) -> Unit,
    onRequestKeyframe: () -> Unit,
    onMouseMove: (Float, Float) -> Unit,
    onMouseButton: (Int, Boolean) -> Unit,
    onTouch: (Int, Int, Float, Float) -> Unit,
    onTouchpad: (Int, Int, Int, Int) -> Unit,
    onPen: (Int, Float, Float, Float, Int, Int, Int) -> Unit,
    onWheel: (Int, Boolean) -> Unit,
    onSendText: (String) -> Unit,
    onSendKey: (Int, Boolean) -> Unit,
    onTapKey: (Int) -> Unit,
    onTapChord: (Int, Int) -> Unit,
    onShowToast: (String) -> Unit,
    onSurfaceReady: (Surface) -> Unit,
    onSurfaceDestroyed: (Surface) -> Unit,
    initialInputBarOpen: Boolean = false,
) {
    val c = RcThemeColors.current
    val config = LocalConfiguration.current
    val density = LocalDensity.current
    val haptics = LocalHapticFeedback.current
    val focusManager = LocalFocusManager.current
    val imeBottomPx = WindowInsets.ime.getBottom(density)
    val imeTargetBottomPx = WindowInsets.imeAnimationTarget.getBottom(density)
    val imeVisible = imeBottomPx > 0
    // Floating keyboards take no bottom inset, so their visibility has to come from the
    // insets' visible state rather than their height.
    val imeShown = WindowInsets.isImeVisible
    val imeTargetVisible = imeTargetBottomPx > 0 || (imeShown && imeBottomPx == 0)

    val hasHardwareKeyboard = config.keyboard == Configuration.KEYBOARD_QWERTY &&
        config.hardKeyboardHidden == Configuration.HARDKEYBOARDHIDDEN_NO

    var lastCapsuleTouchMs by remember { mutableLongStateOf(System.currentTimeMillis()) }
    var capsuleDimmed by remember { mutableStateOf(false) }
    var inputBarOpen by remember { mutableStateOf(initialInputBarOpen) }
    var imeWanted by remember { mutableStateOf(initialInputBarOpen) }
    var pcKeyboardOpen by remember { mutableStateOf(false) }

    /** Capsule keyboard button / 3-finger tap: open the bar + phone keyboard, or close everything. */
    fun toggleKeyboard() {
        if (inputBarOpen) {
            inputBarOpen = false
            imeWanted = false
        } else {
            inputBarOpen = true
            imeWanted = !hasHardwareKeyboard
        }
    }
    var composingText by remember { mutableStateOf("") }
    var imeEverShownForBar by remember { mutableStateOf(false) }
    var sinkView by remember { mutableStateOf<RemoteImeSinkView?>(null) }
    var keyBarBottomPx by remember { mutableIntStateOf(0) }
    // Where the user last tapped in direct-touch mode (stream coords), so the picture can be
    // pushed up to keep that spot above the keyboard.
    var lastTapNormX by remember { mutableFloatStateOf(0.5f) }
    var lastTapNormY by remember { mutableFloatStateOf(0.5f) }

    // Sticky modifier state (Ctrl/Alt/Shift/Win)
    val modStates = remember { mutableStateMapOf<Int, ModState>() }
    val lastModTapMs = remember { HashMap<Int, Long>() }
    val currentOnSendKey by rememberUpdatedState(onSendKey)

    fun releaseArmedModifiers() {
        val toRelease = modStates.entries.filter { it.value == ModState.Armed }.map { it.key }
        for (scan in toRelease) {
            modStates[scan] = ModState.Off
            currentOnSendKey(scan, false)
        }
    }

    fun releaseAllModifiers() {
        val active = modStates.entries.filter { it.value != ModState.Off }.map { it.key }
        for (scan in active) {
            modStates[scan] = ModState.Off
            currentOnSendKey(scan, false)
        }
    }

    fun toggleStickyModifier(scan: Int) {
        val now = SystemClock.uptimeMillis()
        val prev = modStates[scan] ?: ModState.Off
        val prevTap = lastModTapMs[scan] ?: 0L
        lastModTapMs[scan] = now
        when (prev) {
            ModState.Off -> {
                modStates[scan] = ModState.Armed
                onSendKey(scan, true)
            }
            ModState.Armed -> {
                if (now - prevTap <= 380L) {
                    modStates[scan] = ModState.Locked
                } else {
                    modStates[scan] = ModState.Off
                    onSendKey(scan, false)
                }
            }
            ModState.Locked -> {
                modStates[scan] = ModState.Off
                onSendKey(scan, false)
            }
        }
    }

    DisposableEffect(Unit) {
        onDispose { releaseAllModifiers() }
    }

    val currentOnSendText by rememberUpdatedState(onSendText)
    val currentOnTapKey by rememberUpdatedState(onTapKey)
    val imeListener = remember {
        object : RemoteImeSinkView.Listener {
            override fun modifierActive(): Boolean = modStates.values.any { it != ModState.Off }

            override fun onComposing(text: String) {
                composingText = text
            }

            override fun onKey(scan: Int) {
                currentOnTapKey(scan)
                releaseArmedModifiers()
            }

            override fun onCommitText(text: String) {
                if (modifierActive()) {
                    // Ctrl/Alt/Win/Shift are held down on the PC: send real keys so shortcuts work.
                    for (ch in text) {
                        val scan = charToScan(ch)
                        if (scan != 0) currentOnTapKey(scan) else currentOnSendText(ch.toString())
                    }
                    releaseArmedModifiers()
                    return
                }
                // Newlines and tabs go as real Enter / Tab keys, everything else as text.
                val run = StringBuilder()
                for (ch in text) {
                    val key = when (ch) {
                        '\n' -> Scan.ENTER
                        '\t' -> Scan.TAB
                        else -> 0
                    }
                    if (key == 0) {
                        run.append(ch)
                    } else {
                        if (run.isNotEmpty()) currentOnSendText(run.toString())
                        run.clear()
                        currentOnTapKey(key)
                    }
                }
                if (run.isNotEmpty()) currentOnSendText(run.toString())
            }
        }
    }

    var videoContainerSize by remember { mutableStateOf(IntSize.Zero) }
    var zoomScale by remember { mutableFloatStateOf(1f) }
    var panX by remember { mutableFloatStateOf(0f) }
    var panY by remember { mutableFloatStateOf(0f) }
    var trackpadTouchActive by remember { mutableStateOf(false) }

    var cursorNormX by remember { mutableFloatStateOf(0.5f) }
    var cursorNormY by remember { mutableFloatStateOf(0.5f) }
    var isDraggingVisual by remember { mutableStateOf(false) }

    val scope = rememberCoroutineScope()
    var flingJob by remember { mutableStateOf<Job?>(null) }

    var tapPos by remember { mutableStateOf<Offset?>(null) }
    var tapIsRight by remember { mutableStateOf(false) }
    val rippleProgress = remember { Animatable(1f) }

    fun triggerRipple(pos: Offset, isRight: Boolean = false) {
        tapPos = pos
        tapIsRight = isRight
        scope.launch {
            rippleProgress.snapTo(0f)
            rippleProgress.animateTo(1f, tween(210, easing = FastOutSlowInEasing))
        }
    }

    // The "hold reached" buzz: a heavy click the hand clearly feels, unlike the light
    // long-press haptic some phones barely play.
    val context = LocalContext.current
    val vibrator = remember(context) {
        if (Build.VERSION.SDK_INT >= 31) {
            context.getSystemService(VibratorManager::class.java)?.defaultVibrator
        } else {
            @Suppress("DEPRECATION")
            context.getSystemService(Vibrator::class.java)
        }
    }
    fun holdBuzz() {
        val v = vibrator
        if (v == null || !v.hasVibrator()) {
            haptics.performHapticFeedback(HapticFeedbackType.LongPress)
            return
        }
        runCatching { v.vibrate(VibrationEffect.createPredefined(VibrationEffect.EFFECT_HEAVY_CLICK)) }
            .onFailure { haptics.performHapticFeedback(HapticFeedbackType.LongPress) }
    }

    // Set once the user touches the screen: from then on window updates from the PC no longer
    // re-frame the view or move the cursor back to the window's centre. Reset per window/monitor.
    // (A plain state, not keyed, so gesture handlers that outlive a window switch still see it.)
    var userAdjustedViewport by remember { mutableStateOf(false) }
    LaunchedEffect(state.activeWindow?.hwnd, state.currentMonitorIndex) {
        userAdjustedViewport = false
    }

    fun applyInitialWindowFocus(size: IntSize) {
        if (size.width <= 0 || size.height <= 0) return
        val win = state.activeWindow
        if (win == null) {
            if (!userAdjustedViewport) {
                zoomScale = 1f
                panX = 0f
                panY = 0f
            }
            return
        }
        if (userAdjustedViewport) return
        if (!win.hasBounds || win.monitor != state.currentMonitorIndex) {
            zoomScale = 1f
            panX = 0f
            panY = 0f
            return
        }
        // Use the target monitor's known pixel dimensions immediately so switching from a
        // portrait monitor to a landscape monitor (or vice versa) pre-frames the tapped window
        // BEFORE its first keyframe arrives — zero unzoomed full-desktop flash.
        val mon = state.monitors.getOrNull(win.monitor)
        val focusSw = if (mon != null && mon.width > 0 && mon.height > 0) mon.width else state.streamMeta.width
        val focusSh = if (mon != null && mon.width > 0 && mon.height > 0) mon.height else state.streamMeta.height
        val focus = computeWindowAutoFocus(
            rx = win.rx,
            ry = win.ry,
            rw = win.rw,
            rh = win.rh,
            cw = size.width.toFloat(),
            ch = size.height.toFloat(),
            sw = focusSw,
            sh = focusSh,
        )
        if (focus != null) {
            zoomScale = focus.zoomScale
            panX = focus.panX
            panY = focus.panY
            cursorNormX = (win.rx + win.rw * 0.5f).coerceIn(0f, 1f)
            cursorNormY = (win.ry + win.rh * 0.5f).coerceIn(0f, 1f)
        }
    }

    LaunchedEffect(
        state.activeWindow?.hwnd,
        state.activeWindow?.rx,
        state.activeWindow?.ry,
        state.activeWindow?.rw,
        state.activeWindow?.rh,
        state.currentMonitorIndex,
        state.monitors,
        state.streamMeta.width,
        state.streamMeta.height,
        videoContainerSize,
    ) {
        applyInitialWindowFocus(videoContainerSize)
    }

    LaunchedEffect(state.activeWindow?.hwnd, state.virtualDisplay.active) {
        val win = state.activeWindow
        if (win != null && win.hasBounds) {
            cursorNormX = (win.rx + win.rw * 0.5f).coerceIn(0f, 1f)
            cursorNormY = (win.ry + win.rh * 0.5f).coerceIn(0f, 1f)
        } else {
            cursorNormX = 0.5f
            cursorNormY = 0.5f
        }
    }

    // Three independent pieces: the shortcut bar (under the capsule), the phone keyboard
    // (bottom) and the PC keyboard (unfolds under the shortcut bar). Closing one never closes
    // another; the phone keyboard and the PC keyboard are never shown together.
    LaunchedEffect(inputBarOpen, imeWanted, pcKeyboardOpen, sinkView) {
        val sink = sinkView ?: return@LaunchedEffect
        if (inputBarOpen && imeWanted && !pcKeyboardOpen) {
            imeEverShownForBar = false
            focusManager.clearFocus()
            if (hasHardwareKeyboard) sink.requestFocus() else sink.showKeyboard()
        } else {
            sink.hideKeyboard()
        }
        if (!inputBarOpen) {
            pcKeyboardOpen = false
            releaseAllModifiers()
        }
    }

    // The phone keyboard was put away by the user (its own hide key or Back): only the phone
    // keyboard goes, the shortcut bar stays.
    LaunchedEffect(imeShown, imeTargetVisible, inputBarOpen, imeWanted, hasHardwareKeyboard) {
        if (!inputBarOpen || !imeWanted || hasHardwareKeyboard) return@LaunchedEffect
        if (imeShown || imeTargetVisible) {
            imeEverShownForBar = true
        }
        if (imeEverShownForBar && !imeTargetVisible) {
            imeWanted = false
        }
    }

    // The keyboard only ever opens from the capsule button or a 3-finger tap: the host's
    // "text field focused" reports are ignored, since editors like VS Code count as one
    // wherever you tap.

    // Back closes one layer at a time: the phone keyboard handles Back itself first,
    // then the PC keyboard folds up, then the shortcut bar closes.
    BackHandler(enabled = inputBarOpen) {
        if (pcKeyboardOpen) pcKeyboardOpen = false else inputBarOpen = false
    }

    LaunchedEffect(lastCapsuleTouchMs, menuExpanded) {
        if (menuExpanded) {
            capsuleDimmed = false
        } else {
            // Wake only for a fresh capsule tap; closing the menu by tapping outside keeps it away.
            if (System.currentTimeMillis() - lastCapsuleTouchMs < 400L) capsuleDimmed = false
            delay(2_400L)
            capsuleDimmed = true
        }
    }

    val capsuleAlpha by animateFloatAsState(
        targetValue = if (capsuleDimmed && !menuExpanded) 0.34f else 1f,
        animationSpec = tween(260),
        label = "capsuleAlpha",
    )
    // Idle capsule stays small; touching it (or having its menu / key bar open) grows it a bit.
    val capsuleGrow by animateFloatAsState(
        targetValue = if (!capsuleDimmed || menuExpanded || inputBarOpen) 1f else 0f,
        animationSpec = spring(dampingRatio = 0.78f, stiffness = Spring.StiffnessMediumLow),
        label = "capsuleGrow",
    )
    fun grow(small: Float, big: Float) = small + (big - small) * capsuleGrow

    // Move the picture so the spot being typed into stays between the key bar (under the
    // capsule at the top) and the phone keyboard (at the bottom).
    val liftMarginPx = with(density) { 20.dp.toPx() }
    val focusNormX = if (state.touchMode == TouchMode.Direct) lastTapNormX else cursorNormX
    val focusNormY = if (state.touchMode == TouchMode.Direct) lastTapNormY else cursorNormY
    val coveredPx = if (imeVisible) imeBottomPx else 0
    val topCoveredPx = if (inputBarOpen) keyBarBottomPx else 0
    val rawImeLiftY = remember(coveredPx, topCoveredPx, focusNormX, focusNormY, videoContainerSize, zoomScale, panX, panY) {
        if ((coveredPx <= 0 && topCoveredPx <= 0) || videoContainerSize.height <= 0) {
            0f
        } else {
            val ch = videoContainerSize.height.toFloat()
            val curScreen = mapStreamToScreen(
                nx = focusNormX,
                ny = focusNormY,
                cw = videoContainerSize.width.toFloat(),
                ch = ch,
                sw = state.streamMeta.width,
                sh = state.streamMeta.height,
                scale = zoomScale,
                px = panX,
                py = panY,
            )
            val visibleTop = topCoveredPx + liftMarginPx
            val visibleBottom = (ch - coveredPx - liftMarginPx).coerceAtLeast(visibleTop)
            when {
                curScreen == null -> 0f
                curScreen.y > visibleBottom -> -(curScreen.y - visibleBottom).coerceAtMost(coveredPx.toFloat())
                curScreen.y < visibleTop -> (visibleTop - curScreen.y).coerceAtMost(topCoveredPx.toFloat())
                else -> 0f
            }
        }
    }
    val imeLiftY by animateFloatAsState(
        targetValue = rawImeLiftY,
        animationSpec = spring(stiffness = Spring.StiffnessMediumLow),
        label = "imeLiftY",
    )

    val currentState by rememberUpdatedState(state)
    val currentMenuExpanded by rememberUpdatedState(menuExpanded)
    val currentOnMenuExpandedChange by rememberUpdatedState(onMenuExpandedChange)
    val currentOnMouseMove by rememberUpdatedState(onMouseMove)
    val currentOnMouseButton by rememberUpdatedState(onMouseButton)
    val currentOnTouch by rememberUpdatedState(onTouch)
    val currentOnTouchpad by rememberUpdatedState(onTouchpad)
    val currentOnPen by rememberUpdatedState(onPen)
    val currentOnWheel by rememberUpdatedState(onWheel)

    // IMPORTANT: Always compute panY + imeLiftY inside a function reading Compose MutableState/State
    // directly rather than capturing a stale local `val` into `pointerInput` closures!
    fun liveEffectivePanY(): Float = panY + imeLiftY

    val doubleTapMaxDistPx = with(density) { 44.dp.toPx() }
    val pinchSlopPx = with(density) { 18.dp.toPx() }
    val autoPanMarginPx = with(density) { 28.dp.toPx() }
    val edgeScrollWidthPx = with(density) { 26.dp.toPx() }

    fun liveStreamToScreen(nx: Float, ny: Float): Offset? = mapStreamToScreen(
        nx = nx,
        ny = ny,
        cw = videoContainerSize.width.toFloat(),
        ch = videoContainerSize.height.toFloat(),
        sw = currentState.streamMeta.width,
        sh = currentState.streamMeta.height,
        scale = zoomScale,
        px = panX,
        py = liveEffectivePanY(),
    )

    fun liveMapToStream(offset: Offset): Pair<Float, Float>? = mapScreenToStream(
        offset = offset,
        cw = videoContainerSize.width.toFloat(),
        ch = videoContainerSize.height.toFloat(),
        sw = currentState.streamMeta.width,
        sh = currentState.streamMeta.height,
        scale = zoomScale,
        px = panX,
        py = liveEffectivePanY(),
    )

    fun liveClampPan(px: Float, py: Float, scale: Float): Pair<Float, Float> = clampPanOffset(
        px = px,
        py = py,
        scale = scale,
        cw = videoContainerSize.width.toFloat(),
        ch = videoContainerSize.height.toFloat(),
        sw = currentState.streamMeta.width,
        sh = currentState.streamMeta.height,
    )

    fun liveAutoPanForCursor(nx: Float, ny: Float) {
        if (zoomScale <= 1.02f) return
        val pos = liveStreamToScreen(nx, ny) ?: return
        val cw = videoContainerSize.width.toFloat()
        val ch = videoContainerSize.height.toFloat()
        var nextPanX = panX
        var nextPanY = panY
        if (pos.x < autoPanMarginPx) nextPanX += (autoPanMarginPx - pos.x)
        else if (pos.x > cw - autoPanMarginPx) nextPanX -= (pos.x - (cw - autoPanMarginPx))
        if (pos.y < autoPanMarginPx) nextPanY += (autoPanMarginPx - pos.y)
        else if (pos.y > ch - autoPanMarginPx) nextPanY -= (pos.y - (ch - autoPanMarginPx))
        if (nextPanX != panX || nextPanY != panY) {
            val (cx, cy) = liveClampPan(nextPanX, nextPanY, zoomScale)
            panX = cx
            panY = cy
        }
    }

    fun applyVentoTrackpadDelta(dx: Float, dy: Float, dtMs: Float, sendNow: (Float, Float) -> Unit) {
        val st = currentState
        val sw = st.streamMeta.width.coerceAtLeast(1).toFloat()
        val sh = st.streamMeta.height.coerceAtLeast(1).toFloat()
        val cw = videoContainerSize.width.toFloat().coerceAtLeast(1f)
        val ch = videoContainerSize.height.toFloat().coerceAtLeast(1f)
        val (vidW, vidH) = computeVideoRect(cw, ch, sw.toInt(), sh.toInt()) ?: (cw to ch)
        val effW = (vidW * zoomScale).coerceAtLeast(1f)
        val effH = (vidH * zoomScale).coerceAtLeast(1f)

        val distPx = hypot(dx, dy)
        val speedDpPerMs = (distPx / density.density) / dtMs.coerceAtLeast(1f)
        // Vento-inspired nonlinear micro-precision curve:
        // slow precision (0.45x) -> comfortable mid (1.15x) -> fast flick (2.45x)
        var curveGain = when {
            speedDpPerMs < 0.16f -> 0.45f + (speedDpPerMs / 0.16f) * 0.35f
            speedDpPerMs < 0.55f -> 0.80f + ((speedDpPerMs - 0.16f) / 0.39f) * 0.65f
            else -> (1.45f + (speedDpPerMs - 0.55f) * 1.15f).coerceAtMost(2.45f)
        }
        // Hover Assist: gentle magnetic damping over interactive links, resize handles & text fields
        if (st.cursorMetaKind != 0 && speedDpPerMs < 0.28f) {
            curveGain *= 0.78f
        }

        // Isotropic physical scaling: identical thumb travel moves identical Windows pixels on X & Y
        val winPxPerScreenPx = minOf(sw / effW, sh / effH)
        val step = 1.15f * curveGain * winPxPerScreenPx
        val nextX = (cursorNormX + dx * step / sw).coerceIn(0f, 1f)
        val nextY = (cursorNormY + dy * step / sh).coerceIn(0f, 1f)
        cursorNormX = nextX
        cursorNormY = nextY
        liveAutoPanForCursor(nextX, nextY)
        sendNow(nextX, nextY)
    }

    fun applyLocalPinchPan(
        centroid: Offset,
        prevCentroid: Offset?,
        span: Float,
        prevSpan: Float,
        recenterCursor: Boolean = true,
    ) {
        userAdjustedViewport = true
        val ratio = if (prevSpan > 1f) (span / prevSpan).coerceIn(0.75f, 1.35f) else 1f
        val newScale = (zoomScale * ratio).coerceIn(1f, 6.0f)
        val cw = videoContainerSize.width.toFloat().coerceAtLeast(1f)
        val ch = videoContainerSize.height.toFloat().coerceAtLeast(1f)
        val dx = if (prevCentroid != null) centroid.x - prevCentroid.x else 0f
        val dy = if (prevCentroid != null) centroid.y - prevCentroid.y else 0f
        val scaleFactor = if (zoomScale > 0f) newScale / zoomScale else 1f
        val rawPanX = (panX - (centroid.x - cw * 0.5f)) * scaleFactor + (centroid.x - cw * 0.5f) + dx
        val rawPanY = (panY - (centroid.y - ch * 0.5f)) * scaleFactor + (centroid.y - ch * 0.5f) + dy
        if (newScale <= 1.02f) {
            zoomScale = 1f
            panX = 0f
            panY = 0f
        } else {
            val (cxPan, cyPan) = liveClampPan(rawPanX, rawPanY, newScale)
            zoomScale = newScale
            panX = cxPan
            panY = cyPan
            if (!recenterCursor) return
            // If pinch/pan moved the old cursor off-screen, bring it into the visible center
            // so the next trackpad touch never snaps the viewport back to the previous corner.
            val curPos = liveStreamToScreen(cursorNormX, cursorNormY)
            if (curPos == null || curPos.x < 0f || curPos.x > cw || curPos.y < 0f || curPos.y > ch) {
                val centerNorm = liveMapToStream(Offset(cw * 0.5f, ch * 0.5f))
                if (centerNorm != null) {
                    cursorNormX = centerNorm.first
                    cursorNormY = centerNorm.second
                    currentOnMouseMove(centerNorm.first, centerNorm.second)
                }
            }
        }
    }

    /**
     * While selecting with a finger near the screen edge, slides the zoomed picture that way so
     * the selection can go past what is visible. True when the picture moved.
     */
    fun edgeAutoPan(pos: Offset): Boolean {
        if (zoomScale <= 1.02f) return false
        val cw = videoContainerSize.width.toFloat()
        val ch = videoContainerSize.height.toFloat()
        if (cw <= 0f || ch <= 0f) return false
        val margin = with(density) { 48.dp.toPx() }
        val maxStep = with(density) { 14.dp.toPx() }
        fun step(v: Float, size: Float): Float = when {
            v < margin -> maxStep * (1f - v.coerceAtLeast(0f) / margin)
            v > size - margin -> -maxStep * (1f - (size - v).coerceAtLeast(0f) / margin)
            else -> 0f
        }
        val dx = step(pos.x, cw)
        val dy = step(pos.y, ch)
        if (dx == 0f && dy == 0f) return false
        val (nx, ny) = liveClampPan(panX + dx, panY + dy, zoomScale)
        if (nx == panX && ny == panY) return false
        userAdjustedViewport = true
        panX = nx
        panY = ny
        return true
    }

    fun Modifier.trackpadGestureInput(
        surfaceWidthPxProvider: () -> Float,
        allowEdgeScroll: Boolean,
        allowDirectMultiTouch: Boolean,
    ): Modifier = this.pointerInput(allowEdgeScroll, allowDirectMultiTouch) {
        val touchSlop = viewConfiguration.touchSlop
        val longPressTimeoutMs = 380L
        val doubleTapWindowMs = 260L
        var lastTapUpMs = 0L
        var lastTapUpPos = Offset.Zero

        awaitEachGesture {
            val firstDown = awaitFirstDown(requireUnconsumed = false)
            firstDown.consume()
            // Touching the picture puts the capsule away at once (small + faded).
            capsuleDimmed = true
            userAdjustedViewport = true
            flingJob?.cancel()
            flingJob = null

            if (currentMenuExpanded) {
                currentOnMenuExpandedChange(false)
                do {
                    val ev = awaitPointerEvent()
                    ev.changes.forEach { it.consume() }
                } while (ev.changes.any { it.pressed })
                return@awaitEachGesture
            }

            // 1. Hardware Stylus / Pen path (PT_PEN with pressure in BOTH modes)
            if (allowDirectMultiTouch && (firstDown.type == PointerType.Stylus || firstDown.type == PointerType.Eraser)) {
                val flags = if (firstDown.type == PointerType.Eraser) 2 else 0
                liveMapToStream(firstDown.position)?.let { (nx, ny) ->
                    currentOnPen(1, nx, ny, firstDown.pressure, 0, 0, flags)
                }
                var lastNorm = liveMapToStream(firstDown.position) ?: (0.5f to 0.5f)
                try {
                    while (true) {
                        val ev = awaitPointerEvent()
                        ev.changes.forEach { it.consume() }
                        val pen = ev.changes.firstOrNull { it.id == firstDown.id } ?: ev.changes.firstOrNull()
                        if (pen == null || !pen.pressed) {
                            val endNorm = pen?.let { liveMapToStream(it.position) } ?: lastNorm
                            currentOnPen(3, endNorm.first, endNorm.second, 0f, 0, 0, flags)
                            break
                        }
                        liveMapToStream(pen.position)?.let { (nx, ny) ->
                            lastNorm = nx to ny
                            currentOnPen(2, nx, ny, pen.pressure, 0, 0, flags)
                        }
                    }
                } finally {
                    // Nothing lingering
                }
                return@awaitEachGesture
            }

            // 2. Direct Touch Mode — 100% genuine Windows native multi-touch (PT_TOUCH)
            //    NEVER converts touches into mouse clicks or jumps the mouse cursor!
            //    - Virtual Phone Display: pure 10-finger Windows native multi-touch passthrough
            //      (native tap, inertial scroll, long-press right-click, 2-finger pinch/pan/rotate in apps).
            //    - Physical Desktop: 1-finger goes straight to Windows native PT_TOUCH at exact zoomed
            //      coordinates; 2-finger cancels any pending 1st touch cleanly (PT_TOUCH CANCEL) and
            //      performs smooth local GPU pinch-zoom & 2-finger pan.
            val useDirect = allowDirectMultiTouch && currentState.touchMode == TouchMode.Direct

            // Direct mode on a physical desktop: the phone screen is a window onto the PC.
            //   tap = touch tap where the finger went down
            //   1-finger drag = move the zoomed picture (any direction)
            //   hold still, buzz, then drag = left-button drag (select text, drag files/windows)
            //   hold still, buzz, lift = right click
            //   2-finger drag = scroll the PC content (precision touchpad, both axes)
            //   2-finger pinch = zoom the picture; 3-finger tap = keyboard
            // Nothing is sent until the gesture is known, so a pan or a hold never leaves a
            // half-made touch on the PC.
            if (useDirect && !currentState.virtualDisplay.active) {
                val startPos = firstDown.position
                val downMs = SystemClock.uptimeMillis()
                val holdMoveSlop = touchSlop * 0.35f
                var mode = DirectGesture.Pending
                var prevPos = startPos
                var lastDragPos = startPos
                var edgeJob: Job? = null
                var multiStartCentroid = Offset.Zero
                var multiStartSpan = 0f
                var prevCentroid: Offset? = null
                var prevSpan = 0f
                var scrollAxis = 0
                var threeDownMs = 0L

                fun mouseTo(pos: Offset) {
                    liveMapToStream(pos)?.let { (x, y) -> currentOnMouseMove(x, y) }
                }

                try {
                    while (true) {
                        val event = if (mode == DirectGesture.Pending) {
                            val remaining = (longPressTimeoutMs - (SystemClock.uptimeMillis() - downMs)).coerceAtLeast(1L)
                            withTimeoutOrNull(remaining) { awaitPointerEvent() }
                        } else {
                            awaitPointerEvent()
                        }
                        if (event == null) {
                            mode = DirectGesture.Hold
                            holdBuzz()
                            triggerRipple(startPos, isRight = true)
                            continue
                        }
                        event.changes.forEach { it.consume() }
                        val pressed = event.changes.filter { it.pressed }

                        if (pressed.isEmpty()) {
                            val upMs = SystemClock.uptimeMillis()
                            when (mode) {
                                DirectGesture.Pending -> {
                                    liveMapToStream(startPos)?.let { (tx, ty) ->
                                        lastTapNormX = tx
                                        lastTapNormY = ty
                                        currentOnTouch(0, 0, tx, ty)
                                        scope.launch {
                                            delay(25L)
                                            currentOnTouch(0, 2, tx, ty)
                                        }
                                    }
                                    releaseArmedModifiers()
                                }
                                DirectGesture.Hold -> {
                                    mouseTo(startPos)
                                    currentOnMouseButton(1, true)
                                    currentOnMouseButton(1, false)
                                    releaseArmedModifiers()
                                }
                                DirectGesture.HoldDrag -> {
                                    mouseTo(lastDragPos)
                                    currentOnMouseButton(0, false)
                                    releaseArmedModifiers()
                                }
                                DirectGesture.TwoScroll -> currentOnTouchpad(2, 0, 0, 1000)
                                DirectGesture.Three -> if (upMs - threeDownMs <= 300L) {
                                    haptics.performHapticFeedback(HapticFeedbackType.TextHandleMove)
                                    toggleKeyboard()
                                }
                                else -> {}
                            }
                            mode = DirectGesture.Done
                            break
                        }

                        // Hold modes belong to the first finger; extra fingers are ignored.
                        if (mode == DirectGesture.Hold || mode == DirectGesture.HoldDrag) {
                            val f = event.changes.firstOrNull { it.id == firstDown.id } ?: continue
                            if (!f.pressed) continue
                            if (mode == DirectGesture.Hold && (f.position - startPos).getDistance() > holdMoveSlop) {
                                mouseTo(startPos)
                                currentOnMouseButton(0, true)
                                mode = DirectGesture.HoldDrag
                                edgeJob = scope.launch {
                                    while (true) {
                                        delay(16L)
                                        if (edgeAutoPan(lastDragPos)) mouseTo(lastDragPos)
                                    }
                                }
                            }
                            if (mode == DirectGesture.HoldDrag && f.position != lastDragPos) {
                                lastDragPos = f.position
                                mouseTo(f.position)
                            }
                            continue
                        }

                        if (pressed.size >= 3) {
                            if (mode == DirectGesture.TwoScroll) currentOnTouchpad(2, 0, 0, 1000)
                            if (mode != DirectGesture.Three && mode != DirectGesture.Done) {
                                mode = DirectGesture.Three
                                threeDownMs = SystemClock.uptimeMillis()
                            }
                            continue
                        }

                        if (pressed.size == 2) {
                            val centroid = Offset(
                                (pressed[0].position.x + pressed[1].position.x) * 0.5f,
                                (pressed[0].position.y + pressed[1].position.y) * 0.5f,
                            )
                            val span = (pressed[0].position - pressed[1].position).getDistance()
                            when (mode) {
                                DirectGesture.Pending, DirectGesture.Pan -> {
                                    mode = DirectGesture.TwoPending
                                    multiStartCentroid = centroid
                                    multiStartSpan = span
                                    prevCentroid = centroid
                                    prevSpan = span
                                }
                                DirectGesture.TwoPending -> {
                                    val spanDiff = abs(span - multiStartSpan)
                                    val moveDist = (centroid - multiStartCentroid).getDistance()
                                    if (spanDiff > pinchSlopPx && spanDiff > moveDist * 0.7f) {
                                        mode = DirectGesture.TwoPinch
                                        prevCentroid = centroid
                                        prevSpan = span
                                    } else if (moveDist > touchSlop) {
                                        mode = DirectGesture.TwoScroll
                                        val adx = abs(centroid.x - multiStartCentroid.x)
                                        val ady = abs(centroid.y - multiStartCentroid.y)
                                        scrollAxis = when {
                                            ady > adx * 1.6f -> 1
                                            adx > ady * 1.6f -> 2
                                            else -> 0
                                        }
                                        // Touchpad scrolling goes to the window under the cursor.
                                        mouseTo(multiStartCentroid)
                                        currentOnTouchpad(0, 0, 0, 1000)
                                        prevCentroid = centroid
                                    }
                                }
                                DirectGesture.TwoScroll -> {
                                    val prev = prevCentroid ?: centroid
                                    var dxDp10 = ((centroid.x - prev.x) / density.density * 10f).roundToInt()
                                    var dyDp10 = ((centroid.y - prev.y) / density.density * 10f).roundToInt()
                                    if (scrollAxis == 1) dxDp10 = 0 else if (scrollAxis == 2) dyDp10 = 0
                                    if (dxDp10 != 0 || dyDp10 != 0) {
                                        currentOnTouchpad(1, dxDp10, dyDp10, 1000)
                                        prevCentroid = centroid
                                    }
                                }
                                DirectGesture.TwoPinch -> {
                                    applyLocalPinchPan(centroid, prevCentroid, span, prevSpan, recenterCursor = false)
                                    prevCentroid = centroid
                                    prevSpan = span
                                }
                                else -> {}
                            }
                            continue
                        }

                        // One finger down.
                        val p = pressed[0]
                        when (mode) {
                            DirectGesture.Pending -> if ((p.position - startPos).getDistance() > touchSlop) {
                                mode = DirectGesture.Pan
                                prevPos = p.position
                            }
                            DirectGesture.Pan -> {
                                applyLocalPinchPan(p.position, prevPos, 1f, 1f, recenterCursor = false)
                                prevPos = p.position
                            }
                            DirectGesture.TwoScroll -> {
                                // Lifting one of two fingers ends the scroll; the other finger does nothing.
                                currentOnTouchpad(2, 0, 0, 1000)
                                mode = DirectGesture.Done
                            }
                            DirectGesture.TwoPending, DirectGesture.TwoPinch -> mode = DirectGesture.Done
                            else -> {}
                        }
                    }
                } finally {
                    edgeJob?.cancel()
                    // Cancelled mid-gesture (e.g. the screen went away): never leave a button or a scroll held.
                    if (mode == DirectGesture.HoldDrag) currentOnMouseButton(0, false)
                    if (mode == DirectGesture.TwoScroll) currentOnTouchpad(2, 0, 0, 1000)
                }
                return@awaitEachGesture
            }

            if (useDirect) {
                val isVirtualDisplay = currentState.virtualDisplay.active
                val activeSlots = HashMap<PointerId, Int>()
                val lastNormBySlot = HashMap<Int, Pair<Float, Float>>()
                val usedSlots = BooleanArray(10)

                fun allocSlot(pid: PointerId): Int? {
                    activeSlots[pid]?.let { return it }
                    for (i in 0 until 10) {
                        if (!usedSlots[i]) {
                            usedSlots[i] = true
                            activeSlots[pid] = i
                            return i
                        }
                    }
                    return null
                }

                fun cancelAllNativeTouches() {
                    if (activeSlots.isEmpty()) return
                    val entries = activeSlots.entries.toList()
                    for ((pid, slot) in entries) {
                        val (lx, ly) = lastNormBySlot[slot] ?: (0.5f to 0.5f)
                        currentOnTouch(slot, 3, lx, ly) // POINTER_FLAG_UP | POINTER_FLAG_CANCELED
                        usedSlots[slot] = false
                        lastNormBySlot.remove(slot)
                        activeSlots.remove(pid)
                    }
                }

                val startPos = firstDown.position
                val downMs = SystemClock.uptimeMillis()
                var maxSingleTravel = 0f
                var sawMultiFinger = false
                var localPinchActive = false
                var threeFingerDownMs = 0L
                var threeFingerTapEligible = false
                var prevCentroid: Offset? = null
                var prevPinchSpan = 0f

                // Send immediate 1st finger PT_TOUCH DOWN with zero latency
                liveMapToStream(firstDown.position)?.let { (nx, ny) ->
                    val slot = allocSlot(firstDown.id)
                    if (slot != null) {
                        lastNormBySlot[slot] = nx to ny
                        currentOnTouch(slot, 0, nx, ny)
                    }
                }

                try {
                    while (true) {
                        val event = awaitPointerEvent()
                        event.changes.forEach { it.consume() }
                        val pressed = event.changes.filter { it.pressed }

                        // All fingers lifted: finalize any active native touches or 3-finger tap
                        if (pressed.isEmpty()) {
                            val upMs = SystemClock.uptimeMillis()
                            if (threeFingerTapEligible && upMs - threeFingerDownMs <= 280L) {
                                haptics.performHapticFeedback(HapticFeedbackType.TextHandleMove)
                                toggleKeyboard()
                            }
                            if (!localPinchActive) {
                                // A tap lifts where it went down: finger jitter must not turn it into a pan.
                                val stillTap = !sawMultiFinger && maxSingleTravel <= touchSlop
                                for (change in event.changes) {
                                    val slot = activeSlots.remove(change.id) ?: continue
                                    val endNorm = (if (stillTap) lastNormBySlot[slot] else null)
                                        ?: liveMapToStream(change.position)
                                        ?: lastNormBySlot[slot]
                                        ?: (0.5f to 0.5f)
                                    currentOnTouch(slot, 2, endNorm.first, endNorm.second)
                                    usedSlots[slot] = false
                                    lastNormBySlot.remove(slot)
                                }
                                cancelAllNativeTouches()
                                if (!sawMultiFinger && maxSingleTravel <= touchSlop && (upMs - downMs) <= 350L) {
                                    liveMapToStream(startPos)?.let { (tx, ty) ->
                                        lastTapNormX = tx
                                        lastTapNormY = ty
                                    }
                                    releaseArmedModifiers()
                                }
                            }
                            break
                        }

                        // Physical desktop: 2+ fingers switch cleanly to local GPU pinch-zoom & pan
                        // (3-finger quick tap toggles keyboard)
                        if (!isVirtualDisplay && pressed.size >= 2) {
                            sawMultiFinger = true
                            if (!localPinchActive) {
                                cancelAllNativeTouches()
                                localPinchActive = true
                            }
                            if (pressed.size >= 3) {
                                if (threeFingerDownMs == 0L) {
                                    threeFingerDownMs = SystemClock.uptimeMillis()
                                    threeFingerTapEligible = true
                                }
                                prevCentroid = null
                            } else {
                                threeFingerTapEligible = false
                                val centroid = Offset(
                                    (pressed[0].position.x + pressed[1].position.x) * 0.5f,
                                    (pressed[0].position.y + pressed[1].position.y) * 0.5f,
                                )
                                val span = (pressed[0].position - pressed[1].position).getDistance()
                                if (prevCentroid != null) {
                                    applyLocalPinchPan(centroid, prevCentroid, span, prevPinchSpan)
                                }
                                prevCentroid = centroid
                                prevPinchSpan = span
                            }
                            continue
                        }

                        if (localPinchActive) {
                            // Once a gesture entered local pinch-zoom on physical desktop, ignore the
                            // trailing single finger lift so lifting one finger first never triggers a tap.
                            prevCentroid = null
                            continue
                        }

                        // Pure native Windows PT_TOUCH forwarding (1-finger on physical desktop,
                        // or 1..10 simultaneous fingers on Virtual Phone Display)
                        if (pressed.size >= 2) {
                            sawMultiFinger = true
                        } else if (!sawMultiFinger) {
                            val dist = (pressed[0].position - startPos).getDistance()
                            if (dist > maxSingleTravel) maxSingleTravel = dist
                        }

                        for (change in event.changes) {
                            val wasActive = activeSlots.containsKey(change.id)
                            if (change.pressed && !wasActive) {
                                val slot = allocSlot(change.id) ?: continue
                                val norm = liveMapToStream(change.position) ?: continue
                                lastNormBySlot[slot] = norm
                                currentOnTouch(slot, 0, norm.first, norm.second)
                            } else if (change.pressed && wasActive) {
                                // One finger still inside the touch slop: hold the contact where it
                                // went down so the PC sees a clean tap, not a tiny drag.
                                if (!sawMultiFinger && maxSingleTravel <= touchSlop) continue
                                val slot = activeSlots[change.id] ?: continue
                                val norm = liveMapToStream(change.position) ?: continue
                                val prev = lastNormBySlot[slot]
                                if (prev == null || abs(norm.first - prev.first) > 1e-5f || abs(norm.second - prev.second) > 1e-5f) {
                                    lastNormBySlot[slot] = norm
                                    currentOnTouch(slot, 1, norm.first, norm.second)
                                }
                            } else if (!change.pressed && wasActive) {
                                val slot = activeSlots.remove(change.id) ?: continue
                                val endNorm = liveMapToStream(change.position)
                                    ?: lastNormBySlot[slot]
                                    ?: (0.5f to 0.5f)
                                currentOnTouch(slot, 2, endNorm.first, endNorm.second)
                                usedSlots[slot] = false
                                lastNormBySlot.remove(slot)
                            }
                        }
                    }
                } finally {
                    cancelAllNativeTouches()
                }
                return@awaitEachGesture
            }

            // 3. Precision Trackpad Mode (Vento curve + Hover Assist + Win11 PT_TOUCHPAD scroll/pinch)
            val startPos = firstDown.position
            val downTime = SystemClock.uptimeMillis()
            val padW = surfaceWidthPxProvider().coerceAtLeast(1f)
            val startedOnRightEdge = allowEdgeScroll && startPos.x >= padW - edgeScrollWidthPx
            val isDoubleTapCandidate = !startedOnRightEdge &&
                (downTime - lastTapUpMs) <= doubleTapWindowMs &&
                (startPos - lastTapUpPos).getDistance() <= doubleTapMaxDistPx

            var mode = PadGesture.Pending
            var prevSinglePos = startPos
            var prevMoveMs = downTime
            var lastMouseSentMs = 0L
            var leftButtonDown = false
            var ptpScrollActive = false
            var ptpPinchActive = false

            var velPxPerMsX = 0f
            var velPxPerMsY = 0f

            var multiDownMs = 0L
            var multiStartCentroid = Offset.Zero
            var multiStartSpan = 0f
            var prevCentroid: Offset? = null
            var prevPinchSpan = 0f
            var scrollAxis = 0 // 0 free/undecided, 1 vertical, 2 horizontal

            fun sendMouseMoveThrottled(nx: Float, ny: Float, force: Boolean = false) {
                val now = SystemClock.uptimeMillis()
                if (force || now - lastMouseSentMs >= 7L) {
                    currentOnMouseMove(nx, ny)
                    lastMouseSentMs = now
                }
            }

            fun startCursorFling(upMs: Long) {
                if (upMs - prevMoveMs > 60L) return
                val speedDpPerMs = hypot(velPxPerMsX, velPxPerMsY) / density.density
                if (speedDpPerMs < 0.72f) return
                var vx = velPxPerMsX * 12f
                var vy = velPxPerMsY * 12f
                flingJob = scope.launch {
                    repeat(12) {
                        delay(12L)
                        if (hypot(vx, vy) < 1.5f) return@launch
                        applyVentoTrackpadDelta(vx, vy, 12f) { nx, ny ->
                            currentOnMouseMove(nx, ny)
                        }
                        vx *= 0.80f
                        vy *= 0.80f
                    }
                }
            }

            fun endPtpIfActive() {
                if (ptpScrollActive) {
                    currentOnTouchpad(2, 0, 0, 1000)
                    ptpScrollActive = false
                }
                if (ptpPinchActive) {
                    currentOnTouchpad(5, 0, 0, 1000)
                    ptpPinchActive = false
                }
            }

            try {
                trackpadTouchActive = true
                while (true) {
                    val event = when (mode) {
                        PadGesture.Pending -> {
                            val elapsed = SystemClock.uptimeMillis() - downTime
                            val remaining = (longPressTimeoutMs - elapsed).coerceAtLeast(1L)
                            withTimeoutOrNull(remaining) { awaitPointerEvent() }
                        }
                        else -> awaitPointerEvent()
                    }

                    if (event == null) {
                        mode = PadGesture.LongPressed
                        haptics.performHapticFeedback(HapticFeedbackType.LongPress)
                        val rPos = liveStreamToScreen(cursorNormX, cursorNormY) ?: startPos
                        triggerRipple(rPos, isRight = true)
                        continue
                    }

                    val pressed = event.changes.filter { it.pressed }
                    event.changes.forEach { it.consume() }

                    if (pressed.isEmpty()) {
                        val upMs = SystemClock.uptimeMillis()
                        when (mode) {
                            PadGesture.Pending -> {
                                val rPos = liveStreamToScreen(cursorNormX, cursorNormY) ?: startPos
                                triggerRipple(rPos, isRight = false)
                                currentOnMouseMove(cursorNormX, cursorNormY)
                                currentOnMouseButton(0, true)
                                currentOnMouseButton(0, false)
                                releaseArmedModifiers()
                                lastTapUpMs = upMs
                                lastTapUpPos = startPos
                            }
                            PadGesture.TrackpadMove -> {
                                sendMouseMoveThrottled(cursorNormX, cursorNormY, force = true)
                                startCursorFling(upMs)
                                lastTapUpMs = 0L
                            }
                            PadGesture.EdgeScroll, PadGesture.TwoScroll -> {
                                endPtpIfActive()
                                lastTapUpMs = 0L
                            }
                            PadGesture.TwoPinchRemote -> {
                                endPtpIfActive()
                                lastTapUpMs = 0L
                            }
                            PadGesture.LongPressed -> {
                                currentOnMouseMove(cursorNormX, cursorNormY)
                                currentOnMouseButton(1, true)
                                currentOnMouseButton(1, false)
                                releaseArmedModifiers()
                                lastTapUpMs = 0L
                            }
                            PadGesture.Dragging, PadGesture.LongPressDragging -> {
                                if (leftButtonDown) {
                                    sendMouseMoveThrottled(cursorNormX, cursorNormY, force = true)
                                    currentOnMouseButton(0, false)
                                    leftButtonDown = false
                                    isDraggingVisual = false
                                    releaseArmedModifiers()
                                }
                                lastTapUpMs = 0L
                            }
                            PadGesture.TwoPending -> {
                                if (upMs - multiDownMs <= 270L) {
                                    haptics.performHapticFeedback(HapticFeedbackType.LongPress)
                                    val rPos = liveStreamToScreen(cursorNormX, cursorNormY) ?: multiStartCentroid
                                    triggerRipple(rPos, isRight = true)
                                    currentOnMouseMove(cursorNormX, cursorNormY)
                                    currentOnMouseButton(1, true)
                                    currentOnMouseButton(1, false)
                                    releaseArmedModifiers()
                                }
                                lastTapUpMs = 0L
                            }
                            PadGesture.ThreePending -> {
                                if (upMs - multiDownMs <= 280L) {
                                    haptics.performHapticFeedback(HapticFeedbackType.TextHandleMove)
                                    toggleKeyboard()
                                }
                                lastTapUpMs = 0L
                            }
                            PadGesture.TwoPinchLocal, PadGesture.ThreePanZoomLocal, PadGesture.Done -> {
                                endPtpIfActive()
                                lastTapUpMs = 0L
                            }
                        }
                        break
                    }

                    // 3+ fingers: 3-finger tap = keyboard; 3-finger drag = local canvas pan/zoom
                    if (pressed.size >= 3) {
                        if (leftButtonDown) {
                            currentOnMouseButton(0, false)
                            leftButtonDown = false
                            isDraggingVisual = false
                        }
                        endPtpIfActive()
                        val nowMs = SystemClock.uptimeMillis()
                        val cx = pressed.sumOf { it.position.x.toDouble() }.toFloat() / pressed.size
                        val cy = pressed.sumOf { it.position.y.toDouble() }.toFloat() / pressed.size
                        val centroid = Offset(cx, cy)
                        val span = (pressed[0].position - pressed[2].position).getDistance()

                        if (mode != PadGesture.ThreePending && mode != PadGesture.ThreePanZoomLocal) {
                            mode = PadGesture.ThreePending
                            multiDownMs = nowMs
                            multiStartCentroid = centroid
                            multiStartSpan = span
                            prevCentroid = centroid
                            prevPinchSpan = span
                        } else if (mode == PadGesture.ThreePending) {
                            if ((centroid - multiStartCentroid).getDistance() > touchSlop * 1.1f ||
                                abs(span - multiStartSpan) > pinchSlopPx
                            ) {
                                mode = PadGesture.ThreePanZoomLocal
                                prevCentroid = centroid
                                prevPinchSpan = span
                            }
                        } else if (!currentState.virtualDisplay.active) {
                            applyLocalPinchPan(centroid, prevCentroid, span, prevPinchSpan)
                            prevCentroid = centroid
                            prevPinchSpan = span
                        }
                        continue
                    }

                    // 2 fingers: parallel slide ALWAYS scrolls (PT_TOUCHPAD); pinch zooms (remote on VD, local on physical)
                    if (pressed.size == 2) {
                        if (leftButtonDown) {
                            currentOnMouseButton(0, false)
                            leftButtonDown = false
                            isDraggingVisual = false
                        }
                        val nowMs = SystemClock.uptimeMillis()
                        val centroid = Offset(
                            (pressed[0].position.x + pressed[1].position.x) * 0.5f,
                            (pressed[0].position.y + pressed[1].position.y) * 0.5f,
                        )
                        val span = (pressed[0].position - pressed[1].position).getDistance()

                        when (mode) {
                            PadGesture.Pending,
                            PadGesture.EdgeScroll,
                            PadGesture.TrackpadMove,
                            PadGesture.LongPressed,
                            PadGesture.Dragging,
                            PadGesture.LongPressDragging -> {
                                endPtpIfActive()
                                mode = PadGesture.TwoPending
                                multiDownMs = nowMs
                                multiStartCentroid = centroid
                                multiStartSpan = span
                                prevCentroid = centroid
                                prevPinchSpan = span
                                scrollAxis = 0
                            }
                            PadGesture.TwoPending -> {
                                val spanDiff = abs(span - multiStartSpan)
                                val moveDist = (centroid - multiStartCentroid).getDistance()
                                if (zoomScale > 1.02f && (moveDist > touchSlop || spanDiff > pinchSlopPx)) {
                                    // When zoomed in, 2-finger drag/pinch directly pans & zooms the viewport
                                    mode = PadGesture.TwoPinchLocal
                                    prevCentroid = centroid
                                    prevPinchSpan = span
                                } else if (spanDiff > pinchSlopPx && spanDiff > moveDist * 0.7f) {
                                    if (currentState.virtualDisplay.active) {
                                        mode = PadGesture.TwoPinchRemote
                                        currentOnTouchpad(3, 0, 0, 1000)
                                        ptpPinchActive = true
                                    } else {
                                        mode = PadGesture.TwoPinchLocal
                                    }
                                    prevCentroid = centroid
                                    prevPinchSpan = span
                                } else if (moveDist > touchSlop * 1.1f) {
                                    // At 1x zoom, 2-finger parallel slide scrolls smoothly via Windows PTP
                                    mode = PadGesture.TwoScroll
                                    val adx = abs(centroid.x - multiStartCentroid.x)
                                    val ady = abs(centroid.y - multiStartCentroid.y)
                                    scrollAxis = when {
                                        ady > adx * 1.6f -> 1
                                        adx > ady * 1.6f -> 2
                                        else -> 0
                                    }
                                    currentOnTouchpad(0, 0, 0, 1000)
                                    ptpScrollActive = true
                                    prevCentroid = centroid
                                }
                            }
                            PadGesture.TwoScroll -> {
                                val prev = prevCentroid ?: centroid
                                var dxDp10 = ((centroid.x - prev.x) / density.density * 10f).roundToInt()
                                var dyDp10 = ((centroid.y - prev.y) / density.density * 10f).roundToInt()
                                if (scrollAxis == 1) dxDp10 = 0
                                else if (scrollAxis == 2) dyDp10 = 0
                                if (dxDp10 != 0 || dyDp10 != 0) {
                                    currentOnTouchpad(1, dxDp10, dyDp10, 1000)
                                    prevCentroid = centroid
                                }
                            }
                            PadGesture.TwoPinchRemote -> {
                                val ratio = if (multiStartSpan > 1f) {
                                    (span / multiStartSpan).coerceIn(0.25f, 4.0f)
                                } else {
                                    1f
                                }
                                currentOnTouchpad(4, 0, 0, (ratio * 1000f).roundToInt())
                                prevCentroid = centroid
                                prevPinchSpan = span
                            }
                            PadGesture.TwoPinchLocal -> {
                                applyLocalPinchPan(centroid, prevCentroid, span, prevPinchSpan)
                                prevCentroid = centroid
                                prevPinchSpan = span
                            }
                            PadGesture.ThreePending, PadGesture.ThreePanZoomLocal, PadGesture.Done -> {}
                        }
                        continue
                    }

                    // Single finger remaining
                    val p = pressed[0]
                    val nowMs = SystemClock.uptimeMillis()
                    when (mode) {
                        PadGesture.TwoPending, PadGesture.ThreePending -> {
                            // Let the last finger lift cleanly so tap-count completes accurately
                        }
                        PadGesture.TwoScroll, PadGesture.TwoPinchRemote -> {
                            endPtpIfActive()
                            mode = PadGesture.Done
                        }
                        PadGesture.TwoPinchLocal, PadGesture.ThreePanZoomLocal -> {
                            mode = PadGesture.Done
                        }
                        PadGesture.Done -> {}
                        PadGesture.Pending -> {
                            if ((p.position - startPos).getDistance() > touchSlop) {
                                if (startedOnRightEdge && abs(p.position.y - startPos.y) >= abs(p.position.x - startPos.x)) {
                                    mode = PadGesture.EdgeScroll
                                    currentOnTouchpad(0, 0, 0, 1000)
                                    ptpScrollActive = true
                                    prevSinglePos = p.position
                                } else if (isDoubleTapCandidate) {
                                    mode = PadGesture.Dragging
                                    currentOnMouseMove(cursorNormX, cursorNormY)
                                    currentOnMouseButton(0, true)
                                    leftButtonDown = true
                                    isDraggingVisual = true
                                    prevSinglePos = p.position
                                    prevMoveMs = nowMs
                                    applyVentoTrackpadDelta(
                                        p.position.x - startPos.x,
                                        p.position.y - startPos.y,
                                        (nowMs - downTime).coerceAtLeast(1L).toFloat(),
                                    ) { nx, ny -> sendMouseMoveThrottled(nx, ny) }
                                } else {
                                    mode = PadGesture.TrackpadMove
                                    prevSinglePos = p.position
                                    prevMoveMs = nowMs
                                    applyVentoTrackpadDelta(
                                        p.position.x - startPos.x,
                                        p.position.y - startPos.y,
                                        (nowMs - downTime).coerceAtLeast(1L).toFloat(),
                                    ) { nx, ny -> sendMouseMoveThrottled(nx, ny) }
                                }
                            }
                        }
                        PadGesture.EdgeScroll -> {
                            val dyDp10 = ((p.position.y - prevSinglePos.y) / density.density * 14f).roundToInt()
                            if (dyDp10 != 0) {
                                currentOnTouchpad(1, 0, dyDp10, 1000)
                                prevSinglePos = p.position
                            }
                        }
                        PadGesture.TrackpadMove -> {
                            val dx = p.position.x - prevSinglePos.x
                            val dy = p.position.y - prevSinglePos.y
                            val dt = (nowMs - prevMoveMs).coerceAtLeast(1L).toFloat()
                            velPxPerMsX = velPxPerMsX * 0.45f + (dx / dt) * 0.55f
                            velPxPerMsY = velPxPerMsY * 0.45f + (dy / dt) * 0.55f
                            prevSinglePos = p.position
                            prevMoveMs = nowMs
                            applyVentoTrackpadDelta(dx, dy, dt) { nx, ny ->
                                sendMouseMoveThrottled(nx, ny)
                            }
                        }
                        PadGesture.LongPressed -> {
                            if ((p.position - startPos).getDistance() > touchSlop) {
                                mode = PadGesture.LongPressDragging
                                currentOnMouseMove(cursorNormX, cursorNormY)
                                currentOnMouseButton(0, true)
                                leftButtonDown = true
                                isDraggingVisual = true
                                prevSinglePos = p.position
                                prevMoveMs = nowMs
                                applyVentoTrackpadDelta(
                                    p.position.x - startPos.x,
                                    p.position.y - startPos.y,
                                    (nowMs - downTime).coerceAtLeast(1L).toFloat(),
                                ) { nx, ny -> sendMouseMoveThrottled(nx, ny) }
                            }
                        }
                        PadGesture.Dragging, PadGesture.LongPressDragging -> {
                            val dx = p.position.x - prevSinglePos.x
                            val dy = p.position.y - prevSinglePos.y
                            val dt = (nowMs - prevMoveMs).coerceAtLeast(1L).toFloat()
                            prevSinglePos = p.position
                            prevMoveMs = nowMs
                            applyVentoTrackpadDelta(dx, dy, dt) { nx, ny ->
                                sendMouseMoveThrottled(nx, ny)
                            }
                        }
                    }
                }
            } finally {
                trackpadTouchActive = false
                if (leftButtonDown) {
                    currentOnMouseButton(0, false)
                    isDraggingVisual = false
                }
                endPtpIfActive()
            }
        }
    }

    @Composable
    fun CursorAndRippleOverlay() {
        val showCursor = state.cursorVisible && state.touchMode == TouchMode.Trackpad
        val rawCursorPos = liveStreamToScreen(cursorNormX, cursorNormY)
        if (isDraggingVisual && rawCursorPos != null) {
            Canvas(modifier = Modifier.fillMaxSize()) {
                drawCircle(
                    color = Color(0xFF0A84FF).copy(alpha = 0.82f),
                    radius = 14.dp.toPx(),
                    center = rawCursorPos,
                    style = Stroke(width = 2.dp.toPx()),
                )
            }
        }
        val cursorScreenPos = if (showCursor) rawCursorPos else null
        if (cursorScreenPos != null) {
            val glyph = state.cursorGlyphs[state.currentCursorId]
            // Phone px per host desktop px: the cursor image arrives at the size the host shows it,
            // so drawing it at this scale makes it as large as on the remote screen, zoom included.
            val hostToScreen = hostDesktopToScreenScale(
                cw = videoContainerSize.width.toFloat(),
                ch = videoContainerSize.height.toFloat(),
                sw = state.streamMeta.width,
                sh = state.streamMeta.height,
                monitor = state.monitors.getOrNull(state.currentMonitorIndex),
                zoom = zoomScale,
            )
            val minCursorSidePx = with(density) { MIN_CURSOR_BOX_DP.dp.toPx() }
            if (glyph != null) {
                val cursorScale = maxOf(
                    hostToScreen,
                    minCursorSidePx / maxOf(glyph.width, glyph.height).coerceAtLeast(1),
                )
                val drawW = (glyph.width * cursorScale).roundToInt().coerceAtLeast(1)
                val drawH = (glyph.height * cursorScale).roundToInt().coerceAtLeast(1)
                val offX = (cursorScreenPos.x - glyph.hotX * cursorScale).roundToInt()
                val offY = (cursorScreenPos.y - glyph.hotY * cursorScale).roundToInt()
                Image(
                    bitmap = glyph.bitmap,
                    contentDescription = null,
                    contentScale = ContentScale.FillBounds,
                    filterQuality = FilterQuality.Medium,
                    modifier = Modifier
                        .offset { IntOffset(offX, offY) }
                        .size(
                            width = with(density) { drawW.toDp() },
                            height = with(density) { drawH.toDp() },
                        ),
                )
            } else {
                Canvas(modifier = Modifier.fillMaxSize()) {
                    // No shape from the host yet: a Windows arrow is ~17 px tall at 100% scale
                    // (path height is 1.12 × s).
                    val s = maxOf(17f * hostToScreen, minCursorSidePx * 0.6f) / 1.12f
                    val path = Path().apply {
                        moveTo(cursorScreenPos.x, cursorScreenPos.y)
                        lineTo(cursorScreenPos.x, cursorScreenPos.y + s)
                        lineTo(cursorScreenPos.x + s * 0.28f, cursorScreenPos.y + s * 0.74f)
                        lineTo(cursorScreenPos.x + s * 0.50f, cursorScreenPos.y + s * 1.12f)
                        lineTo(cursorScreenPos.x + s * 0.66f, cursorScreenPos.y + s * 1.04f)
                        lineTo(cursorScreenPos.x + s * 0.44f, cursorScreenPos.y + s * 0.66f)
                        lineTo(cursorScreenPos.x + s * 0.76f, cursorScreenPos.y + s * 0.62f)
                        close()
                    }
                    drawPath(
                        path = path,
                        color = Color(0xCC0D0D12),
                        style = Stroke(width = 2.6.dp.toPx(), join = StrokeJoin.Round),
                    )
                    drawPath(path = path, color = Color.White)
                }
            }
        }

        val rPos = tapPos
        val rp = rippleProgress.value
        if (rPos != null && rp < 1f) {
            Canvas(modifier = Modifier.fillMaxSize()) {
                val radius = (6.dp + 16.dp * rp).toPx()
                val alpha = (1f - rp).coerceIn(0f, 1f)
                drawCircle(
                    color = (if (tapIsRight) Color(0xFFFFD60A) else Color.White).copy(alpha = alpha * 0.72f),
                    radius = radius,
                    center = rPos,
                    style = Stroke(width = 1.75.dp.toPx()),
                )
            }
        }
    }

    BoxWithConstraints(
        modifier = Modifier
            .fillMaxSize()
            .background(Color.Black),
    ) {
        // Fullscreen Edge-to-Edge Canvas (unified across portrait, landscape, and tablet)
        Box(
            modifier = Modifier
                .fillMaxSize()
                .onSizeChanged { newSize ->
                    videoContainerSize = newSize
                    applyInitialWindowFocus(newSize)
                },
        ) {
            VideoSurface(
                streamWidth = state.streamMeta.width,
                streamHeight = state.streamMeta.height,
                onSurfaceReady = onSurfaceReady,
                onSurfaceDestroyed = onSurfaceDestroyed,
                zoomScale = zoomScale,
                panX = panX,
                panY = liveEffectivePanY(),
                cornerRadius = 0.dp,
                modifier = Modifier.fillMaxSize(),
            )
            Box(
                modifier = Modifier
                    .fillMaxSize()
                    .trackpadGestureInput(
                        surfaceWidthPxProvider = { videoContainerSize.width.toFloat() },
                        allowEdgeScroll = false,
                        allowDirectMultiTouch = true,
                    ),
            )
            CursorAndRippleOverlay()
        }

        // Tap-outside scrim for floating menu
        if (menuExpanded) {
            Box(
                modifier = Modifier
                    .fillMaxSize()
                    .clickable(
                        interactionSource = remember { MutableInteractionSource() },
                        indication = null,
                    ) {
                        capsuleDimmed = true
                        onMenuExpandedChange(false)
                    },
            )
        }

        // Floating Liquid Glass Capsule positioned at TopCenter (clears Windows top-right - □ ×,
        // top-left back/tabs, camera punch-hole even with hidden status bar, and edge-back gestures).
        Column(
            horizontalAlignment = Alignment.CenterHorizontally,
            modifier = Modifier
                .align(Alignment.TopCenter)
                .displayCutoutPadding()
                .navigationBarsPadding()
                .padding(top = 8.dp, start = 10.dp, end = 10.dp, bottom = 10.dp),
        ) {
            FloatingGlass(
                cornerRadius = 999.dp,
                darkOverlay = true,
                elevation = 12.dp,
                modifier = Modifier
                    .alpha(capsuleAlpha),
            ) {
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    modifier = Modifier
                        .height(grow(38f, 42f).dp)
                        .padding(horizontal = 2.dp),
                ) {
                    CapsuleItem(g = capsuleGrow, contentDescription = "返回", onClick = {
                        lastCapsuleTouchMs = System.currentTimeMillis()
                        onReturnToHub()
                    }) {
                        BackArrowIcon(tint = Color.White, size = grow(18f, 20f).dp, strokeWidth = 1.9.dp)
                    }

                    CapsuleDivider()

                    val isTrackpad = state.touchMode == TouchMode.Trackpad
                    CapsuleItem(g = capsuleGrow, contentDescription = "切换触控模式", horizontalPadding = 11.dp, onClick = {
                        lastCapsuleTouchMs = System.currentTimeMillis()
                        val next = if (isTrackpad) TouchMode.Direct else TouchMode.Trackpad
                        onSetTouchMode(next)
                        onShowToast(
                            if (next == TouchMode.Trackpad) {
                                "触控板：单指移动光标，轻点单击，双指滚动"
                            } else {
                                "直触：点哪里就是哪里，单指拖动画面，长按选字"
                            },
                        )
                    }) {
                        Row(
                            verticalAlignment = Alignment.CenterVertically,
                            horizontalArrangement = Arrangement.spacedBy(5.dp),
                        ) {
                            Icon(
                                imageVector = if (isTrackpad) Icons.Rounded.Mouse else Icons.Rounded.TouchApp,
                                contentDescription = null,
                                tint = Color.White,
                                modifier = Modifier.size(grow(15f, 18f).dp),
                            )
                            Text(
                                text = if (isTrackpad) "触控板" else "直触",
                                color = Color.White,
                                fontSize = grow(12.5f, 13.5f).sp,
                                fontWeight = FontWeight.Medium,
                                maxLines = 1,
                                softWrap = false,
                            )
                        }
                    }

                    if (zoomScale > 1.02f) {
                        CapsuleItem(g = capsuleGrow, contentDescription = "恢复全屏", onClick = {
                            lastCapsuleTouchMs = System.currentTimeMillis()
                            userAdjustedViewport = true
                            zoomScale = 1f
                            panX = 0f
                            panY = 0f
                        }) {
                            Text(
                                text = String.format(java.util.Locale.US, "%.1f×", zoomScale),
                                color = RcInk.accent,
                                fontSize = grow(12f, 13.5f).sp,
                                fontWeight = FontWeight.SemiBold,
                                maxLines = 1,
                                softWrap = false,
                            )
                        }
                    }

                    CapsuleDivider()

                    CapsuleItem(g = capsuleGrow, contentDescription = "键盘", active = inputBarOpen, onClick = {
                        lastCapsuleTouchMs = System.currentTimeMillis()
                        toggleKeyboard()
                    }) {
                        Icon(
                            imageVector = Icons.Rounded.Keyboard,
                            contentDescription = null,
                            tint = if (inputBarOpen) RcInk.accent else Color.White,
                            modifier = Modifier.size(grow(17f, 20f).dp),
                        )
                    }

                    CapsuleItem(g = capsuleGrow, contentDescription = "切换横竖屏", onClick = {
                        lastCapsuleTouchMs = System.currentTimeMillis()
                        onToggleOrientation()
                    }) {
                        Icon(
                            imageVector = Icons.Rounded.ScreenRotation,
                            contentDescription = null,
                            tint = Color.White,
                            modifier = Modifier.size(grow(16f, 19f).dp),
                        )
                    }

                    CapsuleItem(g = capsuleGrow, contentDescription = "更多", active = menuExpanded, neutralActive = true, onClick = {
                        lastCapsuleTouchMs = System.currentTimeMillis()
                        onMenuExpandedChange(!menuExpanded)
                    }) {
                        Icon(
                            imageVector = Icons.Rounded.MoreHoriz,
                            contentDescription = null,
                            tint = Color.White,
                            modifier = Modifier.size(grow(17f, 20f).dp),
                        )
                    }
                }
            }

            Spacer(modifier = Modifier.height(8.dp))

            // Clean Floating Quality Popover (live stats + quality presets only)
            AnimatedVisibility(
                visible = menuExpanded,
                enter = fadeIn(tween(150)) + scaleIn(
                    initialScale = 0.92f,
                    transformOrigin = TransformOrigin(0.5f, 0f),
                    animationSpec = spring(
                        dampingRatio = Spring.DampingRatioNoBouncy,
                        stiffness = Spring.StiffnessMedium,
                    ),
                ),
                exit = fadeOut(tween(120)) + scaleOut(
                    targetScale = 0.94f,
                    transformOrigin = TransformOrigin(0.5f, 0f),
                ),
            ) {
                val stats = state.liveStats
                val curMon = state.monitors.getOrNull(state.currentMonitorIndex)?.displayTitle ?: "主屏 1"
                val activeWin = state.activeWindow
                val headerTitle = if (activeWin != null) {
                    "${activeWin.appName} · $curMon"
                } else {
                    "${state.activeDevice?.name ?: "电脑"} · $curMon"
                }
                val pathLabel = pathKindLabel(stats.pathKind)
                val showRouteOption = state.ownRelayCount > 0 || state.activeDevice?.hasSelfHostedRelay == true
                val cfg = LocalConfiguration.current
                val compactPopover = cfg.screenHeightDp < 480
                val maxPopoverH = (cfg.screenHeightDp.dp - 76.dp).coerceAtLeast(160.dp)
                val maxPopoverW = (cfg.screenWidthDp.dp - 24.dp).coerceAtLeast(220.dp)

                FloatingGlass(
                    cornerRadius = 20.dp,
                    darkOverlay = true,
                    elevation = 20.dp,
                    modifier = Modifier
                        .widthIn(max = maxPopoverW)
                        .width(304.dp)
                        .heightIn(max = maxPopoverH),
                ) {
                    Column(
                        modifier = Modifier
                            .background(Color(0x99141416))
                            .verticalScroll(rememberScrollState())
                            // Consume taps inside the popover so gaps between rows/segments never dismiss it
                            .clickable(
                                interactionSource = remember { MutableInteractionSource() },
                                indication = null,
                            ) {},
                    ) {
                        Column(
                            modifier = Modifier
                                .fillMaxWidth()
                                .padding(
                                    horizontal = 16.dp,
                                    vertical = if (compactPopover) 10.dp else 14.dp,
                                ),
                        ) {
                            Row(
                                verticalAlignment = Alignment.CenterVertically,
                                horizontalArrangement = Arrangement.spacedBy(7.dp),
                            ) {
                                StatusDot(color = c.green, size = 7.dp)
                                Text(
                                    text = headerTitle,
                                    color = RcInk.primary,
                                    fontWeight = FontWeight.SemiBold,
                                    fontSize = 15.sp,
                                    maxLines = 1,
                                    overflow = TextOverflow.Ellipsis,
                                )
                            }
                            if (activeWin != null && activeWin.title.isNotEmpty()) {
                                Text(
                                    text = activeWin.title,
                                    color = RcInk.secondary,
                                    fontSize = 13.sp,
                                    maxLines = 1,
                                    overflow = TextOverflow.Ellipsis,
                                    modifier = Modifier.padding(top = 3.dp),
                                )
                            }
                            Text(
                                text = buildString {
                                    append("${state.streamMeta.width}×${state.streamMeta.height}")
                                    append(" · ${stats.fps} fps · ${stats.rttMs} ms")
                                    if (pathLabel.isNotEmpty()) append(" · $pathLabel")
                                },
                                color = RcInk.secondary,
                                fontSize = 12.sp,
                                style = TextStyle(fontFeatureSettings = "tnum"),
                                modifier = Modifier.padding(top = 3.dp),
                            )
                        }

                        MenuHairline()
                        QualitySettingsSection(
                            quality = state.quality,
                            onUpdateQuality = onUpdateQuality,
                            showRouteOption = showRouteOption,
                            routeHint = if (pathLabel.isNotEmpty()) "当前：$pathLabel" else "",
                            compact = compactPopover,
                            modifier = Modifier.padding(
                                horizontal = 16.dp,
                                vertical = if (compactPopover) 10.dp else 14.dp,
                            ),
                        )
                    }
                }
            }
        }

        // Ultra-clean Left-Thumb Pan Joystick when zoomed in:
        // Moves the zoomed viewport smoothly AND keeps the mouse cursor riding cleanly in the
        // center of the visible screen so touching the screen never jumps back to an old corner.
        val isLandscape = LocalConfiguration.current.orientation == Configuration.ORIENTATION_LANDSCAPE
        AnimatedVisibility(
            visible = zoomScale > 1.05f && !inputBarOpen && !imeVisible,
            enter = fadeIn(tween(150)) + scaleIn(
                initialScale = 0.82f,
                animationSpec = spring(
                    dampingRatio = Spring.DampingRatioNoBouncy,
                    stiffness = Spring.StiffnessMedium,
                ),
            ),
            exit = fadeOut(tween(120)) + scaleOut(targetScale = 0.84f),
            modifier = Modifier
                .align(Alignment.BottomStart)
                .displayCutoutPadding()
                .navigationBarsPadding()
                .padding(
                    start = if (isLandscape) 22.dp else 16.dp,
                    bottom = if (isLandscape) 24.dp else 32.dp,
                ),
        ) {
            var lastJoyMouseSyncMs by remember { mutableLongStateOf(0L) }
            VirtualPanJoystick(
                outerSizeDp = if (isLandscape) 104f else 96f,
                onPanVelocity = { nx, ny, dtSec ->
                    val cw = videoContainerSize.width.toFloat().coerceAtLeast(1f)
                    val ch = videoContainerSize.height.toFloat().coerceAtLeast(1f)
                    val maxSpeed = maxOf(cw, ch) * 0.92f
                    val dx = -nx * maxSpeed * dtSec
                    val dy = -ny * maxSpeed * dtSec
                    if (dx != 0f || dy != 0f) {
                        userAdjustedViewport = true
                        val prevPanX = panX
                        val prevPanY = panY
                        val (cx, cy) = liveClampPan(prevPanX + dx, prevPanY + dy, zoomScale)
                        panX = cx
                        panY = cy

                        // The cursor keeps its spot on the phone screen: the PC desktop slides
                        // under it, so it moves across the desktop by the same amount as the view.
                        val nowMs = SystemClock.uptimeMillis()
                        val actualDx = cx - prevPanX
                        val actualDy = cy - prevPanY
                        val st = currentState
                        val (vidW, vidH) = computeVideoRect(cw, ch, st.streamMeta.width, st.streamMeta.height)
                            ?: (cw to ch)
                        val effW = (vidW * zoomScale).coerceAtLeast(1f)
                        val effH = (vidH * zoomScale).coerceAtLeast(1f)
                        val shiftedX = (cursorNormX - actualDx / effW).coerceIn(0f, 1f)
                        val shiftedY = (cursorNormY - actualDy / effH).coerceIn(0f, 1f)
                        cursorNormX = shiftedX
                        cursorNormY = shiftedY
                        if (nowMs - lastJoyMouseSyncMs >= 16L) {
                            currentOnMouseMove(shiftedX, shiftedY)
                            lastJoyMouseSyncMs = nowMs
                        }
                    }
                },
                onRelease = {
                    // Send the final position that throttling may have skipped.
                    currentOnMouseMove(cursorNormX, cursorNormY)
                },
            )
        }

        // Invisible editor that owns input focus while typing: the phone keyboard writes into it
        // and everything is forwarded to the PC as it is typed.
        AndroidView(
            factory = { ctx ->
                RemoteImeSinkView(ctx).also {
                    it.listener = imeListener
                    sinkView = it
                }
            },
            modifier = Modifier
                .align(Alignment.TopStart)
                .size(1.dp)
                .alpha(0f),
        )

        // Shortcut bar just under the capsule; the PC keyboard unfolds from it. Neither touches
        // the bottom of the screen, so they never fight the phone keyboard for space.
        val screenHDp = config.screenHeightDp.toFloat()
        val capsuleBottomDp = 8f + grow(38f, 42f) + 6f
        val compactKeys = screenHDp < 480f
        AnimatedVisibility(
            visible = inputBarOpen && !menuExpanded,
            enter = fadeIn(tween(120)) + slideInVertically(tween(120), initialOffsetY = { -it / 3 }),
            exit = fadeOut(tween(70)),
            modifier = Modifier
                .align(Alignment.TopCenter)
                .displayCutoutPadding()
                .padding(top = capsuleBottomDp.dp, start = 8.dp, end = 8.dp)
                .onGloballyPositioned { keyBarBottomPx = it.boundsInRoot().bottom.roundToInt() },
        ) {
            RemoteKeyBar(
                compact = compactKeys,
                phoneKeyboardOn = imeWanted && !pcKeyboardOpen,
                pcKeyboardOpen = pcKeyboardOpen,
                composing = composingText,
                modStates = modStates,
                onToggleModifier = { toggleStickyModifier(it) },
                onKey = { scan ->
                    onTapKey(scan)
                    releaseArmedModifiers()
                },
                onChord = { mod, key ->
                    onTapChord(mod, key)
                    releaseArmedModifiers()
                },
                onTogglePhoneKeyboard = {
                    if (imeWanted && !pcKeyboardOpen) {
                        imeWanted = false
                    } else {
                        pcKeyboardOpen = false
                        imeWanted = true
                    }
                },
                onTogglePcKeyboard = {
                    pcKeyboardOpen = !pcKeyboardOpen
                    if (pcKeyboardOpen) imeWanted = false
                },
            )
        }
    }
}

/**
 * Minimalist dual-circle pan joystick (left thumb) for zoomed-in desktops.
 * Pure two-circle geometry — translucent outer ring + clean white glass inner circle,
 * zero extra dots/glows/clutter, smooth spring return on release.
 */
@Composable
private fun VirtualPanJoystick(
    outerSizeDp: Float,
    onPanVelocity: (nx: Float, ny: Float, dtSec: Float) -> Unit,
    onRelease: () -> Unit = {},
    modifier: Modifier = Modifier,
) {
    val currentOnPan by rememberUpdatedState(onPanVelocity)
    val currentOnRelease by rememberUpdatedState(onRelease)
    val haptics = LocalHapticFeedback.current
    val density = LocalDensity.current
    val scope = rememberCoroutineScope()

    val knobSizeDp = outerSizeDp * 0.40f
    val maxTravelPx = with(density) { ((outerSizeDp - knobSizeDp) * 0.48f).dp.toPx() }.coerceAtLeast(1f)

    val knobX = remember { Animatable(0f) }
    val knobY = remember { Animatable(0f) }
    var active by remember { mutableStateOf(false) }

    val baseAlpha by animateFloatAsState(
        targetValue = if (active) 0.94f else 0.56f,
        animationSpec = tween(140),
        label = "joyAlpha",
    )
    val knobScale by animateFloatAsState(
        targetValue = if (active) 1.06f else 1f,
        animationSpec = spring(
            dampingRatio = Spring.DampingRatioNoBouncy,
            stiffness = Spring.StiffnessMedium,
        ),
        label = "joyKnobScale",
    )

    LaunchedEffect(active) {
        if (!active) return@LaunchedEffect
        var lastNs = 0L
        while (active) {
            withFrameNanos { nowNs ->
                if (lastNs != 0L) {
                    val dt = ((nowNs - lastNs) / 1_000_000_000f).coerceIn(0f, 0.05f)
                    val rawNx = (knobX.value / maxTravelPx).coerceIn(-1f, 1f)
                    val rawNy = (knobY.value / maxTravelPx).coerceIn(-1f, 1f)
                    val mag = hypot(rawNx.toDouble(), rawNy.toDouble()).toFloat().coerceAtMost(1f)
                    val deadzone = 0.06f
                    if (mag > deadzone) {
                        val remapped = ((mag - deadzone) / (1f - deadzone)).coerceIn(0f, 1f)
                        val curved = remapped.pow(1.4f)
                        val scale = curved / mag
                        currentOnPan(rawNx * scale, rawNy * scale, dt)
                    }
                }
                lastNs = nowNs
            }
        }
    }

    Box(
        modifier = modifier
            .size(outerSizeDp.dp)
            .alpha(baseAlpha)
            .clip(CircleShape)
            .background(Color(0x2B000000))
            .border(
                width = 1.dp,
                color = if (active) Color(0x72FFFFFF) else Color(0x42FFFFFF),
                shape = CircleShape,
            )
            .pointerInput(maxTravelPx) {
                awaitEachGesture {
                    val down = awaitFirstDown(requireUnconsumed = false)
                    down.consume()
                    active = true
                    haptics.performHapticFeedback(HapticFeedbackType.TextHandleMove)

                    val center = Offset(size.width * 0.5f, size.height * 0.5f)
                    fun updateStick(pos: Offset) {
                        var dx = pos.x - center.x
                        var dy = pos.y - center.y
                        val dist = hypot(dx.toDouble(), dy.toDouble()).toFloat()
                        if (dist > maxTravelPx && dist > 0f) {
                            val k = maxTravelPx / dist
                            dx *= k
                            dy *= k
                        }
                        scope.launch {
                            knobX.snapTo(dx)
                            knobY.snapTo(dy)
                        }
                    }

                    updateStick(down.position)
                    val pointerId = down.id
                    while (true) {
                        val event = awaitPointerEvent()
                        val change = event.changes.firstOrNull { it.id == pointerId } ?: break
                        change.consume()
                        if (!change.pressed) break
                        updateStick(change.position)
                    }

                    active = false
                    currentOnRelease()
                    scope.launch {
                        knobX.animateTo(
                            0f,
                            spring(
                                dampingRatio = Spring.DampingRatioMediumBouncy,
                                stiffness = Spring.StiffnessMedium,
                            ),
                        )
                    }
                    scope.launch {
                        knobY.animateTo(
                            0f,
                            spring(
                                dampingRatio = Spring.DampingRatioMediumBouncy,
                                stiffness = Spring.StiffnessMedium,
                            ),
                        )
                    }
                }
            },
        contentAlignment = Alignment.Center,
    ) {
        // Pure single translucent white inner circle
        Box(
            modifier = Modifier
                .offset { IntOffset(knobX.value.roundToInt(), knobY.value.roundToInt()) }
                .scale(knobScale)
                .size(knobSizeDp.dp)
                .clip(CircleShape)
                .background(if (active) Color(0xEBFFFFFF) else Color(0xBDFFFFFF)),
        )
    }
}

/**
 * One tappable slot on the fullscreen capsule. `g` (0 idle → 1 touched) grows it slightly.
 * `active` gives it a soft accent tint (or a neutral one for the `⋯` menu).
 */
@Composable
private fun CapsuleItem(
    g: Float,
    contentDescription: String,
    active: Boolean = false,
    neutralActive: Boolean = false,
    horizontalPadding: Dp = 10.dp,
    onClick: () -> Unit,
    content: @Composable () -> Unit,
) {
    val bg by animateColorAsState(
        targetValue = when {
            !active -> Color.Transparent
            neutralActive -> Color(0x29FFFFFF)
            else -> RcInk.accentTint
        },
        animationSpec = tween(160),
        label = "capsuleItemBg",
    )
    Box(
        modifier = Modifier
            .height((30f + 4f * g).dp)
            .widthIn(min = (32f + 6f * g).dp)
            .clip(RoundedCornerShape(999.dp))
            .background(bg)
            .semantics { this.contentDescription = contentDescription }
            .liquidPress(pressedScale = 0.9f, onClick = onClick)
            .padding(horizontal = horizontalPadding * (0.9f + 0.1f * g)),
        contentAlignment = Alignment.Center,
    ) {
        content()
    }
}

@Composable
private fun CapsuleDivider() {
    Box(
        modifier = Modifier
            .padding(horizontal = 2.dp)
            .width(0.5.dp)
            .height(20.dp)
            .background(Color(0x2EFFFFFF)),
    )
}

@Composable
private fun MenuHairline() {
    Box(
        modifier = Modifier
            .fillMaxWidth()
            .height(0.5.dp)
            .background(RcInk.hairline),
    )
}

@Composable
private fun FloatingMenuRow(
    icon: ImageVector,
    label: String,
    danger: Boolean = false,
    onClick: () -> Unit,
) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(10.dp))
            .liquidPress(pressedScale = 0.97f, onClick = onClick)
            .padding(horizontal = 10.dp, vertical = 9.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        Icon(
            imageVector = icon,
            contentDescription = null,
            tint = if (danger) Color(0xFFFF453A) else Color.White,
            modifier = Modifier.size(17.dp),
        )
        Text(
            text = label,
            color = if (danger) Color(0xFFFF453A) else Color.White,
            fontSize = 13.5.sp,
            fontWeight = FontWeight.Normal,
        )
    }
}

internal data class AutoFocusViewport(
    val zoomScale: Float,
    val panX: Float,
    val panY: Float,
)

internal fun computeWindowAutoFocus(
    rx: Float,
    ry: Float,
    rw: Float,
    rh: Float,
    cw: Float,
    ch: Float,
    sw: Int,
    sh: Int,
    maxZoom: Float = 6.0f,
): AutoFocusViewport? {
    if (cw <= 0f || ch <= 0f || rw <= 0.02f || rh <= 0.02f) return null
    val (baseW, baseH) = computeVideoRect(cw, ch, sw, sh) ?: return null
    val fitZoomX = cw / (rw * baseW)
    val fitZoomY = ch / (rh * baseH)
    val targetZoom = (minOf(fitZoomX, fitZoomY) * 0.98f).coerceIn(1f, maxZoom)
    val scaledW = baseW * targetZoom
    val scaledH = baseH * targetZoom
    val cx = (rx + rw * 0.5f).coerceIn(0f, 1f)
    val cy = (ry + rh * 0.5f).coerceIn(0f, 1f)
    val rawPanX = (0.5f - cx) * scaledW
    val rawPanY = (0.5f - cy) * scaledH
    val (clampedX, clampedY) = clampPanOffset(rawPanX, rawPanY, targetZoom, cw, ch, sw, sh)
    return AutoFocusViewport(targetZoom, clampedX, clampedY)
}

internal fun computeVideoRect(cw: Float, ch: Float, sw: Int, sh: Int): Pair<Float, Float>? {
    if (cw <= 0f || ch <= 0f) return null
    val streamAspect = (sw.coerceAtLeast(1).toFloat() / sh.coerceAtLeast(1).toFloat()).coerceIn(0.25f, 4.0f)
    val viewAspect = cw / ch
    return if (viewAspect > streamAspect) {
        (ch * streamAspect) to ch
    } else {
        cw to (cw / streamAspect)
    }
}

/**
 * Smallest on-screen side of the cursor image box. A standard 32 px Windows cursor box holds an
 * arrow about 60% of its height, so the arrow itself stays near 10dp when zoomed far out.
 */
private const val MIN_CURSOR_BOX_DP = 16f

/**
 * Phone pixels per host desktop pixel for the video as currently shown: displayed video width
 * (fit rect × pinch zoom) over the host monitor's width in physical pixels. Falls back to the
 * stream width when the monitor size is unknown or doesn't match the stream's aspect ratio
 * (then the stream is assumed to be at desktop resolution).
 */
internal fun hostDesktopToScreenScale(
    cw: Float,
    ch: Float,
    sw: Int,
    sh: Int,
    monitor: MonitorInfo?,
    zoom: Float,
): Float {
    val (baseW, _) = computeVideoRect(cw, ch, sw, sh) ?: return 1f
    val streamW = sw.coerceAtLeast(1).toFloat()
    val streamAspect = streamW / sh.coerceAtLeast(1).toFloat()
    val desktopW = monitor
        ?.takeIf { it.width > 0 && it.height > 0 }
        ?.takeIf { abs(it.width.toFloat() / it.height - streamAspect) <= streamAspect * 0.03f }
        ?.width?.toFloat()
        ?: streamW
    return (baseW * zoom / desktopW).coerceAtLeast(0.01f)
}

internal fun clampPanOffset(
    px: Float,
    py: Float,
    scale: Float,
    cw: Float,
    ch: Float,
    sw: Int,
    sh: Int,
): Pair<Float, Float> {
    val vr = computeVideoRect(cw, ch, sw, sh) ?: return 0f to 0f
    val maxX = ((vr.first * scale - cw).coerceAtLeast(0f)) * 0.5f
    val maxY = ((vr.second * scale - ch).coerceAtLeast(0f)) * 0.5f
    return px.coerceIn(-maxX, maxX) to py.coerceIn(-maxY, maxY)
}

internal fun mapScreenToStream(
    offset: Offset,
    cw: Float,
    ch: Float,
    sw: Int,
    sh: Int,
    scale: Float,
    px: Float,
    py: Float,
): Pair<Float, Float>? {
    val (baseW, baseH) = computeVideoRect(cw, ch, sw, sh) ?: return null
    // Exact mirror of GlVideoSurfaceView.drawFrame() & fragment shader uVideoRect math
    val scaledW = baseW * scale
    val scaledH = baseH * scale
    val left = cw * 0.5f + px - scaledW * 0.5f
    val top = ch * 0.5f + py - scaledH * 0.5f
    val nx = ((offset.x - left) / scaledW).coerceIn(0f, 1f)
    val ny = ((offset.y - top) / scaledH).coerceIn(0f, 1f)
    return nx to ny
}

internal fun mapStreamToScreen(
    nx: Float,
    ny: Float,
    cw: Float,
    ch: Float,
    sw: Int,
    sh: Int,
    scale: Float,
    px: Float,
    py: Float,
): Offset? {
    val (baseW, baseH) = computeVideoRect(cw, ch, sw, sh) ?: return null
    val scaledW = baseW * scale
    val scaledH = baseH * scale
    val left = cw * 0.5f + px - scaledW * 0.5f
    val top = ch * 0.5f + py - scaledH * 0.5f
    return Offset(
        left + nx.coerceIn(0f, 1f) * scaledW,
        top + ny.coerceIn(0f, 1f) * scaledH,
    )
}
