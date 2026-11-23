package com.taildrive.android

import java.io.File
import java.io.InputStream
import java.security.MessageDigest
import java.text.Normalizer
import kotlinx.coroutines.delay
import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonElement
import okhttp3.HttpUrl
import okhttp3.HttpUrl.Companion.toHttpUrlOrNull

internal val driveJson = Json { ignoreUnknownKeys = true }
internal const val MULTIPART_THRESHOLD = 8 * 1024 * 1024
internal const val CHECKSUM_HEADER = "X-Content-SHA256"
internal const val DEFAULT_SERVER = "http://100.97.87.111:8787"

internal data class Connection(val url: String, val token: String) {
    val accountKey = sha256("$url\n$token".byteInputStream()).hex

    override fun toString(): String = "Connection($url)"

    companion object {
        fun parse(host: String, token: String): Connection {
            val value = host.trim()
            val url = (if ("://" in value) value else "https://$value").toHttpUrlOrNull()
            require(url != null && url.encodedPath == "/" && url.query == null &&
                url.fragment == null && url.username.isEmpty() && url.password.isEmpty()) {
                "Enter a server origin, for example https://arturos-mac-mini.<tailnet>.ts.net."
            }
            require(url.scheme == "https" || url.host.isTailnetAddress()) {
                "Use HTTPS for hostnames. HTTP is available for Tailscale IPv4 addresses."
            }
            val secret = token.trim()
            require(secret.isNotEmpty() && secret.all { it in '\u0021'..'\u007e' }) {
                "Enter a valid scoped Bearer token."
            }
            return Connection(url.toString(), secret)
        }
    }
}

private fun String.isTailnetAddress(): Boolean {
    val octets = split('.').mapNotNull(String::toIntOrNull)
    return octets.size == 4 && octets[0] == 100 && octets[1] in 64..127 &&
        octets.all { it in 0..255 }
}

@Serializable
internal data class Quota(
    @SerialName("used_bytes") val usedBytes: Long,
    @SerialName("limit_bytes") val limitBytes: Long,
    @SerialName("remaining_bytes") val remainingBytes: Long,
    val status: String,
) {
    val fraction: Float get() = if (limitBytes > 0) {
        (usedBytes.toDouble() / limitBytes).toFloat().coerceIn(0f, 1f)
    } else 0f
}

@Serializable
internal data class Health(val node: String, val quota: Quota)

@Serializable
internal data class DriveItem(
    val id: String,
    val name: String,
    val path: String,
    val type: String,
    @SerialName("size_bytes") val sizeBytes: Long,
    val mime: String? = null,
    val sha256: String? = null,
    @SerialName("updated_at") val updatedAt: String,
) {
    val isFolder: Boolean get() = type == "folder"
}

@Serializable
internal data class FilePage(
    val items: List<DriveItem>,
    @SerialName("next_cursor") val nextCursor: String? = null,
)

@Serializable
internal data class UploadResult(val item: DriveItem, val quota: Quota)

@Serializable
internal data class ShareLink(val url: String, @SerialName("expires_at") val expiresAt: String) {
    fun absolute(base: HttpUrl): ShareLink =
        copy(url = base.resolve(url)?.toString() ?: throw DriveFailure("ERR_INVALID_SHARE", "Invalid share link."))
}

@Serializable
internal data class ErrorEnvelope(val error: ErrorBody)

@Serializable
internal data class ErrorBody(
    val code: String,
    val message: String,
    val retryable: Boolean = false,
    val details: JsonElement? = null,
)

internal class DriveFailure(
    val code: String,
    override val message: String,
    val status: Int? = null,
    val retryable: Boolean = false,
    val details: JsonElement? = null,
    cause: Throwable? = null,
) : Exception(message, cause) {
    val isQuota: Boolean get() = status == 507 || status == 413 ||
        code == "ERR_QUOTA_EXCEEDED" || code == "ERR_DISK_FLOOR"

    companion object {
        fun fromResponse(status: Int, body: String): DriveFailure {
            val error = runCatching { driveJson.decodeFromString<ErrorEnvelope>(body).error }.getOrNull()
            return DriveFailure(
                error?.code ?: "HTTP_$status",
                error?.message ?: "TailDrive returned HTTP $status.",
                status,
                retryable = status != 507 && (status == 429 || status in 500..599 ||
                    (status == 408 && error?.retryable == true)),
                details = error?.details,
            )
        }
    }
}

internal data class Digest(val bytes: Long, val hex: String)
internal data class UploadSource(val file: File, val digest: Digest, val mime: String?)

internal sealed interface TransferProgress {
    data class Bytes(val sent: Long, val total: Long) : TransferProgress
    data class Backoff(val attempt: Int) : TransferProgress
}

internal interface TailDriveRepository {
    suspend fun health(): Health
    suspend fun list(path: String, cursor: String? = null): FilePage
    suspend fun createFolder(path: String): DriveItem
    suspend fun delete(path: String, recursive: Boolean): Quota
    suspend fun share(path: String): ShareLink
    suspend fun upload(path: String, source: UploadSource, progress: (TransferProgress) -> Unit): UploadResult
    suspend fun download(path: String, destination: File, progress: (TransferProgress) -> Unit = {})
}

internal suspend fun TailDriveRepository.listAll(path: String): List<DriveItem> = buildList {
    var cursor: String? = null
    val seen = mutableSetOf<String>()
    do {
        val page = list(path, cursor)
        addAll(page.items)
        cursor = page.nextCursor
        if (cursor != null && !seen.add(cursor)) {
            throw DriveFailure("ERR_INVALID_RESPONSE", "The server repeated a file listing cursor.")
        }
    } while (cursor != null)
}

internal object DrivePaths {
    fun normalize(raw: String, allowRoot: Boolean = false): String {
        if (raw.isEmpty() && allowRoot) return raw
        val path = Normalizer.normalize(raw, Normalizer.Form.NFC)
        require(path.split('/').none { it.isEmpty() || it == "." || it == ".." } &&
            path.none { it == '\\' || it.isISOControl() } && path.utf8Size() <= 1024 &&
            Charsets.UTF_8.newEncoder().canEncode(path)) { "Enter a valid relative drive path." }
        return path
    }

    fun child(parent: String, name: String): String {
        require('/' !in name) { "A name cannot contain a slash." }
        return normalize(if (parent.isEmpty()) name else "$parent/$name")
    }

    fun conflict(raw: String, host: String, timestamp: String): String {
        val path = normalize(raw)
        val parent = path.substringBeforeLast('/', "")
        val name = path.substringAfterLast('/')
        val dot = name.lastIndexOf('.').takeIf { it > 0 } ?: name.length
        val tag = Normalizer.normalize("$host-$timestamp".filterNot {
            it == '/' || it == '\\' || it.isISOControl()
        }, Normalizer.Form.NFC)
        val suffix = " (conflict $tag)${name.substring(dot)}"
        val budget = minOf(255, 1024 - parent.utf8Size() - if (parent.isEmpty()) 0 else 1)
        var renamed = if (suffix.utf8Size() <= budget) {
            name.substring(0, dot).takeUtf8(budget - suffix.utf8Size()) + suffix
        } else {
            "${sha256((name + suffix).byteInputStream()).hex}.conflict".take(budget)
        }
        if (renamed == name) {
            renamed = (if (renamed.first() == 'c') "d" else "c") +
                renamed.substring(Character.charCount(renamed.codePointAt(0)))
        }
        return child(parent, renamed)
    }

    private fun String.utf8Size(): Int = toByteArray(Charsets.UTF_8).size

    private fun String.takeUtf8(budget: Int): String {
        var end = 0
        var remaining = budget
        while (end < length) {
            val point = codePointAt(end)
            val count = Character.charCount(point)
            remaining -= substring(end, end + count).utf8Size()
            if (remaining < 0) break
            end += count
        }
        return substring(0, end)
    }
}

internal fun sha256(input: InputStream, onBytes: (Long) -> Unit = {}): Digest =
    digestStream(input, onBytes = onBytes)

internal fun digestStream(
    input: InputStream,
    write: (ByteArray, Int) -> Unit = { _, _ -> },
    onBytes: (Long) -> Unit = {},
): Digest {
    val hash = MessageDigest.getInstance("SHA-256")
    val buffer = ByteArray(64 * 1024)
    var total = 0L
    while (true) {
        val count = input.read(buffer)
        if (count < 0) break
        hash.update(buffer, 0, count)
        write(buffer, count)
        total += count
        onBytes(total)
    }
    return Digest(total, hash.digest().joinToString("") { "%02x".format(it.toInt() and 0xff) })
}

internal fun Digest.verify(expected: String?, length: Long) {
    if (!hex.equals(expected, ignoreCase = true) || (length >= 0 && bytes != length)) {
        throw DriveFailure("ERR_DOWNLOAD_INTEGRITY", "The downloaded data failed SHA-256 verification.")
    }
}

internal suspend fun <T> retryTransfer(
    progress: (TransferProgress) -> Unit,
    pause: suspend (Long) -> Unit = { delay(it) },
    action: suspend () -> T,
): T {
    val backoff = listOf(1000L, 2000L, 4000L)
    for ((index, wait) in backoff.withIndex()) {
        try {
            return action()
        } catch (failure: DriveFailure) {
            if (!failure.retryable) throw failure
            progress(TransferProgress.Backoff(index + 1))
            pause(wait)
        }
    }
    return action()
}
