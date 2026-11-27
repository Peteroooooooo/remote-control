package com.rc.android.ui

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.Immutable
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.sp

@Immutable
data class RcColorPalette(
    val isDark: Boolean,
    val bg: Color,
    val bgGrouped: Color,
    val bgCard: Color,
    val bgField: Color,
    val separator: Color,
    val label: Color,
    val label2: Color,
    val label3: Color,
    val accent: Color,
    val green: Color,
    val warn: Color,
    val red: Color,
    val dockBg: Color,
    val dockBorder: Color,
    val dockSpecularTop: Color,
    val dockSep: Color,
    val dockDot: Color,
    val segTrack: Color,
    val segThumb: Color,
    val sheetBg: Color,
    val sheetBorder: Color,
    val sheetBackdrop: Color,
    val popBg: Color,
    val popBorder: Color,
)

val DarkRcPalette = RcColorPalette(
    isDark = true,
    bg = Color(0xFF000000),
    bgGrouped = Color(0xFF1C1C1E),
    bgCard = Color(0xFF161618),
    bgField = Color(0x38767680),
    separator = Color(0x61545458),
    label = Color(0xFFF5F5F7),
    label2 = Color(0x8FEBEBF5),
    label3 = Color(0x57EBEBF5),
    accent = Color(0xFF0A84FF),
    green = Color(0xFF30D158),
    warn = Color(0xFFFFD60A),
    red = Color(0xFFFF453A),
    dockBg = Color(0xD9222226),
    dockBorder = Color(0x2BFFFFFF),
    dockSpecularTop = Color(0x40FFFFFF),
    dockSep = Color(0x29FFFFFF),
    dockDot = Color(0xCCFFFFFF),
    segTrack = Color(0x14FFFFFF),
    segThumb = Color(0x29FFFFFF),
    sheetBg = Color(0xF21C1C1E),
    sheetBorder = Color(0x24FFFFFF),
    sheetBackdrop = Color(0x8A000000),
    popBg = Color(0xEB1E1E22),
    popBorder = Color(0x2BFFFFFF),
)

val LightRcPalette = RcColorPalette(
    isDark = false,
    bg = Color(0xFFF2F2F7),
    bgGrouped = Color(0xFFFFFFFF),
    bgCard = Color(0xFFFFFFFF),
    bgField = Color(0x1F767680),
    separator = Color(0x1F3C3C43),
    label = Color(0xFF1D1D1F),
    label2 = Color(0x993C3C43),
    label3 = Color(0x573C3C43),
    accent = Color(0xFF007AFF),
    green = Color(0xFF34C759),
    warn = Color(0xFFFF9F0A),
    red = Color(0xFFFF3B30),
    dockBg = Color(0xEBFFFFFF),
    dockBorder = Color(0x18000000),
    dockSpecularTop = Color(0xFFFFFFFF),
    dockSep = Color(0x1A000000),
    dockDot = Color(0x99000000),
    segTrack = Color(0x12000000),
    segThumb = Color(0xFFFFFFFF),
    sheetBg = Color(0xF7FFFFFF),
    sheetBorder = Color(0x14000000),
    sheetBackdrop = Color(0x52000000),
    popBg = Color(0xF2FFFFFF),
    popBorder = Color(0x18000000),
)

val LocalRcColors = staticCompositionLocalOf { DarkRcPalette }

object RcThemeColors {
    val current: RcColorPalette
        @Composable get() = LocalRcColors.current
}

private val RcTypography = Typography(
    headlineMedium = TextStyle(
        fontWeight = FontWeight.SemiBold,
        fontSize = 22.sp,
        letterSpacing = (-0.4).sp,
    ),
    titleMedium = TextStyle(
        fontWeight = FontWeight.SemiBold,
        fontSize = 17.sp,
        letterSpacing = (-0.25).sp,
    ),
    bodyMedium = TextStyle(
        fontWeight = FontWeight.Normal,
        fontSize = 14.sp,
    ),
    labelSmall = TextStyle(
        fontFamily = FontFamily.Monospace,
        fontWeight = FontWeight.Normal,
        fontSize = 11.sp,
    ),
)

@Composable
fun RcTheme(
    darkTheme: Boolean = isSystemInDarkTheme(),
    content: @Composable () -> Unit,
) {
    val palette = if (darkTheme) DarkRcPalette else LightRcPalette
    val colorScheme = if (darkTheme) {
        darkColorScheme(
            primary = palette.accent,
            onPrimary = Color.White,
            background = palette.bg,
            onBackground = palette.label,
            surface = palette.bgGrouped,
            onSurface = palette.label,
            surfaceVariant = palette.bgCard,
            onSurfaceVariant = palette.label2,
            outline = palette.separator,
            error = palette.red,
        )
    } else {
        lightColorScheme(
            primary = palette.accent,
            onPrimary = Color.White,
            background = palette.bg,
            onBackground = palette.label,
            surface = palette.bgGrouped,
            onSurface = palette.label,
            surfaceVariant = palette.bgCard,
            onSurfaceVariant = palette.label2,
            outline = palette.separator,
            error = palette.red,
        )
    }

    CompositionLocalProvider(LocalRcColors provides palette) {
        MaterialTheme(
            colorScheme = colorScheme,
            typography = RcTypography,
            content = content,
        )
    }
}
