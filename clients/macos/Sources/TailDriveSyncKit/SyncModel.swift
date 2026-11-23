import Combine
import Foundation
import Network
import TailDriveFFI

public enum ConnectionState: Equatable {
    case setup
    case checking
    case online
    case offline
    case unreachable
    case unauthorized
    case error(String)
}

public struct Transfer: Identifiable {
    public enum State {
        case active
        case retrying(Int)
        case failed(String)
        case cancelled
    }

    public var id: String { path }
    public let path: String
    public let progress: Double?
    public let bytesPerSecond: Double
    public let state: State
}

@MainActor
public final class SyncModel: ObservableObject {
    @Published public private(set) var configuration: DriveConfiguration?
    @Published public private(set) var connection = ConnectionState.setup
    @Published public private(set) var paused = false
    @Published public private(set) var quota: QuotaInfo?
    @Published public private(set) var node = "macmini-ci"
    @Published public private(set) var transfers: [Transfer] = []
    @Published public private(set) var recent: [String] = []
    @Published public private(set) var syncError: String?
    @Published public private(set) var saving = false

    private let store = SettingsStore()
    private let queue = DispatchQueue(label: "com.taildrive.macos.sync", qos: .utility)
    private let network = NWPathMonitor()
    private var networkAvailable = true
    private var worker: SyncWorker?
    private var watcher: FolderWatcher?
    private var timer: Timer?
    private var scanDate = Date.distantPast
    private var scanning = false
    private var attempts: [String: Int] = [:]
    private var retryDates: [String: Date] = [:]

    public init() {
        configuration = store.configuration
        paused = store.paused
        if let configuration {
            do {
                try start(configuration, token: store.token(for: configuration.server))
            } catch {
                connection = .error(Self.message(for: error))
            }
        }

        network.pathUpdateHandler = { [weak self] path in
            DispatchQueue.main.async { [weak self] in
                self?.networkDidChange(available: path.status == .satisfied)
            }
        }
        network.start(queue: DispatchQueue(label: "com.taildrive.macos.network"))
        let timer = Timer(timeInterval: 1, repeats: true) { [weak self] _ in
            DispatchQueue.main.async { [weak self] in self?.tick() }
        }
        RunLoop.main.add(timer, forMode: .common)
        self.timer = timer
    }

    deinit {
        timer?.invalidate()
        network.cancel()
        worker?.control.setRunning(false)
    }

    public var folder: URL { configuration?.folder ?? DriveConfiguration.defaultFolder }

    public func token() throws -> String {
        guard let configuration else { return "" }
        return try store.token(for: configuration.server)
    }

    public func saveSettings(
        serverAddress: String, token: String, folder: URL, completion: @escaping (String?) -> Void
    ) {
        guard !saving else { return }
        do {
            let configuration = DriveConfiguration(
                server: try DriveConfiguration.serverURL(serverAddress), folder: folder
            )
            let token = token.trimmingCharacters(in: .whitespacesAndNewlines)
            saving = true
            worker?.control.setRunning(false)
            queue.async { [weak self] in
                let result = Result {
                    let client = try Self.makeClient(configuration, token: token)
                    let health = try client.health()
                    let worker = try SyncWorker(configuration: configuration, client: client)
                    try SettingsStore().save(configuration, token: token)
                    return (worker, health)
                }
                DispatchQueue.main.async { [weak self] in
                    guard let self else { return }
                    saving = false
                    switch result {
                    case .success(let (worker, health)):
                        install(worker, configuration: configuration)
                        receive(.health(health), from: worker)
                        completion(nil)
                        tick()
                    case .failure(let error):
                        self.worker?.control.setRunning(!paused)
                        completion(Self.message(for: error))
                        syncNow()
                    }
                }
            }
        } catch {
            completion(Self.message(for: error))
        }
    }

    public func setPaused(_ enabled: Bool) {
        paused = enabled
        store.paused = enabled
        worker?.control.setRunning(!enabled)
        if enabled {
            transfers.removeAll {
                if case .active = $0.state { return true }
                return false
            }
        }
        syncNow()
    }

    public func syncNow() {
        scanDate = .distantPast
        tick()
    }

    public func retry(_ path: String) {
        attempts.removeValue(forKey: path)
        retryDates.removeValue(forKey: path)
        worker?.control.unblock(path)
        removeTransfer(path)
        syncNow()
    }

    public func cancel(_ path: String) {
        worker?.control.block(path)
        attempts.removeValue(forKey: path)
        retryDates.removeValue(forKey: path)
        updateTransfer(Transfer(path: path, progress: nil, bytesPerSecond: 0, state: .cancelled))
    }

    public func revealURL(for path: String) -> URL {
        folder.appendingPathComponent(path)
    }

    private func start(_ configuration: DriveConfiguration, token: String) throws {
        let client = try Self.makeClient(configuration, token: token)
        install(try SyncWorker(configuration: configuration, client: client), configuration: configuration)
    }

    nonisolated private static func makeClient(
        _ configuration: DriveConfiguration, token: String
    ) throws -> TailDriveFfiClient {
        try TailDriveFfiClient(baseUrl: configuration.server.absoluteString, bearerToken: token.isEmpty ? nil : token)
    }

    private func install(_ next: SyncWorker, configuration: DriveConfiguration) {
        worker?.control.setRunning(false)
        worker = next
        next.control.setRunning(!paused)
        self.configuration = configuration
        syncError = nil
        quota = nil
        transfers = []
        recent = []
        attempts = [:]
        retryDates = [:]
        connection = .checking
        scanDate = .distantPast
        watcher = FolderWatcher(folder: configuration.folder) { [weak self] in
            Task { @MainActor in
                guard let self, !self.paused else { return }
                self.scanDate = min(self.scanDate, Date(timeIntervalSinceNow: 0.5))
            }
        }
    }

    private func tick() {
        guard !saving, !scanning, let worker, Date() >= scanDate else { return }
        let now = Date()
        for (path, _) in retryDates.filter({ $0.value <= now }) {
            worker.control.unblock(path)
            retryDates.removeValue(forKey: path)
        }
        scanDate = min(retryDates.values.min() ?? .distantFuture, Date(timeIntervalSinceNow: 15))
        scanning = true
        let checkOnly = paused
        queue.async { [weak self] in
            let deliver: (SyncUpdate) -> Void = { [weak self] update in
                DispatchQueue.main.async { [weak self] in self?.receive(update, from: worker) }
            }
            let run = Result {
                if checkOnly {
                    try worker.checkHealth(deliver: deliver)
                } else {
                    try worker.sync(deliver: deliver)
                }
            }
            DispatchQueue.main.async { [weak self] in
                guard let self else { return }
                scanning = false
                if self.worker === worker {
                    switch run {
                    case .success: syncError = nil
                    case .failure(let error): record(error)
                    }
                }
                tick()
            }
        }
    }

    private func receive(_ update: SyncUpdate, from sender: SyncWorker) {
        guard sender === worker else { return }
        switch update {
        case .health(let health):
            connection = .online
            node = health.node
            quota = health.quota
        case .quota(let value):
            quota = value
        case .recent(let paths):
            recent = paths
        case .transfer(let transfer):
            if sender.control.permits(transfer.path) { updateTransfer(transfer) }
        case .finished(let path):
            sender.control.unblock(path)
            attempts.removeValue(forKey: path)
            retryDates.removeValue(forKey: path)
            removeTransfer(path)
        case .blocked(let path, let error):
            guard sender.control.permits(path) else { return }
            let attempts = self.attempts[path, default: 0]
            let delays: [TimeInterval] = [1, 2, 4]
            let state: Transfer.State
            if Self.isRetryable(error) && attempts < delays.count {
                let retryDate = Date(timeIntervalSinceNow: delays[attempts])
                retryDates[path] = retryDate
                scanDate = min(scanDate, retryDate)
                self.attempts[path] = attempts + 1
                state = .retrying(attempts + 1)
            } else {
                state = .failed(Self.message(for: error))
            }
            sender.control.block(path)
            updateTransfer(Transfer(path: path, progress: nil, bytesPerSecond: 0, state: state))
        case .rescan:
            scanDate = min(scanDate, Date(timeIntervalSinceNow: 0.5))
        }
    }

    private func networkDidChange(available: Bool) {
        let restored = available && !networkAvailable
        networkAvailable = available
        if restored { syncNow() }
    }

    private func record(_ error: Error) {
        if let ffi = error as? FfiError {
            switch ffi {
            case .Cancelled: return
            case .Http(401, _, _, _), .Http(403, _, _, _):
                connection = .unauthorized
                return
            case .Transport:
                connection = networkAvailable ? .unreachable : .offline
                return
            default: break
            }
        }
        if connection == .checking {
            connection = .error(Self.message(for: error))
        } else {
            syncError = Self.message(for: error)
        }
    }

    private func removeTransfer(_ path: String) {
        transfers.removeAll { $0.path == path }
    }

    private func updateTransfer(_ transfer: Transfer) {
        if let index = transfers.firstIndex(where: { $0.path == transfer.path }) {
            transfers[index] = transfer
        } else {
            transfers.insert(transfer, at: 0)
        }
    }

    private static func isRetryable(_ error: Error) -> Bool {
        switch error as? FfiError {
        case .Transport(_, let retryable), .Http(_, _, _, let retryable):
            return retryable
        default:
            return false
        }
    }

    private static func message(for error: Error) -> String {
        switch error as? FfiError {
        case .Http(401, _, _, _), .Http(403, _, _, _):
            return "TailDrive could not authenticate. Check the API token or Tailscale identity."
        case .Http(_, "ERR_CHECKSUM_MISMATCH", _, _), .Integrity:
            return "Integrity mismatch. Retry to verify the file again."
        case .Http(_, "ERR_QUOTA_EXCEEDED", _, _), .Http(_, "ERR_DISK_FLOOR", _, _):
            return "Storage limit reached."
        case .Http(_, _, let message, _), .Transport(let message, _), .Io(let message),
             .InvalidPath(let message), .Configuration(let message):
            return message
        default:
            return error.localizedDescription
        }
    }
}
