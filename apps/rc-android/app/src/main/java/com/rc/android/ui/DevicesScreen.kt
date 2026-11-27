package com.rc.android.ui

import androidx.activity.compose.BackHandler
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.Spring
import androidx.compose.animation.core.spring
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutVertically
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.IntrinsicSize
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.rounded.KeyboardArrowRight
import androidx.compose.material.icons.outlined.DesktopWindows
import androidx.compose.material.icons.rounded.Add
import androidx.compose.material.icons.rounded.Close
import androidx.compose.material.icons.rounded.MoreHoriz
import androidx.compose.material.icons.rounded.Refresh
import androidx.compose.material.icons.rounded.ScreenRotation
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.shadow
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.rc.android.R
import com.rc.android.model.DeviceItem
import com.rc.android.model.RcUiState
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

@Composable
fun DevicesScreen(
    state: RcUiState,
    onRefreshScan: () -> Unit,
    onToggleOrientation: () -> Unit,
    onPasteCode: (String) -> Unit,
    onClearPasteFeedback: () -> Unit,
    onSelectDevice: (DeviceItem) -> Unit,
    onRememberDevice: (DeviceItem) -> Unit,
    onAgreeRemember: (DeviceItem) -> Unit,
    onRefuseRemember: (DeviceItem) -> Unit,
    onCancelRemember: (DeviceItem) -> Unit,
    onForgetDevice: (DeviceItem) -> Unit,
    onRetryDevice: (DeviceItem) -> Unit,
    onOpenSettings: () -> Unit,
) {
    val c = RcThemeColors.current
    val clipboard = LocalClipboardManager.current
    val scope = rememberCoroutineScope()

    var addSheetOpen by remember { mutableStateOf(false) }
    var menuDeviceId by remember { mutableStateOf<String?>(null) }
    var codeText by remember { mutableStateOf("") }
    val spin = remember { Animatable(0f) }
    val menuDevice = remember(menuDeviceId, state.devices) {
        menuDeviceId?.let { id -> state.devices.find { it.id == id } }
    }

    LaunchedEffect(state.pasteFeedback) {
        val fb = state.pasteFeedback
        if (fb != null && fb.ok && addSheetOpen) {
            delay(850L)
            addSheetOpen = false
            codeText = ""
            onClearPasteFeedback()
        }
    }

    BackHandler(enabled = addSheetOpen || menuDevice != null) {
        if (menuDevice != null) {
            menuDeviceId = null
        } else {
            addSheetOpen = false
            onClearPasteFeedback()
        }
    }

    val onlineDevices = remember(state.devices) {
        state.devices.filter { it.isOnline || it.status == "checking" || it.ask == "in" }
    }
    val offlineDevices = remember(state.devices) {
        state.devices.filter { !it.isOnline && it.status != "checking" && it.ask != "in" }
    }

    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(c.bg),
    ) {
        Column(
            modifier = Modifier
                .fillMaxSize()
                .statusBarsPadding()
                .navigationBarsPadding(),
        ) {
            // 1:1 HTML .nav-bar: height 44dp, 16sp SemiBold brand, frameless Refresh + Rotate + Plus icons
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .height(60.dp)
                    .padding(start = 20.dp, end = 10.dp, top = 6.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Row(
                    modifier = Modifier
                        .weight(1f)
                        .padding(end = 6.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Image(
                        painter = painterResource(R.drawable.ic_brand),
                        contentDescription = null,
                        modifier = Modifier.size(18.dp),
                    )
                    Spacer(Modifier.width(8.dp))
                    Text(
                        text = "Remote Control",
                        color = c.label,
                        fontSize = 15.sp,
                        fontWeight = FontWeight.Medium,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }

                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(4.dp),
                ) {
                    // Frameless Refresh icon
                    Box(
                        modifier = Modifier
                            .size(38.dp)
                            .clip(CircleShape)
                            .liquidPress(pressedScale = 0.88f) {
                                scope.launch {
                                    spin.snapTo(0f)
                                    spin.animateTo(360f, tween(500, easing = FastOutSlowInEasing))
                                }
                                onRefreshScan()
                            },
                        contentAlignment = Alignment.Center,
                    ) {
                        Icon(
                            imageVector = Icons.Rounded.Refresh,
                            contentDescription = "扫描局域网",
                            tint = c.label,
                            modifier = Modifier
                                .size(20.dp)
                                .graphicsLayer { rotationZ = spin.value },
                        )
                    }

                    // Frameless + icon
                    FramelessIconButton(
                        icon = Icons.Rounded.Add,
                        contentDescription = "粘贴连接码或中转码",
                        tint = c.label,
                        iconSize = 22.dp,
                        onClick = {
                            onClearPasteFeedback()
                            addSheetOpen = true
                        },
                    )

                    Box(
                        modifier = Modifier
                            .size(36.dp)
                            .clip(CircleShape)
                            .semantics { contentDescription = "设置" }
                            .liquidPress(pressedScale = 0.88f, onClick = onOpenSettings),
                        contentAlignment = Alignment.Center,
                    ) {
                        GearIcon(tint = c.label, size = 18.5.dp, strokeWidth = 1.5.dp)
                    }
                }
            }

            Box(
                modifier = Modifier.fillMaxSize(),
                contentAlignment = Alignment.TopCenter,
            ) {
                LazyColumn(
                    modifier = Modifier
                        .fillMaxSize()
                        .widthIn(max = 640.dp),
                    contentPadding = PaddingValues(
                        start = 16.dp,
                        end = 16.dp,
                        top = 10.dp,
                        bottom = 32.dp,
                    ),
                    verticalArrangement = Arrangement.spacedBy(20.dp),
                ) {
                    if (state.initError != null) {
                        item(key = "init_error") {
                            Box(
                                modifier = Modifier
                                    .fillMaxWidth()
                                    .clip(RoundedCornerShape(14.dp))
                                    .background(c.bgGrouped)
                                    .padding(horizontal = 15.dp, vertical = 12.dp),
                            ) {
                                Text(
                                    text = "初始化失败：${state.initError}",
                                    color = c.red,
                                    fontSize = 13.sp,
                                )
                            }
                        }
                    }

                    if (onlineDevices.isNotEmpty()) {
                        item(key = "online_group") {
                            AppleGroupedSection(
                                title = "可用电脑",
                                devices = onlineDevices,
                                onSelectDevice = onSelectDevice,
                                onRememberDevice = onRememberDevice,
                                onAgreeRemember = onAgreeRemember,
                                onRefuseRemember = onRefuseRemember,
                                onCancelRemember = onCancelRemember,
                                onRetryDevice = onRetryDevice,
                                onOpenMenu = { menuDeviceId = it.id },
                            )
                        }
                    }

                    if (offlineDevices.isNotEmpty()) {
                        item(key = "offline_group") {
                            AppleGroupedSection(
                                title = "离线",
                                devices = offlineDevices,
                                onSelectDevice = onSelectDevice,
                                onRememberDevice = onRememberDevice,
                                onAgreeRemember = onAgreeRemember,
                                onRefuseRemember = onRefuseRemember,
                                onCancelRemember = onCancelRemember,
                                onRetryDevice = onRetryDevice,
                                onOpenMenu = { menuDeviceId = it.id },
                            )
                        }
                    }

                    if (state.devices.isEmpty()) {
                        item(key = "empty_scanning") {
                            Column(
                                modifier = Modifier.fillMaxWidth(),
                                verticalArrangement = Arrangement.spacedBy(7.dp),
                            ) {
                                Text(
                                    text = "可用电脑",
                                    color = c.label2,
                                    fontSize = 12.sp,
                                    modifier = Modifier.padding(start = 12.dp),
                                )
                                Box(
                                    modifier = Modifier
                                        .fillMaxWidth()
                                        .clip(RoundedCornerShape(14.dp))
                                        .background(c.bgGrouped)
                                        .padding(horizontal = 16.dp, vertical = 22.dp),
                                    contentAlignment = Alignment.Center,
                                ) {
                                    Row(
                                        verticalAlignment = Alignment.CenterVertically,
                                        horizontalArrangement = Arrangement.spacedBy(8.dp),
                                    ) {
                                        StatusDot(color = c.accent, breathing = true, size = 6.dp)
                                        Text(
                                            text = "正在扫描局域网内的电脑…",
                                            color = c.label2,
                                            fontSize = 14.sp,
                                        )
                                    }
                                }
                            }
                        }
                    }

                    // Clean concise footnote
                    item(key = "footnote") {
                        val extra = if (state.ownRelayCount > 0) " · 已配置 ${state.ownRelayCount} 个自建中转" else ""
                        Text(
                            text = "同一局域网的电脑会自动出现；远程连接请点右上角 +$extra",
                            color = c.label2,
                            fontSize = 12.sp,
                            lineHeight = 18.sp,
                            modifier = Modifier.padding(horizontal = 12.dp),
                        )
                    }
                }
            }
        }

        // 1:1 HTML .sheet-mask
        AnimatedVisibility(
            visible = addSheetOpen,
            enter = fadeIn(tween(180)),
            exit = fadeOut(tween(160)),
            modifier = Modifier.fillMaxSize(),
        ) {
            Box(
                modifier = Modifier
                    .fillMaxSize()
                    .background(c.sheetBackdrop)
                    .clickable(
                        interactionSource = remember { MutableInteractionSource() },
                        indication = null,
                    ) {
                        addSheetOpen = false
                        onClearPasteFeedback()
                    },
            )
        }

        // 1:1 HTML floating Liquid Glass .bottom-sheet (inset 12dp, radius 26dp, side-by-side buttons)
        AnimatedVisibility(
            visible = addSheetOpen,
            enter = slideInVertically(
                initialOffsetY = { it / 2 },
                animationSpec = spring(
                    dampingRatio = Spring.DampingRatioNoBouncy,
                    stiffness = Spring.StiffnessMediumLow,
                ),
            ) + fadeIn(tween(180)),
            exit = slideOutVertically(
                targetOffsetY = { it / 2 },
                animationSpec = tween(160),
            ) + fadeOut(tween(140)),
            modifier = Modifier
                .align(Alignment.BottomCenter)
                .navigationBarsPadding()
                .imePadding()
                .padding(12.dp),
        ) {
            val sheetShape = RoundedCornerShape(26.dp)
            Box(
                modifier = Modifier
                    .widthIn(max = 520.dp)
                    .fillMaxWidth()
                    .shadow(
                        elevation = 24.dp,
                        shape = sheetShape,
                        ambientColor = Color(0x55000000),
                        spotColor = Color(0x55000000),
                    )
                    .clip(sheetShape)
                    .background(c.sheetBg)
                    .border(width = 0.5.dp, color = c.sheetBorder, shape = sheetShape)
                    .clickable(
                        interactionSource = remember { MutableInteractionSource() },
                        indication = null,
                    ) {}
                    .padding(start = 18.dp, end = 18.dp, top = 18.dp, bottom = 20.dp),
            ) {
                Column(
                    modifier = Modifier.fillMaxWidth(),
                    verticalArrangement = Arrangement.spacedBy(12.dp),
                ) {
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.SpaceBetween,
                    ) {
                        Text(
                            text = "添加电脑或中转",
                            color = c.label,
                            fontSize = 16.sp,
                            fontWeight = FontWeight.SemiBold,
                        )
                        Box(
                            modifier = Modifier
                                .size(28.dp)
                                .clip(CircleShape)
                                .background(c.bgField)
                                .liquidPress(pressedScale = 0.9f) {
                                    addSheetOpen = false
                                    onClearPasteFeedback()
                                },
                            contentAlignment = Alignment.Center,
                        ) {
                            Icon(
                                imageVector = Icons.Rounded.Close,
                                contentDescription = "关闭",
                                tint = c.label2,
                                modifier = Modifier.size(15.dp),
                            )
                        }
                    }

                    // 1:1 HTML .sheet-field
                    Box(
                        modifier = Modifier
                            .fillMaxWidth()
                            .height(44.dp)
                            .clip(RoundedCornerShape(12.dp))
                            .background(c.bgField)
                            .padding(horizontal = 14.dp),
                        contentAlignment = Alignment.CenterStart,
                    ) {
                        if (codeText.isEmpty()) {
                            Text(
                                text = "粘贴 rc1… 连接码或 rcr1… 中转码",
                                color = c.label3,
                                fontSize = 14.sp,
                            )
                        }
                        BasicTextField(
                            value = codeText,
                            onValueChange = { codeText = it },
                            singleLine = true,
                            textStyle = TextStyle(
                                color = c.label,
                                fontSize = 14.sp,
                                fontFamily = FontFamily.Monospace,
                            ),
                            cursorBrush = SolidColor(c.accent),
                            modifier = Modifier.fillMaxWidth(),
                        )
                    }

                    state.pasteFeedback?.let { fb ->
                        Text(
                            text = fb.message,
                            color = if (fb.ok) c.green else c.red,
                            fontSize = 12.5.sp,
                        )
                    }

                    // 1:1 HTML .sheet-btn-row: equal-width secondary + primary buttons
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.spacedBy(10.dp),
                    ) {
                        Box(
                            modifier = Modifier
                                .weight(1f)
                                .height(44.dp)
                                .clip(RoundedCornerShape(12.dp))
                                .background(c.bgField)
                                .liquidPress(pressedScale = 0.96f) {
                                    val clipText = clipboard.getText()?.text?.trim().orEmpty()
                                    if (clipText.isNotEmpty()) {
                                        codeText = clipText
                                    }
                                },
                            contentAlignment = Alignment.Center,
                        ) {
                            Text(
                                text = "从剪贴板粘贴",
                                color = c.label,
                                fontSize = 15.sp,
                                fontWeight = FontWeight.Medium,
                            )
                        }

                        Box(
                            modifier = Modifier
                                .weight(1f)
                                .height(44.dp)
                                .clip(RoundedCornerShape(12.dp))
                                .background(if (state.pasteBusy) c.accent.copy(alpha = 0.6f) else c.accent)
                                .liquidPress(enabled = !state.pasteBusy, pressedScale = 0.96f) {
                                    val target = codeText.trim().ifEmpty {
                                        clipboard.getText()?.text?.trim().orEmpty().also { codeText = it }
                                    }
                                    onPasteCode(target)
                                },
                            contentAlignment = Alignment.Center,
                        ) {
                            if (state.pasteBusy) {
                                CircularProgressIndicator(
                                    color = Color.White,
                                    strokeWidth = 2.dp,
                                    modifier = Modifier.size(18.dp),
                                )
                            } else {
                                Text(
                                    text = "添加",
                                    color = Color.White,
                                    fontSize = 15.sp,
                                    fontWeight = FontWeight.Medium,
                                )
                            }
                        }
                    }
                }
            }
        }

        // Device action sheet backdrop
        AnimatedVisibility(
            visible = menuDevice != null,
            enter = fadeIn(tween(180)),
            exit = fadeOut(tween(160)),
            modifier = Modifier.fillMaxSize(),
        ) {
            Box(
                modifier = Modifier
                    .fillMaxSize()
                    .background(c.sheetBackdrop)
                    .clickable(
                        interactionSource = remember { MutableInteractionSource() },
                        indication = null,
                    ) {
                        menuDeviceId = null
                    },
            )
        }

        // Device action sheet
        AnimatedVisibility(
            visible = menuDevice != null,
            enter = slideInVertically(
                initialOffsetY = { it / 2 },
                animationSpec = spring(
                    dampingRatio = Spring.DampingRatioNoBouncy,
                    stiffness = Spring.StiffnessMediumLow,
                ),
            ) + fadeIn(tween(180)),
            exit = slideOutVertically(
                targetOffsetY = { it / 2 },
                animationSpec = tween(160),
            ) + fadeOut(tween(140)),
            modifier = Modifier
                .align(Alignment.BottomCenter)
                .navigationBarsPadding()
                .padding(12.dp),
        ) {
            val dev = menuDevice
            if (dev != null) {
                val sheetShape = RoundedCornerShape(26.dp)
                Column(
                    modifier = Modifier
                        .widthIn(max = 520.dp)
                        .fillMaxWidth()
                        .shadow(
                            elevation = 24.dp,
                            shape = sheetShape,
                            ambientColor = Color(0x55000000),
                            spotColor = Color(0x55000000),
                        )
                        .clip(sheetShape)
                        .background(c.sheetBg)
                        .border(width = 0.5.dp, color = c.sheetBorder, shape = sheetShape)
                        .clickable(
                            interactionSource = remember { MutableInteractionSource() },
                            indication = null,
                        ) {}
                        .padding(start = 18.dp, end = 18.dp, top = 18.dp, bottom = 18.dp),
                    verticalArrangement = Arrangement.spacedBy(10.dp),
                ) {
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.SpaceBetween,
                    ) {
                        Column(modifier = Modifier.weight(1f)) {
                            Text(
                                text = dev.name,
                                color = c.label,
                                fontSize = 16.sp,
                                fontWeight = FontWeight.SemiBold,
                                maxLines = 1,
                                overflow = TextOverflow.Ellipsis,
                            )
                            Spacer(modifier = Modifier.height(2.dp))
                            Text(
                                text = when {
                                    dev.remembered -> "已互相记住 · 电脑关掉大厅也能直接连"
                                    dev.status == "refused" && dev.error.isNotEmpty() -> dev.error
                                    else -> "未记住 · 需要双方同意后才能长期保存"
                                },
                                color = if (dev.status == "refused") c.warn else c.label2,
                                fontSize = 12.sp,
                            )
                        }
                        Box(
                            modifier = Modifier
                                .size(28.dp)
                                .clip(CircleShape)
                                .background(c.bgField)
                                .liquidPress(pressedScale = 0.9f) { menuDeviceId = null },
                            contentAlignment = Alignment.Center,
                        ) {
                            Icon(
                                imageVector = Icons.Rounded.Close,
                                contentDescription = "关闭",
                                tint = c.label2,
                                modifier = Modifier.size(15.dp),
                            )
                        }
                    }

                    if (!dev.remembered && dev.isOnline && dev.ask == "none") {
                        SheetActionButton(
                            label = "记住这台电脑",
                            color = c.accent,
                            bgColor = c.accent.copy(alpha = 0.16f),
                            onClick = {
                                menuDeviceId = null
                                onRememberDevice(dev)
                            },
                        )
                    }
                    if (dev.ask == "out") {
                        SheetActionButton(
                            label = "取消记住请求",
                            color = c.label,
                            bgColor = c.bgField,
                            onClick = {
                                menuDeviceId = null
                                onCancelRemember(dev)
                            },
                        )
                    }
                    if (dev.status == "refused" || dev.status == "offline") {
                        SheetActionButton(
                            label = "重新连接检测",
                            color = c.label,
                            bgColor = c.bgField,
                            onClick = {
                                menuDeviceId = null
                                onRetryDevice(dev)
                            },
                        )
                    }
                    if (dev.remembered || dev.status == "refused" || dev.status == "offline") {
                        SheetActionButton(
                            label = if (dev.remembered) "忘记并删除这台电脑" else "从列表移除",
                            color = c.red,
                            bgColor = c.red.copy(alpha = 0.14f),
                            onClick = {
                                menuDeviceId = null
                                onForgetDevice(dev)
                            },
                        )
                    }
                }
            }
        }
    }
}

@Composable
private fun SheetActionButton(
    label: String,
    color: Color,
    bgColor: Color,
    onClick: () -> Unit,
) {
    Box(
        modifier = Modifier
            .fillMaxWidth()
            .height(44.dp)
            .clip(RoundedCornerShape(12.dp))
            .background(bgColor)
            .liquidPress(pressedScale = 0.97f, onClick = onClick),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            text = label,
            color = color,
            fontSize = 14.5.sp,
            fontWeight = FontWeight.Medium,
        )
    }
}

@Composable
private fun AppleGroupedSection(
    title: String,
    devices: List<DeviceItem>,
    onSelectDevice: (DeviceItem) -> Unit,
    onRememberDevice: (DeviceItem) -> Unit,
    onAgreeRemember: (DeviceItem) -> Unit,
    onRefuseRemember: (DeviceItem) -> Unit,
    onCancelRemember: (DeviceItem) -> Unit,
    onRetryDevice: (DeviceItem) -> Unit,
    onOpenMenu: (DeviceItem) -> Unit,
) {
    val c = RcThemeColors.current

    Column(
        modifier = Modifier.fillMaxWidth(),
        verticalArrangement = Arrangement.spacedBy(7.dp),
    ) {
        Text(
            text = title,
            color = c.label2,
            fontSize = 12.sp,
            modifier = Modifier.padding(start = 12.dp),
        )

        Column(
            modifier = Modifier
                .fillMaxWidth()
                .clip(RoundedCornerShape(14.dp))
                .background(c.bgGrouped),
        ) {
            devices.forEachIndexed { idx, device ->
                DeviceGroupedRow(
                    device = device,
                    onClick = {
                        when {
                            device.ask == "in" -> onAgreeRemember(device)
                            device.isOnline || device.status == "checking" -> onSelectDevice(device)
                            device.status == "refused" -> onRetryDevice(device)
                        }
                    },
                    onRemember = { onRememberDevice(device) },
                    onAgree = { onAgreeRemember(device) },
                    onRefuse = { onRefuseRemember(device) },
                    onCancelAsk = { onCancelRemember(device) },
                    onOpenMenu = { onOpenMenu(device) },
                )
                if (idx < devices.lastIndex) {
                    HairlineDivider(modifier = Modifier.padding(start = 49.dp))
                }
            }
        }
    }
}

@Composable
private fun DeviceGroupedRow(
    device: DeviceItem,
    onClick: () -> Unit,
    onRemember: () -> Unit,
    onAgree: () -> Unit,
    onRefuse: () -> Unit,
    onCancelAsk: () -> Unit,
    onOpenMenu: () -> Unit,
) {
    val c = RcThemeColors.current
    val clickable = device.isOnline || device.status == "checking" || device.status == "refused" || device.ask == "in"
    val askIn = device.ask == "in"
    val askOut = device.ask == "out"
    val showMenu = !askIn && (device.remembered || device.isOnline || device.status == "refused" || device.status == "offline")

    Box(
        modifier = Modifier
            .fillMaxWidth()
            .height(IntrinsicSize.Min)
            .background(if (askIn) c.warn.copy(alpha = 0.08f) else Color.Transparent)
            .liquidPress(enabled = clickable, pressedScale = 0.992f, onClick = onClick),
    ) {
        if (askIn) {
            Box(
                modifier = Modifier
                    .align(Alignment.CenterStart)
                    .padding(vertical = 8.dp)
                    .width(3.dp)
                    .fillMaxHeight()
                    .clip(RoundedCornerShape(2.dp))
                    .background(c.warn),
            )
        }

        Row(
            modifier = Modifier
                .fillMaxWidth()
                .padding(start = 15.dp, end = 12.dp, top = 11.dp, bottom = 11.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Icon(
                imageVector = Icons.Outlined.DesktopWindows,
                contentDescription = null,
                tint = when {
                    askIn -> c.warn
                    device.remembered && device.isOnline -> c.accent
                    device.isOnline -> c.label.copy(alpha = 0.86f)
                    else -> c.label.copy(alpha = 0.42f)
                },
                modifier = Modifier.size(21.dp),
            )

            Column(modifier = Modifier.weight(1f)) {
                Text(
                    text = device.name,
                    color = if (device.isOnline || device.status == "checking" || askIn) c.label else c.label.copy(alpha = 0.56f),
                    fontSize = 15.sp,
                    fontWeight = FontWeight.Normal,
                    letterSpacing = (-0.15).sp,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Spacer(modifier = Modifier.height(2.dp))
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(6.dp),
                ) {
                    StatusDot(
                        color = when {
                            askIn -> c.warn
                            device.isOnline -> c.green
                            device.status == "checking" -> c.warn
                            device.status == "refused" -> c.warn
                            else -> c.label3
                        },
                        breathing = device.status == "checking" || askIn,
                        size = 6.dp,
                    )
                    val sub = when {
                        askIn -> "想记住这台手机 · 同意后可随时连接"
                        askOut -> "已发送记住请求 · 等待电脑端同意"
                        device.isOnline -> buildString {
                            val p = device.pathLabel
                            if (p.isNotEmpty()) append(p)
                            if (device.rttMs > 0) {
                                if (isNotEmpty()) append(" · ")
                                append("${device.rttMs} ms")
                            }
                            if (device.remembered) {
                                if (isNotEmpty()) append(" · ")
                                append("已记住")
                            }
                            if (isEmpty()) append("在线")
                        }
                        device.status == "checking" -> "检测连接中…"
                        device.status == "refused" -> {
                            val base = device.error.ifEmpty { "对方拒绝连接" }
                            "$base（点此重试）"
                        }
                        else -> if (device.remembered) "离线 · 已记住" else "离线"
                    }
                    Text(
                        text = sub,
                        color = when {
                            askIn -> c.warn
                            device.status == "refused" && device.error.isNotEmpty() -> c.warn
                            else -> c.label2
                        },
                        fontSize = 12.sp,
                        maxLines = if (askIn || (device.status == "refused" && device.error.isNotEmpty())) 2 else 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
            }

            when {
                askIn -> {
                    Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                        CompactPillButton(
                            label = "拒绝",
                            fg = c.label2,
                            bg = c.bgField,
                            onClick = onRefuse,
                        )
                        CompactPillButton(
                            label = "同意",
                            fg = Color.White,
                            bg = c.accent,
                            onClick = onAgree,
                        )
                    }
                }
                askOut -> {
                    CompactPillButton(
                        label = "取消",
                        fg = c.label2,
                        bg = c.bgField,
                        onClick = onCancelAsk,
                    )
                }
                else -> {
                    Row(
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.spacedBy(4.dp),
                    ) {
                        if (!device.remembered && device.isOnline) {
                            CompactPillButton(
                                label = "记住",
                                fg = c.accent,
                                bg = c.accent.copy(alpha = 0.16f),
                                onClick = onRemember,
                            )
                        }
                        if (showMenu) {
                            Box(
                                modifier = Modifier
                                    .size(30.dp)
                                    .clip(CircleShape)
                                    .liquidPress(pressedScale = 0.88f, onClick = onOpenMenu),
                                contentAlignment = Alignment.Center,
                            ) {
                                Icon(
                                    imageVector = Icons.Rounded.MoreHoriz,
                                    contentDescription = "设备操作",
                                    tint = c.label2,
                                    modifier = Modifier.size(18.dp),
                                )
                            }
                        }
                        if (device.isOnline || device.status == "checking") {
                            Icon(
                                imageVector = Icons.AutoMirrored.Rounded.KeyboardArrowRight,
                                contentDescription = null,
                                tint = c.label3,
                                modifier = Modifier.size(18.dp),
                            )
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun CompactPillButton(
    label: String,
    fg: Color,
    bg: Color,
    onClick: () -> Unit,
) {
    Box(
        modifier = Modifier
            .clip(RoundedCornerShape(999.dp))
            .background(bg)
            .liquidPress(pressedScale = 0.94f, onClick = onClick)
            .padding(horizontal = 11.dp, vertical = 6.dp),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            text = label,
            color = fg,
            fontSize = 12.sp,
            fontWeight = FontWeight.Medium,
        )
    }
}
