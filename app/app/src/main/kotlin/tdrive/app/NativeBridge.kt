package tdrive.app

import androidx.compose.ui.graphics.Color
import org.json.JSONArray
import org.json.JSONObject

/**
 * The bridge to the engine.
 *
 * Every call answers with a JSON document: the engine is Rust, the application
 * is Kotlin, and a document is something both sides read without a schema
 * either of them has to keep.
 */
object NativeBridge {

    /**
     * The engine's library, loaded when it is first needed.
     *
     * The name is the library's own: Android's loader takes it verbatim, and
     * Rust names a library with underscores where the crate has dashes, so
     * `tdrive_engine` is the name and `tdrive-engine` is not.
     */
    private var loaded = false

    /** Why the library could not be loaded, when it could not. */
    var loadError: String? = null
        private set

    /**
     * Load the engine's library.
     *
     * A failure here is reported rather than thrown: an application that
     * cannot load its engine should say so on the screen, not vanish.
     */
    fun load() {
        if (loaded) return
        loaded = true
        try {
            System.loadLibrary("tdrive_engine")
        } catch (error: UnsatisfiedLinkError) {
            loadError = error.message ?: "the engine's library could not be loaded"
        } catch (error: SecurityException) {
            loadError = error.message ?: "the engine's library could not be loaded"
        }
    }

    /**
     * Set the engine up, and tell it where the application's files live.
     *
     * A phone has one place it may write to and no platform place of its own,
     * so the application says which.
     */
    external fun nativeInit(dataDir: String): String

    /** The credentials the application has stored, if it has any. */
    external fun nativeCredentials(): String

    /** `ready` when the account is signed in, `none` when it is not. */
    external fun nativeSessionState(): String

    /** Ask for the code to be sent to the account. */
    external fun nativeSignIn(apiId: Int, apiHash: String, phone: String): String

    /** Check the code; the answer says whether a password is still wanted. */
    external fun nativeVerifyCode(code: String): String

    /** Check the password the account has of its own. */
    external fun nativeVerifyPassword(password: String): String

    /** Forget the account. */
    external fun nativeSignOut(): String

    /** The channels the account may use as a drive. */
    external fun nativeListDrives(): String

    /** Open a drive and read it. */
    external fun nativeOpenDrive(channelId: Long, accessHash: Long): String

    /** The drive that is open. */
    external fun nativeDriveState(): String

    /** The files of the drive, and its trash. */
    external fun nativeFilesJson(): String

    /** Unlock the drive. */
    external fun nativeUnlock(password: String): String

    /** Fetch a file to a place on the phone. */
    external fun nativeDownload(messageId: Int, dest: String): String

    /** Send a file to the drive. */
    external fun nativeUpload(path: String, parent: String, encrypt: Boolean): String

    /** Move a file to the trash, or put it back. */
    external fun nativeTrash(objectId: String, revision: Long, restore: Boolean): String

    /** Create a folder. */
    external fun nativeMkdir(parent: String, name: String): String

    /** The small picture published beside a file. */
    external fun nativeDerivative(messageId: Int): String

    /** The theme catalogue. */
    external fun nativeThemes(): String
}

/** The answer the engine gives: either it worked, or it says why it did not. */
sealed class Answer {
    /** It worked. */
    data object Ok : Answer()

    /** It did not, and says why. */
    data class Failed(val message: String) : Answer()
}

/** Read an answer. */
fun answerOf(raw: String): Answer {
    val json = runCatching { JSONObject(raw) }.getOrNull()
        ?: return Answer.Failed("The engine answered something unreadable.")
    if (json.optBoolean("ok", false)) return Answer.Ok
    return Answer.Failed(json.optString("error", "Something went wrong."))
}

/** The answer, and whatever else it carried. */
fun jsonOf(raw: String): JSONObject? = runCatching { JSONObject(raw) }.getOrNull()

/** A file in the drive, as the phone reads it. */
data class DriveFile(
    val messageId: Int,
    val objectId: String,
    val name: String,
    val size: Long,
    val kind: String,
    val date: String?,
    val parent: String,
    val encrypted: Boolean,
    val revision: Long,
    /** The message holding the small picture beside this file, if it has one. */
    val derivative: Int?,
) {
    /** Whether this file is a picture the gallery can show. */
    val isPicture: Boolean get() = kind == "image" || kind == "video"

    /** Whether this file is a folder. */
    val isFolder: Boolean get() = kind == "folder"
}

/** A drive the account may use. */
data class DriveCandidate(
    val id: Long,
    val accessHash: Long,
    val title: String,
    val created: Long,
    val hasActivity: Boolean,
)

/** A file in the trash. */
data class TrashFile(
    val objectId: String,
    val name: String,
    val size: Long,
    val deletedAt: String,
    val origin: String,
)

/** The drive that is open. */
data class DriveState(
    val id: Long,
    val title: String,
    val items: Int,
    val size: Long,
    val encrypted: Boolean,
    val unlocked: Boolean,
    val hint: String,
    val trash: Int,
)

/** One theme in the catalogue. */
data class Theme(
    val id: String,
    val name: String,
    val light: Boolean,
    val preview: List<Long>,
    /** The whole palette, as the engine's own tokens. */
    val tokens: Map<String, Long>? = null,
)

/**
 * A colour as the engine holds it: `0xRRGGBB`, or `0xRRGGBBAA` when the token
 * is translucent, turned into the form Compose reads.
 */
fun argb(value: Long): Color {
    val alpha = if (value > 0xFFFFFF) (value shr 24) and 0xFF else 0xFFL
    val red = (value shr 16) and 0xFF
    val green = (value shr 8) and 0xFF
    val blue = value and 0xFF
    return Color((alpha shl 24) or (red shl 16) or (green shl 8) or blue)
}

/** Read the files the engine lists. */
fun filesOf(raw: String): Pair<List<DriveFile>, List<TrashFile>> {
    val json = jsonOf(raw) ?: return emptyList<DriveFile>() to emptyList<TrashFile>()
    val files = json.optJSONArray("files") ?: JSONArray()
    val rows = (0 until files.length()).map { index ->
        val file = files.getJSONObject(index)
        DriveFile(
            messageId = file.optInt("messageId"),
            objectId = file.optString("objectId"),
            name = file.optString("name"),
            size = file.optLong("size"),
            kind = file.optString("kind", "other"),
            date = file.optString("date").takeIf { it.isNotEmpty() && it != "null" },
            parent = file.optString("parent"),
            encrypted = file.optBoolean("encrypted"),
            revision = file.optLong("revision", 1),
            derivative = if (file.isNull("derivative")) null else file.optInt("derivative"),
        )
    }
    val trash = json.optJSONArray("trash") ?: JSONArray()
    val trashed = (0 until trash.length()).map { index ->
        val entry = trash.getJSONObject(index)
        TrashFile(
            objectId = entry.optString("objectId"),
            name = entry.optString("name"),
            size = entry.optLong("size"),
            deletedAt = entry.optString("deletedAt"),
            origin = entry.optString("origin"),
        )
    }
    return rows to trashed
}

/** Read the drives the engine lists. */
fun drivesOf(raw: String): List<DriveCandidate> {
    val json = jsonOf(raw) ?: return emptyList()
    val drives = json.optJSONArray("drives") ?: JSONArray()
    return (0 until drives.length()).map { index ->
        val drive = drives.getJSONObject(index)
        DriveCandidate(
            id = drive.optLong("id"),
            accessHash = drive.optLong("accessHash"),
            title = drive.optString("title"),
            created = drive.optLong("created"),
            hasActivity = drive.optBoolean("hasActivity"),
        )
    }
}

/** Read the drive that is open. */
fun driveOf(raw: String): DriveState? {
    val json = jsonOf(raw) ?: return null
    if (!json.optBoolean("ok", false)) return null
    return DriveState(
        id = json.optLong("id"),
        title = json.optString("title"),
        items = json.optInt("items"),
        size = json.optLong("size"),
        encrypted = json.optBoolean("encrypted"),
        unlocked = json.optBoolean("unlocked"),
        hint = json.optString("hint"),
        trash = json.optInt("trash"),
    )
}

/** Read the theme catalogue. */
fun themesOf(raw: String): Pair<List<Theme>, String> {
    val json = jsonOf(raw) ?: return emptyList<Theme>() to "quiet-relay"
    val themes = json.optJSONArray("themes") ?: JSONArray()
    val list = (0 until themes.length()).map { index ->
        val theme = themes.getJSONObject(index)
        val preview = theme.optJSONArray("preview") ?: JSONArray()
        val palette = theme.optJSONObject("palette")
        val tokens = palette?.let { object_ ->
            object_.keys().asSequence().associateWith { key -> object_.optLong(key, 0L) }
        }
        Theme(
            id = theme.optString("id"),
            name = theme.optString("name"),
            light = theme.optBoolean("light"),
            preview = (0 until preview.length()).map { preview.getLong(it) },
            tokens = tokens,
        )
    }
    return list to json.optString("active", "quiet-relay")
}

/** Read the picture published beside a file. */
fun pictureOf(raw: String): ByteArray? {
    val json = jsonOf(raw) ?: return null
    if (!json.optBoolean("ok", false)) return null
    val bytes = json.optJSONArray("bytes") ?: return null
    val out = ByteArray(bytes.length())
    for (index in 0 until bytes.length()) {
        out[index] = bytes.getLong(index).toByte()
    }
    return out
}
