#!/bin/sh
set -eu

repo=${TAILDRIVE_REPO:-ArturoKuang/taildrive}
tag=${1:-main-latest}
bin_dir=${TAILDRIVE_BIN_DIR:-"$HOME/.local/bin"}
web_dir=${TAILDRIVE_WEB_DIR:-""}
launchd_label=${TAILDRIVE_LAUNCHD_LABEL:-"com.local.taildrive-server"}

case "$(uname -s)" in
    Darwin) os="darwin" ;;
    Linux) os="linux" ;;
    *) echo "Unsupported OS" >&2; exit 1 ;;
esac
case "$(uname -m)" in
    arm64|aarch64) arch="arm64" ;;
    x86_64|amd64) arch="x86_64" ;;
    *) echo "Unsupported arch" >&2; exit 1 ;;
esac
artifact_key="server-${os}-${arch}"

manifest_url="https://github.com/${repo}/releases/download/${tag}/manifest.json"
tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' EXIT

curl -fsSL "$manifest_url" -o "$tmp_dir/manifest.json"
artifact_info=$(/usr/bin/python3 - "$tmp_dir/manifest.json" "$artifact_key" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))
art = data["artifacts"][sys.argv[2]]
print(art["url"])
print(art["sha256"])
print(data["version"])
PY
)
download_url=$(printf '%s\n' "$artifact_info" | sed -n '1p')
expected_sha=$(printf '%s\n' "$artifact_info" | sed -n '2p')
version=$(printf '%s\n' "$artifact_info" | sed -n '3p')

echo "Downloading taildrive-server ${version} (${artifact_key})..."
curl -fsSL "$download_url" -o "$tmp_dir/server.tar.gz"
actual_sha=$(shasum -a 256 "$tmp_dir/server.tar.gz" | awk '{print $1}')
if [ "$actual_sha" != "$expected_sha" ]; then
    echo "Checksum mismatch: expected $expected_sha, got $actual_sha" >&2
    exit 1
fi

mkdir -p "$tmp_dir/extracted"
tar -xzf "$tmp_dir/server.tar.gz" -C "$tmp_dir/extracted"
chmod 755 "$tmp_dir/extracted/taildrive-server"
"$tmp_dir/extracted/taildrive-server" --version

mkdir -p "$bin_dir"
install -m 755 "$tmp_dir/extracted/taildrive-server" "$bin_dir/.taildrive-server.new.$$"
mv "$bin_dir/.taildrive-server.new.$$" "$bin_dir/taildrive-server"
echo "Installed taildrive-server -> $bin_dir/taildrive-server"

if [ -n "$web_dir" ] && [ -d "$tmp_dir/extracted/web-dist" ]; then
    parent_web=$(dirname "$web_dir")
    mkdir -p "$parent_web"
    rm -rf "${web_dir}.new.$$" "${web_dir}.bak.$$"
    cp -R "$tmp_dir/extracted/web-dist" "${web_dir}.new.$$"
    if [ -e "$web_dir" ]; then
        mv "$web_dir" "${web_dir}.bak.$$"
    fi
    mv "${web_dir}.new.$$" "$web_dir"
    rm -rf "${web_dir}.bak.$$"
    echo "Updated web UI -> $web_dir"
fi

if [ "${TAILDRIVE_RESTART:-0}" = "1" ] && [ "$os" = "darwin" ]; then
    uid=$(id -u)
    launchctl kickstart -k "gui/${uid}/${launchd_label}"
    echo "Restarted LaunchAgent gui/${uid}/${launchd_label}"
fi
