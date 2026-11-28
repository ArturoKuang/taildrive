package com.taildrive.android

import android.content.ActivityNotFoundException
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.WindowInsetsSides
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.only
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.filled.Search
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.FloatingActionButton
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.LifecycleResumeEffect
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContent { TailDriveTheme { TailDriveApp() } }
    }
}

@Composable
private fun TailDriveApp(model: DriveViewModel = viewModel()) {
    val state by model.state.collectAsStateWithLifecycle()
    val network by model.network.collectAsStateWithLifecycle()
    val transfers by model.transfers.collectAsStateWithLifecycle()
    val backups by model.backups.collectAsStateWithLifecycle()
    val context = LocalContext.current
    val snackbar = remember { SnackbarHostState() }
    val account = state.connection?.accountKey
    val accountTransfers = transfers.filter { it.account == account }
    val backup = backups[account] ?: BackupStatus()
    val online = network == NetworkState.Connected && state.folderError == null
    var search by rememberSaveable { mutableStateOf(false) }
    var fabOpen by remember { mutableStateOf(false) }
    var folderDialog by rememberSaveable { mutableStateOf(false) }
    var folderName by rememberSaveable { mutableStateOf("") }
    var uploadParent by rememberSaveable { mutableStateOf("") }
    var downloadId by rememberSaveable { mutableStateOf<String?>(null) }
    var deleting by remember { mutableStateOf<DriveItem?>(null) }
    var pendingPermission by rememberSaveable { mutableStateOf(false) }
    val openFile = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocument()) { uri ->
        if (uri != null) model.queueUpload(uri, uploadParent)
    }
    val saveFile = rememberLauncherForActivityResult(ActivityResultContracts.CreateDocument("application/octet-stream")) { uri ->
        val selected = state.preview?.item?.takeIf { it.id == downloadId }
            ?: state.files.firstOrNull { it.id == downloadId }
        if (uri != null && selected != null) model.queueDownload(uri, selected)
        downloadId = null
    }
    val permissions = rememberLauncherForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) {
        if (pendingPermission && context.hasCameraPermissions()) model.backup(true)
        else if (pendingPermission) model.notify("Allow access to all photos and videos to turn on camera backup.")
        pendingPermission = false
    }

    fun openTailscale() {
        val intent = context.packageManager.getLaunchIntentForPackage("com.tailscale.ipn")
            ?: Intent(Intent.ACTION_VIEW, Uri.parse("tailscale://"))
        try {
            context.startActivity(intent)
        } catch (_: ActivityNotFoundException) {
            model.notify("Install Tailscale to connect to the drive.")
        }
    }

    fun openUrl(url: String) {
        try {
            context.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(url)))
        } catch (_: ActivityNotFoundException) {
            model.notify("No browser found to open $url")
        }
    }

    fun upload() {
        uploadParent = state.path
        openFile.launch(arrayOf("*/*"))
    }

    fun download(item: DriveItem) {
        downloadId = item.id
        saveFile.launch(item.name)
    }

    LaunchedEffect(model) {
        model.messages.collect { snackbar.showSnackbar(it) }
    }
    LifecycleResumeEffect(model) {
        model.onResume()
        onPauseOrDispose {}
    }
    BackHandler(state.showSetup && state.connection != null) { model.cancelSetup() }
    BackHandler(!state.showSetup && state.tab == DriveTab.Files && (search || state.path.isNotEmpty())) {
        if (search) {
            model.query("")
            search = false
        } else model.folder(state.path.substringBeforeLast('/', ""))
    }
    BackHandler(!state.showSetup && state.tab != DriveTab.Files) { model.tab(DriveTab.Files) }

    Scaffold(
        contentWindowInsets = WindowInsets.safeDrawing.only(WindowInsetsSides.Horizontal),
        snackbarHost = { SnackbarHost(snackbar) },
        topBar = {
            val title = if (state.showSetup || state.initializing) "TailDrive" else state.tab.title
            Surface(color = MaterialTheme.colorScheme.surface) {
                Column(Modifier.statusBarsPadding()) {
                    Row(
                        Modifier.fillMaxWidth().height(56.dp).padding(start = 16.dp, end = 8.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        if (state.showSetup && state.connection != null) {
                            IconButton(onClick = model::cancelSetup) {
                                Icon(Icons.AutoMirrored.Filled.ArrowBack, "Back")
                            }
                        }
                        Text(title, Modifier.weight(1f), style = MaterialTheme.typography.titleLarge)
                        StatusDot(network)
                        if (!state.showSetup && !state.initializing) {
                            if (state.tab == DriveTab.Files) {
                                IconButton(onClick = {
                                    search = !search
                                    if (!search) model.query("")
                                }) {
                                    Icon(if (search) Icons.Default.Close else Icons.Default.Search,
                                        if (search) "Close search" else "Search")
                                }
                            }
                            IconButton(onClick = { model.editConnection() }) {
                                Icon(Icons.Default.Settings, "Connection settings")
                            }
                        } else Spacer(Modifier.width(8.dp))
                    }
                    HorizontalDivider(thickness = Dp.Hairline, color = MaterialTheme.colorScheme.outlineVariant)
                }
            }
        },
        bottomBar = {
            if (!state.showSetup && !state.initializing) {
                Surface(color = MaterialTheme.colorScheme.surface) {
                    Column(Modifier.navigationBarsPadding()) {
                        HorizontalDivider(thickness = Dp.Hairline, color = MaterialTheme.colorScheme.outlineVariant)
                        NavigationBar(
                            modifier = Modifier.height(64.dp),
                            containerColor = MaterialTheme.colorScheme.surface,
                            tonalElevation = 0.dp,
                            windowInsets = WindowInsets(0, 0, 0, 0),
                        ) {
                            DriveTab.entries.forEach { tab ->
                                NavigationBarItem(
                                    selected = state.tab == tab,
                                    onClick = { model.tab(tab) },
                                    icon = {
                                        val icon = when (tab) {
                                            DriveTab.Files -> R.drawable.ic_folder
                                            DriveTab.Backup -> R.drawable.ic_backup
                                            DriveTab.Transfers -> R.drawable.ic_transfers
                                        }
                                        Icon(painterResource(icon), null, Modifier.size(22.dp))
                                    },
                                    label = { Text(tab.title, style = MaterialTheme.typography.labelMedium) },
                                )
                            }
                        }
                    }
                }
            }
        },
        floatingActionButton = {
            if (online && !state.showSetup && !state.initializing && state.tab == DriveTab.Files) {
                Box {
                    FloatingActionButton(
                        onClick = { fabOpen = true },
                        modifier = Modifier.size(56.dp),
                        containerColor = MaterialTheme.colorScheme.primary,
                        contentColor = MaterialTheme.colorScheme.onPrimary,
                    ) { Icon(Icons.Default.Add, "Add to drive") }
                    DropdownMenu(expanded = fabOpen, onDismissRequest = { fabOpen = false }) {
                        DropdownMenuItem(text = { Text("Upload file") }, onClick = {
                            fabOpen = false
                            upload()
                        })
                        DropdownMenuItem(text = { Text("New folder") }, onClick = {
                            fabOpen = false
                            folderName = ""
                            folderDialog = true
                        })
                    }
                }
            }
        },
    ) { padding ->
        Box(Modifier.fillMaxSize().padding(padding)) {
            when {
                state.initializing -> CircularProgressIndicator(Modifier.size(28.dp).align(Alignment.Center))
                state.showSetup -> SetupPane(state, network, model::connect, ::openTailscale, model::checkForUpdates, ::openUrl)
                else -> Column(Modifier.fillMaxSize()) {
                    if (network != NetworkState.Connected) {
                        NetworkBanner(network, model::refresh, ::openTailscale)
                    }
                    when (state.tab) {
                        DriveTab.Files -> FilesPane(
                            state, online, search, model::query, model::sort, model::folder, model::preview,
                            model::refresh, ::upload,
                        )
                        DriveTab.Backup -> BackupPane(backup, network, context.hasCameraPermissions(),
                            toggle = { enabled ->
                                if (enabled && !context.hasCameraPermissions()) {
                                    pendingPermission = true
                                    permissions.launch(cameraPermissionRequest())
                                } else model.backup(enabled)
                            },
                            run = {
                                if (context.hasCameraPermissions()) model.backup(true)
                                else {
                                    pendingPermission = true
                                    permissions.launch(cameraPermissionRequest())
                                }
                            },
                        )
                        DriveTab.Transfers -> TransfersPane(accountTransfers, model::retry, model::dismiss)
                    }
                }
            }
        }
    }

    if (!state.showSetup) {
        state.preview?.let {
            FileInspector(it, online, model::dismissPreview, model::folder, ::download, model::share) { item ->
                deleting = item
            }
        }
        if (folderDialog) {
            AlertDialog(
                onDismissRequest = { folderDialog = false },
                title = { Text("New folder") },
                text = {
                    OutlinedTextField(
                        value = folderName, onValueChange = { folderName = it },
                        label = { Text("Folder name") }, singleLine = true,
                    )
                },
                confirmButton = {
                    TextButton(enabled = online && folderName.isNotBlank(), onClick = {
                        model.addFolder(state.path, folderName.trim()) { folderDialog = false }
                    }) { Text("Create") }
                },
                dismissButton = { TextButton(onClick = { folderDialog = false }) { Text("Cancel") } },
            )
        }
        deleting?.let { item ->
            AlertDialog(
                onDismissRequest = { deleting = null },
                title = { Text("Delete ${item.name}?") },
                text = { Text(if (item.isFolder) "This permanently deletes the folder and its contents."
                    else "This permanently deletes the file.") },
                confirmButton = {
                    TextButton(enabled = online, onClick = { deleting = null; model.delete(item) }) {
                        Text("Delete", color = MaterialTheme.colorScheme.error)
                    }
                },
                dismissButton = { TextButton(onClick = { deleting = null }) { Text("Cancel") } },
            )
        }
        state.share?.let { link ->
            AlertDialog(
                onDismissRequest = model::dismissShare,
                title = { Text("Share link") },
                text = {
                    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        Text(link.url, style = monospace)
                        Text("Expires ${formatDate(link.expiresAt)}")
                    }
                },
                confirmButton = {
                    TextButton(onClick = {
                        context.getSystemService(ClipboardManager::class.java)
                            .setPrimaryClip(ClipData.newPlainText("TailDrive share link", link.url))
                        model.dismissShare()
                        model.notify("Link copied")
                    }) { Text("Copy link") }
                },
                dismissButton = { TextButton(onClick = model::dismissShare) { Text("Close") } },
            )
        }
        state.quotaAlert?.let { message ->
            AlertDialog(
                onDismissRequest = model::dismissQuota,
                title = { Text("Storage limit reached") },
                text = {
                    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        state.health?.quota?.let { Text("${formatSize(it.usedBytes)} / ${formatSize(it.limitBytes)}") }
                        Text(message)
                    }
                },
                confirmButton = {
                    TextButton(onClick = {
                        model.dismissQuota()
                        model.tab(DriveTab.Files)
                        model.sort(FileSort.Largest)
                    }) { Text("Sort by largest") }
                },
                dismissButton = { TextButton(onClick = model::dismissQuota) { Text("Close") } },
            )
        }
    }
}

@Composable
internal fun StatusDot(network: NetworkState) {
    val (color, label) = when (network) {
        NetworkState.Connected -> LocalDriveColors.current.online to "Tailscale connected"
        NetworkState.Disconnected -> MaterialTheme.colorScheme.outline to "Tailscale disconnected"
        NetworkState.Offline -> LocalDriveColors.current.warning to "Offline"
    }
    Box(Modifier.size(8.dp).background(color, CircleShape).semantics { contentDescription = label })
}

@Composable
private fun NetworkBanner(network: NetworkState, retry: () -> Unit, tailscale: () -> Unit) {
    Surface(color = MaterialTheme.colorScheme.surfaceVariant) {
        Column(Modifier.fillMaxWidth()) {
            Row(Modifier.fillMaxWidth().padding(start = 16.dp, end = 8.dp).heightIn(min = 48.dp),
                verticalAlignment = Alignment.CenterVertically) {
                Text(
                    if (network == NetworkState.Offline) "Offline · read-only cache" else "Tailscale disconnected",
                    Modifier.weight(1f),
                    color = LocalDriveColors.current.warning,
                    style = MaterialTheme.typography.labelMedium,
                )
                if (network == NetworkState.Disconnected) TextButton(onClick = tailscale) { Text("Open Tailscale") }
                IconButton(onClick = retry) { Icon(Icons.Default.Refresh, "Retry connection") }
            }
            HorizontalDivider(thickness = Dp.Hairline, color = MaterialTheme.colorScheme.outlineVariant)
        }
    }
}
