import Foundation
import TailDriveFFI
@testable import TailDriveSyncKit
import XCTest

final class TailDriveSyncKitTests: XCTestCase {
    func testRustPathNormalizationAndConflictNames() throws {
        let normalized = try normalizeDrivePath(raw: "documents/cafe\u{301}.txt")
        XCTAssertEqual(Array(normalized.utf8), Array("documents/café.txt".utf8))
        XCTAssertThrowsError(try normalizeDrivePath(raw: "documents/../secret"))
        XCTAssertThrowsError(try normalizeDrivePath(raw: "/absolute.txt"))
        XCTAssertEqual(
            try formatConflictPath(path: "documents/report.pdf", host: "macmini-ci", ts: "20261122T190000Z"),
            "documents/report (conflict macmini-ci-20261122T190000Z).pdf"
        )
        XCTAssertEqual(
            try formatConflictPath(path: ".env", host: "mac/mini", ts: "20261122T190000Z"),
            ".env (conflict macmini-20261122T190000Z)"
        )
    }

    func testRustStreamingSHA256() throws {
        try withTemporaryDirectory { root in
            let file = root.appendingPathComponent("abc.txt")
            try Data("abc".utf8).write(to: file)
            XCTAssertEqual(
                try hashFileSha256(localPath: file.path),
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
            )
            XCTAssertThrowsError(try hashFileSha256(localPath: root.appendingPathComponent("missing").path))
        }
    }

    func testRustSyncPlanComparesBothSidesToTheLastSyncedHash() throws {
        func entry(_ path: String, _ hash: String, baseline: String? = nil) -> SyncEntry {
            SyncEntry(path: path, kind: .file, sha256: hash, lastSyncedSha256: baseline)
        }
        let a = String(repeating: "a", count: 64)
        let b = String(repeating: "b", count: 64)
        let c = String(repeating: "c", count: 64)
        let local = [
            entry("conflict", a, baseline: c), entry("same", a),
            entry("folder/new", a), entry("local-change", a, baseline: b),
            entry("remote-change", b, baseline: b),
            SyncEntry(path: "folder", kind: .folder, sha256: nil, lastSyncedSha256: nil),
        ]
        let remote = [
            entry("remote-change", a), entry("same", a), entry("conflict", b),
            entry("local-change", b), entry("remote-only", a),
        ]
        XCTAssertEqual(try planFolderSync(localEntries: local, remoteEntries: remote), [
            .conflict(path: "conflict"), .createRemoteFolder(path: "folder"),
            .upload(path: "folder/new"), .upload(path: "local-change"),
            .download(path: "remote-change"), .download(path: "remote-only"),
        ])
        XCTAssertEqual(
            try planFolderSync(localEntries: local, remoteEntries: remote + [entry("folder", b)]).filter {
                if case .upload(path: "folder/new") = $0 { return true }
                return false
            },
            []
        )
        XCTAssertThrowsError(try planFolderSync(
            localEntries: [entry("café", a), entry("cafe\u{301}", b)], remoteEntries: []
        ))
    }

    func testLiveRustClientRoundTripForSingleAndMultipartFiles() throws {
        try withTestServer { server, root in
            let unauthenticated = try TailDriveFfiClient(baseUrl: server.absoluteString, bearerToken: nil)
            XCTAssertThrowsError(try unauthenticated.health()) { error in
                guard case FfiError.Http(401, "ERR_UNAUTHORIZED", _, _) = error else {
                    return XCTFail("Expected HTTP 401, got \(error)")
                }
            }
            let client = try makeClient(server)
            let initial = try client.health()
            XCTAssertEqual(initial.status, "ok")
            XCTAssertEqual(initial.node, "macmini-ci")
            XCTAssertEqual(initial.storageDriver, "local-fs")
            XCTAssertEqual(initial.quota.usedBytes, 0)
            XCTAssertEqual(initial.quota.limitBytes, 5 * 1024 * 1024 * 1024)
            XCTAssertEqual(try client.createFolder(path: "fixtures").kind, .folder)

            var total: UInt64 = 0
            for (name, data) in [
                ("empty.txt", Data()), ("small.txt", Data("TailDrive".utf8)),
                ("large.bin", Data(repeating: 0xa5, count: 8 * 1024 * 1024 + 123)),
            ] {
                let remote = "fixtures/\(name)"
                let upload = root.appendingPathComponent("upload")
                let download = root.appendingPathComponent("download")
                try data.write(to: upload)
                let result = try client.uploadFile(remotePath: remote, localFilePath: upload.path, ifMatch: nil)
                total += UInt64(data.count)
                XCTAssertEqual(result.item.sizeBytes, UInt64(data.count))
                XCTAssertEqual(result.item.sha256, try hashFileSha256(localPath: upload.path))
                XCTAssertEqual(result.quota.usedBytes, total)
                try client.downloadFile(remotePath: remote, destLocalPath: download.path)
                XCTAssertEqual(try Data(contentsOf: download), data)
                XCTAssertThrowsError(try client.uploadFile(
                    remotePath: remote, localFilePath: upload.path, ifMatch: "\"stale\""
                )) { error in
                    guard case FfiError.Http(409, "ERR_CONFLICT", _, _) = error else {
                        return XCTFail("Expected HTTP 409, got \(error)")
                    }
                }
            }

            let firstPage = try client.listFiles(path: "fixtures", cursor: nil, limit: 1)
            let cursor = try XCTUnwrap(firstPage.nextCursor)
            XCTAssertEqual(try client.listFiles(path: "fixtures", cursor: cursor, limit: nil).items.count, 2)
            let share = try client.createShare(path: "fixtures/small.txt", expiresInSeconds: 60)
            XCTAssertEqual(try client.listShares().map(\.id), [share.id])
            try client.revokeShare(id: share.id)
            XCTAssertEqual(try client.listShares(), [])
            XCTAssertEqual(try client.deletePath(path: "fixtures", recursive: true).quota.usedBytes, 0)
        }
    }

    func testMirrorPreservesLocalEditsWhenTheServerReturns409() throws {
        try withTestServer { server, root in
            let client = try makeClient(server)
            let competingClient = try makeClient(server)
            let folder = root.appendingPathComponent("mirror")
            let file = folder.appendingPathComponent("report.txt")
            let competingFile = root.appendingPathComponent("competing.txt")
            let worker = try SyncWorker(
                configuration: DriveConfiguration(server: server, folder: folder), client: client,
                stateDirectory: root.appendingPathComponent("state")
            )
            worker.control.setRunning(true)
            try Data("baseline".utf8).write(to: file)
            try worker.sync { _ in }
            let baseline = try XCTUnwrap(client.listFiles(path: "", cursor: nil, limit: nil).items.first)
            try Data("local edit".utf8).write(to: file)
            try Data("server edit".utf8).write(to: competingFile)
            var raced = false
            try worker.sync { update in
                guard case .transfer(let transfer) = update, transfer.path == "report.txt", !raced else { return }
                raced = true
                let overwritten = DispatchSemaphore(value: 0)
                DispatchQueue.global().async {
                    defer { overwritten.signal() }
                    do {
                        _ = try competingClient.uploadFile(
                            remotePath: "report.txt", localFilePath: competingFile.path, ifMatch: baseline.etag
                        )
                    } catch {
                        XCTFail("Could not make concurrent server edit: \(error)")
                    }
                }
                XCTAssertEqual(overwritten.wait(timeout: .now() + 5), .success)
            }
            XCTAssertTrue(raced)
            let sibling = try XCTUnwrap(FileManager.default.contentsOfDirectory(atPath: folder.path).first {
                $0.hasPrefix("report (conflict ") && $0.hasSuffix(").txt")
            })
            XCTAssertEqual(try Data(contentsOf: folder.appendingPathComponent(sibling)), Data("local edit".utf8))

            try worker.sync { _ in }
            XCTAssertEqual(try Data(contentsOf: file), Data("server edit".utf8))
            let remote = try client.listFiles(path: "", cursor: nil, limit: nil).items
            XCTAssertEqual(Set(remote.map(\.path)), ["report.txt", sibling])
            XCTAssertEqual(
                remote.first { $0.path == sibling }?.sha256,
                try hashFileSha256(localPath: folder.appendingPathComponent(sibling).path)
            )
        }
    }

    func testCancellingMultipartUploadReleasesTheSession() throws {
        try withTestServer { server, root in
            let client = try makeClient(server)
            let file = root.appendingPathComponent("upload.bin")
            let remote = "upload.bin"
            try Data(repeating: 1, count: 9 * 1024 * 1024).write(to: file)
            XCTAssertThrowsError(try client.uploadWithProgress(
                remotePath: remote, localFilePath: file.path, ifMatch: nil, observer: CancelAfterFirstChunk()
            )) { error in
                guard case FfiError.Cancelled = error else {
                    return XCTFail("Expected a cancelled transfer, got \(error)")
                }
            }
            XCTAssertEqual(try client.listFiles(path: "", cursor: nil, limit: nil).items, [])
            let newSize = 10 * 1024 * 1024
            try Data(repeating: 2, count: newSize).write(to: file)
            let retry = try client.uploadFile(remotePath: remote, localFilePath: file.path, ifMatch: nil)
            XCTAssertEqual(retry.item.sizeBytes, UInt64(newSize))
        }
    }

    private func makeClient(_ server: URL) throws -> TailDriveFfiClient {
        try TailDriveFfiClient(baseUrl: server.absoluteString, bearerToken: "phase4a-test-token")
    }

    private func withTemporaryDirectory(_ body: (URL) throws -> Void) throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: root) }
        try body(root)
    }

    private func withTestServer(_ run: (URL, URL) throws -> Void) throws {
        try withTemporaryDirectory { root in
            let repository = URL(fileURLWithPath: #filePath)
                .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
                .deletingLastPathComponent().deletingLastPathComponent()
            let candidate = repository.appendingPathComponent("target/debug/taildrive-server")
            let executable = try XCTUnwrap(
                FileManager.default.isExecutableFile(atPath: candidate.path) ? candidate : nil,
                "Build the loopback test server with cargo test --workspace or cargo build -p taildrive-server."
            )

            let log = root.appendingPathComponent("server.log")
            try Data().write(to: log)
            let handle = try FileHandle(forWritingTo: log)
            defer { try? handle.close() }
            let server = Process()
            server.executableURL = executable
            server.currentDirectoryURL = root
            server.environment = [
                "TAILDRIVE_BIND": "127.0.0.1:0", "TAILDRIVE_DATA_DIR": root.appendingPathComponent("data").path,
                "TAILDRIVE_API_TOKENS": "phase4a-test-token", "TAILDRIVE_FREE_FLOOR_BYTES": "0",
                "TAILDRIVE_STORAGE_DRIVER": "local-fs", "RUST_LOG": "info", "NO_COLOR": "1",
            ]
            server.standardOutput = handle
            server.standardError = handle
            try server.run()
            defer {
                if server.isRunning { server.terminate() }
                server.waitUntilExit()
            }
            let deadline = ProcessInfo.processInfo.systemUptime + 5
            while server.isRunning && ProcessInfo.processInfo.systemUptime < deadline {
                let contents = try String(contentsOf: log, encoding: .utf8)
                if let address = contents.range(of: #"127\.0\.0\.1:[0-9]+"#, options: .regularExpression),
                   let baseURL = URL(string: "http://" + contents[address]) {
                    try run(baseURL, root)
                    return
                }
                Thread.sleep(forTimeInterval: 0.025)
            }
            XCTFail("Loopback TailDrive server failed to start: \(try String(contentsOf: log, encoding: .utf8))")
        }
    }
}

private final class CancelAfterFirstChunk: TransferObserver {
    func progress(bytes: UInt64, total: UInt64) -> Bool { bytes == 0 }
}
