package tdrive.app

import android.content.Context
import android.net.Uri
import android.provider.OpenableColumns
import java.io.File

/**
 * Copy a document the phone handed over into the application's own storage.
 *
 * The engine reads files by path, and a document the user picked is a
 * content address rather than a path, so it is copied somewhere the engine
 * can reach before it is sent.
 */
fun copyToCache(context: Context, uri: Uri): String? = runCatching {
    val name = documentName(context, uri) ?: "upload"
    val folder = File(context.cacheDir, "uploads")
    folder.mkdirs()
    val target = File(folder, name)
    context.contentResolver.openInputStream(uri)?.use { input ->
        target.outputStream().use { output -> input.copyTo(output) }
    } ?: return null
    target.absolutePath
}.getOrNull()

/** The name of a document, as the phone knows it. */
private fun documentName(context: Context, uri: Uri): String? {
    context.contentResolver.query(uri, null, null, null, null)?.use { cursor ->
        val index = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
        if (index >= 0 && cursor.moveToFirst()) {
            return cursor.getString(index)
        }
    }
    return uri.lastPathSegment
}
