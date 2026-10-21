package tdrive.app

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.CloudUpload
import androidx.compose.material.icons.filled.Lock
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExtendedFloatingActionButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import org.json.JSONObject

/** The application. */
class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContent {
            val model: TDriveViewModel = viewModel()
            val state by model.state.collectAsStateWithLifecycle()
            TDriveTheme(theme = state.theme) {
                TDriveApp(state, model)
            }
        }
    }
}

/** The application, as the screen draws it. */
@Composable
fun TDriveApp(state: TDriveState, model: TDriveViewModel) {
    when (state.screen) {
        Screen.SignIn -> SignInScreen(
            state = state,
            onCredentials = model::setCredentials,
            onPhone = model::setPhone,
            onCode = model::setCode,
            onPassword = model::setPassword,
            onSignIn = model::signIn,
            onVerify = { if (state.needsPassword) model.verifyPassword() else model.verifyCode() },
        )

        Screen.DrivePicker -> DrivePickerScreen(state = state, onOpen = model::openDrive)

        Screen.Drive -> DriveScreen(state = state, model = model)
    }
}

/** The drive, with the tabs along the bottom. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun DriveScreen(state: TDriveState, model: TDriveViewModel) {
    var tab by remember { mutableStateOf(Tab.Files) }
    var sheet by remember { mutableStateOf<Pair<String, DriveFile>?>(null) }
    var unlock by remember { mutableStateOf(false) }
    var newFolder by remember { mutableStateOf(false) }
    val snackbar = remember { SnackbarHostState() }

    LaunchedEffect(state.message) {
        state.message?.let { snackbar.showSnackbar(it) }
    }

    // The file the phone hands over when the user picks something to upload.
    val context = LocalContext.current
    val picker = rememberLauncherForActivityResult(
        ActivityResultContracts.OpenDocument(),
    ) { uri ->
        if (uri != null) {
            val path = copyToCache(context, uri)
            if (path != null) {
                model.upload(path, encrypt = state.drive?.encrypted == true)
            }
        }
    }

    Scaffold(
        snackbarHost = { SnackbarHost(snackbar) },
        topBar = {
            TopAppBar(
                title = {
                    Column {
                        Text(
                            text = when (tab) {
                                Tab.Files -> state.drive?.title ?: "TDrive"
                                Tab.Gallery -> "Gallery"
                                Tab.Trash -> "Trash"
                                Tab.Account -> "Account"
                            },
                            style = MaterialTheme.typography.titleMedium,
                        )
                        if (tab == Tab.Files && state.path.isNotEmpty()) {
                            Text(
                                text = state.path.joinToString(" / ") { it.second },
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                        } else if (tab == Tab.Files && state.drive != null) {
                            Text(
                                text = "${state.drive.items} items",
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                        }
                    }
                },
                navigationIcon = {
                    if (tab == Tab.Files && state.path.isNotEmpty()) {
                        IconButton(onClick = model::leaveFolder) {
                            Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
                        }
                    }
                },
                actions = {
                    when (tab) {
                        Tab.Files -> {
                            IconButton(onClick = model::refresh) {
                                Icon(Icons.Filled.Refresh, contentDescription = "Reload")
                            }
                            if (state.drive?.encrypted == true && state.drive?.unlocked != true) {
                                IconButton(onClick = { unlock = true }) {
                                    Icon(Icons.Filled.Lock, contentDescription = "Unlock")
                                }
                            }
                        }
                        else -> {}
                    }
                },
                colors = TopAppBarDefaults.topAppBarColors(
                    containerColor = MaterialTheme.colorScheme.background,
                ),
            )
        },
        bottomBar = {
            NavigationBar {
                Tab.entries.forEach { entry ->
                    NavigationBarItem(
                        selected = tab == entry,
                        onClick = { tab = entry },
                        icon = { Icon(entry.icon, contentDescription = entry.label) },
                        label = { Text(entry.label) },
                    )
                }
            }
        },
        floatingActionButton = {
            if (tab == Tab.Files) {
                ExtendedFloatingActionButton(
                    onClick = { picker.launch(arrayOf("*/*")) },
                    icon = { Icon(Icons.Filled.CloudUpload, contentDescription = null) },
                    text = { Text("Upload") },
                )
            }
        },
    ) { padding ->
        Box(modifier = Modifier.fillMaxSize().padding(padding)) {
            when (tab) {
                Tab.Files -> FilesTab(state, model) { file -> sheet = file.name to file }
                Tab.Gallery -> GalleryTab(state, model)
                Tab.Trash -> TrashTab(state, model)
                Tab.Account -> AccountTab(state, model)
            }
        }
    }

    sheet?.let { (title, file) ->
        ActionSheet(
            title = title,
            actions = listOf(
                "Download" to { model.download(file) },
                "Move to the trash" to { model.trash(file, restore = false) },
            ),
            onDismiss = { sheet = null },
        )
    }

    if (unlock) {
        UnlockDialog(
            hint = state.drive?.hint ?: "",
            onUnlock = { password ->
                model.unlock(password)
                unlock = false
            },
            onDismiss = { unlock = false },
        )
    }

    if (newFolder) {
        NameDialog(
            title = "New folder",
            onName = { name ->
                model.createFolder(name)
                newFolder = false
            },
            onDismiss = { newFolder = false },
        )
    }
}

/** The files, in the folder on screen. */
@Composable
fun FilesTab(
    state: TDriveState,
    model: TDriveViewModel,
    onMore: (DriveFile) -> Unit,
) {
    val folder = state.folder
    val files = state.files.filter { it.parent == folder }
    if (files.isEmpty()) {
        Column(
            modifier = Modifier.fillMaxSize().padding(24.dp),
            verticalArrangement = Arrangement.Center,
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            Text(
                text = if (state.files.isEmpty()) {
                    // The drive is open, and holds nothing the reader can see.
                    "This drive holds ${state.drive?.items ?: 0} items, and none of them are here."
                } else {
                    "This folder is empty."
                },
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            state.error?.let {
                Spacer(Modifier.height(8.dp))
                Text(
                    text = it,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.error,
                )
            }
        }
        return
    }
    LazyColumn(modifier = Modifier.fillMaxSize()) {
        items(files) { file ->
            FileRow(
                file = file,
                onOpen = {
                    if (file.isFolder) {
                        model.enterFolder(file)
                    } else {
                        model.download(file)
                    }
                },
                onMore = { onMore(file) },
            )
        }
    }
}

/** The pictures, in a grid. */
@Composable
fun GalleryTab(state: TDriveState, model: TDriveViewModel) {
    val pictures = state.files.filter { it.isPicture }
    if (pictures.isEmpty()) {
        Column(
            modifier = Modifier.fillMaxSize().padding(24.dp),
            verticalArrangement = Arrangement.Center,
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            Text(
                text = "No pictures in this drive yet.",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        return
    }
    LazyVerticalGrid(
        columns = GridCells.Adaptive(minSize = 108.dp),
        modifier = Modifier.fillMaxSize(),
    ) {
        items(pictures) { file ->
            PictureTile(
                file = file,
                // The small picture published beside the file, when it has one.
                picture = model.picture(file),
                onOpen = { model.download(file) },
            )
        }
    }
}

/** The drive's trash. */
@Composable
fun TrashTab(state: TDriveState, model: TDriveViewModel) {
    if (state.trash.isEmpty()) {
        Column(
            modifier = Modifier.fillMaxSize().padding(24.dp),
            verticalArrangement = Arrangement.Center,
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            Text(
                text = "The trash is empty.",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        return
    }
    LazyColumn(modifier = Modifier.fillMaxSize()) {
        items(state.trash) { entry ->
            Row(
                modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 10.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Column(modifier = Modifier.weight(1f)) {
                    Text(
                        text = entry.name,
                        style = MaterialTheme.typography.bodyMedium,
                        fontWeight = FontWeight.Medium,
                        color = MaterialTheme.colorScheme.onBackground,
                    )
                    Text(
                        text = formatSize(entry.size) + " · " + formatDate(entry.deletedAt),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                TextButton(onClick = {
                    val file = state.files.firstOrNull { it.objectId == entry.objectId }
                    if (file != null) model.trash(file, restore = true)
                }) {
                    Text("Restore")
                }
            }
        }
    }
}

/** The account: the drive, the vault, the theme, and the way out. */
@Composable
fun AccountTab(state: TDriveState, model: TDriveViewModel) {
    LazyColumn(
        modifier = Modifier.fillMaxSize(),
        contentPadding = androidx.compose.foundation.layout.PaddingValues(16.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        item {
            Text(
                text = state.drive?.title ?: "TDrive",
                style = MaterialTheme.typography.titleMedium,
                color = MaterialTheme.colorScheme.onBackground,
            )
            Text(
                text = "${state.drive?.items ?: 0} items · ${formatSize(state.drive?.size ?: 0)}",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        if (state.drive?.encrypted == true) {
            item {
                Text(
                    text = if (state.drive.unlocked) {
                        "This drive is encrypted, and unlocked."
                    } else {
                        "This drive is encrypted. Its files need the encryption password."
                    },
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
        item {
            Text(
                text = "Appearance",
                style = MaterialTheme.typography.titleMedium,
                color = MaterialTheme.colorScheme.onBackground,
                modifier = Modifier.padding(top = 12.dp),
            )
        }
        items(state.themes) { theme ->
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(vertical = 4.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                androidx.compose.foundation.Canvas(
                    modifier = Modifier.width(56.dp).height(32.dp),
                ) {
                    val colors = theme.preview.map {
                        androidx.compose.ui.graphics.Color(
                            (it or 0xFF000000L).toInt()
                        )
                    }
                    if (colors.size >= 4) {
                        drawRect(colors[0], size = androidx.compose.ui.geometry.Size(size.width, size.height))
                        drawRect(
                            colors[1],
                            topLeft = androidx.compose.ui.geometry.Offset(0f, size.height / 3f),
                            size = androidx.compose.ui.geometry.Size(size.width / 3f, size.height / 3f),
                        )
                    }
                }
                Spacer(Modifier.width(12.dp))
                Text(
                    text = theme.name,
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onBackground,
                    modifier = Modifier.weight(1f),
                )
                if (theme.id == state.themeId) {
                    Text(
                        text = "In use",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.primary,
                    )
                } else {
                    TextButton(onClick = { model.chooseTheme(theme.id) }) {
                        Text("Use")
                    }
                }
            }
        }
        item {
            TextButton(onClick = model::signOut) {
                Text("Sign out", color = MaterialTheme.colorScheme.error)
            }
        }
    }
}

/** The dialog that asks for the encryption password. */
@Composable
fun UnlockDialog(hint: String, onUnlock: (String) -> Unit, onDismiss: () -> Unit) {
    var password by remember { mutableStateOf("") }
    androidx.compose.material3.AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Unlock encrypted files") },
        text = {
            Column {
                Text(
                    text = "This drive stores encrypted files. Enter your encryption password to open them.",
                    style = MaterialTheme.typography.bodySmall,
                )
                if (hint.isNotEmpty()) {
                    Text(
                        text = "Hint: $hint",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                OutlinedTextField(
                    value = password,
                    onValueChange = { password = it },
                    label = { Text("Encryption password") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
            }
        },
        confirmButton = {
            TextButton(onClick = { onUnlock(password) }) { Text("Unlock") }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) { Text("Cancel") }
        },
    )
}

/** The dialog that asks for a name. */
@Composable
fun NameDialog(title: String, onName: (String) -> Unit, onDismiss: () -> Unit) {
    var name by remember { mutableStateOf("") }
    androidx.compose.material3.AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(title) },
        text = {
            OutlinedTextField(
                value = name,
                onValueChange = { name = it },
                label = { Text("Name") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
        },
        confirmButton = {
            TextButton(onClick = { onName(name) }, enabled = name.isNotBlank()) { Text("Create") }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) { Text("Cancel") }
        },
    )
}
