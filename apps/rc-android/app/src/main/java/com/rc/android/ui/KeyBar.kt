package com.rc.android.ui

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.tween
import androidx.compose.animation.expandVertically
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.shrinkVertically
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.waitForUpOrCancellation
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.Keyboard
import androidx.compose.material.icons.rounded.KeyboardArrowDown
import androidx.compose.material.icons.rounded.KeyboardArrowUp
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

internal enum class KeyBarModState { Off, Armed, Locked }

/** One key on the bar or the PC keyboard. `mod` marks Ctrl/Alt/Shift/Win (sticky). */
private data class K(
    val label: String,
    val scan: Int,
    val w: Float = 1f,
    val chordWith: Int? = null,
    val repeat: Boolean = false,
    val mod: Boolean = false,
    val shiftLabel: String? = null,
)

private val Shortcuts = listOf(
    K("Esc", Scan.ESC),
    K("Tab", Scan.TAB),
    K("Copy", 0x2E, chordWith = Scan.CTRL),
    K("Paste", 0x2F, chordWith = Scan.CTRL),
    K("Undo", 0x2C, chordWith = Scan.CTRL),
    K("All", 0x1E, chordWith = Scan.CTRL),
)

private fun letters(row: String, base: Int) = row.mapIndexed { i, c -> K(c.toString(), base + i, shiftLabel = c.uppercase()) }

private val PcRows: List<List<K>> = listOf(
    listOf(K("Esc", Scan.ESC)) + (1..12).map { K("F$it", Scan.f(it)) } + K("Del", Scan.DELETE, repeat = true),
    listOf(K("`", 0x29, shiftLabel = "~")) +
        "1234567890".mapIndexed { i, c -> K(c.toString(), 0x02 + i, shiftLabel = ")!@#$%^&*(".let { s -> s[(i + 1) % 10].toString() }) } +
        listOf(
            K("-", 0x0C, shiftLabel = "_"),
            K("=", 0x0D, shiftLabel = "+"),
            K("Bksp", Scan.BACKSPACE, w = 1.6f, repeat = true),
        ),
    listOf(K("Tab", Scan.TAB, w = 1.4f)) + letters("qwertyuiop", 0x10) +
        listOf(K("[", 0x1A, shiftLabel = "{"), K("]", 0x1B, shiftLabel = "}"), K("\\", 0x2B, w = 1.2f, shiftLabel = "|")),
    listOf(K("Caps", 0x3A, w = 1.7f)) + letters("asdfghjkl", 0x1E) +
        listOf(K(";", 0x27, shiftLabel = ":"), K("'", 0x28, shiftLabel = "\""), K("Enter", Scan.ENTER, w = 1.9f)),
    listOf(K("Shift", Scan.SHIFT, w = 2.4f, mod = true)) + letters("zxcvbnm", 0x2C) +
        listOf(
            K(",", 0x33, shiftLabel = "<"),
            K(".", 0x34, shiftLabel = ">"),
            K("/", 0x35, shiftLabel = "?"),
            K("↑", Scan.UP, w = 1.2f, repeat = true),
            K("Home", Scan.HOME),
        ),
    listOf(
        K("Ctrl", Scan.CTRL, w = 1.4f, mod = true),
        K("Win", Scan.WIN, w = 1.2f, mod = true),
        K("Alt", Scan.ALT, w = 1.2f, mod = true),
        K("Space", Scan.SPACE, w = 6.2f),
        K("←", Scan.LEFT, w = 1.2f, repeat = true),
        K("↓", Scan.DOWN, w = 1.2f, repeat = true),
        K("→", Scan.RIGHT, w = 1.2f, repeat = true),
        K("End", Scan.END),
    ),
)

/**
 * Shortcut bar under the capsule. The `⌨` button shows/hides the phone keyboard; the arrow
 * unfolds a full PC keyboard right under the bar (which puts the phone keyboard away).
 */
@Composable
internal fun RemoteKeyBar(
    compact: Boolean,
    phoneKeyboardOn: Boolean,
    pcKeyboardOpen: Boolean,
    composing: String,
    modStates: Map<Int, KeyBarModState>,
    onToggleModifier: (Int) -> Unit,
    onKey: (Int) -> Unit,
    onChord: (Int, Int) -> Unit,
    onTogglePhoneKeyboard: () -> Unit,
    onTogglePcKeyboard: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val barH = if (compact) 32.dp else 40.dp
    val pcH = if (compact) 28.dp else 40.dp
    val gap = if (compact) 4.dp else 5.dp
    val shape = RoundedCornerShape(18.dp)
    val shift = (modStates[Scan.SHIFT] ?: KeyBarModState.Off) != KeyBarModState.Off
    Column(
        modifier = modifier
            .widthIn(max = 760.dp)
            .fillMaxWidth()
            .clip(shape)
            .background(Color(0xEB1C1C1E))
            .border(0.5.dp, Color(0x1FFFFFFF), shape)
            .pointerInput(Unit) {
                // Swallow touches between keys so they never reach the remote screen underneath.
                awaitEachGesture { awaitFirstDown(requireUnconsumed = false).consume() }
            }
            .padding(6.dp),
        verticalArrangement = Arrangement.spacedBy(gap),
    ) {
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(gap),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Shortcuts.forEach { k ->
                KeyCap(k.label, barH, KeyBarModState.Off, false, Modifier.weight(1f)) {
                    val m = k.chordWith
                    if (m != null) onChord(m, k.scan) else onKey(k.scan)
                }
            }
            Box(
                modifier = Modifier
                    .width(0.5.dp)
                    .height(barH * 0.6f)
                    .background(Color(0x33FFFFFF)),
            )
            IconCap(Icons.Rounded.Keyboard, "Phone keyboard", barH, phoneKeyboardOn, onTogglePhoneKeyboard)
            IconCap(
                if (pcKeyboardOpen) Icons.Rounded.KeyboardArrowUp else Icons.Rounded.KeyboardArrowDown,
                "PC keyboard",
                barH,
                pcKeyboardOpen,
                onTogglePcKeyboard,
            )
        }
        AnimatedVisibility(
            visible = pcKeyboardOpen,
            enter = expandVertically(tween(160)) + fadeIn(tween(160)),
            exit = shrinkVertically(tween(120)) + fadeOut(tween(100)),
        ) {
            Column(verticalArrangement = Arrangement.spacedBy(gap)) {
                Box(
                    modifier = Modifier
                        .fillMaxWidth()
                        .height(0.5.dp)
                        .background(Color(0x22FFFFFF)),
                )
                PcRows.forEach { row ->
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.spacedBy(gap),
                    ) {
                        row.forEach { k -> PcKey(k, pcH, shift, modStates, onToggleModifier, onKey) }
                    }
                }
            }
        }
        if (composing.isNotEmpty()) {
            Text(
                text = composing,
                color = Color(0xFF0A84FF),
                fontSize = 12.sp,
                maxLines = 1,
                modifier = Modifier.padding(start = 4.dp),
            )
        }
    }
}

@Composable
private fun RowScope.PcKey(
    k: K,
    height: Dp,
    shift: Boolean,
    modStates: Map<Int, KeyBarModState>,
    onToggleModifier: (Int) -> Unit,
    onKey: (Int) -> Unit,
) {
    val st = if (k.mod) modStates[k.scan] ?: KeyBarModState.Off else KeyBarModState.Off
    val label = if (shift && k.shiftLabel != null) k.shiftLabel else k.label
    KeyCap(label, height, st, k.repeat, Modifier.weight(k.w)) {
        if (k.mod) onToggleModifier(k.scan) else onKey(k.scan)
    }
}

/**
 * A key that fires on press like a real keyboard and repeats while held when `repeat` is set.
 * It never takes input focus, so it does not disturb the phone keyboard.
 */
@Composable
private fun KeyCap(
    label: String,
    height: Dp,
    highlight: KeyBarModState,
    repeat: Boolean,
    modifier: Modifier,
    onPress: () -> Unit,
) {
    val haptics = LocalHapticFeedback.current
    val scope = rememberCoroutineScope()
    val currentOnPress by rememberUpdatedState(onPress)
    var pressed by remember { mutableStateOf(false) }
    val bg = when {
        highlight == KeyBarModState.Locked -> Color(0xFF0A84FF)
        highlight == KeyBarModState.Armed -> Color(0x660A84FF)
        pressed -> Color(0x47FFFFFF)
        else -> Color(0x21FFFFFF)
    }
    Box(
        modifier = modifier
            .height(height)
            .clip(RoundedCornerShape(9.dp))
            .background(bg)
            .pointerInput(repeat) {
                awaitEachGesture {
                    awaitFirstDown(requireUnconsumed = false).consume()
                    pressed = true
                    haptics.performHapticFeedback(HapticFeedbackType.TextHandleMove)
                    currentOnPress()
                    var job: Job? = null
                    if (repeat) {
                        job = scope.launch {
                            delay(400L)
                            while (true) {
                                currentOnPress()
                                delay(45L)
                            }
                        }
                    }
                    waitForUpOrCancellation()
                    job?.cancel()
                    pressed = false
                }
            },
        contentAlignment = Alignment.Center,
    ) {
        Text(
            text = label,
            color = Color.White,
            fontSize = if (height < 34.dp) 11.sp else 13.5.sp,
            fontWeight = if (highlight != KeyBarModState.Off) FontWeight.SemiBold else FontWeight.Medium,
            maxLines = 1,
            softWrap = false,
        )
    }
}

@Composable
private fun IconCap(
    icon: ImageVector,
    description: String,
    height: Dp,
    active: Boolean,
    onPress: () -> Unit,
) {
    val haptics = LocalHapticFeedback.current
    val currentOnPress by rememberUpdatedState(onPress)
    var pressed by remember { mutableStateOf(false) }
    Box(
        modifier = Modifier
            .width(46.dp)
            .height(height)
            .clip(RoundedCornerShape(9.dp))
            .background(
                when {
                    pressed -> Color(0x47FFFFFF)
                    active -> Color(0x330A84FF)
                    else -> Color(0x21FFFFFF)
                },
            )
            .pointerInput(Unit) {
                awaitEachGesture {
                    awaitFirstDown(requireUnconsumed = false).consume()
                    pressed = true
                    val up = waitForUpOrCancellation()
                    pressed = false
                    if (up != null) {
                        haptics.performHapticFeedback(HapticFeedbackType.TextHandleMove)
                        currentOnPress()
                    }
                }
            },
        contentAlignment = Alignment.Center,
    ) {
        Icon(
            imageVector = icon,
            contentDescription = description,
            tint = if (active) Color(0xFF0A84FF) else Color(0xD9FFFFFF),
            modifier = Modifier.size(20.dp),
        )
    }
}
