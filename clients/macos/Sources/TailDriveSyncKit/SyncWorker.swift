import Foundation
import CryptoKit
import TailDriveFFI

struct Checkpoint: Codable {
    struct Version: Codable {
        let sha256: String
        let etag: String?
        let version: UInt64
    }

    var files: [String: Version] = [:]
    var recent: [String] = []

    mutating func record(_ item: FileItem) {
        if let hash = item.sha256 {
            files[item.path] = Version(sha256: hash, etag: item.etag, version: item.version)
        }
    }

    mutating func touch(_ path: String) {
        recent.removeAll { $0 == path }
        recent.insert(path, at: 0)
        recent = Array(recent.prefix(5))
    }
}

enum SyncUpdate {
    case health(HealthResponse)
    case quota(QuotaInfo)
    case recent([String])
    case transfer(Transfer)
    case finished(String)
    case blocked(String, Error)
    case rescan
}

final class SyncControl: @unchecked Sendable {
    private let lock = NSLock()
    private var running = false
    private var blocked: Set<String> = []

    func setRunning(_ enabled: Bool) { lock.withLock { running = enabled } }

    func permits(_ path: String? = nil) -> Bool {
        lock.withLock { running && path.map { !blocked.contains($0) } != false }
    }

    func block(_ path: String) { _ = lock.withLock { blocked.insert(path) } }
    func unblock(_ path: String) { _ = lock.withLock { blocked.remove(path) } }
}

private final class ProgressReporter: TransferObserver, @unchecked Sendable {
    let path: String
    let control: SyncControl
    let deliver: (SyncUpdate) -> Void
    let start = ProcessInfo.processInfo.systemUptime
    private var lastReport: TimeInterval = -.infinity

    init(path: String, control: SyncControl, deliver: @escaping (SyncUpdate) -> Void) {
        self.path = path
        self.control = control
        self.deliver = deliver
    }

    func progress(bytes: UInt64, total: UInt64) -> Bool {
        guard control.permits(path) else { return false }
        let now = ProcessInfo.processInfo.systemUptime
        if now - lastReport >= 0.15 || bytes == total {
            lastReport = now
            let speed = Double(bytes) / max(now - start, 0.001)
            deliver(.transfer(Transfer(
                path: path, progress: total == 0 ? nil : Double(bytes) / Double(total),
                bytesPerSecond: speed, state: .active
            )))
        }
        return true
    }
}

// SyncModel runs health checks and reconciliation on the same serial queue.
final class SyncWorker: @unchecked Sendable {
    private let client: TailDriveFfiClient
    private var mirror: LocalMirror
    private let stateFile: URL
    private var checkpoint: Checkpoint
    let control = SyncControl()

    init(configuration: DriveConfiguration, client: TailDriveFfiClient, stateDirectory: URL? = nil) throws {
        let files = FileManager.default
        let folder = configuration.folder.standardizedFileURL
        try files.createDirectory(at: folder, withIntermediateDirectories: true)
        mirror = LocalMirror(root: folder.resolvingSymlinksInPath())
        self.client = client
        let state = try stateDirectory ?? files.url(
            for: .applicationSupportDirectory, in: .userDomainMask, appropriateFor: nil, create: true
        ).appendingPathComponent("TailDrive", isDirectory: true)
        let identity = configuration.server.absoluteString + "\n" + mirror.root.path
        let digest = SHA256.hash(data: Data(identity.utf8)).map { String(format: "%02x", $0) }.joined()
        stateFile = state.appendingPathComponent(digest + ".json")
        try files.createDirectory(at: state, withIntermediateDirectories: true)
        if files.fileExists(atPath: stateFile.path) {
            checkpoint = try JSONDecoder().decode(Checkpoint.self, from: Data(contentsOf: stateFile))
        } else {
            checkpoint = Checkpoint()
        }
    }

    func checkHealth(deliver: (SyncUpdate) -> Void) throws {
        deliver(.health(try client.health()))
        deliver(.recent(checkpoint.recent))
    }

    func sync(deliver: @escaping (SyncUpdate) -> Void) throws {
        let health = try client.health()
        deliver(.health(health))
        deliver(.recent(checkpoint.recent))
        guard control.permits() else { return }
        var quota = health.quota
        let remote = try remoteSnapshot()
        let local = try mirror.scan()

        let localEntries = local.map { path, entry in
            SyncEntry(
                path: path, kind: entry.kind, sha256: entry.sha256,
                lastSyncedSha256: checkpoint.files[path]?.sha256
            )
        }
        let remoteEntries = remote.values.map { item in
            SyncEntry(path: item.path, kind: item.kind, sha256: item.sha256, lastSyncedSha256: nil)
        }
        let actions = try TailDriveFFI.planFolderSync(localEntries: localEntries, remoteEntries: remoteEntries)
        let planned = Set(actions.map(\.path))
        let current = Set(local.keys).union(remote.keys)
        checkpoint.files = checkpoint.files.filter { current.contains($0.key) }
        checkpoint.recent = checkpoint.recent.filter { local[$0] != nil }
        for (path, item) in remote where item.sha256 != nil && local[path]?.sha256 == item.sha256 {
            checkpoint.record(item)
        }
        try save()
        deliver(.recent(checkpoint.recent))

        var unavailable: Set<String> = []
        for action in actions {
            let path = action.path
            guard control.permits() else { return }
            guard control.permits(path), !hasAncestor(path, in: unavailable) else {
                unavailable.insert(path)
                continue
            }
            do {
                try apply(action, local: local[path], remote: remote, quota: &quota, deliver: deliver)
                try save()
                deliver(.finished(path))
                deliver(.recent(checkpoint.recent))
            } catch SyncError.changed {
                unavailable.insert(path)
                deliver(.finished(path))
            } catch FfiError.Cancelled {
                unavailable.insert(path)
            } catch {
                unavailable.insert(path)
                deliver(.blocked(path, error))
                if let ffi = error as? FfiError, case .Transport = ffi { throw ffi }
            }
        }
        for path in current.subtracting(planned) { deliver(.finished(path)) }
    }

    private func remoteSnapshot() throws -> [String: FileItem] {
        var items: [String: FileItem] = [:]
        var folders = [""]
        while let folder = folders.popLast() {
            guard control.permits() else { throw FfiError.Cancelled }
            var cursor: String?
            repeat {
                let page = try client.listFiles(path: folder, cursor: cursor, limit: 100)
                for item in page.items where item.name != ".DS_Store" {
                    items[item.path] = item
                    if item.kind == .folder { folders.append(item.path) }
                }
                cursor = page.nextCursor
            } while cursor != nil
        }
        return items
    }

    private func apply(
        _ action: SyncAction, local: LocalEntry?, remote: [String: FileItem],
        quota: inout QuotaInfo, deliver: @escaping (SyncUpdate) -> Void
    ) throws {
        let path = action.path
        let target = try mirror.destination(for: path)
        switch action {
        case .createRemoteFolder:
            guard let local else { throw SyncError.changed(path) }
            try mirror.validate(target, expected: local, path: path)
            _ = try client.createFolder(path: path)
        case .createLocalFolder:
            try mirror.validate(target, expected: nil, path: path)
            try FileManager.default.createDirectory(at: target, withIntermediateDirectories: false)
        case .upload:
            guard let local else { throw SyncError.changed(path) }
            try mirror.validate(target, expected: local, path: path)
            let current = remote[path]
            let etag = current?.etag
            if current != nil && etag == nil { throw SyncError.message("Missing server version for \(path).") }
            if current == nil, try remoteItem(path) != nil { throw SyncError.changed(path) }
            let size = current?.sizeBytes ?? 0
            if local.size > size && local.size - size > quota.remainingBytes {
                throw SyncError.message("Storage limit reached.")
            }
            let observer = ProgressReporter(path: path, control: control, deliver: deliver)
            do {
                let result = try client.uploadWithProgress(
                    remotePath: path, localFilePath: target.path, ifMatch: etag, observer: observer
                )
                quota = result.quota
                deliver(.quota(quota))
                checkpoint.record(result.item)
                checkpoint.touch(path)
            } catch let error as FfiError {
                guard case .Http(409, "ERR_CONFLICT", _, _) = error, let etag,
                      try remoteItem(path)?.etag != etag else { throw error }
                try saveConflict(path: path, local: local, remote: remote, deliver: deliver)
            }
        case .download:
            guard let item = remote[path], let hash = item.sha256 else { throw SyncError.changed(path) }
            let files = FileManager.default
            let work = try files.url(
                for: .itemReplacementDirectory, in: .userDomainMask,
                appropriateFor: target.deletingLastPathComponent(), create: true
            )
            defer { try? files.removeItem(at: work) }
            let staged = work.appendingPathComponent("download")
            let observer = ProgressReporter(path: path, control: control, deliver: deliver)
            try client.downloadWithProgress(remotePath: path, destLocalPath: staged.path, observer: observer)
            guard control.permits(path) else { throw FfiError.Cancelled }
            guard try TailDriveFFI.hashFileSha256(localPath: staged.path) == hash else {
                throw SyncError.changed(path)
            }
            try mirror.install(staged, expected: local, path: path)
            checkpoint.record(item)
            checkpoint.touch(path)
        case .conflict:
            guard let local else { throw SyncError.changed(path) }
            try saveConflict(path: path, local: local, remote: remote, deliver: deliver)
        }
    }

    private func saveConflict(
        path: String, local: LocalEntry, remote: [String: FileItem], deliver: (SyncUpdate) -> Void
    ) throws {
        guard control.permits(path) else { throw FfiError.Cancelled }
        let sibling = try mirror.preserveConflict(
            path: path, expected: local, occupiedRemotePaths: Set(remote.keys)
        )
        if local.kind == .file { checkpoint.touch(sibling) }
        deliver(.rescan)
    }

    private func remoteItem(_ path: String) throws -> FileItem? {
        let folder = path.lastIndex(of: "/").map { String(path[..<$0]) } ?? ""
        var cursor: String?
        repeat {
            let page = try client.listFiles(path: folder, cursor: cursor, limit: 100)
            if let item = page.items.first(where: { $0.path == path }) { return item }
            cursor = page.nextCursor
        } while cursor != nil
        return nil
    }

    private func save() throws {
        try JSONEncoder().encode(checkpoint).write(to: stateFile, options: .atomic)
    }

    private func hasAncestor(_ path: String, in paths: Set<String>) -> Bool {
        path.indices.contains { path[$0] == "/" && paths.contains(String(path[..<$0])) }
    }
}

private extension SyncAction {
    var path: String {
        switch self {
        case .upload(let path), .download(let path), .createRemoteFolder(let path),
             .createLocalFolder(let path), .conflict(let path):
            return path
        }
    }
}
