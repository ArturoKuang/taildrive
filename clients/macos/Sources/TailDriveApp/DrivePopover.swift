import AppKit
import SwiftUI
import TailDriveSyncKit

struct DrivePopover: View {
    @ObservedObject var model: SyncModel
    @Environment(\.openWindow) private var openWindow
    @Environment(\.colorScheme) private var colorScheme
    @Environment(\.colorSchemeContrast) private var contrast

    private var accent: Color {
        colorScheme == .dark ? Color(red: 0.23, green: 0.51, blue: 0.96) : Color(red: 0.15, green: 0.39, blue: 0.92)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            header
            Divider()
            quota
            if let message = statusMessage ?? model.syncError { statusBanner(message) }
            Divider()
            ScrollView {
                VStack(alignment: .leading, spacing: 8) {
                    transfers
                    Divider()
                    recent
                }
            }
            .scrollBounceBehavior(.basedOnSize)
            Divider()
            footer
        }
        .padding(12)
        .frame(width: 320, height: 420)
        .background {
            if contrast == .increased {
                Color(nsColor: .windowBackgroundColor)
            }
        }
        .tint(accent)
    }

    private var header: some View {
        HStack(spacing: 8) {
            Circle()
                .fill(model.connection == .online ? .green : .secondary)
                .frame(width: 8, height: 8)
                .accessibilityLabel(model.connection == .online ? "Tailscale host reachable" : "TailDrive unavailable")
            Text(model.node).font(.system(size: 14, weight: .semibold))
            Spacer()
            Button(model.paused ? "Resume" : "Pause") { model.setPaused(!model.paused) }
                .disabled(model.configuration == nil)
                .controlSize(.small)
        }
    }

    private var quota: some View {
        let used = model.quota?.usedBytes ?? 0
        let limit = model.quota?.limitBytes ?? 5 * 1024 * 1024 * 1024
        let fraction = limit == 0 ? 1 : min(Double(used) / Double(limit), 1)
        let status = model.quota?.status
        let color: Color = status == .full ? .red : status == .warn ? .orange : accent
        return VStack(alignment: .leading, spacing: 4) {
            HStack {
                Text("Storage").fontWeight(.medium)
                Spacer()
                Text(model.quota == nil ? "— / \(gb(limit))" : "\(gb(used, includeUnit: false)) / \(gb(limit))")
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
            }
            .font(.system(size: 11))
            ProgressView(value: fraction).tint(color)
                .accessibilityLabel("Storage usage")
        }
    }

    private var transfers: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("Transfers").font(.system(size: 11, weight: .semibold))
            if model.transfers.isEmpty {
                Text(model.paused ? "Sync paused" : "No active transfers")
                    .font(.system(size: 11)).foregroundStyle(.secondary)
            }
            ForEach(model.transfers) { transfer in
                VStack(alignment: .leading, spacing: 3) {
                    HStack(spacing: 6) {
                        Text(name(transfer.path)).font(.system(size: 13)).lineLimit(1)
                            .help(transfer.path)
                        Spacer(minLength: 0)
                        switch transfer.state {
                        case .active:
                            if transfer.bytesPerSecond > 0 {
                                Text(speed(transfer.bytesPerSecond)).font(.system(size: 11))
                                    .monospacedDigit().foregroundStyle(.secondary).fixedSize()
                            }
                            Button {
                                model.cancel(transfer.path)
                            } label: {
                                Image(systemName: "xmark.circle")
                            }
                            .buttonStyle(.borderless)
                            .accessibilityLabel("Cancel \(name(transfer.path))")
                        case .retrying, .failed, .cancelled:
                            Button("Retry") { model.retry(transfer.path) }.controlSize(.small)
                        }
                    }
                    switch transfer.state {
                    case .active:
                        if let value = transfer.progress {
                            ProgressView(value: value)
                        } else {
                            ProgressView().controlSize(.small)
                        }
                    case .retrying(let attempt):
                        Text("Retrying (\(attempt)/3)…").foregroundStyle(.orange).font(.system(size: 11))
                    case .failed(let message):
                        Text(message).foregroundStyle(.red).font(.system(size: 11))
                    case .cancelled:
                        Text("Cancelled").foregroundStyle(.secondary).font(.system(size: 11))
                    }
                }
                .padding(.vertical, 3)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private var recent: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text("Recent files").font(.system(size: 11, weight: .semibold)).padding(.bottom, 4)
            if model.recent.isEmpty {
                Text("No recent files").font(.system(size: 11)).foregroundStyle(.secondary)
            }
            ForEach(model.recent.prefix(5), id: \.self) { path in
                Button {
                    NSWorkspace.shared.activateFileViewerSelecting([model.revealURL(for: path)])
                } label: {
                    HStack(spacing: 8) {
                        Image(systemName: "doc").foregroundStyle(.secondary)
                        Text(name(path)).lineLimit(1).truncationMode(.middle)
                        Spacer(minLength: 0)
                        Image(systemName: "arrow.up.right.square").foregroundStyle(.secondary)
                    }
                    .font(.system(size: 13))
                    .frame(height: 30)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .help("Reveal \(path) in Finder")
            }
        }
    }

    private var footer: some View {
        HStack(spacing: 6) {
            Button(model.folder == DriveConfiguration.defaultFolder ? "Open ~/TailDrive" : "Open Folder") {
                NSWorkspace.shared.open(model.folder)
            }
            .disabled(model.configuration == nil)
            Spacer(minLength: 0)
            Button("Web UI") {
                if let server = model.configuration?.server { NSWorkspace.shared.open(server) }
            }
            .disabled(model.configuration == nil)
            Button("Preferences") { showPreferences() }
        }
        .controlSize(.small)
    }

    private var statusMessage: String? {
        switch model.connection {
        case .setup: return "Set up your TailDrive server in Preferences."
        case .checking: return "Connecting to TailDrive…"
        case .online: return nil
        case .offline: return "Offline. Your local files are still available."
        case .unreachable: return "Could not reach \(model.node). Check your Tailscale connection."
        case .unauthorized: return "Sign-in failed. Check your API token or Tailscale identity."
        case .error(let message): return message
        }
    }

    private func statusBanner(_ message: String) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(message).font(.system(size: 11)).foregroundStyle(.secondary)
            if model.connection == .unreachable || model.connection == .offline {
                HStack {
                    Button("Retry") { model.syncNow() }
                    if model.connection == .unreachable {
                        Button("Open Tailscale") {
                            if let url = URL(string: "tailscale://") { NSWorkspace.shared.open(url) }
                        }
                    }
                }
                .controlSize(.small)
            }
        }
    }

    private func showPreferences() {
        openWindow(id: "preferences")
        NSApp.activate()
    }

    private func name(_ path: String) -> String {
        path.split(separator: "/").last.map(String.init) ?? path
    }

    private func gb(_ bytes: UInt64, includeUnit: Bool = true) -> String {
        let value = (Double(bytes) / (1024 * 1024 * 1024)).formatted(.number.precision(.fractionLength(2)))
        return includeUnit ? value + " GB" : value
    }

    private func speed(_ bytes: Double) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(bytes), countStyle: .file) + "/s"
    }
}
