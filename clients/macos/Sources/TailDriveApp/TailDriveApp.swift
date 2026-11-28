import AppKit
import SwiftUI
import TailDriveSyncKit

@main
@MainActor
struct TailDriveApp: App {
    @NSApplicationDelegateAdaptor(MenuBarDelegate.self) private var delegate
    @StateObject private var model = SyncModel()
    @StateObject private var updater = AppUpdater()

    private var menuBarSymbol: String {
        if updater.state.availableRelease != nil {
            return "externaldrive.badge.plus"
        }
        return model.connection == .online && !model.paused ? "externaldrive.fill" : "externaldrive"
    }

    var body: some Scene {
        MenuBarExtra {
            DrivePopover(model: model, updater: updater)
        } label: {
            Image(systemName: menuBarSymbol)
                .accessibilityLabel(updater.state.availableRelease != nil ? "TailDrive (Update Available)" : "TailDrive")
        }
        .menuBarExtraStyle(.window)

        Window("TailDrive Preferences", id: "preferences") {
            PreferencesView(model: model, updater: updater)
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
