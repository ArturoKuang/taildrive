# TailDrive — Operations & Multi-Machine Scaling Runbook

## 1. Live Services on `macmini-ci` (`100.97.87.111`)
| Service | LaunchAgent | Endpoint | Config |
|---|---|---|---|
| **Garage 2.5.0 S3** | `com.local.garage` | `127.0.0.1:3900` (S3) / `3903` (Admin) | `~/.taildrive/garage/config.toml` |
| **TailDrive Server** | `com.local.taildrive-server` | `127.0.0.1:8787` (`/v1/*` + Web UI `/`) | `~/.taildrive/taildrive.env` |
| **CAO Server** | `com.local.cao-server` | `127.0.0.1:9889` | `~/.aws/cli-agent-orchestrator/` |
| **Hermes Serve** | `com.local.hermes-serve` | `127.0.0.1:9119` | `~/.hermes/` |

## 2. Enable Tailscale HTTPS (`tailscale serve`)
1. Enable Serve once on your tailnet: `https://login.tailscale.com/f/serve?node=nPgV8Uu6GY11CNTRL`
2. Run on `macmini-ci`:
   ```bash
   tailscale serve --bg http://127.0.0.1:8787
   tailscale serve status
   ```
3. Open `https://arturos-mac-mini.<your-tailnet>.ts.net` from your MacBook Pro or Android phone (auto-authenticates via `Tailscale-User-Login` over loopback; mobile/native clients can also use the Bearer token in `~/.taildrive/taildrive.env`).

## 3. Swap to an External SSD / DAS on `macmini-ci`
Zero app or client changes required:
```bash
./deploy/switch-storage-root.sh --target /Volumes/TailDriveSSD --quota-gb 1000 --dry-run
./deploy/switch-storage-root.sh --target /Volumes/TailDriveSSD --quota-gb 1000
```

## 4. Scale Horizontally to Machine #2 / #3 Over Tailscale
1. Install `garage` on the new machine and configure it from `deploy/garage-multinode.toml.example` using the shared `rpc_secret` from `~/.taildrive/garage/config.toml` and each machine's Tailscale `100.x` IP (`rpc_bind_addr` / `rpc_public_addr`).
2. Bump `replication_factor = 2` (for 2–3 nodes) or `3` (for 3+ nodes) across nodes and restart Garage.
3. Join the new node from `macmini-ci`:
   ```bash
   ./deploy/node-join.sh "<NEW_NODE_ID>@100.x.y.z:3901" dc2 2T 2
   ```
4. **Metadata resilience:** Add Litestream continuous replication of `~/.taildrive/server-data/taildrive.db` to a dedicated Garage bucket (or migrate `db.rs` to Postgres when running multiple active API frontends).

## 5. Upgrading Clients to v2 (Zero Backend Changes)
- **macOS File Provider Extension:** `/v1/files` already returns `id`, `version`, `etag`, `sha256`, and `If-Match` `409` conflict semantics — attach an `NSFileProviderReplicatedExtension` target to `TailDriveSyncKit` without touching `taildrive-server`.
- **Android Native Rust `.so`:** Install Android NDK + `cargo-ndk`, then run `./clients/android/scripts/build-android-rust.sh` to bundle `libtaildrive_ffi.so` into `app/src/main/jniLibs/arm64-v8a/`.
