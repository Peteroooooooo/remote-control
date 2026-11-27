package com.rc.android.ui

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.core.tween
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
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
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.rc.android.update.UpdatePhase
import kotlinx.coroutines.delay

@Composable
fun SettingsScreen(
    customName: String,
    modelName: String,
    maxNameLen: Int,
    onSetName: (String) -> Unit,
    version: String,
    update: UpdatePhase,
    onCheckUpdate: () -> Unit,
    onUpdateNow: () -> Unit,
    onRetryUpdate: () -> Unit,
    onInstall: () -> Unit,
    onBack: () -> Unit,
) {
    val c = RcThemeColors.current
    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(c.bg),
        contentAlignment = Alignment.TopCenter,
    ) {
        Column(
            modifier = Modifier
                .fillMaxSize()
                .widthIn(max = 640.dp)
                .statusBarsPadding()
                .navigationBarsPadding(),
        ) {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .height(52.dp)
                    .padding(start = 6.dp, end = 16.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                NavBackButton(onClick = onBack)
                Text(
                    text = "设置",
                    color = c.label,
                    fontSize = 17.sp,
                    fontWeight = FontWeight.SemiBold,
                    modifier = Modifier.padding(start = 2.dp),
                )
            }

            Column(
                modifier = Modifier
                    .fillMaxSize()
                    .verticalScroll(rememberScrollState())
                    .padding(horizontal = 16.dp, vertical = 8.dp),
            ) {
                SectionHead("本机")
                SettingsGroup {
                    NameRow(customName, modelName, maxNameLen, onSetName)
                }
                Footnote("电脑上看到的这台手机的名称，留空则使用手机型号")

                Spacer(Modifier.height(22.dp))
                SectionHead("关于")
                SettingsGroup {
                    AboutRow(version, update, onCheckUpdate, onUpdateNow, onRetryUpdate, onInstall)
                }
            }
        }
    }
}

@Composable
private fun SectionHead(text: String) {
    Text(
        text = text,
        color = RcThemeColors.current.label2,
        fontSize = 13.sp,
        fontWeight = FontWeight.Medium,
        modifier = Modifier.padding(start = 16.dp, bottom = 7.dp),
    )
}

@Composable
private fun Footnote(text: String) {
    Text(
        text = text,
        color = RcThemeColors.current.label3,
        fontSize = 12.5.sp,
        lineHeight = 17.sp,
        modifier = Modifier.padding(start = 16.dp, end = 16.dp, top = 7.dp),
    )
}

@Composable
private fun SettingsGroup(content: @Composable () -> Unit) {
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(14.dp))
            .background(RcThemeColors.current.bgGrouped),
    ) {
        content()
    }
}

@Composable
private fun NameRow(customName: String, modelName: String, maxLen: Int, onSetName: (String) -> Unit) {
    val c = RcThemeColors.current
    val focus = LocalFocusManager.current
    var text by remember(customName) { mutableStateOf(customName) }
    var focused by remember { mutableStateOf(false) }
    var savedTick by remember { mutableIntStateOf(0) }
    var showSaved by remember { mutableStateOf(false) }

    LaunchedEffect(savedTick) {
        if (savedTick == 0) return@LaunchedEffect
        showSaved = true
        delay(1_500L)
        showSaved = false
    }

    fun commit() {
        val cleaned = text.trim()
        val effective = if (cleaned == modelName) "" else cleaned
        if (effective != customName) {
            onSetName(cleaned)
            savedTick++
        }
        text = effective
    }

    Row(
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(min = 52.dp)
            .padding(horizontal = 16.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(text = "设备名称", color = c.label, fontSize = 16.sp)
        Spacer(Modifier.width(12.dp))
        AnimatedVisibility(visible = showSaved, enter = fadeIn(tween(120)), exit = fadeOut(tween(300))) {
            Text(text = "✓ 已保存", color = c.green, fontSize = 12.5.sp, modifier = Modifier.padding(end = 8.dp))
        }
        Box(modifier = Modifier.weight(1f), contentAlignment = Alignment.CenterEnd) {
            if (text.isEmpty() && !focused) {
                Text(
                    text = modelName,
                    color = c.label2,
                    fontSize = 16.sp,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    textAlign = TextAlign.End,
                    modifier = Modifier.fillMaxWidth(),
                )
            }
            BasicTextField(
                value = text,
                onValueChange = { v -> text = v.replace("\n", "").take(maxLen) },
                singleLine = true,
                textStyle = TextStyle(color = c.label, fontSize = 16.sp, textAlign = TextAlign.End),
                cursorBrush = SolidColor(c.accent),
                keyboardOptions = KeyboardOptions(imeAction = ImeAction.Done),
                keyboardActions = KeyboardActions(onDone = { focus.clearFocus() }),
                modifier = Modifier
                    .fillMaxWidth()
                    .onFocusChanged { st ->
                        if (focused && !st.isFocused) commit()
                        focused = st.isFocused
                    },
            )
        }
    }
}

@Composable
private fun AboutRow(
    version: String,
    phase: UpdatePhase,
    onCheck: () -> Unit,
    onUpdateNow: () -> Unit,
    onRetry: () -> Unit,
    onInstall: () -> Unit,
) {
    val c = RcThemeColors.current
    val desc = when (phase) {
        UpdatePhase.Idle -> "手动检查，不会主动打扰"
        UpdatePhase.Checking -> "正在检查更新…"
        UpdatePhase.UpToDate -> "已是最新版本"
        is UpdatePhase.Available -> {
            val note = phase.m.notes.lineSequence().firstOrNull()?.trim().orEmpty()
            if (note.isEmpty()) "发现新版本 v${phase.m.version}" else "发现新版本 v${phase.m.version} · $note"
        }
        is UpdatePhase.Downloading -> {
            val mb = { b: Long -> "%.1f".format(b / 1e6) }
            if (phase.total > 0) {
                val pct = (phase.done * 100 / phase.total).coerceIn(0, 100)
                "正在下载 v${phase.m.version} · $pct%（${mb(phase.done)} / ${mb(phase.total)} MB）"
            } else {
                "正在下载 v${phase.m.version}（${mb(phase.done)} MB）…"
            }
        }
        is UpdatePhase.Ready -> "v${phase.m.version} 已下载"
        is UpdatePhase.Failed -> "更新失败：${phase.message}"
    }
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(min = 64.dp)
            .padding(horizontal = 16.dp, vertical = 10.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(modifier = Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
            Text(text = "Remote Control v$version", color = c.label, fontSize = 16.sp)
            Text(
                text = desc,
                color = if (phase is UpdatePhase.Failed) c.red else c.label2,
                fontSize = 12.5.sp,
                lineHeight = 17.sp,
            )
        }
        Spacer(Modifier.width(10.dp))
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically) {
            when (phase) {
                UpdatePhase.Idle -> PillButton("检查更新", onClick = onCheck)
                UpdatePhase.Checking -> PillButton("检查中…", enabled = false, onClick = {})
                UpdatePhase.UpToDate -> PillButton("再次检查", onClick = onCheck)
                is UpdatePhase.Available -> PillButton("立即更新", primary = true, onClick = onUpdateNow)
                is UpdatePhase.Downloading -> PillButton("更新中…", enabled = false, onClick = {})
                is UpdatePhase.Ready -> PillButton("安装", primary = true, onClick = onInstall)
                is UpdatePhase.Failed -> PillButton("重试", onClick = onRetry)
            }
        }
    }
}

@Composable
private fun PillButton(text: String, primary: Boolean = false, enabled: Boolean = true, onClick: () -> Unit) {
    val c = RcThemeColors.current
    Box(
        modifier = Modifier
            .clip(RoundedCornerShape(999.dp))
            .background(if (primary) c.accent else c.bgField)
            .liquidPress(enabled = enabled, pressedScale = 0.94f, onClick = onClick)
            .padding(horizontal = 14.dp, vertical = 7.dp),
    ) {
        Text(
            text = text,
            color = when {
                primary -> Color.White
                enabled -> c.label
                else -> c.label3
            },
            fontSize = 13.5.sp,
            fontWeight = FontWeight.Medium,
            maxLines = 1,
        )
    }
}
