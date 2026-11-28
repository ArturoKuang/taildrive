import AppKit
import SwiftUI
import TailDriveSyncKit

struct PreferencesView: View {
    @ObservedObject var model: SyncModel
    @ObservedObject var updater: AppUpdater
    @Environment(\.dismiss) private var dismiss
    @State private var address = ""
    @State private var token = ""
    @State private var folder = DriveConfiguration.defaultFolder
    @State private var error: String?

    var body: some View {
        Form {
            TextField("Server", text: $address, prompt: Text("https://host.tailnet.ts.net"))
            SecureField("API token", text: $token, prompt: Text("Optional with Tailscale identity"))
            LabeledContent("Sync folder") {
                HStack {
                    Text(folder.path(percentEncoded: false)).lineLimit(1).truncationMode(.head)
                        .help(folder.path)
                    Spacer()
                    Button("Choose…", action: chooseFolder)
                }
            }
            if let error {
                Text(error).foregroundStyle(.red)
            }
            HStack {
                Spacer()
                if model.saving { ProgressView().controlSize(.small) }
                Button("Save") {
                    error = nil
                    model.saveSettings(serverAddress: address, token: token, folder: folder) { message in
                        if let message { error = message } else { dismiss() }
                    }
                }
                .keyboardShortcut(.defaultAction)
                .disabled(address.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || model.saving)
            }
            Divider()
            Toggle("Automatically check for releases on main", isOn: $updater.autoCheckEnabled)
            LabeledContent("Current version") {
                HStack(spacing: 8) {
                    Text(updater.currentBuild.version)
                        .font(.system(.body, design: .monospaced))
                        .foregroundStyle(.secondary)
                    Spacer()
                    if let release = updater.state.availableRelease {
                        Button("Update & Restart") {
                            updater.installUpdate(
                                release,
                                beforeExit: { model.prepareForShutdown() },
                                terminate: { NSApp.terminate(nil) }
                            )
                        }
                        .disabled(updater.state.isBusy)
                    } else {
                        Button("Check for Updates") {
                            updater.checkForUpdates(manual: true)
                        }
                        .disabled(updater.state.isBusy)
                    }
                }
            }
            if let summary = updateStatusSummary {
                Text(summary)
                    .font(.caption)
                    .foregroundStyle(isUpdateError ? .red : .secondary)
            }
        }
        .padding(20)
        .frame(width: 480)
        .onAppear {
            address = model.configuration?.server.absoluteString ?? ""
            folder = model.folder
            do { token = try model.token() } catch { self.error = error.localizedDescription }
        }
    }

    private var isUpdateError: Bool {
        if case .error = updater.state { return true }
        return false
    }

    private var updateStatusSummary: String? {
        switch updater.state {
        case .idle:
            return nil
        case .checking:
            return "Checking GitHub Releases…"
        case .upToDate:
            return "You are on the latest release (\(updater.currentBuild.shortSha))."
        case .available(let release):
            return "New build available: \(release.version)"
        case .downloading(let release, let fraction):
            if let fraction {
                return "Downloading \(release.shortSha) (\(Int(fraction * 100))%)…"
            }
            return "Downloading \(release.shortSha)…"
        case .installing(let release):
            return "Installing \(release.shortSha) and restarting…"
        case .error(let message):
            return message
        }
    }

    private func chooseFolder() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.canCreateDirectories = true
        panel.directoryURL = folder.deletingLastPathComponent()
        if panel.runModal() == .OK, let selected = panel.url { folder = selected }
    }
}
