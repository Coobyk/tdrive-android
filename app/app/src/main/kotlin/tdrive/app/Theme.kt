package tdrive.app

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.sp

/**
 * The colours one theme carries.
 *
 * They are the original's own tokens — the same values the desktop shell
 * draws with — read out of the engine, so the phone and the desktop are the
 * same application wearing the same colours.
 */
data class Palette(
    val canvas: Color,
    val surface0: Color,
    val surface1: Color,
    val surface2: Color,
    val surface3: Color,
    val text: Color,
    val textSoft: Color,
    val textMuted: Color,
    val textSubtle: Color,
    val border: Color,
    val borderSoft: Color,
    val accent: Color,
    val accentHover: Color,
    val onAccent: Color,
    val danger: Color,
    val warning: Color,
    val success: Color,
) {
    companion object {
        /** The palette of a theme the engine lists. */
        fun of(theme: Theme): Palette? {
            val tokens = theme.tokens ?: return null
            fun token(name: String, fallback: Long): Color =
                argb(tokens[name] ?: fallback)
            return Palette(
                canvas = token("canvas", 0x0e171c),
                surface0 = token("surface0", 0x121e24),
                surface1 = token("surface1", 0x18272f),
                surface2 = token("surface2", 0x20343d),
                surface3 = token("surface3", 0x29454f),
                text = token("text", 0xe7f0f2),
                textSoft = token("textSoft", 0xc1d0d4),
                textMuted = token("textMuted", 0x94a7ad),
                textSubtle = token("textSubtle", 0x899da4),
                border = token("border", 0x5a737c),
                borderSoft = token("borderSoft", 0x30464f),
                accent = token("accent", 0x2aabee),
                accentHover = token("accentHover", 0x4bb8ef),
                onAccent = token("onAccent", 0x0e171c),
                danger = token("danger", 0xde6c76),
                warning = token("warning", 0xd6a34e),
                success = token("success", 0x49b889),
            )
        }
    }
}

/**
 * The application's colours.
 *
 * The type scale is the original's, in the same order: the same names, the
 * same weights, the same sizes.
 */
@Composable
fun TDriveTheme(
    theme: Theme?,
    content: @Composable () -> Unit,
) {
    val palette = remember(theme) { theme?.let { Palette.of(it) } }
    val dark = theme?.light == false
    val colors = if (dark) {
        darkColorScheme(
            primary = palette?.accent ?: Color(0x2aabee),
            onPrimary = palette?.onAccent ?: Color(0x0e171c),
            background = palette?.canvas ?: Color(0x0e171c),
            onBackground = palette?.text ?: Color(0xe7f0f2),
            surface = palette?.surface1 ?: Color(0x18272f),
            onSurface = palette?.textSoft ?: Color(0xc1d0d4),
            surfaceVariant = palette?.surface2 ?: Color(0x20343d),
            onSurfaceVariant = palette?.textMuted ?: Color(0x94a7ad),
            outline = palette?.borderSoft ?: Color(0x30464f),
            error = palette?.danger ?: Color(0xde6c76),
        )
    } else {
        lightColorScheme(
            primary = palette?.accent ?: Color(0x1689be),
            onPrimary = palette?.onAccent ?: Color(0x17262c),
            background = palette?.canvas ?: Color(0xeef4f5),
            onBackground = palette?.text ?: Color(0x17262c),
            surface = palette?.surface1 ?: Color(0xffffff),
            onSurface = palette?.textSoft ?: Color(0x354a52),
            surfaceVariant = palette?.surface2 ?: Color(0xe6eef0),
            onSurfaceVariant = palette?.textMuted ?: Color(0x5a6e75),
            outline = palette?.borderSoft ?: Color(0xd8e4e7),
            error = palette?.danger ?: Color(0xde6c76),
        )
    }

    // The original's type scale, in the same order.
    val type = Typography(
        labelSmall = TextStyle(fontSize = 11.sp, fontWeight = FontWeight.Bold),
        bodySmall = TextStyle(fontSize = 13.sp),
        bodyMedium = TextStyle(fontSize = 14.sp),
        bodyLarge = TextStyle(fontSize = 16.sp),
        titleMedium = TextStyle(fontSize = 16.sp, fontWeight = FontWeight.SemiBold),
        titleLarge = TextStyle(fontSize = 19.sp, fontWeight = FontWeight.Bold),
    )

    MaterialTheme(colorScheme = colors, typography = type, content = content)
}
