package com.taildrive.android

import java.io.ByteArrayOutputStream
import kotlinx.coroutines.runBlocking
import kotlinx.serialization.json.jsonObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test

class DriveContractTest {
    @Test fun pathNormalizationMatchesTheRustContract() {
        assertEquals("docs/café.txt", DrivePaths.normalize("docs/cafe\u0301.txt"))
        assertEquals("", DrivePaths.normalize("", allowRoot = true))
        assertEquals("é".repeat(512), DrivePaths.normalize("é".repeat(512)))
        for (invalid in listOf("", "/", "/root", "a//b", "a/../b", "a/./b", "folder/",
            "a\\b", "nul\u0000", "control\u0085", "é".repeat(513))) {
            assertThrows(invalid, IllegalArgumentException::class.java) { DrivePaths.normalize(invalid) }
        }
        assertThrows(IllegalArgumentException::class.java) { DrivePaths.child("docs", "a/b") }
    }

    @Test fun conflictPathsKeepTheExtensionAndFitTheByteLimit() {
        assertEquals("docs/spec (conflict android-20261122T190000Z).pdf",
            DrivePaths.conflict("docs/spec.pdf", "android", "20261122T190000Z"))
        assertEquals(".env (conflict ..evilhost-t)", DrivePaths.conflict(".env", "../evil\\host", "t"))
        val path = "é".repeat(510) + ".txt"
        val result = DrivePaths.conflict(path, "cafe\u0301", "t")
        assertTrue(result.endsWith(" (conflict café-t).txt"))
        assertTrue(result.toByteArray().size <= 255)
        val longParent = "p".repeat(1022)
        val tiny = DrivePaths.conflict("$longParent/q", "android", "t")
        assertEquals(longParent, tiny.substringBeforeLast('/'))
        assertNotEquals("q", tiny.substringAfterLast('/'))
        assertEquals(tiny, DrivePaths.normalize(tiny))
    }

    @Test fun serverOriginsAllowDirectTailnetIpv4AndHttps() {
        assertEquals("http://100.97.87.111:8787/", Connection.parse(DEFAULT_SERVER, "scoped").url)
        assertEquals("https://arturos-mac-mini.example.ts.net/",
            Connection.parse("arturos-mac-mini.example.ts.net", "scoped").url)
        assertThrows(IllegalArgumentException::class.java) {
            Connection.parse("http://100.128.1.2:8787", "scoped")
        }
        assertThrows(IllegalArgumentException::class.java) {
            Connection.parse("https://example.ts.net/v1", "scoped")
        }
        assertThrows(IllegalArgumentException::class.java) {
            Connection.parse("https://example.ts.net", "a\nb")
        }
    }

    @Test fun streamingDigestCopiesAndVerifiesBytes() {
        val abc = sha256("abc".byteInputStream())
        assertEquals("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad", abc.hex)
        abc.verify(abc.hex.uppercase(), 3)
        assertThrows(DriveFailure::class.java) { abc.verify(abc.hex, 4) }
        assertThrows(DriveFailure::class.java) { abc.verify(null, 3) }
        val bytes = ByteArray(160_000) { (it % 251).toByte() }
        val output = ByteArrayOutputStream()
        val progress = mutableListOf<Long>()
        val digest = digestStream(bytes.inputStream(), write = { data, count -> output.write(data, 0, count) },
            onBytes = progress::add)
        assertTrue(bytes.contentEquals(output.toByteArray()))
        assertEquals(sha256(bytes.inputStream()), digest)
        assertEquals(listOf(65_536L, 131_072L, 160_000L), progress)
    }

    @Test fun errorEnvelopesKeepStructuredDetailsAndRecoveryCodes() {
        val codes = mapOf(422 to "ERR_CHECKSUM_MISMATCH", 507 to "ERR_QUOTA_EXCEEDED", 401 to "ERR_UNAUTHORIZED")
        for ((status, code) in codes) {
            val failure = DriveFailure.fromResponse(status, """{"error":{
                "code":"$code","message":"From TailDrive","retryable":false,"details":{"remaining_bytes":42}}}""")
            assertEquals(code, failure.code)
            assertEquals(status, failure.status)
            assertEquals("From TailDrive", failure.message)
            assertFalse(failure.retryable)
            assertEquals(status == 507, failure.isQuota)
            assertEquals("42", failure.details?.jsonObject?.get("remaining_bytes")?.toString())
        }
        assertEquals("ERR_DISK_FLOOR", DriveFailure.fromResponse(507,
            """{"error":{"code":"ERR_DISK_FLOOR","message":"Disk floor"}}""").code)
        assertTrue(DriveFailure.fromResponse(429, "").retryable)
        assertTrue(DriveFailure.fromResponse(503, "proxy unavailable").retryable)
    }

    @Test fun retryBackoffStopsAfterThreeRetries() = runBlocking {
        val waits = mutableListOf<Long>()
        val states = mutableListOf<TransferProgress>()
        var calls = 0
        val error = runCatching {
            retryTransfer(states::add, { waits += it }) {
                calls++
                throw DriveFailure.fromResponse(503, "")
            }
        }.exceptionOrNull() as DriveFailure
        assertEquals(503, error.status)
        assertEquals(4, calls)
        assertEquals(listOf(1000L, 2000L, 4000L), waits)
        assertEquals((1..3).map(TransferProgress::Backoff), states)
    }
}
