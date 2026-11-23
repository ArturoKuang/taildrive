package com.taildrive.android

import java.io.File
import java.io.IOException
import java.io.RandomAccessFile
import java.net.Proxy
import java.nio.file.Files
import java.nio.file.StandardCopyOption
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.withContext
import kotlinx.serialization.Serializable
import kotlinx.serialization.SerialName
import kotlinx.serialization.encodeToString
import okhttp3.Call
import okhttp3.Callback
import okhttp3.HttpUrl.Companion.toHttpUrl
import okhttp3.MediaType
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.MediaType.Companion.toMediaTypeOrNull
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody
import okhttp3.RequestBody.Companion.toRequestBody
import okhttp3.Response
import okio.BufferedSink

internal class HttpTailDriveRepository(
    connection: Connection,
    private val client: OkHttpClient = directClient,
    private val pause: suspend (Long) -> Unit = { kotlinx.coroutines.delay(it) },
) : TailDriveRepository {
    private val base = connection.url.toHttpUrl()
    private val bearer = "Bearer ${connection.token}"

    override suspend fun health(): Health = json(request("health").get().build())

    override suspend fun list(path: String, cursor: String?): FilePage =
        json(request("files", path = DrivePaths.normalize(path, allowRoot = true),
            parameters = mapOf("limit" to "100", "cursor" to cursor)).get().build())

    override suspend fun createFolder(path: String): DriveItem =
        json(request("folders").post(body(FolderRequest(DrivePaths.normalize(path)))).build())

    override suspend fun delete(path: String, recursive: Boolean): Quota =
        json<DeleteResult>(request("files", path = DrivePaths.normalize(path),
            parameters = mapOf("recursive" to recursive.toString())).delete().build()).quota

    override suspend fun share(path: String): ShareLink =
        json<ShareLink>(request("shares").post(body(ShareRequest(DrivePaths.normalize(path), 86400))).build())
            .absolute(base)

    override suspend fun upload(
        path: String,
        source: UploadSource,
        progress: (TransferProgress) -> Unit,
    ): UploadResult = withContext(kotlinx.coroutines.Dispatchers.IO) {
        val remote = DrivePaths.normalize(path)
        val result = if (source.digest.bytes > MULTIPART_THRESHOLD) {
            uploadParts(remote, source, progress)
        } else {
            val request = request("files", path = remote)
                .header(CHECKSUM_HEADER, source.digest.hex)
                .put(object : RequestBody() {
                    override fun contentType(): MediaType = source.mime?.toMediaTypeOrNull() ?: BINARY
                    override fun contentLength(): Long = source.digest.bytes
                    override fun writeTo(sink: BufferedSink) {
                        source.file.inputStream().use { input ->
                            val buffer = ByteArray(64 * 1024)
                            var sent = 0L
                            while (true) {
                                val count = input.read(buffer)
                                if (count < 0) break
                                sink.write(buffer, 0, count)
                                sent += count
                                progress(TransferProgress.Bytes(sent, contentLength()))
                            }
                        }
                    }
                }).build()
            commit(remote, source.digest, progress) { json(request) }
        }
        if (!source.digest.hex.equals(result.item.sha256, ignoreCase = true)) {
            throw DriveFailure("ERR_INVALID_RESPONSE", "The server did not confirm the uploaded SHA-256.")
        }
        result
    }

    private suspend fun uploadParts(
        path: String,
        source: UploadSource,
        progress: (TransferProgress) -> Unit,
    ): UploadResult {
        val total = source.digest.bytes
        val session = retryTransfer(progress, pause) {
            json<UploadSession>(request("uploads").post(body(InitUpload(path, total, source.mime))).build())
        }
        try {
            val partSize = session.partSize
            if (partSize !in 1..MULTIPART_THRESHOLD) {
                throw DriveFailure("ERR_INVALID_RESPONSE", "Unsupported upload part size.")
            }
            RandomAccessFile(source.file, "r").use { file ->
                var part = 1L
                var sent = 0L
                while (sent < total) {
                    val bytes = ByteArray(minOf(partSize.toLong(), total - sent).toInt())
                    file.readFully(bytes)
                    val digest = sha256(bytes.inputStream())
                    val request = request("uploads", session.id, "parts", part.toString())
                        .header(CHECKSUM_HEADER, digest.hex)
                        .put(bytes.toRequestBody(BINARY)).build()
                    retryTransfer(progress, pause) {
                        progress(TransferProgress.Bytes(sent, total))
                        execute(request) { }
                    }
                    sent += bytes.size
                    progress(TransferProgress.Bytes(sent, total))
                    part++
                }
            }
            val complete = request("uploads", session.id, "complete")
                .header(CHECKSUM_HEADER, source.digest.hex)
                .post(ByteArray(0).toRequestBody()).build()
            return commit(path, source.digest, progress) { json(complete) }
        } catch (failure: Exception) {
            // Retain the server session after a network failure so initialization can recover it.
            if (failure is DriveFailure && !failure.retryable) {
                withContext(NonCancellable) {
                    runCatching { execute(request("uploads", session.id).delete().build()) { } }
                }
            }
            throw failure
        }
    }

    private suspend fun commit(
        path: String,
        digest: Digest,
        progress: (TransferProgress) -> Unit,
        write: suspend () -> UploadResult,
    ): UploadResult {
        var replay = false
        return retryTransfer(progress, pause) {
            if (replay) {
                val stored = listAll(path.substringBeforeLast('/', "")).firstOrNull {
                    it.path == path && it.sha256.equals(digest.hex, ignoreCase = true)
                }
                if (stored != null) return@retryTransfer UploadResult(stored, health().quota)
            }
            replay = true
            write()
        }
    }

    override suspend fun download(
        path: String,
        destination: File,
        progress: (TransferProgress) -> Unit,
    ) {
        val request = request("files", "content", path = DrivePaths.normalize(path))
            .header("Accept-Encoding", "identity")
            .get().build()
        retryTransfer(progress, pause) {
            execute(request) { response ->
                val body = response.body ?: missingBody()
                val partial = File.createTempFile("download-", ".part", destination.absoluteFile.parentFile)
                try {
                    val length = body.contentLength()
                    val digest = body.byteStream().use { stream ->
                        partial.outputStream().use { output ->
                            digestStream(stream, write = { buffer, count -> output.write(buffer, 0, count) }) {
                                progress(TransferProgress.Bytes(it, length))
                            }
                        }
                    }
                    digest.verify(response.header(CHECKSUM_HEADER), length)
                    Files.move(partial.toPath(), destination.toPath(), StandardCopyOption.REPLACE_EXISTING)
                } finally {
                    partial.delete()
                }
            }
        }
    }

    private fun request(
        vararg segments: String,
        path: String? = null,
        parameters: Map<String, String?> = emptyMap(),
    ): Request.Builder {
        val url = base.newBuilder().addPathSegment("v1")
        segments.forEach { url.addPathSegment(it) }
        if (path != null) url.addQueryParameter("path", path)
        parameters.forEach { (key, value) -> if (value != null) url.addQueryParameter(key, value) }
        return Request.Builder().url(url.build()).header("Authorization", bearer)
    }

    private suspend inline fun <reified T> json(request: Request): T = execute(request) {
        try {
            driveJson.decodeFromString<T>(it.body?.string() ?: missingBody())
        } catch (failure: IllegalArgumentException) {
            throw DriveFailure("ERR_INVALID_RESPONSE", "The server returned an invalid response.", cause = failure)
        }
    }

    private suspend fun <T> execute(request: Request, read: (Response) -> T): T =
        suspendCancellableCoroutine { continuation ->
            val call = client.newCall(request)
            continuation.invokeOnCancellation { call.cancel() }
            call.enqueue(object : Callback {
                override fun onFailure(call: Call, e: IOException) {
                    continuation.resumeWith(Result.failure(e.asNetworkFailure()))
                }

                override fun onResponse(call: Call, response: Response) {
                    val result = runCatching {
                        response.use {
                            continuation.context.ensureActive()
                            if (!it.isSuccessful) {
                                throw DriveFailure.fromResponse(it.code, it.peekBody(64 * 1024).string())
                            }
                            read(it)
                        }
                    }.recoverCatching { failure ->
                        throw if (failure is IOException) failure.asNetworkFailure() else failure
                    }
                    continuation.resumeWith(result)
                }
            })
        }

    @Serializable private data class FolderRequest(val path: String)
    @Serializable private data class DeleteResult(val quota: Quota)
    @Serializable private data class InitUpload(
        val path: String,
        @SerialName("size_bytes") val size: Long,
        val mime: String?,
    )
    @Serializable private data class UploadSession(
        @SerialName("upload_id") val id: String,
        @SerialName("part_size") val partSize: Int,
    )
    @Serializable private data class ShareRequest(
        val path: String,
        @SerialName("expires_in_seconds") val expiresIn: Long,
    )

    private companion object {
        val JSON = "application/json".toMediaType()
        val BINARY = "application/octet-stream".toMediaType()
        val directClient = OkHttpClient.Builder()
            .followRedirects(false).followSslRedirects(false)
            .retryOnConnectionFailure(false).proxy(Proxy.NO_PROXY).build()

        inline fun <reified T> body(value: T): RequestBody =
            driveJson.encodeToString(value).toRequestBody(JSON)

        fun missingBody(): Nothing =
            throw DriveFailure("ERR_INVALID_RESPONSE", "The server returned no response body.")

        fun IOException.asNetworkFailure() =
            DriveFailure("ERR_NETWORK", "Connection to TailDrive interrupted.", retryable = true, cause = this)
    }
}
