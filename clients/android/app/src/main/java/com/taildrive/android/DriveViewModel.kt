package com.taildrive.android

import android.app.Application
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.pdf.PdfRenderer
import android.net.Uri
import android.os.ParcelFileDescriptor
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import java.io.DataInputStream
import java.io.File
import java.util.Locale
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.receiveAsFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

internal enum class DriveTab(val title: String) { Files("Files"), Backup("Backup"), Transfers("Transfers") }
internal enum class FileSort(val label: String) { Name("Name"), Largest("Largest"), Modified("Modified") }

internal data class BrowserState(
    val initializing: Boolean = true,
    val editingConnection: Boolean = false,
    val testing: Boolean = false,
    val setupError: String? = null,
    val connection: Connection? = null,
    val health: Health? = null,
    val tab: DriveTab = DriveTab.Files,
    val path: String = "",
    val files: List<DriveItem> = emptyList(),
    val query: String = "",
    val sort: FileSort = FileSort.Name,
    val loading: Boolean = false,
    val hasFolderSnapshot: Boolean = false,
    val folderError: String? = null,
    val quotaAlert: String? = null,
    val preview: PreviewState? = null,
    val share: ShareLink? = null,
) {
    val showSetup: Boolean get() = !initializing && (connection == null || editingConnection)
}

internal data class PreviewState(
    val item: DriveItem,
    val loading: Boolean = false,
    val content: PreviewContent? = null,
    val message: String? = null,
)

internal sealed interface PreviewContent {
    data class Text(val value: String) : PreviewContent
    data class Image(val bitmap: Bitmap, val caption: String? = null) : PreviewContent
}

internal class DriveViewModel(application: Application) : AndroidViewModel(application) {
    private val drive = application.drive
    private val mutable = MutableStateFlow(BrowserState())
    val state = mutable.asStateFlow()
    val network = drive.network.state
    val transfers = drive.transfers.entries
    val backups = drive.backup.states
    private val notices = Channel<String>(Channel.BUFFERED)
    val messages = notices.receiveAsFlow()
    private var browserJob: Job? = null
    private var previewJob: Job? = null
    private var connectingJob: Job? = null

    init {
        viewModelScope.launch {
            try {
                val existing = drive.activeConnection()
                mutable.update { it.copy(connection = existing, initializing = false) }
                if (existing != null) {
                    resumeWork(existing)
                    refresh()
                }
            } catch (cancelled: CancellationException) {
                throw cancelled
            } catch (_: Exception) {
                mutable.update { it.copy(initializing = false, setupError = "Enter your connection details again.") }
            }
        }
        viewModelScope.launch {
            var previous = network.value
            network.collect { status ->
                if (status != previous) {
                    if (status == NetworkState.Connected) {
                        state.value.connection?.let { resumeWork(it, reconnecting = true) }
                        refresh()
                    }
                    previous = status
                }
            }
        }
        viewModelScope.launch {
            var previous = transfers.value.associate { it.id to it.phase }
            transfers.collect { entries ->
                val account = state.value.connection?.accountKey
                val changed = entries.filter { it.account == account && previous[it.id] != it.phase }
                if (changed.any { it.phase == TransferPhase.Done && it.kind == TransferKind.Upload }) refresh()
                changed.filter { it.phase == TransferPhase.Failed }.forEach { entry ->
                    if (entry.needsRehash) notices.trySend(entry.errorMessage.orEmpty())
                    if (entry.errorCode in QUOTA_ERRORS) {
                        mutable.update { it.copy(quotaAlert = entry.errorMessage) }
                    }
                    if (entry.errorCode == "ERR_UNAUTHORIZED") editConnection("The server rejected the Bearer token.")
                }
                previous = entries.associate { it.id to it.phase }
            }
        }
    }

    fun connect(host: String, token: String) {
        if (connectingJob?.isActive == true) return
        connectingJob = viewModelScope.launch {
            mutable.update { it.copy(testing = true, setupError = null) }
            try {
                val connection = Connection.parse(host, token)
                if (drive.network.current() != NetworkState.Connected) {
                    throw DriveFailure("ERR_NO_VPN", "Connect Tailscale, then test the connection again.")
                }
                val health = drive.repositories.open(connection).health()
                val previous = state.value.connection
                withContext(Dispatchers.IO) { drive.credentials.save(connection) }
                if (previous != null && previous.accountKey != connection.accountKey) {
                    drive.stopBackup(previous.accountKey)
                }
                mutable.value = BrowserState(initializing = false, connection = connection, health = health)
                resumeWork(connection)
                refresh()
            } catch (cancelled: CancellationException) {
                throw cancelled
            } catch (failure: Exception) {
                mutable.update { it.copy(setupError = failure.message ?: "Could not reach TailDrive.") }
            } finally {
                mutable.update { it.copy(testing = false) }
            }
        }
    }

    fun editConnection(error: String? = null) {
        dismissPreview()
        mutable.update { it.copy(editingConnection = true, setupError = error) }
    }

    fun cancelSetup() {
        connectingJob?.cancel()
        mutable.update { it.copy(editingConnection = false, setupError = null) }
    }

    fun onResume() {
        drive.network.update()
        state.value.connection?.let { resumeWork(it) }
    }

    fun tab(tab: DriveTab) {
        mutable.update { it.copy(tab = tab) }
    }

    fun folder(path: String) {
        mutable.update { it.copy(tab = DriveTab.Files, path = path, files = emptyList(),
            query = "", hasFolderSnapshot = false) }
        refresh()
    }

    fun query(query: String) { mutable.update { it.copy(query = query) } }
    fun sort(sort: FileSort) { mutable.update { it.copy(sort = sort) } }

    fun refresh() {
        val connection = state.value.connection ?: return
        val path = state.value.path
        browserJob?.cancel()
        browserJob = viewModelScope.launch {
            mutable.update { it.copy(loading = true, folderError = null) }
            try {
                val cached = withContext(Dispatchers.IO) { drive.cache.read(connection, path) }
                if (cached != null) {
                    mutable.update {
                        it.copy(files = cached.items, health = it.health ?: cached.health, hasFolderSnapshot = true)
                    }
                }
                if (drive.network.current() != NetworkState.Connected) return@launch
                val repository = drive.repositories.open(connection)
                val health = repository.health()
                mutable.update { it.copy(health = health) }
                val files = repository.listAll(path)
                withContext(Dispatchers.IO) { drive.cache.save(connection, path, CachedFolder(files, health)) }
                mutable.update { it.copy(files = files, hasFolderSnapshot = true) }
            } catch (cancelled: CancellationException) {
                throw cancelled
            } catch (failure: Exception) {
                if ((failure as? DriveFailure)?.status == 401) {
                    editConnection(failure.message)
                } else {
                    mutable.update { it.copy(folderError = failure.message ?: "Could not load this folder.") }
                }
            } finally {
                mutable.update { it.copy(loading = false) }
            }
        }
    }

    fun addFolder(parent: String, name: String, onSuccess: () -> Unit) = action {
        it.createFolder(DrivePaths.child(parent, name))
        onSuccess()
        refresh()
    }

    fun delete(item: DriveItem) = action {
        val quota = it.delete(item.path, recursive = item.isFolder)
        mutable.update { state -> state.copy(health = state.health?.copy(quota = quota)) }
        dismissPreview()
        refresh()
        notices.trySend("Deleted ${item.name}")
    }

    fun share(item: DriveItem) = action { repository ->
        val link = repository.share(item.path)
        mutable.update { it.copy(share = link) }
    }

    fun dismissShare() { mutable.update { it.copy(share = null) } }

    fun queueUpload(uri: Uri, parent: String) = action { repository ->
        val connection = requireConnection()
        val resolution = drive.context.contentResolver
        val info = withContext(Dispatchers.IO) { resolution.documentInfo(uri) }
        if (info.size >= 0) repository.health().quota.checkUpload(info.size)
        val path = DrivePaths.child(parent, info.name)
        val task = withContext(Dispatchers.IO) {
            drive.transfers.add(connection, uri, path, info.mime, TransferKind.Upload, info.size)
        }
        drive.schedule(task)
        notices.trySend("Upload queued")
    }

    fun queueDownload(uri: Uri, item: DriveItem) = action {
        val connection = requireConnection()
        val task = withContext(Dispatchers.IO) {
            drive.transfers.add(connection, uri, item.path, item.mime, TransferKind.Download, item.sizeBytes)
        }
        drive.schedule(task)
        dismissPreview()
        notices.trySend("Download queued")
    }

    fun retry(entry: Transfer) {
        viewModelScope.launch {
            try {
                val pending = withContext(Dispatchers.IO) { drive.transfers.update(entry.id, TransferEvent.Retry) }
                pending?.takeIf { it.phase == TransferPhase.Queued }?.let { drive.schedule(it, replace = true) }
            } catch (cancelled: CancellationException) {
                throw cancelled
            } catch (failure: Exception) {
                showFailure(failure)
            }
        }
    }

    fun dismiss(entry: Transfer) {
        viewModelScope.launch {
            try {
                withContext(Dispatchers.IO) { drive.transfers.dismiss(entry.id) }
            } catch (cancelled: CancellationException) {
                throw cancelled
            } catch (failure: Exception) {
                showFailure(failure)
            }
        }
    }

    fun backup(enabled: Boolean, runNow: Boolean = enabled) {
        val connection = state.value.connection ?: return
        viewModelScope.launch {
            try {
                withContext(Dispatchers.IO) {
                    drive.backup.update(connection.accountKey) {
                        it.copy(enabled = enabled, message = if (enabled) "Waiting for unmetered Tailnet access" else null)
                    }
                }
                if (enabled) drive.scheduleBackup(connection, runNow) else drive.stopBackup(connection.accountKey)
            } catch (cancelled: CancellationException) {
                throw cancelled
            } catch (failure: Exception) {
                showFailure(failure)
            }
        }
    }

    fun preview(item: DriveItem) {
        previewJob?.cancel()
        mutable.update { it.copy(preview = PreviewState(item)) }
        if (item.isFolder || item.previewType == null) return
        if (network.value != NetworkState.Connected || item.sizeBytes > 24L * 1024 * 1024) {
            mutable.update { state ->
                state.copy(preview = state.preview?.copy(message = if (network.value == NetworkState.Connected) {
                    "Download to view files larger than 24 MiB."
                } else "Connect to TailDrive to preview this file."))
            }
            return
        }
        previewJob = viewModelScope.launch {
            mutable.update { it.copy(preview = PreviewState(item, loading = true)) }
            try {
                val repository = drive.repositories.open(requireConnection())
                val content = withContext(Dispatchers.IO) {
                    val file = File.createTempFile("preview-", ".stage", drive.context.cacheDir)
                    try {
                        repository.download(item.path, file)
                        readPreview(file, item)
                    } finally {
                        file.delete()
                    }
                }
                mutable.update {
                    it.copy(preview = PreviewState(item, content = content,
                        message = if (content == null) "Preview unavailable for this file." else null))
                }
            } catch (cancelled: CancellationException) {
                throw cancelled
            } catch (failure: Exception) {
                mutable.update { it.copy(preview = PreviewState(item, message = failure.message ?: "Preview unavailable.")) }
            }
        }
    }

    fun dismissPreview() {
        previewJob?.cancel()
        mutable.update { it.copy(preview = null) }
    }

    fun dismissQuota() { mutable.update { it.copy(quotaAlert = null) } }
    fun notify(message: String) { notices.trySend(message) }

    private fun resumeWork(connection: Connection, reconnecting: Boolean = false) {
        transfers.value.filter { it.account == connection.accountKey && it.pending }.forEach {
            drive.schedule(it, replace = reconnecting && it.phase == TransferPhase.Queued)
        }
        if (backups.value[connection.accountKey]?.enabled == true) drive.scheduleBackup(connection, runNow = false)
    }

    private fun action(block: suspend (TailDriveRepository) -> Unit) {
        viewModelScope.launch {
            try {
                if (drive.network.current() != NetworkState.Connected) {
                    throw DriveFailure("ERR_NO_VPN", "Connect Tailscale to reach TailDrive.")
                }
                block(drive.repositories.open(requireConnection()))
            } catch (cancelled: CancellationException) {
                throw cancelled
            } catch (failure: Exception) {
                showFailure(failure)
            }
        }
    }

    private fun requireConnection(): Connection = checkNotNull(state.value.connection) { "Connect to TailDrive first." }

    private fun showFailure(failure: Exception) {
        val driveFailure = failure as? DriveFailure
        val message = failure.message ?: "Something went wrong."
        when {
            driveFailure?.isQuota == true -> mutable.update { it.copy(quotaAlert = message) }
            driveFailure?.status == 401 -> editConnection(message)
            else -> notices.trySend(message)
        }
    }

    private companion object {
        val QUOTA_ERRORS = setOf("ERR_QUOTA_EXCEEDED", "ERR_DISK_FLOOR", "HTTP_413", "HTTP_507")
    }
}

internal fun formatSize(size: Long): String = when {
    size < 0 -> "—"
    size < 1024 -> "$size B"
    size < 1024L * 1024 -> String.format(Locale.getDefault(), "%.1f KiB", size / 1024.0)
    size < 1024L * 1024 * 1024 -> String.format(Locale.getDefault(), "%.1f MiB", size / 1048576.0)
    else -> String.format(Locale.getDefault(), "%.2f GiB", size / 1073741824.0)
}

private enum class PreviewType { Text, Image, Pdf }

private val DriveItem.previewType: PreviewType? get() = when {
    mime?.startsWith("image/") == true -> PreviewType.Image
    mime == "application/pdf" || name.endsWith(".pdf", ignoreCase = true) -> PreviewType.Pdf
    mime?.startsWith("text/") == true || mime in setOf("application/json", "application/xml") ||
        name.substringAfterLast('.', "").lowercase() in setOf("txt", "md", "json", "xml", "csv", "rs", "kt", "js", "py") ->
        PreviewType.Text
    else -> null
}

private fun readPreview(file: File, item: DriveItem): PreviewContent? = when (item.previewType) {
    PreviewType.Text -> file.inputStream().use {
        val prefix = ByteArray(minOf(file.length(), 32L * 1024).toInt())
        DataInputStream(it).readFully(prefix)
        val text = prefix.decodeToString()
        PreviewContent.Text(text + if (file.length() > 32 * 1024) "\n…" else "")
    }
    PreviewType.Image -> {
        val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
        BitmapFactory.decodeFile(file.path, bounds)
        val options = BitmapFactory.Options()
        options.inSampleSize = 1
        while (maxOf(bounds.outWidth, bounds.outHeight) / options.inSampleSize > 1600) {
            options.inSampleSize *= 2
        }
        BitmapFactory.decodeFile(file.path, options)?.let { PreviewContent.Image(it) }
    }
    PreviewType.Pdf -> ParcelFileDescriptor.open(file, ParcelFileDescriptor.MODE_READ_ONLY).use { descriptor ->
        PdfRenderer(descriptor).use { renderer ->
            if (renderer.pageCount == 0) null else renderer.openPage(0).use { page ->
                val scale = 1200.0 / maxOf(page.width, page.height)
                val bitmap = Bitmap.createBitmap((page.width * scale).toInt().coerceAtLeast(1),
                    (page.height * scale).toInt().coerceAtLeast(1), Bitmap.Config.ARGB_8888)
                bitmap.eraseColor(android.graphics.Color.WHITE)
                page.render(bitmap, null, null, PdfRenderer.Page.RENDER_MODE_FOR_DISPLAY)
                PreviewContent.Image(bitmap, "Page 1 of ${renderer.pageCount}")
            }
        }
    }
    null -> null
}
