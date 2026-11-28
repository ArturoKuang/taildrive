package com.taildrive.android

import android.content.Context
import com.taildrive.ffi.FfiException
import com.taildrive.ffi.FileItem
import com.taildrive.ffi.FileType
import com.taildrive.ffi.QuotaInfo
import com.taildrive.ffi.TailDriveFfiClient
import com.taildrive.ffi.TransferObserver
import java.io.File
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.withContext
import okhttp3.HttpUrl.Companion.toHttpUrl

internal class Repositories(private val clients: FfiClientCache) {
    suspend fun open(connection: Connection): TailDriveRepository = withContext(Dispatchers.IO) {
        clients.ensureLoaded()
        NativeTailDriveRepository(connection, clients)
    }
}

internal object NativeTls {
    @JvmStatic external fun initialize(context: Context)
}

private class NativeTailDriveRepository(
    private val connection: Connection,
    private val clients: FfiClientCache,
) : TailDriveRepository {
    override suspend fun health(): Health = call { health().let { Health(it.node, it.quota.model()) } }

    override suspend fun list(path: String, cursor: String?): FilePage = call {
        val page = listFiles(DrivePaths.normalize(path, allowRoot = true), cursor, 100u)
        FilePage(page.items.map { it.model() }, page.nextCursor)
    }

    override suspend fun createFolder(path: String): DriveItem = call {
        createFolder(DrivePaths.normalize(path)).model()
    }

    override suspend fun delete(path: String, recursive: Boolean): Quota = call {
        deletePath(DrivePaths.normalize(path), recursive).quota.model()
    }

    override suspend fun share(path: String): ShareLink = call {
        val share = createShare(DrivePaths.normalize(path), 86400uL)
        ShareLink(share.url, share.expiresAt).absolute(connection.url.toHttpUrl())
    }

    override suspend fun upload(
        path: String,
        source: UploadSource,
        progress: (TransferProgress) -> Unit,
    ): UploadResult {
        val observer = observer(progress)
        var replay = false
        return retryTransfer(progress) {
            if (replay) {
                val stored = listAll(path.substringBeforeLast('/', "")).firstOrNull {
                    it.path == path && it.sha256.equals(source.digest.hex, ignoreCase = true)
                }
                if (stored != null) return@retryTransfer UploadResult(stored, health().quota)
            }
            replay = true
            call {
                val result = uploadWithProgress(DrivePaths.normalize(path), source.file.absolutePath, null, observer)
                val item = result.item.model()
                if (!source.digest.hex.equals(item.sha256, ignoreCase = true)) {
                    throw DriveFailure("ERR_INVALID_RESPONSE", "The server did not confirm the uploaded SHA-256.")
                }
                UploadResult(item, result.quota.model())
            }
        }
    }

    override suspend fun download(
        path: String,
        destination: File,
        progress: (TransferProgress) -> Unit,
    ) {
        val observer = observer(progress)
        retryTransfer(progress) {
            call { downloadWithProgress(DrivePaths.normalize(path), destination.absolutePath, observer) }
        }
    }

    private suspend fun observer(callback: (TransferProgress) -> Unit): TransferObserver {
        val job = currentCoroutineContext()[Job]
        return object : TransferObserver {
            override fun progress(bytes: ULong, total: ULong): Boolean {
                if (job?.isActive == false) return false
                callback(TransferProgress.Bytes(bytes.toLong(), total.toLong()))
                return true
            }
        }
    }

    private suspend fun <T> call(action: TailDriveFfiClient.() -> T): T = withContext(Dispatchers.IO) {
        ensureActive()
        try {
            clients.withClient(connection, action)
        } catch (failure: FfiException) {
            ensureActive()
            throw when (failure) {
                is FfiException.Http -> {
                    val status = failure.status.toInt()
                    DriveFailure(failure.code, failure.message.orEmpty(), status, status != 507 &&
                        (failure.retryable || status == 429 || status in 500..599))
                }
                is FfiException.Transport ->
                    DriveFailure("ERR_NETWORK", failure.message.orEmpty(), retryable = failure.retryable)
                is FfiException.Integrity ->
                    DriveFailure("ERR_DOWNLOAD_INTEGRITY", failure.message.orEmpty())
                is FfiException.InvalidPath -> DriveFailure("ERR_INVALID_PATH", failure.message.orEmpty())
                is FfiException.Cancelled -> CancellationException("Transfer cancelled")
                else -> DriveFailure("ERR_NATIVE", failure.message.orEmpty())
            }
        }
    }
}

private fun FileItem.model() = DriveItem(
    id, name, path, if (kind == FileType.FOLDER) "folder" else "file",
    sizeBytes.toLong(), mime, sha256, updatedAt,
)

private fun QuotaInfo.model() = Quota(
    usedBytes.toLong(), limitBytes.toLong(), remainingBytes.toLong(), status.name.lowercase(),
)
