package dev.tdrive.app

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import org.json.JSONArray
import org.json.JSONObject

data class ChannelInfo(val id: Long, val accessHash: Long, val title: String, val kind: String)

data class NsEntry(
    val id: String,
    val name: String,
    val kind: String,       // "file" | "dir"
    val size: Long,
    val parent: String?,
    val trashed: Boolean,
)

data class Transfer(
    val id: String,
    val label: String,
    val sent: Long,
    val total: Long,
    val state: String,
)

data class Vault(val configured: Boolean, val unlocked: Boolean, val hint: String)

sealed class Screen {
    data object Setup : Screen()
    data object Phone : Screen()
    data object Code : Screen()
    data class Password(val hint: String) : Screen()
    data object Channels : Screen()
    data object Browser : Screen()
}

class Core(private val scope: CoroutineScope) {

    private val _screen = MutableStateFlow<Screen>(Screen.Setup)
    val screen = _screen.asStateFlow()

    private val _channels = MutableStateFlow<List<ChannelInfo>>(emptyList())
    val channels = _channels.asStateFlow()

    private val _entries = MutableStateFlow<List<NsEntry>>(emptyList())
    val entries = _entries.asStateFlow()

    private val _cwd = MutableStateFlow("")
    val cwd = _cwd.asStateFlow()

    private val _driveTitle = MutableStateFlow("")
    val driveTitle = _driveTitle.asStateFlow()

    private val _transfers = MutableStateFlow<Map<String, Transfer>>(emptyMap())
    val transfers = _transfers.asStateFlow()

    private val _status = MutableStateFlow("")
    val status = _status.asStateFlow()

    private val _vault = MutableStateFlow(Vault(false, false, ""))
    val vault = _vault.asStateFlow()

    private val _trashMode = MutableStateFlow(false)
    val trashMode = _trashMode.asStateFlow()

    @Volatile
    private var running = true

    private lateinit var dataDir: String

    fun start(dataDir: String) {
        this.dataDir = dataDir
        NativeCore.nativeInit(dataDir)
        NativeCore.nativeSpawn()
        scope.launch(Dispatchers.IO) { pollLoop() }
    }

    private fun send(cmd: String, args: JSONObject = JSONObject()) {
        val res = NativeCore.nativeDispatch(JSONObject().put("cmd", cmd).put("args", args).toString())
        if (!JSONObject(res).optBoolean("ok", false)) {
            _status.value = "dispatch failed: $res"
        }
    }

    fun connect(apiId: Int, apiHash: String) =
        send("Connect", JSONObject().put("api_id", apiId).put("api_hash", apiHash))

    fun requestCode(phone: String) = send("RequestCode", JSONObject().put("phone", phone))
    fun submitCode(code: String) = send("SubmitCode", JSONObject().put("code", code))
    fun submitPassword(pw: String) = send("SubmitPassword", JSONObject().put("password", pw))
    fun listChannels() = send("ListChannels")
    fun createChannel(title: String) = send("CreateChannel", JSONObject().put("title", title))
    fun createSharedChannel(title: String) = send("CreateSharedChannel", JSONObject().put("title", title))
    fun openChannel(c: ChannelInfo) =
        send("OpenChannel", JSONObject().put("id", c.id).put("access_hash", c.accessHash)
            .put("title", c.title).put("kind", c.kind))
    fun sync() = send("Sync")
    fun upload(path: String, parent: String = _cwd.value) =
        send("Upload", JSONObject().put("path", path).put("parent", parent))
    fun download(fileId: String, name: String) {
        val dir = java.io.File(dataDir, "downloads").apply { mkdirs() }
        send("Download", JSONObject().put("file_id", fileId).put("dest", java.io.File(dir, name).absolutePath))
    }
    fun mkdir(name: String) =
        send("Mkdir", JSONObject().put("parent", _cwd.value).put("name", name))
    fun rename(obj: String, name: String) =
        send("Rename", JSONObject().put("obj", obj).put("name", name))
    fun move(obj: String, parent: String) =
        send("Move", JSONObject().put("obj", obj).put("parent", parent))
    fun delete(obj: String) = send("Delete", JSONObject().put("obj", obj))
    fun restore(obj: String) = send("Restore", JSONObject().put("obj", obj))
    fun purge(obj: String) = send("Purge", JSONObject().put("obj", obj))
    fun emptyTrash() = send("EmptyTrash")
    fun cancelTransfer(id: String) = send("Cancel", JSONObject().put("id", id))
    fun unlock(password: String) = send("Unlock", JSONObject().put("password", password))
    fun lock() = send("Lock")
    fun enableEncryption(password: String, hint: String) =
        send("EnableEncryption", JSONObject().put("password", password).put("hint", hint))
    fun optimize() = send("OptimizeDrive")
    fun optimizeApply() = send("OptimizeApply")
    fun fetchThumbnail(fileId: String) = send("FetchThumbnail", JSONObject().put("file_id", fileId))
    fun fetchPreview(fileId: String) = send("FetchPreview", JSONObject().put("file_id", fileId))
    fun signOut() = send("SignOut")
    fun resumeUploads() = send("ResumeUploads")
    fun liveSync() = send("LiveSync")
    fun setTrashMode(v: Boolean) { _trashMode.value = v }
    fun setCwd(id: String) { _cwd.value = id }

    private suspend fun pollLoop() {
        while (running) {
            val raw = NativeCore.nativePoll()
            if (raw.isNullOrEmpty()) {
                delay(120)
                continue
            }
            try {
                handle(JSONObject(raw))
            } catch (e: Exception) {
                _status.value = "bad event: ${e.message}"
            }
        }
    }

    private fun handle(ev: JSONObject) {
        when (ev.getString("ev")) {
            "Connected" -> {
                val authorized = ev.getJSONObject("args").getBoolean("authorized")
                _screen.value = if (authorized) Screen.Channels else Screen.Phone
                if (authorized) listChannels()
            }
            "CodeSent" -> _screen.value = Screen.Code
            "NeedPassword" -> _screen.value = Screen.Password(ev.getJSONObject("args").optString("hint"))
            "SignedIn" -> {
                _screen.value = Screen.Channels
                listChannels()
            }
            "Channels" -> {
                val arr = ev.getJSONObject("args").getJSONArray("0")
                val list = mutableListOf<ChannelInfo>()
                for (i in 0 until arr.length()) {
                    val c = arr.getJSONObject(i)
                    list.add(ChannelInfo(c.getLong("id"), c.getLong("access_hash"), c.getString("title"), c.getString("kind")))
                }
                _channels.value = list
            }
            "ChannelOpened" -> {
                val args = ev.getJSONObject("args")
                val c = args.getJSONObject("info")
                _driveTitle.value = c.getString("title")
                _cwd.value = ""
                _trashMode.value = false
                _screen.value = Screen.Browser
            }
            "Namespace" -> {
                val snap = ev.getJSONObject("args")
                _entries.value = parseNamespace(snap)
            }
            "Transfer" -> {
                val t = ev.getJSONObject("args")
                val tr = Transfer(t.getString("id"), t.getString("label"), t.getLong("sent"), t.getLong("total"), t.getString("state"))
                _transfers.value = _transfers.value.toMutableMap().apply { put(tr.id, tr) }
            }
            "VaultState" -> {
                val v = ev.getJSONObject("args")
                _vault.value = Vault(v.getBoolean("configured"), v.getBoolean("unlocked"), v.optString("hint"))
            }
            "Status" -> _status.value = ev.getJSONObject("args").getString("message")
            "Error" -> _status.value = ev.getJSONObject("args").optString("message")
            "SignedOut" -> {
                _screen.value = Screen.Phone
                _channels.value = emptyList()
                _entries.value = emptyList()
            }
        }
    }

    private fun parseNamespace(snap: JSONObject): List<NsEntry> {
        val out = mutableListOf<NsEntry>()
        val trash = snap.optJSONObject("trash")
        val folders = snap.optJSONObject("folders")
        folders?.keys()?.forEach { k ->
            val o = folders.getJSONObject(k)
            if (o.optBoolean("tombstoned")) return@forEach
            val id = o.getString("id")
            out.add(NsEntry(
                id = id,
                name = o.getString("name"),
                kind = "dir",
                size = 0,
                parent = o.optString("parent").ifEmpty { null },
                trashed = trash?.has(id) == true,
            ))
        }
        val files = snap.optJSONObject("files")
        files?.keys()?.forEach { k ->
            val o = files.getJSONObject(k)
            if (o.optBoolean("tombstoned")) return@forEach
            val id = o.getString("id")
            out.add(NsEntry(
                id = id,
                name = o.getString("name"),
                kind = "file",
                size = o.optLong("size"),
                parent = o.optString("parent").ifEmpty { null },
                trashed = trash?.has(id) == true,
            ))
        }
        return out
    }
}
