import Foundation
import TailDriveFFI
@testable import TailDriveSyncKit
import XCTest

final class AppUpdaterTests: XCTestCase {
    func testCurrentBuildInfoExposedOverFFI() {
        let info = currentBuildInfo()
        XCTAssertFalse(info.version.isEmpty)
        XCTAssertFalse(info.channel.isEmpty)
        XCTAssertFalse(info.commitSha.isEmpty)
    }

    func testSwapScriptReplacesBundleAndCleansBackup() throws {
        try withTempDir { root in
            let target = root.appendingPathComponent("TailDrive.app", isDirectory: true)
            let staged = root.appendingPathComponent("staged/TailDrive.app", isDirectory: true)
            let backup = root.appendingPathComponent(".TailDrive.app.backup", isDirectory: true)
            try makeDummyBundle(at: target, marker: "old-version")
            try makeDummyBundle(at: staged, marker: "new-version")

            let scriptURL = root.appendingPathComponent("swap.sh")
            let script = AppUpdater.buildSwapScript(
                parentPid: 0,
                targetApp: target,
                stagedApp: staged,
                backupApp: backup,
                launchUpdated: false
            )
            try script.write(to: scriptURL, atomically: true, encoding: .utf8)

            let process = Process()
            process.executableURL = URL(fileURLWithPath: "/bin/sh")
            process.arguments = [scriptURL.path]
            try process.run()
            process.waitUntilExit()
            XCTAssertEqual(process.terminationStatus, 0)

            let marker = try String(contentsOf: target.appendingPathComponent("Contents/marker.txt"), encoding: .utf8)
            XCTAssertEqual(marker, "new-version")
            XCTAssertFalse(FileManager.default.fileExists(atPath: backup.path))
            XCTAssertFalse(FileManager.default.fileExists(atPath: staged.path))
        }
    }

    func testValidateAppBundleAcceptsValidAndRejectsInvalid() throws {
        try withTempDir { root in
            let valid = root.appendingPathComponent("Valid.app", isDirectory: true)
            try makeDummyBundle(at: valid, marker: "ok")
            XCTAssertNoThrow(try AppUpdater.validateAppBundle(at: valid, verifyCodesign: false))

            let wrongId = root.appendingPathComponent("WrongId.app", isDirectory: true)
            try makeDummyBundle(at: wrongId, bundleId: "com.other.app", marker: "bad")
            XCTAssertThrowsError(try AppUpdater.validateAppBundle(at: wrongId, verifyCodesign: false))
        }
    }

    private func makeDummyBundle(
        at appURL: URL,
        bundleId: String = "com.taildrive.macos",
        marker: String
    ) throws {
        let macosDir = appURL.appendingPathComponent("Contents/MacOS", isDirectory: true)
        try FileManager.default.createDirectory(at: macosDir, withIntermediateDirectories: true)
        let plist = """
        <?xml version="1.0" encoding="UTF-8"?>
        <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
        <plist version="1.0">
        <dict>
            <key>CFBundleIdentifier</key>
            <string>\(bundleId)</string>
            <key>CFBundleExecutable</key>
            <string>TailDriveApp</string>
        </dict>
        </plist>
        """
        try plist.write(to: appURL.appendingPathComponent("Contents/Info.plist"), atomically: true, encoding: .utf8)
        let exe = macosDir.appendingPathComponent("TailDriveApp")
        try "#!/bin/sh\nexit 0\n".write(to: exe, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: exe.path)
        try marker.write(to: appURL.appendingPathComponent("Contents/marker.txt"), atomically: true, encoding: .utf8)
    }

    private func withTempDir(_ body: (URL) throws -> Void) throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("taildrive-updater-test-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        try body(dir)
    }
}
