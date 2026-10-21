package tdrive.app

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.io.File

/** Where the application is. */
enum class Screen {
    /** Not signed in yet. */
    SignIn,

    /** Signed in, but no drive chosen yet. */
    DrivePicker,

    /** The drive is open. */
    Drive,
}

/** What the application is doing, as the screen draws it. */
data class TDriveState(
    val screen: Screen = Screen.SignIn,
    val working: Boolean = false,
    val message: String? = null,
    val error: String? = null,

    // The sign-in.
    val apiId: String = "",
    val apiHash: String = "",
    val phone: String = "",
    val code: String = "",
    val password: String = "",
    val needsPassword: Boolean = false,
    val codeSent: Boolean = false,

    // The drive.
    val drives: List<DriveCandidate> = emptyList(),
    val drive: DriveState? = null,
    val files: List<DriveFile> = emptyList(),
    val trash: List<TrashFile> = emptyList(),
    val folder: String = "",
    val path: List<Pair<String, String>> = emptyList(),

    // The appearance.
    val themes: List<Theme> = emptyList(),
    val themeId: String = "quiet-relay",
) {
    /** The theme in use. */
    val theme: Theme? get() = themes.firstOrNull { it.id == themeId }
}

/**
 * The application's state.
 *
 * Every call into the engine happens here, off the main thread, and the
 * answer comes back as a plain value the screens draw.
 */
class TDriveViewModel(application: Application) : AndroidViewModel(application) {

    private val _state = MutableStateFlow(TDriveState())
    val state: StateFlow<TDriveState> = _state.asStateFlow()

    /** The pictures that have been fetched, by the file they belong to. */
    private val pictures = mutableMapOf<Int, ByteArray>()

    init {
        run {
            // The engine's library is loaded first, and a failure is reported
            // rather than thrown: an application that cannot load its engine
            // should say so, not vanish.
            NativeBridge.load()
            val failure = NativeBridge.loadError
            if (failure != null) {
                _state.value = _state.value.copy(
                    error = "The engine could not be loaded: $failure",
                )
                return@run
            }
            // The engine keeps its files where the application says: a phone
            // has one place it may write to.
            val files = getApplication<Application>().filesDir.absolutePath
            NativeBridge.nativeInit(files)
            // The credentials the application has stored, if it has any: the
            // sign-in screen comes up filled in rather than empty.
            val stored = jsonOf(NativeBridge.nativeCredentials())
            val apiId = stored?.optInt("apiId") ?: 0
            val apiHash = stored?.optString("apiHash") ?: "" 
            val (themes, active) = themesOf(NativeBridge.nativeThemes())
            _state.value = _state.value.copy(
                themes = themes,
                themeId = active,
                apiId = if (apiId > 0) apiId.toString() else _state.value.apiId,
                apiHash = if (apiHash.isNotEmpty()) apiHash else _state.value.apiHash,
            )
            if (NativeBridge.nativeSessionState() == "ready") {
                openTheSavedDrive()
            }
        }
    }

    /** Change what is on screen. */
    private fun update(block: (TDriveState) -> TDriveState) {
        _state.value = block(_state.value)
    }

    /**
     * Do something that takes time, off the main thread.
     *
     * The block answers with the state to carry on with and the error to
     * show, if there was one; a state of `null` means the state on screen
     * stands.
     */
    private fun work(
        message: String? = null,
        next: ((TDriveState) -> Unit)? = null,
        block: suspend () -> Pair<TDriveState?, String?>?,
    ) {
        update { it.copy(working = true, message = message, error = null) }
        viewModelScope.launch {
            val (state, error) = withContext(Dispatchers.IO) { block() } ?: return@launch
            var carried = _state.value
            update { current ->
                val base = state ?: current
                carried = base.copy(working = false, message = null, error = error)
                carried
            }
            if (error == null) next?.invoke(carried)
        }
    }

    // --- The sign-in --------------------------------------------------------

    /** Set the application credentials. */
    fun setCredentials(apiId: String, apiHash: String) {
        update { it.copy(apiId = apiId, apiHash = apiHash) }
    }

    /** Set the account's number. */
    fun setPhone(phone: String) {
        update { it.copy(phone = phone) }
    }

    /** Set the code the account was sent. */
    fun setCode(code: String) {
        update { it.copy(code = code) }
    }

    /** Set the password the account has of its own. */
    fun setPassword(password: String) {
        update { it.copy(password = password) }
    }

    /** Ask for the code to be sent. */
    fun signIn() {
        val current = _state.value
        work(
            message = "Signing in...",
            next = { state ->
                // The account is in: the drive is next.
                if (!state.codeSent) openTheSavedDrive()
            },
        ) {
            val answer = NativeBridge.nativeSignIn(
                current.apiId.toIntOrNull() ?: 0,
                current.apiHash,
                current.phone,
            )
            when (val result = answerOf(answer)) {
                is Answer.Failed -> null to result.message
                Answer.Ok -> {
                    val signedIn = NativeBridge.nativeSessionState() == "ready"
                    _state.value.copy(
                        screen = Screen.SignIn,
                        codeSent = !signedIn,
                    ) to null
                }
            }
        }
    }

    /** Check the code. */
    fun verifyCode() {
        val code = _state.value.code
        work(
            message = "Checking the code...",
            next = { state ->
                // A password is asked for when the account has one of its own.
                if (!state.needsPassword) openTheSavedDrive()
            },
        ) {
            val raw = NativeBridge.nativeVerifyCode(code)
            val json = jsonOf(raw)
            when (val result = answerOf(raw)) {
                is Answer.Failed -> null to result.message
                Answer.Ok -> {
                    if (json?.optBoolean("needsPassword") == true) {
                        _state.value.copy(needsPassword = true) to null
                    } else {
                        // The account is in: the drive is next.
                        null to null
                    }
                }
            }
        }
    }

    /** Check the password. */
    fun verifyPassword() {
        val password = _state.value.password
        work(
            message = "Checking the password...",
            next = { openTheSavedDrive() },
        ) {
            when (val result = answerOf(NativeBridge.nativeVerifyPassword(password))) {
                is Answer.Failed -> null to result.message
                Answer.Ok -> {
                    _state.value.copy(needsPassword = false) to null
                }
            }
        }
    }

    // --- The drive ----------------------------------------------------------

    /**
     * Open the drive this device remembers, and offer the others when it
     * remembers none.
     *
     * The drive is the one the drive list says is the account's own, so the
     * reader is not asked to pick the same one on every launch.
     */
    private fun openTheSavedDrive() {
        work("Opening the drive...") {
            val saved = jsonOf(NativeBridge.nativeSavedDrive())
            val savedId = saved?.optLong("id") ?: 0
            val drives = drivesOf(NativeBridge.nativeListDrives())
            val candidate = drives.firstOrNull { it.id == savedId }
            if (candidate == null) {
                // Nothing remembered, or the drive is gone: the reader picks.
                _state.value.copy(screen = Screen.DrivePicker, drives = drives) to null
            } else {
                val answer = NativeBridge.nativeOpenDrive(candidate.id, candidate.accessHash)
                when (val result = answerOf(answer)) {
                    is Answer.Failed -> _state.value.copy(drives = drives) to result.message
                    Answer.Ok -> {
                        readTheDrive()
                        _state.value.copy(
                            screen = Screen.Drive,
                            drives = drives,
                            drive = driveOf(answer),
                        ) to null
                    }
                }
            }
        }
    }

    /** The drives this account may use. */
    fun listDrives() {
        work("Looking for drives...") {
            val drives = drivesOf(NativeBridge.nativeListDrives())
            _state.value.copy(screen = Screen.DrivePicker, drives = drives) to null
        }
    }

    /** Open a drive. */
    fun openDrive(candidate: DriveCandidate) {
        work("Opening ${candidate.title}...") {
            val answer = NativeBridge.nativeOpenDrive(candidate.id, candidate.accessHash)
            when (val result = answerOf(answer)) {
                is Answer.Failed -> null to result.message
                Answer.Ok -> {
                    readTheDrive()
                    _state.value.copy(
                        screen = Screen.Drive,
                        drive = driveOf(answer),
                    ) to null
                }
            }
        }
    }

    /** Read the drive's files into the state. */
    private fun readTheDrive() {
        val (files, trash) = filesOf(NativeBridge.nativeFilesJson())
        _state.value = _state.value.copy(files = files, trash = trash)
    }

    /** Re-read the drive. */
    fun refresh() {
        val current = _state.value
        work("Reloading...") {
            val answer = NativeBridge.nativeOpenDrive(current.drive?.id ?: 0, 0)
            // The drive is opened by its own id; the access hash is not needed
            // to read it again, so the answer that matters is the files.
            val (files, trash) = filesOf(NativeBridge.nativeFilesJson())
            _state.value.copy(
                files = files,
                trash = trash,
                drive = driveOf(NativeBridge.nativeDriveState()),
            ) to if (answerOf(answer) is Answer.Failed) null else null
        }
    }

    /** Go into a folder. */
    fun enterFolder(file: DriveFile) {
        update {
            it.copy(
                folder = file.objectId,
                path = it.path + (file.objectId to file.name),
            )
        }
    }

    /** Go up a folder. */
    fun leaveFolder() {
        update { state ->
            val path = state.path.dropLast(1)
            state.copy(folder = path.lastOrNull()?.first ?: "", path = path)
        }
    }

    // --- The files ----------------------------------------------------------

    /** Download a file to the phone's own storage. */
    fun download(file: DriveFile) {
        work("Downloading ${file.name}...") {
            val folder = File(getApplication<Application>().filesDir, "downloads")
            folder.mkdirs()
            val target = File(folder, file.name)
            when (val result = answerOf(NativeBridge.nativeDownload(file.messageId, target.absolutePath))) {
                is Answer.Failed -> null to result.message
                Answer.Ok -> _state.value.copy(message = "Saved ${file.name}") to null
            }
        }
    }

    /** Send a file to the drive. */
    fun upload(path: String, encrypt: Boolean) {
        work("Uploading...") {
            val parent = _state.value.folder
            when (val result = answerOf(NativeBridge.nativeUpload(path, parent, encrypt))) {
                is Answer.Failed -> null to result.message
                Answer.Ok -> {
                    readTheDrive()
                    _state.value.copy(message = "Uploaded") to null
                }
            }
        }
    }

    /** Create a folder. */
    fun createFolder(name: String) {
        work("Creating the folder...") {
            val parent = _state.value.folder
            when (val result = answerOf(NativeBridge.nativeMkdir(parent, name))) {
                is Answer.Failed -> null to result.message
                Answer.Ok -> {
                    readTheDrive()
                    _state.value.copy(message = "Created $name") to null
                }
            }
        }
    }

    /** Move a file to the trash, and put it back. */
    fun trash(file: DriveFile, restore: Boolean) {
        work(if (restore) "Restoring..." else "Moving to the trash...") {
            val answer = NativeBridge.nativeTrash(file.objectId, file.revision, restore)
            when (val result = answerOf(answer)) {
                is Answer.Failed -> null to result.message
                Answer.Ok -> {
                    readTheDrive()
                    _state.value.copy(message = if (restore) "Restored" else "Moved to the trash") to null
                }
            }
        }
    }

    /** Unlock the drive. */
    fun unlock(password: String) {
        work("Unlocking...") {
            when (val result = answerOf(NativeBridge.nativeUnlock(password))) {
                is Answer.Failed -> null to result.message
                Answer.Ok -> {
                    readTheDrive()
                    _state.value.copy(
                        message = "Unlocked",
                        drive = driveOf(NativeBridge.nativeDriveState()),
                    ) to null
                }
            }
        }
    }

    // --- The gallery --------------------------------------------------------

    /** The picture published beside a file, if it has one. */
    fun picture(file: DriveFile): ByteArray? {
        pictures[file.messageId]?.let { return it }
        val bytes = pictureOf(NativeBridge.nativeDerivative(file.messageId)) ?: return null
        pictures[file.messageId] = bytes
        return bytes
    }

    // --- The appearance -----------------------------------------------------

    /** Put a theme on screen. */
    fun chooseTheme(id: String) {
        update { it.copy(themeId = id) }
    }

    /** Forget the account. */
    fun signOut() {
        work("Signing out...") {
            NativeBridge.nativeSignOut()
            _state.value.copy(
                screen = Screen.SignIn,
                drive = null,
                files = emptyList(),
                trash = emptyList(),
            ) to null
        }
    }
}
