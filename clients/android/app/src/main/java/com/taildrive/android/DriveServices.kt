package com.taildrive.android

import android.app.Application
import android.content.ContentResolver
import android.content.Context
import android.content.Intent
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.net.Uri
import android.os.Handler
import android.os.Looper
import android.provider.OpenableColumns
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import androidx.work.WorkManager
import java.io.File
import java.io.IOException
import java.security.KeyStore
import java.util.UUID
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.withContext
import kotlinx.serialization.Serializable
import kotlinx.serialization.encodeToString

internal class TailDriveApplication : Application() {
    val drive by lazy { DriveServices(this) }
}

internal val Context.drive: DriveServices
    get() = (applicationContext as TailDriveApplication).drive

internal class DriveServices(val context: Context) {
    val credentials = CredentialStore(context)
    val network = TailnetMonitor(context)
    val repositories = Repositories(context)
    val transfers = TransferStore(context)
    val backup = BackupStore(context)
    val cache = DirectoryCache(context)
    val work by lazy { WorkManager.getInstance(context) }
    val uploadLock = Mutex()

    suspend fun activeConnection(): Connection? = withContext(Dispatchers.IO) { credentials.load() }
}

internal class CredentialStore(context: Context) {
    private val prefs = context.getSharedPreferences("connection", Context.MODE_PRIVATE)
    private val vault by lazy { KeyStore.getInstance("AndroidKeyStore").apply { load(null) } }
    private val keyAlias = "taildrive-connection-v1"

    @Synchronized
    fun load(): Connection? {
        val ciphertext = prefs.getString("ciphertext", null) ?: return null
        val nonce = Base64.decode(prefs.getString("nonce", null), Base64.NO_WRAP)
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        val key = vault.getKey(keyAlias, null) as? SecretKey ?: throw IOException("Stored credentials are unavailable.")
        cipher.init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(128, nonce))
        val stored = driveJson.decodeFromString<StoredConnection>(
            cipher.doFinal(Base64.decode(ciphertext, Base64.NO_WRAP)).decodeToString(),
        )
        return Connection.parse(stored.url, stored.token)
    }

    @Synchronized
    fun save(connection: Connection) {
        val key = vault.getKey(keyAlias, null) as? SecretKey ?: KeyGenerator
            .getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore").run {
                init(KeyGenParameterSpec.Builder(keyAlias, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                    .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                    .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                    .setKeySize(256)
                    .build())
                generateKey()
            }
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.ENCRYPT_MODE, key)
        val stored = driveJson.encodeToString(StoredConnection(connection.url, connection.token))
        val ciphertext = Base64.encodeToString(cipher.doFinal(stored.encodeToByteArray()), Base64.NO_WRAP)
        if (!prefs.edit().putString("ciphertext", ciphertext)
                .putString("nonce", Base64.encodeToString(cipher.iv, Base64.NO_WRAP)).commit()) {
            throw IOException("The connection could not be saved.")
        }
    }

    @Serializable private data class StoredConnection(val url: String, val token: String)
}

internal enum class NetworkState { Offline, Disconnected, Connected }

internal class TailnetMonitor(context: Context) {
    private val manager = context.getSystemService(ConnectivityManager::class.java)
    private val handler = Handler(Looper.getMainLooper())
    private val observed = MutableStateFlow(current())
    val state = observed.asStateFlow()

    init {
        manager.registerDefaultNetworkCallback(callback())
        val vpns = NetworkRequest.Builder().removeCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
            .addTransportType(NetworkCapabilities.TRANSPORT_VPN).build()
        manager.registerNetworkCallback(vpns, callback())
    }

    @Suppress("DEPRECATION")
    fun current(): NetworkState {
        if (manager.activeNetwork == null) return NetworkState.Offline
        val vpn = manager.allNetworks.any { network ->
            manager.getNetworkCapabilities(network)?.hasTransport(NetworkCapabilities.TRANSPORT_VPN) == true &&
                manager.getLinkProperties(network)?.linkAddresses?.any { address ->
                    val bytes = address.address.address
                    (bytes.size == 4 && bytes[0] == 100.toByte() && (bytes[1].toInt() and 255) in 64..127) ||
                        bytes.take(6).toByteArray().contentEquals(TAILSCALE_V6_PREFIX)
                } == true
        }
        return if (vpn) NetworkState.Connected else NetworkState.Disconnected
    }

    fun update() { observed.value = current() }

    private fun callback() = object : ConnectivityManager.NetworkCallback() {
        private fun changed() { handler.post(::update) }
        override fun onAvailable(network: Network) = changed()
        override fun onLost(network: Network) = changed()
        override fun onCapabilitiesChanged(network: Network, capabilities: NetworkCapabilities) = changed()
        override fun onLinkPropertiesChanged(network: Network, properties: android.net.LinkProperties) = changed()
    }

    private companion object {
        val TAILSCALE_V6_PREFIX = byteArrayOf(0xfd.toByte(), 0x7a, 0x11, 0x5c, 0xa1.toByte(), 0xe0.toByte())
    }
}

internal class TransferStore(context: Context) {
    private val resolver = context.contentResolver
    private val prefs = context.getSharedPreferences("transfers", Context.MODE_PRIVATE)
    private val mutable = MutableStateFlow(
        prefs.getString("queue", null)?.let { driveJson.decodeFromString<List<Transfer>>(it) }.orEmpty(),
    )
    val entries = mutable.asStateFlow()

    @Synchronized
    fun add(connection: Connection, uri: Uri, path: String, mime: String?, kind: TransferKind, size: Long): Transfer {
        val grant = kind.grant
        resolver.takePersistableUriPermission(uri, grant)
        val task = Transfer(UUID.randomUUID().toString(), connection.accountKey,
            uri.toString(), path, mime, kind, size, createdAt = System.currentTimeMillis())
        try {
            save(listOf(task) + mutable.value)
        } catch (failure: IOException) {
            release(task)
            throw failure
        }
        return task
    }

    @Synchronized
    fun update(id: String, event: TransferEvent): Transfer? {
        val previous = mutable.value.firstOrNull { it.id == id } ?: return null
        val next = previous.advance(event)
        if (next == previous) return next
        val entries = mutable.value.map { if (it.id == id) next else it }
        if (event is TransferEvent.Update && event.progress is TransferProgress.Bytes &&
            previous.phase == next.phase) {
            mutable.value = entries
        } else {
            save(entries)
        }
        if (next.phase == TransferPhase.Done) release(next)
        return next
    }

    @Synchronized
    fun dismiss(id: String) {
        val entry = mutable.value.firstOrNull { it.id == id && !it.pending } ?: return
        save(mutable.value.filterNot { it.id == id })
        if (entry.phase == TransferPhase.Failed) release(entry)
    }

    private fun save(entries: List<Transfer>) {
        if (!prefs.edit().putString("queue", driveJson.encodeToString(entries)).commit()) {
            throw IOException("The transfer queue could not be saved.")
        }
        mutable.value = entries
    }

    private fun release(entry: Transfer) {
        if (mutable.value.none {
                it.id != entry.id && it.uri == entry.uri && it.kind == entry.kind &&
                    it.phase != TransferPhase.Done
            }) {
            runCatching { resolver.releasePersistableUriPermission(Uri.parse(entry.uri), entry.kind.grant) }
        }
    }

    private val TransferKind.grant: Int get() = when (this) {
        TransferKind.Upload -> Intent.FLAG_GRANT_READ_URI_PERMISSION
        TransferKind.Download -> Intent.FLAG_GRANT_WRITE_URI_PERMISSION
    }
}

@Serializable
internal data class CachedFolder(val items: List<DriveItem>, val health: Health?)

internal class DirectoryCache(context: Context) {
    private val prefs = context.getSharedPreferences("offline-folders", Context.MODE_PRIVATE)

    fun read(connection: Connection, path: String): CachedFolder? =
        prefs.getString(key(connection, path), null)?.let { driveJson.decodeFromString<CachedFolder>(it) }

    @Synchronized
    fun save(connection: Connection, path: String, folder: CachedFolder) {
        val key = key(connection, path)
        val recent = prefs.getString("recent", "").orEmpty().split(',').filter { it.isNotEmpty() }
        val order = listOf(key) + recent.filterNot { it == key }
        val edit = prefs.edit().putString(key, driveJson.encodeToString(folder))
            .putString("recent", order.take(32).joinToString(","))
        order.drop(32).forEach(edit::remove)
        edit.apply()
    }

    private fun key(connection: Connection, path: String) =
        "${connection.accountKey}:${sha256(path.byteInputStream()).hex}"
}

internal data class DocumentInfo(val name: String, val mime: String?, val size: Long)

internal fun ContentResolver.documentInfo(uri: Uri): DocumentInfo {
    val type = getType(uri)
    query(uri, arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE), null, null, null)?.use { cursor ->
        if (cursor.moveToFirst()) {
            val name = cursor.getString(0) ?: throw IOException("The document has no filename.")
            return DocumentInfo(name, type, if (cursor.isNull(1)) -1 else cursor.getLong(1))
        }
    }
    throw IOException("The document could not be opened.")
}

internal suspend fun <T> Context.withStagedUpload(
    uri: Uri,
    mime: String?,
    action: suspend (UploadSource) -> T,
): T = withContext(Dispatchers.IO) {
    val staged = File.createTempFile("upload-", ".stage", cacheDir)
    try {
        val digest = contentResolver.openInputStream(uri)?.use { input ->
            staged.outputStream().use { output ->
                digestStream(input, write = { bytes, count -> output.write(bytes, 0, count) }) { ensureActive() }
            }
        } ?: throw IOException("The source file could not be opened.")
        action(UploadSource(staged, digest, mime))
    } finally {
        staged.delete()
    }
}
