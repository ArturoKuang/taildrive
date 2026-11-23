package com.taildrive.android

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Shapes
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

internal data class DriveColors(val online: Color, val warning: Color, val danger: Color)
internal val LocalDriveColors = staticCompositionLocalOf {
    DriveColors(Color(0xFF16A34A), Color(0xFFD97706), Color(0xFFDC2626))
}

private val light = lightColorScheme(
    background = Color(0xFFFAFAFA),
    surface = Color.White,
    surfaceContainer = Color(0xFFF4F4F5),
    surfaceContainerHigh = Color(0xFFF4F4F5),
    surfaceContainerHighest = Color(0xFFF4F4F5),
    surfaceVariant = Color(0xFFF4F4F5),
    outline = Color(0xFF71717A),
    outlineVariant = Color(0xFFE4E4E7),
    onBackground = Color(0xFF111214),
    onSurface = Color(0xFF111214),
    onSurfaceVariant = Color(0xFF52525B),
    primary = Color(0xFF2563EB),
    onPrimary = Color.White,
    primaryContainer = Color(0xFFDCE7FF),
    onPrimaryContainer = Color(0xFF163C99),
    secondary = Color(0xFF52525B),
    secondaryContainer = Color(0xFFDCE7FF),
    onSecondaryContainer = Color(0xFF163C99),
    error = Color(0xFFDC2626),
)

private val dark = darkColorScheme(
    background = Color(0xFF0B0C0E),
    surface = Color(0xFF131518),
    surfaceContainer = Color(0xFF1B1E23),
    surfaceContainerHigh = Color(0xFF1B1E23),
    surfaceContainerHighest = Color(0xFF1B1E23),
    surfaceVariant = Color(0xFF1B1E23),
    outline = Color(0xFF777F8B),
    outlineVariant = Color(0xFF272A30),
    onBackground = Color(0xFFF4F4F6),
    onSurface = Color(0xFFF4F4F6),
    onSurfaceVariant = Color(0xFF9CA3AF),
    primary = Color(0xFF3B82F6),
    onPrimary = Color(0xFF070E1B),
    primaryContainer = Color(0xFF182E51),
    onPrimaryContainer = Color(0xFFCDE1FF),
    secondary = Color(0xFF9CA3AF),
    secondaryContainer = Color(0xFF182E51),
    onSecondaryContainer = Color(0xFFCDE1FF),
    error = Color(0xFFEF4444),
)

private fun font(size: Int, line: Int, weight: FontWeight = FontWeight.Normal) =
    TextStyle(fontSize = size.sp, lineHeight = line.sp, fontWeight = weight,
        fontFamily = FontFamily.SansSerif, fontFeatureSettings = "tnum")

internal val monospace = font(11, 16).copy(fontFamily = FontFamily.Monospace)
private val type = Typography(
    titleLarge = font(18, 24, FontWeight.SemiBold),
    titleMedium = font(14, 20, FontWeight.SemiBold),
    titleSmall = font(13, 18, FontWeight.SemiBold),
    bodyLarge = font(14, 20),
    bodyMedium = font(13, 18),
    bodySmall = font(11, 16),
    labelLarge = font(14, 20, FontWeight.Medium),
    labelMedium = font(12, 16, FontWeight.Medium),
    labelSmall = font(11, 16, FontWeight.Medium),
)

@Composable
internal fun TailDriveTheme(content: @Composable () -> Unit) {
    val darkMode = isSystemInDarkTheme()
    val status = if (darkMode) DriveColors(Color(0xFF22C55E), Color(0xFFF59E0B), Color(0xFFEF4444))
    else DriveColors(Color(0xFF16A34A), Color(0xFFD97706), Color(0xFFDC2626))
    CompositionLocalProvider(LocalDriveColors provides status) {
        MaterialTheme(
            colorScheme = if (darkMode) dark else light,
            typography = type,
            shapes = Shapes(extraSmall = RoundedCornerShape(6.dp), small = RoundedCornerShape(6.dp),
                medium = RoundedCornerShape(10.dp), large = RoundedCornerShape(10.dp),
                extraLarge = RoundedCornerShape(10.dp)),
            content = content,
        )
    }
}
