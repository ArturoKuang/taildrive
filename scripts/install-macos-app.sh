#!/bin/sh
set -eu

repo=${TAILDRIVE_REPO:-ArturoKuang/taildrive}
tag=${1:-main-latest}
install_dir=${TAILDRIVE_APP_DIR:-"$HOME/Applications"}
arch=$(uname -m)
artifact_key="macos-app-${arch}"

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

echo "Downloading TailDrive ${version} (${artifact_key})..."
curl -fsSL "$download_url" -o "$tmp_dir/TailDrive.zip"
actual_sha=$(shasum -a 256 "$tmp_dir/TailDrive.zip" | awk '{print $1}')
if [ "$actual_sha" != "$expected_sha" ]; then
    echo "Checksum mismatch: expected $expected_sha, got $actual_sha" >&2
    exit 1
fi

/usr/bin/ditto -x -k "$tmp_dir/TailDrive.zip" "$tmp_dir/extracted"
xattr -dr com.apple.quarantine "$tmp_dir/extracted/TailDrive.app" 2>/dev/null || true
codesign --verify --deep "$tmp_dir/extracted/TailDrive.app"

mkdir -p "$install_dir"
target_app="$install_dir/TailDrive.app"
backup_app="$install_dir/.TailDrive.app.backup"

osascript -e 'tell application "TailDrive" to quit' 2>/dev/null || true
pkill -x TailDriveApp 2>/dev/null || true
sleep 1

rm -rf "$backup_app"
if [ -e "$target_app" ]; then
    mv "$target_app" "$backup_app"
fi
if ! mv "$tmp_dir/extracted/TailDrive.app" "$target_app" 2>/dev/null; then
    /usr/bin/ditto "$tmp_dir/extracted/TailDrive.app" "$target_app"
fi
rm -rf "$backup_app"

echo "Installed TailDrive ${version} -> ${target_app}"
if [ "${TAILDRIVE_NO_LAUNCH:-0}" != "1" ]; then
    /usr/bin/open -n "$target_app"
fi
