package com.taildrive.android

import kotlinx.serialization.encodeToString
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class TransferQueueTest {
    private val queued = Transfer("transfer", "account", "content://docs/photo",
        "Camera/photo.jpg", "image/jpeg", TransferKind.Upload, size = 100)

    @Test fun checksumFailureCanBeRehashedWithTheSamePersistedDocument() {
        val preparing = queued.advance(TransferEvent.Start)
        assertEquals(TransferPhase.Preparing, preparing.phase)
        val sending = preparing.advance(TransferEvent.Update(TransferProgress.Bytes(30, 100)))
        assertEquals(TransferPhase.Transferring, sending.phase)
        assertEquals(30L, sending.bytes)
        val waiting = sending.advance(TransferEvent.Update(TransferProgress.Backoff(1)))
        assertEquals(TransferPhase.Backoff, waiting.phase)
        assertEquals(1, waiting.attempt)
        val failure = waiting.advance(TransferEvent.Fail("ERR_CHECKSUM_MISMATCH", "Hash differs"))
        assertEquals(TransferPhase.Failed, failure.phase)
        assertTrue(failure.needsRehash)
        assertFalse(failure.pending)

        val restored = driveJson.decodeFromString<Transfer>(driveJson.encodeToString(failure))
        val requeued = restored.advance(TransferEvent.Retry)
        assertEquals(queued, requeued)
        assertNull(requeued.errorCode)
        val done = requeued.advance(TransferEvent.Start).advance(TransferEvent.Finish)
        assertEquals(TransferPhase.Done, done.phase)
        assertEquals(100L, done.bytes)
        assertFalse(done.pending)
        assertEquals(done, done.advance(TransferEvent.Retry))
        assertEquals(done, done.advance(TransferEvent.Update(TransferProgress.Bytes(0, 100))))
    }

    @Test fun networkLossReturnsActiveDownloadToThePersistentQueue() {
        val active = queued.copy(kind = TransferKind.Download, size = -1)
            .advance(TransferEvent.Start).advance(TransferEvent.Update(TransferProgress.Bytes(14, 80)))
        val waiting = active.advance(TransferEvent.WaitForNetwork)
        assertEquals(TransferPhase.Queued, waiting.phase)
        assertEquals(active.uri, waiting.uri)
        assertEquals(0L, waiting.bytes)
        assertEquals(80L, waiting.size)
        assertTrue(waiting.pending)
        assertEquals(waiting, waiting.advance(TransferEvent.Update(TransferProgress.Bytes(90, 100))))
    }
}
