import Foundation
import Security

public struct DriveConfiguration: Equatable, Sendable {
    public let server: URL
    public let folder: URL

    public init(server: URL, folder: URL) {
        self.server = server
        self.folder = folder
    }

    public static var defaultFolder: URL {
        FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("TailDrive", isDirectory: true)
    }

    public static func serverURL(_ input: String) throws -> URL {
        let host = input.trimmingCharacters(in: .whitespacesAndNewlines)
        let address = host.contains("://") ? host : "https://" + host
        guard let parts = URLComponents(string: address),
              ["http", "https"].contains(parts.scheme?.lowercased() ?? ""),
              let hostname = parts.host, !hostname.isEmpty,
              parts.user == nil, parts.password == nil, parts.query == nil, parts.fragment == nil,
              let url = parts.url else {
            throw SyncError.message("Enter a TailDrive server URL or MagicDNS hostname.")
        }
        return url
    }
}

struct SettingsStore {
    let defaults: UserDefaults
    private let service = "com.taildrive.macos.server-token"
    private let prefix = "TailDrive."

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
    }

    var configuration: DriveConfiguration? {
        guard let address = defaults.string(forKey: prefix + "server"),
              let server = try? DriveConfiguration.serverURL(address) else { return nil }
        let folder = defaults.string(forKey: prefix + "folder").map { URL(fileURLWithPath: $0) }
        return DriveConfiguration(server: server, folder: folder ?? DriveConfiguration.defaultFolder)
    }

    var paused: Bool {
        get { defaults.bool(forKey: prefix + "paused") }
        nonmutating set { defaults.set(newValue, forKey: prefix + "paused") }
    }

    func token(for server: URL) throws -> String {
        var request = keychainQuery(server)
        request[kSecReturnData as String] = true
        request[kSecMatchLimit as String] = kSecMatchLimitOne
        var result: CFTypeRef?
        let status = SecItemCopyMatching(request as CFDictionary, &result)
        if status == errSecItemNotFound { return "" }
        try check(status)
        guard let data = result as? Data, let token = String(data: data, encoding: .utf8) else {
            throw SyncError.message("The stored API token could not be read.")
        }
        return token
    }

    func save(_ configuration: DriveConfiguration, token: String) throws {
        let request = keychainQuery(configuration.server)
        if token.isEmpty {
            let status = SecItemDelete(request as CFDictionary)
            if status != errSecItemNotFound { try check(status) }
        } else {
            let value = Data(token.utf8)
            let update = [kSecValueData as String: value]
            let status = SecItemUpdate(request as CFDictionary, update as CFDictionary)
            if status == errSecItemNotFound {
                var newItem = request
                newItem[kSecValueData as String] = value
                try check(SecItemAdd(newItem as CFDictionary, nil))
            } else {
                try check(status)
            }
        }
        defaults.set(configuration.server.absoluteString, forKey: prefix + "server")
        defaults.set(configuration.folder.path, forKey: prefix + "folder")
    }

    private func keychainQuery(_ server: URL) -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: server.absoluteString,
        ]
    }

    private func check(_ status: OSStatus) throws {
        guard status == errSecSuccess else {
            let message = SecCopyErrorMessageString(status, nil) as String? ?? "Error \(status)"
            throw SyncError.message("Keychain: \(message)")
        }
    }
}

enum SyncError: LocalizedError {
    case message(String)
    case changed(String)

    var errorDescription: String? {
        switch self {
        case .message(let message): return message
        case .changed(let path): return "\(path) changed during sync. TailDrive will scan it again."
        }
    }
}
