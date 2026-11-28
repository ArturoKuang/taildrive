#!/bin/sh
set -eu

repo_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_root"

channel=${TAILDRIVE_CHANNEL:-dev}
build_number=${TAILDRIVE_BUILD_NUMBER:-$(git rev-list --count HEAD 2>/dev/null || echo 1)}
commit_sha=${TAILDRIVE_COMMIT_SHA:-$(git rev-parse HEAD 2>/dev/null || echo dev)}
short_sha=$(printf '%s' "$commit_sha" | cut -c1-7)
built_at=${TAILDRIVE_BUILT_AT:-$(date -u +"%Y-%m-%dT%H:%M:%SZ")}
out_dir=${TAILDRIVE_DIST_DIR:-"$repo_root/dist"}

export TAILDRIVE_CHANNEL="$channel"
export TAILDRIVE_BUILD_NUMBER="$build_number"
export TAILDRIVE_COMMIT_SHA="$commit_sha"
export TAILDRIVE_BUILT_AT="$built_at"

case "$(uname -s)" in
    Darwin) os="darwin" ;;
    Linux) os="linux" ;;
    *) echo "Unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
    arm64|aarch64) arch="arm64" ;;
    x86_64|amd64) arch="x86_64" ;;
    *) echo "Unsupported arch: $(uname -m)" >&2; exit 1 ;;
esac

cargo build --locked --release -p taildrive-server

if [ -d "$repo_root/web" ] && command -v npm >/dev/null 2>&1; then
    (cd "$repo_root/web" && npm ci --no-audit --no-fund && npm run build)
fi

stage_dir=$(mktemp -d)
trap 'rm -rf "$stage_dir"' EXIT
install -m 755 "$repo_root/target/release/taildrive-server" "$stage_dir/taildrive-server"
if [ -d "$repo_root/web/dist" ]; then
    cp -R "$repo_root/web/dist" "$stage_dir/web-dist"
fi

mkdir -p "$out_dir"
archive_path="$out_dir/taildrive-server-${os}-${arch}-${short_sha}.tar.gz"
rm -f "$archive_path"
tar -czf "$archive_path" -C "$stage_dir" .
echo "Archive $archive_path"
