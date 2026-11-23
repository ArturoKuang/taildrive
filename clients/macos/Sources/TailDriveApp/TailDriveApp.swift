import AppKit
import SwiftUI
import TailDriveSyncKit

@main
@MainActor
struct TailDriveApp: App {
    @NSApplicationDelegateAdaptor(MenuBarDelegate.self) private var delegate
    @StateObject private var model = SyncModel()

    var body: some Scene {
        MenuBarExtra {
            DrivePopover(model: model)
        } label: {
            Image(systemName: model.connection == .online && !model.paused ? "externaldrive.fill" : "externaldrive")
                .accessibilityLabel("TailDrive")
        }
        .menuBarExtraStyle(.window)

        Window("TailDrive Preferences", id: "preferences") {
            PreferencesView(model: model)
        }
        .defaultPosition(.center)
        .windowResizability(.contentSize)
    }
}

private final class MenuBarDelegate: NSObject, NSApplicationDelegate {
    func applicationWillFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory)
    }
}
