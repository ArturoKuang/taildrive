import Foundation
import TailDriveFFI

struct LocalEntry {
    let kind: FileType
    let size: UInt64
    let sha256: String?
}

struct LocalMirror {
    private struct FileStamp: Equatable {
        let device: dev_t
        let inode: ino_t
        let size: off_t
        let modificationSeconds: Int
        let modificationNanoseconds: Int
        let changeSeconds: Int
        let changeNanoseconds: Int

        init(_ status: stat) {
            device = status.st_dev
            inode = status.st_ino
            size = status.st_size
            modificationSeconds = status.st_mtimespec.tv_sec
            modificationNanoseconds = status.st_mtimespec.tv_nsec
            changeSeconds = status.st_ctimespec.tv_sec
            changeNanoseconds = status.st_ctimespec.tv_nsec
        }
    }

    let root: URL
    private let files = FileManager.default
    private var hashes: [String: (FileStamp, String)] = [:]

    init(root: URL) { self.root = root }

    mutating func scan() throws -> [String: LocalEntry] {
        var entries: [String: LocalEntry] = [:]
        var nextHashes: [String: (FileStamp, String)] = [:]
        var pending: [(String, URL)] = [("", root)]
        while let (parent, directory) = pending.popLast() {
            try requireDirectory(directory)
            for child in try files.contentsOfDirectory(at: directory, includingPropertiesForKeys: []) {
                let name = child.lastPathComponent
                if name == ".DS_Store" { continue }

                var status = stat()
                if lstat(child.path, &status) != 0 {
                    if errno == ENOENT { continue }
                    throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
                }
                let type = status.st_mode & S_IFMT
                guard type == S_IFDIR || type == S_IFREG else { continue }

                let path = try TailDriveFFI.normalizeDrivePath(raw: parent.isEmpty ? name : "\(parent)/\(name)")
                if entries[path] != nil {
                    throw SyncError.message("Multiple local items map to \(path).")
                }
                if type == S_IFDIR {
                    entries[path] = LocalEntry(kind: .folder, size: 0, sha256: nil)
                    pending.append((path, child))
                } else {
                    let stamp = FileStamp(status)
                    let hash: String
                    if let cached = hashes[path], cached.0 == stamp {
                        hash = cached.1
                    } else {
                        hash = try TailDriveFFI.hashFileSha256(localPath: child.path)
                    }
                    nextHashes[path] = (stamp, hash)
                    entries[path] = LocalEntry(kind: .file, size: UInt64(stamp.size), sha256: hash)
                }
            }
        }
        hashes = nextHashes
        return entries
    }

    func destination(for path: String) throws -> URL {
        try requireDirectory(root)
        var url = root
        for segment in path.split(separator: "/") {
            url.appendPathComponent(String(segment))
            if (try? files.attributesOfItem(atPath: url.path)[.type] as? FileAttributeType) == .typeSymbolicLink {
                throw SyncError.message("Sync cannot follow the symbolic link at \(url.path).")
            }
        }
        return url
    }

    private func requireDirectory(_ url: URL) throws {
        let type = try files.attributesOfItem(atPath: url.path)[.type] as? FileAttributeType
        guard type == .typeDirectory else {
            throw SyncError.message("Sync folder is no longer a directory at \(url.path).")
        }
    }

    func validate(_ url: URL, expected: LocalEntry?, path: String) throws {
        let type = try? files.attributesOfItem(atPath: url.path)[.type] as? FileAttributeType
        guard let expected else {
            if type != nil { throw SyncError.changed(path) }
            return
        }
        switch expected.kind {
        case .folder:
            guard type == .typeDirectory else { throw SyncError.changed(path) }
        case .file:
            guard type == .typeRegular,
                  try TailDriveFFI.hashFileSha256(localPath: url.path) == expected.sha256 else {
                throw SyncError.changed(path)
            }
        }
    }

    func preserveConflict(
        path: String, expected: LocalEntry, occupiedRemotePaths: Set<String>
    ) throws -> String {
        let original = try destination(for: path)
        try validate(original, expected: expected, path: path)
        let host = Host.current().localizedName ?? ProcessInfo.processInfo.hostName
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        let timestamp = formatter.string(from: Date()).replacingOccurrences(of: ":", with: "")
        for attempt in 0..<100 {
            let tag = attempt == 0 ? timestamp : "\(timestamp)-\(attempt)"
            let sibling = try TailDriveFFI.formatConflictPath(path: path, host: host, ts: tag)
            let url = try destination(for: sibling)
            if occupiedRemotePaths.contains(sibling) { continue }
            if (try? files.attributesOfItem(atPath: url.path)) != nil { continue }
            try files.moveItem(at: original, to: url)
            return sibling
        }
        throw SyncError.message("Could not find an available conflict name for \(path).")
    }

    func install(_ staged: URL, expected: LocalEntry?, path: String) throws {
        let destination = try destination(for: path)
        try validate(destination, expected: expected, path: path)
        if expected == nil {
            try files.moveItem(at: staged, to: destination)
        } else if rename(staged.path, destination.path) != 0 {
            throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
        }
    }
}
