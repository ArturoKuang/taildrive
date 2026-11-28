import Combine
import Foundation
import TailDriveFFI

public enum UpdateState: Equatable {
    case idle
    case checking
    case upToDate
    case available(UpdateRelease)
    case downloading(UpdateRelease, Double?)
    case installing(UpdateRelease)
    case error(String)

    public var availableRelease: UpdateRelease? {
        switch self {
        case .available(let release), .downloading(let release, _), .installing(let release):
            return release
        default:
            return nil
        }
    }

    public var isBusy: Bool {
        switch self {
        case .checking, .downloading, .installing:
            return true
        default:
            return false
        }
    }
}

private final class DownloadProgressBridge: TransferObserver, @unchecked Sendable {
    private let onUpdate: (Double?) -> Void
    private var lastReport: TimeInterval = -.infinity

    init(onUpdate: @escaping (Double?) -> Void) {
        self.onUpdate = onUpdate
    }

    func progress(bytes: UInt64, total: UInt64) -> Bool {
        let now = ProcessInfo.processInfo.systemUptime
        if now - lastReport >= 0.1 || (total > 0 && bytes >= total) {
            lastReport = now
            let fraction = total > 0 ? min(Double(bytes) / Double(total), 1.0) : nil
            onUpdate(fraction)
        }
        return true
    }
}

public enum AppUpdaterError: LocalizedError {
    case notAppBundle(URL)
    case directoryNotWritable(URL)
    case extractionFailed(String)
    case invalidBundle(String)

    public var errorDescription: String? {
        switch self {
        case .notAppBundle(let url):
            return "Self-update requires running TailDrive.app (current bundle: \(url.path))."
        case .directoryNotWritable(let dir):
            return "Cannot write to \(dir.path). Move TailDrive.app to ~/Applications to enable in-app updates."
        case .extractionFailed(let message):
            return "Failed to extract update archive: \(message)"
        case .invalidBundle(let message):
            return "Downloaded app bundle is invalid: \(message)"
        }
    }
}

@MainActor
public final class AppUpdater: ObservableObject {
    public static let autoCheckDefaultsKey = "TailDrive.autoCheckUpdates"
    public static let checkIntervalSeconds: TimeInterval = 4 * 60 * 60
    public static let initialCheckDelaySeconds: TimeInterval = 30

    public static var currentArtifactKey: String {
        #if arch(arm64)
        return "macos-app-arm64"
        #else
        return "macos-app-x86_64"
        #endif
    }

    @Published public private(set) var state: UpdateState = .idle
    @Published public private(set) var currentBuild: BuildInfo
    @Published public var autoCheckEnabled: Bool {
        didSet {
            defaults.set(autoCheckEnabled, forKey: Self.autoCheckDefaultsKey)
            rescheduleBackgroundChecks()
        }
    }

    private let defaults: UserDefaults
    private let manifestURL: String?
    private let publicKeyB64: String?
    private let queue = DispatchQueue(label: "com.taildrive.macos.updater", qos: .utility)
    private var initialTimer: Timer?
    private var periodicTimer: Timer?

    public init(
        defaults: UserDefaults = .standard,
        manifestURL: String? = nil,
        publicKeyB64: String? = nil,
        startBackgroundChecks: Bool = true
    ) {
        self.defaults = defaults
        self.manifestURL = manifestURL
        self.publicKeyB64 = publicKeyB64
        self.currentBuild = currentBuildInfo()
        if defaults.object(forKey: Self.autoCheckDefaultsKey) == nil {
            self.autoCheckEnabled = true
        } else {
            self.autoCheckEnabled = defaults.bool(forKey: Self.autoCheckDefaultsKey)
        }

        Self.cleanupStaleArtifacts()
        if startBackgroundChecks {
            rescheduleBackgroundChecks()
        }
    }

    deinit {
        initialTimer?.invalidate()
        periodicTimer?.invalidate()
    }

    public func checkForUpdates(manual: Bool) {
        guard !state.isBusy else { return }
        if manual {
            state = .checking
        }
        let manifestURL = self.manifestURL
        let publicKeyB64 = self.publicKeyB64
        let artifactKey = Self.currentArtifactKey
        queue.async { [weak self] in
            let result = Result {
                try checkForUpdateWithKey(
                    artifactKey: artifactKey,
                    manifestUrl: manifestURL,
                    publicKeyB64: publicKeyB64,
                    allowDevUpgrade: manual
                )
            }
            DispatchQueue.main.async { [weak self] in
                guard let self else { return }
                switch result {
                case .success(.upToDate):
                    if manual {
                        self.state = .upToDate
                    }
                case .success(.available(_, let release)):
                    self.state = .available(release)
                case .failure(let error):
                    if manual {
                        self.state = .error(Self.formatError(error))
                    }
                }
            }
        }
    }

    public func installUpdate(
        _ release: UpdateRelease,
        bundleURL: URL = Bundle.main.bundleURL,
        beforeExit: @escaping () -> Void,
        terminate: @escaping () -> Void
    ) {
        guard !state.isBusy else { return }
        guard bundleURL.pathExtension == "app" else {
            state = .error(AppUpdaterError.notAppBundle(bundleURL).localizedDescription)
            return
        }
        let parentDir = bundleURL.deletingLastPathComponent()
        guard FileManager.default.isWritableFile(atPath: parentDir.path) else {
            state = .error(AppUpdaterError.directoryNotWritable(parentDir).localizedDescription)
            return
        }

        state = .downloading(release, nil)
        let cacheRoot = Self.updatesCacheDirectory()
        let pid = ProcessInfo.processInfo.processIdentifier
        queue.async { [weak self] in
            let result = Result<URL, Error> {
                let stageDir = cacheRoot.appendingPathComponent(
                    "\(release.shortSha)-\(UUID().uuidString.prefix(8))",
                    isDirectory: true
                )
                try FileManager.default.createDirectory(at: stageDir, withIntermediateDirectories: true)
                let archiveURL = stageDir.appendingPathComponent("TailDrive.zip")
                let observer = DownloadProgressBridge { fraction in
                    DispatchQueue.main.async { [weak self] in
                        guard let self else { return }
                        self.state = .downloading(release, fraction)
                    }
                }
                try downloadUpdateArtifact(
                    release: release,
                    destPath: archiveURL.path,
                    observer: observer
                )
                DispatchQueue.main.async { [weak self] in
                    self?.state = .installing(release)
                }
                let extractDir = stageDir.appendingPathComponent("extracted", isDirectory: true)
                try FileManager.default.createDirectory(at: extractDir, withIntermediateDirectories: true)
                try Self.extractZip(archiveURL, to: extractDir)
                let stagedApp = extractDir.appendingPathComponent("TailDrive.app", isDirectory: true)
                Self.clearQuarantine(at: stagedApp)
                try Self.validateAppBundle(at: stagedApp)
                let backupApp = parentDir.appendingPathComponent(
                    ".\(bundleURL.lastPathComponent).backup"
                )
                let scriptURL = stageDir.appendingPathComponent("swap-and-relaunch.sh")
                let script = Self.buildSwapScript(
                    parentPid: pid,
                    targetApp: bundleURL,
                    stagedApp: stagedApp,
                    backupApp: backupApp,
                    launchUpdated: true
                )
                try script.write(to: scriptURL, atomically: true, encoding: .utf8)
                try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: scriptURL.path)
                return scriptURL
            }

            DispatchQueue.main.async { [weak self] in
                guard let self else { return }
                switch result {
                case .success(let scriptURL):
                    beforeExit()
                    do {
                        try Self.spawnDetachedScript(scriptURL)
                        terminate()
                    } catch {
                        self.state = .error(Self.formatError(error))
                    }
                case .failure(let error):
                    self.state = .error(Self.formatError(error))
                }
            }
        }
    }

    private func rescheduleBackgroundChecks() {
        initialTimer?.invalidate()
        initialTimer = nil
        periodicTimer?.invalidate()
        periodicTimer = nil
        guard autoCheckEnabled else { return }

        let initial = Timer(timeInterval: Self.initialCheckDelaySeconds, repeats: false) { [weak self] _ in
            Task { @MainActor [weak self] in
                self?.checkForUpdates(manual: false)
            }
        }
        RunLoop.main.add(initial, forMode: .common)
        initialTimer = initial

        let periodic = Timer(timeInterval: Self.checkIntervalSeconds, repeats: true) { [weak self] _ in
            Task { @MainActor [weak self] in
                self?.checkForUpdates(manual: false)
            }
        }
        RunLoop.main.add(periodic, forMode: .common)
        periodicTimer = periodic
    }

    public nonisolated static func updatesCacheDirectory() -> URL {
        let base = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask).first
            ?? FileManager.default.temporaryDirectory
        return base
            .appendingPathComponent("com.taildrive.macos", isDirectory: true)
            .appendingPathComponent("updates", isDirectory: true)
    }

    public nonisolated static func cleanupStaleArtifacts(bundleURL: URL = Bundle.main.bundleURL) {
        let fm = FileManager.default
        if bundleURL.pathExtension == "app" {
            let backup = bundleURL.deletingLastPathComponent()
                .appendingPathComponent(".\(bundleURL.lastPathComponent).backup")
            try? fm.removeItem(at: backup)
        }
        let cacheDir = updatesCacheDirectory()
        if let entries = try? fm.contentsOfDirectory(
            at: cacheDir,
            includingPropertiesForKeys: [.contentModificationDateKey],
            options: []
        ) {
            let cutoff = Date().addingTimeInterval(-60 * 60)
            for entry in entries {
                let modified = (try? entry.resourceValues(forKeys: [.contentModificationDateKey]))?
                    .contentModificationDate ?? .distantPast
                if modified < cutoff {
                    try? fm.removeItem(at: entry)
                }
            }
        }
    }

    public nonisolated static func extractZip(_ archiveURL: URL, to destination: URL) throws {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/ditto")
        process.arguments = ["-x", "-k", archiveURL.path, destination.path]
        let stderr = Pipe()
        process.standardError = stderr
        try process.run()
        process.waitUntilExit()
        if process.terminationStatus != 0 {
            let msg = String(
                data: stderr.fileHandleForReading.readDataToEndOfFile(),
                encoding: .utf8
            )?.trimmingCharacters(in: .whitespacesAndNewlines) ?? "ditto exited \(process.terminationStatus)"
            throw AppUpdaterError.extractionFailed(msg)
        }
    }

    public nonisolated static func clearQuarantine(at appURL: URL) {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/xattr")
        process.arguments = ["-dr", "com.apple.quarantine", appURL.path]
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        try? process.run()
        process.waitUntilExit()
    }

    public nonisolated static func validateAppBundle(
        at appURL: URL,
        expectedBundleId: String = "com.taildrive.macos",
        verifyCodesign: Bool = true
    ) throws {
        let plistURL = appURL.appendingPathComponent("Contents/Info.plist")
        guard let data = try? Data(contentsOf: plistURL),
              let plist = try? PropertyListSerialization.propertyList(from: data, format: nil) as? [String: Any] else {
            throw AppUpdaterError.invalidBundle("Contents/Info.plist is missing or unreadable")
        }
        guard let bundleId = plist["CFBundleIdentifier"] as? String, bundleId == expectedBundleId else {
            throw AppUpdaterError.invalidBundle("unexpected CFBundleIdentifier")
        }
        guard let executableName = plist["CFBundleExecutable"] as? String, !executableName.isEmpty else {
            throw AppUpdaterError.invalidBundle("CFBundleExecutable is missing")
        }
        let executableURL = appURL
            .appendingPathComponent("Contents/MacOS", isDirectory: true)
            .appendingPathComponent(executableName)
        guard FileManager.default.isExecutableFile(atPath: executableURL.path) else {
            throw AppUpdaterError.invalidBundle("executable \(executableName) is missing or not executable")
        }
        if verifyCodesign {
            let process = Process()
            process.executableURL = URL(fileURLWithPath: "/usr/bin/codesign")
            process.arguments = ["--verify", "--deep", appURL.path]
            let stderr = Pipe()
            process.standardError = stderr
            try process.run()
            process.waitUntilExit()
            if process.terminationStatus != 0 {
                let msg = String(
                    data: stderr.fileHandleForReading.readDataToEndOfFile(),
                    encoding: .utf8
                )?.trimmingCharacters(in: .whitespacesAndNewlines) ?? "codesign verify failed"
                throw AppUpdaterError.invalidBundle(msg)
            }
        }
    }

    public nonisolated static func buildSwapScript(
        parentPid: Int32,
        targetApp: URL,
        stagedApp: URL,
        backupApp: URL,
        launchUpdated: Bool = true
    ) -> String {
        let target = shellQuote(targetApp.path)
        let staged = shellQuote(stagedApp.path)
        let backup = shellQuote(backupApp.path)
        let launchFlag = launchUpdated ? "1" : "0"
        return """
        #!/bin/sh
        set -eu
        PARENT_PID=\(parentPid)
        TARGET=\(target)
        STAGED=\(staged)
        BACKUP=\(backup)
        LAUNCH=\(launchFlag)

        i=0
        while [ "$PARENT_PID" -gt 0 ] && kill -0 "$PARENT_PID" 2>/dev/null; do
            i=$((i + 1))
            if [ "$i" -ge 150 ]; then
                break
            fi
            sleep 0.1
        done

        restore_backup() {
            if [ -e "$BACKUP" ] && [ ! -e "$TARGET" ]; then
                mv "$BACKUP" "$TARGET" || true
            fi
            if [ "$LAUNCH" = "1" ] && [ -e "$TARGET" ]; then
                /usr/bin/open -n "$TARGET" || true
            fi
        }

        rm -rf "$BACKUP"
        if [ -e "$TARGET" ]; then
            mv "$TARGET" "$BACKUP"
        fi

        if ! mv "$STAGED" "$TARGET" 2>/dev/null; then
            if ! /usr/bin/ditto "$STAGED" "$TARGET"; then
                rm -rf "$TARGET"
                restore_backup
                exit 1
            fi
            rm -rf "$STAGED"
        fi

        if [ "$LAUNCH" = "1" ]; then
            if ! /usr/bin/open -n "$TARGET"; then
                rm -rf "$TARGET"
                restore_backup
                exit 1
            fi
        fi
        rm -rf "$BACKUP"
        """
    }

    private nonisolated static func spawnDetachedScript(_ scriptURL: URL) throws {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/sh")
        process.arguments = ["-c", "nohup /bin/sh \(shellQuote(scriptURL.path)) >/dev/null 2>&1 &"]
        process.standardInput = FileHandle.nullDevice
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        try process.run()
        process.waitUntilExit()
    }

    private nonisolated static func shellQuote(_ value: String) -> String {
        "'" + value.replacingOccurrences(of: "'", with: "'\"'\"'") + "'"
    }

    private nonisolated static func formatError(_ error: Error) -> String {
        switch error as? FfiError {
        case .Integrity(let message), .Configuration(let message), .Io(let message),
             .Transport(let message, _), .Http(_, _, let message, _):
            return message
        case .Cancelled:
            return "Update download cancelled."
        default:
            return error.localizedDescription
        }
    }
}
