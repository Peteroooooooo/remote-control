package com.rc.android.ui

import android.content.Context
import android.text.InputType
import android.text.Selection
import android.view.KeyEvent
import android.view.View
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager

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
 * Invisible editor that receives the phone keyboard's output and forwards it to the
 * remote PC as it happens: committed text is sent at once, composing text (pinyin before a
 * candidate is picked) stays local, and Backspace / Enter / arrows on the phone keyboard
 * become the PC's keys even when nothing has been typed.
 */
internal class RemoteImeSinkView(context: Context) : View(context) {
    interface Listener {
        /** Text the user finished typing (a letter, a word, or picked Chinese characters). */
        fun onCommitText(text: String)

        /** The pinyin / word still being composed, "" when none. */
        fun onComposing(text: String)

        /** A key the phone keyboard pressed (Backspace, Enter, arrows...). */
        fun onKey(scan: Int)

        /** True while a sticky modifier (Ctrl / Alt / Win, or Shift) is active. */
        fun modifierActive(): Boolean
    }

    var listener: Listener? = null
    private val imm = context.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
    private var conn: SinkConnection? = null

    init {
        isFocusable = true
        isFocusableInTouchMode = true
    }

    override fun onCheckIsTextEditor(): Boolean = true

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection {
        // No suggestions so English letters are committed one by one instead of word by word.
        // Plain text class keeps Chinese input methods available (a password type would not).
        outAttrs.inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        // No full-screen extract editor in landscape: the PC picture must stay visible.
        outAttrs.imeOptions = EditorInfo.IME_ACTION_NONE or
            EditorInfo.IME_FLAG_NO_EXTRACT_UI or
            EditorInfo.IME_FLAG_NO_FULLSCREEN or
            EditorInfo.IME_FLAG_NO_ENTER_ACTION
        outAttrs.initialSelStart = 0
        outAttrs.initialSelEnd = 0
        return SinkConnection().also { conn = it }
    }

    /** Drops any half-typed composition and tells the keyboard to start fresh. */
    fun reset() {
        conn?.editable?.clear()
        listener?.onComposing("")
        imm.restartInput(this)
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
        conn?.editable?.clear()
        listener?.onComposing("")
        clearFocus()
    }

    private inner class SinkConnection : BaseInputConnection(this@RemoteImeSinkView, true) {
        override fun commitText(text: CharSequence?, newCursorPosition: Int): Boolean {
            super.commitText(text, newCursorPosition)
            flush()
            return true
        }

        override fun setComposingText(text: CharSequence?, newCursorPosition: Int): Boolean {
            val l = listener
            // With Ctrl armed, the first letter a Chinese keyboard starts composing is a shortcut key.
            if (l != null && l.modifierActive() && text != null && text.length == 1 && text[0].code < 128) {
                editable?.clear()
                l.onComposing("")
                l.onCommitText(text.toString())
                post { imm.restartInput(this@RemoteImeSinkView) }
                return true
            }
            super.setComposingText(text, newCursorPosition)
            flush()
            return true
        }

        override fun finishComposingText(): Boolean {
            super.finishComposingText()
            flush()
            return true
        }

        override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
            if (editable.isNullOrEmpty()) {
                repeat(beforeLength.coerceIn(0, 64)) { listener?.onKey(Scan.BACKSPACE) }
                repeat(afterLength.coerceIn(0, 64)) { listener?.onKey(Scan.DELETE) }
                return true
            }
            super.deleteSurroundingText(beforeLength, afterLength)
            flush()
            return true
        }

        override fun deleteSurroundingTextInCodePoints(beforeLength: Int, afterLength: Int): Boolean =
            deleteSurroundingText(beforeLength, afterLength)

        override fun sendKeyEvent(event: KeyEvent): Boolean {
            if (event.action != KeyEvent.ACTION_DOWN) return true
            val scan = when (event.keyCode) {
                KeyEvent.KEYCODE_DEL -> Scan.BACKSPACE
                KeyEvent.KEYCODE_FORWARD_DEL -> Scan.DELETE
                KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER -> Scan.ENTER
                KeyEvent.KEYCODE_TAB -> Scan.TAB
                KeyEvent.KEYCODE_ESCAPE -> Scan.ESC
                KeyEvent.KEYCODE_DPAD_LEFT -> Scan.LEFT
                KeyEvent.KEYCODE_DPAD_RIGHT -> Scan.RIGHT
                KeyEvent.KEYCODE_DPAD_UP -> Scan.UP
                KeyEvent.KEYCODE_DPAD_DOWN -> Scan.DOWN
                KeyEvent.KEYCODE_MOVE_HOME -> Scan.HOME
                KeyEvent.KEYCODE_MOVE_END -> Scan.END
                KeyEvent.KEYCODE_PAGE_UP -> Scan.PGUP
                KeyEvent.KEYCODE_PAGE_DOWN -> Scan.PGDN
                else -> 0
            }
            if (scan != 0) {
                listener?.onKey(scan)
            } else {
                val ch = event.unicodeChar
                if (ch > 0) listener?.onCommitText(String(Character.toChars(ch)))
            }
            return true
        }

        override fun performEditorAction(actionCode: Int): Boolean {
            listener?.onKey(Scan.ENTER)
            return true
        }

        /** Sends everything outside the composing region and keeps only the composition locally. */
        private fun flush() {
            val e = editable ?: return
            val cs = getComposingSpanStart(e)
            val ce = getComposingSpanEnd(e)
            val committed: String
            if (cs < 0 || ce < 0 || cs == ce) {
                committed = e.toString()
                e.clear()
            } else {
                val a = minOf(cs, ce)
                val b = maxOf(cs, ce)
                committed = e.substring(0, a) + e.substring(b)
                if (b < e.length) e.delete(b, e.length)
                if (a > 0) e.delete(0, a)
            }
            Selection.setSelection(e, e.length)
            val composing = e.toString()
            if (committed.isNotEmpty()) listener?.onCommitText(committed)
            listener?.onComposing(composing)
            val start = if (composing.isEmpty()) -1 else 0
            val end = if (composing.isEmpty()) -1 else composing.length
            imm.updateSelection(this@RemoteImeSinkView, e.length, e.length, start, end)
        }
    }
}

