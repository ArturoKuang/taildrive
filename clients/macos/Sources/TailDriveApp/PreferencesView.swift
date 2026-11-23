import AppKit
import SwiftUI
import TailDriveSyncKit

struct PreferencesView: View {
    @ObservedObject var model: SyncModel
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
        }
        .padding(20)
        .frame(width: 480)
        .onAppear {
            address = model.configuration?.server.absoluteString ?? ""
            folder = model.folder
            do { token = try model.token() } catch { self.error = error.localizedDescription }
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
