// swift-tools-version: 5.9
import Foundation
import PackageDescription

let rustLibrary = URL(fileURLWithPath: #filePath)
    .deletingLastPathComponent()
    .appendingPathComponent("../../target/release/libtaildrive_ffi.a")
    .standardizedFileURL.path

let package = Package(
    name: "TailDrive",
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "TailDriveSyncKit", targets: ["TailDriveSyncKit"]),
        .executable(name: "TailDriveApp", targets: ["TailDriveApp"]),
    ],
    targets: [
        .target(
            name: "CTailDriveFFI",
            path: "Sources/TailDriveFFI/CTailDriveFFI",
            publicHeadersPath: "include",
            linkerSettings: [
                .unsafeFlags(["-Xlinker", rustLibrary]),
                .linkedFramework("Security"),
                .linkedFramework("SystemConfiguration"),
            ]
        ),
        .target(name: "TailDriveFFI", dependencies: ["CTailDriveFFI"], exclude: ["CTailDriveFFI"]),
        .target(name: "TailDriveSyncKit", dependencies: ["TailDriveFFI"]),
        .executableTarget(name: "TailDriveApp", dependencies: ["TailDriveSyncKit"]),
        .testTarget(name: "TailDriveSyncKitTests", dependencies: ["TailDriveSyncKit", "TailDriveFFI"]),
    ]
)
