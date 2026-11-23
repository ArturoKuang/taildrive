package com.taildrive.android

import androidx.compose.animation.core.animateFloat
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Info
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.pulltorefresh.PullToRefreshBox
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter

@Composable
internal fun SetupPane(
    state: BrowserState,
    network: NetworkState,
    connect: (String, String) -> Unit,
    openTailscale: () -> Unit,
) {
    var host by rememberSaveable(state.connection?.url) {
        mutableStateOf(state.connection?.url?.trimEnd('/') ?: DEFAULT_SERVER)
    }
    var token by remember { mutableStateOf("") }
    var tokenVisible by remember { mutableStateOf(false) }
    val keyboard = LocalSoftwareKeyboardController.current
    Box(Modifier.fillMaxSize().navigationBarsPadding().imePadding(), contentAlignment = Alignment.TopCenter) {
        Column(
            Modifier.widthIn(max = 480.dp).verticalScroll(rememberScrollState()).padding(24.dp),
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            Text("Connect to your drive", style = MaterialTheme.typography.titleLarge)
            Surface(color = MaterialTheme.colorScheme.surfaceVariant, shape = MaterialTheme.shapes.small) {
                Row(Modifier.fillMaxWidth().padding(start = 12.dp, end = 4.dp).heightIn(min = 48.dp),
                    horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically) {
                    StatusDot(network)
                    Text(when (network) {
                        NetworkState.Connected -> "Tailscale VPN active"
                        NetworkState.Disconnected -> "Tailscale disconnected"
                        NetworkState.Offline -> "No network connection"
                    }, Modifier.weight(1f), style = MaterialTheme.typography.labelMedium)
                    if (network != NetworkState.Connected) {
                        TextButton(onClick = openTailscale) { Text("Open Tailscale") }
                    }
                }
            }
            OutlinedTextField(
                value = host, onValueChange = { host = it }, label = { Text("MagicDNS hostname or Tailnet IP") },
                singleLine = true, modifier = Modifier.fillMaxWidth(),
                textStyle = monospace, keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri, imeAction = ImeAction.Next),
                supportingText = { Text("Example  arturos-mac-mini.<tailnet>.ts.net") },
                enabled = !state.testing,
            )
            OutlinedTextField(
                value = token, onValueChange = { token = it }, label = { Text("Scoped Bearer token") },
                singleLine = true, modifier = Modifier.fillMaxWidth(), enabled = !state.testing,
                visualTransformation = if (tokenVisible) VisualTransformation.None else PasswordVisualTransformation(),
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password, imeAction = ImeAction.Done),
                keyboardActions = KeyboardActions(onDone = {
                    if (network == NetworkState.Connected && host.isNotBlank() && token.isNotBlank()) {
                        keyboard?.hide()
                        connect(host, token)
                    }
                }),
                trailingIcon = {
                    TextButton(onClick = { tokenVisible = !tokenVisible }) {
                        Text(if (tokenVisible) "Hide" else "Show")
                    }
                },
            )
            state.setupError?.let { Text(it, color = MaterialTheme.colorScheme.error) }
            Button(
                enabled = network == NetworkState.Connected && !state.testing && host.isNotBlank() && token.isNotBlank(),
                onClick = { keyboard?.hide(); connect(host, token) },
                shape = MaterialTheme.shapes.small,
                modifier = Modifier.fillMaxWidth().heightIn(min = 48.dp),
            ) {
                if (state.testing) {
                    CircularProgressIndicator(Modifier.size(16.dp), strokeWidth = 2.dp)
                    Spacer(Modifier.width(8.dp))
                }
                Text(if (state.testing) "Testing connection…" else "Test connection")
            }
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun FilesPane(
    state: BrowserState,
    online: Boolean,
    showSearch: Boolean,
    query: (String) -> Unit,
    sort: (FileSort) -> Unit,
    open: (String) -> Unit,
    inspect: (DriveItem) -> Unit,
    refresh: () -> Unit,
    upload: () -> Unit,
) {
    val visible = remember(state.files, state.query, state.sort) {
        val order = when (state.sort) {
            FileSort.Name -> compareBy<DriveItem> { it.name.lowercase() }
            FileSort.Largest -> compareByDescending<DriveItem> { it.sizeBytes }.thenBy { it.name.lowercase() }
            FileSort.Modified -> compareByDescending<DriveItem> { it.updatedAt }.thenBy { it.name.lowercase() }
        }
        state.files.filter { it.name.contains(state.query.trim(), ignoreCase = true) }
            .sortedWith(compareByDescending<DriveItem> { it.isFolder }.then(order))
    }
    Column(Modifier.fillMaxSize()) {
        state.health?.let { QuotaHeader(it) }
        if (showSearch) {
            val focus = remember { FocusRequester() }
            OutlinedTextField(
                value = state.query, onValueChange = query, placeholder = { Text("Search this folder") },
                modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 8.dp).focusRequester(focus),
                singleLine = true, keyboardOptions = KeyboardOptions(imeAction = ImeAction.Search),
                keyboardActions = KeyboardActions(onSearch = { defaultKeyboardAction(ImeAction.Search) }),
            )
            LaunchedEffect(Unit) { focus.requestFocus() }
        }
        Breadcrumbs(state.path, state.sort, open, sort)
        state.folderError?.let { failure ->
            Row(Modifier.fillMaxWidth().padding(start = 16.dp, end = 8.dp), verticalAlignment = Alignment.CenterVertically) {
                Text(failure, Modifier.weight(1f), color = MaterialTheme.colorScheme.error)
                TextButton(onClick = refresh) { Text("Retry") }
            }
        }
        PullToRefreshBox(isRefreshing = state.loading && state.files.isNotEmpty(),
            onRefresh = refresh, modifier = Modifier.weight(1f)) {
            LazyColumn(
                modifier = Modifier.fillMaxSize(),
                contentPadding = PaddingValues(start = 16.dp, end = 16.dp, bottom = 80.dp),
            ) {
                if (state.loading && state.files.isEmpty()) {
                    items(6) { LoadingRow() }
                } else if (visible.isEmpty() && state.folderError == null) {
                    item {
                        EmptyPane(when {
                            !online && !state.hasFolderSnapshot -> "No cached files"
                            state.query.isNotBlank() -> "No matching files"
                            else -> "This folder is empty"
                        }) {
                            if (state.query.isBlank() && online) TextButton(onClick = upload) { Text("Upload first file") }
                        }
                    }
                } else {
                    items(visible, key = { it.id }) { item ->
                        FileRow(item, open, inspect)
                    }
                }
            }
        }
    }
}

@Composable
private fun QuotaHeader(health: Health) {
    val quota = health.quota
    val status = LocalDriveColors.current
    val color = when {
        quota.status == "full" || quota.fraction >= .95f -> status.danger
        quota.status == "warn" || quota.fraction >= .80f -> status.warning
        else -> MaterialTheme.colorScheme.primary
    }
    val closeToLimit = quota.status != "ok" || quota.fraction >= .80f
    val capacity = if (quota.remainingBytes <= 0) "Storage limit reached" else "Storage nearly full"
    Surface(color = MaterialTheme.colorScheme.surface) {
        Column(Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 12.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp)) {
            Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                Text(health.node, Modifier.weight(1f), style = MaterialTheme.typography.labelMedium,
                    maxLines = 1, overflow = TextOverflow.Ellipsis)
                Text("${formatSize(quota.usedBytes)} / ${formatSize(quota.limitBytes)}", style = monospace)
            }
            LinearProgressIndicator(
                progress = { quota.fraction }, color = color,
                trackColor = MaterialTheme.colorScheme.outlineVariant,
                modifier = Modifier.fillMaxWidth().height(4.dp),
            )
            if (closeToLimit) Text("$capacity · ${formatSize(quota.remainingBytes)} remaining",
                style = MaterialTheme.typography.labelSmall, color = color)
        }
    }
    HorizontalDivider(thickness = Dp.Hairline, color = MaterialTheme.colorScheme.outlineVariant)
}

@Composable
private fun Breadcrumbs(path: String, selected: FileSort, open: (String) -> Unit, sort: (FileSort) -> Unit) {
    val segments = if (path.isEmpty()) emptyList() else path.split('/')
    val scroll = rememberScrollState()
    var openSort by remember { mutableStateOf(false) }
    LaunchedEffect(path, scroll.maxValue) { scroll.scrollTo(scroll.maxValue) }
    Row(Modifier.fillMaxWidth().padding(start = 16.dp, end = 8.dp).height(48.dp),
        verticalAlignment = Alignment.CenterVertically) {
        Row(Modifier.weight(1f).horizontalScroll(scroll), verticalAlignment = Alignment.CenterVertically) {
            Text("My Drive", Modifier.clickable(enabled = segments.isNotEmpty()) { open("") }.padding(vertical = 14.dp),
                style = MaterialTheme.typography.labelMedium)
            segments.forEachIndexed { index, name ->
                Icon(Icons.AutoMirrored.Filled.KeyboardArrowRight, null, Modifier.size(16.dp),
                    tint = MaterialTheme.colorScheme.onSurfaceVariant)
                Text(name, Modifier.clickable(enabled = index != segments.lastIndex) {
                    open(segments.take(index + 1).joinToString("/"))
                }.padding(vertical = 14.dp), style = monospace, maxLines = 1)
            }
        }
        Box {
            TextButton(onClick = { openSort = true }) {
                Text(selected.label)
                Icon(Icons.Default.KeyboardArrowDown, "Sort files", Modifier.size(16.dp))
            }
            DropdownMenu(expanded = openSort, onDismissRequest = { openSort = false }) {
                FileSort.entries.forEach { order ->
                    DropdownMenuItem(text = { Text(order.label) }, onClick = {
                        sort(order)
                        openSort = false
                    })
                }
            }
        }
    }
    HorizontalDivider(Modifier.padding(horizontal = 16.dp), thickness = Dp.Hairline, color = MaterialTheme.colorScheme.outlineVariant)
}

@Composable
private fun FileRow(item: DriveItem, open: (String) -> Unit, inspect: (DriveItem) -> Unit) {
    Row(
        Modifier.fillMaxWidth().height(56.dp).clickable {
            if (item.isFolder) open(item.path) else inspect(item)
        },
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Icon(painterResource(if (item.isFolder) R.drawable.ic_folder else R.drawable.ic_file), null,
            Modifier.size(20.dp), tint = if (item.isFolder) MaterialTheme.colorScheme.primary
            else MaterialTheme.colorScheme.onSurfaceVariant)
        Column(Modifier.padding(start = 12.dp, end = 8.dp).weight(1f)) {
            Text(item.name, maxLines = 1, overflow = TextOverflow.Ellipsis, style = MaterialTheme.typography.bodyMedium)
            Text(formatDate(item.updatedAt), maxLines = 1, style = monospace, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        if (!item.isFolder) Text(formatSize(item.sizeBytes), style = monospace, color = MaterialTheme.colorScheme.onSurfaceVariant)
        IconButton(onClick = { inspect(item) }) { Icon(Icons.Default.MoreVert, "Actions for ${item.name}", Modifier.size(18.dp)) }
        if (item.isFolder) Icon(Icons.AutoMirrored.Filled.KeyboardArrowRight, null, Modifier.size(16.dp),
            tint = MaterialTheme.colorScheme.onSurfaceVariant)
    }
    HorizontalDivider(thickness = Dp.Hairline, color = MaterialTheme.colorScheme.outlineVariant)
}

@Composable
private fun LoadingRow() {
    val alpha by androidx.compose.animation.core.rememberInfiniteTransition(label = "Loading rows").animateFloat(
        initialValue = 0.45f, targetValue = 0.85f, animationSpec = androidx.compose.animation.core.infiniteRepeatable(
            androidx.compose.animation.core.tween(680), androidx.compose.animation.core.RepeatMode.Reverse),
        label = "Loading row opacity",
    )
    val neutral = MaterialTheme.colorScheme.outlineVariant.copy(alpha = alpha)
    Row(Modifier.fillMaxWidth().height(56.dp), horizontalArrangement = Arrangement.spacedBy(12.dp),
        verticalAlignment = Alignment.CenterVertically) {
        Box(Modifier.size(20.dp).background(neutral, RoundedCornerShape(4.dp)))
        Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
            Box(Modifier.width(132.dp).height(10.dp).background(neutral, RoundedCornerShape(3.dp)))
            Box(Modifier.width(76.dp).height(7.dp).background(neutral, RoundedCornerShape(3.dp)))
        }
    }
    HorizontalDivider(thickness = Dp.Hairline, color = MaterialTheme.colorScheme.outlineVariant)
}

@Composable
internal fun BackupPane(
    backup: BackupStatus,
    network: NetworkState,
    permitted: Boolean,
    toggle: (Boolean) -> Unit,
    run: () -> Unit,
) {
    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(16.dp)) {
        Surface(
            color = MaterialTheme.colorScheme.surface,
            shape = MaterialTheme.shapes.medium,
            border = androidx.compose.foundation.BorderStroke(1.dp, MaterialTheme.colorScheme.outlineVariant),
        ) {
            Column(Modifier.fillMaxWidth()) {
                Row(Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 8.dp).heightIn(min = 56.dp),
                    horizontalArrangement = Arrangement.spacedBy(12.dp), verticalAlignment = Alignment.CenterVertically) {
                    Icon(painterResource(R.drawable.ic_backup), null, Modifier.size(22.dp),
                        tint = MaterialTheme.colorScheme.primary)
                    Column(Modifier.weight(1f)) {
                        Text("Camera auto-upload", style = MaterialTheme.typography.titleSmall)
                        Text("DCIM/Camera → Camera Uploads", style = monospace, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    }
                    Switch(checked = backup.enabled, onCheckedChange = toggle,
                        modifier = Modifier.semantics { contentDescription = "Camera auto-upload" })
                }
                HorizontalDivider(thickness = Dp.Hairline, color = MaterialTheme.colorScheme.outlineVariant)
                Column(Modifier.fillMaxWidth().padding(12.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    Text("Uploads new photos and videos over unmetered Wi-Fi with Tailscale.",
                        color = MaterialTheme.colorScheme.onSurfaceVariant)
                    if (backup.enabled) {
                        Text(when {
                            !permitted -> "Photo and video access required"
                            network != NetworkState.Connected -> "Waiting for Tailscale"
                            else -> backup.message ?: "Scheduled"
                        }, style = MaterialTheme.typography.labelMedium)
                        TextButton(onClick = run) { Text(if (permitted) "Back up now" else "Allow access") }
                    }
                }
            }
        }
    }
}

@Composable
internal fun TransfersPane(entries: List<Transfer>, retry: (Transfer) -> Unit, dismiss: (Transfer) -> Unit) {
    val ordered = remember(entries) { entries.sortedBy { it.phase == TransferPhase.Done } }
    LazyColumn(Modifier.fillMaxSize(), contentPadding = PaddingValues(horizontal = 16.dp)) {
        if (ordered.isEmpty()) item { EmptyPane("No transfers") {} }
        items(ordered, key = { it.id }) { entry ->
            Column(Modifier.fillMaxWidth().heightIn(min = 56.dp).padding(vertical = 8.dp),
                verticalArrangement = Arrangement.spacedBy(4.dp)) {
                Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                    Icon(painterResource(R.drawable.ic_transfers), null, Modifier.size(20.dp),
                        tint = MaterialTheme.colorScheme.onSurfaceVariant)
                    Column(Modifier.weight(1f).padding(start = 10.dp)) {
                        Text(entry.path.substringAfterLast('/'), maxLines = 1, overflow = TextOverflow.Ellipsis,
                            style = MaterialTheme.typography.titleSmall)
                        val verb = if (entry.kind == TransferKind.Upload) "Upload" else "Download"
                        val label = when (entry.phase) {
                            TransferPhase.Queued -> "Queued"
                            TransferPhase.Preparing -> if (entry.kind == TransferKind.Upload) "Hashing…" else "Preparing…"
                            TransferPhase.Transferring -> "${formatSize(entry.bytes)} / ${formatSize(entry.size)}"
                            TransferPhase.Backoff -> "Retrying (${entry.attempt}/3)…"
                            TransferPhase.Failed -> if (entry.needsRehash) "Integrity mismatch" else "Failed"
                            TransferPhase.Done -> "Complete"
                        }
                        val color = when (entry.phase) {
                            TransferPhase.Failed -> LocalDriveColors.current.danger
                            TransferPhase.Backoff -> LocalDriveColors.current.warning
                            TransferPhase.Done -> LocalDriveColors.current.online
                            else -> MaterialTheme.colorScheme.onSurfaceVariant
                        }
                        Text("$verb · $label", style = monospace, color = color)
                    }
                    if (entry.phase == TransferPhase.Failed) {
                        TextButton(onClick = { retry(entry) }) {
                            Text(if (entry.needsRehash) "Re-hash & Retry" else "Retry")
                        }
                    }
                    if (!entry.pending) {
                        TextButton(onClick = { dismiss(entry) }) { Text("Dismiss") }
                    }
                }
                if (entry.phase == TransferPhase.Transferring && entry.size > 0) {
                    LinearProgressIndicator(
                        progress = { (entry.bytes.toDouble() / entry.size).toFloat().coerceIn(0f, 1f) },
                        modifier = Modifier.fillMaxWidth().padding(start = 30.dp).height(3.dp),
                    )
                }
                entry.errorMessage?.let {
                    Text(it, Modifier.padding(start = 30.dp), color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
            }
            HorizontalDivider(thickness = Dp.Hairline, color = MaterialTheme.colorScheme.outlineVariant)
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun FileInspector(
    preview: PreviewState,
    online: Boolean,
    dismiss: () -> Unit,
    open: (String) -> Unit,
    download: (DriveItem) -> Unit,
    share: (DriveItem) -> Unit,
    delete: (DriveItem) -> Unit,
) {
    val item = preview.item
    ModalBottomSheet(onDismissRequest = dismiss, containerColor = MaterialTheme.colorScheme.surface,
        shape = RoundedCornerShape(topStart = 10.dp, topEnd = 10.dp)) {
        Column(
            Modifier.fillMaxWidth().verticalScroll(rememberScrollState()).padding(start = 16.dp, end = 16.dp, bottom = 16.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Text(item.name, style = MaterialTheme.typography.titleLarge)
            Text(item.path, style = monospace, color = MaterialTheme.colorScheme.onSurfaceVariant)
            Text(if (item.isFolder) "Folder · ${formatDate(item.updatedAt)}"
                else "${formatSize(item.sizeBytes)} · ${formatDate(item.updatedAt)}",
                style = monospace, color = MaterialTheme.colorScheme.onSurfaceVariant)
            item.sha256?.let { hash ->
                SelectionContainer { Text("SHA-256  $hash", style = monospace) }
            }
            if (preview.loading) {
                LinearProgressIndicator(Modifier.fillMaxWidth().height(3.dp))
            }
            preview.message?.let { Text(it, style = MaterialTheme.typography.bodySmall) }
            when (val content = preview.content) {
                is PreviewContent.Text -> Surface(
                    color = MaterialTheme.colorScheme.surfaceVariant, shape = MaterialTheme.shapes.small,
                ) {
                    Box(Modifier.fillMaxWidth().heightIn(max = 180.dp).verticalScroll(rememberScrollState()).padding(12.dp)) {
                        SelectionContainer { Text(content.value, style = monospace) }
                    }
                }
                is PreviewContent.Image -> {
                    Image(content.bitmap.asImageBitmap(), "Preview of ${item.name}",
                        Modifier.fillMaxWidth().heightIn(max = 240.dp))
                    content.caption?.let { Text(it, style = MaterialTheme.typography.bodySmall) }
                }
                null -> Unit
            }
            HorizontalDivider(thickness = Dp.Hairline, color = MaterialTheme.colorScheme.outlineVariant)
            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                if (item.isFolder) {
                    TextButton(onClick = { dismiss(); open(item.path) }) { Text("Open folder") }
                } else {
                    TextButton(enabled = online, onClick = { download(item) }) { Text("Download") }
                    TextButton(enabled = online, onClick = { share(item) }) { Text("Create share link") }
                }
                Spacer(Modifier.weight(1f))
                TextButton(enabled = online, onClick = { delete(item) }) { Text("Delete", color =
                    if (online) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.onSurfaceVariant) }
            }
        }
    }
}

@Composable
private fun EmptyPane(title: String, action: @Composable () -> Unit) {
    Column(
        Modifier.fillMaxWidth().padding(vertical = 48.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Icon(Icons.Default.Info, null, Modifier.size(18.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
        Text(title, style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
        action()
    }
}

private val dateFormat = DateTimeFormatter.ofPattern("MMM d, HH:mm").withZone(ZoneId.systemDefault())
internal fun formatDate(timestamp: String): String =
    runCatching { dateFormat.format(Instant.parse(timestamp)) }.getOrDefault(timestamp)
