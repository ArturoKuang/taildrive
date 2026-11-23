package com.taildrive.android

import android.Manifest
import android.content.ContentUris
import android.content.Context
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.provider.MediaStore
import androidx.work.Constraints
import androidx.work.BackoffPolicy
import androidx.work.CoroutineWorker
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.ExistingWorkPolicy
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkerParameters
import androidx.work.workDataOf
import java.io.File
import java.io.IOException
import java.time.Instant
import java.time.ZoneOffset
import java.time.format.DateTimeFormatter
import java.util.concurrent.TimeUnit
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import kotlinx.serialization.Serializable
import kotlinx.serialization.encodeToString

internal fun DriveServices.schedule(transfer: Transfer, replace: Boolean = false) {
    val request = OneTimeWorkRequestBuilder<DocumentTransferWorker>()
        .setInputData(workDataOf("id" to transfer.id))
        .setConstraints(Constraints(requiredNetworkType = NetworkType.CONNECTED))
        .setBackoffCriteria(BackoffPolicy.LINEAR, 10, TimeUnit.SECONDS)
        .build()
    val policy = if (replace) ExistingWorkPolicy.REPLACE else ExistingWorkPolicy.KEEP
    work.enqueueUniqueWork("transfer-${transfer.id}", policy, request)
}

internal class DocumentTransferWorker(context: Context, params: WorkerParameters) : CoroutineWorker(context, params) {
    private val drive = context.drive

    override suspend fun doWork(): Result = withContext(Dispatchers.IO) {
        val id = inputData.getString("id") ?: return@withContext Result.failure()
        val entry = drive.transfers.entries.value.firstOrNull { it.id == id && it.pending }
            ?: return@withContext Result.success()
        try {
            val connection = drive.activeConnection()
            if (connection?.accountKey != entry.account) {
                throw DriveFailure("ERR_CONNECTION_CHANGED", "Reconnect to the original drive to retry.")
            }
            if (drive.network.current() != NetworkState.Connected) {
                drive.transfers.update(id, TransferEvent.WaitForNetwork)
                return@withContext Result.retry()
            }
            drive.transfers.update(id, TransferEvent.Start)
            val repository = drive.repositories.open(connection)
            val uri = Uri.parse(entry.uri)
            val progress: (TransferProgress) -> Unit = { value ->
                drive.transfers.update(id, TransferEvent.Update(value))
            }
            if (entry.kind == TransferKind.Upload) {
                applicationContext.withStagedUpload(uri, entry.mime) { source ->
                    drive.uploadLock.withLock {
                        val time = CONFLICT_TIME.format(Instant.ofEpochMilli(entry.createdAt))
                        uploadToDrive(repository, entry.path, source, "$time-${id.take(8)}", progress)
                    }
                }
            } else {
                val staged = File.createTempFile("export-", ".stage", applicationContext.cacheDir)
                try {
                    repository.download(entry.path, staged, progress)
                    val resolver = applicationContext.contentResolver
                    val output = resolver.openOutputStream(uri, "rwt") ?: throw IOException("Cannot open the destination.")
                    output.use {
                        staged.inputStream().use { input ->
                            val buffer = ByteArray(64 * 1024)
                            while (true) {
                                ensureActive()
                                val count = input.read(buffer)
                                if (count < 0) break
                                output.write(buffer, 0, count)
                            }
                        }
                    }
                } finally {
                    staged.delete()
                }
            }
            drive.transfers.update(id, TransferEvent.Finish)
            Result.success()
        } catch (cancelled: CancellationException) {
            throw cancelled
        } catch (failure: Exception) {
            if (failure is DriveFailure && failure.retryable && drive.network.current() != NetworkState.Connected) {
                drive.transfers.update(id, TransferEvent.WaitForNetwork)
                return@withContext Result.retry()
            }
            val code = (failure as? DriveFailure)?.code ?: "ERR_LOCAL_FILE"
            val message = when (code) {
                "ERR_CHECKSUM_MISMATCH" -> "Integrity mismatch. The server discarded the upload."
                else -> failure.message ?: "Transfer failed."
            }
            drive.transfers.update(id, TransferEvent.Fail(code, message))
            Result.success()
        }
    }

}

internal fun Quota.checkUpload(bytes: Long) {
    if (bytes > remainingBytes) {
        throw DriveFailure("ERR_QUOTA_EXCEEDED",
            "Storage limit reached. ${formatSize(remainingBytes)} available.", 507)
    }
}

internal suspend fun uploadToDrive(
    repository: TailDriveRepository,
    path: String,
    source: UploadSource,
    conflictTag: String,
    progress: (TransferProgress) -> Unit,
    siblingItems: List<DriveItem>? = null,
): DriveItem {
    val siblings = siblingItems ?: repository.listAll(path.substringBeforeLast('/', ""))
    val existing = siblings.firstOrNull { it.path == path }
    if (existing?.sha256.equals(source.digest.hex, ignoreCase = true)) return checkNotNull(existing)
    var target = path
    var suffix = conflictTag
    var index = 1
    while (siblings.any { it.path == target }) {
        if (index > 32) throw DriveFailure("ERR_CONFLICT", "No available conflict filename.")
        target = DrivePaths.conflict(path, "android", suffix)
        val conflict = siblings.firstOrNull { it.path == target }
        if (conflict?.sha256.equals(source.digest.hex, ignoreCase = true)) return checkNotNull(conflict)
        suffix = "$conflictTag-${index++}"
    }
    repository.health().quota.checkUpload(source.digest.bytes)
    return repository.upload(target, source, progress).item
}

internal fun cameraPermissions(): Array<String> = if (Build.VERSION.SDK_INT >= 33) {
    arrayOf(Manifest.permission.READ_MEDIA_IMAGES, Manifest.permission.READ_MEDIA_VIDEO)
} else {
    arrayOf(Manifest.permission.READ_EXTERNAL_STORAGE)
}

internal fun cameraPermissionRequest(): Array<String> = if (Build.VERSION.SDK_INT >= 34) {
    cameraPermissions() + Manifest.permission.READ_MEDIA_VISUAL_USER_SELECTED
} else cameraPermissions()

internal fun Context.hasCameraPermissions(): Boolean =
    cameraPermissions().all { checkSelfPermission(it) == PackageManager.PERMISSION_GRANTED }

internal fun DriveServices.scheduleBackup(connection: Connection, runNow: Boolean) {
    val name = backupWorkName(connection.accountKey)
    val constraints = Constraints(requiredNetworkType = NetworkType.UNMETERED)
    val data = workDataOf("account" to connection.accountKey)
    work.enqueueUniquePeriodicWork(name, ExistingPeriodicWorkPolicy.KEEP,
        PeriodicWorkRequestBuilder<CameraBackupWorker>(6, TimeUnit.HOURS)
            .setInitialDelay(6, TimeUnit.HOURS).setConstraints(constraints).setInputData(data).build())
    if (runNow) {
        val immediate = OneTimeWorkRequestBuilder<CameraBackupWorker>()
            .setConstraints(constraints).setInputData(data).build()
        work.enqueueUniqueWork("$name-now", ExistingWorkPolicy.KEEP, immediate)
    }
}

internal fun DriveServices.stopBackup(account: String) {
    val name = backupWorkName(account)
    work.cancelUniqueWork(name)
    work.cancelUniqueWork("$name-now")
}

private fun backupWorkName(account: String) = "camera-backup-$account"

@Serializable
internal data class BackupStatus(
    val enabled: Boolean = false,
    val message: String? = null,
)

internal class BackupStore(context: Context) {
    private val prefs = context.getSharedPreferences("camera-backup", Context.MODE_PRIVATE)
    private val mutable = MutableStateFlow(
        prefs.all.filterKeys { it.startsWith("status:") }.mapNotNull { (key, value) ->
            (value as? String)?.let { key.removePrefix("status:") to driveJson.decodeFromString<BackupStatus>(it) }
        }.toMap(),
    )
    val states = mutable.asStateFlow()
    val scanLock = kotlinx.coroutines.sync.Mutex()

    @Synchronized
    fun update(account: String, transform: (BackupStatus) -> BackupStatus) {
        val status = transform(mutable.value[account] ?: BackupStatus())
        if (!prefs.edit().putString("status:$account", driveJson.encodeToString(status)).commit()) {
            throw IOException("Camera backup settings could not be saved.")
        }
        mutable.value = mutable.value + (account to status)
    }

    fun uploaded(account: String, hash: String): Boolean = prefs.contains("hash:$account:$hash")

    fun record(account: String, hash: String) {
        if (!prefs.edit().putBoolean("hash:$account:$hash", true).commit()) {
            throw IOException("Camera backup history could not be saved.")
        }
    }
}

internal class CameraBackupWorker(context: Context, params: WorkerParameters) : CoroutineWorker(context, params) {
    private val drive = context.drive

    override suspend fun doWork(): Result = withContext(Dispatchers.IO) {
        val account = inputData.getString("account") ?: return@withContext Result.failure()
        val connection = drive.activeConnection()
        if (connection?.accountKey != account || drive.backup.states.value[account]?.enabled != true) {
            return@withContext Result.success()
        }
        if (!applicationContext.hasCameraPermissions()) {
            drive.backup.update(account) { it.copy(message = "Allow access to photos and videos to resume backup.") }
            return@withContext Result.success()
        }
        if (drive.network.current() != NetworkState.Connected) return@withContext Result.retry()
        drive.backup.scanLock.withLock {
            try {
                val repository = drive.repositories.open(connection)
                val media = cameraMedia()
                var siblings: MutableList<DriveItem>? = null
                var uploaded = 0
                var unreadable = 0
                for (item in media) {
                    ensureActive()
                    if (drive.network.current() != NetworkState.Connected) return@withLock Result.retry()
                    try {
                        applicationContext.withStagedUpload(item.uri, item.mime) { source ->
                            val hash = source.digest.hex
                            if (drive.backup.uploaded(account, hash)) return@withStagedUpload
                            drive.uploadLock.withLock {
                                val entries = siblings ?: cameraFolder(repository).toMutableList().also { siblings = it }
                                val match = entries.any { it.sha256.equals(hash, ignoreCase = true) }
                                if (!match) {
                                    val path = DrivePaths.child(CAMERA_FOLDER, item.name)
                                    val date = item.modified.takeIf { it > 0 }?.let(Instant::ofEpochSecond)
                                        ?: Instant.EPOCH
                                    val tag = "${CONFLICT_TIME.format(date)}-${hash.take(8)}"
                                    entries += uploadToDrive(repository, path, source, tag, {}, entries)
                                    uploaded++
                                }
                                drive.backup.record(account, hash)
                            }
                        }
                    } catch (_: IOException) {
                        unreadable++
                    } catch (_: IllegalArgumentException) {
                        unreadable++
                    }
                }
                val result = "$uploaded new uploads" + if (unreadable > 0) " · $unreadable unreadable files" else ""
                drive.backup.update(account) { it.copy(message = result) }
                Result.success()
            } catch (cancelled: CancellationException) {
                throw cancelled
            } catch (failure: Exception) {
                drive.backup.update(account) { it.copy(message = failure.message ?: "Camera backup failed.") }
                if (failure is DriveFailure && failure.retryable) Result.retry() else Result.success()
            }
        }
    }

    private suspend fun cameraFolder(repository: TailDriveRepository): List<DriveItem> {
        try {
            repository.createFolder(CAMERA_FOLDER)
        } catch (failure: DriveFailure) {
            if (failure.code != "ERR_ALREADY_EXISTS") throw failure
        }
        return repository.listAll(CAMERA_FOLDER)
    }

    private fun cameraMedia(): List<CameraMedia> = buildList {
        val resolver = applicationContext.contentResolver
        val path = if (Build.VERSION.SDK_INT >= 29) MediaStore.MediaColumns.RELATIVE_PATH else "_data"
        val selection = if (Build.VERSION.SDK_INT >= 29) "$path = ?" else "$path LIKE ?"
        val arguments = arrayOf(if (Build.VERSION.SDK_INT >= 29) "DCIM/Camera/" else "%/DCIM/Camera/%")
        val columns = arrayOf(MediaStore.MediaColumns._ID, MediaStore.MediaColumns.DISPLAY_NAME,
            MediaStore.MediaColumns.MIME_TYPE, MediaStore.MediaColumns.DATE_MODIFIED)
        val collections = listOf(MediaStore.Images.Media.EXTERNAL_CONTENT_URI, MediaStore.Video.Media.EXTERNAL_CONTENT_URI)
        for (collection in collections) {
            resolver.query(collection, columns, selection, arguments, "${MediaStore.MediaColumns._ID} ASC")?.use { rows ->
                while (rows.moveToNext()) {
                    val name = rows.getString(1) ?: continue
                    add(CameraMedia(ContentUris.withAppendedId(collection, rows.getLong(0)), name,
                        rows.getString(2), rows.getLong(3)))
                }
            }
        }
    }

    private data class CameraMedia(val uri: Uri, val name: String, val mime: String?, val modified: Long)

    private companion object {
        const val CAMERA_FOLDER = "Camera Uploads"
    }
}

private val CONFLICT_TIME: DateTimeFormatter = DateTimeFormatter.ofPattern("uuuuMMdd'T'HHmmss'Z'")
    .withZone(ZoneOffset.UTC)
