package com.taildrive.android

import java.nio.file.Files
import java.util.concurrent.TimeUnit
import kotlinx.coroutines.runBlocking
import kotlinx.serialization.encodeToString
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import okhttp3.mockwebserver.RecordedRequest
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test

class HttpTailDriveRepositoryTest {
    private val server = MockWebServer()
    private val directory = Files.createTempDirectory("taildrive-http").toFile()
    private val waits = mutableListOf<Long>()
    private val progress = mutableListOf<TransferProgress>()
    private val quota = Quota(20, 200_000_000, 199_999_980, "ok")

    @Before fun startServer() { server.start() }

    @After fun cleanUp() {
        server.shutdown()
        directory.deleteRecursively()
    }

    @Test fun authenticatedHealthListingAndShareUseTheRealApiRoutes() = runBlocking {
        val repository = repository()
        server.enqueue(json(200, driveJson.encodeToString(Health("macmini-ci", quota))))
        server.enqueue(json(200, """{"path":"docs/café +","items":[],"next_cursor":null}"""))
        server.enqueue(json(201, """{"url":"/s/demo","expires_at":"2026-11-24T00:00:00Z"}"""))

        assertEquals(Health("macmini-ci", quota), repository.health())
        assertTrue(repository.list("docs/cafe\u0301 +", "a/+").items.isEmpty())
        assertEquals(server.url("/s/demo").toString(), repository.share("docs/spec.pdf").url)

        assertEquals("/v1/health", take("GET").path)
        val listing = take("GET").requestUrl!!
        assertEquals("/v1/files", listing.encodedPath)
        assertEquals("docs/café +", listing.queryParameter("path"))
        assertEquals("a/+", listing.queryParameter("cursor"))
        assertEquals("100", listing.queryParameter("limit"))
        val share = take("POST")
        assertEquals("/v1/shares", share.path)
        val request = driveJson.decodeFromString<Map<String, kotlinx.serialization.json.JsonElement>>(share.body.readUtf8())
        assertEquals("86400", request.getValue("expires_in_seconds").toString())
    }

    @Test fun thresholdFileStreamsInOnePutAndConfirmsTheHash() = runBlocking {
        val source = source(MULTIPART_THRESHOLD)
        val repository = repository()
        val expected = result("one.bin", source.digest)
        server.enqueue(json(201, driveJson.encodeToString(expected)))
        assertEquals(expected, repository.upload("one.bin", source, progress::add))
        val request = take("PUT")
        assertEquals("/v1/files?path=one.bin", request.path)
        assertEquals(source.digest.bytes.toString(), request.getHeader("Content-Length"))
        assertEquals(source.digest.hex, request.getHeader(CHECKSUM_HEADER))
        assertEquals(source.digest, sha256(request.body.inputStream()))
        assertTrue(waits.isEmpty())
    }

    @Test fun multipartReplaysOnlyTheFailedPartAndCompletesWithWholeDigest() = runBlocking {
        val source = source(MULTIPART_THRESHOLD + 23)
        val expected = result("folder/large.bin", source.digest)
        val repository = repository()
        server.enqueue(json(201, """{"upload_id":"u_1","part_size":$MULTIPART_THRESHOLD}"""))
        server.enqueue(json(503, """{"error":{"code":"ERR_BUSY","message":"Try again","retryable":true}}"""))
        server.enqueue(MockResponse().setResponseCode(204))
        server.enqueue(MockResponse().setResponseCode(204))
        server.enqueue(json(201, driveJson.encodeToString(expected)))

        assertEquals(expected, repository.upload("folder/large.bin", source, progress::add))
        val initialization = take("POST")
        assertEquals("/v1/uploads", initialization.path)
        val initBody = initialization.body.readUtf8()
        assertTrue(initBody.contains("folder/large.bin"))
        assertTrue(initBody.contains("\"size_bytes\":${source.digest.bytes}"))

        val first = take("PUT")
        val replay = take("PUT")
        val tail = take("PUT")
        assertEquals("/v1/uploads/u_1/parts/1", first.path)
        assertEquals(first.path, replay.path)
        assertEquals("/v1/uploads/u_1/parts/2", tail.path)
        assertEquals(MULTIPART_THRESHOLD.toLong(), first.bodySize)
        assertEquals(23L, tail.bodySize)
        assertEquals(first.getHeader(CHECKSUM_HEADER), replay.getHeader(CHECKSUM_HEADER))
        assertEquals(first.body.snapshot(), replay.body.snapshot())
        for (part in listOf(first, tail)) {
            assertEquals(sha256(part.body.inputStream()).hex, part.getHeader(CHECKSUM_HEADER))
        }

        val complete = take("POST")
        assertEquals("/v1/uploads/u_1/complete", complete.path)
        assertEquals(source.digest.hex, complete.getHeader(CHECKSUM_HEADER))
        assertEquals(listOf(1000L), waits)
        assertTrue(progress.contains(TransferProgress.Backoff(1)))
        assertEquals(TransferProgress.Bytes(source.digest.bytes, source.digest.bytes), progress.last())
    }

    @Test fun rejectedMultipartCompletionAbortsAndLeavesChecksumErrorForTheQueue() = runBlocking {
        val source = source(MULTIPART_THRESHOLD + 1)
        val repository = repository()
        server.enqueue(json(201, """{"upload_id":"u_bad","part_size":$MULTIPART_THRESHOLD}"""))
        repeat(2) { server.enqueue(MockResponse().setResponseCode(204)) }
        server.enqueue(json(422, """{"error":{"code":"ERR_CHECKSUM_MISMATCH","message":"Wrong checksum"}}"""))
        server.enqueue(MockResponse().setResponseCode(204))

        val failure = runCatching { repository.upload("file.bin", source, progress::add) }.exceptionOrNull() as DriveFailure
        assertEquals("ERR_CHECKSUM_MISMATCH", failure.code)
        assertEquals(422, failure.status)
        assertTrue(waits.isEmpty())
        take("POST")
        repeat(2) { take("PUT") }
        take("POST")
        assertEquals("/v1/uploads/u_bad", take("DELETE").path)
    }

    @Test fun verifiedDownloadsReplaceTheDestinationAndCorruptionDoesNot() = runBlocking {
        val repository = repository()
        val target = directory.resolve("download")
        target.writeText("previous")
        val digest = sha256("authentic".byteInputStream())
        server.enqueue(MockResponse().setBody("authentic").setHeader(CHECKSUM_HEADER, digest.hex))
        repository.download("docs/file.txt", target)
        assertEquals("authentic", target.readText())
        assertEquals("/v1/files/content?path=docs%2Ffile.txt", take("GET").path)

        server.enqueue(MockResponse().setBody("corrupted").setHeader(CHECKSUM_HEADER, digest.hex))
        val failure = runCatching { repository.download("docs/file.txt", target) }.exceptionOrNull() as DriveFailure
        assertEquals("ERR_DOWNLOAD_INTEGRITY", failure.code)
        assertEquals("authentic", target.readText())
        assertTrue(waits.isEmpty())
        assertEquals(listOf("download"), directory.list()?.toList())
    }

    private fun repository() = HttpTailDriveRepository(Connection(server.url("/").toString(), "test-token"),
        pause = { waits += it })

    private fun source(bytes: Int): UploadSource {
        val file = directory.resolve("source")
        file.writeBytes(ByteArray(bytes) { (it % 251).toByte() })
        return UploadSource(file, file.inputStream().use { sha256(it) }, "application/octet-stream")
    }

    private fun result(path: String, digest: Digest) = UploadResult(
        DriveItem("f_1", path.substringAfterLast('/'), path, "file", digest.bytes,
            "application/octet-stream", digest.hex, "2026-11-22T19:00:00Z"),
        quota,
    )

    private fun take(method: String): RecordedRequest {
        val request = checkNotNull(server.takeRequest(5, TimeUnit.SECONDS))
        assertEquals(method, request.method)
        assertEquals("Bearer test-token", request.getHeader("Authorization"))
        return request
    }

    private fun json(status: Int, body: String): MockResponse =
        MockResponse().setResponseCode(status).setHeader("Content-Type", "application/json").setBody(body)
}
