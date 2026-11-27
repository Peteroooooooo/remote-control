package com.rc.android

import android.content.pm.ActivityInfo
import android.content.res.Configuration
import android.os.Bundle
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.WindowManager
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.compose.BackHandler
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.viewModels
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.Spring
import androidx.compose.animation.core.spring
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.scaleIn
import androidx.compose.animation.scaleOut
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.animation.slideOutVertically
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.background
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.ime
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import com.rc.android.model.AppLayer
import com.rc.android.state.RcViewModel
import com.rc.android.ui.DevicesScreen
import com.rc.android.ui.FloatingGlass
import com.rc.android.ui.FullscreenScreen
import com.rc.android.ui.HubScreen
import com.rc.android.ui.RcTheme
import com.rc.android.ui.RcThemeColors
import com.rc.android.ui.SettingsScreen
import com.rc.android.ui.liquidPress

class MainActivity : ComponentActivity() {
    private val vm: RcViewModel by viewModels()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge(
            statusBarStyle = SystemBarStyle.auto(
                android.graphics.Color.TRANSPARENT,
                android.graphics.Color.TRANSPARENT,
            ),
            navigationBarStyle = SystemBarStyle.auto(
                android.graphics.Color.TRANSPARENT,
                android.graphics.Color.TRANSPARENT,
            ),
        )
        vm.ensureInitialized(this)

        setContent {
            val state by vm.ui.collectAsState()
            val updater = remember { vm.updater(this@MainActivity) }
            val updatePhase by updater.phase.collectAsState()
            var settingsOpen by remember { mutableStateOf(false) }
            val config = LocalConfiguration.current
            val isDark = true
            var fullscreenMenuOpen by remember { mutableStateOf(false) }
            var lastHubBackMs by remember { mutableLongStateOf(0L) }

            LaunchedEffect(state.layer) {
                if (state.layer == AppLayer.Devices) {
                    requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED
                }
            }

            LaunchedEffect(state.layer, config.orientation, config.smallestScreenWidthDp, isDark) {
                applySystemBars(state.layer, isDark)
                if (state.layer != AppLayer.Fullscreen) {
                    fullscreenMenuOpen = false
                }
                if (state.layer == AppLayer.Devices) {
                    window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
                } else {
                    window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
                }
            }

            BackHandler(enabled = state.layer != AppLayer.Devices) {
                when (state.layer) {
                    AppLayer.Fullscreen -> {
                        if (fullscreenMenuOpen) {
                            fullscreenMenuOpen = false
                        } else {
                            vm.returnToHub()
                        }
                    }
                    AppLayer.Hub -> {
                        val now = System.currentTimeMillis()
                        if (now - lastHubBackMs <= 2_500L) {
                            lastHubBackMs = 0L
                            vm.disconnectAndReturnToDevices()
                        } else {
                            lastHubBackMs = now
                            vm.showToast("再滑一次断开并返回电脑列表")
                        }
                    }
                    AppLayer.Devices -> {}
                }
            }

            BackHandler(enabled = settingsOpen && state.layer == AppLayer.Devices) {
                settingsOpen = false
            }

            RcTheme(darkTheme = isDark) {
                val c = RcThemeColors.current
                LaunchedEffect(config.orientation, config.screenWidthDp, config.screenHeightDp) {
                    val (w, h) = currentScreenPixels()
                    if (w > 0 && h > 0) {
                        vm.updateViewport(w, h, imeVisible = false)
                    }
                }
                Box(
                    modifier = Modifier
                        .fillMaxSize()
                        .background(c.bg),
                ) {
                    AnimatedContent(
                        targetState = state.layer,
                        transitionSpec = {
                            val forward = targetState.ordinal > initialState.ordinal
                            val enter = fadeIn(tween(190)) + scaleIn(
                                initialScale = if (forward) 0.968f else 1.022f,
                                animationSpec = spring(
                                    dampingRatio = Spring.DampingRatioNoBouncy,
                                    stiffness = Spring.StiffnessMediumLow,
                                ),
                            )
                            val exit = fadeOut(tween(145)) + scaleOut(
                                targetScale = if (forward) 1.022f else 0.972f,
                                animationSpec = tween(145),
                            )
                            enter togetherWith exit
                        },
                        label = "layerTransition",
                    ) { layer ->
                        when (layer) {
                            AppLayer.Devices -> Box(Modifier.fillMaxSize()) {
                                DevicesScreen(
                                    state = state,
                                    onOpenSettings = { settingsOpen = true },
                                    onRefreshScan = { vm.triggerScan(this@MainActivity) },
                                    onToggleOrientation = { toggleOrientation() },
                                    onPasteCode = { vm.pasteCode(it) },
                                    onClearPasteFeedback = { vm.clearPasteFeedback() },
                                    onSelectDevice = { dev ->
                                        val (w, h) = currentScreenPixels()
                                        vm.connectToDevice(dev, w, h)
                                    },
                                    onRememberDevice = { vm.rememberDevice(it.id) },
                                    onAgreeRemember = { vm.agreeRemember(it.id) },
                                    onRefuseRemember = { vm.refuseRemember(it.id) },
                                    onCancelRemember = { vm.cancelRemember(it.id) },
                                    onForgetDevice = { vm.forgetDevice(it.id) },
                                    onRetryDevice = {
                                        vm.retryDevice(it.id)
                                        vm.showToast("正在重新检测连接…")
                                    },
                                )
                                AnimatedVisibility(
                                    visible = settingsOpen,
                                    enter = fadeIn(tween(180)) + slideInHorizontally(tween(240)) { it / 3 },
                                    exit = fadeOut(tween(150)) + slideOutHorizontally(tween(200)) { it / 3 },
                                ) {
                                    SettingsScreen(
                                        customName = state.customName,
                                        modelName = state.modelName,
                                        maxNameLen = RcViewModel.MAX_NAME_LEN,
                                        onSetName = { vm.setDeviceName(it) },
                                        version = updater.currentVersion,
                                        update = updatePhase,
                                        onCheckUpdate = { updater.check() },
                                        onUpdateNow = { updater.update() },
                                        onRetryUpdate = { updater.retry() },
                                        onInstall = { updater.install() },
                                        onBack = { settingsOpen = false },
                                    )
                                }
                            }

                            AppLayer.Hub -> {
                                HubScreen(
                                    state = state,
                                    onBackToDevices = { vm.disconnectAndReturnToDevices() },
                                    onEnterFullscreen = { vm.enterFullscreen() },
                                    onFocusWindow = { vm.focusWindow(it) },
                                    onSelectMonitor = { vm.selectMonitor(it) },
                                    onUpdateQuality = { vm.setQuality(it) },
                                    onReconnect = {
                                        val (w, h) = currentScreenPixels()
                                        vm.reconnectActive(w, h)
                                    },
                                    onToggleOrientation = { toggleOrientation() },
                                    onSurfaceReady = { vm.attachSurface(it) },
                                    onSurfaceDestroyed = { vm.detachSurface(it) },
                                )
                            }

                            AppLayer.Fullscreen -> {
                                FullscreenScreen(
                                    state = state,
                                    menuExpanded = fullscreenMenuOpen,
                                    onMenuExpandedChange = { fullscreenMenuOpen = it },
                                    onReturnToHub = { vm.returnToHub() },
                                    onDisconnect = { vm.disconnectAndReturnToDevices() },
                                    onToggleOrientation = { toggleOrientation() },
                                    onSetTouchMode = { vm.setTouchMode(it) },
                                    onUpdateQuality = { vm.setQuality(it) },
                                    onRequestKeyframe = { vm.requestKeyframe() },
                                    onMouseMove = { x, y -> vm.sendMouseMove(x, y) },
                                    onMouseButton = { b, d -> vm.sendMouseButton(b, d) },
                                    onTouch = { id, phase, x, y -> vm.sendTouch(id, phase, x, y) },
                                    onTouchpad = { phase, dx, dy, scale -> vm.sendTouchpad(phase, dx, dy, scale) },
                                    onPen = { phase, x, y, p, tx, ty, fl -> vm.sendPen(phase, x, y, p, tx, ty, fl) },
                                    onWheel = { d, h -> vm.sendWheel(d, h) },
                                    onSendText = { vm.sendText(it) },
                                    onSendKey = { scan, down -> vm.sendKey(scan, down) },
                                    onTapKey = { vm.tapKey(it) },
                                    onTapChord = { m, k -> vm.tapChord(m, k) },
                                    onShowToast = { vm.showToast(it) },
                                    onSurfaceReady = { vm.attachSurface(it) },
                                    onSurfaceDestroyed = { vm.detachSurface(it) },
                                )
                            }
                        }
                    }

                    val incomingAsk = if (state.layer == AppLayer.Hub) {
                        state.devices.firstOrNull { it.ask == "in" }
                    } else {
                        null
                    }
                    AnimatedVisibility(
                        visible = incomingAsk != null,
                        enter = fadeIn(tween(180)) + slideInVertically(initialOffsetY = { -it / 2 }),
                        exit = fadeOut(tween(150)) + slideOutVertically(targetOffsetY = { -it / 2 }),
                        modifier = Modifier
                            .align(Alignment.TopCenter)
                            .statusBarsPadding()
                            .padding(horizontal = 14.dp, vertical = 10.dp),
                    ) {
                        val req = incomingAsk
                        if (req != null) {
                            FloatingGlass(
                                cornerRadius = 18.dp,
                                darkOverlay = false,
                                elevation = 12.dp,
                                modifier = Modifier.widthIn(max = 460.dp),
                            ) {
                                Row(
                                    modifier = Modifier
                                        .fillMaxWidth()
                                        .padding(horizontal = 14.dp, vertical = 10.dp),
                                    verticalAlignment = Alignment.CenterVertically,
                                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                                ) {
                                    Text(
                                        text = "「${req.name}」想记住这台手机",
                                        color = c.label,
                                        fontSize = 13.sp,
                                        fontWeight = FontWeight.Medium,
                                        maxLines = 1,
                                        overflow = TextOverflow.Ellipsis,
                                        modifier = Modifier.weight(1f),
                                    )
                                    Box(
                                        modifier = Modifier
                                            .clip(RoundedCornerShape(999.dp))
                                            .background(c.bgField)
                                            .liquidPress(pressedScale = 0.94f) { vm.refuseRemember(req.id) }
                                            .padding(horizontal = 10.dp, vertical = 5.dp),
                                    ) {
                                        Text(
                                            text = "拒绝",
                                            color = c.label2,
                                            fontSize = 12.sp,
                                            fontWeight = FontWeight.Medium,
                                        )
                                    }
                                    Box(
                                        modifier = Modifier
                                            .clip(RoundedCornerShape(999.dp))
                                            .background(c.accent)
                                            .liquidPress(pressedScale = 0.94f) { vm.agreeRemember(req.id) }
                                            .padding(horizontal = 10.dp, vertical = 5.dp),
                                    ) {
                                        Text(
                                            text = "同意",
                                            color = Color.White,
                                            fontSize = 12.sp,
                                            fontWeight = FontWeight.Medium,
                                        )
                                    }
                                }
                            }
                        }
                    }

                    // Floating Liquid Glass Toast
                    AnimatedVisibility(
                        visible = state.toastMessage != null,
                        enter = fadeIn(tween(160)) + slideInVertically(
                            initialOffsetY = { it / 2 },
                            animationSpec = spring(
                                dampingRatio = Spring.DampingRatioNoBouncy,
                                stiffness = Spring.StiffnessMedium,
                            ),
                        ),
                        exit = fadeOut(tween(140)) + slideOutVertically(targetOffsetY = { it / 2 }),
                        modifier = Modifier
                            .align(Alignment.BottomCenter)
                            .navigationBarsPadding()
                            .padding(bottom = 26.dp),
                    ) {
                        FloatingGlass(
                            cornerRadius = 999.dp,
                            darkOverlay = false,
                            elevation = 10.dp,
                        ) {
                            Text(
                                text = state.toastMessage.orEmpty(),
                                color = c.label,
                                fontSize = 13.sp,
                                fontWeight = FontWeight.Medium,
                                modifier = Modifier.padding(horizontal = 18.dp, vertical = 10.dp),
                            )
                        }
                    }
                }
            }
        }
    }

    override fun onResume() {
        super.onResume()
        vm.triggerScan(this)
        vm.updater(this).onResume()
        applySystemBars(vm.ui.value.layer, true)
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) {
            applySystemBars(vm.ui.value.layer, true)
        } else {
            vm.releaseAllInput()
        }
    }

    override fun dispatchGenericMotionEvent(ev: MotionEvent): Boolean {
        if (vm.ui.value.layer == AppLayer.Fullscreen && ev.isFromSource(InputDevice.SOURCE_MOUSE)) {
            when (ev.actionMasked) {
                MotionEvent.ACTION_SCROLL -> {
                    val v = (ev.getAxisValue(MotionEvent.AXIS_VSCROLL) * 120f).toInt()
                    val h = (ev.getAxisValue(MotionEvent.AXIS_HSCROLL) * 120f).toInt()
                    if (v != 0) vm.sendWheel(v, horizontal = false)
                    if (h != 0) vm.sendWheel(h, horizontal = true)
                    return true
                }
                MotionEvent.ACTION_HOVER_MOVE -> {
                    val rx = ev.getAxisValue(MotionEvent.AXIS_RELATIVE_X)
                    val ry = ev.getAxisValue(MotionEvent.AXIS_RELATIVE_Y)
                    if (rx != 0f || ry != 0f) {
                        vm.sendMouseDelta(rx.toInt(), ry.toInt())
                        return true
                    }
                }
            }
        }
        return super.dispatchGenericMotionEvent(ev)
    }

    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        if (vm.ui.value.layer == AppLayer.Fullscreen &&
            event.device != null &&
            !event.device.isVirtual &&
            event.keyCode != KeyEvent.KEYCODE_BACK &&
            event.keyCode != KeyEvent.KEYCODE_VOLUME_UP &&
            event.keyCode != KeyEvent.KEYCODE_VOLUME_DOWN
        ) {
            val scan = androidKeyToWindowsScan(event.keyCode)
            if (scan != 0) {
                when (event.action) {
                    KeyEvent.ACTION_DOWN -> {
                        vm.sendKey(scan, true)
                        return true
                    }
                    KeyEvent.ACTION_UP -> {
                        vm.sendKey(scan, false)
                        return true
                    }
                }
            }
        }
        return super.dispatchKeyEvent(event)
    }

    private fun androidKeyToWindowsScan(keyCode: Int): Int = when (keyCode) {
        KeyEvent.KEYCODE_ESCAPE -> 0x01
        KeyEvent.KEYCODE_1 -> 0x02
        KeyEvent.KEYCODE_2 -> 0x03
        KeyEvent.KEYCODE_3 -> 0x04
        KeyEvent.KEYCODE_4 -> 0x05
        KeyEvent.KEYCODE_5 -> 0x06
        KeyEvent.KEYCODE_6 -> 0x07
        KeyEvent.KEYCODE_7 -> 0x08
        KeyEvent.KEYCODE_8 -> 0x09
        KeyEvent.KEYCODE_9 -> 0x0A
        KeyEvent.KEYCODE_0 -> 0x0B
        KeyEvent.KEYCODE_MINUS -> 0x0C
        KeyEvent.KEYCODE_EQUALS -> 0x0D
        KeyEvent.KEYCODE_DEL -> 0x0E
        KeyEvent.KEYCODE_TAB -> 0x0F
        KeyEvent.KEYCODE_Q -> 0x10
        KeyEvent.KEYCODE_W -> 0x11
        KeyEvent.KEYCODE_E -> 0x12
        KeyEvent.KEYCODE_R -> 0x13
        KeyEvent.KEYCODE_T -> 0x14
        KeyEvent.KEYCODE_Y -> 0x15
        KeyEvent.KEYCODE_U -> 0x16
        KeyEvent.KEYCODE_I -> 0x17
        KeyEvent.KEYCODE_O -> 0x18
        KeyEvent.KEYCODE_P -> 0x19
        KeyEvent.KEYCODE_LEFT_BRACKET -> 0x1A
        KeyEvent.KEYCODE_RIGHT_BRACKET -> 0x1B
        KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER -> 0x1C
        KeyEvent.KEYCODE_CTRL_LEFT -> 0x1D
        KeyEvent.KEYCODE_A -> 0x1E
        KeyEvent.KEYCODE_S -> 0x1F
        KeyEvent.KEYCODE_D -> 0x20
        KeyEvent.KEYCODE_F -> 0x21
        KeyEvent.KEYCODE_G -> 0x22
        KeyEvent.KEYCODE_H -> 0x23
        KeyEvent.KEYCODE_J -> 0x24
        KeyEvent.KEYCODE_K -> 0x25
        KeyEvent.KEYCODE_L -> 0x26
        KeyEvent.KEYCODE_SEMICOLON -> 0x27
        KeyEvent.KEYCODE_APOSTROPHE -> 0x28
        KeyEvent.KEYCODE_GRAVE -> 0x29
        KeyEvent.KEYCODE_SHIFT_LEFT -> 0x2A
        KeyEvent.KEYCODE_BACKSLASH -> 0x2B
        KeyEvent.KEYCODE_Z -> 0x2C
        KeyEvent.KEYCODE_X -> 0x2D
        KeyEvent.KEYCODE_C -> 0x2E
        KeyEvent.KEYCODE_V -> 0x2F
        KeyEvent.KEYCODE_B -> 0x30
        KeyEvent.KEYCODE_N -> 0x31
        KeyEvent.KEYCODE_M -> 0x32
        KeyEvent.KEYCODE_COMMA -> 0x33
        KeyEvent.KEYCODE_PERIOD -> 0x34
        KeyEvent.KEYCODE_SLASH -> 0x35
        KeyEvent.KEYCODE_SHIFT_RIGHT -> 0x36
        KeyEvent.KEYCODE_ALT_LEFT -> 0x38
        KeyEvent.KEYCODE_SPACE -> 0x39
        KeyEvent.KEYCODE_CAPS_LOCK -> 0x3A
        KeyEvent.KEYCODE_F1 -> 0x3B
        KeyEvent.KEYCODE_F2 -> 0x3C
        KeyEvent.KEYCODE_F3 -> 0x3D
        KeyEvent.KEYCODE_F4 -> 0x3E
        KeyEvent.KEYCODE_F5 -> 0x3F
        KeyEvent.KEYCODE_F6 -> 0x40
        KeyEvent.KEYCODE_F7 -> 0x41
        KeyEvent.KEYCODE_F8 -> 0x42
        KeyEvent.KEYCODE_F9 -> 0x43
        KeyEvent.KEYCODE_F10 -> 0x44
        KeyEvent.KEYCODE_F11 -> 0x57
        KeyEvent.KEYCODE_F12 -> 0x58
        KeyEvent.KEYCODE_CTRL_RIGHT -> 0xE01D
        KeyEvent.KEYCODE_ALT_RIGHT -> 0xE038
        KeyEvent.KEYCODE_META_LEFT -> 0xE05B
        KeyEvent.KEYCODE_DPAD_UP -> 0xE048
        KeyEvent.KEYCODE_DPAD_DOWN -> 0xE050
        KeyEvent.KEYCODE_DPAD_LEFT -> 0xE04B
        KeyEvent.KEYCODE_DPAD_RIGHT -> 0xE04D
        KeyEvent.KEYCODE_MOVE_HOME -> 0xE047
        KeyEvent.KEYCODE_MOVE_END -> 0xE04F
        KeyEvent.KEYCODE_PAGE_UP -> 0xE049
        KeyEvent.KEYCODE_PAGE_DOWN -> 0xE051
        KeyEvent.KEYCODE_INSERT -> 0xE052
        KeyEvent.KEYCODE_FORWARD_DEL -> 0xE053
        else -> 0
    }

    private fun isNightModeActive(): Boolean {
        val uiMode = resources.configuration.uiMode and Configuration.UI_MODE_NIGHT_MASK
        return uiMode == Configuration.UI_MODE_NIGHT_YES
    }

    private fun currentScreenPixels(): Pair<Int, Int> {
        return if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.R) {
            val bounds = windowManager.currentWindowMetrics.bounds
            bounds.width() to bounds.height()
        } else {
            val dm = resources.displayMetrics
            dm.widthPixels to dm.heightPixels
        }
    }

    private fun toggleOrientation() {
        val isLand = resources.configuration.orientation == Configuration.ORIENTATION_LANDSCAPE
        requestedOrientation = if (isLand) {
            ActivityInfo.SCREEN_ORIENTATION_SENSOR_PORTRAIT
        } else {
            ActivityInfo.SCREEN_ORIENTATION_SENSOR_LANDSCAPE
        }
    }

    private fun applySystemBars(layer: AppLayer, isDark: Boolean) {
        val controller = WindowInsetsControllerCompat(window, window.decorView)
        val lightIcons = when (layer) {
            AppLayer.Fullscreen -> false
            else -> !isDark
        }
        controller.isAppearanceLightStatusBars = lightIcons
        controller.isAppearanceLightNavigationBars = lightIcons

        val isLandscape = resources.configuration.orientation == Configuration.ORIENTATION_LANDSCAPE

        when {
            // Fullscreen on all devices: hide both status & navigation bars
            layer == AppLayer.Fullscreen -> {
                controller.systemBarsBehavior =
                    WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
                controller.hide(WindowInsetsCompat.Type.systemBars())
            }
            // Landscape (phones and tablets): vertical space is scarce, so hide the status bar
            isLandscape -> {
                controller.systemBarsBehavior =
                    WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
                controller.hide(WindowInsetsCompat.Type.statusBars())
                controller.show(WindowInsetsCompat.Type.navigationBars())
            }
            // Portrait: show system bars normally
            else -> {
                controller.show(WindowInsetsCompat.Type.systemBars())
            }
        }
    }
}
