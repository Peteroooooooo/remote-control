package com.rc.android.ui

import kotlin.math.ceil
import kotlin.math.floor
import kotlin.math.max
import kotlin.math.min

/** A rectangle in dp, relative to the hub content area. */
data class DpBox(val x: Float, val y: Float, val w: Float, val h: Float)

/**
 * Where the hub puts the desktop preview ("TV") and the window cards.
 * Same rule as dist/hub-playground.html.
 */
data class HubLayoutResult(
    val sideBySide: Boolean,
    val tv: DpBox,
    val list: DpBox,
    val columns: Int,
)

object HubLayout {
    const val GAP = 12f
    const val CARD_GAP = 14f
    const val CARD_MIN_W = 150f
    const val CARD_MAX_W = 240f
    const val SIDE_LIST_MIN_W = 310f
    const val TV_MAX_FRACTION = 0.66f
    /** Side by side wins when its picture is at least this share of the stacked one's. */
    const val SIDE_BIAS = 0.9f
    /** Room the floating dock needs below the last visible card row. */
    const val DOCK_ROOM = 68f

    /** Height of a window card: a full-bleed 16:10.8 thumbnail. */
    fun cardHeight(cardW: Float): Float = cardW * 10.8f / 16f

    /** Cards stay between CARD_MIN_W and CARD_MAX_W wide; up to 4 columns, 5 on very wide screens. */
    fun columnsFor(listW: Float): Int {
        val byMin = floor((listW + CARD_GAP) / (CARD_MIN_W + CARD_GAP)).toInt().coerceIn(1, 4)
        val byMax = ceil((listW + CARD_GAP) / (CARD_MAX_W + CARD_GAP)).toInt()
        return max(byMin, byMax).coerceIn(1, 5)
    }

    private fun cardW(listW: Float, cols: Int) = (listW - (cols - 1) * CARD_GAP) / cols

    /**
     * Picks side-by-side or stacked by which one shows the PC picture larger.
     *
     * @param w width of the content area (below the top bar), dp
     * @param h height of the content area, dp
     * @param pcRatio width / height of the PC's main monitor
     * @param dockRoom space reserved under the cards for the floating dock (0 when hidden)
     * @param forceSide reuse an earlier decision (the top bar depends on it)
     */
    fun compute(
        w: Float,
        h: Float,
        pcRatio: Float,
        dockRoom: Float = DOCK_ROOM,
        forceSide: Boolean? = null,
    ): HubLayoutResult {
        val ratio = pcRatio.takeIf { it.isFinite() && it > 0.1f } ?: (16f / 9f)
        val aw = max(w, 1f)
        val ah = max(h, 1f)

        // Side by side: the card column always fits two cards.
        val listMin = max(SIDE_LIST_MIN_W, 2 * CARD_MIN_W + CARD_GAP)
        val sideW = min(ah * ratio, aw - GAP - listMin).coerceAtLeast(0f)
        val sideArea = sideW * (sideW / ratio)

        // Stacked: TV full width, at most TV_MAX_FRACTION of the height, and one whole row of
        // cards (at their real size) plus the dock must stay visible below it.
        val topCols = columnsFor(aw).coerceAtLeast(2)
        val minRow = cardHeight(cardW(aw, topCols)) + dockRoom
        val topH = minOf(aw / ratio, ah * TV_MAX_FRACTION, ah - GAP - minRow).coerceAtLeast(0f)
        val topArea = (topH * ratio).coerceAtMost(aw) * topH

        val side = forceSide ?: (sideArea >= SIDE_BIAS * topArea && sideW > 0f)
        return if (side) {
            val tvW = sideW.coerceAtLeast(aw * 0.35f).coerceAtMost(aw - GAP - CARD_MIN_W)
            // Frame hugs the picture: when the column is narrower than a full-height picture,
            // the frame gets shorter instead of leaving empty strips above and below.
            val tvH = min(ah, tvW / ratio)
            // Center vertically in the space above the floating dock so wide tall screens
            // (e.g. 16:10 tablets) have balanced whitespace above and below the TV.
            val tvY = ((ah - dockRoom - tvH) * 0.5f).coerceAtLeast(0f)
            val list = DpBox(tvW + GAP, 0f, aw - tvW - GAP, ah)
            HubLayoutResult(true, DpBox(0f, tvY, tvW, tvH), list, columnsFor(list.w))
        } else {
            // Never taller than the picture, so no empty strips above and below it.
            val tvH = max(topH, 60f)
            val list = DpBox(0f, tvH + GAP, aw, ah - tvH - GAP)
            HubLayoutResult(false, DpBox(0f, 0f, aw, tvH), list, topCols)
        }
    }
}
