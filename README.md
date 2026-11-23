# TailDrive

Private, self-hosted Dropbox-style cloud storage data center in **Rust**, anchored on `macmini-ci` over **Tailscale**, backed by **Garage S3**, with **Web**, **native macOS (SwiftUI)**, and **native Android (Kotlin + Jetpack Compose)** clients sharing one Rust core via **UniFFI**.

## Monorepo Layout
- `crates/taildrive-core/` — Shared Rust domain models, NFC path safety, streaming SHA-256, `/v1` API client, retry backoff.
- `crates/taildrive-server/` — `axum` + `tokio` + SQLite WAL + OpenDAL (`local-fs` & `s3`/Garage) API server, resumable `/v1/uploads`, expiring `/v1/shares`, loopback TCP peer `Tailscale-User-Login` + constant-time Bearer auth, static Web UI serving.
- `crates/taildrive-ffi/` — UniFFI bindings + workspace `uniffi-bindgen` generating Swift (`clients/macos/`) and Kotlin (`clients/android/`) bindings from one Rust sync engine.
- `web/` — React + TypeScript + Vite Web UI matching `docs/design-v1.md`.
- `clients/macos/` — Native macOS 14+ SwiftUI menu-bar app + `~/TailDrive` mirror statically linked to `libtaildrive_ffi.a`.
- `clients/android/` — Native Kotlin + Jetpack Compose + WorkManager Android app (`Files`, `Backup`, `Transfers`) + UniFFI adapter.
- `deploy/` & `docs/scaling-runbook.md` — External SSD migration, multi-node Garage cluster join scripts, and LaunchAgent templates.

## Quick Verification Commands
```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
(cd web && npm run build)
(cd clients/macos && swift build && swift test)
(cd clients/android && ANDROID_HOME=/opt/homebrew/share/android-commandlinetools JAVA_HOME=/opt/homebrew/opt/openjdk@17/libexec/openjdk.jdk/Contents/Home gradle testDebugUnitTest assembleDebug)
```
