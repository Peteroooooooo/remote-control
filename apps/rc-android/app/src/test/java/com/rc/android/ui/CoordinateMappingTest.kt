package com.rc.android.ui

import androidx.compose.ui.geometry.Offset
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Test

class CoordinateMappingTest {

    @Test
    fun landscapeZoomAndPanMatchesGlVideoSurfaceRect() {
        val cw = 2412f
        val ch = 1080f
        val sw = 2560
        val sh = 1440
        val scale = 2.8f
        val (px, py) = clampPanOffset(610f, -430f, scale, cw, ch, sw, sh)

        // Check multiple screen positions round-trip identically
        val points = listOf(
            Offset(120f, 90f),
            Offset(640f, 320f),
            Offset(1206f, 540f),
            Offset(1980f, 890f),
            Offset(2300f, 1020f),
        )
        for (p in points) {
            val mapped = mapScreenToStream(p, cw, ch, sw, sh, scale, px, py)
            assertNotNull(mapped)
            val (nx, ny) = mapped!!
            val back = mapStreamToScreen(nx, ny, cw, ch, sw, sh, scale, px, py)
            assertNotNull(back)
            assertEquals("x round-trip for $p", p.x, back!!.x, 0.05f)
            assertEquals("y round-trip for $p", p.y, back.y, 0.05f)
        }
    }

    @Test
    fun workbenchTopVideoZoomAndVerticalPanExact() {
        val cw = 1024f
        val ch = 576f
        val sw = 2560
        val sh = 1440
        val scale = 3.0f
        val px = 420f
        val py = -350f
        // GlVideoSurfaceView computes:
        // scaledW = 1024 * 3 = 3072, scaledH = 576 * 3 = 1728
        // left = 512 + 420 - 1536 = -604, top = 288 - 350 - 864 = -926
        val tap = Offset(300f, 200f)
        val (nx, ny) = mapScreenToStream(tap, cw, ch, sw, sh, scale, px, py)!!
        assertEquals((300f - (-604f)) / 3072f, nx, 1e-5f)
        assertEquals((200f - (-926f)) / 1728f, ny, 1e-5f)
    }

    @Test
    fun pinchZoomKeepsCentroidPointPinned() {
        val cw = 2412f
        val ch = 1080f
        val sw = 2560
        val sh = 1440
        var scale = 1.0f
        var panX = 0f
        var panY = 0f

        val centroid = Offset(720f, 260f)
        val before = mapScreenToStream(centroid, cw, ch, sw, sh, scale, panX, panY)!!

        // Simulate pinch zoom 1.0 -> 2.4 centered at `centroid` with finger shift (+40, -30)
        val newScale = 2.4f
        val dx = 40f
        val dy = -30f
        val scaleFactor = newScale / scale
        val rawPanX = (panX - (centroid.x - cw * 0.5f)) * scaleFactor + (centroid.x - cw * 0.5f) + dx
        val rawPanY = (panY - (centroid.y - ch * 0.5f)) * scaleFactor + (centroid.y - ch * 0.5f) + dy
        val (clampedX, clampedY) = clampPanOffset(rawPanX, rawPanY, newScale, cw, ch, sw, sh)
        scale = newScale
        panX = clampedX
        panY = clampedY

        // Only assert exact pinning when unclamped
        assertEquals(rawPanX, clampedX, 0.01f)
        assertEquals(rawPanY, clampedY, 0.01f)

        val after = mapScreenToStream(centroid + Offset(dx, dy), cw, ch, sw, sh, scale, panX, panY)!!
        assertEquals(before.first, after.first, 1e-4f)
        assertEquals(before.second, after.second, 1e-4f)
    }

    @Test
    fun autoFocusZoomsAndCentersWeChatOnPortraitPhone() {
        // 1080x2412 portrait phone viewing 2560x1440 monitor; WeChat is 960x720 centered around x=0.65, y=0.50
        val cw = 1080f
        val ch = 2412f
        val sw = 2560
        val sh = 1440
        val rw = 960f / 2560f
        val rh = 720f / 1440f
        val rx = 0.65f - rw * 0.5f
        val ry = 0.50f - rh * 0.5f

        val focus = computeWindowAutoFocus(rx, ry, rw, rh, cw, ch, sw, sh)
        assertNotNull(focus)
        val expectedZoom = (1f / rw) * 0.98f
        assertEquals(expectedZoom, focus!!.zoomScale, 0.01f)

        // Window center maps cleanly to horizontal screen center
        val center = mapStreamToScreen(
            rx + rw * 0.5f,
            ry + rh * 0.5f,
            cw,
            ch,
            sw,
            sh,
            focus.zoomScale,
            focus.panX,
            focus.panY,
        )!!
        assertEquals(cw * 0.5f, center.x, 1.5f)
        assertEquals(ch * 0.5f, center.y, 1.5f)
    }
}
