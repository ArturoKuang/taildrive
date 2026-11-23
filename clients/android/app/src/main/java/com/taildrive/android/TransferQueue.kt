package com.taildrive.android

import kotlinx.serialization.Serializable

@Serializable internal enum class TransferKind { Upload, Download }
@Serializable internal enum class TransferPhase { Queued, Preparing, Transferring, Backoff, Failed, Done }

@Serializable
internal data class Transfer(
    val id: String,
    val account: String,
    val uri: String,
    val path: String,
    val mime: String?,
    val kind: TransferKind,
    val size: Long = -1,
    val createdAt: Long = 0,
    val phase: TransferPhase = TransferPhase.Queued,
    val bytes: Long = 0,
    val attempt: Int = 0,
    val errorCode: String? = null,
    val errorMessage: String? = null,
) {
    val pending: Boolean get() = phase != TransferPhase.Failed && phase != TransferPhase.Done
    val needsRehash: Boolean get() = errorCode == "ERR_CHECKSUM_MISMATCH"
}

internal sealed interface TransferEvent {
    data object Start : TransferEvent
    data object WaitForNetwork : TransferEvent
    data object Retry : TransferEvent
    data object Finish : TransferEvent
    data class Update(val progress: TransferProgress) : TransferEvent
    data class Fail(val code: String, val message: String) : TransferEvent
}

internal fun Transfer.advance(event: TransferEvent): Transfer = when (event) {
    TransferEvent.Start -> if (pending) {
        copy(phase = TransferPhase.Preparing, bytes = 0, attempt = 0)
    } else this
    TransferEvent.WaitForNetwork -> if (pending) {
        copy(phase = TransferPhase.Queued, bytes = 0, attempt = 0)
    } else this
    TransferEvent.Retry -> if (phase == TransferPhase.Failed) {
        copy(phase = TransferPhase.Queued, bytes = 0, attempt = 0, errorCode = null, errorMessage = null)
    } else this
    TransferEvent.Finish -> if (pending) {
        copy(phase = TransferPhase.Done, bytes = size.coerceAtLeast(0), attempt = 0)
    } else this
    is TransferEvent.Fail -> if (pending) {
        copy(phase = TransferPhase.Failed, errorCode = event.code, errorMessage = event.message)
    } else this
    is TransferEvent.Update -> if (pending && phase != TransferPhase.Queued) {
        when (val progress = event.progress) {
            is TransferProgress.Bytes -> copy(
                phase = TransferPhase.Transferring, bytes = progress.sent,
                size = progress.total.takeIf { it >= 0 } ?: size,
            )
            is TransferProgress.Backoff -> copy(phase = TransferPhase.Backoff, attempt = progress.attempt)
        }
    } else this
}
