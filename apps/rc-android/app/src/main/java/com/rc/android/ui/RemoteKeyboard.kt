package com.rc.android.ui

import android.content.Context
import android.graphics.Color
import android.os.SystemClock
import android.text.Editable
import android.text.InputType
import android.text.TextWatcher
import android.util.TypedValue
import android.view.Gravity
import android.view.KeyEvent
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputConnectionWrapper
import android.view.inputmethod.InputMethodManager
import android.widget.EditText

/** Windows set-1 scan codes used by the key bar and the keyboard sink. */
internal object Scan {
    const val ESC = 0x01
    const val BACKSPACE = 0x0E
    const val TAB = 0x0F
    const val ENTER = 0x1C
    const val CTRL = 0x1D
    const val SHIFT = 0x2A
    const val ALT = 0x38
    const val SPACE = 0x39
    const val WIN = 0xE05B
    const val HOME = 0xE047
    const val UP = 0xE048
    const val PGUP = 0xE049
    const val LEFT = 0xE04B
    const val RIGHT = 0xE04D
    const val END = 0xE04F
    const val DOWN = 0xE050
    const val PGDN = 0xE051
    const val DELETE = 0xE053

    fun f(n: Int): Int = when (n) {
        in 1..10 -> 0x3A + n
        11 -> 0x57
        else -> 0x58
    }
}

private val LetterRows = listOf("qwertyuiop" to 0x10, "asdfghjkl" to 0x1E, "zxcvbnm" to 0x2C)

/**
 * US-layout scan code for a character typed while a sticky modifier is active,
 * so Ctrl + "c" from the phone keyboard becomes the real Ctrl+C key, not the text "c".
 * Returns 0 for characters without a key.
 */
internal fun charToScan(ch: Char): Int {
    val c = ch.lowercaseChar()
    for ((row, base) in LetterRows) {
        val i = row.indexOf(c)
        if (i >= 0) return base + i
    }
    if (c in '1'..'9') return 0x02 + (c - '1')
    return when (c) {
        '0' -> 0x0B
        '-' -> 0x0C
        '=' -> 0x0D
        '[' -> 0x1A
        ']' -> 0x1B
        ';' -> 0x27
        '\'' -> 0x28
        '`' -> 0x29
        '\\' -> 0x2B
        ',' -> 0x33
        '.' -> 0x34
        '/' -> 0x35
        ' ' -> Scan.SPACE
        '\n' -> Scan.ENTER
        '\t' -> Scan.TAB
        else -> 0
    }
}

/**
 * Visible single-line draft input box mounted in the top key bar.
 * Lets the user compose, inspect, and edit text locally (including Chinese IME candidates)
 * before sending it to the remote cursor, while still forwarding Backspace / Delete / Enter
 * directly to the remote PC whenever the draft is empty.
 */
internal class RemoteDraftEditText(context: Context) : EditText(context) {
    interface Listener {
        /** Sends composed draft text to the remote PC, optionally followed by Enter. */
        fun onSendDraft(text: String, withEnter: Boolean)

        /** Sends a single hardware key scan code to the remote PC. */
        fun onKey(scan: Int)

        /** Sends typed text as modifier chord keys while Ctrl/Alt/Win/Shift is armed. */
        fun onModifierChordText(text: String)

        /** True while a sticky modifier (Ctrl / Alt / Win / Shift) is active. */
        fun modifierActive(): Boolean

        /** Called when draft emptiness toggles (for highlighting the Send button). */
        fun onHasTextChanged(hasText: Boolean)

        /** Called when the user focuses/taps the draft box directly. */
        fun onFocused()
    }

    var listener: Listener? = null
    private val imm = context.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
    private var lastImeSubmitMs = 0L

    init {
        background = null
        setTextColor(Color.WHITE)
        setHintTextColor(0x8CFFFFFF.toInt())
        hint = "Type to send…"
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 13.5f)
        setSingleLine(true)
        maxLines = 1
        gravity = Gravity.CENTER_VERTICAL or Gravity.START
        setPadding(0, 0, 0, 0)
        isHorizontalFadingEdgeEnabled = true
        inputType = InputType.TYPE_CLASS_TEXT
        imeOptions = EditorInfo.IME_ACTION_SEND or
            EditorInfo.IME_FLAG_NO_EXTRACT_UI or
            EditorInfo.IME_FLAG_NO_FULLSCREEN

        addTextChangedListener(
            object : TextWatcher {
                override fun beforeTextChanged(s: CharSequence?, start: Int, count: Int, after: Int) = Unit
                override fun onTextChanged(s: CharSequence?, start: Int, before: Int, count: Int) = Unit
                override fun afterTextChanged(s: Editable?) {
                    listener?.onHasTextChanged(!s.isNullOrEmpty())
                }
            },
        )

        setOnFocusChangeListener { _, hasFocus ->
            if (hasFocus) listener?.onFocused()
        }

        setOnClickListener {
            listener?.onFocused()
        }

        setOnEditorActionListener { _, actionId, event ->
            val isAction = actionId == EditorInfo.IME_ACTION_SEND ||
                actionId == EditorInfo.IME_ACTION_DONE ||
                actionId == EditorInfo.IME_ACTION_GO ||
                actionId == EditorInfo.IME_ACTION_NEXT ||
                (actionId == EditorInfo.IME_NULL && (event == null || event.action == KeyEvent.ACTION_DOWN))
            if (isAction) {
                submitDraft(withEnter = false, sendEnterWhenEmpty = true, fromIme = true)
                true
            } else {
                false
            }
        }
    }

    fun setCompactStyle(compact: Boolean) {
        setTextSize(TypedValue.COMPLEX_UNIT_SP, if (compact) 13f else 14f)
    }

    fun submitDraft(withEnter: Boolean, sendEnterWhenEmpty: Boolean = withEnter, fromIme: Boolean = false) {
        if (fromIme) {
            val now = SystemClock.uptimeMillis()
            if (now - lastImeSubmitMs < 70L) return
            lastImeSubmitMs = now
        }
        val content = text?.toString().orEmpty()
        if (content.isNotEmpty()) {
            setText("")
            imm.restartInput(this)
            listener?.onSendDraft(content, withEnter)
        } else if (sendEnterWhenEmpty) {
            listener?.onKey(Scan.ENTER)
        }
    }

    fun clearDraft() {
        if (!text.isNullOrEmpty()) {
            setText("")
            imm.restartInput(this)
        }
    }

    fun showKeyboard() {
        requestFocus()
        post {
            requestFocus()
            imm.showSoftInput(this, 0)
        }
    }

    fun hideKeyboard() {
        imm.hideSoftInputFromWindow(windowToken, 0)
        clearFocus()
    }

    override fun onKeyDown(keyCode: Int, event: KeyEvent?): Boolean {
        when (keyCode) {
            KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER -> {
                submitDraft(withEnter = false, sendEnterWhenEmpty = true, fromIme = true)
                return true
            }
            KeyEvent.KEYCODE_DEL -> if (text.isNullOrEmpty()) {
                listener?.onKey(Scan.BACKSPACE)
                return true
            }
            KeyEvent.KEYCODE_FORWARD_DEL -> if (text.isNullOrEmpty()) {
                listener?.onKey(Scan.DELETE)
                return true
            }
            KeyEvent.KEYCODE_ESCAPE -> {
                listener?.onKey(Scan.ESC)
                return true
            }
            KeyEvent.KEYCODE_TAB -> {
                listener?.onKey(Scan.TAB)
                return true
            }
        }
        return super.onKeyDown(keyCode, event)
    }

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection? {
        val base = super.onCreateInputConnection(outAttrs) ?: return null
        // Keep IME action label crisp ("Send") without allowing full-screen extract takeover.
        outAttrs.imeOptions = (outAttrs.imeOptions and EditorInfo.IME_MASK_ACTION.inv()) or
            EditorInfo.IME_ACTION_SEND or
            EditorInfo.IME_FLAG_NO_EXTRACT_UI or
            EditorInfo.IME_FLAG_NO_FULLSCREEN
        return object : InputConnectionWrapper(base, true) {
            override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
                if (text.isNullOrEmpty()) {
                    repeat(beforeLength.coerceIn(0, 64)) { listener?.onKey(Scan.BACKSPACE) }
                    repeat(afterLength.coerceIn(0, 64)) { listener?.onKey(Scan.DELETE) }
                    return true
                }
                return super.deleteSurroundingText(beforeLength, afterLength)
            }

            override fun deleteSurroundingTextInCodePoints(beforeLength: Int, afterLength: Int): Boolean {
                if (text.isNullOrEmpty()) {
                    repeat(beforeLength.coerceIn(0, 64)) { listener?.onKey(Scan.BACKSPACE) }
                    repeat(afterLength.coerceIn(0, 64)) { listener?.onKey(Scan.DELETE) }
                    return true
                }
                return super.deleteSurroundingTextInCodePoints(beforeLength, afterLength)
            }

            override fun commitText(txt: CharSequence?, newCursorPosition: Int): Boolean {
                val l = listener
                if (l != null && l.modifierActive() && text.isNullOrEmpty() && !txt.isNullOrEmpty()) {
                    l.onModifierChordText(txt.toString())
                    post { imm.restartInput(this@RemoteDraftEditText) }
                    return true
                }
                return super.commitText(txt, newCursorPosition)
            }

            override fun setComposingText(txt: CharSequence?, newCursorPosition: Int): Boolean {
                val l = listener
                if (l != null && l.modifierActive() && text.isNullOrEmpty() && txt != null && txt.length == 1 && txt[0].code < 128) {
                    l.onModifierChordText(txt.toString())
                    post { imm.restartInput(this@RemoteDraftEditText) }
                    return true
                }
                return super.setComposingText(txt, newCursorPosition)
            }

            override fun sendKeyEvent(event: KeyEvent): Boolean {
                if (event.action == KeyEvent.ACTION_DOWN) {
                    when (event.keyCode) {
                        KeyEvent.KEYCODE_DEL -> if (text.isNullOrEmpty()) {
                            listener?.onKey(Scan.BACKSPACE)
                            return true
                        }
                        KeyEvent.KEYCODE_FORWARD_DEL -> if (text.isNullOrEmpty()) {
                            listener?.onKey(Scan.DELETE)
                            return true
                        }
                        KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER -> {
                            submitDraft(withEnter = false, sendEnterWhenEmpty = true, fromIme = true)
                            return true
                        }
                        KeyEvent.KEYCODE_ESCAPE -> {
                            listener?.onKey(Scan.ESC)
                            return true
                        }
                        KeyEvent.KEYCODE_TAB -> {
                            listener?.onKey(Scan.TAB)
                            return true
                        }
                    }
                }
                return super.sendKeyEvent(event)
            }
        }
    }
}

