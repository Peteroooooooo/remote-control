package com.rc.android.ui

import android.graphics.BlurMaskFilter
import android.graphics.LinearGradient
import android.graphics.Shader
import android.view.Surface
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.Spring
import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.core.animateDpAsState
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.spring
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.scaleIn
import androidx.compose.animation.scaleOut
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.displayCutoutPadding
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.foundation.lazy.grid.rememberLazyGridState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.rounded.Send
import androidx.compose.material.icons.outlined.DesktopWindows
import androidx.compose.material.icons.rounded.Code
import androidx.compose.material.icons.rounded.Description
import androidx.compose.material.icons.rounded.EditNote
import androidx.compose.material.icons.rounded.Folder
import androidx.compose.material.icons.rounded.Forum
import androidx.compose.material.icons.rounded.GridView
import androidx.compose.material.icons.rounded.HeadsetMic
import androidx.compose.material.icons.rounded.KeyboardArrowDown
import androidx.compose.material.icons.rounded.Language
import androidx.compose.material.icons.rounded.MusicNote
import androidx.compose.material.icons.rounded.OpenInFull
import androidx.compose.material.icons.rounded.Public
import androidx.compose.material.icons.rounded.Refresh
import androidx.compose.material.icons.rounded.ScreenRotation
import androidx.compose.material.icons.rounded.Shield
import androidx.compose.material.icons.rounded.Slideshow
import androidx.compose.material.icons.rounded.SportsEsports
import androidx.compose.material.icons.rounded.TableChart
import androidx.compose.material.icons.rounded.Tag
import androidx.compose.material.icons.rounded.Terminal
import androidx.compose.material.icons.rounded.WebAsset
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.draw.scale
import androidx.compose.ui.draw.shadow
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.launch
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.layout.boundsInRoot
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.layout.positionInRoot
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.TransformOrigin
import androidx.compose.ui.graphics.drawscope.drawIntoCanvas
import androidx.compose.ui.graphics.nativeCanvas
import androidx.compose.ui.graphics.toArgb
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.text.PlatformTextStyle
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.rc.android.model.MonitorInfo
import com.rc.android.model.QualityPrefs
import com.rc.android.model.RcUiState
import com.rc.android.model.RunningWindowItem
import com.rc.android.model.SessionPhase

private data class DockAppGroup(
    val appKey: String,
    val appName: String,
    val count: Int,
)

@Composable
fun HubScreen(
    state: RcUiState,
    onBackToDevices: () -> Unit,
    onEnterFullscreen: () -> Unit,
    onFocusWindow: (RunningWindowItem) -> Unit,
    onSelectMonitor: (Int) -> Unit,
    onUpdateQuality: ((QualityPrefs) -> QualityPrefs) -> Unit,
    onReconnect: () -> Unit,
    onToggleOrientation: () -> Unit,
    onSurfaceReady: (Surface) -> Unit,
    onSurfaceDestroyed: (Surface) -> Unit,
) {
    val c = RcThemeColors.current
    val config = LocalConfiguration.current

    // Tapping a Dock app floats that app's windows smoothly to the top (zero extra header row / layout jump)
    var dockPinnedApp by remember { mutableStateOf<String?>(null) }
    var dockPulseToken by remember { mutableIntStateOf(0) }
    var qualityPopoverOpen by remember { mutableStateOf(false) }

    val appGroups = remember(state.windows) {
        val order = LinkedHashMap<String, DockAppGroup>()
        for (w in state.windows) {
            val prev = order[w.appKey]
            order[w.appKey] = if (prev == null) {
                DockAppGroup(w.appKey, w.appName, 1)
            } else {
                prev.copy(count = prev.count + 1)
            }
        }
        order.values.toList()
    }

    val appWindowCounts = remember(appGroups) {
        appGroups.associate { it.appKey to it.count }
    }

    val orderedWindows = remember(state.windows, dockPinnedApp) {
        val pinned = dockPinnedApp
        if (pinned == null) {
            state.windows
        } else {
            val (top, rest) = state.windows.partition { it.appKey == pinned }
            if (top.isEmpty()) state.windows else top + rest
        }
    }

    val showFloatingDock = appGroups.size >= 2

    // The layout follows the PC's main monitor only, so switching monitors never moves anything.
    val mainRatio = remember(state.monitors, state.streamMeta.width, state.streamMeta.height) {
        val m = state.monitors.firstOrNull { it.primary } ?: state.monitors.firstOrNull()
        if (m != null && m.width > 0 && m.height > 0) {
            m.width.toFloat() / m.height
        } else if (state.streamMeta.width > 0 && state.streamMeta.height > 0) {
            state.streamMeta.width.toFloat() / state.streamMeta.height
        } else {
            16f / 9f
        }
    }
    val glassBackdrop = rememberGlassBackdrop()
    val halo = rememberHalo()
    var tvBounds by remember { mutableStateOf(Rect.Zero) }
    var haloOrigin by remember { mutableStateOf(Offset.Zero) }
    val bottomClearance = if (showFloatingDock) 82.dp else 16.dp
    val dockRoom = bottomClearance.value + 4f

    BoxWithConstraints(
        modifier = Modifier
            .fillMaxSize()
            .background(c.bg)
            .statusBarsPadding()
            .navigationBarsPadding()
            .displayCutoutPadding(),
    ) {
        // First pass on the whole area decides the arrangement, which picks the top bar;
        // the second pass below sizes everything in the space that is really left.
        val sideBySide = HubLayout.compute(
            w = maxWidth.value - HUB_PAD_H * 2,
            h = maxHeight.value - 58f,
            pcRatio = mainRatio,
            dockRoom = dockRoom,
        ).sideBySide
        val compactBar = sideBySide && maxHeight < 520.dp

        Column(modifier = Modifier.fillMaxSize().glassBackdropSource(glassBackdrop)) {
            if (sideBySide) {
                WideHubTopBar(
                    state = state,
                    compact = compactBar,
                    qualityPopoverOpen = qualityPopoverOpen,
                    onToggleQualityPopover = { qualityPopoverOpen = !qualityPopoverOpen },
                    onBackToDevices = onBackToDevices,
                    onSelectMonitor = onSelectMonitor,
                    onToggleOrientation = onToggleOrientation,
                )
            } else {
                PortraitHubTopBar(
                    state = state,
                    qualityPopoverOpen = qualityPopoverOpen,
                    onToggleQualityPopover = { qualityPopoverOpen = !qualityPopoverOpen },
                    onBackToDevices = onBackToDevices,
                    onToggleOrientation = onToggleOrientation,
                )
                if (state.monitors.size > 1) {
                    Box(Modifier.fillMaxWidth().padding(top = 2.dp, bottom = 6.dp), contentAlignment = Alignment.Center) {
                        MonitorSpacesPill(
                            monitors = state.monitors,
                            currentIndex = state.currentMonitorIndex,
                            onSelect = onSelectMonitor,
                        )
                    }
                }
            }

            BoxWithConstraints(
                modifier = Modifier
                    .fillMaxWidth()
                    .weight(1f)
                    .padding(start = HUB_PAD_H.dp, end = HUB_PAD_H.dp, top = 4.dp),
            ) {
                val layout = HubLayout.compute(
                    w = maxWidth.value,
                    h = maxHeight.value,
                    pcRatio = mainRatio,
                    dockRoom = dockRoom,
                    forceSide = sideBySide,
                )
                val tvRadius = HUB_TV_RADIUS
                // Layers, bottom to top: window cards, desktop preview, colour halo (drawn
                // above the page, so the cards never hide it). Stacked cards scroll up
                // underneath the preview.
                val listBox = if (layout.sideBySide) layout.list else DpBox(0f, 0f, layout.list.w, layout.list.y + layout.list.h)
                MissionControlWindowsSection(
                    windows = orderedWindows,
                    appIcons = state.appIcons,
                    windowThumbs = state.windowThumbs,
                    appWindowCounts = appWindowCounts,
                    selectedWindowHwnd = state.activeWindow?.hwnd,
                    pinnedAppKey = dockPinnedApp,
                    pulseToken = dockPulseToken,
                    columns = layout.columns,
                    topClearance = if (layout.sideBySide) 4.dp else (layout.list.y + 4f).dp,
                    bottomClearance = bottomClearance,
                    onTapWindow = onFocusWindow,
                    modifier = Modifier
                        .offset(listBox.x.dp, listBox.y.dp)
                        .size(listBox.w.dp, listBox.h.dp),
                )
                HubLiveDesktop(
                    state = state,
                    cornerRadius = tvRadius,
                    onEnterFullscreen = onEnterFullscreen,
                    onReconnect = onReconnect,
                    onSurfaceReady = onSurfaceReady,
                    onSurfaceDestroyed = onSurfaceDestroyed,
                    modifier = Modifier
                        .offset(layout.tv.x.dp, layout.tv.y.dp)
                        .size(layout.tv.w.dp, layout.tv.h.dp)
                        .onGloballyPositioned { tvBounds = it.boundsInRoot() },
                )
            }
        }

        // The halo sits outside the page the dock's glass blurs, so its turning doesn't
        // make the dock blur the whole page again every frame.
        Box(
            Modifier
                .matchParentSize()
                .onGloballyPositioned { haloOrigin = it.positionInRoot() }
                .ambientHalo(halo, { tvBounds.translate(-haloOrigin) }, HUB_TV_RADIUS),
        )

        // True floating Liquid Glass Dock overlay in BOTH Portrait and Landscape/Tablet
        if (showFloatingDock) {
            MacOsFloatingDock(
                appGroups = appGroups,
                appIcons = state.appIcons,
                selectedAppKey = dockPinnedApp,
                backdrop = glassBackdrop,
                compact = compactBar || !sideBySide,
                onSelectAll = { dockPinnedApp = null },
                onTapApp = { key ->
                    if (dockPinnedApp == key) {
                        dockPinnedApp = null
                    } else {
                        dockPinnedApp = key
                        dockPulseToken += 1
                    }
                },
                modifier = Modifier
                    .align(Alignment.BottomCenter)
                    .padding(horizontal = 12.dp)
                    .padding(bottom = if (compactBar) 8.dp else 10.dp),
            )
        }

        // Tap-outside scrim + live Liquid Glass Quality Popover triggered directly from top telemetry pill
        if (qualityPopoverOpen) {
            Box(
                modifier = Modifier
                    .fillMaxSize()
                    .clickable(
                        interactionSource = remember { MutableInteractionSource() },
                        indication = null,
                    ) { qualityPopoverOpen = false },
            )
        }

        AnimatedVisibility(
            visible = qualityPopoverOpen,
            enter = fadeIn(tween(140)) + scaleIn(
                initialScale = 0.92f,
                transformOrigin = TransformOrigin(0.5f, 0f),
                animationSpec = spring(
                    dampingRatio = Spring.DampingRatioNoBouncy,
                    stiffness = Spring.StiffnessMedium,
                ),
            ),
            exit = fadeOut(tween(110)) + scaleOut(
                targetScale = 0.94f,
                transformOrigin = TransformOrigin(0.5f, 0f),
            ),
            modifier = Modifier
                .align(Alignment.TopCenter)
                .padding(top = if (compactBar) 48.dp else 62.dp)
                .padding(horizontal = 12.dp, vertical = 4.dp),
        ) {
            val showRouteOption = state.ownRelayCount > 0 || state.activeDevice?.hasSelfHostedRelay == true
            val compactPopover = config.screenHeightDp < 480
            val maxPopoverH = (config.screenHeightDp.dp - if (compactBar) 64.dp else 96.dp).coerceAtLeast(160.dp)
            val maxPopoverW = (config.screenWidthDp.dp - 24.dp).coerceAtLeast(220.dp)
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
                        .clickable(
                            interactionSource = remember { MutableInteractionSource() },
                            indication = null,
                        ) {}
                        .padding(horizontal = 16.dp, vertical = if (compactPopover) 12.dp else 16.dp),
                ) {
                    QualitySettingsSection(
                        quality = state.quality,
                        onUpdateQuality = onUpdateQuality,
                        showRouteOption = showRouteOption,
                        routeHint = pathKindLabel(state.liveStats.pathKind).let { if (it.isEmpty()) "" else "当前：$it" },
                        compact = compactPopover,
                    )
                }
            }
        }
    }
}

private const val HUB_PAD_H = 14f
private val HUB_TV_RADIUS = 12.dp

@Composable
private fun HubTelemetryPill(
    text: String,
    active: Boolean,
    enabled: Boolean,
    onClick: () -> Unit,
) {
    val c = RcThemeColors.current
    val pillBg = when {
        active -> Color(0x330A84FF)
        c.isDark -> Color(0x29767680)
        else -> Color(0x14767680)
    }
    val fg = if (active) c.accent else c.label2
    Row(
        modifier = Modifier
            .liquidPress(enabled = enabled, pressedScale = 0.94f, onClick = onClick)
            .height(28.dp)
            .clip(RoundedCornerShape(999.dp))
            .background(pillBg)
            .padding(start = 12.dp, end = if (enabled) 8.dp else 12.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(3.dp),
    ) {
        Text(
            text = text,
            color = fg,
            style = TextStyle(
                fontSize = 12.5.sp,
                lineHeight = 15.sp,
                fontWeight = FontWeight.Medium,
                fontFeatureSettings = "tnum",
                platformStyle = PlatformTextStyle(includeFontPadding = false),
            ),
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
        if (enabled) {
            Icon(
                imageVector = Icons.Rounded.KeyboardArrowDown,
                contentDescription = null,
                tint = fg,
                modifier = Modifier.size(16.dp),
            )
        }
    }
}


@Composable
private fun PortraitHubTopBar(
    state: RcUiState,
    qualityPopoverOpen: Boolean,
    onToggleQualityPopover: () -> Unit,
    onBackToDevices: () -> Unit,
    onToggleOrientation: () -> Unit,
) {
    val c = RcThemeColors.current
    val devName = state.activeDevice?.name ?: "Windows"
    val telemetry = formatTelemetry(state, includeResolution = true)
    val isLive = state.sessionPhase == SessionPhase.Connected

    Box(
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(min = 58.dp)
            .padding(horizontal = 10.dp, vertical = 4.dp),
    ) {
        NavBackButton(
            onClick = onBackToDevices,
            modifier = Modifier.align(Alignment.CenterStart),
        )

        Column(
            modifier = Modifier
                .align(Alignment.Center)
                .padding(horizontal = 56.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(5.dp),
            ) {
                if (isLive) {
                    StatusDot(color = c.green, size = 6.dp)
                }
                Text(
                    text = devName,
                    color = c.label,
                    style = TextStyle(
                        fontSize = 17.sp,
                        lineHeight = 20.sp,
                        fontWeight = FontWeight.SemiBold,
                        platformStyle = PlatformTextStyle(includeFontPadding = false),
                    ),
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            HubTelemetryPill(
                text = telemetry,
                active = qualityPopoverOpen,
                enabled = isLive,
                onClick = onToggleQualityPopover,
            )
        }

        FramelessIconButton(
            icon = Icons.Rounded.ScreenRotation,
            contentDescription = "切换横竖屏",
            tint = c.label,
            iconSize = 19.dp,
            onClick = onToggleOrientation,
            modifier = Modifier.align(Alignment.CenterEnd),
        )
    }
}

@Composable
private fun WideHubTopBar(
    state: RcUiState,
    compact: Boolean,
    qualityPopoverOpen: Boolean,
    onToggleQualityPopover: () -> Unit,
    onBackToDevices: () -> Unit,
    onSelectMonitor: (Int) -> Unit,
    onToggleOrientation: () -> Unit,
) {
    val c = RcThemeColors.current
    val devName = state.activeDevice?.name ?: "Windows"
    val telemetry = formatTelemetry(state, includeResolution = true)
    val isLive = state.sessionPhase == SessionPhase.Connected

    Row(
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(min = if (compact) 42.dp else 46.dp)
            .padding(
                start = if (compact) 14.dp else 16.dp,
                end = if (compact) 14.dp else 16.dp,
                top = 2.dp,
                bottom = 2.dp,
            ),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(7.dp),
        ) {
            NavBackButton(
                onClick = onBackToDevices,
                compact = compact,
            )

            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(6.dp),
            ) {
                if (isLive) {
                    StatusDot(color = c.green, size = 6.dp)
                }
                Text(
                    text = devName,
                    color = c.label,
                    style = TextStyle(
                        fontSize = 15.sp,
                        lineHeight = 18.sp,
                        fontWeight = FontWeight.SemiBold,
                        platformStyle = PlatformTextStyle(includeFontPadding = false),
                    ),
                    maxLines = 1,
                )
                HubTelemetryPill(
                    text = telemetry,
                    active = qualityPopoverOpen,
                    enabled = isLive,
                    onClick = onToggleQualityPopover,
                )
            }
        }

        // Inline compact full-pill monitor switcher aligned over the left desktop video
        if (state.monitors.size > 1) {
            MonitorSpacesPill(
                monitors = state.monitors,
                currentIndex = state.currentMonitorIndex,
                onSelect = onSelectMonitor,
            )
        }

        Spacer(modifier = Modifier.weight(1f))

        FramelessIconButton(
            icon = Icons.Rounded.ScreenRotation,
            contentDescription = "切换横竖屏",
            tint = c.label,
            iconSize = 18.dp,
            onClick = onToggleOrientation,
        )
    }
}

/**
 * 1:1 HTML .spaces-pill: compact, centered, full-rounded pill capsule (`999dp`),
 * NOT a stretched full-width bar. Auto-hidden when only 1 monitor exists.
 */
@Composable
private fun MonitorSpacesPill(
    monitors: List<MonitorInfo>,
    currentIndex: Int,
    onSelect: (Int) -> Unit,
    modifier: Modifier = Modifier,
) {
    val c = RcThemeColors.current
    val pillShape = RoundedCornerShape(999.dp)

    Row(
        modifier = modifier
            .clip(pillShape)
            .background(c.segTrack)
            .padding(3.dp)
            .horizontalScroll(rememberScrollState()),
        horizontalArrangement = Arrangement.spacedBy(2.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        monitors.forEach { m ->
            val active = m.index == currentIndex
            val segMod = if (active) {
                Modifier
                    .shadow(
                        elevation = 2.dp,
                        shape = pillShape,
                        ambientColor = Color(0x28000000),
                        spotColor = Color(0x28000000),
                    )
                    .clip(pillShape)
                    .background(c.segThumb)
            } else {
                Modifier.clip(pillShape)
            }

            Row(
                modifier = segMod
                    .height(30.dp)
                    .liquidPress(pressedScale = 0.95f) { onSelect(m.index) }
                    .padding(horizontal = 14.dp),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(4.dp),
            ) {
                Text(
                    text = m.displayTitle,
                    color = if (active) c.label else c.label2,
                    fontSize = 13.sp,
                    fontWeight = FontWeight.Medium,
                    maxLines = 1,
                )
                if (m.shortRes.isNotEmpty()) {
                    Text(
                        text = m.shortRes,
                        color = (if (active) c.label else c.label2).copy(alpha = 0.58f),
                        fontSize = 11.sp,
                        fontWeight = FontWeight.Normal,
                        maxLines = 1,
                    )
                }
            }
        }
    }
}

@Composable
private fun HubLiveDesktop(
    state: RcUiState,
    cornerRadius: Dp,
    onEnterFullscreen: () -> Unit,
    onReconnect: () -> Unit,
    onSurfaceReady: (Surface) -> Unit,
    onSurfaceDestroyed: (Surface) -> Unit,
    modifier: Modifier = Modifier,
) {
    val c = RcThemeColors.current
    val isLive = state.sessionPhase == SessionPhase.Connected
    val shape = RoundedCornerShape(cornerRadius)

    Box(
        modifier = modifier
            .liquidPress(
                enabled = isLive,
                pressedScale = 0.99f,
                onClick = onEnterFullscreen,
            )

    ) {
        Box(
            modifier = Modifier
                .fillMaxSize()
                .clip(shape)
                .background(Color(0xFF111215)),
        ) {
            VideoSurface(
                streamWidth = state.streamMeta.width,
                streamHeight = state.streamMeta.height,
                onSurfaceReady = onSurfaceReady,
                onSurfaceDestroyed = onSurfaceDestroyed,
                cornerRadius = cornerRadius,
                outsideColor = c.bg,
                ambient = true,
                modifier = Modifier.fillMaxSize(),
            )

            // Minimal top-right translucent fullscreen affordance (zero visual clutter over desktop)
            if (isLive && state.hasFirstFrame) {
                Box(
                    modifier = Modifier
                        .align(Alignment.TopEnd)
                        .padding(8.dp)
                        .size(28.dp)
                        .clip(CircleShape)
                        .background(Color(0x7A14151B))
                        .border(0.6.dp, Color(0x33FFFFFF), CircleShape)
                        .liquidPress(pressedScale = 0.90f, onClick = onEnterFullscreen),
                    contentAlignment = Alignment.Center,
                ) {
                    Icon(
                        imageVector = Icons.Rounded.OpenInFull,
                        contentDescription = "进入全屏",
                        tint = Color(0xE6FFFFFF),
                        modifier = Modifier.size(13.5.dp),
                    )
                }
            }

            val showOverlay = !isLive || !state.hasFirstFrame || state.hintCode != 0
            AnimatedVisibility(
                visible = showOverlay,
                enter = fadeIn(),
                exit = fadeOut(),
                modifier = Modifier.fillMaxSize(),
            ) {
                Box(
                    modifier = Modifier
                        .fillMaxSize()
                        .background(Color(0xC80D0D10)),
                    contentAlignment = Alignment.Center,
                ) {
                    Column(
                        horizontalAlignment = Alignment.CenterHorizontally,
                        verticalArrangement = Arrangement.spacedBy(10.dp),
                        modifier = Modifier.padding(horizontal = 24.dp),
                    ) {
                        when (state.sessionPhase) {
                            SessionPhase.Connecting, SessionPhase.WaitingConsent -> {
                                SubtleGlassRing(size = 54.dp) {
                                    Icon(
                                        imageVector = Icons.Outlined.DesktopWindows,
                                        contentDescription = null,
                                        tint = Color(0xCCFFFFFF),
                                        modifier = Modifier.size(20.dp),
                                    )
                                }
                                Text(
                                    text = state.sessionMessage.ifEmpty { "正在连接…" },
                                    color = Color.White,
                                    fontSize = 13.sp,
                                )
                            }

                            SessionPhase.Connected -> {
                                if (state.hintCode != 0) {
                                    Text(
                                        text = state.hintMessage,
                                        color = c.warn,
                                        fontSize = 13.sp,
                                    )
                                } else {
                                    SubtleGlassRing(size = 50.dp) {
                                        StatusDot(color = c.green, size = 7.dp)
                                    }
                                    Text(
                                        text = "正在加载画面…",
                                        color = Color(0xB3FFFFFF),
                                        fontSize = 12.sp,
                                    )
                                }
                            }

                            SessionPhase.Paused -> {
                                Text(
                                    text = state.sessionMessage,
                                    color = c.warn,
                                    fontSize = 13.sp,
                                )
                            }

                            SessionPhase.Disconnected, SessionPhase.Error -> {
                                Text(
                                    text = state.sessionMessage.ifEmpty { "连接已断开" },
                                    color = c.red,
                                    fontSize = 13.sp,
                                )
                                FloatingGlass(
                                    cornerRadius = 999.dp,
                                    darkOverlay = true,
                                    modifier = Modifier.liquidPress(pressedScale = 0.94f, onClick = onReconnect),
                                ) {
                                    Row(
                                        modifier = Modifier.padding(horizontal = 16.dp, vertical = 8.dp),
                                        verticalAlignment = Alignment.CenterVertically,
                                        horizontalArrangement = Arrangement.spacedBy(5.dp),
                                    ) {
                                        Icon(
                                            imageVector = Icons.Rounded.Refresh,
                                            contentDescription = null,
                                            tint = Color.White,
                                            modifier = Modifier.size(15.dp),
                                        )
                                        Text(
                                            text = "重新连接",
                                            color = Color.White,
                                            fontSize = 13.sp,
                                            fontWeight = FontWeight.Medium,
                                        )
                                    }
                                }
                            }

                            SessionPhase.Idle -> {}
                        }
                    }
                }
            }
        }
    }
}

/**
 * A2+ Mission Control window grid — zero redundant section header, zero filter chip row.
 * Tapping an app in the floating Dock floats its windows to index 0 and scrolls smoothly to top.
 */
@Composable
private fun MissionControlWindowsSection(
    windows: List<RunningWindowItem>,
    appIcons: Map<String, ImageBitmap>,
    windowThumbs: Map<Long, ImageBitmap>,
    appWindowCounts: Map<String, Int>,
    selectedWindowHwnd: Long?,
    pinnedAppKey: String?,
    pulseToken: Int,
    columns: Int,
    topClearance: Dp,
    bottomClearance: Dp,
    onTapWindow: (RunningWindowItem) -> Unit,
    modifier: Modifier = Modifier,
) {
    val c = RcThemeColors.current
    val gridState = rememberLazyGridState()
    val compact = false

    LaunchedEffect(pinnedAppKey, pulseToken) {
        if (windows.isNotEmpty()) {
            gridState.animateScrollToItem(0)
        }
    }

    Box(modifier = modifier) {
        if (windows.isEmpty()) {
            Box(
                modifier = Modifier
                    .padding(top = topClearance)
                    .fillMaxWidth()
                    .clip(RoundedCornerShape(15.dp))
                    .background(c.bgCard)
                    .padding(horizontal = 16.dp, vertical = 22.dp),
                contentAlignment = Alignment.Center,
            ) {
                Text(
                    text = "等待同步运行中窗口（点上方画面直接进入桌面）",
                    color = c.label3,
                    fontSize = 12.sp,
                    lineHeight = 18.sp,
                )
            }
        } else {
            LazyVerticalGrid(
                state = gridState,
                columns = GridCells.Fixed(columns),
                modifier = Modifier
                    .fillMaxSize(),
                contentPadding = PaddingValues(top = topClearance, bottom = bottomClearance),
                horizontalArrangement = Arrangement.spacedBy(HubLayout.CARD_GAP.dp),
                verticalArrangement = Arrangement.spacedBy(HubLayout.CARD_GAP.dp),
            ) {
                items(windows, key = { it.id }) { win ->
                    val isPinned = pinnedAppKey != null && win.appKey == pinnedAppKey
                    val isSelected = selectedWindowHwnd != null && win.hwnd == selectedWindowHwnd
                    WindowThumbCard(
                        window = win,
                        realIcon = appIcons[win.appKey],
                        thumb = windowThumbs[win.hwnd],
                        appWindowCount = appWindowCounts[win.appKey] ?: 1,
                        selected = isSelected,
                        pinned = isPinned,
                        pulseToken = if (isPinned) pulseToken else 0,
                        compact = compact,
                        onClick = { onTapWindow(win) },
                        modifier = Modifier.animateItem(
                            placementSpec = spring(
                                dampingRatio = 0.82f,
                                stiffness = Spring.StiffnessMediumLow,
                            ),
                        ),
                    )
                }
            }
        }
    }
}

private fun cleanWindowSubtitle(rawTitle: String, appName: String): String {
    val t = rawTitle.trim()
    val app = appName.trim()
    if (t.isEmpty() || t.equals(app, ignoreCase = true)) return ""
    val suffixes = listOf(
        " - $app",
        " — $app",
        " – $app",
        " | $app",
        " - Visual Studio Code",
        " - Google Chrome",
        " - Microsoft Edge",
        " — Mozilla Firefox",
    )
    var cleaned = t
    for (suffix in suffixes) {
        if (cleaned.endsWith(suffix, ignoreCase = true) && cleaned.length > suffix.length) {
            cleaned = cleaned.dropLast(suffix.length).trim()
            break
        }
    }
    return if (cleaned.equals(app, ignoreCase = true)) "" else cleaned
}

/**
 * A2+ Pure Apple Thumbnail Card:
 * - Zero solid black footer; full-bleed 16:10.5 rounded window thumbnail
 * - Soft app-tinted radial glow backdrop when minimized / no thumbnail yet
 * - Bottom progressive scrim + floating frosted glass pill `[icon] AppName · shortTitle`
 */
@Composable
private fun WindowThumbCard(
    window: RunningWindowItem,
    realIcon: ImageBitmap?,
    thumb: ImageBitmap?,
    appWindowCount: Int,
    selected: Boolean,
    pinned: Boolean,
    pulseToken: Int,
    compact: Boolean,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val c = RcThemeColors.current
    val style = remember(window.appKey) { appVisualFor(window.appKey) }
    val cardRadius = if (compact) 13.dp else 15.dp
    val cardShape = RoundedCornerShape(cardRadius)
    val subtitle = remember(window.title, window.appName) {
        cleanWindowSubtitle(window.title, window.appName)
    }
    val displayApp = window.appName.ifEmpty { window.title.ifEmpty { "窗口" } }

    // Tactile pop + luminous cyan-blue rim flash when floated to top via Dock tap
    val popScale = remember { Animatable(1f) }
    val glowAlpha = remember { Animatable(0f) }

    LaunchedEffect(pinned, pulseToken) {
        if (pinned && pulseToken > 0) {
            coroutineScope {
                launch {
                    popScale.snapTo(0.95f)
                    popScale.animateTo(
                        targetValue = 1.045f,
                        animationSpec = spring(
                            dampingRatio = 0.52f,
                            stiffness = Spring.StiffnessMedium,
                        ),
                    )
                    popScale.animateTo(
                        targetValue = 1f,
                        animationSpec = spring(
                            dampingRatio = Spring.DampingRatioMediumBouncy,
                            stiffness = Spring.StiffnessMediumLow,
                        ),
                    )
                }
                launch {
                    glowAlpha.snapTo(1f)
                    glowAlpha.animateTo(
                        targetValue = 0.45f,
                        animationSpec = tween(durationMillis = 680),
                    )
                }
            }
        } else if (!pinned) {
            glowAlpha.animateTo(0f, animationSpec = tween(220))
        }
    }

    val glow = glowAlpha.value
    val borderWidth = when {
        selected -> 2.dp
        glow > 0.05f -> (0.75f + 1.35f * glow).dp
        else -> 0.75.dp
    }
    val borderColor = when {
        selected -> Color(0xFF0A84FF)
        glow > 0.05f -> Color(0xFF0A84FF).copy(alpha = (0.35f + 0.65f * glow).coerceIn(0f, 1f))
        c.isDark -> Color(0x24FFFFFF)
        else -> Color(0x18000000)
    }

    Column(
        modifier = modifier
            .fillMaxWidth()
            .scale(popScale.value)
            .liquidPress(pressedScale = 0.965f, onClick = onClick),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Box(
            modifier = Modifier
                .fillMaxWidth()
                .aspectRatio(16f / 10.8f)
                .clip(cardShape)
                .background(if (c.isDark) Color(0xFF15171E) else Color(0xFFE9EBEF))
                .border(
                    width = borderWidth,
                    color = borderColor,
                    shape = cardShape,
                ),
        ) {
            // Thumbnail or soft app-tinted radial glow backdrop (minimized / loading)
            if (thumb != null) {
                Image(
                    bitmap = thumb,
                    contentDescription = window.title,
                    contentScale = ContentScale.Crop,
                    alignment = Alignment.TopCenter,
                    modifier = Modifier
                        .fillMaxSize()
                        .alpha(if (window.minimized) 0.62f else 1f),
                )
            } else {
                val baseBg = if (c.isDark) Color(0xFF15171F) else Color(0xFFECEEF3)
                Box(
                    modifier = Modifier
                        .fillMaxSize()
                        .background(baseBg)
                        .background(
                            Brush.radialGradient(
                                colors = listOf(
                                    style.topColor.copy(alpha = if (c.isDark) 0.28f else 0.16f),
                                    Color.Transparent,
                                ),
                                center = Offset(0.5f, 0.38f),
                                radius = 420f,
                            ),
                        ),
                    contentAlignment = Alignment.Center,
                ) {
                    val iconSize = if (compact) 30.dp else 36.dp
                    if (realIcon != null) {
                        Image(
                            bitmap = realIcon,
                            contentDescription = displayApp,
                            contentScale = ContentScale.Fit,
                            modifier = Modifier
                                .padding(bottom = 14.dp)
                                .size(iconSize)
                                .alpha(0.9f),
                        )
                    } else {
                        Box(
                            modifier = Modifier
                                .padding(bottom = 14.dp)
                                .size(iconSize)
                                .clip(RoundedCornerShape(10.dp))
                                .background(
                                    Brush.verticalGradient(listOf(style.topColor, style.bottomColor)),
                                ),
                            contentAlignment = Alignment.Center,
                        ) {
                            Icon(
                                imageVector = style.icon,
                                contentDescription = null,
                                tint = Color.White,
                                modifier = Modifier.size(iconSize * 0.56f),
                            )
                        }
                    }
                }
            }

            // Minimal top-right status pill when minimized
            if (window.minimized) {
                Box(
                    modifier = Modifier
                        .align(Alignment.TopEnd)
                        .padding(6.dp)
                        .clip(RoundedCornerShape(999.dp))
                        .background(Color(0x99121318))
                        .border(0.5.dp, Color(0x28FFFFFF), RoundedCornerShape(999.dp))
                        .padding(horizontal = 6.5.dp, vertical = 2.dp),
                ) {
                    Text(
                        text = "已最小化",
                        color = Color(0xCCFFFFFF),
                        fontSize = 9.5.sp,
                        fontWeight = FontWeight.Medium,
                    )
                }
            }

            // Luminous flash sheen when popped to top from Dock
            if (glow > 0.46f) {
                val sheenAlpha = ((glow - 0.45f) / 0.55f * 0.24f).coerceIn(0f, 0.24f)
                Box(
                    modifier = Modifier
                        .fillMaxSize()
                        .background(
                            Brush.verticalGradient(
                                listOf(
                                    Color(0xFF0A84FF).copy(alpha = sheenAlpha),
                                    Color(0xFF0A84FF).copy(alpha = sheenAlpha * 0.45f),
                                ),
                            ),
                        ),
                )
            }

            // Bottom progressive scrim ensuring crystal-clear frosted pill contrast on light or dark windows
            Box(
                modifier = Modifier
                    .align(Alignment.BottomCenter)
                    .fillMaxWidth()
                    .fillMaxHeight(0.32f)
                    .background(
                        Brush.verticalGradient(
                            0.0f to Color.Transparent,
                            1.0f to Color(0x38000000),
                        ),
                    ),
            )

            // Floating frosted glass pill tag: [16dp icon] AppName · shortTitle
            Row(
                modifier = Modifier
                    .align(Alignment.BottomStart)
                    .padding(horizontal = 7.dp, vertical = 7.dp)
                    .clip(RoundedCornerShape(999.dp))
                    .background(Color(0xAB181A22))
                    .border(0.6.dp, Color(0x30FFFFFF), RoundedCornerShape(999.dp))
                    .padding(start = 5.dp, end = 8.dp, top = 4.dp, bottom = 4.dp),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(5.dp),
            ) {
                if (realIcon != null) {
                    Image(
                        bitmap = realIcon,
                        contentDescription = displayApp,
                        contentScale = ContentScale.Fit,
                        modifier = Modifier
                            .size(16.dp)
                            .clip(RoundedCornerShape(4.dp)),
                    )
                } else {
                    Box(
                        modifier = Modifier
                            .size(16.dp)
                            .clip(RoundedCornerShape(4.dp))
                            .background(Brush.verticalGradient(listOf(style.topColor, style.bottomColor))),
                        contentAlignment = Alignment.Center,
                    ) {
                        Icon(
                            imageVector = style.icon,
                            contentDescription = null,
                            tint = Color.White,
                            modifier = Modifier.size(10.dp),
                        )
                    }
                }

                val pillText = remember(displayApp, subtitle) {
                    buildAnnotatedString {
                        withStyle(SpanStyle(color = Color.White, fontWeight = FontWeight.SemiBold)) {
                            append(displayApp)
                        }
                        if (subtitle.isNotEmpty()) {
                            withStyle(SpanStyle(color = Color(0x66FFFFFF))) {
                                append(" · ")
                            }
                            withStyle(SpanStyle(color = Color(0xD0FFFFFF), fontWeight = FontWeight.Normal)) {
                                append(subtitle)
                            }
                        }
                    }
                }

                Text(
                    text = pillText,
                    fontSize = if (compact) 10.5.sp else 11.sp,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }
    }
}

/**
 * 1:1 HTML .mac-dock: centered floating Liquid Glass dock with "全部" + running app icons.
 */
@Composable
private fun MacOsFloatingDock(
    appGroups: List<DockAppGroup>,
    appIcons: Map<String, ImageBitmap>,
    selectedAppKey: String?,
    backdrop: GlassBackdrop?,
    compact: Boolean,
    onSelectAll: () -> Unit,
    onTapApp: (String) -> Unit,
    modifier: Modifier = Modifier,
) {
    val c = RcThemeColors.current
    val tileSize = if (compact) 38.dp else 48.dp
    val iconRadius = if (compact) 10.dp else 13.dp
    val dockRadius = if (compact) 19.dp else 24.dp

    FloatingGlass(
        cornerRadius = dockRadius,
        darkOverlay = false,
        elevation = 12.dp,
        backdrop = backdrop,
        modifier = modifier,
    ) {
        Row(
            modifier = Modifier.padding(
                start = if (compact) 9.dp else 10.dp,
                end = if (compact) 9.dp else 10.dp,
                top = if (compact) 5.dp else 7.dp,
                bottom = if (compact) 4.dp else 6.dp,
            ),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            // Leftmost "全部" (All windows) icon
            DockItemTile(
                size = tileSize,
                radius = iconRadius,
                label = "全部",
                realIcon = null,
                fallbackIcon = Icons.Rounded.GridView,
                isAllButton = true,
                iconGlyphSize = if (compact) 18.dp else 20.dp,
                badgeCount = 0,
                selected = selectedAppKey == null,
                onClick = onSelectAll,
            )

            if (appGroups.isNotEmpty()) {
                Box(
                    modifier = Modifier
                        .padding(start = 7.dp, end = 7.dp, top = 4.dp, bottom = 5.dp)
                        .width(1.dp)
                        .height(if (compact) 22.dp else 28.dp)
                        .background(c.dockSep),
                )

                Row(
                    modifier = Modifier
                        .weight(1f, fill = false)
                        .horizontalScroll(rememberScrollState()),
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    appGroups.forEach { g ->
                        val style = appVisualFor(g.appKey)
                        DockItemTile(
                            size = tileSize,
                            radius = iconRadius,
                            label = g.appName,
                            realIcon = appIcons[g.appKey],
                            fallbackIcon = style.icon,
                            isAllButton = false,
                            iconGlyphSize = if (compact) 18.dp else 20.dp,
                            badgeCount = if (g.count > 1) g.count else 0,
                            selected = selectedAppKey == g.appKey,
                            onClick = { onTapApp(g.appKey) },
                        )
                    }
                }
            }
        }
    }
}

@Composable
private fun DockItemTile(
    size: Dp,
    radius: Dp,
    label: String,
    realIcon: ImageBitmap?,
    fallbackIcon: ImageVector,
    isAllButton: Boolean,
    iconGlyphSize: Dp,
    badgeCount: Int,
    selected: Boolean,
    onClick: () -> Unit,
) {
    val c = RcThemeColors.current
    // macOS Dock feel: the chosen app lifts a little and its dot lights up; no outlines, no counters.
    val liftY by animateDpAsState(targetValue = if (selected) (-3).dp else 0.dp, label = "dockLift")
    val dotAlpha by animateFloatAsState(targetValue = if (selected) 0.95f else 0.32f, label = "dockDotA")
    val shape = RoundedCornerShape(radius)
    val squircleBacking = if (c.isDark) Color(0x14FFFFFF) else Color(0x0F000000)

    Column(
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.spacedBy(4.dp),
        modifier = Modifier
            .padding(top = 4.dp)
            .offset(y = liftY)
            .liquidPress(pressedScale = 0.90f, onClick = onClick),
    ) {
        Box {
            val tileMod = Modifier
                .size(size)
                .clip(shape)
                .let { base ->
                    when {
                        isAllButton -> base.background(Brush.verticalGradient(listOf(Color(0xFF434752), Color(0xFF2A2D35))))
                        realIcon != null -> base.background(Color.Transparent)
                        else -> base.background(squircleBacking)
                    }
                }
                .then(
                    when {
                        realIcon != null && !isAllButton -> Modifier
                        else -> Modifier.border(
                            width = 0.5.dp,
                            color = if (c.isDark || isAllButton) Color(0x28FFFFFF) else Color(0x18000000),
                            shape = shape,
                        )
                    },
                )

            Box(
                modifier = tileMod,
                contentAlignment = Alignment.Center,
            ) {
                if (realIcon != null) {
                    Image(
                        bitmap = realIcon,
                        contentDescription = label,
                        contentScale = ContentScale.Fit,
                        modifier = Modifier
                            .size(size - 2.dp)
                            .clip(shape),
                    )
                } else {
                    Icon(
                        imageVector = fallbackIcon,
                        contentDescription = label,
                        tint = if (isAllButton) Color.White else c.label2,
                        modifier = Modifier.size(iconGlyphSize),
                    )
                }
            }
        }

        Box(
            modifier = Modifier
                .size(4.dp)
                .clip(CircleShape)
                .background(c.label.copy(alpha = dotAlpha)),
        )
    }
}

private data class AppVisualStyle(
    val topColor: Color,
    val bottomColor: Color,
    val icon: ImageVector,
)

private fun appVisualFor(appKey: String): AppVisualStyle = when (appKey.lowercase()) {
    "vscode", "code" -> AppVisualStyle(Color(0xFF2574B8), Color(0xFF195690), Icons.Rounded.Code)
    "cursor" -> AppVisualStyle(Color(0xFF2E3440), Color(0xFF191D24), Icons.Rounded.Code)
    "chrome" -> AppVisualStyle(Color(0xFF3F4654), Color(0xFF272C36), Icons.Rounded.Language)
    "edge", "msedge" -> AppVisualStyle(Color(0xFF1D6B8A), Color(0xFF154E6B), Icons.Rounded.Language)
    "firefox" -> AppVisualStyle(Color(0xFFC2592B), Color(0xFF8F3816), Icons.Rounded.Language)
    "brave" -> AppVisualStyle(Color(0xFFB94D2B), Color(0xFF873018), Icons.Rounded.Shield)
    "wechat", "weixin" -> AppVisualStyle(Color(0xFF2AA259), Color(0xFF1F7F44), Icons.Rounded.Forum)
    "qq" -> AppVisualStyle(Color(0xFF374151), Color(0xFF1F2937), Icons.Rounded.Forum)
    "telegram" -> AppVisualStyle(Color(0xFF2A87B8), Color(0xFF1B6189), Icons.AutoMirrored.Rounded.Send)
    "discord" -> AppVisualStyle(Color(0xFF535BB8), Color(0xFF3B418B), Icons.Rounded.HeadsetMic)
    "slack" -> AppVisualStyle(Color(0xFF5A3762), Color(0xFF3D2343), Icons.Rounded.Tag)
    "feishu", "lark" -> AppVisualStyle(Color(0xFF2475B8), Color(0xFF185489), Icons.Rounded.Forum)
    "dingtalk" -> AppVisualStyle(Color(0xFF2276BA), Color(0xFF16558A), Icons.Rounded.Forum)
    "onenote" -> AppVisualStyle(Color(0xFF6D3B8E), Color(0xFF4F286A), Icons.Rounded.Description)
    "notion" -> AppVisualStyle(Color(0xFF383A40), Color(0xFF202226), Icons.Rounded.Description)
    "obsidian" -> AppVisualStyle(Color(0xFF60439B), Color(0xFF412B70), Icons.Rounded.Description)
    "term", "terminal", "windowsterminal", "wt", "pwsh", "powershell", "cmd" ->
        AppVisualStyle(Color(0xFF2C2D31), Color(0xFF191A1D), Icons.Rounded.Terminal)
    "folder", "explorer" -> AppVisualStyle(Color(0xFF4B89D8), Color(0xFF3068B5), Icons.Rounded.Folder)
    "music", "cloudmusic" -> AppVisualStyle(Color(0xFFB83B3B), Color(0xFF8B2828), Icons.Rounded.MusicNote)
    "spotify" -> AppVisualStyle(Color(0xFF26934B), Color(0xFF196834), Icons.Rounded.MusicNote)
    "qqmusic" -> AppVisualStyle(Color(0xFF2B8C59), Color(0xFF1C663F), Icons.Rounded.MusicNote)
    "winword" -> AppVisualStyle(Color(0xFF285EA8), Color(0xFF1B427A), Icons.Rounded.Description)
    "excel" -> AppVisualStyle(Color(0xFF277B4A), Color(0xFF1A5733), Icons.Rounded.TableChart)
    "powerpnt" -> AppVisualStyle(Color(0xFFB84E32), Color(0xFF87331D), Icons.Rounded.Slideshow)
    "notepad", "notepad++" -> AppVisualStyle(Color(0xFF3B6682), Color(0xFF26475D), Icons.Rounded.EditNote)
    "steam" -> AppVisualStyle(Color(0xFF2A3E59), Color(0xFF192638), Icons.Rounded.SportsEsports)
    "v2rayn", "clash" -> AppVisualStyle(Color(0xFF34567A), Color(0xFF213A55), Icons.Rounded.Public)
    "remote-control" -> AppVisualStyle(Color(0xFF2F5E9E), Color(0xFF1E4172), Icons.Outlined.DesktopWindows)
    else -> AppVisualStyle(Color(0xFF434752), Color(0xFF2A2D35), Icons.Rounded.WebAsset)
}

private fun formatTelemetry(state: RcUiState, includeResolution: Boolean): String {
    val stats = state.liveStats
    return when (state.sessionPhase) {
        SessionPhase.Connected -> buildString {
            if (stats.fps > 0) append("${stats.fps} fps")
            if (stats.rttMs > 0) {
                if (isNotEmpty()) append(" · ")
                append("${stats.rttMs} ms")
            }
            if (includeResolution) {
                val curMon = state.monitors.getOrNull(state.currentMonitorIndex)
                val res = curMon?.shortRes?.takeIf { it.isNotEmpty() }
                    ?: state.streamMeta.height.takeIf { it > 0 }?.let { "${it}p" }
                if (res != null) {
                    if (isNotEmpty()) append(" · ")
                    append(res)
                }
            }
            if (isEmpty()) append("已连接")
        }
        SessionPhase.WaitingConsent -> "等待电脑允许…"
        SessionPhase.Connecting -> "连接中…"
        SessionPhase.Paused -> "等待直连恢复…"
        SessionPhase.Disconnected -> "已断开"
        SessionPhase.Error -> "连接失败"
        SessionPhase.Idle -> ""
    }
}
