package com.rc.android.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class HubLayoutTest {
    private val r169 = 16f / 9f

    @Test fun phonePortraitStacksWithTwoColumns() {
        val l = HubLayout.compute(384f, 790f, r169)
        assertFalse(l.sideBySide)
        assertEquals(2, l.columns)
        assertEquals("TV hugs a 16:9 picture", 384f / r169, l.tv.h, 0.5f)
    }

    @Test fun phoneLandscapeGoesSideBySideWithTwoCardColumns() {
        val l = HubLayout.compute(887f, 300f, r169)
        assertTrue(l.sideBySide)
        assertEquals(2, l.columns)
        assertEquals(300f, l.tv.h, 0.01f)
    }

    @Test fun unfoldedFoldableStacksEvenWhenHeldSideways() {
        assertFalse(HubLayout.compute(813f, 560f, r169).sideBySide)
        assertFalse(HubLayout.compute(645f, 740f, r169).sideBySide)
    }

    @Test fun tabletLandscapeDependsOnPcShape() {
        val wide = HubLayout.compute(1252f, 690f, r169)
        assertTrue(wide.sideBySide)
        assertTrue("tablet side-by-side TV centers vertically above dock", wide.tv.y > 20f)
        assertTrue("tablet TV stays above the floating dock", wide.tv.y + wide.tv.h <= 690f - HubLayout.DOCK_ROOM + 0.5f)
        val ultrawide = HubLayout.compute(1252f, 690f, 21f / 9f)
        assertFalse(ultrawide.sideBySide)
        assertTrue("ultrawide TV takes most of the height", ultrawide.tv.h > 690f * 0.55f)
    }

    @Test fun phoneLandscapeTvStaysTopAligned() {
        val l = HubLayout.compute(887f, 300f, r169)
        assertTrue(l.sideBySide)
        assertEquals(0f, l.tv.y, 0.01f)
    }

    @Test fun tabletPortraitUsesFourColumns() {
        val l = HubLayout.compute(772f, 1170f, r169)
        assertFalse(l.sideBySide)
        assertEquals(4, l.columns)
    }

    @Test fun portraitMonitorOnLandscapePhoneLeavesWideCardArea() {
        val l = HubLayout.compute(887f, 300f, 9f / 16f)
        assertTrue(l.sideBySide)
        assertTrue(l.columns >= 3)
    }

    @Test fun stackedAlwaysShowsOneFullCardRowAboveTheDock() {
        for ((w, h) in listOf(384f to 790f, 813f to 560f, 996f to 640f, 384f to 360f, 772f to 1170f)) {
            for (ratio in listOf(r169, 1.6f, 21f / 9f, 32f / 9f, 4f / 3f, 1f, 9f / 16f)) {
                val l = HubLayout.compute(w, h, ratio)
                if (l.sideBySide) continue
                val cw = (l.list.w - (l.columns - 1) * HubLayout.CARD_GAP) / l.columns
                val need = HubLayout.cardHeight(cw) + HubLayout.DOCK_ROOM
                assertTrue("w=$w h=$h r=$ratio list=${l.list.h} need=$need", l.list.h + 0.5f >= need || l.tv.h <= 60.5f)
            }
        }
    }

    @Test fun frameNeverLeavesStripsAroundAWidePicture() {
        for ((w, h) in listOf(887f to 300f, 887f to 360f, 1252f to 690f, 384f to 790f, 813f to 560f)) {
            for (ratio in listOf(r169, 1.6f, 21f / 9f, 32f / 9f)) {
                val l = HubLayout.compute(w, h, ratio)
                assertTrue("w=$w h=$h r=$ratio tv=${l.tv}", l.tv.h <= l.tv.w / ratio + 0.5f)
            }
        }
    }

    @Test fun forcedDecisionIsKept() {
        assertTrue(HubLayout.compute(384f, 790f, r169, forceSide = true).sideBySide)
        assertFalse(HubLayout.compute(887f, 300f, r169, forceSide = false).sideBySide)
    }
}
