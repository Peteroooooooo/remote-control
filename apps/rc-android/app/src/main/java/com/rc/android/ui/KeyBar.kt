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
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.rounded.KeyboardReturn
import androidx.compose.material.icons.rounded.Keyboard
import androidx.compose.material.icons.rounded.KeyboardArrowDown
import androidx.compose.material.icons.rounded.KeyboardArrowUp
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
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
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
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
 * Shortcut bar + visible draft input box under the capsule.
 * The `⌨` button shows/hides the phone keyboard; the arrow unfolds a full PC keyboard
 * right under the bar (which puts the phone keyboard and draft box away).
 */
@Composable
internal fun RemoteKeyBar(
    compact: Boolean,
    phoneKeyboardOn: Boolean,
    pcKeyboardOpen: Boolean,
    hasHardwareKeyboard: Boolean,
    modStates: Map<Int, KeyBarModState>,
    onToggleModifier: (Int) -> Unit,
    onKey: (Int) -> Unit,
    onChord: (Int, Int) -> Unit,
    onSendDraft: (text: String, withEnter: Boolean) -> Unit,
    onModifierChordText: (String) -> Unit,
    onDraftFocused: () -> Unit,
    onTogglePhoneKeyboard: () -> Unit,
    onTogglePcKeyboard: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val barH = if (compact) 30.dp else 34.dp
    val draftH = if (compact) 32.dp else 36.dp
    val pcH = if (compact) 28.dp else 38.dp
    val gap = 5.dp
    val shift = (modStates[Scan.SHIFT] ?: KeyBarModState.Off) != KeyBarModState.Off

    var draftView by remember { mutableStateOf<RemoteDraftEditText?>(null) }
    var hasDraftText by remember { mutableStateOf(false) }

    val currentOnKey by rememberUpdatedState(onKey)
    val currentOnSendDraft by rememberUpdatedState(onSendDraft)
    val currentOnModifierChordText by rememberUpdatedState(onModifierChordText)
    val currentOnDraftFocused by rememberUpdatedState(onDraftFocused)
    val currentModStates by rememberUpdatedState(modStates)

    val draftListener = remember {
        object : RemoteDraftEditText.Listener {
            override fun onSendDraft(text: String, withEnter: Boolean) {
                currentOnSendDraft(text, withEnter)
            }

            override fun onKey(scan: Int) {
                currentOnKey(scan)
            }

            override fun onModifierChordText(text: String) {
                currentOnModifierChordText(text)
            }

            override fun modifierActive(): Boolean = currentModStates.values.any { it != KeyBarModState.Off }

            override fun onHasTextChanged(hasText: Boolean) {
                hasDraftText = hasText
            }

            override fun onFocused() {
                currentOnDraftFocused()
            }
        }
    }

    LaunchedEffect(phoneKeyboardOn, pcKeyboardOpen, hasHardwareKeyboard, draftView) {
        val view = draftView ?: return@LaunchedEffect
        if (phoneKeyboardOn && !pcKeyboardOpen) {
            if (hasHardwareKeyboard) view.requestFocus() else view.showKeyboard()
        } else {
            view.hideKeyboard()
        }
    }

    DisposableEffect(draftView) {
        val view = draftView
        onDispose {
            view?.hideKeyboard()
            view?.clearDraft()
        }
    }

    // Match the top capsule's translucent Liquid Glass material and keep a compact width (~420dp)
    // unless the full 14-column PC keyboard is unfolded (up to 740dp).
    FloatingGlass(
        cornerRadius = 18.dp,
        darkOverlay = true,
        elevation = 12.dp,
        modifier = modifier
            .widthIn(max = if (pcKeyboardOpen) 740.dp else 420.dp)
            .fillMaxWidth()
            .pointerInput(Unit) {
                // Swallow touches between keys so they never reach the remote screen underneath.
                awaitEachGesture { awaitFirstDown(requireUnconsumed = false).consume() }
            },
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(6.dp),
            verticalArrangement = Arrangement.spacedBy(gap),
        ) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.spacedBy(gap),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Shortcuts.forEach { k ->
                    val keyMod = if (pcKeyboardOpen) Modifier.width(66.dp) else Modifier.weight(1f)
                    KeyCap(k.label, barH, KeyBarModState.Off, false, keyMod) {
                        val m = k.chordWith
                        if (m != null) onChord(m, k.scan) else onKey(k.scan)
                    }
                }
                if (pcKeyboardOpen) {
                    Spacer(Modifier.weight(1f))
                }
                Box(
                    modifier = Modifier
                        .width(0.5.dp)
                        .height(barH * 0.56f)
                        .background(Color(0x2EFFFFFF)),
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
                visible = !pcKeyboardOpen,
                enter = expandVertically(tween(140)) + fadeIn(tween(140)),
                exit = shrinkVertically(tween(100)) + fadeOut(tween(90)),
            ) {
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.spacedBy(gap),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    val inputShape = RoundedCornerShape(10.dp)
                    Box(
                        modifier = Modifier
                            .weight(1f)
                            .height(draftH)
                            .clip(inputShape)
                            .background(Color(0x18FFFFFF))
                            .border(
                                width = 0.75.dp,
                                color = if (phoneKeyboardOn) Color(0x800A84FF) else Color(0x22FFFFFF),
                                shape = inputShape,
                            )
                            .padding(horizontal = 10.dp),
                        contentAlignment = Alignment.CenterStart,
                    ) {
                        AndroidView(
                            factory = { ctx ->
                                RemoteDraftEditText(ctx).also {
                                    it.setCompactStyle(compact)
                                    it.listener = draftListener
                                    draftView = it
                                }
                            },
                            update = { view ->
                                view.setCompactStyle(compact)
                                view.listener = draftListener
                            },
                            modifier = Modifier
                                .fillMaxWidth()
                                .height(draftH),
                        )
                    }
                    DraftActionButton(
                        contentDescription = "Send text",
                        height = draftH,
                        accent = hasDraftText,
                        horizontalPadding = 12.dp,
                        onPress = { draftView?.submitDraft(withEnter = false, sendEnterWhenEmpty = false) },
                    ) {
                        Text(
                            text = "Send",
                            color = Color.White,
                            fontSize = 12.5.sp,
                            fontWeight = FontWeight.SemiBold,
                            maxLines = 1,
                            softWrap = false,
                        )
                    }
                    DraftActionButton(
                        contentDescription = "Send with Enter",
                        height = draftH,
                        accent = false,
                        minWidth = 36.dp,
                        onPress = { draftView?.submitDraft(withEnter = true, sendEnterWhenEmpty = true) },
                    ) {
                        Icon(
                            imageVector = Icons.AutoMirrored.Rounded.KeyboardReturn,
                            contentDescription = null,
                            tint = Color(0xE6FFFFFF),
                            modifier = Modifier.size(16.dp),
                        )
                    }
                }
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
        }
    }
}

/**
 * Action button next to the draft box (`Send` / Enter). Never takes input focus so tapping it
 * keeps the phone keyboard open for continuous typing.
 */
@Composable
private fun DraftActionButton(
    contentDescription: String,
    height: Dp,
    accent: Boolean,
    horizontalPadding: Dp = 0.dp,
    minWidth: Dp = 0.dp,
    onPress: () -> Unit,
    content: @Composable () -> Unit,
) {
    val haptics = LocalHapticFeedback.current
    val currentOnPress by rememberUpdatedState(onPress)
    var pressed by remember { mutableStateOf(false) }
    val shape = RoundedCornerShape(10.dp)
    val bg = when {
        pressed && accent -> Color(0xCC0A84FF)
        pressed -> Color(0x3DFFFFFF)
        accent -> Color(0xE60A84FF)
        else -> Color(0x1AFFFFFF)
    }
    Box(
        modifier = Modifier
            .height(height)
            .widthIn(min = minWidth)
            .clip(shape)
            .background(bg)
            .border(
                width = 0.5.dp,
                color = if (accent) Color.Transparent else Color(0x22FFFFFF),
                shape = shape,
            )
            .semantics { this.contentDescription = contentDescription }
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
            }
            .padding(horizontal = horizontalPadding),
        contentAlignment = Alignment.Center,
    ) {
        content()
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
        highlight == KeyBarModState.Locked -> Color(0xE60A84FF)
        highlight == KeyBarModState.Armed -> Color(0x660A84FF)
        pressed -> Color(0x3DFFFFFF)
        else -> Color(0x1AFFFFFF)
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
            fontSize = if (height < 30.dp) 11.sp else 12.5.sp,
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
            .width(38.dp)
            .height(height)
            .clip(RoundedCornerShape(9.dp))
            .background(
                when {
                    pressed -> Color(0x3DFFFFFF)
                    active -> Color(0x380A84FF)
                    else -> Color(0x1AFFFFFF)
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
            tint = if (active) Color(0xFF4DA6FF) else Color(0xD9FFFFFF),
            modifier = Modifier.size(18.dp),
        )
    }
}
