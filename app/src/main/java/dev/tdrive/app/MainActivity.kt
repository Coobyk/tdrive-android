package dev.tdrive.app

import android.content.Context
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import androidx.compose.ui.window.Dialog
import androidx.lifecycle.lifecycleScope
import java.io.File
import java.io.FileOutputStream

class MainActivity : ComponentActivity() {

    private val core by lazy { Core(lifecycleScope) }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        core.start(filesDir.absolutePath)
        setContent {
            MaterialTheme(colorScheme = darkColorScheme()) {
                Surface(Modifier.fillMaxSize()) { App(core, applicationContext) }
            }
        }
    }
}

@Composable
fun App(core: Core, ctx: Context) {
    val screen by core.screen.collectAsState()
    val status by core.status.collectAsState()

    Column(Modifier.fillMaxSize().statusBarsPadding().navigationBarsPadding()) {
        Box(Modifier.weight(1f)) {
            when (val s = screen) {
                Screen.Setup -> SetupScreen(core, ctx)
                Screen.Phone -> PhoneScreen(core)
                Screen.Code -> CodeScreen(core)
                is Screen.Password -> PasswordScreen(core, s.hint)
                Screen.Channels -> ChannelsScreen(core)
                Screen.Browser -> BrowserScreen(core, ctx)
            }
        }
        if (status.isNotEmpty()) {
            Text(
                status,
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 16.dp, vertical = 6.dp),
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                maxLines = 1,
            )
        }
    }
}

@Composable
private fun Field(value: String, onChange: (String) -> Unit, label: String, password: Boolean = false, keyboard: KeyboardType = KeyboardType.Text) {
    OutlinedTextField(
        value = value,
        onValueChange = onChange,
        label = { Text(label) },
        singleLine = true,
        visualTransformation = if (password) PasswordVisualTransformation() else androidx.compose.ui.text.input.VisualTransformation.None,
        keyboardOptions = KeyboardOptions(keyboardType = keyboard),
        modifier = Modifier.fillMaxWidth(),
    )
}

@Composable
fun SetupScreen(core: Core, ctx: Context) {
    val prefs = remember { ctx.getSharedPreferences("tdrive", Context.MODE_PRIVATE) }
    var apiId by remember { mutableStateOf(prefs.getString("api_id", "") ?: "") }
    var apiHash by remember { mutableStateOf(prefs.getString("api_hash", "") ?: "") }

    Column(
        Modifier.fillMaxSize().padding(24.dp).verticalScroll(rememberScrollState()),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Text("TDrive", style = MaterialTheme.typography.headlineMedium)
        Text("Telegram API credentials — get them at my.telegram.org", style = MaterialTheme.typography.bodyMedium)
        Field(apiId, { apiId = it }, "API ID", keyboard = KeyboardType.Number)
        Field(apiHash, { apiHash = it }, "API Hash")
        Button(
            onClick = {
                prefs.edit().putString("api_id", apiId).putString("api_hash", apiHash).apply()
                core.connect(apiId.toIntOrNull() ?: 0, apiHash)
            },
            enabled = apiId.isNotBlank() && apiHash.isNotBlank(),
            modifier = Modifier.fillMaxWidth(),
        ) { Text("Connect") }
    }
}

@Composable
fun PhoneScreen(core: Core) {
    var phone by remember { mutableStateOf("") }
    Column(Modifier.fillMaxSize().padding(24.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
        Text("Sign in", style = MaterialTheme.typography.headlineMedium)
        Field(phone, { phone = it }, "Phone number (+1…)", keyboard = KeyboardType.Phone)
        Button(onClick = { core.requestCode(phone) }, enabled = phone.isNotBlank(), modifier = Modifier.fillMaxWidth()) {
            Text("Send code")
        }
    }
}

@Composable
fun CodeScreen(core: Core) {
    var code by remember { mutableStateOf("") }
    Column(Modifier.fillMaxSize().padding(24.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
        Text("Enter the code", style = MaterialTheme.typography.headlineMedium)
        Field(code, { code = it }, "Login code", keyboard = KeyboardType.Number)
        Button(onClick = { core.submitCode(code) }, enabled = code.isNotBlank(), modifier = Modifier.fillMaxWidth()) {
            Text("Sign in")
        }
    }
}

@Composable
fun PasswordScreen(core: Core, hint: String) {
    var pw by remember { mutableStateOf("") }
    Column(Modifier.fillMaxSize().padding(24.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
        Text("Two-factor password", style = MaterialTheme.typography.headlineMedium)
        if (hint.isNotEmpty()) Text("Hint: $hint", style = MaterialTheme.typography.bodySmall)
        Field(pw, { pw = it }, "Password", password = true)
        Button(onClick = { core.submitPassword(pw) }, enabled = pw.isNotBlank(), modifier = Modifier.fillMaxWidth()) {
            Text("Unlock")
        }
    }
}

@Composable
fun ChannelsScreen(core: Core) {
    val channels by core.channels.collectAsState()
    var newTitle by remember { mutableStateOf("") }
    var newSharedTitle by remember { mutableStateOf("") }

    Column(Modifier.fillMaxSize().padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween, verticalAlignment = Alignment.CenterVertically) {
            Text("Your drives", style = MaterialTheme.typography.headlineSmall)
            TextButton(onClick = { core.signOut() }) { Text("Sign out") }
        }

        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            OutlinedTextField(newTitle, { newTitle = it }, label = { Text("New personal drive") }, singleLine = true, modifier = Modifier.weight(1f))
            Button(onClick = { core.createChannel(newTitle); newTitle = "" }, enabled = newTitle.isNotBlank()) { Text("Create") }
        }
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            OutlinedTextField(newSharedTitle, { newSharedTitle = it }, label = { Text("New shared drive") }, singleLine = true, modifier = Modifier.weight(1f))
            Button(onClick = { core.createSharedChannel(newSharedTitle); newSharedTitle = "" }, enabled = newSharedTitle.isNotBlank()) { Text("Create") }
        }

        LazyColumn(Modifier.fillMaxSize(), verticalArrangement = Arrangement.spacedBy(4.dp)) {
            items(channels, key = { it.id }) { c ->
                ListItem(
                    headlineContent = { Text(c.title) },
                    supportingContent = { Text(if (c.kind == "Personal") "personal" else "shared") },
                    leadingContent = { Icon(Icons.Default.Folder, null) },
                    modifier = Modifier.clickable { core.openChannel(c) },
                )
            }
        }
    }
}

@Composable
fun BrowserScreen(core: Core, ctx: Context) {
    val entries by core.entries.collectAsState()
    val cwd by core.cwd.collectAsState()
    val title by core.driveTitle.collectAsState()
    val transfers by core.transfers.collectAsState()
    val trashMode by core.trashMode.collectAsState()
    val vault by core.vault.collectAsState()

    var selected by remember { mutableStateOf<NsEntry?>(null) }
    var showMkdir by remember { mutableStateOf(false) }
    var showRename by remember { mutableStateOf<NsEntry?>(null) }
    var showMove by remember { mutableStateOf<NsEntry?>(null) }
    var showUnlock by remember { mutableStateOf(false) }
    var showEnable by remember { mutableStateOf(false) }
    var showOptimize by remember { mutableStateOf(false) }
    var downloadFor by remember { mutableStateOf<NsEntry?>(null) }

    val crumbs = remember(entries, cwd) {
        val path = mutableListOf<Pair<String, String>>()
        var cur: String? = cwd
        while (!cur.isNullOrEmpty()) {
            val e = entries.find { it.id == cur } ?: break
            path.add(0, e.id to e.name)
            cur = e.parent
        }
        path
    }

    val visible = remember(entries, cwd, trashMode) {
        if (trashMode) entries.filter { it.trashed }
        else entries.filter { it.parent == cwd && !it.trashed }
            .sortedWith(compareBy({ it.kind != "dir" }, { it.name.lowercase() }))
    }

    val activeTransfers = transfers.values.filter { it.state == "Active" || it.state == "Paused" }

    Column(Modifier.fillMaxSize()) {
        // Header
        Column(Modifier.padding(horizontal = 12.dp, vertical = 6.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text("TDrive · $title", style = MaterialTheme.typography.titleMedium, modifier = Modifier.weight(1f), maxLines = 1)
                TextButton(onClick = { core.setTrashMode(!trashMode); selected = null }) {
                    Text(if (trashMode) "Files" else "Trash")
                }
                TextButton(onClick = { core.listChannels() }) { Text("Drives") }
            }
            if (!trashMode) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text("root", style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.primary,
                        modifier = Modifier.clickable { core.setCwd("") })
                    crumbs.forEach { (id, name) ->
                        Text(" / $name", style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.primary,
                            modifier = Modifier.clickable { core.setCwd(id) })
                    }
                }
            }
        }
        HorizontalDivider()

        // Listing
        LazyColumn(Modifier.weight(1f)) {
            if (cwd.isNotEmpty() && !trashMode) {
                item {
                    ListItem(
                        headlineContent = { Text("..") },
                        leadingContent = { Icon(Icons.Default.ArrowBack, null) },
                        modifier = Modifier.clickable {
                            val parent = entries.find { it.id == cwd }?.parent ?: ""
                            core.setCwd(parent)
                        },
                    )
                }
            }
            items(visible, key = { it.id }) { e ->
                ListItem(
                    headlineContent = { Text(e.name, maxLines = 1) },
                    supportingContent = {
                        if (e.kind == "file") Text(humanSize(e.size), style = MaterialTheme.typography.bodySmall)
                    },
                    leadingContent = {
                        Icon(if (e.kind == "dir") Icons.Default.Folder else Icons.Default.InsertDriveFile, null)
                    },
                    modifier = Modifier
                        .clickable(enabled = e.kind == "dir" && !trashMode) { core.setCwd(e.id) }
                        .clickable { selected = e },
                )
            }
        }

        // Selection actions
        selected?.let { e ->
            HorizontalDivider()
            Row(
                Modifier.fillMaxWidth().padding(horizontal = 8.dp),
                horizontalArrangement = Arrangement.spacedBy(4.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(e.name, Modifier.weight(1f), maxLines = 1, style = MaterialTheme.typography.bodySmall)
                if (trashMode) {
                    TextButton(onClick = { core.restore(e.id); selected = null }) { Text("Restore") }
                    TextButton(onClick = { core.purge(e.id); selected = null }) { Text("Purge") }
                } else {
                    if (e.kind == "file") TextButton(onClick = { downloadFor = e }) { Text("Get") }
                    TextButton(onClick = { showRename = e }) { Text("Rename") }
                    TextButton(onClick = { showMove = e }) { Text("Move") }
                    TextButton(onClick = { core.delete(e.id); selected = null }) { Text("Delete") }
                }
            }
        }

        // Transfers
        if (activeTransfers.isNotEmpty()) {
            HorizontalDivider()
            Column(Modifier.padding(horizontal = 12.dp, vertical = 4.dp)) {
                activeTransfers.take(3).forEach { t ->
                    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        Text(t.label, Modifier.weight(1f), maxLines = 1, style = MaterialTheme.typography.bodySmall)
                        LinearProgressIndicator(
                            progress = { if (t.total > 0) (t.sent.toFloat() / t.total).coerceIn(0f, 1f) else 0f },
                            modifier = Modifier.width(100.dp),
                        )
                        IconButton(onClick = { core.cancelTransfer(t.id) }) {
                            Icon(Icons.Default.Close, null, modifier = Modifier.size(16.dp))
                        }
                    }
                }
            }
        }

        // Action bar
        HorizontalDivider()
        Row(
            Modifier.fillMaxWidth().padding(horizontal = 8.dp),
            horizontalArrangement = Arrangement.spacedBy(4.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            var menu by remember { mutableStateOf(false) }
            if (!trashMode) {
                UploadButton(core, ctx)
                TextButton(onClick = { showMkdir = true }) { Text("New folder") }
                TextButton(onClick = { core.sync() }) { Text("Sync") }
            } else {
                TextButton(onClick = { core.emptyTrash() }) { Text("Empty trash") }
            }
            Spacer(Modifier.weight(1f))
            IconButton(onClick = { menu = true }) { Icon(Icons.Default.MoreVert, null) }
            DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                if (vault.configured) {
                    DropdownMenuItem(text = { Text(if (vault.unlocked) "Lock vault" else "Unlock vault") },
                        onClick = { menu = false; if (vault.unlocked) core.lock() else showUnlock = true })
                } else {
                    DropdownMenuItem(text = { Text("Enable encryption") }, onClick = { menu = false; showEnable = true })
                }
                DropdownMenuItem(text = { Text("Optimize drive") }, onClick = { menu = false; showOptimize = true })
                DropdownMenuItem(text = { Text("Resume uploads") }, onClick = { menu = false; core.resumeUploads() })
            }
        }
    }

    // Dialogs
    if (showMkdir) InputDialog("New folder", "Folder name") { name ->
        showMkdir = false
        if (name.isNotBlank()) core.mkdir(name)
    }
    showRename?.let { e ->
        InputDialog("Rename", "New name", e.name) { name ->
            showRename = null
            if (name.isNotBlank()) core.rename(e.id, name)
        }
    }
    showMove?.let { e ->
        val folders = entries.filter { it.kind == "dir" && !it.trashed && it.id != e.id }
        Dialog(onDismissRequest = { showMove = null }) {
            Surface(shape = MaterialTheme.shapes.medium) {
                Column(Modifier.padding(16.dp)) {
                    Text("Move “${e.name}” to", style = MaterialTheme.typography.titleMedium)
                    LazyColumn(Modifier.heightIn(max = 300.dp)) {
                        item { ListItem(headlineContent = { Text("root") }, modifier = Modifier.clickable { core.move(e.id, ""); showMove = null }) }
                        items(folders, key = { it.id }) { f ->
                            ListItem(headlineContent = { Text(f.name) }, modifier = Modifier.clickable { core.move(e.id, f.id); showMove = null })
                        }
                    }
                }
            }
        }
    }
    downloadFor?.let { e ->
        AlertDialog(
            onDismissRequest = { downloadFor = null },
            title = { Text("Download ${e.name}") },
            text = { Text("Save to the app files dir (${humanSize(e.size)}).") },
            confirmButton = {
                TextButton(onClick = {
                    core.download(e.id, e.name)
                    downloadFor = null
                }) { Text("Download") }
            },
            dismissButton = { TextButton(onClick = { downloadFor = null }) { Text("Cancel") } },
        )
    }
    if (showUnlock) InputDialog("Unlock vault", "Password", password = true) { pw ->
        showUnlock = false
        core.unlock(pw)
    }
    if (showEnable) {
        var pw by remember { mutableStateOf("") }
        var hint by remember { mutableStateOf("") }
        AlertDialog(
            onDismissRequest = { showEnable = false },
            title = { Text("Enable encryption") },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    Field(pw, { pw = it }, "Password", password = true)
                    Field(hint, { hint = it }, "Hint (optional)")
                }
            },
            confirmButton = {
                TextButton(onClick = { showEnable = false; core.enableEncryption(pw, hint) }, enabled = pw.isNotBlank()) { Text("Enable") }
            },
            dismissButton = { TextButton(onClick = { showEnable = false }) { Text("Cancel") } },
        )
    }
    if (showOptimize) {
        AlertDialog(
            onDismissRequest = { showOptimize = false },
            title = { Text("Optimize drive") },
            text = { Text("Scan the channel and delete messages belonging to objects that were created and later removed.") },
            confirmButton = {
                TextButton(onClick = { showOptimize = false; core.optimize() }) { Text("Scan") }
            },
            dismissButton = { TextButton(onClick = { showOptimize = false }) { Text("Cancel") } },
        )
    }
}

@Composable
fun UploadButton(core: Core, ctx: Context) {
    val launcher = rememberLauncherForActivityResult(ActivityResultContracts.GetContent()) { uri ->
        uri ?: return@rememberLauncherForActivityResult
        val name = ctx.contentResolver.query(uri, null, null, null, null)?.use { c ->
            val i = c.getColumnIndex(android.provider.OpenableColumns.DISPLAY_NAME)
            if (c.moveToFirst() && i >= 0) c.getString(i) else null
        } ?: uri.lastPathSegment?.substringAfterLast('/') ?: "upload"
        val dst = File(ctx.cacheDir, "inbox/${System.currentTimeMillis()}-$name")
        dst.parentFile?.mkdirs()
        try {
            ctx.contentResolver.openInputStream(uri)?.use { inp ->
                FileOutputStream(dst).use { out -> inp.copyTo(out) }
            }
            core.upload(dst.absolutePath)
        } catch (_: Exception) {
        }
    }
    TextButton(onClick = { launcher.launch("*/*") }) { Text("Upload") }
}

@Composable
fun InputDialog(title: String, label: String, initial: String = "", password: Boolean = false, onDone: (String) -> Unit) {
    var text by remember { mutableStateOf(initial) }
    AlertDialog(
        onDismissRequest = { onDone("") },
        title = { Text(title) },
        text = { Field(text, { text = it }, label, password = password) },
        confirmButton = { TextButton(onClick = { onDone(text) }) { Text("OK") } },
        dismissButton = { TextButton(onClick = { onDone("") }) { Text("Cancel") } },
    )
}

fun humanSize(n: Long): String {
    if (n < 1024) return "$n B"
    val units = arrayOf("KiB", "MiB", "GiB", "TiB")
    var v = n.toDouble()
    var i = -1
    do { v /= 1024; i++ } while (v >= 1024 && i < units.lastIndex)
    return "%.1f %s".format(v, units[i])
}
