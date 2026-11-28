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
        func entry(_ path: String, _ hash: String) -> SyncEntry {
            SyncEntry(path: path, kind: .file, sha256: hash)
        }
        let a = String(repeating: "a", count: 64)
        let b = String(repeating: "b", count: 64)
        let c = String(repeating: "c", count: 64)
        let local = [
            entry("conflict", a), entry("same", a),
            entry("folder/new", a), entry("local-change", a),
            entry("remote-change", b),
            SyncEntry(path: "folder", kind: .folder, sha256: nil),
        ]
        let remote = [
            entry("remote-change", a), entry("same", a), entry("conflict", b),
            entry("local-change", b), entry("remote-only", a),
        ]
        let baseline = [
            entry("conflict", c), entry("local-change", b), entry("remote-change", b),
        ]
        XCTAssertEqual(try planFolderSync(localEntries: local, remoteEntries: remote, baselineEntries: baseline), [
            .conflict(path: "conflict"), .createRemoteFolder(path: "folder"),
            .upload(path: "folder/new"), .upload(path: "local-change"),
            .download(path: "remote-change"), .download(path: "remote-only"),
        ])
        XCTAssertEqual(
            try planFolderSync(localEntries: local, remoteEntries: remote + [entry("folder", b)], baselineEntries: baseline).filter {
                if case .upload(path: "folder/new") = $0 { return true }
                return false
            },
            []
        )
        XCTAssertThrowsError(try planFolderSync(
            localEntries: [entry("café", a), entry("cafe\u{301}", b)], remoteEntries: [], baselineEntries: []
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


    func testLocalNonEmptyFolderDeletePropagatesAndDoesNotResurrect() throws {
        try withTestServer { server, root in
            let files = FileManager.default
            let folderA = root.appendingPathComponent("client-a", isDirectory: true)
            let stateA = root.appendingPathComponent("state-a", isDirectory: true)
            let folderB = root.appendingPathComponent("client-b", isDirectory: true)
            let stateB = root.appendingPathComponent("state-b", isDirectory: true)

            let clientA = try makeClient(server)
            let workerA = try SyncWorker(
                configuration: DriveConfiguration(server: server, folder: folderA),
                client: clientA, stateDirectory: stateA
            )
            workerA.control.setRunning(true)

            let clientB = try makeClient(server)
            let workerB = try SyncWorker(
                configuration: DriveConfiguration(server: server, folder: folderB),
                client: clientB, stateDirectory: stateB
            )
            workerB.control.setRunning(true)

            // Seed non-empty folder, nested folder, empty folder, and root file on Client A.
            let aiNotesA = folderA.appendingPathComponent("AI Notes", isDirectory: true)
            let subA = aiNotesA.appendingPathComponent("sub", isDirectory: true)
            let emptyA = folderA.appendingPathComponent("EmptyFolder", isDirectory: true)
            try files.createDirectory(at: subA, withIntermediateDirectories: true)
            try files.createDirectory(at: emptyA, withIntermediateDirectories: true)
            try Data("note 1".utf8).write(to: aiNotesA.appendingPathComponent("note1.md"))
            try Data("note 2".utf8).write(to: subA.appendingPathComponent("note2.md"))
            try Data("root note".utf8).write(to: folderA.appendingPathComponent("root.md"))

            // Sync A -> server -> B.
            try workerA.sync { _ in }
            try workerB.sync { _ in }

            let aiNotesB = folderB.appendingPathComponent("AI Notes", isDirectory: true)
            let emptyB = folderB.appendingPathComponent("EmptyFolder", isDirectory: true)
            let rootB = folderB.appendingPathComponent("root.md")
            XCTAssertTrue(files.fileExists(atPath: aiNotesB.appendingPathComponent("sub/note2.md").path))
            XCTAssertTrue(files.fileExists(atPath: emptyB.path))
            XCTAssertTrue(files.fileExists(atPath: rootB.path))

            // Add a .DS_Store inside Client B's AI Notes folder to ensure ignored metadata does not block folder deletion.
            try Data("finder-meta".utf8).write(to: aiNotesB.appendingPathComponent(".DS_Store"))

            // Delete non-empty folder, empty folder, and root file on Client A (simulate Trash move / delete).
            try files.removeItem(at: aiNotesA)
            try files.removeItem(at: emptyA)
            try files.removeItem(at: folderA.appendingPathComponent("root.md"))

            // Sync A twice to verify deletes propagate and NEVER resurrect.
            try workerA.sync { _ in }
            try workerA.sync { _ in }

            XCTAssertFalse(files.fileExists(atPath: aiNotesA.path))
            XCTAssertFalse(files.fileExists(atPath: emptyA.path))
            XCTAssertFalse(files.fileExists(atPath: folderA.appendingPathComponent("root.md").path))
            XCTAssertEqual(try clientA.listFiles(path: "", cursor: nil, limit: nil).items, [])

            // Sync B twice to verify remote deletes propagate locally and do not re-upload.
            try workerB.sync { _ in }
            try workerB.sync { _ in }

            XCTAssertFalse(files.fileExists(atPath: aiNotesB.path))
            XCTAssertFalse(files.fileExists(atPath: emptyB.path))
            XCTAssertFalse(files.fileExists(atPath: rootB.path))
            XCTAssertEqual(try clientA.listFiles(path: "", cursor: nil, limit: nil).items, [])

            // Recreating a previously deleted folder + file syncs cleanly again.
            try files.createDirectory(at: aiNotesA, withIntermediateDirectories: true)
            try Data("fresh".utf8).write(to: aiNotesA.appendingPathComponent("fresh.md"))
            try workerA.sync { _ in }
            try workerB.sync { _ in }
            XCTAssertEqual(
                try Data(contentsOf: aiNotesB.appendingPathComponent("fresh.md")),
                Data("fresh".utf8)
            )
        }
    }

    func testDeleteVsConcurrentRemoteModifyAndAddPreservesRemoteChanges() throws {
        try withTestServer { server, root in
            let files = FileManager.default
            let folderA = root.appendingPathComponent("client-a", isDirectory: true)
            let stateA = root.appendingPathComponent("state-a", isDirectory: true)
            let folderB = root.appendingPathComponent("client-b", isDirectory: true)
            let stateB = root.appendingPathComponent("state-b", isDirectory: true)

            let workerA = try SyncWorker(
                configuration: DriveConfiguration(server: server, folder: folderA),
                client: try makeClient(server), stateDirectory: stateA
            )
            workerA.control.setRunning(true)

            let workerB = try SyncWorker(
                configuration: DriveConfiguration(server: server, folder: folderB),
                client: try makeClient(server), stateDirectory: stateB
            )
            workerB.control.setRunning(true)

            let dirA = folderA.appendingPathComponent("AI Notes", isDirectory: true)
            try files.createDirectory(at: dirA, withIntermediateDirectories: true)
            try Data("v1".utf8).write(to: dirA.appendingPathComponent("edited.md"))
            try Data("untouched".utf8).write(to: dirA.appendingPathComponent("old.md"))

            try workerA.sync { _ in }
            try workerB.sync { _ in }

            // Client B edits edited.md and adds new.md, then syncs to server.
            let dirB = folderB.appendingPathComponent("AI Notes", isDirectory: true)
            try Data("v2 from B".utf8).write(to: dirB.appendingPathComponent("edited.md"))
            try Data("added on B".utf8).write(to: dirB.appendingPathComponent("new.md"))
            try workerB.sync { _ in }

            // Client A deletes the entire AI Notes folder locally before seeing B's changes.
            try files.removeItem(at: dirA)
            try workerA.sync { _ in }
            try workerB.sync { _ in }

            // Remote edit and addition are preserved on both clients; untouched old.md is deleted everywhere.
            XCTAssertEqual(try Data(contentsOf: dirA.appendingPathComponent("edited.md")), Data("v2 from B".utf8))
            XCTAssertEqual(try Data(contentsOf: dirA.appendingPathComponent("new.md")), Data("added on B".utf8))
            XCTAssertFalse(files.fileExists(atPath: dirA.appendingPathComponent("old.md").path))
            XCTAssertFalse(files.fileExists(atPath: dirB.appendingPathComponent("old.md").path))
        }
    }

    func testLegacyCheckpointUpgradeInfersFoldersAndAvoidsSpuriousDeletes() throws {
        try withTestServer { server, root in
            let files = FileManager.default
            let folder = root.appendingPathComponent("client", isDirectory: true)
            let state = root.appendingPathComponent("state", isDirectory: true)
            let client = try makeClient(server)

            // Upload two folders on remote: SyncedDir/a.md and UntrackedDir/b.md.
            _ = try client.createFolder(path: "SyncedDir")
            let tmpA = root.appendingPathComponent("a.md")
            try Data("hello".utf8).write(to: tmpA)
            let uploadedA = try client.uploadFile(remotePath: "SyncedDir/a.md", localFilePath: tmpA.path, ifMatch: nil)

            _ = try client.createFolder(path: "UntrackedDir")
            let tmpB = root.appendingPathComponent("b.md")
            try Data("world".utf8).write(to: tmpB)
            _ = try client.uploadFile(remotePath: "UntrackedDir/b.md", localFilePath: tmpB.path, ifMatch: nil)

            // Create local folder and write a legacy Checkpoint JSON (no "folders" field) that only knew about SyncedDir/a.md.
            try files.createDirectory(at: folder, withIntermediateDirectories: true)
            try files.createDirectory(at: state, withIntermediateDirectories: true)
            let legacyPayload: [String: Any] = [
                "files": [
                    "SyncedDir/a.md": [
                        "sha256": uploadedA.item.sha256!,
                        "etag": uploadedA.item.etag!,
                        "version": uploadedA.item.version,
                    ]
                ],
                "recent": [],
            ]
            let legacyData = try JSONSerialization.data(withJSONObject: legacyPayload)
            let decoded = try JSONDecoder().decode(Checkpoint.self, from: legacyData)
            XCTAssertEqual(decoded.folders, ["SyncedDir"])

            let syncedDir = folder.appendingPathComponent("SyncedDir", isDirectory: true)
            try files.createDirectory(at: syncedDir, withIntermediateDirectories: true)
            try Data("hello".utf8).write(to: syncedDir.appendingPathComponent("a.md"))

            let workerInit = try SyncWorker(
                configuration: DriveConfiguration(server: server, folder: folder),
                client: client, stateDirectory: state
            )
            workerInit.control.setRunning(true)
            try workerInit.sync { _ in }

            // Rewrite checkpoint to legacy format (no "folders" key, only SyncedDir/a.md) and drop UntrackedDir locally.
            try files.removeItem(at: folder.appendingPathComponent("UntrackedDir"))
            let checkpointURL = try XCTUnwrap(
                try files.contentsOfDirectory(at: state, includingPropertiesForKeys: nil).first { $0.pathExtension == "json" }
            )
            try legacyData.write(to: checkpointURL)

            // User deletes SyncedDir locally right after upgrading.
            try files.removeItem(at: syncedDir)

            let upgradedWorker = try SyncWorker(
                configuration: DriveConfiguration(server: server, folder: folder),
                client: client, stateDirectory: state
            )
            upgradedWorker.control.setRunning(true)
            try upgradedWorker.sync { _ in }

            // SyncedDir (and a.md) were deleted remotely; UntrackedDir (and b.md) were downloaded, NOT deleted!
            XCTAssertFalse(files.fileExists(atPath: syncedDir.path))
            XCTAssertEqual(
                try Data(contentsOf: folder.appendingPathComponent("UntrackedDir/b.md")),
                Data("world".utf8)
            )
            let remoteRoot = try client.listFiles(path: "", cursor: nil, limit: nil).items.map(\.path)
            XCTAssertEqual(remoteRoot, ["UntrackedDir"])
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
